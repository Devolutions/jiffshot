//! A scripted walk through the editor for testing without a mouse: draws one
//! object with each tool through the canvas, edits properties through the
//! view model, opens the other windows, and writes window screenshots, the
//! exported image and a log to the directory named by `JIFFSHOT_UITEST`.
//! `JIFFSHOT_UITEST_IMAGE` optionally names the image to annotate (and turns
//! on the image-tool, template and window steps), `JIFFSHOT_UITEST_VIDEO` a
//! video to step through; `JIFFSHOT_UITEST_KEEP` leaves the test window open.
//! `JIFFSHOT_UITEST_CAPTURE_ONLY` checks the Capture window and chooser
//! without running the editor and overlay walk.
//! On macOS, `JIFFSHOT_UITEST_OVERLAY` runs only the synthetic overlay test.
//! `JIFFSHOT_UITEST_STARTUP` checks login-item status without changing it.
//! `JIFFSHOT_UITEST_RECENT` checks recent-file actions with a scratch library.
//! `JIFFSHOT_UITEST_LIBRARY` checks library actions with synthetic captures.
//! `JIFFSHOT_UITEST_LIBRARY_COPY` checks library copy using a private pasteboard.
//! `JIFFSHOT_UITEST_LIBRARY_DRAG` checks outgoing library drag gestures.
//! `JIFFSHOT_UITEST_CONTROL` leaves an isolated control server for an external fixture.
//! `JIFFSHOT_UITEST_CLIPBOARD` checks editor clipboard behavior without using
//! the system clipboard.
//! `JIFFSHOT_UITEST_FONTS` checks editor font notices without sending input.
//! `JIFFSHOT_UITEST_SHARE_PROGRAM` checks the custom application picker and
//! sharing with a disposable application bundle.
//! `JIFFSHOT_UITEST_SHARE_PREFS` checks an actual stale Preferences submission.
//! `JIFFSHOT_UITEST_PANEL_SHUTDOWN` checks quitting with a pending share panel.
//!
//! The steps run one after another on the UI thread, each followed by a pause
//! so the windows can render; screenshots come from the windows themselves
//! (`PrintWindow`), so they work for windows left out of screen captures.

use super::overlay::overlay_api as api;
use super::UiOnly;
use crate::canvas::{POINTER_MOVED, POINTER_PRESSED, POINTER_RELEASED};
use crate::vm::{MainViewModel, MainViewModelSink};
#[cfg(windows)]
use jiffshot_capture::gdi::Dib;
use jiffshot_capture::Rect;
use rustolonia::{AppScope, Signal};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "macos")]
use std::sync::Arc;
use std::time::{Duration, Instant};
#[cfg(windows)]
use windows_sys::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
#[cfg(windows)]
use windows_sys::Win32::Graphics::Gdi::{GetDC, GetPixel, ReleaseDC};
#[cfg(windows)]
use windows_sys::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
#[cfg(windows)]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowDisplayAffinity, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, SetWindowDisplayAffinity, WindowFromPoint, WDA_NONE, WM_CLOSE,
};

/// `PW_RENDERFULLCONTENT`: what the compositor shows, for GPU-drawn windows.
#[cfg(windows)]
const PW_RENDERFULLCONTENT: PRINT_WINDOW_FLAGS = 2;

static FAILED: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "macos")]
static EXIT_ENGINE_BLOCKED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static EXIT_UI_PROGRESS: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static EXIT_ENGINE_RELEASE: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>> = std::sync::Mutex::new(None);
#[cfg(target_os = "macos")]
static RECORDING_QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static NATIVE_CLEANUP_VERIFIED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static RECORDING_FAILURE_VERIFIED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static NATIVE_TERMINATION_OBSERVED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static FINAL_NATIVE_REQUESTED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static IMAGE_QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static IMAGE_JOB_RELEASED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static IMAGE_FAILURE_VERIFIED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static IMAGE_JOB_READY: std::sync::Mutex<Option<std::sync::mpsc::Receiver<std::result::Result<(), String>>>> =
    std::sync::Mutex::new(None);
#[cfg(target_os = "macos")]
static IMAGE_JOB_RELEASE: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>> = std::sync::Mutex::new(None);
#[cfg(target_os = "macos")]
static PANEL_QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static PANEL_RETRY_VERIFIED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static PANEL_SHARE_RESULT: std::sync::Mutex<Option<std::sync::mpsc::Receiver<std::result::Result<Vec<String>, String>>>> =
    std::sync::Mutex::new(None);
#[cfg(target_os = "macos")]
thread_local! {
    static TEST_PENDING_EXPORT: RefCell<Option<crate::video::PendingWork>> = const { RefCell::new(None) };
    static DENIED_RECORDING_PUBLICATION: RefCell<Option<DeniedRecordingPublication>> = const { RefCell::new(None) };
    static DENIED_IMAGE_PUBLICATION: RefCell<Option<DeniedImagePublication>> = const { RefCell::new(None) };
}
#[cfg(target_os = "macos")]
static RECORDING_QUIT_RESULT: std::sync::Mutex<Option<std::sync::mpsc::Receiver<std::result::Result<Option<PathBuf>, String>>>> =
    std::sync::Mutex::new(None);

pub(super) fn failed() -> bool {
    FAILED.load(Ordering::Acquire)
}

#[cfg(target_os = "macos")]
#[path = "selftest_macos.rs"]
mod platform;
#[cfg(target_os = "macos")]
use platform::{affinity, close_window, main_window, probe, process_windows, window_rect};

#[cfg(target_os = "macos")]
thread_local! {
    static EDITOR_TEST_MENU: RefCell<Option<rustolonia::MenuFlyout>> = const { RefCell::new(None) };
}

/// What the steps share.
struct Test {
    dir: PathBuf,
    log: String,
    /// The document size after the first fit.
    w: f64,
    h: f64,
    /// Values a step saves for a later one.
    note: String,
    windows_before: Vec<isize>,
    #[cfg(target_os = "macos")]
    popup: Option<(i64, u32)>,
    #[cfg(target_os = "macos")]
    probe_area: Option<Rect>,
    #[cfg(target_os = "macos")]
    popup_samples: Vec<usize>,
    #[cfg(target_os = "macos")]
    probe_deadline: Option<Instant>,
    #[cfg(target_os = "macos")]
    gate_probe: Option<super::hud::native_popup_macos::GateDropProbe>,
}

type Step = Box<dyn FnOnce(&mut Test) -> u64>;

/// The steps still to run, and the pause after each.
struct Runner {
    scope: AppScope,
    test: Test,
    steps: VecDeque<Step>,
}

fn sink() -> MainViewModelSink {
    super::main().sink
}

/// Sends `action` to the model, as the window's controls do.
fn run(action: impl Into<String>) {
    let action = action.into();
    super::main().model.run(|m| m.run(action));
}

fn call(f: impl FnOnce(&mut dyn MainViewModel) -> rustolonia::Result<()>) {
    super::main().model.run(f);
}

/// Changes a property as a control would: the model's setter runs.
fn set<T: 'static>(signal: Signal<T>, value: T) {
    signal.set(value);
}

fn canvas_feed(kind: i32, x: f64, y: f64) {
    if let Some(canvas) = super::canvas() {
        canvas.simulate(kind, x, y, 0);
    }
}

// ---- windows ------------------------------------------------------------------

/// This process's visible top-level windows.
#[cfg(windows)]
fn process_windows() -> Vec<isize> {
    unsafe extern "system" fn each(hwnd: HWND, list: LPARAM) -> i32 {
        let mut pid = 0;
        // SAFETY: `hwnd` comes from EnumWindows; `list` is the Vec below.
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid == GetCurrentProcessId() && IsWindowVisible(hwnd) != 0 {
                (*(list as *mut Vec<isize>)).push(hwnd as isize);
            }
        }
        1
    }
    let mut list: Vec<isize> = Vec::new();
    // SAFETY: the callback only runs during the call, with `list` alive.
    unsafe { EnumWindows(Some(each), &mut list as *mut Vec<isize> as LPARAM) };
    list
}

#[cfg(windows)]
fn window_rect(hwnd: isize) -> Rect {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: a plain query; a stale handle only fails.
    unsafe { GetWindowRect(hwnd as HWND, &mut r) };
    Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top)
}

/// The window's display affinity (0x11: left out of captures).
#[cfg(windows)]
fn affinity(hwnd: isize) -> u32 {
    let mut a = 0;
    // SAFETY: a plain query.
    unsafe { GetWindowDisplayAffinity(hwnd as HWND, &mut a) };
    a
}

/// What the screen shows over `r`, with `hwnd` not left out of captures
/// while it is grabbed.
#[cfg(windows)]
fn grab_shown(hwnd: isize, r: Rect) -> std::io::Result<jiffshot_capture::frame::Frame> {
    let was = affinity(hwnd);
    // SAFETY: changing, then restoring, one of this process's windows.
    unsafe { SetWindowDisplayAffinity(hwnd as HWND, WDA_NONE) };
    // The compositor needs a frame to show the window in captures.
    std::thread::sleep(Duration::from_millis(120));
    let frame = jiffshot_capture::screen::grab(r);
    unsafe { SetWindowDisplayAffinity(hwnd as HWND, was) };
    frame
}

/// "#RRGGBB, hits: yes|no": what is on screen at a point, and whether it is
/// `hwnd` there.
#[cfg(windows)]
fn probe(x: i32, y: i32, hwnd: isize) -> String {
    // SAFETY: the screen DC is released before returning.
    let (color, hit) = unsafe {
        let dc = GetDC(std::ptr::null_mut());
        let color = GetPixel(dc, x, y);
        ReleaseDC(std::ptr::null_mut(), dc);
        (color, WindowFromPoint(POINT { x, y }) as isize == hwnd)
    };
    format!("#{:02X}{:02X}{:02X}, hits: {}", color & 0xFF, (color >> 8) & 0xFF, (color >> 16) & 0xFF, if hit { "yes" } else { "no" })
}

#[cfg(windows)]
fn main_window() -> Option<isize> {
    super::main_window_handle()
}

#[cfg(windows)]
fn close_window(hwnd: isize) {
    // SAFETY: posting to one of this process's windows.
    unsafe { PostMessageW(hwnd as HWND, WM_CLOSE, 0, 0) };
}

impl Test {
    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Saves what `hwnd` shows as `name`.
    #[cfg(windows)]
    fn shot_of(&mut self, hwnd: isize, name: &str) {
        let r = window_rect(hwnd);
        let result = Dib::new(r.width.max(1), r.height.max(1)).and_then(|dib| {
            // SAFETY: the DIB's DC is valid for the DIB's life.
            if unsafe { PrintWindow(hwnd as HWND, dib.dc(), PW_RENDERFULLCONTENT) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut frame = dib.to_frame();
            // A per-pixel transparent window prints black: take it from the
            // screen instead, with its capture exclusion lifted meanwhile.
            if frame.pixels.chunks_exact(4).all(|p| p[..3] == [0, 0, 0]) {
                frame = grab_shown(hwnd, r)?;
            }
            frame.save_png(&self.path(name))
        });
        match result {
            Ok(()) => writeln!(self.log, "wrote {}", self.path(name).display()),
            Err(e) => writeln!(self.log, "{name}: {e}"),
        }
        .ok();
    }

    #[cfg(target_os = "macos")]
    fn shot_of(&mut self, hwnd: isize, name: &str) {
        let result = jiffshot_capture::screen::grab_window(hwnd as u32).and_then(|frame| frame.save_png(&self.path(name)));
        match result {
            Ok(()) => writeln!(self.log, "wrote {}", self.path(name).display()),
            Err(error) => writeln!(self.log, "{name}: {error}"),
        }
        .ok();
    }

    /// Saves the editor window.
    fn shot(&mut self, name: &str) {
        match main_window() {
            Some(hwnd) => self.shot_of(hwnd, name),
            None => self.line(format!("{name}: no editor window")),
        }
    }

    /// The windows shown since [`Test::remember_windows`].
    fn new_windows(&self) -> Vec<isize> {
        process_windows().into_iter().filter(|w| !self.windows_before.contains(w)).collect()
    }

    fn remember_windows(&mut self) {
        self.windows_before = process_windows();
    }

    fn line(&mut self, text: impl AsRef<str>) {
        self.log.push_str(text.as_ref());
        self.log.push('\n');
    }

    /// Keeps the log on disk current, so a hang shows where it stopped.
    fn flush(&self) {
        if let Err(error) = std::fs::write(self.path("ui-log.txt"), &self.log) {
            FAILED.store(true, Ordering::Release);
            eprintln!("ui self-test: could not write the log: {error}");
        }
    }

    fn describe(&self) -> String {
        let s = sink();
        format!("selection={} panel={} status={}", s.selection.get_untracked(), s.panel.get_untracked(), s.status.get_untracked())
    }
}

// ---- the script ---------------------------------------------------------------

/// Builds the steps; each returns how long to wait after it, in milliseconds.
struct Script(VecDeque<Step>);

impl Script {
    fn step(&mut self, f: impl FnOnce(&mut Test) -> u64 + 'static) -> &mut Self {
        self.0.push_back(Box::new(f));
        self
    }

    fn wait(&mut self, ms: u64) -> &mut Self {
        self.step(move |_| ms)
    }

    /// Drags with `tool` between two points given as fractions of the document.
    fn drag(&mut self, tool: &'static str, a: (f64, f64), b: (f64, f64), steps: u32) -> &mut Self {
        self.step(move |t| {
            run(format!("tool:{tool}"));
            let (x0, y0, x1, y1) = (t.w * a.0, t.h * a.1, t.w * b.0, t.h * b.1);
            canvas_feed(POINTER_PRESSED, x0, y0);
            for i in 1..=steps {
                let f = f64::from(i) / f64::from(steps);
                canvas_feed(POINTER_MOVED, x0 + (x1 - x0) * f, y0 + (y1 - y0) * f);
            }
            canvas_feed(POINTER_RELEASED, x1, y1);
            120
        })
        .step(move |t| {
            let line = format!("{tool}: {}", t.describe());
            t.line(line);
            0
        })
    }

    fn click(&mut self, tool: &'static str, at: (f64, f64)) -> &mut Self {
        self.step(move |t| {
            run(format!("tool:{tool}"));
            canvas_feed(POINTER_PRESSED, t.w * at.0, t.h * at.1);
            canvas_feed(POINTER_RELEASED, t.w * at.0, t.h * at.1);
            120
        })
        .step(move |t| {
            let line = format!("{tool} click: selection={}", sink().selection.get_untracked());
            t.line(line);
            0
        })
    }

    fn shot(&mut self, name: &'static str) -> &mut Self {
        self.wait(250).step(move |t| {
            t.shot(name);
            0
        })
    }

    fn log(&mut self, f: impl FnOnce(&mut Test) -> String + 'static) -> &mut Self {
        self.step(move |t| {
            let line = f(t);
            t.line(line);
            0
        })
    }

    /// Runs `open`, then saves each window it showed and closes it.
    fn shot_new_windows(&mut self, name: &'static str, open: impl FnOnce(&mut Test) + 'static) -> &mut Self {
        self.step(move |t| {
            t.remember_windows();
            open(t);
            500
        })
        .step(move |t| {
            let shown = t.new_windows();
            if shown.is_empty() {
                t.line(format!("{name}: no window shown"));
            }
            for (i, hwnd) in shown.iter().enumerate() {
                let file = if i == 0 { name.to_owned() } else { name.replace(".png", &format!("-{i}.png")) };
                t.shot_of(*hwnd, &file);
            }
            for hwnd in shown {
                close_window(hwnd);
            }
            200
        })
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn editor_walk(s: &mut Script) {
    let image = env("JIFFSHOT_UITEST_IMAGE");
    s.wait(800);
    if let Some(image) = image.clone() {
        s.step(move |_| {
            run(format!("open:{image}"));
            600
        });
    }
    s.step(|t| {
        run("zoom:fit");
        let (w, h) = super::canvas().map(|c| c.document_size()).unwrap_or((0, 0));
        (t.w, t.h) = (f64::from(w), f64::from(h));
        let zoom = super::canvas().map(|c| c.zoom()).unwrap_or(0.0);
        t.line(format!("document {w}x{h}, zoom {zoom:.2}"));
        120
    });
}

/// The whole walk, in order.
fn script() -> Script {
    let mut s = Script(VecDeque::new());
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_NARRATION").is_some() {
        return narration_script();
    }
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_RECENT").is_some() {
        return recent_files_script();
    }
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_LIBRARY").is_some() {
        return library_script();
    }
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_LIBRARY_COPY").is_some() {
        return library_copy_script();
    }
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_LIBRARY_DRAG").is_some() {
        return library_drag_script();
    }
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_CONTROL").is_some() {
        s.step(|test| {
            platform::create_control_video_fixture(&test.dir.join("control-video.mp4")).expect("create the control video fixture");
            std::fs::write(test.dir.join("control-video-ready"), b"ready").expect("publish the control video fixture");
            100
        });
        if env("JIFFSHOT_UITEST_CONTROL_CAPTURE").is_some() {
            s.step(|test| {
                let (area, window) = platform::start_control_capture_fixture().expect("create the control capture fixture");
                let focus_window = platform::start_control_focus_fixture().expect("create the control focus fixture");
                let fixture = serde_json::to_vec(&serde_json::json!({ "rect": area, "window": window, "focus_window": focus_window }))
                    .expect("serialize the control capture fixture");
                std::fs::write(test.dir.join("control-capture.json"), fixture).expect("publish the control capture area");
                let directory = test.dir.clone();
                std::thread::spawn(move || {
                    let result = objc2::rc::autoreleasepool(|_| jiffshot_capture::targets::element_at_checked(area.center().0, area.center().1));
                    let value = match result {
                        Ok(Some((name, kind))) => serde_json::json!({ "name": name, "kind": kind }),
                        Ok(None) => serde_json::json!({ "error": "no Accessibility element at the fixture" }),
                        Err(error) => serde_json::json!({ "error": error.to_string() }),
                    };
                    let pending = directory.join("control-element.tmp");
                    let published = directory.join("control-element.json");
                    let result = std::fs::write(&pending, value.to_string()).and_then(|_| std::fs::rename(pending, published));
                    if let Err(error) = result {
                        eprintln!("could not publish the Accessibility fixture result: {error}");
                    }
                });
                300
            });
        }
        s.log(|_| "control fixture ready".to_owned());
        return s;
    }
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_CLIPBOARD").is_some() {
        return clipboard_script();
    }
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_FONTS").is_some() {
        return fonts_script();
    }
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_PANEL_SHUTDOWN").is_some() {
        return panel_shutdown_script();
    }
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_SHARE_PROGRAM").is_some() {
        return share_program_script();
    }
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_SHARE_PREFS").is_some() {
        return share_preferences_window_script();
    }
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_RECORDING_SHUTDOWN").is_some() {
        return recording_shutdown_script();
    }
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_IMAGE_SHUTDOWN").is_some() {
        return image_shutdown_script();
    }
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_TERMINATION_TRAY_ONLY").is_some() {
        return tray_termination_script();
    }
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_ENGINE_QUIT").is_some() {
        return engine_quit_script();
    }
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_OVERLAY").is_some() {
        return overlay_script();
    }
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_SAVED_AREAS").is_some() {
        return saved_areas_script();
    }
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_SETTINGS_HANDOFF").is_some() {
        return capture_settings_handoff_script();
    }
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_STARTUP").is_some() {
        return startup_script();
    }
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_GUIDES").is_some() {
        return video_guides_script();
    }
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_MULTISTREAM_EXCLUSION").is_some() {
        return multistream_exclusion_script();
    }
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_EXCLUSION").is_some() {
        return exclusion_script();
    }
    #[cfg(target_os = "macos")]
    if let Some(case) = env("JIFFSHOT_UITEST_POPUP_SHUTDOWN") {
        return popup_shutdown_script(&case);
    }
    if env("JIFFSHOT_UITEST_CAPTURE_ONLY").is_some() {
        capture_ux_steps(&mut s);
        return s;
    }
    #[cfg(target_os = "macos")]
    native_lifetime_steps(&mut s);
    #[cfg(target_os = "macos")]
    if let Some(expected) = env("JIFFSHOT_UITEST_EXPECT_OPEN") {
        let wait = env("JIFFSHOT_UITEST_OPEN_WAIT_MS").and_then(|value| value.parse::<u64>().ok()).unwrap_or(8000).min(60_000);
        s.log(|_| "waiting for external file activation".to_owned()).wait(wait).step(move |t| {
            let status = sink().status.get_untracked();
            t.line(format!("external file activation: {status}"));
            assert!(status.starts_with("Opened ") && status.contains(&expected), "the external file was not opened");
            0
        });
    }
    let image = env("JIFFSHOT_UITEST_IMAGE");
    editor_walk(&mut s);
    let k = sink();
    s.drag("Arrow", (0.08, 0.12), (0.30, 0.30), 12)
        .drag("Shape", (0.35, 0.10), (0.55, 0.30), 12)
        .step(move |_| {
            set(k.variant, "Rounded".into());
            set(k.stroke_color, "#FF1E88E5".into());
            120
        })
        .drag("Text", (0.60, 0.08), (0.92, 0.18), 12)
        .step(move |_| {
            set(k.text, "Jiffshot text".into());
            120
        })
        .drag("Callout", (0.06, 0.42), (0.30, 0.58), 12)
        .step(move |_| {
            set(k.text, "A callout with a tail".into());
            set(k.fill2, "#FF8E24AA".into());
            set(k.underline, true);
            set(k.v_align, "Middle".into());
            120
        })
        .click("Step", (0.40, 0.45))
        .click("Step", (0.47, 0.45))
        .click("Step", (0.54, 0.45))
        .drag("Pen", (0.62, 0.40), (0.90, 0.55), 30)
        .drag("Highlight", (0.60, 0.62), (0.90, 0.70), 12)
        .drag("Blur", (0.06, 0.68), (0.26, 0.90), 12)
        .drag("Magnify", (0.32, 0.66), (0.46, 0.88), 12)
        .click("Stamp", (0.52, 0.78))
        .drag("Line", (0.60, 0.80), (0.90, 0.92), 12)
        .step(move |_| {
            set(k.shadow, true);
            set(k.shadow_distance, 10.0);
            set(k.end_cap, "Diamond".into());
            120
        })
        .step(move |_| {
            run("tool:Shape");
            set(k.variant, "Polygon".into());
            120
        })
        .step(|t| {
            for (px, py) in [(0.92, 0.30), (0.98, 0.40), (0.90, 0.48)] {
                canvas_feed(POINTER_PRESSED, t.w * px, t.h * py);
                canvas_feed(POINTER_RELEASED, t.w * px, t.h * py);
            }
            run("finish");
            120
        })
        .log(move |_| format!("polygon: selection={} {}", k.selection.get_untracked(), k.document_info.get_untracked()))
        .step(move |_| {
            set(k.variant, "Rectangle".into());
            0
        })
        .shot("ui-1-annotated.png")
        .step(|_| {
            call(|m| m.cycle_theme());
            300
        })
        .log(move |_| format!("theme: {}", k.theme.get_untracked()))
        .shot("ui-1b-light.png")
        .step(|_| {
            call(|m| m.cycle_theme());
            call(|m| m.cycle_theme());
            300
        })
        .log(move |_| format!("theme back: {}", k.theme.get_untracked()))
        // Select, move and restyle the first arrow; undo and redo.
        .drag("Select", (0.19, 0.21), (0.24, 0.26), 12)
        .step(move |_| {
            set(k.stroke_width, 14.0);
            120
        })
        .step(|_| {
            call(|m| m.undo());
            call(|m| m.redo());
            120
        })
        .log(move |_| format!("after edits: {}; title {}", k.document_info.get_untracked(), k.title.get_untracked()))
        .step(|t| {
            run(format!("export-to:{}", t.path("ui-export.png").display()));
            run(format!("export-to:{}", t.path("ui-export.pdf").display()));
            run("deselect");
            120
        })
        .step(|t| {
            // A drag from the canvas's corner outwards (past the document).
            run("tool:Select");
            let (w, h) = (t.w, t.h);
            canvas_feed(POINTER_PRESSED, w, h);
            for i in 1..=12 {
                let f = f64::from(i) / 12.0;
                canvas_feed(POINTER_MOVED, w + 120.0 * f, h + 60.0 * f);
            }
            canvas_feed(POINTER_RELEASED, w + 120.0, h + 60.0);
            120
        })
        .log(move |_| format!("canvas drag: {}", k.document_info.get_untracked()))
        .step(|_| {
            call(|m| m.undo());
            240
        })
        .step(|_| {
            run("effect:Border");
            300
        })
        .step(|_| {
            run("effect:Shadow");
            300
        })
        .shot("ui-2-effects.png")
        .log(move |_| format!("after effects: {}", k.document_info.get_untracked()))
        .drag("Crop", (0.1, 0.1), (0.6, 0.6), 12)
        .shot("ui-3-cropped.png")
        .log(move |_| format!("after crop: {}", k.document_info.get_untracked()))
        .log(move |_| format!("status: {}", k.status.get_untracked()));

    if let Some(fresh) = image.clone() {
        image_tools(&mut s, fresh);
    }
    if let Some(video) = env("JIFFSHOT_UITEST_VIDEO") {
        video_steps(&mut s, video);
    }
    library_steps(&mut s);
    if let Some(drop) = image {
        windows_steps(&mut s, drop);
    }
    s
}

#[cfg(target_os = "macos")]
fn empty_scratch_library(test: &Test) -> PathBuf {
    let ui_dir = test.dir.canonicalize().expect("resolve the test directory");
    let library = PathBuf::from(std::env::var_os("JIFFSHOT_LIBRARY").expect("scratch JIFFSHOT_LIBRARY"));
    let resolved = library.canonicalize().expect("resolve the scratch library");
    assert!(resolved.starts_with(&ui_dir) && resolved != ui_dir, "the library must be inside the test directory");
    assert!(std::fs::read_dir(&resolved).expect("read the scratch library").next().is_none(), "the test library must be empty");
    library
}

#[cfg(target_os = "macos")]
fn create_fixture_video(path: &Path) -> Result<(), String> {
    use jiffshot_capture::media::{Reader, VideoSpec, Writer, HNS_PER_SECOND};

    let spec = VideoSpec::for_size(64, 48, 10);
    let mut writer = Writer::create(path, spec, None)?;
    let frame = [20, 40, 60, 255].repeat((spec.width * spec.height) as usize);
    let step = HNS_PER_SECOND / i64::from(spec.fps);
    for index in 0..5 {
        writer.write_frame(&frame, index * step, step)?;
    }
    writer.finish()?;
    let mut reader = Reader::open(path)?;
    if (reader.width, reader.height) != (spec.width, spec.height) || reader.frame_at(0)?.is_none() {
        return Err("the synthetic recording could not be decoded".into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn share_program_script() -> Script {
    const NAME: &str = "Fixture Share";
    fn share_initial(app: &crate::app::App) -> serde_json::Value {
        let dialog: serde_json::Value = serde_json::from_str(&crate::prefs::dialog(app, "share")).expect("parse the Preferences dialog");
        dialog["share_initial"].clone()
    }

    fn share_request(app: &crate::app::App, mut request: serde_json::Value) -> String {
        request["_share_initial"] = share_initial(app);
        request.to_string()
    }

    fn saved_images(library: &Path) -> Vec<PathBuf> {
        let mut images = Vec::new();
        for folder in std::fs::read_dir(library).expect("read the scratch library") {
            let folder = folder.expect("read a scratch library entry").path();
            if folder.is_dir() {
                for entry in std::fs::read_dir(folder).expect("read a scratch capture folder") {
                    let path = entry.expect("read a scratch capture entry").path();
                    if path.extension().is_some_and(|ext| ext == "png")
                        && !path.file_name().unwrap_or_default().to_string_lossy().ends_with(".cursor.png")
                    {
                        images.push(path);
                    }
                }
            }
        }
        images
    }

    let mut script = Script(VecDeque::new());
    let release = Rc::new(RefCell::new(None::<std::sync::mpsc::Sender<()>>));
    let started = Arc::new(AtomicBool::new(false));
    let expected_share = Rc::new(RefCell::new(None::<(PathBuf, image::RgbaImage)>));
    let expected_video = Rc::new(RefCell::new(None::<Vec<u8>>));
    script.wait(800).step(|test| {
        empty_scratch_library(test);
        let config = PathBuf::from(std::env::var_os("JIFFSHOT_CONFIG_DIR").expect("scratch JIFFSHOT_CONFIG_DIR"));
        let ui_dir = test.dir.canonicalize().expect("resolve the test directory");
        let config = config.canonicalize().expect("resolve the scratch config");
        assert!(config.starts_with(&ui_dir) && config != ui_dir, "the config must be inside the test directory");
        let folder = test.path("applications");
        assert!(folder.join(format!("{NAME}.app")).is_dir(), "the disposable application bundle is missing");
        crate::app::act(|app| app.pick_program_in(Some(&folder))).expect("open the application picker");
        test.line("custom application picker opened");
        30_000
    });
    script.step(|test| {
        let program = test.path("applications").join(format!("{NAME}.app"));
        let saved = crate::settings::Settings::load().programs;
        test.line(format!("picker result: {}", sink().status.get_untracked()));
        assert_eq!(saved, [(NAME.to_owned(), program)]);
        assert!(sink().shares.get_untracked().iter().any(|name| name == NAME));
        crate::app::act(|app| app.share(NAME)).expect("share to the selected application");
        test.line("selected application was saved and published");
        5_000
    });
    let first_share = expected_share.clone();
    script.step(move |test| {
        let received = std::fs::read_to_string(test.path("applications/received.txt"))
            .expect("the disposable application did not receive the image");
        let image_path = PathBuf::from(received.trim());
        let tmp = test.path("tmp").canonicalize().expect("resolve the scratch temporary folder");
        assert!(image_path.canonicalize().expect("resolve the shared image").starts_with(tmp));
        let image = image::open(&image_path).expect("read the shared image").to_rgba8();
        assert!(image.width() > 0 && image.height() > 0);
        *first_share.borrow_mut() = Some((image_path, image));
        std::fs::remove_file(test.path("applications/received.txt")).expect("clear the received-image marker");
        let folder = test.path("applications");
        crate::app::act(|app| app.share_dialog_destination("Open with…", Some(folder)))
            .expect("open the application chooser");
        test.line("the selected application received the exported image");
        20_000
    });
    script.step(move |test| {
        assert_eq!(sink().status.get_untracked(), "Opened in the selected program");
        let received = std::fs::read_to_string(test.path("applications/received.txt"))
            .expect("Open With did not deliver the image");
        let image = PathBuf::from(received.trim());
        let tmp = test.path("tmp").canonicalize().expect("resolve the scratch temporary folder");
        assert!(image.canonicalize().expect("resolve the shared image").starts_with(tmp));
        let delivered = image::open(&image).expect("Open With delivered an unreadable image").to_rgba8();
        let first = expected_share.borrow();
        let (first_path, first_image) = first.as_ref().expect("the first shared image is missing");
        assert_ne!(&image, first_path, "Open With reused another share's temporary file");
        assert_eq!(&delivered, first_image, "Open With changed the exported image");
        std::fs::remove_file(test.path("applications/received.txt")).expect("clear the received-image marker");
        let folder = test.path("applications");
        crate::app::act(|app| app.share_dialog_destination("Open with…", Some(folder)))
            .expect("reopen the application chooser");
        test.line("Open With delivered the capture to the selected application");
        20_000
    });
    script.step(|test| {
        assert_eq!(sink().status.get_untracked(), "Open with… cancelled");
        assert!(!test.path("applications/received.txt").exists(), "cancellation delivered the image");
        crate::app::act(|app| app.share("Open with…")).expect("open the default application chooser");
        test.line("Open With cancellation was reported");
        20_000
    });
    script.step(|test| {
        assert_eq!(sink().status.get_untracked(), "Open with… cancelled");
        assert!(!test.path("applications/received.txt").exists(), "default chooser cancellation delivered the image");
        crate::app::act(|app| app.share_work("Sharing the automatic capture…", || {
            jiffshot_workflow::finish_program_share_selftest(None)
        })).expect("start the automatic image share");
        test.line("default Open With cancellation was reported");
        20_000
    });
    script.step(|test| {
        let status = sink().status.get_untracked();
        assert!(status.contains("opened in the selected program"), "{status}");
        let received = std::fs::read_to_string(test.path("applications/received.txt"))
            .expect("automatic image sharing did not deliver the image");
        let path = PathBuf::from(received.trim());
        let tmp = test.path("tmp").canonicalize().expect("resolve the scratch temporary folder");
        assert!(path.canonicalize().expect("resolve the shared capture").starts_with(tmp));
        let image = image::open(&path).expect("read the automatic image share").to_rgba8();
        assert_eq!(image.dimensions(), (8, 8));
        assert_eq!(image.get_pixel(0, 0).0, [60, 40, 20, 255]);
        assert_eq!(image.get_pixel(2, 3).0, [200, 100, 50, 255], "the editable pointer was not included");
        let originals = saved_images(&test.path("library"));
        assert_eq!(originals.len(), 1, "the capture was not saved once");
        let original = image::open(&originals[0]).expect("read the saved capture").to_rgba8();
        assert_eq!(original.get_pixel(2, 3).0, [60, 40, 20, 255], "the saved capture flattened its editable pointer");
        let (cursor, placement) = jiffshot_capture::library::cursor_paths(&originals[0]);
        let cursor = image::open(cursor).expect("read the saved editable pointer").to_rgba8();
        assert_eq!(cursor.get_pixel(0, 0).0, [200, 100, 50, 255]);
        let placement: jiffshot_capture::library::CursorPlacement =
            serde_json::from_slice(&std::fs::read(placement).expect("read the pointer placement"))
                .expect("parse the pointer placement");
        assert_eq!((placement.x, placement.y), (2.0, 3.0));
        std::fs::remove_file(test.path("applications/received.txt")).expect("clear the received-image marker");
        let video = test.path("automatic-share.mp4");
        crate::app::act(|app| app.share_work("Sharing the automatic recording…", move || {
            create_fixture_video(&video)?;
            jiffshot_workflow::finish_program_share_selftest(Some(&video))
        })).expect("start the automatic recording share");
        test.line("automatic image sharing delivered the editable pointer");
        20_000
    });
    let successful_video = expected_video.clone();
    script.step(move |test| {
        let status = sink().status.get_untracked();
        assert!(status.contains("opened in the selected program"), "{status}");
        let received = std::fs::read_to_string(test.path("applications/received.txt"))
            .expect("automatic recording sharing did not deliver the video");
        assert_eq!(
            PathBuf::from(received.trim()).canonicalize().expect("resolve the shared recording"),
            test.path("automatic-share.mp4").canonicalize().expect("resolve the synthetic recording"),
        );
        *successful_video.borrow_mut() = Some(std::fs::read(test.path("automatic-share.mp4")).expect("read the synthetic recording"));
        std::fs::remove_file(test.path("applications/received.txt")).expect("clear the received-video marker");
        crate::app::act(|app| app.share_work("Sharing the automatic capture…", || {
            jiffshot_workflow::finish_program_share_selftest(None)
        })).expect("start a cancellable automatic image share");
        test.line("automatic recording sharing delivered the recording");
        20_000
    });
    script.step(|test| {
        let status = sink().status.get_untracked();
        assert!(status.contains("saved as") && !status.contains("selected program") && !status.starts_with("Share failed:"), "{status}");
        assert!(!test.path("applications/received.txt").exists(), "cancelled automatic image sharing delivered a file");
        assert_eq!(saved_images(&test.path("library")).len(), 2, "cancellation did not preserve the saved image");
        let video = test.path("automatic-share.mp4");
        crate::app::act(|app| app.share_work("Sharing the automatic recording…", move || {
            jiffshot_workflow::finish_program_share_selftest(Some(&video))
        })).expect("start a cancellable automatic recording share");
        test.line("automatic image sharing cancellation preserved the capture");
        20_000
    });
    script.step(move |test| {
        let status = sink().status.get_untracked();
        assert!(status.contains("Recording saved as") && !status.contains("selected program") && !status.starts_with("Share failed:"), "{status}");
        assert!(!test.path("applications/received.txt").exists(), "cancelled automatic recording sharing delivered a file");
        let video = std::fs::read(test.path("automatic-share.mp4")).expect("read the recording after cancellation");
        assert_eq!(&video, expected_video.borrow().as_ref().expect("the original recording is missing"));
        crate::app::act(|app| {
            app.settings.programs.push(("Missing Fixture".into(), test.path("applications/Missing Fixture.app")));
            app.share("Missing Fixture");
        }).expect("share to a missing application");
        test.line("automatic recording sharing cancellation preserved the recording");
        3_000
    });
    script.step(|test| {
        let status = sink().status.get_untracked();
        assert!(status.starts_with("Share failed:") && status.contains("could not start:"), "{status}");
        let folder = test.path("applications");
        crate::app::act(|app| {
            app.settings.programs.retain(|(name, _)| name != "Missing Fixture");
            app.pick_program_in(Some(&folder));
        }).expect("reopen the application picker");
        test.line("missing application errors were reported");
        20_000
    });
    script.step(|test| {
        assert_eq!(sink().status.get_untracked(), "Application selection cancelled");
        assert_eq!(crate::settings::Settings::load().programs.len(), 1);
        crate::app::act(|app| app.add_share_program(test.path("Preview.app"))).expect("try a reserved name");
        test.line("application picker cancellation left the destination unchanged");
        150
    });
    script.step(|test| {
        assert!(sink().status.get_untracked().contains("already used by a built-in share destination"));
        let config = PathBuf::from(std::env::var_os("JIFFSHOT_CONFIG_DIR").unwrap()).join(jiffshot_capture::config::FILE);
        let backup = config.with_extension("json.fixture-backup");
        std::fs::rename(&config, &backup).expect("save the scratch settings");
        std::fs::create_dir(&config).expect("block the scratch settings file");
        crate::app::act(|app| {
            let previous = app.settings.programs.clone();
            app.add_share_program(test.path("Unsaveable.app"));
            assert_eq!(app.settings.programs, previous, "a failed save changed the destination list");
        }).expect("attempt to add the application");
        std::fs::remove_dir(&config).expect("unblock the scratch settings file");
        std::fs::rename(&backup, &config).expect("restore the scratch settings");
        test.line("reserved destination names were rejected");
        150
    });
    script.step(|test| {
        assert!(sink().status.get_untracked().starts_with("Could not add Unsaveable:"));
        assert_eq!(crate::settings::Settings::load().programs.len(), 1);
        let second = test.path("applications/Second Fixture.app");
        std::fs::create_dir(&second).expect("create the second scratch bundle");
        crate::app::act(|app| app.add_share_program(second)).expect("add a second destination");
        test.line("settings failures were reported without changing saved destinations");
        150
    });
    script.step(|test| {
        crate::app::act(|app| {
            let initial = share_initial(app);
            let third = test.path("applications/Concurrent Fixture.app");
            std::fs::create_dir(&third).expect("create the concurrent scratch bundle");
            app.add_share_program(third);
            let current = (app.settings.ftp.clone(), app.settings.programs.clone());
            assert_eq!(current.1.len(), 3);

            let unchanged = serde_json::json!({
                "_share_initial": initial,
                "share": ["", null, null, true, true]
            });
            crate::prefs::apply(app, &unchanged.to_string());
            assert_eq!((app.settings.ftp.clone(), app.settings.programs.clone()), current);

            let stale = serde_json::json!({
                "_share_initial": unchanged["_share_initial"],
                "share": ["ftp://user@example.test/images", null, null, false, true]
            });
            crate::prefs::apply(app, &stale.to_string());
            assert_eq!((app.settings.ftp.clone(), app.settings.programs.clone()), current);
        }).expect("apply stale Share preferences");
        150
    });
    script.step(|test| {
        assert!(sink().status.get_untracked().contains("Share destinations changed while Preferences was open"));
        let saved = crate::settings::Settings::load();
        assert_eq!(saved.programs.len(), 3);
        assert!(saved.ftp.is_none());
        crate::app::act(|app| assert!(app.save_share_preferences(None, &["Concurrent Fixture".into()])))
            .expect("remove the concurrent scratch destination");
        test.line("stale Share preferences preserved concurrent destination changes");
        150
    });
    script.step(|_| {
        assert_eq!(crate::settings::Settings::load().programs.len(), 2);
        let config = PathBuf::from(std::env::var_os("JIFFSHOT_CONFIG_DIR").unwrap()).join(jiffshot_capture::config::FILE);
        let backup = config.with_extension("json.fixture-backup");
        std::fs::rename(&config, &backup).expect("save the scratch settings");
        std::fs::create_dir(&config).expect("block the scratch settings file");
        crate::app::act(|app| {
            let previous = (app.settings.ftp.clone(), app.settings.programs.clone());
            let request = share_request(app, serde_json::json!({
                "capture": [0], "editor": [0, true, true, true, 0],
                "share": ["ftp://user@example.test/images", null, null, false, false]
            }));
            crate::prefs::apply(app, &request);
            assert_eq!((app.settings.ftp.clone(), app.settings.programs.clone()), previous, "a failed save changed Share preferences");
        }).expect("attempt to remove both destinations");
        std::fs::remove_dir(&config).expect("unblock the scratch settings file");
        std::fs::rename(&backup, &config).expect("restore the scratch settings");
        150
    });
    script.step(|test| {
        assert!(sink().status.get_untracked().starts_with("Could not save Share preferences:"));
        assert_eq!(crate::settings::Settings::load().programs.len(), 2);
        for name in [NAME, "Second Fixture"] {
            assert!(sink().shares.get_untracked().iter().any(|item| item == name));
        }
        test.line("failed removal preserved all destinations");
        let config = PathBuf::from(std::env::var_os("JIFFSHOT_CONFIG_DIR").unwrap()).join(jiffshot_capture::config::FILE);
        let backup = config.with_extension("json.fixture-backup");
        std::fs::rename(&config, &backup).expect("save the scratch settings");
        std::fs::write(&config, b"{invalid").expect("write malformed scratch settings");
        crate::app::act(|app| {
            let previous = (app.settings.ftp.clone(), app.settings.programs.clone());
            let request = share_request(app, serde_json::json!({
                "capture": [0], "editor": [0, true, true, true, 0],
                "share": ["ftp://user@example.test/images", null, null, false, false]
            }));
            crate::prefs::apply(app, &request);
            assert_eq!((app.settings.ftp.clone(), app.settings.programs.clone()), previous);
        }).expect("attempt Preferences with malformed settings");
        assert_eq!(std::fs::read(&config).expect("read malformed settings"), b"{invalid");
        std::fs::remove_file(&config).expect("remove malformed scratch settings");
        std::fs::rename(&backup, &config).expect("restore the scratch settings");
        150
    });
    script.step(|test| {
        assert!(sink().status.get_untracked().starts_with("Could not save Share preferences:"));
        let saved = crate::settings::Settings::load();
        assert!(saved.ftp.is_none());
        assert_eq!(saved.programs.len(), 2);
        test.line("malformed settings were not overwritten by changed Share preferences");
        let config = PathBuf::from(std::env::var_os("JIFFSHOT_CONFIG_DIR").unwrap()).join(jiffshot_capture::config::FILE);
        let backup = config.with_extension("json.fixture-backup");
        std::fs::rename(&config, &backup).expect("save the scratch settings");
        std::fs::write(&config, b"{invalid").expect("write malformed scratch settings");
        crate::app::act(|app| {
            let previous = (app.settings.theme.clone(), app.settings.layout.labels, app.settings.layout.side_bar, app.settings.layout.tray);
            let request = share_request(app, serde_json::json!({
                "editor": [1, false, false, false, 2],
                "share": ["", null, null, true, true]
            }));
            crate::prefs::apply(app, &request);
            assert_eq!((app.settings.theme.clone(), app.settings.layout.labels, app.settings.layout.side_bar, app.settings.layout.tray), previous);
        }).expect("attempt unrelated Preferences with malformed settings");
        assert_eq!(std::fs::read(&config).expect("read malformed settings"), b"{invalid");
        std::fs::remove_file(&config).expect("remove malformed scratch settings");
        std::fs::rename(&backup, &config).expect("restore the scratch settings");
        150
    });
    script.step(|test| {
        assert!(sink().status.get_untracked().starts_with("Could not apply Preferences:"));
        test.line("malformed settings were not overwritten by unrelated preferences");
        let config = PathBuf::from(std::env::var_os("JIFFSHOT_CONFIG_DIR").unwrap()).join(jiffshot_capture::config::FILE);
        let before = std::fs::read(&config).expect("read the scratch settings");
        let blocked = config.with_file_name(format!("{}.tmp", jiffshot_capture::config::FILE));
        std::fs::create_dir(&blocked).expect("block the scratch settings temporary file");
        crate::app::act(|app| {
            let previous = (app.settings.theme.clone(), app.settings.layout.labels, app.settings.layout.side_bar, app.settings.layout.tray, app.settings.export_format.clone());
            let request = share_request(app, serde_json::json!({
                "editor": [1, false, false, false, 2],
                "share": ["", null, null, true, true]
            }));
            crate::prefs::apply(app, &request);
            assert_eq!((app.settings.theme.clone(), app.settings.layout.labels, app.settings.layout.side_bar, app.settings.layout.tray, app.settings.export_format.clone()), previous);
        }).expect("attempt to save Editor preferences");
        std::fs::remove_dir(&blocked).expect("unblock the scratch settings temporary file");
        assert_eq!(std::fs::read(&config).expect("read the scratch settings"), before);
        150
    });
    script.step(|test| {
        assert!(sink().status.get_untracked().starts_with("Could not save Editor preferences:"));
        let saved = crate::settings::Settings::load();
        assert_eq!(saved.programs.len(), 2);
        assert!(saved.ftp.is_none());
        test.line("failed Editor preferences preserved the previous settings");
        crate::app::act(|app| {
            let request = share_request(app, serde_json::json!({
                "editor": [1, false, true, false, 2],
                "share": ["", null, null, true, true]
            }));
            crate::prefs::apply(app, &request);
        }).expect("save Editor preferences");
        150
    });
    script.step(|test| {
        let saved = crate::settings::Settings::load();
        assert_eq!(saved.theme.as_deref(), Some("Light"));
        assert!(!saved.layout.labels && saved.layout.side_bar && !saved.layout.tray);
        assert_eq!(saved.export_format.as_deref(), Some("jpg"));
        assert_eq!(saved.programs.len(), 2);
        assert!(saved.ftp.is_none());
        test.line("Editor preferences saved together without changing Share");
        crate::app::act(|app| {
            let request = share_request(app, serde_json::json!({
                "share": ["ftp://user@example.test/images", null, null, false, false]
            }));
            crate::prefs::apply(app, &request);
        }).expect("save FTP and remove both destinations");
        150
    });
    let release_sender = release.clone();
    let worker_started = started.clone();
    script.step(move |test| {
        assert_eq!(sink().status.get_untracked(), "Share preferences saved");
        let saved = crate::settings::Settings::load();
        assert_eq!(saved.ftp.as_deref(), Some("ftp://user@example.test/images"));
        assert!(saved.programs.is_empty());
        for name in [NAME, "Second Fixture"] {
            assert!(!sink().shares.get_untracked().iter().any(|item| item == name));
        }
        test.line("FTP and multiple destination removals were saved together");
        let (sender, receiver) = std::sync::mpsc::channel();
        *release_sender.borrow_mut() = Some(sender);
        crate::app::act(|app| app.share_work("Waiting for the synthetic share…", move || {
            worker_started.store(true, Ordering::Release);
            receiver.recv_timeout(Duration::from_secs(10)).map_err(|error| error.to_string())?;
            Ok("Synthetic share completed".into())
        })).expect("start the synthetic share");
        500
    });
    script.step(move |test| {
        assert!(started.load(Ordering::Acquire), "the sharing worker did not start");
        assert_eq!(sink().status.get_untracked(), "Waiting for the synthetic share…");
        release.borrow_mut().take().expect("the share release sender is missing").send(()).expect("release the share worker");
        test.line("the UI remained responsive while a share was blocked");
        500
    });
    script.step(|test| {
        assert_eq!(sink().status.get_untracked(), "Synthetic share completed");
        crate::app::act(|app| {
            app.settings.ftp = Some("invalid://fixture".into());
            app.share("FTP");
        }).expect("start an invalid FTP share");
        test.line("the sharing worker reported completion");
        500
    });
    script.step(|test| {
        assert!(sink().status.get_untracked().contains("the FTP address must start with ftp://"));
        test.line("FTP errors were reported by the sharing worker");
        crate::app::act(|app| app.share("Folder…")).expect("open the folder picker");
        20_000
    });
    script.step(|test| {
        assert_eq!(sink().status.get_untracked(), "Folder sharing cancelled");
        test.line("folder picker cancellation was reported");
        crate::app::act(|app| app.share("Folder…")).expect("reopen the folder picker");
        20_000
    });
    script.step(|test| {
        let name = std::env::var("JIFFSHOT_UITEST_SHARE_FOLDER").expect("the fixture folder name is missing");
        let folder = test.path("applications").join(name);
        assert_eq!(std::fs::read(folder.join("Untitled.png")).expect("read the existing fixture"), b"existing fixture");
        let copied = folder.join("Untitled (2).png");
        let image = image::open(&copied).expect("read the shared image");
        assert!(image.width() > 0 && image.height() > 0);
        assert_eq!(sink().status.get_untracked(), format!("Copied to {}", copied.display()));
        test.line("folder sharing copied the image without replacing an existing file");
        0
    });
    script
}

#[cfg(target_os = "macos")]
fn share_preferences_window_script() -> Script {
    const LEGACY_FTP: &str = "  ftp://fixture@example.test/captures  ";
    let mut script = Script(VecDeque::new());
    script.wait(800).step(|test| {
        empty_scratch_library(test);
        let applications = test.path("applications");
        std::fs::create_dir(&applications).expect("create the scratch applications folder");
        for name in ["First Fixture", "Second Fixture", "Concurrent Fixture"] {
            std::fs::create_dir(applications.join(format!("{name}.app"))).expect("create a scratch application bundle");
        }
        crate::app::act(|app| {
            app.add_share_program(applications.join("First Fixture.app"));
            app.add_share_program(applications.join("Second Fixture.app"));
            app.settings.ftp = Some(LEGACY_FTP.into());
            app.settings.try_save().expect("save the legacy scratch FTP value");
            app.open_prefs("share");
        }).expect("open Share preferences");
        1_000
    });
    script.step(|test| {
        assert!(super::prefs_window::showing_for_selftest(), "Preferences did not open");
        crate::app::act(|app| app.add_share_program(test.path("applications/Concurrent Fixture.app")))
            .expect("change Share destinations while Preferences is open");
        test.line("ready to submit stale Share preferences");
        15_000
    });
    script.step(|test| {
        assert!(sink().status.get_untracked().contains("Share destinations changed while Preferences was open"));
        let saved = crate::settings::Settings::load();
        assert_eq!(saved.programs.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            ["First Fixture", "Second Fixture", "Concurrent Fixture"]);
        assert_eq!(saved.ftp.as_deref(), Some(LEGACY_FTP));
        assert!(!super::prefs_window::showing_for_selftest(), "Preferences did not close");
        test.line("the Preferences window rejected a stale Share removal");
        crate::app::act(|app| app.open_prefs("share")).expect("reopen Share preferences");
        1_000
    });
    script.step(|test| {
        assert!(super::prefs_window::showing_for_selftest(), "Preferences did not reopen");
        test.line("ready to submit current Share preferences");
        15_000
    });
    script.step(|test| {
        let saved = crate::settings::Settings::load();
        assert_eq!(saved.programs.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            ["Second Fixture", "Concurrent Fixture"]);
        assert_eq!(saved.ftp.as_deref(), Some(LEGACY_FTP));
        assert!(!super::prefs_window::showing_for_selftest(), "Preferences did not close");
        test.line("the Preferences window removed only the selected destination");
        crate::app::act(|app| app.open_prefs("share")).expect("reopen Share preferences to correct FTP");
        1_000
    });
    script.step(|test| {
        assert!(super::prefs_window::showing_for_selftest(), "Preferences did not reopen");
        test.line("ready to correct the legacy FTP value");
        15_000
    });
    script.step(|test| {
        let saved = crate::settings::Settings::load();
        assert_eq!(saved.ftp.as_deref(), Some("ftp://fixture@example.test/captures"));
        assert_eq!(saved.programs.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            ["Second Fixture", "Concurrent Fixture"]);
        assert!(!super::prefs_window::showing_for_selftest(), "Preferences did not close");
        test.line("the Preferences window corrected the legacy FTP value");
        crate::app::act(|app| app.open_prefs("share")).expect("reopen Share preferences to clear FTP");
        1_000
    });
    script.step(|test| {
        assert!(super::prefs_window::showing_for_selftest(), "Preferences did not reopen");
        test.line("ready to clear the FTP value");
        15_000
    });
    script.step(|test| {
        let saved = crate::settings::Settings::load();
        assert!(saved.ftp.is_none());
        assert_eq!(saved.programs.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            ["Second Fixture", "Concurrent Fixture"]);
        assert!(!super::prefs_window::showing_for_selftest(), "Preferences did not close");
        test.line("the Preferences window cleared the FTP value");
        0
    });
    script
}

#[cfg(target_os = "macos")]
fn library_copy_script() -> Script {
    use image::GenericImageView;
    use jiffshot_capture::clipboard::{self, Pasted};

    const POINTER: &str = "pointer 雪.png";
    const SAVED: &str = "saved.png";
    const CURRENT: &str = "current.png";
    const VIDEO: &str = "recording.mp4";
    let copied_files = Rc::new(RefCell::new(Vec::<PathBuf>::new()));
    let original_video = Rc::new(RefCell::new(Vec::<u8>::new()));
    let mut script = Script(VecDeque::new());

    let video_bytes = original_video.clone();
    script.wait(800).step(move |test| {
        let folder = empty_scratch_library(test).join("2026-01");
        std::fs::create_dir(&folder).expect("create the scratch capture folder");
        for (name, width, height) in [(POINTER, 5, 4), (SAVED, 5, 4), (CURRENT, 3, 2)] {
            let mut frame = jiffshot_capture::Frame::new(width, height);
            for pixel in frame.pixels.chunks_exact_mut(4) {
                pixel.copy_from_slice(&[30, 40, 50, 255]);
            }
            frame.save_png(&folder.join(name)).expect("save the synthetic capture");
        }
        let mut pointer = jiffshot_capture::Frame::new(1, 1);
        pointer.pixels.copy_from_slice(&[0, 0, 255, 255]);
        jiffshot_capture::library::write_cursor(&folder.join(POINTER), &pointer, (1, 1)).expect("save the editable pointer");
        let edits = jiffshot_doc::Document::blank(7, 6, jiffshot_doc::Rgba::WHITE);
        jiffshot_doc::io::save_document(&edits, &folder.join(SAVED).with_extension("jsd")).expect("save the synthetic edits");
        create_fixture_video(&folder.join(VIDEO)).expect("create a decodable synthetic recording");
        *video_bytes.borrow_mut() = std::fs::read(folder.join(VIDEO)).expect("read the synthetic recording");
        crate::app::act(|app| {
            app.refresh_library();
            app.open_path(&folder.join(CURRENT));
            app.resize_to(9, 8);
        }).expect("open and edit the current capture");
        500
    });
    script.step(|_| {
        assert_eq!(sink().library.get_untracked().len(), 4);
        library_fixture_item(&library_fixture_path(CURRENT)).model.run(|model| model.copy());
        1_000
    });
    script.step(|test| {
        let Some(Pasted::Image { width, height, .. }) = clipboard::read(0).expect("read the private pasteboard") else {
            panic!("the current image was not copied as pixels");
        };
        assert_eq!((width, height), (9, 8));
        assert_eq!(image::open(library_fixture_path(CURRENT)).unwrap().dimensions(), (3, 2));
        assert_eq!(crate::canvas::with_editor(|editor| editor.is_modified()), Some(true));
        library_fixture_item(&library_fixture_path(SAVED)).model.run(|model| model.copy());
        test.line("library Copy included the current unsaved edits");
        1_000
    });
    script.step(|test| {
        let Some(Pasted::Image { width, height, rgba }) = clipboard::read(0).expect("read the saved image") else {
            panic!("the saved image was not copied as pixels");
        };
        assert_eq!((width, height), (7, 6));
        assert_eq!(&rgba[..4], &[255, 255, 255, 255]);
        library_fixture_item(&library_fixture_path(POINTER)).model.run(|model| model.copy());
        test.line("library Copy included saved document edits");
        1_000
    });
    script.step(|test| {
        let Some(Pasted::Image { width, height, rgba }) = clipboard::read(0).expect("read the pointer image") else {
            panic!("the pointer capture was not copied as pixels");
        };
        assert_eq!((width, height), (5, 4));
        assert_eq!(&rgba[(5 + 1) * 4..(5 + 2) * 4], &[255, 0, 0, 255]);
        library_fixture_item(&library_fixture_path(VIDEO)).model.run(|model| model.copy());
        test.line("library Copy included the separate editable pointer");
        1_000
    });
    let video_bytes = original_video.clone();
    script.step(move |test| {
        let Some(Pasted::Files(files)) = clipboard::read(0).expect("read the video file URL") else {
            panic!("the recording was not copied as a file");
        };
        assert_eq!(files, [std::fs::canonicalize(library_fixture_path(VIDEO)).unwrap()]);
        assert_eq!(std::fs::read(&files[0]).unwrap(), *video_bytes.borrow());
        for name in [POINTER, SAVED, VIDEO] {
            library_fixture_item(&library_fixture_path(name)).model.run(|model| model.toggle_select());
        }
        test.line("library Copy represented a recording as a file");
        500
    });
    script.step(|_| {
        assert_eq!(sink().library_selected.get_untracked(), 3);
        library_fixture_item(&library_fixture_path(POINTER)).model.run(|model| model.copy());
        1_000
    });
    let saved_files = copied_files.clone();
    let video_bytes = original_video.clone();
    script.step(move |test| {
        let Some(Pasted::Files(files)) = clipboard::read(0).expect("read selected file URLs") else {
            panic!("the selected captures were not copied as files");
        };
        assert_eq!(files.len(), 3);
        let tmp = test.path("tmp").canonicalize().unwrap();
        let pointer = files.iter().find(|path| path.file_name().is_some_and(|name| name == POINTER)).expect("copied pointer image");
        let saved = files.iter().find(|path| path.file_name().is_some_and(|name| name == SAVED)).expect("copied saved image");
        for path in [pointer, saved] {
            assert!(path.canonicalize().unwrap().starts_with(&tmp));
        }
        let pointer_image = image::open(pointer).unwrap().to_rgba8();
        assert_eq!(pointer_image.dimensions(), (5, 4));
        assert_eq!(pointer_image.get_pixel(1, 1).0, [255, 0, 0, 255]);
        assert_eq!(image::open(saved).unwrap().dimensions(), (7, 6));
        let video = std::fs::canonicalize(library_fixture_path(VIDEO)).unwrap();
        assert!(files.contains(&video));
        assert_eq!(std::fs::read(&video).unwrap(), *video_bytes.borrow());
        assert_eq!(image::open(library_fixture_path(POINTER)).unwrap().to_rgba8().get_pixel(1, 1).0, [50, 40, 30, 255]);
        assert!(library_fixture_path(POINTER).with_extension("cursor.png").exists());
        assert!(library_fixture_path(POINTER).with_extension("cursor.json").exists());
        *saved_files.borrow_mut() = files;
        std::fs::write(library_fixture_path(SAVED).with_extension("jsd"), b"invalid edits").unwrap();
        library_fixture_item(&library_fixture_path(SAVED)).model.run(|model| model.copy());
        test.line("multi-selection copied flattened snapshots and retained the original files");
        1_000
    });
    script.step(move |test| {
        assert!(sink().status.get_untracked().starts_with("Copy failed:"), "{}", sink().status.get_untracked());
        let Some(Pasted::Files(files)) = clipboard::read(0).expect("read the previous file URLs") else {
            panic!("a failed copy replaced the previous clipboard");
        };
        assert_eq!(files, *copied_files.borrow());
        let edits = jiffshot_doc::Document::blank(7, 6, jiffshot_doc::Rgba::WHITE);
        jiffshot_doc::io::save_document(&edits, &library_fixture_path(SAVED).with_extension("jsd")).unwrap();
        crate::app::act(|app| {
            app.clear_library_selection();
            app.library_action(&library_fixture_path(POINTER), "copy", None);
            clipboard::copy_text("newer clipboard content", 0).unwrap();
        }).unwrap();
        test.line("failed preparation left the previous clipboard intact");
        1_000
    });
    script.step(|test| {
        assert!(sink().status.get_untracked().starts_with("Copy cancelled because the clipboard changed"), "{}", sink().status.get_untracked());
        assert!(matches!(clipboard::read(0).unwrap(), Some(Pasted::Text(text)) if text == "newer clipboard content"));
        crate::app::act(|app| {
            app.retry_library_copy_for_selftest(library_fixture_path(POINTER)).expect("queue a copy while the editor is busy");
        }).unwrap();
        test.line("a newer clipboard change was not overwritten");
        500
    });
    script.step(|test| {
        let Some(Pasted::Image { width, height, rgba }) = clipboard::read(0).expect("read the retried copy") else {
            panic!("the busy copy was not delivered");
        };
        assert_eq!((width, height), (5, 4));
        assert_eq!(&rgba[(5 + 1) * 4..(5 + 2) * 4], &[255, 0, 0, 255]);
        assert!(sink().status.get_untracked().starts_with("Copied the image"), "{}", sink().status.get_untracked());
        crate::app::act(|app| {
            app.library_action(&library_fixture_path(POINTER), "copy", None);
            app.library_action(&library_fixture_path(SAVED), "copy", None);
        }).unwrap();
        test.line("library Copy retried while the editor was busy");
        1_000
    });
    script.step(|test| {
        let Some(Pasted::Image { width, height, .. }) = clipboard::read(0).expect("read the latest copy") else {
            panic!("the latest request did not copy an image");
        };
        assert_eq!((width, height), (7, 6), "an earlier request overwrote the latest copy");
        let edits = library_fixture_path(SAVED).with_extension("jsd");
        crate::app::act(|app| {
            app.open_path(&edits);
            app.resize_to(11, 10);
        }).unwrap();
        library_fixture_item(&library_fixture_path(SAVED)).model.run(|model| model.copy());
        test.line("the latest library Copy request won");
        1_000
    });
    script.step(|test| {
        let Some(Pasted::Image { width, height, .. }) = clipboard::read(0).expect("read the directly opened edits") else {
            panic!("the directly opened edits were not copied");
        };
        assert_eq!((width, height), (11, 10));
        let edits = library_fixture_path(SAVED).with_extension("jsd");
        assert_eq!(jiffshot_doc::io::load_document(&edits).unwrap().size(), (7, 6));
        if env("JIFFSHOT_UITEST_LIBRARY_COPY_AUTOSAVE_FAILURE").is_some() {
            std::fs::rename(&edits, edits.with_extension("jsd.saved")).expect("preserve the previous edits");
            std::fs::create_dir(&edits).expect("block the final autosave");
            test.line("blocked the final autosave without changing the prior document");
        }
        test.line("library Copy included unsaved edits opened directly as a document");
        0
    });
    script
}

#[cfg(target_os = "macos")]
#[derive(Default)]
struct LibraryDragFixture {
    window: Option<rustolonia::Window>,
    number: isize,
    gate: Option<std::sync::mpsc::Sender<()>>,
    receiver: Option<std::process::Child>,
    receiver_error: Option<String>,
    global_drag: Option<platform::GlobalDrag>,
    previous_events: usize,
    token: i64,
}

#[cfg(target_os = "macos")]
impl LibraryDragFixture {
    fn mount(&mut self, path: &Path) {
        let scope = super::scope().expect("the application scope is unavailable");
        let key = library_fixture_item(path).key;
        let window = super::main_window::library_tile_for_selftest(&scope, key).expect("mount the library drag tile");
        let handle = scope.platform_handle(&window).expect("get the fixture window handle");
        self.number = crate::mac_window::number(handle).expect("get the fixture window number") as isize;
        crate::mac_window::activate(handle).expect("activate the fixture window");
        self.window = Some(window);
    }

    fn mouse(&self, kind: objc2_app_kit::NSEventType, x: f64, y: f64) {
        platform::post_drag_mouse(self.number, kind, x, y).expect("send a mouse event to the fixture window");
    }

    fn click(&self, move_distance: f64) {
        use objc2_app_kit::NSEventType;
        self.mouse(NSEventType::LeftMouseDown, 50.0, 50.0);
        if move_distance != 0.0 {
            self.mouse(NSEventType::LeftMouseDragged, 50.0 + move_distance, 50.0);
        }
        self.mouse(NSEventType::LeftMouseUp, 50.0 + move_distance, 50.0);
    }

    fn start_pending(&mut self, path: &Path) {
        use crate::file_drag_macos::TestEvent;
        use objc2_app_kit::NSEventType;
        assert!(self.gate.is_none(), "a previous fixture preparation is still held");
        let previous = crate::file_drag_macos::events_for_selftest().len();
        self.gate = Some(crate::file_drag_macos::hold_preparation_for_selftest());
        self.mouse(NSEventType::LeftMouseDown, 50.0, 50.0);
        self.mouse(NSEventType::LeftMouseDragged, 70.0, 50.0);
        let events = crate::file_drag_macos::events_for_selftest();
        let preparing = events.get(previous..).unwrap_or_default().iter().filter_map(|event| match event {
            TestEvent::Preparing { token, path: actual } => Some((*token, actual)),
            _ => None,
        }).collect::<Vec<_>>();
        assert_eq!(preparing.len(), 1, "the tile did not begin exactly one drag: {events:?}");
        assert_eq!(preparing[0].1, path);
        self.token = preparing[0].0;
    }

    fn release_preparation(&mut self) {
        self.gate.take().expect("the fixture preparation was not held").send(()).expect("release the fixture preparation");
    }

    fn start_global_pending(&mut self) {
        assert!(self.gate.is_none(), "a previous fixture preparation is still held");
        self.previous_events = crate::file_drag_macos::events_for_selftest().len();
        self.gate = Some(crate::file_drag_macos::hold_preparation_for_selftest());
        self.global_drag = Some(platform::GlobalDrag::begin(self.number).expect("start the controlled global drag"));
    }

    fn check_global_pending(&mut self, path: &Path) {
        use crate::file_drag_macos::TestEvent;
        let events = crate::file_drag_macos::events_for_selftest();
        let preparing = events.get(self.previous_events..).unwrap_or_default().iter().filter_map(|event| match event {
            TestEvent::Preparing { token, path: actual } => Some((*token, actual)),
            _ => None,
        }).collect::<Vec<_>>();
        assert_eq!(preparing.len(), 1, "the native gesture did not begin exactly one drag: {events:?}");
        assert_eq!(preparing[0].1, path);
        self.token = preparing[0].0;
    }
}

#[cfg(target_os = "macos")]
fn assert_drag_completed(token: i64) {
    use crate::file_drag_macos::TestEvent;
    let events = crate::file_drag_macos::events_for_selftest();
    let completions = events.iter().filter_map(|event| match event {
        TestEvent::Completed { token: actual, effect, status } if *actual == token => Some((*effect, *status)),
        _ => None,
    }).collect::<Vec<_>>();
    assert_eq!(completions, [(0, 0)], "expected one successful no-drop completion: {events:?}");
}

#[cfg(target_os = "macos")]
fn assert_stale_drag_discarded(token: i64) {
    use crate::file_drag_macos::TestEvent;
    let events = crate::file_drag_macos::events_for_selftest();
    assert!(!events.iter().any(|event| matches!(event, TestEvent::Submitted { token: actual, .. } if *actual == token)),
        "a cancelled preparation was submitted: {events:?}");
    let discarded = events.iter().filter_map(|event| match event {
        TestEvent::Discarded { token: actual, files } if *actual == token => Some(files),
        _ => None,
    }).collect::<Vec<_>>();
    assert_eq!(discarded.len(), 1, "the stale preparation was not discarded: {events:?}");
    assert!(!discarded[0].is_empty());
    assert!(discarded[0].iter().all(|path| !path.exists()), "cancelled exports were retained: {:?}", discarded[0]);
}

#[cfg(target_os = "macos")]
fn library_drag_script() -> Script {
    use crate::file_drag_macos::TestEvent;
    use objc2_app_kit::NSEventType;

    const FIRST: &str = "first.png";
    const SECOND: &str = "second 雪.png";
    let fixture = Rc::new(RefCell::new(LibraryDragFixture::default()));
    let mut script = Script(VecDeque::new());

    let state = fixture.clone();
    script.wait(800).step(move |test| {
        let folder = empty_scratch_library(test).join("2026-01");
        std::fs::create_dir(&folder).expect("create the scratch capture folder");
        for (name, width, height) in [(FIRST, 3, 2), (SECOND, 5, 4)] {
            jiffshot_capture::Frame::new(width, height).save_png(&folder.join(name)).expect("save the synthetic capture");
        }
        crate::app::act(|app| {
            app.refresh_library();
            app.open_path(&folder.join(FIRST));
        }).expect("open the first capture");
        platform::observe_native_file_drag().expect("install the native drag observer");
        state.borrow_mut().mount(&folder.join(SECOND));
        test.line("mounted the production library tile in an isolated window");
        1_000
    });
    let state = fixture.clone();
    script.step(move |_| {
        assert!(crate::file_drag_macos::registered_for_selftest(&library_fixture_path(SECOND)) > 0);
        assert_eq!(super::canvas().expect("editor canvas").document_size(), (3, 2));
        state.borrow().click(2.0);
        500
    });
    let state = fixture.clone();
    script.step(move |test| {
        assert_eq!(super::canvas().expect("editor canvas").document_size(), (5, 4), "a below-threshold gesture did not open the tile");
        assert!(crate::file_drag_macos::events_for_selftest().is_empty(), "a below-threshold gesture began a drag");
        crate::app::act(|app| app.open_path(&library_fixture_path(FIRST))).expect("reopen the first capture");
        state.borrow_mut().start_pending(&library_fixture_path(SECOND));
        test.line("a below-threshold gesture preserved the normal tile click");
        1_000
    });
    let state = fixture.clone();
    script.step(move |test| {
        assert!(crate::file_drag_macos::preparation_waiting_for_selftest(), "the preparation worker did not reach the fixture gate");
        assert!(crate::file_drag_macos::has_pending().expect("inspect pending drags"));
        crate::app::act(|app| app.exit()).expect("request quit during preparation");
        let toast = sink().toast.get_untracked();
        assert!(toast.contains("Could not quit Jiffshot") && toast.contains("library drag"), "{toast}");
        {
            let fixture = state.borrow();
            fixture.mouse(NSEventType::LeftMouseDragged, 51.0, 50.0);
            fixture.mouse(NSEventType::LeftMouseUp, 51.0, 50.0);
            assert_eq!(super::canvas().expect("editor canvas").document_size(), (3, 2), "a completed drag gesture also opened the tile");
            assert_drag_completed(fixture.token);
        }
        state.borrow_mut().release_preparation();
        test.line("pending drag vetoed quit and suppressed a return-inside tap");
        1_500
    });
    let state = fixture.clone();
    script.step(move |test| {
        let state = state.borrow();
        assert_stale_drag_discarded(state.token);
        assert!(!crate::file_drag_macos::has_pending().expect("inspect finished drags"));
        state.click(0.0);
        test.line("late preparation was discarded after button release");
        500
    });
    script.step(|test| {
        assert_eq!(super::canvas().expect("editor canvas").document_size(), (5, 4), "the next normal click did not recover");
        crate::app::act(|app| {
            app.open_path(&library_fixture_path(FIRST));
            app.resize_to(9, 8);
        }).expect("edit the first capture");
        for name in [FIRST, SECOND] {
            library_fixture_item(&library_fixture_path(name)).model.run(|model| model.toggle_select());
        }
        test.line("normal clicks recovered after cancellation");
        500
    });
    let state = fixture.clone();
    script.step(move |_| {
        assert_eq!(sink().library_selected.get_untracked(), 2);
        state.borrow_mut().start_pending(&library_fixture_path(SECOND));
        1_000
    });
    let state = fixture.clone();
    script.step(move |_| {
        assert!(crate::file_drag_macos::preparation_waiting_for_selftest(), "the multi-file worker did not reach the gate");
        platform::cancel_next_native_file_drag();
        state.borrow_mut().release_preparation();
        2_500
    });
    let state = fixture.clone();
    script.step(move |test| {
        let token = state.borrow().token;
        assert_drag_completed(token);
        let events = crate::file_drag_macos::events_for_selftest();
        let submissions = events.iter().filter_map(|event| match event {
            TestEvent::Submitted { token: actual, files } if *actual == token => Some(files),
            _ => None,
        }).collect::<Vec<_>>();
        assert_eq!(submissions.len(), 1, "the multi-file drag was not submitted exactly once: {events:?}");
        let observations = platform::native_file_drags();
        assert_eq!(observations.len(), 1, "expected one native drag session: {observations:?}");
        let observed = observations[0].as_ref().expect("the isolated receiver could not read the native drag");
        assert!(observed.retained_mouse_down, "the native drag did not retain the originating mouse-down");
        assert!(observed.copy_only, "the native drag exposed a non-copy operation");
        assert_eq!(observed.files, *submissions[0], "the native receiver saw different file URLs");
        assert_eq!(observed.files.len(), 2);
        for (file, size) in observed.files.iter().zip(&observed.image_sizes) {
            let name = file.file_name().unwrap().to_string_lossy();
            let expected = if name == FIRST { (9, 8) } else if name == SECOND { (5, 4) } else { panic!("unexpected drag file: {file:?}") };
            assert_eq!(*size, Some(expected), "the drag did not include the expected snapshot");
            assert!(!file.exists(), "a cancelled drag retained an export: {file:?}");
        }
        assert_eq!(image::image_dimensions(library_fixture_path(FIRST)).unwrap(), (3, 2));
        assert_eq!(crate::canvas::with_editor(|editor| editor.is_modified()), Some(true));
        assert!(!crate::file_drag_macos::has_pending().expect("inspect finished drags"));
        if env("JIFFSHOT_UITEST_DRAG_DROP").is_some() {
            platform::GlobalDrag::raise_global_drag_window(state.borrow().number).expect("raise the global drag fixture");
        } else {
            state.borrow_mut().start_pending(&library_fixture_path(SECOND));
        }
        test.line("AppKit received two Copy-only URLs, including the current unsaved edits; Escape cancelled once");
        1_000
    });
    if env("JIFFSHOT_UITEST_DRAG_DROP").is_some() {
        let state = fixture.clone();
        script.step(move |_| {
            state.borrow_mut().start_global_pending();
            1_000
        });
        let state = fixture.clone();
        script.step(move |_| {
            state.borrow_mut().global_drag.as_mut().expect("the global drag fixture is unavailable")
                .move_to_threshold().expect("move the controlled drag past its threshold");
            1_000
        });
        let state = fixture.clone();
        script.step(move |_| {
            state.borrow_mut().check_global_pending(&library_fixture_path(SECOND));
            500
        });
    }
    let state = fixture.clone();
    script.step(move |_| {
        assert!(crate::file_drag_macos::preparation_waiting_for_selftest(), "the active-drag worker did not reach the gate");
        state.borrow_mut().release_preparation();
        1_500
    });
    let state = fixture.clone();
    script.step(move |test| {
        let mut state = state.borrow_mut();
        let observations = platform::native_file_drags();
        assert_eq!(observations.len(), 2, "the second native drag did not start: {observations:?}");
        let observed = observations[1].as_ref().expect("the active drag receiver could not read its file URLs");
        assert_eq!(observed.files.len(), 2, "the active drag did not contain both images");
        for (file, size) in observed.files.iter().zip(&observed.image_sizes) {
            let name = file.file_name().unwrap().to_string_lossy();
            let expected = if name == FIRST { (9, 8) } else if name == SECOND { (5, 4) } else { panic!("unexpected drag file: {file:?}") };
            assert_eq!(*size, Some(expected), "the active drag did not contain the expected snapshot");
        }
        assert!(observed.files.iter().all(|path| path.exists()), "active drag exports were removed prematurely");
        assert!(crate::file_drag_macos::has_pending().expect("inspect active drag"));
        assert!(!crate::file_drag_macos::events_for_selftest().iter().any(|event| matches!(event,
            TestEvent::Completed { token, .. } if *token == state.token)), "the active drag completed prematurely");
        crate::app::act(|app| app.exit()).expect("request quit during the native drag");
        let toast = sink().toast.get_untracked();
        assert!(toast.contains("Could not quit Jiffshot") && toast.contains("library drag"), "{toast}");
        crate::app::act(|app| app.refresh_library()).expect("rebuild the library during a native drag");
        assert!(crate::file_drag_macos::has_pending().expect("inspect the retained native drag"));
        if let Some(receiver) = std::env::var_os("JIFFSHOT_UITEST_DRAG_RECEIVER") {
            let mut command = std::process::Command::new(receiver);
            for (file, size) in observed.files.iter().zip(&observed.image_sizes) {
                let (width, height) = size.expect("the active drag image has no size");
                command.arg(file).arg(width.to_string()).arg(height.to_string());
            }
            if env("JIFFSHOT_UITEST_DRAG_DROP").is_some() {
                let path = std::env::var_os("JIFFSHOT_UITEST_DRAG_RECEIVER_STARTED_FILE")
                    .expect("the receiver-started marker is unavailable");
                std::fs::write(path, b"started").expect("record the fixture receiver start");
                command.env("JIFFSHOT_UITEST_DRAG_SOURCE_PID", std::process::id().to_string());
            }
            state.receiver = Some(command.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
                .spawn().expect("start the external drag pasteboard receiver"));
        }
        test.line("active drag retained its exports while quit and library rebuild were requested");
        if env("JIFFSHOT_UITEST_DRAG_DROP").is_some() { 8_000 } else { 5_000 }
    });
    let state = fixture.clone();
    script.step(move |test| {
        let mut state = state.borrow_mut();
        if let Some(mut receiver) = state.receiver.take() {
            if receiver.try_wait().expect("inspect the drag receiver").is_none() {
                receiver.kill().expect("stop the stalled drag receiver");
                receiver.wait().expect("reap the stalled drag receiver");
                state.receiver_error = Some("the external drag receiver did not finish".into());
            }
            let result = receiver.wait_with_output().expect("collect the drag receiver output");
            if env("JIFFSHOT_UITEST_DRAG_DROP").is_some() {
                let path = std::env::var_os("JIFFSHOT_UITEST_DRAG_RECEIVER_FINISHED_FILE")
                    .expect("the receiver-finished marker is unavailable");
                std::fs::write(path, b"finished").expect("record the fixture receiver completion");
            }
            if let Some(mut drag) = state.global_drag.take() {
                if let Err(error) = drag.finish() {
                    platform::post_drag_escape(state.number).expect("cancel the interrupted external drag");
                    state.receiver_error = Some(error);
                }
            }
            if !result.status.success() {
                platform::post_drag_escape(state.number).expect("cancel the failed external drag");
                if state.receiver_error.is_none() {
                    state.receiver_error = Some(format!("external drag receiver failed: {}", String::from_utf8_lossy(&result.stderr)));
                }
            } else if state.receiver_error.is_none() {
                if env("JIFFSHOT_UITEST_DRAG_DROP").is_some() {
                    assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "external AppKit receiver accepted both dragged images");
                    test.line("an external AppKit application accepted both dragged snapshots");
                } else {
                    assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "external process read both active drag URLs and their images");
                    test.line("an external process read the active AppKit drag pasteboard");
                }
            }
        }
        if state.receiver_error.is_none() && env("JIFFSHOT_UITEST_DRAG_DROP").is_none() {
            assert!(platform::close_owned_window(state.number), "could not close the active drag owner");
            platform::post_drag_escape(state.number).expect("cancel the active drag");
            state.window.take();
        }
        2_500
    });
    let state = fixture.clone();
    script.step(move |test| {
        let mut state = state.borrow_mut();
        assert!(!crate::file_drag_macos::has_pending().expect("inspect the finished native drag"));
        if let Some(error) = state.receiver_error.take() {
            let completed = crate::file_drag_macos::events_for_selftest().into_iter()
                .filter(|event| matches!(event, TestEvent::Completed { token, .. } if *token == state.token))
                .count();
            assert_eq!(completed, 1, "the failed fixture drag did not complete exactly once");
            if env("JIFFSHOT_UITEST_DRAG_RECEIVER_EXIT_AFTER_WARP").is_some() {
                assert_drag_completed(state.token);
                let observations = platform::native_file_drags();
                let observed = observations[1].as_ref().expect("the failed drag's native session");
                assert!(observed.files.iter().all(|path| !path.exists()), "failed-drag exports were retained");
            }
            assert!(platform::close_owned_window(state.number), "could not close the failed drag owner");
            state.window.take();
            test.line(format!("failed external drag cleaned up: {error}"));
            panic!("{error}");
        }
        let observations = platform::native_file_drags();
        let observed = observations[1].as_ref().expect("the active drag receiver");
        if env("JIFFSHOT_UITEST_DRAG_DROP").is_some() {
            let completions = crate::file_drag_macos::events_for_selftest().into_iter().filter_map(|event| match event {
                TestEvent::Completed { token, effect, status } if token == state.token => Some((effect, status)),
                _ => None,
            }).collect::<Vec<_>>();
            assert_eq!(completions, [(1, 0)], "expected one successful copied-drag completion");
            assert!(observed.files.iter().all(|path| path.exists()), "copied drag exports were removed prematurely");
            assert!(platform::close_owned_window(state.number), "could not close the copied drag owner");
            state.window.take();
            test.line("a copied drag retained its snapshots after native completion");
        } else {
            assert_drag_completed(state.token);
            assert!(observed.files.iter().all(|path| !path.exists()), "finished no-drop exports were not removed");
            test.line("active owner closure waited for one native completion before removing exports");
        }
        state.mount(&library_fixture_path(SECOND));
        1_000
    });
    let state = fixture.clone();
    script.step(move |_| {
        state.borrow_mut().start_pending(&library_fixture_path(SECOND));
        1_000
    });
    let state = fixture.clone();
    script.step(move |test| {
        assert!(crate::file_drag_macos::preparation_waiting_for_selftest(), "the close-race worker did not reach the gate");
        let mut state = state.borrow_mut();
        assert!(platform::close_owned_window(state.number), "could not close the drag fixture window");
        assert_drag_completed(state.token);
        state.window.take();
        state.release_preparation();
        test.line("closing the owner cancelled the pending gesture");
        1_500
    });
    let state = fixture.clone();
    script.step(move |test| {
        let mut state = state.borrow_mut();
        assert_stale_drag_discarded(state.token);
        assert!(!crate::file_drag_macos::has_pending().expect("inspect closed-owner drags"));
        state.mount(&library_fixture_path(SECOND));
        test.line("closing the owner discarded the late snapshots");
        1_000
    });
    let state = fixture.clone();
    script.step(move |_| {
        state.borrow_mut().start_pending(&library_fixture_path(SECOND));
        1_000
    });
    let state = fixture.clone();
    script.step(move |test| {
        assert!(crate::file_drag_macos::preparation_waiting_for_selftest(), "the rebuild-race worker did not reach the gate");
        crate::app::act(|app| app.refresh_library()).expect("rebuild the library");
        let mut state = state.borrow_mut();
        assert_drag_completed(state.token);
        state.release_preparation();
        test.line("rebuilding the library cancelled the pending gesture");
        1_500
    });
    let state = fixture.clone();
    script.step(move |test| {
        let mut state = state.borrow_mut();
        assert_stale_drag_discarded(state.token);
        assert!(!crate::file_drag_macos::has_pending().expect("inspect rebuilt drags"));
        assert!(crate::file_drag_macos::registered_for_selftest(&library_fixture_path(SECOND)) > 0, "rebuilt library tiles did not re-register");
        assert!(platform::close_owned_window(state.number), "could not close the rebuilt fixture window");
        state.window.take();
        platform::stop_observing_native_file_drag().expect("restore the native drag method");
        test.line("rebuilt tiles re-registered and stale snapshots were removed");
        0
    });
    script
}

#[cfg(target_os = "macos")]
fn fonts_script() -> Script {
    use jiffshot_doc::object::{Kind, Style};
    use jiffshot_doc::{Box2, Document, Rgba};

    const FAMILY: &str = "Jiffshot missing\u{1f}family 621be9";
    const SHOWN_FAMILY: &str = "Jiffshot missing family 621be9";
    let mut script = Script(VecDeque::new());
    script.wait(800).step(|test| {
        empty_scratch_library(test);
        let mut document = Document::blank(180, 90, Rgba::WHITE);
        document.add(
            Kind::Text { rect: Box2::new(4.0, 4.0, 170.0, 70.0), text: "Visible text".into() },
            Style { font: FAMILY.into(), italic: true, ..Style::default() },
        );
        for name in ["font-fixture.jsd", "font-fixture-copy.jsd"] {
            jiffshot_doc::io::save_document(&document, &test.path(name)).expect("save the synthetic document");
        }
        crate::app::act(|app| app.open_path(&test.path("font-fixture.jsd"))).unwrap();
        150
    });
    script.step(|test| {
        let status = sink().status.get_untracked();
        assert!(status.contains(SHOWN_FAMILY) && status.contains("Opened"), "{status}");
        let toast = sink().toast.get_untracked();
        let fields: Vec<_> = toast.split('\u{1f}').collect();
        assert_eq!(fields.len(), 4, "{toast:?}");
        assert_eq!(fields[1], "Font substitution");
        assert!(fields[2].contains(SHOWN_FAMILY));
        crate::app::act(|app| app.copy_image_with(|frame, _| {
            assert!(frame.pixels.chunks_exact(4).any(|pixel| pixel[..3] != [255, 255, 255]), "the substitute text was not drawn");
            Ok(())
        })).unwrap();
        test.line("opening the document reported a sanitized substitution");
        150
    });
    script.step(|test| {
        let status = sink().status.get_untracked();
        assert!(status.contains(SHOWN_FAMILY) && status.contains("Copied the image"), "{status}");
        crate::app::act(|app| app.copy_image_with(|_, _| Err(std::io::Error::other("fixture copy failure")))).unwrap();
        test.line("copying the flattened image reported the substitution");
        150
    });
    script.step(|test| {
        let status = sink().status.get_untracked();
        assert!(status.contains("Copy failed: fixture copy failure"), "{status}");
        let path = test.path("font-export.png");
        crate::app::act(|app| app.run(&format!("export-to:{}", path.display()))).unwrap();
        test.line("a copy failure remained visible");
        150
    });
    script.step(|test| {
        let status = sink().status.get_untracked();
        assert!(status.contains(SHOWN_FAMILY) && status.contains("Exported"), "{status}");
        assert!(test.path("font-export.png").is_file());
        crate::canvas::with_editor(|editor| editor.select(Some(editor.document().objects[0].id))).unwrap();
        crate::app::act(|app| app.flatten_selected()).unwrap();
        test.line("export reported the substitution");
        150
    });
    script.step(|test| {
        let status = sink().status.get_untracked();
        assert!(status.contains(SHOWN_FAMILY) && status.contains("Flattened the selected objects"), "{status}");
        assert!(crate::canvas::with_editor(|editor| editor.document().objects.is_empty()).unwrap());
        crate::app::act(|app| app.open_path(&test.path("font-fixture-copy.jsd"))).unwrap();
        test.line("flattening selected text reported the face before rasterization");
        150
    });
    script.step(|_| {
        crate::app::act(|app| app.flatten_document()).unwrap();
        150
    });
    script.step(|test| {
        let status = sink().status.get_untracked();
        assert!(status.contains(SHOWN_FAMILY) && status.contains("Flattened the document"), "{status}");
        assert!(crate::canvas::with_editor(|editor| editor.document().objects.is_empty()).unwrap());
        crate::app::act(|app| {
            app.set_tool(jiffshot_doc::editor::Tool::Text);
            app.style_edit("font", |style| style.font = FAMILY.into());
        }).unwrap();
        test.line("flattening the document reported the face before rasterization");
        150
    });
    script.step(|test| {
        let status = sink().status.get_untracked();
        assert!(status.contains(SHOWN_FAMILY), "{status}");
        crate::app::act(|app| app.style_edit("font", |style| {
            style.font = "Helvetica Neue".into();
            style.bold = false;
            style.italic = false;
        })).unwrap();
        test.line("changing the text font reported the substitution");
        150
    });
    script.step(|test| {
        assert_eq!(sink().status.get_untracked(), "Font available");
        test.line("choosing an available face cleared the font warning");
        0
    });
    script
}

#[cfg(target_os = "macos")]
fn clipboard_script() -> Script {
    use jiffshot_capture::clipboard::Pasted;

    let mut script = Script(VecDeque::new());
    script.wait(800).step(|test| {
        empty_scratch_library(test);
        let rgba = image::RgbaImage::from_raw(2, 2, vec![
            255, 0, 0, 128, 0, 0, 255, 255,
            0, 0, 0, 0, 0, 255, 0, 64,
        ]).unwrap();
        let pixmap = jiffshot_doc::io::from_rgba_image(&rgba).unwrap();
        crate::app::act(|app| {
            app.load(jiffshot_doc::Document::new(pixmap), None, None);
            app.copy_image_with(|frame, _| {
                assert_eq!((frame.width, frame.height, frame.alpha), (2, 2, true));
                assert_eq!(frame.pixels, [
                    0, 0, 255, 128, 255, 0, 0, 255,
                    0, 0, 0, 0, 0, 255, 0, 64,
                ]);
                Ok(())
            });
        }).unwrap();
        150
    });
    script.step(|test| {
        assert!(sink().status.get_untracked().starts_with("Copied the image"));
        crate::app::act(|app| {
            app.copy_image_with(|_, _| Err(std::io::Error::other("fixture TIFF refusal")));
        }).unwrap();
        test.line("editor Copy Image delivered the transparent flattened document");
        150
    });
    script.step(|test| {
        assert!(sink().status.get_untracked().contains("Copy failed: fixture TIFF refusal"));
        crate::app::act(|app| {
            app.paste_from_clipboard(Ok(Some(Pasted::Image {
                width: 1, height: 1, rgba: vec![0, 255, 0, 128],
            })));
        }).unwrap();
        test.line("editor reported a clipboard write failure");
        150
    });
    script.step(|test| {
        assert!(sink().status.get_untracked().starts_with("Pasted a 1 × 1 image"));
        crate::canvas::with_editor(|editor| {
            assert_eq!(editor.document().objects.len(), 1);
            let jiffshot_doc::object::Kind::Image { image, .. } = &editor.document().objects[0].kind else {
                panic!("clipboard image was not pasted as an image object");
            };
            assert_eq!(jiffshot_doc::io::to_rgba_image(image.pixmap.as_ref().unwrap()).as_raw(), &[0, 255, 0, 128]);
        }).expect("editor");
        crate::app::act(|app| {
            app.copy();
            app.paste_from_clipboard(Err(std::io::Error::other("fixture read failure")));
        }).unwrap();
        test.line("editor pasted a transparent clipboard image as an editable object");
        150
    });
    script.step(|test| {
        assert!(sink().status.get_untracked().contains("Paste failed: fixture read failure"));
        assert_eq!(crate::canvas::with_editor(|e| e.document().objects.len()), Some(1));
        crate::app::act(|app| app.paste_from_clipboard(Ok(None))).unwrap();
        test.line("clipboard read failure did not paste a stale internal object");
        150
    });
    script.step(|test| {
        assert_eq!(crate::canvas::with_editor(|e| e.document().objects.len()), Some(2));
        crate::app::act(|app| {
            app.paste_from_clipboard(Ok(Some(Pasted::Image { width: 2, height: 1, rgba: vec![0; 4] })));
        }).unwrap();
        test.line("internal object paste remained available after the failed read");
        150
    });
    script.step(|test| {
        assert!(sink().status.get_untracked().contains("Paste failed: the clipboard image has invalid"));
        assert_eq!(crate::canvas::with_editor(|e| e.document().objects.len()), Some(2));
        let path = test.path("clipboard-fixture.png");
        let mut frame = jiffshot_capture::Frame::new(1, 1);
        frame.pixels.copy_from_slice(&[0, 0, 255, 255]);
        frame.save_png(&path).unwrap();
        let missing = test.path("missing-clipboard-fixture.png");
        crate::app::act(|app| {
            app.paste_from_clipboard(Ok(Some(Pasted::Files(vec![path, missing]))));
        }).unwrap();
        test.line("invalid clipboard pixels were reported without modifying the document");
        150
    });
    script.step(|test| {
        let status = sink().status.get_untracked();
        assert!(status.contains("Pasted 1 image file(s); could not open") && status.contains("missing-clipboard-fixture.png"), "{status}");
        assert_eq!(crate::canvas::with_editor(|e| e.document().objects.len()), Some(3));
        std::fs::remove_file(test.path("clipboard-fixture.png")).unwrap();
        crate::app::act(|app| {
            app.new_from_clipboard_data(Err(std::io::Error::other("fixture read failure")));
        }).unwrap();
        test.line("partial clipboard file paste reported the missing file");
        150
    });
    script.step(|test| {
        assert!(sink().status.get_untracked().contains("New from clipboard failed: fixture read failure"));
        assert_eq!(crate::canvas::with_editor(|e| e.document().objects.len()), Some(3));
        crate::app::act(|app| {
            app.new_from_clipboard_data(Ok(Some(Pasted::Image { width: 2, height: 1, rgba: vec![0; 4] })));
        }).unwrap();
        test.line("new from clipboard reported read failure without replacing the document");
        150
    });
    script.step(|test| {
        assert!(sink().status.get_untracked().contains("New from clipboard failed: the image has invalid"));
        assert_eq!(crate::canvas::with_editor(|e| e.document().objects.len()), Some(3));
        crate::app::act(|app| {
            app.new_from_clipboard_data(Ok(Some(Pasted::Image {
                width: 2, height: 1, rgba: vec![255, 0, 0, 128, 0, 0, 255, 255],
            })));
        }).unwrap();
        test.line("new from clipboard rejected invalid pixels without replacing the document");
        150
    });
    script.step(|test| {
        assert!(sink().status.get_untracked().starts_with("New image from the clipboard (2 × 1)"));
        crate::canvas::with_editor(|editor| {
            let frame = crate::winutil::frame_from_pixmap(&editor.document().flatten());
            assert_eq!((frame.width, frame.height, frame.alpha), (2, 1, true));
            assert_eq!(frame.pixels, [0, 0, 255, 128, 255, 0, 0, 255]);
        }).expect("editor");
        test.line("new from clipboard preserved transparent pixels");
        0
    });
    script
}

#[cfg(target_os = "macos")]
fn recent_files_script() -> Script {
    const NAME: &str = "report 雪 \\ # 100%?.png";
    let path = Rc::new(RefCell::new(None::<PathBuf>));
    let uri = Rc::new(RefCell::new(None::<String>));
    let tray = Rc::new(RefCell::new(None::<crate::vm::TrayBinding>));
    let mut script = Script(VecDeque::new());
    let created = path.clone();
    script.wait(800).step(move |test| {
        let library = empty_scratch_library(test);
        let folder = library.join("2026-01");
        std::fs::create_dir(&folder).expect("create the scratch capture folder");
        let image = folder.join(NAME);
        let mut picture = jiffshot_doc::Pixmap::new(54, 34).expect("create a synthetic image");
        picture.fill(tiny_skia::Color::from_rgba8(30, 90, 160, 255));
        picture.save_png(&image).expect("save the synthetic image");
        crate::app::act(|app| {
            app.refresh_library();
            app.open_path(&image);
        }).expect("open the synthetic image");
        *created.borrow_mut() = Some(image);
        500
    });
    let opened = path.clone();
    let remembered = uri.clone();
    let attached = tray.clone();
    script.step(move |test| {
        let image = opened.borrow().clone().expect("synthetic image");
        assert_eq!(super::canvas().expect("editor canvas").document_size(), (54, 34));
        let files = sink().recent_files.get_untracked();
        let recent = files.first().expect("editor recent file").clone();
        assert!(recent.starts_with("file:///") && recent.contains("%5C") && recent.contains("%25"), "{recent}");
        assert_eq!(super::recent_file_name(&recent), NAME);
        assert_eq!(crate::settings::Settings::load().recent.first(), Some(&image), "the recent path was not persisted");
        let binding = crate::vm::TrayBinding::new(crate::tray::TrayModel).expect("attach the tray model");
        assert_eq!(binding.sink.recent_captures.get_untracked().first(), Some(&recent));
        *remembered.borrow_mut() = Some(recent);
        *attached.borrow_mut() = Some(binding);
        call(|model| model.new_image());
        test.line("recent file stored and shown in editor and tray");
        400
    });
    let editor_uri = uri.clone();
    script.step(move |_| {
        assert_eq!(super::canvas().expect("editor canvas").document_size(), (1280, 720));
        let recent = editor_uri.borrow().clone().expect("editor recent URI");
        call(move |model| model.open_recent_file(recent));
        500
    });
    script.step(|test| {
        assert_eq!(super::canvas().expect("editor canvas").document_size(), (54, 34), "editor recent command did not reopen the image");
        assert!(sink().status.get_untracked().contains(NAME), "editor recent command did not report the opened path");
        call(|model| model.new_image());
        test.line("editor recent command reopened the synthetic image");
        400
    });
    let menu = tray;
    script.step(move |_| {
        assert_eq!(super::canvas().expect("editor canvas").document_size(), (1280, 720));
        let tray = menu.borrow();
        super::tray::select_recent_for_selftest(tray.as_ref().expect("tray model"), 0);
        500
    });
    let unchanged = uri.clone();
    script.step(move |test| {
        assert_eq!(super::canvas().expect("editor canvas").document_size(), (54, 34), "tray recent command did not reopen the image");
        assert!(sink().status.get_untracked().contains(NAME), "tray recent command did not report the opened path");
        test.line("tray recent command reopened the synthetic image");
        call(|model| model.open_recent_file("file:///tmp/%ZZ".into()));
        300
    });
    script.step(move |test| {
        assert!(sink().status.get_untracked().contains("invalid percent escape"), "the invalid URI was not reported");
        assert_eq!(sink().recent_files.get_untracked().first(), unchanged.borrow().as_ref(), "the invalid URI changed recent files");
        test.line("invalid recent URI reported without changing recent files");
        0
    });
    script
}

#[cfg(target_os = "macos")]
fn library_fixture_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os("JIFFSHOT_LIBRARY").expect("scratch JIFFSHOT_LIBRARY")).join("2026-01").join(name)
}

#[cfg(target_os = "macos")]
fn library_fixture_item(path: &Path) -> Rc<crate::vm::LibraryItemItem> {
    let path = path.to_str().expect("fixture path is UTF-8");
    sink().library.get_untracked().into_iter()
        .find(|item| item.fields.get_untracked().path == path)
        .expect("fixture capture is not visible in the library")
}

#[cfg(target_os = "macos")]
fn library_script() -> Script {
    const FIRST: &str = "notes-for-review 雪雪.png";
    const SECOND: &str = "slide \\ # 100%?.png";
    const RENAMED: &str = "Renamed 雪 100%.png";
    const RENAMED_CLOSED: &str = "Renamed again 雪.png";
    const DIRECT_RENAMED: &str = "Document source 雪.png";
    let mut script = Script(VecDeque::new());
    script.wait(800).step(|test| {
        let library = empty_scratch_library(test);
        let folder = library.join("2026-01");
        std::fs::create_dir(&folder).expect("create the scratch capture folder");
        for (name, width, height) in [(FIRST, 54, 34), (SECOND, 63, 39)] {
            let mut image = jiffshot_doc::Pixmap::new(width, height).expect("create a synthetic image");
            image.fill(tiny_skia::Color::from_rgba8(30, 90, 160, 255));
            image.save_png(&folder.join(name)).expect("save the synthetic image");
        }
        let second = folder.join(SECOND);
        let edits = jiffshot_doc::Document::blank(57, 31, jiffshot_doc::Rgba::WHITE);
        jiffshot_doc::io::save_document(&edits, &second.with_extension("jsd")).expect("save synthetic edits");
        std::fs::write(second.with_extension("cursor.png"), b"synthetic pointer").expect("save synthetic cursor image");
        std::fs::write(second.with_extension("cursor.json"), b"synthetic pointer metadata").expect("save synthetic cursor metadata");
        crate::app::act(|app| {
            assert_eq!(app.library.root, library, "the app is not using the scratch library");
            app.refresh_library();
        }).expect("refresh the scratch library");
        500
    });
    script.step(|test| {
        assert_eq!(sink().library.get_untracked().len(), 2, "the cursor companion must not appear as a capture");
        let first = library_fixture_item(&library_fixture_path(FIRST));
        assert_eq!(first.fields.get_untracked().label, "notes-for-review 雪雪");
        first.model.run(|model| model.set_tags("docs, DOCS; ui test".into()));
        test.line("synthetic captures and Unicode labels are visible");
        400
    });
    script.step(|_| {
        let first = library_fixture_path(FIRST);
        let meta = jiffshot_capture::library::read_meta(&first).expect("new metadata was not saved");
        assert_eq!(meta.tags, ["docs", "ui test"]);
        assert_eq!((meta.width, meta.height), (54, 34));
        assert!(sink().library_filters.get_untracked().contains(&"Tag: docs".to_owned()));
        library_fixture_item(&library_fixture_path(SECOND)).model.run(|model| model.toggle_pin());
        400
    });
    script.step(|test| {
        let second = library_fixture_path(SECOND);
        let meta = jiffshot_capture::library::read_meta(&second).expect("pin metadata was not saved");
        assert!(meta.pinned);
        let items = sink().library.get_untracked();
        assert_eq!(items.first().expect("pinned item").fields.get_untracked().path, second.to_str().unwrap());
        assert!(items.first().unwrap().fields.get_untracked().is_pinned);
        set(sink().library_filter, "Tag: docs".into());
        test.line("tags and pins persisted; pinned item sorted first");
        400
    });
    script.step(|_| {
        let items = sink().library.get_untracked();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].fields.get_untracked().path, library_fixture_path(FIRST).to_str().unwrap());
        assert_eq!(crate::settings::Settings::load().library_view.filter, "Tag: docs");
        set(sink().library_filter, "All".into());
        set(sink().library_search, "100%".into());
        400
    });
    script.step(|test| {
        let items = sink().library.get_untracked();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].fields.get_untracked().path, library_fixture_path(SECOND).to_str().unwrap());
        set(sink().library_search, String::new());
        set(sink().library_sort, "Name".into());
        test.line("tag filtering, persisted filter and filename search worked");
        400
    });
    script.step(|_| {
        for name in [FIRST, SECOND] {
            library_fixture_item(&library_fixture_path(name)).model.run(|model| model.toggle_select());
        }
        400
    });
    script.step(|_| {
        assert_eq!(sink().library_selected.get_untracked(), 2);
        for name in [FIRST, SECOND] {
            assert!(library_fixture_item(&library_fixture_path(name)).fields.get_untracked().is_selected);
        }
        library_fixture_item(&library_fixture_path(FIRST)).model.run(|model| model.set_tags("Shared, shared; macOS".into()));
        400
    });
    script.step(|test| {
        for name in [FIRST, SECOND] {
            let meta = jiffshot_capture::library::read_meta(&library_fixture_path(name)).expect("bulk tag metadata");
            assert_eq!(meta.tags, ["Shared", "macOS"]);
        }
        assert_eq!(sink().library_selected.get_untracked(), 2);
        call(|model| model.combine_selected());
        test.line("multi-selection and bulk tagging persisted");
        500
    });
    script.step(|test| {
        assert_eq!(super::canvas().expect("editor canvas").document_size(), (189, 87));
        assert_eq!(crate::canvas::with_editor(|editor| editor.document().objects.len()), Some(2));
        library_fixture_item(&library_fixture_path(SECOND)).model.run(|model| model.open());
        test.line("selected captures combined into two editable objects");
        500
    });
    script.step(|_| {
        assert_eq!(super::canvas().expect("editor canvas").document_size(), (57, 31), "saved edits did not open");
        library_fixture_item(&library_fixture_path(SECOND)).model.run(|model| model.rename("Renamed 雪 100%".into()));
        500
    });
    script.step(|test| {
        let old = library_fixture_path(SECOND);
        let renamed = library_fixture_path(RENAMED);
        for extension in ["png", "json", "jsd", "cursor.png", "cursor.json"] {
            assert!(!old.with_extension(extension).exists(), "old {extension} remained");
            assert!(renamed.with_extension(extension).exists(), "renamed {extension} is missing");
        }
        assert_eq!(std::fs::read(renamed.with_extension("cursor.png")).unwrap(), b"synthetic pointer");
        assert_eq!(std::fs::read(renamed.with_extension("cursor.json")).unwrap(), b"synthetic pointer metadata");
        assert_eq!(jiffshot_doc::io::load_document(&renamed.with_extension("jsd")).unwrap().size(), (57, 31));
        let meta = jiffshot_capture::library::read_meta(&renamed).expect("renamed metadata");
        assert!(meta.pinned);
        assert_eq!(meta.tags, ["Shared", "macOS"]);
        let item = library_fixture_item(&renamed);
        assert!(item.fields.get_untracked().is_current, "renamed capture is not current");
        assert!(item.fields.get_untracked().is_selected, "renamed capture lost its selection");
        assert_eq!(sink().library_selected.get_untracked(), 2);
        assert_eq!(crate::settings::Settings::load().recent.first(), Some(&renamed), "renamed recent path was not persisted");
        assert_eq!(super::recent_file_name(sink().recent_files.get_untracked().first().unwrap()), RENAMED);
        crate::app::act(|app| app.resize_to(58, 32)).expect("resize the open synthetic document");
        call(|model| model.save());
        call(|model| model.clear_library_selection());
        test.line("rename preserved edits, metadata, pointer files, selection and recent path");
        400
    });
    script.step(|_| {
        let renamed = library_fixture_path(RENAMED);
        assert_eq!(jiffshot_doc::io::load_document(&renamed.with_extension("jsd")).unwrap().size(), (58, 32));
        assert!(!library_fixture_path(SECOND).with_extension("jsd").exists(), "saving recreated the old edits file");
        assert_eq!(sink().library_selected.get_untracked(), 0);
        library_fixture_item(&renamed).model.run(|model| model.close());
        400
    });
    script.step(|_| {
        let renamed = library_fixture_path(RENAMED);
        assert_eq!(sink().library.get_untracked().len(), 1);
        assert!(crate::settings::Settings::load().library_view.closed.contains(&renamed));
        set(sink().library_expanded, true);
        400
    });
    script.step(|test| {
        assert_eq!(sink().library.get_untracked().len(), 2, "closed capture is missing from the expanded library");
        assert!(library_fixture_item(&library_fixture_path(RENAMED)).fields.get_untracked().is_pinned);
        library_fixture_item(&library_fixture_path(RENAMED)).model.run(|model| model.rename("Renamed again 雪".into()));
        test.line("saving after rename used the renamed edits file");
        400
    });
    script.step(|_| {
        let old = library_fixture_path(RENAMED);
        let renamed = library_fixture_path(RENAMED_CLOSED);
        let settings = crate::settings::Settings::load();
        assert!(!old.exists() && renamed.exists());
        assert!(!settings.library_view.closed.contains(&old) && settings.library_view.closed.contains(&renamed));
        assert_eq!(settings.recent.first(), Some(&renamed));
        set(sink().library_expanded, false);
        400
    });
    script.step(|_| {
        assert_eq!(sink().library.get_untracked().len(), 1, "the renamed closed capture returned to the tray");
        call(|model| model.show_closed());
        400
    });
    script.step(|test| {
        assert_eq!(sink().library.get_untracked().len(), 2);
        assert!(crate::settings::Settings::load().library_view.closed.is_empty());
        let edits = library_fixture_path(RENAMED_CLOSED).with_extension("jsd");
        crate::app::act(|app| app.open_path(&edits)).expect("open edits directly");
        test.line("renaming a closed capture preserved its tray state and recent path");
        400
    });
    script.step(|_| {
        assert_eq!(super::canvas().expect("editor canvas").document_size(), (58, 32));
        assert_eq!(crate::settings::Settings::load().recent.first(), Some(&library_fixture_path(RENAMED_CLOSED).with_extension("jsd")));
        library_fixture_item(&library_fixture_path(RENAMED_CLOSED)).model.run(|model| model.rename("Document source 雪".into()));
        400
    });
    script.step(|_| {
        let old_edits = library_fixture_path(RENAMED_CLOSED).with_extension("jsd");
        let new_edits = library_fixture_path(DIRECT_RENAMED).with_extension("jsd");
        let recent = crate::settings::Settings::load().recent;
        assert_eq!(recent.first(), Some(&new_edits));
        assert!(recent.contains(&library_fixture_path(DIRECT_RENAMED)));
        assert!(!recent.contains(&old_edits));
        assert!(!old_edits.exists() && new_edits.exists());
        crate::app::act(|app| app.resize_to(59, 33)).expect("resize the directly opened edits");
        call(|model| model.save());
        400
    });
    script.step(|test| {
        let new_edits = library_fixture_path(DIRECT_RENAMED).with_extension("jsd");
        assert_eq!(jiffshot_doc::io::load_document(&new_edits).unwrap().size(), (59, 33));
        assert!(!library_fixture_path(RENAMED_CLOSED).with_extension("jsd").exists());
        std::fs::write(library_fixture_path("Conflicting.json"), b"unrelated metadata").expect("create a companion conflict");
        library_fixture_item(&library_fixture_path(DIRECT_RENAMED)).model.run(|model| model.rename("Conflicting".into()));
        test.line("directly opened edits followed the renamed capture");
        400
    });
    script.step(|test| {
        let conflict = library_fixture_path("Conflicting.json");
        let current = library_fixture_path(DIRECT_RENAMED);
        assert!(sink().status.get_untracked().starts_with("Could not rename:"));
        assert_eq!(std::fs::read(&conflict).unwrap(), b"unrelated metadata");
        assert!(current.exists() && current.with_extension("jsd").exists());
        assert_eq!(crate::settings::Settings::load().recent.first(), Some(&current.with_extension("jsd")));
        std::fs::remove_file(conflict).expect("remove the synthetic conflict");
        for name in [FIRST, DIRECT_RENAMED] {
            library_fixture_item(&library_fixture_path(name)).model.run(|model| model.toggle_select());
        }
        test.line("rename conflicts preserved unrelated files and existing capture state");
        400
    });
    script.step(|_| {
        assert_eq!(sink().library_selected.get_untracked(), 2);
        let first = library_fixture_path(FIRST);
        std::fs::rename(first.with_extension("json"), first.with_extension("json.test-backup")).expect("save fixture metadata");
        std::fs::create_dir(first.with_extension("json")).expect("block metadata write");
        library_fixture_item(&first).model.run(|model| model.set_tags("partly saved".into()));
        400
    });
    script.step(|test| {
        let first = library_fixture_path(FIRST);
        let status = sink().status.get_untracked();
        std::fs::remove_dir(first.with_extension("json")).expect("unblock metadata write");
        std::fs::rename(first.with_extension("json.test-backup"), first.with_extension("json")).expect("restore fixture metadata");
        assert!(status.starts_with("Could not save the capture's details:"), "metadata failure was not reported: {status}");
        assert!(status.contains("1 of 2 updated"), "partial metadata success was not reported: {status}");
        assert_eq!(jiffshot_capture::library::read_meta(&first).unwrap().tags, ["Shared", "macOS"]);
        assert_eq!(jiffshot_capture::library::read_meta(&library_fixture_path(DIRECT_RENAMED)).unwrap().tags, ["partly saved"]);
        test.line("partial metadata write failures are reported");
        std::fs::rename(first.with_extension("json"), first.with_extension("json.test-backup")).expect("save fixture metadata");
        std::fs::write(first.with_extension("json"), b"{invalid").expect("create invalid metadata");
        library_fixture_item(&first).model.run(|model| model.toggle_pin());
        400
    });
    script.step(|test| {
        let first = library_fixture_path(FIRST);
        let status = sink().status.get_untracked();
        let invalid = std::fs::read(first.with_extension("json")).expect("read invalid fixture metadata");
        std::fs::remove_file(first.with_extension("json")).expect("remove invalid fixture metadata");
        std::fs::rename(first.with_extension("json.test-backup"), first.with_extension("json")).expect("restore fixture metadata");
        assert!(status.starts_with("Could not read the capture's details"), "invalid metadata was not reported: {status}");
        assert!(status.contains("invalid capture metadata"), "the metadata error was not described: {status}");
        assert_eq!(invalid, b"{invalid", "invalid metadata was overwritten");
        let other = jiffshot_capture::library::read_meta(&library_fixture_path(DIRECT_RENAMED)).unwrap();
        assert!(other.pinned && other.tags == ["partly saved"], "the failed pin changed another selection");
        test.line("invalid metadata was preserved and did not trigger a bulk pin");
        0
    });
    script
}

#[cfg(target_os = "macos")]
fn startup_script() -> Script {
    use super::capture_view::parts::{self, Form, Row, Subscriptions};
    use jiffshot_workflow::{capture, StartupNotice, StartupStatus};
    use rustolonia::{NodeRef, Window};
    use serde_json::{json, Value};

    let original = Rc::new(RefCell::new(None::<StartupStatus>));
    let loaded = original.clone();
    let mut script = Script(VecDeque::new());
    script.wait(1500).step(move |test| {
        let status = capture::startup_status();
        assert_ne!(status.notice, StartupNotice::Unavailable, "production login-item status: {:?}", status.detail);
        if env("JIFFSHOT_UITEST_STARTUP_LEGACY").is_some() {
            assert_eq!(status.notice, StartupNotice::Legacy, "the isolated legacy item was not detected");
        } else {
            assert_ne!(status.notice, StartupNotice::Legacy, "the test HOME must not contain a legacy item");
        }
        let tray = saved_area_tray();
        assert_eq!(tray.sink.run_at_startup.get_untracked(), status.requested);
        assert_eq!(tray.sink.startup_notice.get_untracked(), status.notice);
        *loaded.borrow_mut() = Some(status.clone());
        test.line(format!("packaged login-item status: {:?}, requested {}", status.notice, status.requested));
        run("prefs:capture");
        500
    });
    let for_preferences = original.clone();
    script.step(move |test| {
        let status = for_preferences.borrow().clone().expect("initial login-item status");
        assert!(super::prefs_window::showing_for_selftest(), "Preferences did not open");
        let encoded = sink().prefs.get_untracked();
        let dialog: Value = serde_json::from_str(parts::payload(&encoded).expect("Preferences payload")).expect("Preferences JSON");
        assert_eq!(dialog["startup_initial"], json!(status.requested));
        let tabs = dialog["tabs"].as_array().expect("Preferences tabs");
        let capture_tab = tabs.iter().find(|tab| tab["id"] == "capture").expect("Capture preferences");
        let rows_json = capture_tab["rows"].clone();
        assert_eq!(rows_json[0]["label"], "Run Jiffshot at login");
        assert_eq!(rows_json[0]["value"], json!(status.requested));
        if let Some(detail) = &status.detail {
            assert!(rows_json.as_array().unwrap().iter().any(|row| row["text"] == *detail), "login-item guidance is missing");
        }
        let rows: Vec<Row> = serde_json::from_value(rows_json).expect("Capture preference rows");
        let scope = super::scope().expect("editor scope");
        let mut inputs = Vec::new();
        let owner = NodeRef::<Window>::new();
        let subscriptions = Subscriptions::default();
        let _controls: Vec<_> = rows.iter().map(|row| {
            parts::row(&scope, Form::Prefs, row, owner, &mut inputs, &subscriptions).expect("build a Capture preference")
        }).collect();
        let mut values = parts::values(&inputs).as_array().expect("Capture preference values").clone();
        assert_eq!(values[0], json!(status.requested));
        let delay = values[5].as_i64().expect("capture delay");
        let changed_delay = if delay == 60 { 59 } else { delay + 1 };
        values[5] = json!(changed_delay);
        run(format!("prefs-apply:{}", json!({"capture": values, "_startup_initial": status.requested})));
        test.note = changed_delay.to_string();
        test.line("Preferences shows the login status and submitted an unrelated capture delay");
        700
    });
    let for_refresh = original.clone();
    script.step(move |test| {
        let status = for_refresh.borrow().clone().expect("initial login-item status");
        assert_eq!(jiffshot_workflow::settings_snapshot().delay_seconds.to_string(), test.note, "the capture delay was not saved");
        assert_eq!(capture::startup_status(), status, "saving unrelated preferences changed the login item");
        let capture_view = super::capture().sink.view.get_untracked();
        let simulated = StartupStatus {
            requested: !status.requested,
            notice: StartupNotice::Legacy,
            detail: Some("simulated external change".into()),
        };
        crate::tray::publish_startup(simulated.clone());
        let tray = saved_area_tray();
        assert_eq!(tray.sink.run_at_startup.get_untracked(), simulated.requested);
        assert_eq!(tray.sink.startup_notice.get_untracked(), simulated.notice);
        assert_eq!(super::capture().sink.view.get_untracked(), capture_view, "login status refresh changed the Capture view");
        crate::tray::publish_startup(status);
        test.line("simulated login status refresh updated only tray status");
        0
    });
    if env("JIFFSHOT_UITEST_STARTUP_WATCH").is_some() {
        struct LegacyFixture(PathBuf);
        impl Drop for LegacyFixture {
            fn drop(&mut self) {
                if let Err(error) = std::fs::remove_file(&self.0) {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        FAILED.store(true, Ordering::Release);
                        eprintln!("could not remove the scratch login-item fixture: {error}");
                    }
                }
            }
        }

        let fixture = Rc::new(RefCell::new(None::<LegacyFixture>));
        let view = Rc::new(RefCell::new(None::<String>));
        let create = fixture.clone();
        let before = view.clone();
        script.step(move |test| {
            use std::io::Write as _;
            assert!(env("JIFFSHOT_UITEST_STARTUP_LEGACY").is_none(), "the status watcher requires an initially empty scratch home");
            let ui_dir = test.dir.canonicalize().expect("resolve the test directory");
            let home = PathBuf::from(env("HOME").expect("scratch HOME")).canonicalize().expect("resolve scratch HOME");
            let expected = ui_dir.parent().expect("test parent").join("home").canonicalize().expect("resolve expected scratch home");
            assert_eq!(home, expected, "HOME must be the test directory's sibling named home");
            let agents = home.join("Library/LaunchAgents");
            std::fs::create_dir_all(&agents).expect("create scratch LaunchAgents directory");
            assert!(agents.canonicalize().expect("resolve scratch LaunchAgents").starts_with(&home), "scratch LaunchAgents escaped HOME");
            let path = agents.join("com.devolutions.jiffshot.plist");
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).expect("create a new scratch legacy item");
            *create.borrow_mut() = Some(LegacyFixture(path));
            file.write_all(br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>Label</key><string>com.devolutions.jiffshot</string>
<key>ProgramArguments</key><array><string>/usr/bin/true</string></array><key>RunAtLoad</key><true/></dict></plist>"#)
                .expect("write scratch legacy item");
            *before.borrow_mut() = Some(super::capture().sink.view.get_untracked());
            crate::app::hold_capture_state_for_selftest(true);
            test.line("created an unregistered legacy plist under scratch HOME and held the editor busy");
            11_000
        });
        script.step(move |test| {
            let status = capture::startup_status();
            let (_, state) = jiffshot_workflow::capture_state_snapshot().expect("the capture state is unavailable");
            assert_eq!(status.notice, StartupNotice::Legacy);
            assert_eq!(state.startup_notice, status.notice, "the login-item timer did not retain the legacy warning");
            assert_ne!(saved_area_tray().sink.startup_notice.get_untracked(), status.notice,
                "the busy editor published the login-item warning prematurely");
            assert!(crate::app::capture_state_busy_attempts_for_selftest() > 0, "the login-item refresh was not retried");
            super::capture().model.run(|model| model.run("switch:clipboard".into()));
            super::capture().model.run(|model| model.run("switch:clipboard".into()));
            test.line("held the login-item update while newer Capture settings were published");
            700
        });
        script.step(move |test| {
            let (_, state) = jiffshot_workflow::capture_state_snapshot().expect("the capture state is unavailable");
            assert_eq!(state.startup_notice, StartupNotice::Legacy);
            crate::app::hold_capture_state_for_selftest(false);
            test.line("released the busy editor after the login-item and Capture updates");
            700
        });
        let remove = fixture.clone();
        let during = view.clone();
        script.step(move |test| {
            let status = capture::startup_status();
            let tray = saved_area_tray();
            assert_eq!(status.notice, StartupNotice::Legacy);
            assert_eq!(tray.sink.run_at_startup.get_untracked(), status.requested);
            assert_eq!(tray.sink.startup_notice.get_untracked(), status.notice, "the login-item timer did not publish the legacy warning");
            assert_eq!(Some(super::capture().sink.view.get_untracked()), *during.borrow(), "the login-item timer changed the Capture view");
            let fixture = remove.borrow_mut().take().expect("scratch legacy item");
            let path = fixture.0.clone();
            drop(fixture);
            assert!(!path.exists(), "the scratch legacy item was not removed");
            test.line("login-item timer detected the scratch legacy item without changing the Capture view");
            11_000
        });
        let after = view;
        script.step(move |test| {
            let status = original.borrow().clone().expect("initial login-item status");
            let tray = saved_area_tray();
            assert_eq!(capture::startup_status(), status);
            assert_eq!(tray.sink.run_at_startup.get_untracked(), status.requested);
            assert_eq!(tray.sink.startup_notice.get_untracked(), status.notice, "the login-item timer did not clear the legacy warning");
            assert_eq!(Some(super::capture().sink.view.get_untracked()), *after.borrow(), "the login-item timer changed the Capture view");
            test.line("login-item timer cleared the scratch legacy warning");
            0
        });
    }
    script
}

#[cfg(target_os = "macos")]
fn saved_areas_script() -> Script {
    use super::capture_view::parts::{self, Form, Row, Subscriptions};
    use jiffshot_workflow::{panel, settings::Settings};
    use rustolonia::{NodeRef, Window};

    fn values(scope: &AppScope, panel: &panel::Panel) -> Vec<serde_json::Value> {
        let json = serde_json::to_string(&panel.rows).expect("could not serialize the saved-area panel");
        let rows: Vec<Row> = serde_json::from_str(&json).expect("could not read the saved-area panel");
        let mut inputs = Vec::new();
        let owner = NodeRef::<Window>::new();
        let subscriptions = Subscriptions::default();
        let _controls: Vec<_> = rows.iter().map(|row| {
            parts::row(scope, Form::Panel, row, owner, &mut inputs, &subscriptions).expect("could not build a saved-area row")
        }).collect();
        parts::values(&inputs).as_array().expect("panel values are not an array").clone()
    }

    let panel_view = Rc::new(RefCell::new(None));
    let opened = panel_view.clone();
    let mut script = Script(VecDeque::new());
    script.step(move |test| {
        let scope = super::scope().expect("the editor scope is unavailable");
        let mut settings = Settings::default();
        settings.capture_window.fixed_at = Some((-50000, 0));
        settings.fixed_size = (40000, 50000);
        let panel = panel::panel(&settings, "selection", false, "Capture").unwrap();
        let inputs = values(&scope, &panel);
        assert_eq!(inputs.len(), 8);
        assert_eq!((inputs[1].as_i64(), inputs[2].as_i64(), inputs[4].as_i64()), (Some(40000), Some(50000), Some(-50000)));
        assert_eq!(panel::apply(&mut settings, "selection", &inputs, false), panel::Applied::Changed);
        assert_eq!((settings.capture_window.fixed_at, settings.fixed_size), (Some((-50000, 0)), (40000, 50000)));

        settings.schedule_area = Some((-50000, 0, 40000, 50000));
        let panel = panel::panel(&settings, "delay", false, "Capture").unwrap();
        let inputs = values(&scope, &panel);
        assert_eq!(inputs.len(), 11);
        assert_eq!((inputs[6].as_i64(), inputs[8].as_i64(), inputs[9].as_i64()), (Some(-50000), Some(40000), Some(50000)));
        assert_eq!(panel::apply(&mut settings, "delay", &inputs, false), panel::Applied::Changed);
        assert_eq!(settings.schedule_area, Some((-50000, 0, 40000, 50000)));

        let monitor = jiffshot_capture::screen::monitors().into_iter().find(|monitor| monitor.primary).expect("no primary display");
        settings.capture_window.fixed = true;
        settings.capture_window.fixed_at = Some((monitor.rect.x, monitor.rect.y));
        settings.fixed_size = (100, 80);
        let mut panel = panel::panel(&settings, "selection", false, "Capture").unwrap();
        match panel.rows.last_mut() {
            Some(panel::Row::Check { value, .. }) => *value = true,
            _ => panic!("the fixed-position confirmation is missing"),
        }
        let inputs = values(&scope, &panel);
        assert_eq!(panel::apply(&mut settings, "selection", &inputs, false), panel::Applied::Changed);
        assert!(settings.macos_fixed_area.is_some());
        test.line("saved-area controls preserve legacy values and submit explicit confirmation");

        settings.macos_fixed_area.as_mut().unwrap().session_only = true;
        let fixed = panel::panel(&settings, "selection", false, "Capture").unwrap();
        assert!(fixed.rows.iter().any(|row| matches!(row, panel::Row::Note { text } if text.contains("review it again after restarting"))));
        assert_eq!(values(&scope, &fixed).len(), 7);
        let mut scheduled = settings.clone();
        scheduled.schedule_area = Some((monitor.rect.x, monitor.rect.y, 100, 80));
        let mut delay = panel::panel(&scheduled, "delay", false, "Capture").unwrap();
        match delay.rows.last_mut() {
            Some(panel::Row::Check { value, .. }) => *value = true,
            _ => panic!("the scheduled-area confirmation is missing"),
        }
        let inputs = values(&scope, &delay);
        assert_eq!(panel::apply(&mut scheduled, "delay", &inputs, false), panel::Applied::Changed);
        scheduled.macos_schedule_area.as_mut().unwrap().session_only = true;
        let mut delay = panel::panel(&scheduled, "delay", false, "Capture").unwrap();
        assert!(delay.rows.iter().any(|row| matches!(row, panel::Row::Note { text } if text.contains("review it again after restarting"))));
        assert_eq!(values(&scope, &delay).len(), 10);
        test.line("session-only notices build in both saved-area panels");
        assert_eq!(delay.action.as_ref().map(|action| action.id), Some("select-scheduled-area"));
        delay.id = "image:delay".into();
        let json = serde_json::to_string(&delay).unwrap();
        crate::capture_window::show(&scope);
        test.remember_windows();
        *opened.borrow_mut() = Some(super::capture_view::show_delay_for_selftest(&json, 300.0)
            .expect("could not open the Time delay panel").expect("the panel is not scrollable"));
        500
    });
    let scrolled = panel_view.clone();
    script.step(move |test| {
        let view = scrolled.borrow();
        let (scroll, _, _) = view.as_ref().expect("the Time delay panel is unavailable");
        let extent = scroll.extent().expect("could not read the panel extent");
        let viewport = scroll.viewport().expect("could not read the panel viewport");
        let maximum = scroll.scroll_bar_maximum().expect("could not read the panel scroll range");
        assert!(viewport.height > 0.0 && viewport.height <= 301.0, "unexpected panel viewport: {viewport:?}");
        assert!(extent.height > viewport.height && maximum.y > 0.0, "the small panel must scroll: {extent:?}, {viewport:?}");
        scroll.set_offset((0.0, maximum.y)).expect("could not scroll the Time delay panel");
        test.line(format!("Time delay panel extent {:.1}, viewport {:.1}", extent.height, viewport.height));
        250
    });
    script.step(move |test| {
        let (scroll, flyout, _) = panel_view.borrow_mut().take().expect("the Time delay panel is unavailable");
        assert!(scroll.get_offset().expect("could not read the panel offset").y > 0.0, "the Time delay panel did not scroll");
        let popup = test.new_windows().into_iter().find(|number| !window_rect(*number).is_empty())
            .expect("the Time delay flyout has no visible native window");
        let bounds = window_rect(popup);
        let monitor = jiffshot_capture::screen::monitors().into_iter()
            .find(|monitor| monitor.rect.contains(bounds.center().0, bounds.center().1))
            .expect("the Time delay flyout is not on a display");
        let tolerance = (2.0 * jiffshot_capture::screen::scale_factor()).ceil() as i32;
        assert!(bounds.height <= ((300.0 + 180.0) * jiffshot_capture::screen::scale_factor()).ceil() as i32,
            "the small-display flyout is too tall: {bounds:?}");
        assert!(bounds.x >= monitor.work.x - tolerance && bounds.y >= monitor.work.y - tolerance
            && bounds.right() <= monitor.work.right() + tolerance && bounds.bottom() <= monitor.work.bottom() + tolerance,
            "the Time delay flyout is outside the visible work area: {bounds:?}, {:?}", monitor.work);
        if env("JIFFSHOT_UITEST_SAVED_AREAS_PIXELS").is_some() {
            let frame = jiffshot_capture::screen::grab_window(popup as u32).expect("could not capture the Time delay flyout");
            frame.save_png(&test.path("saved-area-panel.png")).expect("could not save the Time delay flyout");
        }
        flyout.hide().expect("could not close the Time delay panel");
        test.line(format!("small-display Time delay flyout fits and scrolls: {bounds:?}"));
        0
    });
    if env("JIFFSHOT_UITEST_SAVED_AREAS_ENGINE").is_some() {
        add_saved_area_engine_steps(&mut script);
    }
    script
}

#[cfg(target_os = "macos")]
fn saved_area_tray() -> crate::vm::TrayBinding {
    super::TRAY.with(|slot| slot.borrow().as_ref().cloned().expect("the test tray is unavailable"))
}

#[cfg(target_os = "macos")]
fn open_saved_area_action() {
    let scope = super::scope().expect("the editor scope is unavailable");
    crate::capture_window::show(&scope);
    let settings = jiffshot_workflow::settings_snapshot();
    let mut panel = jiffshot_workflow::panel::panel(&settings, "delay", false, "Capture").expect("the Time delay panel is unavailable");
    panel.id = "image:delay".into();
    let json = serde_json::to_string(&panel).expect("could not encode the Time delay panel");
    let (_, _, action) = super::capture_view::show_delay_for_selftest(&json, 300.0)
        .expect("could not open the Time delay panel").expect("the Time delay panel is not scrollable");
    action.expect("the scheduled-area action is unavailable")();
}

#[cfg(target_os = "macos")]
fn assert_saved_area_overlay() {
    let windows = super::overlay::windows_for_selftest().expect("could not inspect the scheduled-area overlay");
    assert!(!windows.is_empty(), "the scheduled-area overlay has no windows");
    for window in windows {
        assert_eq!(platform::exclusion_window_visible(window.number), Some(true), "the selection window is not visible");
        assert!(platform::exclusion_window_alpha(window.number as isize).is_some_and(|alpha| alpha > 0.0),
            "the selection window is still cloaked");
        let actual = platform::window_bounds(window.number as isize).expect("could not read the selection window frame");
        assert_mapped_window(actual, window.bounds, window.viewport);
        let [left, top, right, bottom] = window.viewport.0;
        let expected = ((right - left) / window.scale, (bottom - top) / window.scale);
        assert!((window.surface.0 - expected.0).abs() < 0.01 && (window.surface.1 - expected.1).abs() < 0.01,
            "selection input surface {:?} does not fill the window {expected:?}", window.surface);
        let x = window.bounds[0] + (window.bounds[2] - window.bounds[0]) / 3.0;
        for y in [window.bounds[1] + 8.0 * window.scale, window.bounds[3] - 8.0 * window.scale] {
            assert!(platform::window_hits(window.number as isize, x, y),
                "the selection window cannot receive input near the display edge ({x}, {y})");
        }
    }
}

#[cfg(target_os = "macos")]
fn assert_screen_draw_window(id: u64, number: isize, rect: Rect) {
    assert!(api::draw_pending(id), "the Screen Draw session ended");
    assert_eq!(affinity(number), 0x11, "the Screen Draw window is not excluded from capture");
    assert!(platform::exclusion_window_alpha(number).is_some_and(|alpha| alpha > 0.0), "the Screen Draw window is still cloaked");
    let bounds = [f64::from(rect.x), f64::from(rect.y), f64::from(rect.right()), f64::from(rect.bottom())];
    let actual = platform::window_bounds(number).expect("could not read the Screen Draw window frame");
    let viewport = super::overlay::draw_viewport_for_selftest(id).expect("could not read the Screen Draw viewport");
    assert_mapped_window(actual, bounds, viewport);
    let (width, height, scale) = super::overlay::draw_surface_for_selftest(id).expect("could not inspect the Screen Draw surface");
    let expected = ((viewport.0[2] - viewport.0[0]) / scale, (viewport.0[3] - viewport.0[1]) / scale);
    assert!((width - expected.0).abs() < 0.01 && (height - expected.1).abs() < 0.01,
        "Screen Draw input surface ({width}, {height}) does not fill the window {expected:?}");
    let x = bounds[0] + (bounds[2] - bounds[0]) / 3.0;
    for y in [bounds[1] + 8.0 * scale, bounds[3] - 8.0 * scale] {
        assert!(platform::window_hits(number, x, y), "Screen Draw cannot receive input near the display edge ({x}, {y})");
    }
}

#[cfg(target_os = "macos")]
fn primary_screen_draw_area(small: bool) -> Rect {
    let monitor = jiffshot_capture::screen::monitors().into_iter().find(|monitor| monitor.primary).expect("no primary display");
    let rect = monitor.rect;
    if small { Rect::new(rect.x, rect.y, rect.width.min(1280), rect.height.min(720)) } else { rect }
}

#[cfg(target_os = "macos")]
fn capture_settings_handoff_script() -> Script {
    struct SettingsWriteBlock(PathBuf);
    impl Drop for SettingsWriteBlock {
        fn drop(&mut self) {
            if let Err(error) = std::fs::remove_dir(&self.0) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    FAILED.store(true, Ordering::Release);
                    eprintln!("could not remove the scratch settings blocker: {error}");
                }
            }
        }
    }

    let initial = Rc::new(RefCell::new(None));
    let recorded = initial.clone();
    type DelayedView = Box<dyn FnOnce() -> rustolonia::Result<()>>;
    let delayed_view = Rc::new(RefCell::new(None::<DelayedView>));
    let delayed_widget = Rc::new(RefCell::new(None::<DelayedView>));
    let old_view = delayed_view.clone();
    let old_widget = delayed_widget.clone();
    let mut script = Script(VecDeque::new());
    script.wait(1000);
    script.step(move |test| {
        let (_, state) = jiffshot_workflow::capture_state_snapshot().expect("the initial capture state was not published");
        let tray = saved_area_tray();
        assert_eq!(tray.sink.copy_to_clipboard.get_untracked(), state.copy_to_clipboard);
        assert_eq!(tray.sink.show_widget.get_untracked(), state.show_widget);
        assert!(!state.show_widget, "the isolated fixture must start with the widget hidden");
        *recorded.borrow_mut() = Some(state);
        *old_view.borrow_mut() = Some(Box::new(super::capture_view::delayed_view_for_selftest()));
        *old_widget.borrow_mut() = Some(Box::new(super::extras::delayed_widget_for_selftest()));
        crate::app::hold_capture_state_for_selftest(true);
        crate::app::request_capture_state_refresh();
        test.line("held the editor state busy while capture notifications arrive");
        100
    });
    add_capture_settings_steps(&mut script);
    let old = initial;
    script.step(move |test| {
        let (_, current) = jiffshot_workflow::capture_state_snapshot().expect("the edited capture state was not published");
        let previous = old.borrow().clone().expect("the initial tray state is missing");
        assert_ne!(current.copy_to_clipboard, previous.copy_to_clipboard);
        assert_eq!(saved_area_tray().sink.copy_to_clipboard.get_untracked(), previous.copy_to_clipboard,
            "a busy editor unexpectedly published the new tray state");
        assert!(crate::app::capture_state_busy_attempts_for_selftest() > 0, "the busy refresh was not retried");
        super::capture().model.run(|model| model.run("tab:2".into()));
        super::capture().model.run(|model| model.run("pick:widget".into()));
        test.line("the busy refresh retained the latest Capture and Preferences changes");
        600
    });
    script.step(move |test| {
        let (_, current) = jiffshot_workflow::capture_state_snapshot().expect("the widget change was not published");
        assert!(current.show_widget, "the widget change did not reach the capture service");
        assert!(!saved_area_tray().sink.show_widget.get_untracked(), "the busy refresh updated the tray prematurely");
        assert!(!sink().widget.get_untracked(), "the busy refresh updated the widget prematurely");
        crate::app::hold_capture_state_for_selftest(false);
        test.line("released the busy editor after a newer widget update");
        1000
    });
    let late_view = delayed_view;
    let late_widget = delayed_widget;
    script.step(move |test| {
        let (settings, state) = jiffshot_workflow::capture_state_snapshot().expect("the capture state is unavailable");
        let tray = saved_area_tray();
        assert_eq!(tray.sink.copy_to_clipboard.get_untracked(), state.copy_to_clipboard);
        assert_eq!(tray.sink.open_in_editor.get_untracked(), state.open_in_editor);
        assert_eq!(tray.sink.show_widget.get_untracked(), state.show_widget);
        assert_eq!(tray.sink.run_at_startup.get_untracked(), state.run_at_startup);
        assert_eq!(tray.sink.startup_notice.get_untracked(), state.startup_notice);
        assert!(sink().widget.get_untracked(), "the latest widget state was lost");
        assert!(super::extras::widget_visible_for_selftest().expect("could not inspect the Capture Widget"));
        assert_eq!(settings.capture_window.tab, 2);
        assert_eq!(super::capture_view::view_tab_for_selftest(), Some(2));
        let view = serde_json::to_string(&jiffshot_workflow::panel::unified_view(&settings, &state.capture_key, state.time_lapse)).unwrap();
        assert_eq!(super::capture().sink.view.get_untracked(), view, "the Capture window did not show the latest settings");
        late_view.borrow_mut().take().expect("the delayed Capture callback is missing")()
            .expect("could not apply the delayed Capture callback");
        late_widget.borrow_mut().take().expect("the delayed widget callback is missing")()
            .expect("could not apply the delayed widget callback");
        assert_eq!(super::capture_view::view_tab_for_selftest(), Some(2), "a delayed callback restored the old Capture tab");
        assert!(super::extras::widget_visible_for_selftest().expect("could not inspect the Capture Widget"),
            "a delayed callback hid the new widget");
        super::capture().model.run(|model| model.run("pick:widget".into()));
        test.line("tray, Capture window and widget retained their latest state after older callbacks ran");
        1000
    });
    script.step(move |test| {
        assert!(!sink().widget.get_untracked(), "the widget did not turn off");
        assert!(!super::extras::widget_visible_for_selftest().expect("could not inspect the Capture Widget"));
        assert!(!saved_area_tray().sink.show_widget.get_untracked());
        crate::app::hold_cancelled_token_for_selftest(912_345).expect("could not start the cancelled-quit fixture");
        super::capture().model.run(|model| model.run("switch:clipboard".into()));
        test.line("held an update during a simulated pending quit");
        700
    });
    script.step(move |test| {
        let (_, current) = jiffshot_workflow::capture_state_snapshot().expect("the pending-quit update is unavailable");
        assert_ne!(saved_area_tray().sink.copy_to_clipboard.get_untracked(), current.copy_to_clipboard,
            "the tray updated while quitting");
        crate::app::finish_cancelled_token_for_selftest(912_345);
        test.line("cancelled the pending quit with a capture update retained");
        1000
    });
    script.step(move |test| {
        let (_, current) = jiffshot_workflow::capture_state_snapshot().expect("the resumed capture state is unavailable");
        assert_eq!(saved_area_tray().sink.copy_to_clipboard.get_untracked(), current.copy_to_clipboard,
            "cancelling quit lost the pending capture update");
        test.line("the pending capture update was published after quit was cancelled");
        100
    });
    let blocked = Rc::new(RefCell::new(None::<SettingsWriteBlock>));
    let before_presets = Rc::new(RefCell::new(None));
    let install_block = blocked.clone();
    let remember_presets = before_presets.clone();
    script.step(move |test| {
        let config = PathBuf::from(env("JIFFSHOT_CONFIG_DIR").expect("scratch settings directory"))
            .canonicalize().expect("resolve the scratch settings directory");
        let root = test.dir.canonicalize().expect("resolve the self-test directory");
        assert!(config.starts_with(&root) && config != root, "settings must be inside the self-test directory");
        let temporary = config.join(format!("{}.tmp", jiffshot_capture::config::FILE));
        std::fs::create_dir(&temporary).expect("block the scratch settings write");
        *install_block.borrow_mut() = Some(SettingsWriteBlock(temporary));
        let settings = jiffshot_workflow::settings_snapshot();
        assert!(settings.presets.len() >= 2 && settings.capture_hotkey.is_none());
        *remember_presets.borrow_mut() = Some(settings.presets);
        super::capture().model.run(|model| model.run("pick:preset-delete:0".into()));
        test.line("blocked scratch settings while deleting a preset");
        700
    });
    let old_presets = before_presets;
    script.step(move |test| {
        let settings = jiffshot_workflow::settings_snapshot();
        let previous = old_presets.borrow().clone().expect("the original presets are missing");
        assert_eq!(settings.presets, previous[1..]);
        assert_eq!(jiffshot_workflow::settings::Settings::load().presets, previous, "a failed save changed the saved presets");
        let problem = super::capture().sink.problem.get_untracked();
        assert!(problem.contains("Hotkeys were not saved") && problem.contains("only until Jiffshot quits"), "{problem}");
        let mut hotkeys = vec![serde_json::Value::Null; 7 + settings.presets.len()];
        hotkeys[1] = serde_json::json!(settings.capture_key());
        crate::app::hold_cancelled_token_for_selftest(912_346).expect("could not hold the Preferences warning during quit");
        crate::capture_window::apply_preferences(Vec::new(), hotkeys, jiffshot_workflow::capture::runs_at_startup());
        test.line("preset save failure was reported; a Preferences warning is held during quit");
        700
    });
    script.step(move |test| {
        let settings = jiffshot_workflow::settings_snapshot();
        assert_eq!(settings.capture_hotkey.as_deref(), Some(settings.capture_key()));
        assert!(jiffshot_workflow::settings::Settings::load().capture_hotkey.is_none(), "a failed save changed the saved hotkey");
        assert!(!sink().toast.get_untracked().contains("Hotkeys were not saved"), "the Preferences warning appeared while quitting");
        crate::app::finish_cancelled_token_for_selftest(912_346);
        test.line("cancelled quit after the session-only hotkey change");
        700
    });
    let remove_block = blocked;
    script.step(move |test| {
        let toast = sink().toast.get_untracked();
        assert!(toast.contains("Capture preferences not fully applied") && toast.contains("Hotkeys were not saved")
            && toast.contains("only until Jiffshot quits"), "{toast}");
        drop(remove_block.borrow_mut().take().expect("the settings blocker is missing"));
        super::capture().model.run(|model| model.run("tab:1".into()));
        test.line("Preferences reported its retained session-only hotkey warning after quit was cancelled");
        700
    });
    script.step(move |test| {
        let settings = jiffshot_workflow::settings_snapshot();
        let saved = jiffshot_workflow::settings::Settings::load();
        assert_eq!(saved.presets, settings.presets);
        assert_eq!(saved.capture_hotkey, settings.capture_hotkey);
        test.line("a later successful settings write persisted the session-only hotkey and preset changes");
        0
    });
    script
}

#[cfg(target_os = "macos")]
fn add_capture_settings_steps(script: &mut Script) {
    let panel_fixed = Rc::new(RefCell::new(None));
    let requested = panel_fixed.clone();
    script.step(move |test| {
        let monitor = jiffshot_capture::screen::monitors().into_iter().find(|monitor| monitor.primary).expect("no primary display");
        let fixed = Rect::new(monitor.rect.x + 14, monitor.rect.y + 14, 100, 80);
        *requested.borrow_mut() = Some(fixed);
        let capture = super::capture();
        capture.model.run(|model| model.run("tab:1".into()));
        capture.model.run(|model| model.run(format!(
            "apply:selection:{}",
            serde_json::json!([true, fixed.width, fixed.height, true, fixed.x, fixed.y, false])
        )));
        capture.model.run(|model| model.run("switch:clipboard".into()));
        let startup = jiffshot_workflow::capture::runs_at_startup();
        crate::capture_window::apply_preferences(
            serde_json::json!([startup, false, false, true, false, 7, 2, 2, 3, true, false, 4])
                .as_array().unwrap().clone(),
            Vec::new(),
            startup,
        );
        capture.model.run(|model| model.run("apply:video:selection:[3,7]".into()));
        capture.model.run(|model| model.run("switch:preview".into()));
        capture.model.run(|model| model.run("tab:2".into()));
        capture.model.run(|model| model.run("tab:1".into()));
        test.line("queued a fixed-area confirmation, scoped video settings, Preferences and tab changes");
        2000
    });
    let confirmed = panel_fixed;
    script.step(move |test| {
        let fixed = confirmed.borrow().expect("the fixed-area request is missing");
        let settings = jiffshot_workflow::settings_snapshot();
        assert_eq!(settings.capture_window.fixed_at, Some((fixed.x, fixed.y)));
        assert_eq!(settings.fixed_size, (fixed.width, fixed.height));
        assert_eq!(settings.macos_fixed_area.as_ref().map(|proof| proof.rect), Some(fixed));
        assert!(!settings.copy_to_clipboard && !settings.open_in_editor);
        assert_eq!((settings.delay_seconds, settings.capture_window.tab), (7, 1));
        assert_eq!((settings.record_fps, settings.countdown), (60, 7));
        let persisted = jiffshot_workflow::settings::Settings::load();
        assert_eq!(persisted.macos_fixed_area, settings.macos_fixed_area);
        assert_eq!(persisted.capture_window.fixed_at, settings.capture_window.fixed_at);
        assert_eq!((persisted.copy_to_clipboard, persisted.open_in_editor, persisted.delay_seconds, persisted.capture_window.tab), (false, false, 7, 1));
        assert_eq!((persisted.record_fps, persisted.countdown), (60, 7));
        test.line("queued Capture and Preferences edits retained their order and persisted the fixed-area confirmation");
        0
    });
}

#[cfg(target_os = "macos")]
fn add_saved_area_engine_steps(script: &mut Script) {
    let old_area = Rc::new(RefCell::new(None));
    let prepared = old_area.clone();
    script.step(move |test| {
        let monitor = jiffshot_capture::screen::monitors().into_iter().find(|monitor| monitor.primary).expect("no primary display");
        let old = Rect::new(monitor.rect.x + 12, monitor.rect.y + 12, 100, 80);
        let (_, _, _, hour, minute, _) = jiffshot_capture::os::local_time();
        let later = (u32::from(hour) * 60 + u32::from(minute) + 5) % (24 * 60);
        let at = format!("{:02}:{:02}", later / 60, later % 60);
        let mut settings = jiffshot_workflow::settings::Settings::default();
        settings.schedule = Some(at.clone());
        settings.schedule_default = Some(at);
        settings.schedule_area = Some((old.x, old.y, old.width, old.height));
        settings.capture_window.timing = "schedule".into();
        settings.capture_window.timer = true;
        *prepared.borrow_mut() = Some(old);
        jiffshot_workflow::capture::apply(settings);
        test.line("scheduled-area picker: installed an unconfirmed saved area");
        1000
    });
    add_capture_settings_steps(script);
    script.step(move |test| {
        let tray = saved_area_tray();
        assert!(tray.sink.schedule.get_untracked() && tray.sink.schedule_paused.get_untracked(), "the legacy schedule must be visibly paused");
        open_saved_area_action();
        test.line("scheduled-area picker: opened from the Time delay action");
        2000
    });
    script.step(move |test| {
        let id = super::overlay::session_for_selftest().expect("the scheduled-area overlay did not open");
        assert!(api::pending(id), "the scheduled-area overlay is not pending");
        assert_saved_area_overlay();
        assert!(api::input(id, api::CANCEL, 0.0, 0.0, 0, 0).has(api::FLAG_DONE), "the scheduled-area overlay did not cancel");
        test.line("scheduled-area picker: cancelled without moving the pointer");
        1000
    });
    let cancelled = old_area.clone();
    script.step(move |test| {
        let old = cancelled.borrow().expect("the initial area is missing");
        let settings = jiffshot_workflow::settings_snapshot();
        assert_eq!(settings.schedule_area, Some((old.x, old.y, old.width, old.height)));
        assert!(settings.macos_schedule_area.is_none());
        assert!(saved_area_tray().sink.schedule_paused.get_untracked(), "cancelling must leave the schedule paused");
        open_saved_area_action();
        test.line("scheduled-area picker: cancellation preserved the previous area");
        2000
    });
    let selected = Rc::new(RefCell::new(None));
    let selection = selected.clone();
    script.step(move |test| {
        let id = super::overlay::session_for_selftest().expect("the scheduled-area overlay did not reopen");
        assert!(api::pending(id), "the reopened scheduled-area overlay is not pending");
        assert_saved_area_overlay();
        let monitor = api::input(id, api::REFRESH, 0.0, 0.0, 0, 0).monitor();
        assert!(!monitor.is_empty(), "the current monitor is unavailable");
        let before = jiffshot_capture::screen::cursor_pos();
        assert!(api::input(id, api::KEY, 0.0, 0.0, i32::from(b'F'), 0).has(api::FLAG_DONE), "the monitor was not selected");
        assert_eq!(jiffshot_capture::screen::cursor_pos(), before, "the selector moved the pointer");
        *selection.borrow_mut() = Some(monitor);
        test.line("scheduled-area picker: selected the current monitor with the keyboard");
        1000
    });
    let saved = selected.clone();
    script.step(move |test| {
        let area = saved.borrow().expect("the selected area is missing");
        let settings = jiffshot_workflow::settings_snapshot();
        assert_eq!(settings.schedule_area, Some((area.x, area.y, area.width, area.height)));
        assert_eq!(settings.macos_schedule_area.as_ref().map(|proof| proof.rect), Some(area));
        let persisted = jiffshot_workflow::settings::Settings::load();
        assert_eq!(persisted.schedule_area, settings.schedule_area);
        assert_eq!(persisted.macos_schedule_area, settings.macos_schedule_area);
        let tray = saved_area_tray();
        assert!(tray.sink.schedule.get_untracked() && !tray.sink.schedule_paused.get_untracked(), "the reviewed schedule must rearm");
        tray.model.run(|model| model.set_schedule(false));
        test.line("scheduled-area picker: saved the selected area and rearmed the schedule");
        500
    });
    script.step(move |test| {
        let tray = saved_area_tray();
        assert!(!tray.sink.schedule.get_untracked(), "the tray did not disable the schedule");
        assert!(jiffshot_workflow::settings_snapshot().schedule.is_none());
        tray.model.run(|model| model.set_schedule(true));
        test.line("scheduled-area picker: disabled the schedule from the tray");
        500
    });
    script.step(move |test| {
        let tray = saved_area_tray();
        assert!(tray.sink.schedule.get_untracked() && !tray.sink.schedule_paused.get_untracked(), "the tray did not restore the confirmed schedule");
        assert!(jiffshot_workflow::settings_snapshot().schedule.is_some());
        test.line("scheduled-area picker: the tray restored the confirmed schedule");
        500
    });
    script.step(move |test| {
        let json = api::draw_demo_sized(primary_screen_draw_area(false), 320, 200);
        assert!(!json.is_empty(), "could not start the full-display Screen Draw fixture");
        test.note = json.clone();
        test.remember_windows();
        set(sink().screen_draw, json);
        1000
    });
    script.step(move |test| {
        let id = session_of(&test.note);
        let windows = test.new_windows();
        assert_eq!(windows.len(), 1, "the full-display Screen Draw window was not shown");
        assert_screen_draw_window(id, windows[0], primary_screen_draw_area(false));
        api::draw_end();
        set(sink().screen_draw, String::new());
        test.line("full-display Screen Draw: protected frame and input surface cover the display");
        250
    });
    script.step(move |test| {
        assert!(test.new_windows().is_empty(), "the full-display Screen Draw window remained visible");
        test.line("full-display Screen Draw: window closed");
        let json = api::draw_demo_sized(primary_screen_draw_area(true), 320, 200);
        assert!(!json.is_empty(), "could not start the menu-bar-edge Screen Draw fixture");
        test.note = json.clone();
        test.remember_windows();
        set(sink().screen_draw, json);
        1000
    });
    script.step(move |test| {
        let id = session_of(&test.note);
        let windows = test.new_windows();
        assert_eq!(windows.len(), 1, "the menu-bar-edge Screen Draw window was not shown");
        assert_screen_draw_window(id, windows[0], primary_screen_draw_area(true));
        api::draw_end();
        set(sink().screen_draw, String::new());
        test.line("menu-bar-edge Screen Draw: protected frame and input surface match the selection");
        250
    });
    script.step(move |test| {
        assert!(test.new_windows().is_empty(), "the menu-bar-edge Screen Draw window remained visible");
        test.line("menu-bar-edge Screen Draw: window closed");
        0
    });
}

#[cfg(target_os = "macos")]
fn narration_script() -> Script {
    let mut s = Script(VecDeque::new());
    let harness = Rc::new(RefCell::new(None));
    let first = harness.clone();
    s.step(move |t| {
        let test = crate::video::narration_selftest::Harness::start(&t.dir).expect("start synthetic narration");
        *first.borrow_mut() = Some(test);
        t.line("narration: pending source started");
        400
    });
    let second = harness.clone();
    s.step(move |t| {
        second.borrow_mut().as_mut().expect("narration fixture").cancel_and_replace().expect("cancel and replace narration");
        t.line("narration: cancelled pending source and started replacement");
        400
    });
    let third = harness.clone();
    s.step(move |t| {
        third.borrow_mut().as_mut().expect("narration fixture").interrupt().expect("interrupt synthetic narration");
        t.line("narration: replacement started; interruption delivered");
        400
    });
    s.step(move |t| {
        harness.borrow_mut().as_mut().expect("narration fixture").check_stopped().expect("narration failure was reported");
        let status = sink().status.get_untracked();
        assert!(status.contains("synthetic microphone interruption") && status.contains("No narration audio"), "{status}");
        t.line(format!("narration: stopped and reported failure: {status}"));
        100
    });
    let finishing = Rc::new(RefCell::new(None));
    let first = finishing.clone();
    s.step(move |t| {
        *first.borrow_mut() = Some(crate::video::narration_selftest::FinishHarness::start(&t.dir).expect("start finish fixture"));
        t.line("narration: finish fixture started");
        400
    });
    let second = finishing.clone();
    s.step(move |t| {
        second.borrow_mut().as_mut().expect("finish fixture").finish_and_replace().expect("finish and replace narration");
        t.line("narration: finish was nonblocking and replacement started");
        400
    });
    let third = finishing.clone();
    s.step(move |t| {
        third.borrow_mut().as_mut().expect("finish fixture").finish_replacement().expect("finish replacement narration");
        t.line("narration: replacement survived stale completion");
        800
    });
    s.step(move |t| {
        finishing.borrow().as_ref().expect("finish fixture").check_finished().expect("finish narration");
        let status = sink().status.get_untracked();
        assert!(status.contains("No microphone samples were recorded"), "{status}");
        t.line(format!("narration: finished without a false silent export: {status}"));
        0
    });
    s
}

/// Image tools on a fresh copy of the image (coordinates assume the
/// synthetic screenshot from the doc crate's example).
fn image_tools(s: &mut Script, fresh: String) {
    let k = sink();
    let (f1, f2, f3) = (fresh.clone(), fresh.clone(), fresh);
    s.step(move |_| {
        run(format!("open:{f1}"));
        600
    })
    .step(|_| {
        run("zoom:fit");
        120
    })
    .drag("Text Replace", (0.26, 0.15), (0.84, 0.205), 12)
    .log(move |_| format!("text replace: selection={} text={}", k.selection.get_untracked(), k.text.get_untracked()))
    .step(move |_| {
        set(k.text, "Text Replace made this line editable.".into());
        300
    })
    .drag("Smart Move", (0.58, 0.54), (0.745, 0.88), 12)
    .log(move |_| format!("smart move: selection={} status={}", k.selection.get_untracked(), k.status.get_untracked()))
    .step(|t| {
        // Select drags in document pixels past fractions: 230 right, 40 up.
        run("tool:Select");
        let (x0, y0) = (t.w * 0.66, t.h * 0.71);
        canvas_feed(POINTER_PRESSED, x0, y0);
        for i in 1..=12 {
            let f = f64::from(i) / 12.0;
            canvas_feed(POINTER_MOVED, x0 + 230.0 * f, y0 - 40.0 * f);
        }
        canvas_feed(POINTER_RELEASED, x0 + 230.0, y0 - 40.0);
        120
    })
    .click("Fill", (0.02, 0.5))
    .drag("Spotlight", (0.25, 0.40), (0.50, 0.50), 12)
    .shot("ui-4-image-tools.png")
    .step(|_| {
        call(|m| m.undo());
        0
    })
    .drag("Cut Out", (0.3, 0.33), (0.3, 0.40), 12)
    .log(move |_| format!("after cut out: {}", k.document_info.get_untracked()))
    .shot("ui-5-cut-out.png")
    .step(move |_| {
        run(format!("open:{f2}"));
        600
    })
    .step(|_| {
        call(|m| m.smart_redact());
        1200
    })
    .log(move |_| format!("smart redact: {}", k.status.get_untracked()))
    .click("Magic Wand", (0.30, 0.38))
    .log(move |_| format!("magic wand: selection={}", k.selection.get_untracked()))
    .drag("Eraser", (0.05, 0.95), (0.20, 0.95), 12)
    .step(|_| {
        call(|m| m.select_all());
        120
    })
    .log(move |_| format!("select all: {} count={}", k.selection.get_untracked(), k.selection_count.get_untracked()))
    .step(|_| {
        call(|m| m.group());
        120
    })
    .log(move |_| format!("group: {}; {}", k.selection.get_untracked(), k.document_info.get_untracked()))
    .step(|_| {
        call(|m| m.ungroup());
        120
    })
    .log(move |_| format!("ungroup: {}; {}", k.selection.get_untracked(), k.document_info.get_untracked()))
    .shot("ui-5b-redact-wand.png")
    .step(move |_| {
        run(format!("open:{f3}"));
        600
    })
    .step(|_| {
        call(|m| m.simplify());
        1500
    })
    .log(move |_| format!("simplify: {}; {}", k.status.get_untracked(), k.document_info.get_untracked()))
    .shot("ui-6-simplified.png");
}

fn video_steps(s: &mut Script, video: String) {
    let k = sink();
    s.step(move |_| {
        run(format!("open:{video}"));
        800
    })
    .log(move |_| format!("video: mode={} duration={:.2} info={}", k.mode.get_untracked(), k.video_duration.get_untracked(), k.video_info.get_untracked()))
    .step(move |_| {
        set(k.video_position, k.video_duration.get_untracked() * 0.25);
        400
    })
    .step(move |_| {
        call(|m| m.set_trim_start());
        set(k.video_position, k.video_duration.get_untracked() * 0.75);
        400
    })
    .step(|_| {
        call(|m| m.set_trim_end());
        120
    })
    .log(move |_| format!("trim {:.2}..{:.2}; status {}", k.trim_start.get_untracked(), k.trim_end.get_untracked(), k.status.get_untracked()))
    .step(move |_| {
        set(k.video_position, k.video_duration.get_untracked() * 0.3);
        300
    })
    .step(|_| {
        call(|m| m.play_pause());
        800
    })
    .log(move |_| format!("playing={} position={:.2}", k.playing.get_untracked(), k.video_position.get_untracked()))
    .shot("ui-8-video.png")
    .step(move |t| {
        t.note = format!("{:.3}", k.video_position.get_untracked());
        call(|m| m.prev_frame());
        300
    })
    .step(|_| {
        call(|m| m.next_frame());
        call(|m| m.next_frame());
        300
    })
    .log(move |t| format!("frame step: {} -> {:.3}", t.note, k.video_position.get_untracked()))
    .step(move |_| {
        set(k.gif_preset, "High motion".into());
        set(k.gif_fade, true);
        call(|m| m.toggle_mute());
        200
    })
    .log(move |_| format!("gif {} fade={} muted={}", k.gif_preset.get_untracked(), k.gif_fade.get_untracked(), k.muted.get_untracked()))
    .step(move |_| {
        call(|m| m.toggle_mute());
        set(k.gif_preset, "Screen recording".into());
        set(k.gif_fade, false);
        200
    })
    .shot("ui-8b-video-options.png")
    .step(|_| {
        call(|m| m.play_pause());
        300
    })
    .step(|_| {
        call(|m| m.frame_to_image());
        500
    })
    .log(move |_| format!("frame to image: mode={} {}", k.mode.get_untracked(), k.document_info.get_untracked()));
}

/// The library item `index`'s model, if there is one.
fn library_item(index: usize) -> Option<Rc<crate::vm::LibraryItemItem>> {
    sink().library.get_untracked().get(index).cloned()
}

fn isolated_library(library: &Path, output: &Path) -> bool {
    match (std::fs::canonicalize(library), std::fs::canonicalize(output)) {
        (Ok(library), Ok(output)) => library != output && library.starts_with(output),
        _ => false,
    }
}

fn library_steps(s: &mut Script) {
    let safe = env("JIFFSHOT_LIBRARY")
        .zip(env("JIFFSHOT_UITEST"))
        .is_some_and(|(library, output)| isolated_library(Path::new(&library), Path::new(&output)));
    if !safe {
        s.log(|_| "library edits skipped: JIFFSHOT_LIBRARY must be an existing folder inside JIFFSHOT_UITEST".to_owned());
        return;
    }

    let k = sink();
    s.step(move |_| {
        if let Some(first) = library_item(0) {
            first.model.run(|m| m.copy());
        }
        300
    })
    .log(move |_| {
        use jiffshot_capture::clipboard::{read, Pasted};
        let pasted = match read(crate::winutil::main_hwnd()) {
            Ok(Some(Pasted::Image { width, height, .. })) => format!("image {width}x{height}"),
            Ok(Some(_)) => "something else".to_owned(),
            Ok(None) => "nothing".to_owned(),
            Err(error) => format!("error {error}"),
        };
        format!("library copy: {pasted}; status: {}", k.status.get_untracked())
    });
    let original = Rc::new(RefCell::new(HashMap::<String, (bool, String)>::new()));
    let saved = original.clone();
    let restore_pins = original.clone();
    s.step(move |t| {
        let items = k.library.get_untracked();
        let count = items.len();
        for item in &items {
            let fields = item.fields.get_untracked();
            saved.borrow_mut().insert(fields.path, (fields.is_pinned, fields.tags));
        }
        t.note = count.to_string();
        if count > 1 {
            if let Some(first) = library_item(0) {
                first.model.run(|m| m.set_tags("uitest, docs".into()));
            }
        }
        300
    })
    .step(move |t| {
        if t.note != "0" && t.note != "1" {
            if let Some(last) = library_item(k.library.get_untracked().len().saturating_sub(1)) {
                last.model.run(|m| m.toggle_pin());
            }
        }
        300
    })
    .step(move |t| {
        if t.note == "0" || t.note == "1" {
            t.line("library: fewer than two captures, skipped");
            return 0;
        }
        let items = k.library.get_untracked();
        let f = |i: usize| items.get(i).map(|e| e.fields.get_untracked()).unwrap_or_default();
        let (first, second) = (f(0), f(1));
        t.line(format!("library: {}; first={} pinned={} tags=[{}{}]", k.library_info.get_untracked(), first.label, first.is_pinned, second.tags, first.tags));
        set(k.library_filter, "Tag: uitest".into());
        300
    })
    .step(move |t| {
        if t.note == "0" || t.note == "1" {
            return 0;
        }
        t.line(format!("filter Tag: uitest -> {} item(s); filters: {}", k.library.get_untracked().len(), k.library_filters.get_untracked().join(" | ")));
        set(k.library_filter, "All".into());
        set(k.library_search, "zzz-nothing".into());
        300
    })
    .step(move |t| {
        if t.note == "0" || t.note == "1" {
            return 0;
        }
        t.line(format!("search zzz -> {}", k.library.get_untracked().len()));
        set(k.library_search, String::new());
        200
    })
    .step(move |t| {
        if t.note == "0" || t.note == "1" {
            return 0;
        }
        for i in [0, 1] {
            if let Some(item) = library_item(i) {
                item.model.run(|m| m.toggle_select());
            }
        }
        200
    })
    .step(move |t| {
        if t.note == "0" || t.note == "1" {
            return 0;
        }
        t.line(format!("picked {}", k.library_selected.get_untracked()));
        set(k.library_expanded, true);
        set(k.thumb_size, 180.0);
        600
    })
    .step(move |t| {
        if t.note != "0" && t.note != "1" {
            t.shot("ui-9-library.png");
            call(|m| m.clear_library_selection());
            let original = restore_pins.borrow();
            for item in k.library.get_untracked() {
                let fields = item.fields.get_untracked();
                if original.get(&fields.path).is_some_and(|(pinned, _)| *pinned != fields.is_pinned) {
                    item.model.run(|m| m.toggle_pin());
                }
            }
        }
        300
    })
    .step(move |t| {
        if t.note == "0" || t.note == "1" {
            return 0;
        }
        for item in k.library.get_untracked() {
            let fields = item.fields.get_untracked();
            let tags = original.borrow().get(&fields.path).map(|(_, tags)| tags.clone());
            if let Some(tags) = tags {
                if tags != fields.tags {
                    item.model.run(move |m| m.set_tags(tags));
                }
            }
        }
        set(k.thumb_size, 120.0);
        set(k.library_expanded, false);
        300
    })
    .log(move |t| if t.note == "0" || t.note == "1" { String::new() } else { format!("library restored: {}", k.library_info.get_untracked()) });
}

/// Templates, the toolbar and layout switches, Image's commands, and the
/// other windows: dialogs, Preferences, toasts, the HUD, the overlay, Screen
/// Draw and the Capture window.
fn windows_steps(s: &mut Script, drop: String) {
    let k = sink();
    s.step(move |_| {
        set(k.template_theme, "Ocean".into());
        set(k.template_numbers, true);
        set(k.template_captions, true);
        set(k.template_footer, "Made with Jiffshot".into());
        200
    })
    .step(|_| {
        run("template:Two sections");
        500
    })
    .log(move |_| format!("template: {}; status {}", k.document_info.get_untracked(), k.status.get_untracked()))
    .step(move |_| {
        // The test image on the first zone, and twice more for the next ones.
        run(format!("drop:200,200|{drop}|{drop}"));
        500
    })
    .log(move |_| format!("dropped: {}; selection={} panel={}", k.status.get_untracked(), k.selection.get_untracked(), k.panel.get_untracked()))
    .step(|_| {
        run("zone-fit:1");
        300
    })
    .log(move |_| format!("zone fit={}", k.zone_fit.get_untracked()))
    .step(|_| {
        call(|m| m.zoom_fit());
        400
    })
    .shot("ui-10-template.png")
    .step(|_| {
        run("toolbar:hide:Line");
        run("toolbar:up:Text");
        300
    })
    .log(move |_| format!("toolbar: {}", k.tool_order.get_untracked()))
    .shot("ui-11-toolbar.png")
    // The layout switches: the Effects panel, an icon-only toolbar, and the
    // side bar and tray hidden.
    .step(move |_| {
        set(k.show_effects, true);
        300
    })
    .shot("ui-12-effects-panel.png")
    .step(move |_| {
        set(k.show_effects, false);
        set(k.tool_labels, false);
        set(k.show_side_bar, false);
        set(k.show_tray, false);
        300
    })
    .log(move |_| format!("layout: labels={} side={} tray={}", k.tool_labels.get_untracked(), k.show_side_bar.get_untracked(), k.show_tray.get_untracked()))
    .shot("ui-13-compact.png")
    .step(move |t| {
        set(k.tool_labels, true);
        set(k.show_side_bar, true);
        set(k.show_tray, true);
        t.note = k.document_info.get_untracked();
        run("resize:800,500");
        300
    })
    .log(move |t| format!("resize: {} -> {}", t.note, k.document_info.get_untracked()))
    .step(|_| {
        call(|m| m.undo());
        run("trim-image:10,20,30,40");
        300
    })
    .log(move |_| format!("trim: -> {}", k.document_info.get_untracked()))
    .step(|_| {
        call(|m| m.undo());
        200
    })
    .step(|_| {
        call(|m| m.auto_trim());
        300
    })
    .log(move |_| format!("auto trim: -> {}; {}", k.document_info.get_untracked(), k.status.get_untracked()))
    .log(move |_| format!("recent files: {}", k.recent_files.get_untracked().len()))
    .shot_new_windows("ui-14-resize.png", |_| {
        if let (Some(owner), Some(scope)) = (super::main_window(), super::scope()) {
            let _ = super::dialogs::resize_image(&scope, &owner, 1600, 1000, |_, _| {});
        }
    })
    // Preferences: Cancelled (closed), so nothing is applied.
    .shot_new_windows("ui-15-preferences.png", |_| run("prefs:capture"))
    .shot_new_windows("ui-16-shortcuts.png", |_| {
        if let (Some(owner), Some(scope)) = (super::main_window(), super::scope()) {
            let _ = super::dialogs::keyboard_shortcuts(&scope, &owner);
        }
    })
    .shot_new_windows("ui-17-toast.png", move |_| set(k.toast, "9001\u{1f}Capture saved\u{1f}Copied to the clipboard; opened in the editor\u{1f}0".into()))
    .shot_new_windows("ui-17b-toast-problem.png", move |_| set(k.toast, "9002\u{1f}Recording failed\u{1f}The microphone is in use by another app\u{1f}1".into()));
    hud_steps(s);
    overlay_steps(s);
    capture_window_steps(s);
    s.step(|_| {
        run("toolbar:reset");
        run("tool:Arrow");
        run("style-theme:Warm");
        300
    })
    .log(move |_| {
        let styles = k.quick_styles.get_untracked();
        format!("arrow quick styles themed: {}, first stroke {}", styles.len(), styles.first().map(|q| q.fields.get_untracked().stroke).unwrap_or_else(|| "-".into()))
    })
    .step(move |_| {
        if let Some(second) = k.quick_styles.get_untracked().get(1) {
            second.model.run(|m| m.toggle_favorite());
        }
        300
    })
    .step(move |t| {
        let styles = k.quick_styles.get_untracked();
        let favorite = styles.first().is_some_and(|q| q.fields.get_untracked().is_favorite);
        t.line(format!("favorite first: {favorite}"));
        if favorite {
            styles[0].model.run(|m| m.toggle_favorite());
        }
        run("style-theme:reset");
        300
    })
    .log(move |_| format!("toolbar reset: [{}], styles reset: {}", k.tool_order.get_untracked(), k.quick_styles.get_untracked().len()))
    .step(move |_| {
        set(k.template_theme, "Light".into());
        set(k.template_numbers, false);
        set(k.template_captions, false);
        set(k.template_footer, String::new());
        200
    });
}

const HUD_PANEL: &str = r#"{"panels":[{"id":9001,"area":[200,200,640,360],"status":"● Recording 0:07 · 210 frames\nShift+F9 pause · Shift+F10 stop · Esc cancels","buttons":["Stop","Pause","Draw","Cancel"]}],"borders":[{"id":9002,"area":[200,200,640,360]}]}"#;
const HUD_RECORDING: &str = r#"{"id":9003,"area":[200,200,640,360],"status":"","buttons":[],"toolbar":{"millis":29000,"frames":870,"paused":false,"cursor":true,"highlight":false,"clicks":true,"webcam":false,"webcam_corner":"bottom-right","webcam_circle":true,"system":false,"microphone":true,"draw":false,"draw_offered":true,"note":null}}"#;

fn show_capture_tab_fixture(t: &mut Test, tab: usize, timing: &str, running: bool, schedule: Option<&str>) {
    let mut settings = jiffshot_workflow::settings::Settings::default();
    settings.capture_window.tab = tab;
    settings.capture_window.timer = timing != "delay";
    settings.capture_window.timing = timing.into();
    settings.delay_seconds = 0;
    settings.schedule_area = Some((10, 20, 640, 360));
    settings.schedule = schedule.map(str::to_owned);
    let view = jiffshot_workflow::panel::unified_view(&settings, "PrtScn", running);
    match serde_json::to_string(&view) {
        Ok(json) => set(super::capture().sink.view, json),
        Err(error) => t.line(format!("capture fixture: {error}")),
    }
}

fn show_capture_fixture(t: &mut Test, timing: &str, running: bool) {
    show_capture_tab_fixture(t, 0, timing, running, None);
}

fn show_capture_scheduled_fixture(t: &mut Test, timing: &str, running: bool) {
    show_capture_tab_fixture(t, 0, timing, running, Some("12:45"));
}

/// Checks the Capture window's normal and automatic states and its chooser.
fn capture_ux_steps(s: &mut Script) {
    let k = sink();
    s.step(|t| {
        run("theme:Dark");
        match crate::capture_window::window() {
            Some(window) => {
                if let Err(error) = window.set_show_activated(false).and_then(|_| window.show()) {
                    t.line(format!("capture window: {error:?}"));
                }
            }
            None => t.line("capture window: not mounted"),
        }
        500
    })
    .step(|t| {
        match (super::scope(), crate::capture_window::window()) {
            (Some(scope), Some(window)) => match scope.platform_handle(&window) {
                Ok(hwnd) => t.note = hwnd.to_string(),
                Err(error) => t.line(format!("capture window handle: {error:?}")),
            },
            _ => t.line("capture window: not mounted"),
        }
        show_capture_fixture(t, "delay", false);
        500
    })
    .step(|t| {
        if let Ok(hwnd) = t.note.parse::<isize>() {
            t.shot_of(hwnd, "capture-manual.png");
        } else {
            t.line("capture window: not shown");
        }
        t.shot("editor-dark.png");
        show_capture_fixture(t, "interval", false);
        500
    })
    .step(|t| {
        if let Ok(hwnd) = t.note.parse::<isize>() {
            t.shot_of(hwnd, "capture-interval.png");
        }
        show_capture_fixture(t, "interval", true);
        500
    })
    .step(|t| {
        if let Ok(hwnd) = t.note.parse::<isize>() {
            t.shot_of(hwnd, "capture-interval-running.png");
        }
        show_capture_fixture(t, "delay", true);
        500
    })
    .step(|t| {
        if let Ok(hwnd) = t.note.parse::<isize>() {
            t.shot_of(hwnd, "capture-manual-interval-running.png");
        }
        show_capture_scheduled_fixture(t, "delay", false);
        500
    })
    .step(|t| {
        if let Ok(hwnd) = t.note.parse::<isize>() {
            t.shot_of(hwnd, "capture-manual-schedule-running.png");
        }
        show_capture_scheduled_fixture(t, "delay", true);
        500
    })
    .step(|t| {
        if let Ok(hwnd) = t.note.parse::<isize>() {
            t.shot_of(hwnd, "capture-manual-both-running.png");
        }
        show_capture_scheduled_fixture(t, "interval", true);
        500
    })
    .step(|t| {
        if let Ok(hwnd) = t.note.parse::<isize>() {
            t.shot_of(hwnd, "capture-interval-schedule-running.png");
        }
        show_capture_scheduled_fixture(t, "schedule", true);
        500
    })
    .step(|t| {
        if let Ok(hwnd) = t.note.parse::<isize>() {
            t.shot_of(hwnd, "capture-schedule-both-running.png");
        }
        show_capture_fixture(t, "schedule", false);
        500
    })
    .step(move |t| {
        if let Ok(hwnd) = t.note.parse::<isize>() {
            t.shot_of(hwnd, "capture-schedule.png");
        }
        show_capture_fixture(t, "schedule", true);
        500
    })
    .step(|t| {
        if let Ok(hwnd) = t.note.parse::<isize>() {
            t.shot_of(hwnd, "capture-schedule-interval-running.png");
        }
        show_capture_tab_fixture(t, 1, "interval", true, None);
        500
    })
    .step(|t| {
        if let Ok(hwnd) = t.note.parse::<isize>() {
            t.shot_of(hwnd, "capture-image-interval-running.png");
        }
        show_capture_tab_fixture(t, 1, "delay", false, None);
        500
    })
    .step(|t| {
        if let Ok(hwnd) = t.note.parse::<isize>() {
            t.shot_of(hwnd, "capture-image-manual.png");
        }
        show_capture_tab_fixture(t, 2, "delay", false, None);
        500
    })
    .step(|t| {
        if let Ok(hwnd) = t.note.parse::<isize>() {
            t.shot_of(hwnd, "capture-video.png");
        }
        show_capture_scheduled_fixture(t, "delay", true);
        run("theme:Light");
        500
    })
    .step(|t| {
        if let Ok(hwnd) = t.note.parse::<isize>() {
            t.shot_of(hwnd, "capture-manual-both-running-light.png");
        }
        t.shot("editor-light.png");
        show_capture_fixture(t, "delay", false);
        500
    })
    .step(|t| {
        if let Ok(hwnd) = t.note.parse::<isize>() {
            t.shot_of(hwnd, "capture-manual-light.png");
        }
        show_capture_fixture(t, "interval", false);
        500
    })
    .step(move |t| {
        if let Ok(hwnd) = t.note.parse::<isize>() {
            t.shot_of(hwnd, "capture-interval-light.png");
        }
        if let Some(window) = crate::capture_window::window() {
            if let Err(error) = window.hide() {
                t.line(format!("capture window: {error:?}"));
            }
        }
        t.remember_windows();
        let chooser = serde_json::json!({
            "panels": [{
                "id": 9004,
                "area": [200, 200, 640, 360],
                "status": jiffshot_workflow::panel::SELECT_CREATE_PROMPT,
                "buttons": jiffshot_workflow::panel::SELECT_CREATE_CHOICES,
            }],
            "borders": [],
        });
        set(k.hud, chooser.to_string());
        500
    })
    .step(move |t| {
        let windows = t.new_windows();
        if windows.is_empty() {
            t.line("capture chooser: not shown");
        }
        for hwnd in windows {
            t.shot_of(hwnd, "capture-chooser.png");
        }
        set(k.hud, r#"{"panels":[],"borders":[]}"#.into());
        200
    })
    .step(|t| {
        if env("JIFFSHOT_UITEST_KEEP").is_some() {
            run("theme:Dark");
            show_capture_fixture(t, "delay", false);
            match crate::capture_window::window() {
                Some(window) => {
                    if let Err(error) = window.set_show_activated(false).and_then(|_| window.show()) {
                        t.line(format!("capture window: {error:?}"));
                    }
                }
                None => t.line("capture window: not mounted"),
            }
        }
        200
    });
}

/// A capture panel and outline from the capture side's JSON, then the
/// recording toolbar: recording, then paused with the webcam on and the
/// microphone off.
fn hud_steps(s: &mut Script) {
    let k = sink();
    s.step(move |t| {
        t.remember_windows();
        set(k.hud, HUD_PANEL.into());
        400
    })
    .step(|t| {
        for hwnd in t.new_windows() {
            let r = window_rect(hwnd);
            let name = if r.width > 600 { "ui-19-hud-border.png" } else { "ui-19-hud-panel.png" };
            t.shot_of(hwnd, name);
            let line = format!(
                "hud window at {},{} size {}x{} affinity 0x{:X}; on the frame: {}; inside: {}",
                r.x,
                r.y,
                r.width,
                r.height,
                affinity(hwnd),
                probe(r.x + 1, r.y + r.height / 2, hwnd),
                probe(r.x + r.width / 2, r.y + r.height / 2, hwnd)
            );
            t.line(line);
        }
        0
    })
    .step(move |t| {
        t.remember_windows();
        set(k.hud, format!(r#"{{"panels":[{HUD_RECORDING}],"borders":[]}}"#));
        500
    })
    .step(|t| {
        for hwnd in t.new_windows() {
            t.shot_of(hwnd, "ui-19b-recording-toolbar.png");
            let r = window_rect(hwnd);
            let line = format!("recording toolbar at {},{} size {}x{} affinity 0x{:X}", r.x, r.y, r.width, r.height, affinity(hwnd));
            t.line(line);
        }
        0
    })
    .step(move |_| {
        let paused = HUD_RECORDING
            .replace("\"paused\":false", "\"paused\":true")
            .replace("\"webcam\":false", "\"webcam\":true")
            .replace("\"microphone\":true", "\"microphone\":false")
            .replace("\"millis\":29000", "\"millis\":3725000")
            .replace("\"note\":null", "\"note\":\"no microphone: the device is in use\"");
        set(k.hud, format!(r#"{{"panels":[{paused}],"borders":[]}}"#));
        400
    })
    .step(|t| {
        for hwnd in t.new_windows() {
            t.shot_of(hwnd, "ui-19c-recording-toolbar-paused.png");
        }
        0
    })
    .step(move |_| {
        set(k.hud, r#"{"panels":[],"borders":[]}"#.into());
        200
    })
    .log(|t| format!("hud after clear: {} new windows still shown", t.new_windows().len()));
}

/// Input for the overlay showing, through its windows (so they redraw).
fn overlay_input(session: u64, kind: i32, x: f64, y: f64, value: i32, mods: i32) -> api::OverlayState {
    super::overlay::feed(kind, x, y, value, mods).unwrap_or_else(|| api::input(session, kind, x, y, value, mods))
}

fn session_of(json: &str) -> u64 {
    serde_json::from_str::<serde_json::Value>(json).ok().and_then(|v| v["id"].as_u64()).unwrap_or(0)
}

fn describe_overlay(session: u64, st: &api::OverlayState) -> String {
    let lit = st.lit();
    let monitor = st.monitor();
    format!(
        "lit {},{} {}x{} kind {} '{}' {} on {},{} {}x{} at {}x",
        lit.x,
        lit.y,
        lit.width,
        lit.height,
        st.lit_kind,
        api::text(session, api::TEXT_TITLE).expect("selection title"),
        api::text(session, api::TEXT_SIZE).expect("selection size"),
        monitor.x,
        monitor.y,
        monitor.width,
        monitor.height,
        st.scale
    )
}

/// The selection overlay over the capture side's generated desktop (two
/// screens, the second at 150%, two windows): hover, wheel, a Shift drag,
/// help, the second screen, freehand, multi-area and video guides, each shown
/// for real and saved, then input timings.
fn overlay_steps(s: &mut Script) {
    let k = sink();
    let start = move |t: &mut Test, mode: i32| -> u64 {
        let json = api::demo(mode);
        t.note = session_of(&json).to_string();
        t.remember_windows();
        set(k.overlay, json);
        600
    };
    let session = |t: &Test| t.note.parse::<u64>().unwrap_or(0);
    let shoot = |t: &mut Test, names: &[&str]| {
        let mut shown = t.new_windows();
        shown.sort_by_key(|w| window_rect(*w).x);
        for (hwnd, name) in shown.iter().zip(names) {
            if !name.is_empty() {
                t.shot_of(*hwnd, name);
            }
        }
    };
    let end = move |t: &mut Test| -> u64 {
        set(k.overlay, String::new());
        api::end_demo(t.note.parse().unwrap_or(0));
        200
    };
    s.step(move |t| start(t, 0))
        .step(move |t| {
            let id = session(t);
            let st = overlay_input(id, api::MOVE, 400.0, 300.0, 0, 0);
            t.line(format!("overlay hover: {}", describe_overlay(id, &st)));
            300
        })
        .step(move |t| {
            shoot(t, &["ui-20-overlay-hover.png", "ui-20b-overlay-other-screen.png"]);
            let id = session(t);
            let st = overlay_input(id, api::WHEEL, 400.0, 300.0, 120, 0);
            t.line(format!("overlay wheel up: {}", describe_overlay(id, &st)));
            overlay_input(id, api::PRESS, 200.0, 200.0, 0, 0);
            overlay_input(id, api::MOVE, 500.0, 400.0, 0, 0);
            let st = overlay_input(id, api::MOVE, 760.0, 460.0, 0, 1);
            t.line(format!("overlay shift drag: {}", describe_overlay(id, &st)));
            300
        })
        .step(move |t| {
            shoot(t, &["ui-21-overlay-drag.png"]);
            let st = overlay_input(session(t), api::RELEASE, 760.0, 460.0, 0, 1);
            t.line(format!("overlay released: done {}", st.has(api::FLAG_DONE)));
            end(t)
        })
        .step(move |t| start(t, 0))
        .step(move |t| {
            let id = session(t);
            overlay_input(id, api::KEY, 400.0, 300.0, 0x70, 0);
            let st = overlay_input(id, api::MOVE, 1900.0, 500.0, 0, 0);
            t.line(format!("overlay second screen: {}", describe_overlay(id, &st)));
            300
        })
        .step(move |t| {
            shoot(t, &["", "ui-22-overlay-help-150.png"]);
            // Timing: the pointer sweeping the first screen.
            let id = session(t);
            let mut times = Vec::with_capacity(240);
            for i in 0..240 {
                let f = f64::from(i) / 239.0;
                let begin = Instant::now();
                api::input(id, api::MOVE, 40.0 + 1200.0 * f, 360.0 + 300.0 * (f * 12.0).sin(), 0, 0);
                times.push(begin.elapsed().as_secs_f64() * 1000.0);
            }
            times.sort_by(f64::total_cmp);
            let avg = times.iter().sum::<f64>() / times.len() as f64;
            t.line(format!("overlay timing (240 moves): input avg {avg:.2} ms, p95 {:.2} ms, max {:.2} ms", times[228], times[239]));
            let st = overlay_input(id, api::CANCEL, 0.0, 0.0, 0, 0);
            t.line(format!("overlay cancelled: done {}", st.has(api::FLAG_DONE)));
            end(t)
        })
        .step(move |t| start(t, 1))
        .step(move |t| {
            let id = session(t);
            overlay_input(id, api::PRESS, 840.0, 360.0, 0, 0);
            let mut st = api::OverlayState::default();
            for i in 1..=60 {
                let a = f64::from(i) / 60.0 * std::f64::consts::PI * 1.7;
                st = overlay_input(id, api::MOVE, 640.0 + 200.0 * a.cos(), 360.0 + 200.0 * a.sin(), 0, 0);
            }
            let points = api::points(id, api::POINTS_PATH, 4096).len() / 2;
            t.line(format!("overlay freehand: {points} points, {}", describe_overlay(id, &st)));
            300
        })
        .step(move |t| {
            shoot(t, &["ui-23-overlay-freehand.png"]);
            end(t)
        })
        .step(move |t| start(t, 2))
        .step(move |t| {
            let id = session(t);
            for ((ax, ay), (bx, by)) in [((180.0, 150.0), (520.0, 330.0)), ((640.0, 400.0), (1000.0, 620.0))] {
                overlay_input(id, api::PRESS, ax, ay, 0, 0);
                overlay_input(id, api::MOVE, (ax + bx) / 2.0, (ay + by) / 2.0, 0, 0);
                overlay_input(id, api::MOVE, bx, by, 0, 0);
                overlay_input(id, api::RELEASE, bx, by, 0, 0);
            }
            overlay_input(id, api::MOVE, 1100.0, 200.0, 0, 0);
            t.line(format!("overlay multi-area: {} regions", api::points(id, api::POINTS_REGIONS, 4096).len() / 4));
            300
        })
        .step(move |t| {
            shoot(t, &["ui-24-overlay-multi.png"]);
            let st = overlay_input(session(t), api::KEY, 1100.0, 200.0, 0x0D, 0);
            t.line(format!("overlay multi-area Enter: done {}", st.has(api::FLAG_DONE)));
            end(t)
        })
        .step(move |t| start(t, 3))
        .step(move |t| {
            let id = session(t);
            overlay_input(id, api::KEY, 640.0, 360.0, i32::from(b'G'), 0);
            let st = overlay_input(id, api::MOVE, 640.0, 360.0, 0, 0);
            t.line(format!("overlay video guides: {}", st.has(api::FLAG_GUIDES)));
            300
        })
        .step(move |t| {
            shoot(t, &["ui-25-overlay-video-guides.png"]);
            end(t)
        })
        // Screen Draw over a small area: a box and an arrow, then the window.
        .step(move |t| {
            let json = api::draw_demo(Rect::new(120, 120, 420, 260));
            t.note = serde_json::from_str::<serde_json::Value>(&json).ok().and_then(|v| v["id"].as_u64()).unwrap_or(0).to_string();
            t.remember_windows();
            set(k.screen_draw, json);
            400
        })
        .step(move |_| {
            for (kind, x, y, value) in [
                (api::KEY, 0.0, 0.0, i32::from(b'S')),
                (api::PRESS, 60.0, 50.0, 0),
                (api::MOVE, 300.0, 200.0, 0),
                (api::RELEASE, 300.0, 200.0, 0),
                (api::KEY, 0.0, 0.0, i32::from(b'A')),
                (api::PRESS, 340.0, 40.0, 0),
                (api::MOVE, 220.0, 120.0, 0),
                (api::RELEASE, 220.0, 120.0, 0),
            ] {
                super::overlay::feed_draw(kind, x, y, value);
            }
            300
        })
        .step(move |t| {
            for hwnd in t.new_windows() {
                let r = window_rect(hwnd);
                let line = format!("screen draw window at {},{} size {}x{} affinity 0x{:X}", r.x, r.y, r.width, r.height, affinity(hwnd));
                t.line(line);
                t.shot_of(hwnd, "ui-26-screen-draw.png");
            }
            api::draw_end();
            set(k.screen_draw, String::new());
            200
        });
}

#[cfg(target_os = "macos")]
fn native_lifetime_steps(s: &mut Script) {
    s.step(|t| {
        t.note = platform::window_lifetime().expect("native window lifetime").to_string();
        300
    })
    .step(|t| {
        let number = t.note.parse::<u32>().expect("native window number");
        platform::finish_window_lifetime(number).expect("native window exclusion ownership");
        t.line("native window lifetime: close retired only its own exclusion and cleared cloak state");
        0
    });
}

#[cfg(target_os = "macos")]
fn assert_rendered_guide(rect: Rect, label: &str) {
    let guides = super::overlay::rendered_guides_for_selftest(2).expect("could not inspect the rendered video guides");
    assert_eq!(guides.len(), 2, "each synthetic monitor must draw the guide");
    for guide in guides {
        let d = |value: i32| f64::from(value) / guide.scale;
        assert_eq!(guide.bounds, [
            d(rect.x - 1 - guide.monitor.x),
            d(rect.y - 1 - guide.monitor.y),
            d(rect.width + 2),
            d(rect.height + 2),
        ]);
        assert_eq!(guide.label.as_deref(), Some(label));
        assert!(guide.visible, "the guide controls are hidden: {guide:?}");
    }
}

#[cfg(target_os = "macos")]
fn video_guides_script() -> Script {
    let mut s = Script(VecDeque::new());
    let k = sink();
    s.step(move |t| {
        let json = api::demo(3);
        t.note = session_of(&json).to_string();
        t.remember_windows();
        set(k.overlay, json);
        500
    })
    .step(|t| {
        assert!(t.new_windows().len() >= 2, "the synthetic selection windows were not shown");
        let state = super::overlay::feed(api::KEY, 0.0, 0.0, i32::from(b'G'), 0).expect("the overlay is not active");
        assert!(state.has(api::FLAG_GUIDES), "video guides did not turn on");
        let guides = super::overlay::guides_for_selftest().expect("the guide scene is unavailable");
        assert_eq!(guides[2], (Rect::new(80, 60, 640, 480), (640, 480)));
        250
    })
    .step(|_| {
        assert_rendered_guide(Rect::new(80, 60, 640, 480), "640 × 480");
        let state = super::overlay::feed(api::MOVE, 1600.0, 300.0, 0, 0).expect("the overlay is not active");
        assert!(state.has(api::FLAG_GUIDES));
        let guides = super::overlay::guides_for_selftest().expect("the guide scene is unavailable");
        assert_eq!(guides[2], (Rect::new(1280, 60, 640, 480), (640, 480)));
        250
    })
    .step(|t| {
        assert_rendered_guide(Rect::new(1280, 60, 640, 480), "640 × 480");
        let id = t.note.parse::<u64>().expect("overlay session ID");
        assert!(super::overlay::feed(api::MOVE, f64::from(i32::MIN), 300.0, 0, 0).is_none());
        assert!(!api::pending(id), "a guide geometry error did not fail the selection");
        assert!(super::overlay::guides_for_selftest().is_none(), "a failed overlay stayed active");
        api::end_demo(id);
        t.line("video guides: rendered and moved across synthetic monitors without pointer events; invalid geometry failed the selection");
        200
    })
    .step(|t| {
        assert!(t.new_windows().is_empty(), "selection windows remained visible");
        t.line("video guides: selection windows are hidden");
        0
    });
    s
}

#[cfg(target_os = "macos")]
fn assert_mapped_window(actual: [f64; 4], bounds: [f64; 4], viewport: ([f64; 4], [f64; 4])) {
    let scale = jiffshot_capture::screen::scale_factor();
    let padding = [bounds[0] - actual[0], bounds[1] - actual[1], actual[2] - bounds[2], actual[3] - bounds[3]];
    assert!(padding.iter().all(|value| *value >= -0.000_001 && *value < scale + 0.000_001), "window {actual:?} does not tightly enclose {bounds:?}");
    assert!(actual.iter().zip(viewport.0).all(|(a, b)| (a - b).abs() < 0.000_001), "surface frame {:?} differs from window {actual:?}", viewport.0);
    let content = [
        (bounds[0] - actual[0]) / scale,
        (bounds[1] - actual[1]) / scale,
        (bounds[2] - bounds[0]) / scale,
        (bounds[3] - bounds[1]) / scale,
    ];
    assert!(viewport.1.iter().zip(content).all(|(a, b)| (a - b).abs() < 0.000_001), "surface placement {:?} differs from {content:?}", viewport.1);
}

#[cfg(target_os = "macos")]
fn relative_in_window(frame: [f64; 4], x: f64, y: f64) -> (f64, f64) {
    ((x - frame[0]) / (frame[2] - frame[0]), (y - frame[1]) / (frame[3] - frame[1]))
}

#[cfg(target_os = "macos")]
fn assert_overlay_magnifier_center(grid: bool) {
    let scale = jiffshot_capture::screen::scale_factor();
    let zoom = ((8.0 * scale).round() as i32).max(6);
    let gap = (22.0 * scale).round() as i32;
    let x = 64 + gap + 7 * zoom + zoom / 2;
    let y = 300 + gap + 7 * zoom + zoom / 2;
    let pixels = platform::raw_desktop_pixels(Rect::new(x - 2, y - 2, 5, 5)).expect("magnifier pixels");
    let pixel = &pixels[(2 * 5 + 2) * 4..][..3];
    if grid {
        assert!(pixel.iter().all(|value| (40..=90).contains(value)), "the magnifier did not sample the grid pixel: {pixel:?}");
    } else {
        assert!(pixel[0] > 100 && pixel[1] > 170 && pixel[2] > 180, "the magnifier did not sample the adjacent pixel: {pixel:?}");
    }
}

#[cfg(target_os = "macos")]
fn overlay_script() -> Script {
    let mut s = Script(VecDeque::new());
    let k = sink();
    native_lifetime_steps(&mut s);
    s.step(move |t| {
        let mut announcement: serde_json::Value = serde_json::from_str(&api::demo(0)).expect("selection announcement");
        announcement["monitors"][0]["bounds"] = serde_json::json!([0.25, 0.25, 1280.25, 720.25]);
        let json = announcement.to_string();
        t.note = json.clone();
        t.remember_windows();
        set(k.overlay, json);
        600
    })
    .step(move |t| {
        let windows = t.new_windows();
        assert!(windows.len() >= 2, "the synthetic selection windows were not shown");
        let measured: Vec<_> = windows.iter().map(|number| (*number, window_rect(*number), platform::window_bounds(*number))).collect();
        let first = measured.iter().find(|(_, rect, _)| rect.x == 0 && rect.width >= 1280).map(|(number, _, _)| *number)
            .unwrap_or_else(|| panic!("the first selection window is unavailable: {measured:?}"));
        let actual = platform::window_bounds(first).expect("selection window bounds");
        let bounds = [0.25, 0.25, 1280.25, 720.25];
        let viewport = super::overlay::viewport_for_selftest(Rect::new(0, 0, 1280, 720)).expect("selection viewport");
        assert_mapped_window(actual, bounds, viewport);
        assert!(platform::window_hits(first, 401.0, 301.0), "the transparent selection window does not receive pointer hits");
        if env("JIFFSHOT_UITEST_OVERLAY_PIXELS").is_some() {
            let pixels = platform::raw_desktop_pixels(Rect::new(76, 62, 8, 8)).expect("selection pixels");
            let sample = |row: usize| &pixels[(row * 8 + 4) * 4..][..3];
            let grid = sample(2);
            let above = sample(1);
            let below = sample(3);
            assert!(grid.iter().zip(above).all(|(a, b)| u16::from(*a) + 10 < u16::from(*b))
                && grid.iter().zip(below).all(|(a, b)| u16::from(*a) + 10 < u16::from(*b)),
                "selection screenshot grid is misplaced: above {above:?}, grid {grid:?}, below {below:?}");
            t.line("overlay pixels: fractional screenshot placement matches the native display");
        }
        platform::move_in_window(first, relative_in_window(actual, 401.0, 301.0)).expect("could not deliver selection mouse movement");
        assert_eq!(super::overlay::cursor_for_selftest(), Some([401, 301]), "selection pointer did not use the enclosing window origin");
        let state = super::overlay::feed(api::MOVE, 63.75, 300.25, 0, 0).expect("the overlay is not active");
        assert_eq!(state.cursor, [64, 300]);
        assert_eq!(state.cursor_precise, [63.75, 300.25]);
        250
    })
    .step(|_| {
        if env("JIFFSHOT_UITEST_OVERLAY_PIXELS").is_some() {
            assert_overlay_magnifier_center(false);
        }
        let state = super::overlay::feed(api::MOVE, 64.25, 300.25, 0, 0).expect("the overlay is not active");
        assert_eq!(state.cursor, [64, 300]);
        assert_eq!(state.cursor_precise, [64.25, 300.25]);
        250
    })
    .step(move |t| {
        if env("JIFFSHOT_UITEST_OVERLAY_PIXELS").is_some() {
            assert_overlay_magnifier_center(true);
            t.line("overlay magnifier: fractional pointer positions select adjacent native pixels");
        }
        let id = session_of(&t.note);
        let windows = t.new_windows();
        let state = super::overlay::feed(api::WHEEL, 400.25, 300.125, 120, 0).expect("the overlay is not active");
        assert_eq!(state.cursor_precise, [400.25, 300.125]);
        assert_eq!(state.lit_kind, 1, "wheel input did not use its own pointer position");
        t.line("overlay wheel: input updates the fractional pointer and target");
        let state = super::overlay::feed(api::MOVE, 400.0, 300.0, 0, 0).expect("the overlay is not active");
        assert!(!state.has(api::FLAG_DONE));
        assert_eq!(state.cursor, [400, 300]);
        assert_eq!(state.lit_kind, 1);
        assert!(platform::close_owned_window(windows[0]), "could not close the synthetic selection window");
        set(k.overlay, String::new());
        t.line(format!("overlay {id}: selection windows shown; closed one native window"));
        200
    })
    .step(move |t| {
        set(k.overlay, t.note.clone());
        600
    })
    .step(move |t| {
        let id = session_of(&t.note);
        assert!(t.new_windows().len() >= 2, "the closed selection window was not recreated");
        super::overlay::feed(api::PRESS, 200.0, 200.0, 0, 0).expect("the restored overlay is not active");
        let state = super::overlay::feed(api::MOVE, 760.0, 460.0, 0, api::MOD_SHIFT).expect("the restored overlay stopped");
        assert_eq!((state.lit().width, state.lit().height), (520, 520), "a constrained selection must remain square at the desktop edge");
        let state = super::overlay::feed(api::RELEASE, 760.0, 460.0, 0, api::MOD_SHIFT).expect("the restored overlay stopped");
        assert!(state.has(api::FLAG_DONE), "the restored overlay did not finish selection");
        set(k.overlay, String::new());
        api::end_demo(id);
        t.line("overlay recovery: recreated the closed window and completed a selection");
        200
    })
    .step(|t| {
        assert!(t.new_windows().is_empty(), "selection windows remained visible after capture");
        t.line("overlay cleanup: selection windows are hidden");
        0
    })
    .step(move |t| {
        let json = api::draw_demo_sized(Rect::new(120, 120, 420, 260), 210, 130);
        let mut root: serde_json::Value = serde_json::from_str(&json).expect("Screen Draw demo announcement");
        assert_eq!(root["pixels"], serde_json::json!([210, 130]));
        root["bounds"] = serde_json::json!([120.25, 120.25, 540.25, 380.25]);
        t.note = root["id"].as_u64().expect("Screen Draw session ID").to_string();
        t.remember_windows();
        set(k.screen_draw, root.to_string());
        400
    })
    .step(move |t| {
        let id = t.note.parse::<u64>().expect("Screen Draw session ID");
        assert!(api::draw_pending(id));
        let windows = t.new_windows();
        assert_eq!(windows.len(), 1, "the Screen Draw window was not shown");
        let actual = platform::window_bounds(windows[0]).expect("Screen Draw window bounds");
        let bounds = [120.25, 120.25, 540.25, 380.25];
        let viewport = super::overlay::draw_viewport_for_selftest(id).expect("Screen Draw viewport");
        assert_mapped_window(actual, bounds, viewport);
        assert!(platform::window_hits(windows[0], 330.0, 250.0), "the transparent Screen Draw window does not receive pointer hits");
        super::overlay::feed_draw(api::KEY, 0.0, 0.0, i32::from(b'S'));
        let point = |fraction: f64| relative_in_window(
            actual,
            bounds[0] + (bounds[2] - bounds[0]) * fraction,
            bounds[1] + (bounds[3] - bounds[1]) * fraction,
        );
        platform::drag_window(windows[0], point(0.25), point(0.75)).expect("could not deliver Screen Draw mouse events");
        let mut pixels = vec![0; 210 * 130 * 4];
        assert_eq!(api::draw_copy(id, &mut pixels, 210, 130), Ok(true));
        let at = (32 * 210 + 105) * 4;
        let pixel = &pixels[at..at + 4];
        assert!(pixel[2] > 200 && pixel[1] < 80 && pixel[0] < 80 && pixel[3] > 200, "expected the box at its mapped position: {pixel:?}");
        t.line("screen draw: native preview and pointer map to recording pixels");
        200
    })
    .step(move |t| {
        if env("JIFFSHOT_UITEST_OVERLAY_PIXELS").is_some() {
            let pixels = platform::raw_desktop_pixels(Rect::new(324, 180, 12, 12)).expect("Screen Draw pixels");
            assert!(pixels.chunks_exact(4).any(|pixel| pixel[2] > 180 && u16::from(pixel[2]) > u16::from(pixel[0].max(pixel[1])) + 80),
                "the Screen Draw mark was not visible at the mapped position");
            t.line("screen draw pixels: the mapped mark is visible on the desktop");
        }
        api::draw_end();
        set(k.screen_draw, String::new());
        200
    })
    .step(|t| {
        assert!(t.new_windows().is_empty(), "the Screen Draw window remained visible");
        t.line("screen draw cleanup: window is hidden");
        0
    });
    s
}

#[cfg(target_os = "macos")]
fn prepare_popup(open: impl FnOnce() -> std::result::Result<(), String>) -> (i64, u32) {
    let before = super::hud::native_popup_macos::pending_for_selftest().expect("could not inspect popup registrations");
    open().expect("could not open the test popup");
    let after = super::hud::native_popup_macos::pending_for_selftest().expect("could not inspect popup registrations");
    let added: Vec<_> = after.into_iter().filter(|entry| !before.contains(entry)).collect();
    assert_eq!(added.len(), 1, "opening a popup did not create exactly one new gate token");
    let (token, number) = added[0];
    assert_eq!(platform::exclusion_window_visible(number), Some(false), "the native popup was shown before recording acknowledged it");
    assert!(jiffshot_capture::screen::is_excluded_window(number), "the native popup was not registered for exclusion");
    assert!(!jiffshot_capture::screen::exclusion_ready(number), "the popup bypassed the active recording");
    (token, number)
}

#[cfg(target_os = "macos")]
fn assert_popup_visible(popup: (i64, u32)) {
    let (token, number) = popup;
    let pending = super::hud::native_popup_macos::pending_for_selftest().expect("could not inspect popup registrations");
    assert_eq!(platform::exclusion_window_visible(number), Some(true), "popup {token}/{number} did not appear; pending: {pending:?}");
    assert!(jiffshot_capture::screen::is_excluded_window(number), "the visible popup is not registered for exclusion");
    assert_eq!(
        super::hud::native_popup_macos::acknowledge_for_selftest(token).expect("could not test duplicate acknowledgment"),
        rustolonia_sys::E_INVALIDARG,
        "the host accepted a duplicate popup acknowledgment"
    );
}

#[cfg(target_os = "macos")]
fn popup_visible_samples(area: Rect, popup_rect: Rect, raw: &[u8]) -> Vec<usize> {
    let intersection = popup_rect.intersect(area)
        .unwrap_or_else(|| panic!("the test popup {popup_rect:?} does not overlap the recording probe {area:?}"));
    let mut samples = Vec::new();
    for y in 0..130 {
        for x in 0..210 {
            let desktop_x = area.x + ((2 * x + 1) * area.width / 420);
            let desktop_y = area.y + ((2 * y + 1) * area.height / 260);
            if desktop_x >= intersection.x + 2 && desktop_x < intersection.right() - 2
                && desktop_y >= intersection.y + 2 && desktop_y < intersection.bottom() - 2
            {
                let offset = ((y * 210 + x) * 4) as usize;
                if !green_pixel(raw, offset) {
                    samples.push(offset);
                }
            }
        }
    }
    assert!(samples.len() >= 10, "the native popup was not visible over the test background: {popup_rect:?}");
    samples
}

#[cfg(target_os = "macos")]
fn green_pixel(pixels: &[u8], offset: usize) -> bool {
    let pixel = pixels.get(offset..offset + 3).expect("the popup probe frame is too small");
    pixel[1] > 160 && u16::from(pixel[1]) > u16::from(pixel[0].max(pixel[2])) + 100
}

#[cfg(target_os = "macos")]
fn assert_popup_pixels(test: &mut Test, popup: (i64, u32)) {
    let reply = platform::exclusion_reply().expect("the popup recording probe did not finish");
    let platform::ProbeReply::PopupFrames { frames, visible_frames, raw, still, frozen } = reply
        else { panic!("unexpected popup probe reply: {reply:?}") };
    assert_popup_visible(popup);
    let area = test.probe_area.expect("the popup probe area is unavailable");
    let samples = popup_visible_samples(area, window_rect(popup.1 as isize), &raw);
    assert!(frames.len() >= 10, "the popup probe recorded only {} frames", frames.len());
    assert!(visible_frames.len() >= 5, "the visible-popup probe recorded only {} frames", visible_frames.len());
    for (phase, frames) in [("opening", &frames), ("visible", &visible_frames)] {
        for (index, frame) in frames.iter().enumerate() {
            let leaked = samples.iter().filter(|&&offset| !green_pixel(frame, offset)).count();
            assert_eq!(leaked, 0, "popup pixels leaked into {phase} frame {index}: {leaked} of {} samples", samples.len());
        }
    }
    for (name, pixels) in [("still", still), ("frozen desktop", frozen)] {
        let leaked = samples.iter().filter(|&&offset| !green_pixel(&pixels, offset)).count();
        assert_eq!(leaked, 0, "popup pixels leaked into the {name}: {leaked} of {} samples", samples.len());
    }
    test.line(format!("popup excluded from {} opening and {} visible recording frames, a still capture and a frozen desktop ({} visible samples)",
        frames.len(), visible_frames.len(), samples.len()));
    test.popup_samples = samples;
}

#[cfg(target_os = "macos")]
fn editor_menu_for_selftest(open: bool) -> rustolonia::Result<()> {
    if open {
        let owner = super::main_window().ok_or(rustolonia::Error::NoUiContext)?;
        let menu = rustolonia::MenuFlyout::new()?.placement(rustolonia::PlacementMode::Center)?;
        menu.items()?.add(rustolonia::MenuItem::new()?.header_text("Normal editor menu")?)?;
        menu.show_at_with_control(&owner)?;
        EDITOR_TEST_MENU.with(|slot| *slot.borrow_mut() = Some(menu));
    } else {
        let menu = EDITOR_TEST_MENU.with(|slot| slot.borrow_mut().take()).ok_or(rustolonia::Error::NoUiContext)?;
        menu.hide()?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn assert_editor_popup_pixels(test: &mut Test, number: u32) {
    let reply = platform::exclusion_reply().expect("the editor popup recording probe did not finish");
    let platform::ProbeReply::PopupFrames { visible_frames, raw, still, frozen, .. } = reply
        else { panic!("unexpected editor popup probe reply: {reply:?}") };
    assert_eq!(platform::exclusion_window_visible(number), Some(true), "the editor popup disappeared");
    assert!(!jiffshot_capture::screen::is_excluded_window(number), "the editor popup was incorrectly excluded");
    let samples = popup_visible_samples(test.probe_area.expect("the popup probe area is unavailable"), window_rect(number as isize), &raw);
    assert!(visible_frames.len() >= 5, "too few editor popup frames were captured");
    for (name, pixels) in visible_frames.iter().map(|pixels| ("recording", pixels))
        .chain([("still", &still), ("frozen desktop", &frozen)])
    {
        let visible = samples.iter().filter(|&&offset| !green_pixel(pixels, offset)).count();
        assert!(visible >= samples.len() / 4, "the normal editor popup was missing from the {name}: {visible} of {} samples", samples.len());
    }
    test.line("normal editor popups remain visible in recording, still and frozen captures");
}

#[cfg(target_os = "macos")]
fn assert_popup_closed(popup: (i64, u32)) {
    let (token, _) = popup;
    assert!(!super::hud::native_popup_macos::pending_for_selftest().expect("could not inspect popup registrations")
        .iter().any(|entry| entry.0 == token), "the closed popup retained its gate token");
    assert_eq!(
        super::hud::native_popup_macos::acknowledge_for_selftest(token).expect("could not test stale acknowledgment"),
        rustolonia_sys::E_INVALIDARG,
        "the host accepted a closed popup acknowledgment"
    );
}

#[cfg(target_os = "macos")]
fn multistream_exclusion_script() -> Script {
    let mut s = Script(VecDeque::new());
    let k = sink();
    s.step(|t| {
        t.probe_area = Some(platform::start_exclusion_probe().expect("could not start the recording exclusion probe"));
        t.probe_deadline = Some(Instant::now() + Duration::from_secs(15));
        0
    })
    .step(move |t| {
        let reply = platform::exclusion_reply().expect("the first recording did not start");
        assert!(matches!(reply, platform::ProbeReply::Ready), "unexpected first recording reply: {reply:?}");
        t.remember_windows();
        set(k.widget, true);
        500
    })
    .step(|t| {
        let windows = t.new_windows();
        assert_eq!(windows.len(), 1, "the Capture Widget was not created");
        assert_eq!(platform::exclusion_window_alpha(windows[0]), Some(0.0), "the Capture Widget bypassed the first recording");
        t.note = windows[0].to_string();
        platform::exclusion_command(platform::ProbeCommand::Refresh).expect("could not update the first recording filter");
        t.probe_deadline = Some(Instant::now() + Duration::from_secs(15));
        0
    })
    .step(|_| {
        let reply = platform::exclusion_reply().expect("the first recording did not acknowledge the widget");
        assert!(matches!(reply, platform::ProbeReply::Refreshed), "unexpected first recording reply: {reply:?}");
        500
    })
    .step(|t| {
        let widget = t.note.parse::<isize>().expect("the Capture Widget number is invalid");
        assert_eq!(platform::exclusion_window_alpha(widget), Some(1.0), "the Capture Widget was not revealed");
        super::extras::expand_widget_for_selftest().expect("could not expand the Capture Widget");
        500
    })
    .step(|t| {
        let widget = t.note.parse::<isize>().expect("the Capture Widget number is invalid");
        let area = t.probe_area.expect("the popup probe area is unavailable");
        platform::place_for_exclusion_probe(widget, area.x + 80, area.y + 80).expect("could not position the test Capture Widget");
        platform::exclusion_command(platform::ProbeCommand::StartSecond).expect("could not start the second recording");
        t.probe_deadline = Some(Instant::now() + Duration::from_secs(15));
        0
    })
    .step(|t| {
        let reply = platform::exclusion_reply().expect("the second recording did not start");
        assert!(matches!(reply, platform::ProbeReply::SecondReady), "unexpected second recording reply: {reply:?}");
        t.popup = Some(prepare_popup(|| super::extras::widget_tooltip_for_selftest(true)));
        platform::exclusion_command(platform::ProbeCommand::Refresh).expect("could not update the first popup filter");
        t.probe_deadline = Some(Instant::now() + Duration::from_secs(15));
        0
    })
    .step(|t| {
        let reply = platform::exclusion_reply().expect("the first recording did not update its popup filter");
        assert!(matches!(reply, platform::ProbeReply::Refreshed), "unexpected first recording reply: {reply:?}");
        let (token, number) = t.popup.expect("the widget tooltip was not recorded");
        assert_eq!(platform::exclusion_window_visible(number), Some(false), "popup {token} was revealed while the second recording was stale");
        assert!(!jiffshot_capture::screen::exclusion_ready(number), "one recording acknowledged the popup for both streams");
        t.line("popup remains hidden after the first recording acknowledges it");
        1000
    })
    .step(|t| {
        let (_, number) = t.popup.expect("the widget tooltip was not recorded");
        assert_eq!(platform::exclusion_window_visible(number), Some(false), "the popup became visible while the second recording was stale");
        assert!(!jiffshot_capture::screen::exclusion_ready(number), "the stale recording was unexpectedly acknowledged");
        jiffshot_capture::screen::exclude_window(u32::MAX, true);
        platform::exclusion_command(platform::ProbeCommand::RefreshSecond).expect("could not exercise the second recording's resolution failure");
        t.probe_deadline = Some(Instant::now() + Duration::from_secs(15));
        0
    })
    .step(|t| {
        let error = platform::exclusion_reply().expect_err("the invalid exclusion unexpectedly succeeded");
        assert!(error.contains("could not apply capture exclusions") && error.contains(&u32::MAX.to_string()),
            "unexpected second-filter failure: {error}");
        let (_, number) = t.popup.expect("the widget tooltip was not recorded");
        assert_eq!(platform::exclusion_window_visible(number), Some(false), "the popup was revealed after an exclusion-resolution failure");
        assert!(!jiffshot_capture::screen::exclusion_ready(number), "the failed resolution acknowledged the popup");
        t.line("popup remains hidden after the second recording cannot resolve an exclusion");
        500
    })
    .step(|t| {
        let (_, number) = t.popup.expect("the widget tooltip was not recorded");
        assert_eq!(platform::exclusion_window_visible(number), Some(false), "the popup became visible after an exclusion-resolution failure");
        jiffshot_capture::screen::exclude_window(u32::MAX, false);
        platform::exclusion_command(platform::ProbeCommand::Refresh).expect("could not restore the first popup filter");
        t.probe_deadline = Some(Instant::now() + Duration::from_secs(15));
        0
    })
    .step(|t| {
        let reply = platform::exclusion_reply().expect("the first recording did not restore its popup filter");
        assert!(matches!(reply, platform::ProbeReply::Refreshed), "unexpected first recording reply: {reply:?}");
        let (_, number) = t.popup.expect("the widget tooltip was not recorded");
        assert_eq!(platform::exclusion_window_visible(number), Some(false), "the popup was revealed before the second filter recovered");
        assert!(!jiffshot_capture::screen::exclusion_ready(number), "the stale second recording was unexpectedly acknowledged");
        platform::exclusion_command(platform::ProbeCommand::RefreshSecond).expect("could not restore the second popup filter");
        t.probe_deadline = Some(Instant::now() + Duration::from_secs(15));
        0
    })
    .step(|_| {
        let reply = platform::exclusion_reply().expect("the second recording did not update its popup filter");
        assert!(matches!(reply, platform::ProbeReply::Refreshed), "unexpected second recording reply: {reply:?}");
        500
    })
    .step(|t| {
        assert_popup_visible(t.popup.expect("the widget tooltip was not recorded"));
        platform::exclusion_command(platform::ProbeCommand::DualFlow).expect("could not capture both recording streams");
        t.probe_deadline = Some(Instant::now() + Duration::from_secs(15));
        0
    })
    .step(move |t| {
        let reply = platform::exclusion_reply().expect("the dual recording probe did not finish");
        let platform::ProbeReply::DualFrames { first, second, raw } = reply
            else { panic!("unexpected dual recording probe reply: {reply:?}") };
        let popup = t.popup.take().expect("the widget tooltip was not recorded");
        let samples = popup_visible_samples(t.probe_area.expect("the popup probe area is unavailable"), window_rect(popup.1 as isize), &raw);
        for (name, frames) in [("first", &first), ("second", &second)] {
            assert!(frames.len() >= 5, "the {name} recording captured only {} frames", frames.len());
            for (index, frame) in frames.iter().enumerate() {
                let leaked = samples.iter().filter(|&&offset| !green_pixel(frame, offset)).count();
                assert_eq!(leaked, 0, "popup pixels leaked into the {name} recording frame {index}: {leaked} of {} samples", samples.len());
            }
        }
        super::extras::widget_tooltip_for_selftest(false).expect("could not close the Capture Widget tooltip");
        assert_popup_closed(popup);
        set(k.widget, false);
        t.note = popup.1.to_string();
        t.line(format!("popup remained hidden until both recordings acknowledged it and stayed excluded in {} and {} frames ({} visible samples)",
            first.len(), second.len(), samples.len()));
        platform::finish_exclusion_probe().expect("could not stop both recordings");
        500
    })
    .step(|t| {
        platform::join_exclusion_probe().expect("could not join both recording probes");
        let number = t.note.parse::<u32>().expect("the closed tooltip number is invalid");
        assert!(!jiffshot_capture::screen::is_excluded_window(number), "the retired tooltip remained excluded after both recordings stopped");
        t.line("both recording exclusion probes cleaned up");
        0
    });
    s
}

#[cfg(target_os = "macos")]
fn exclusion_script() -> Script {
    let mut s = Script(VecDeque::new());
    let k = sink();
    s.step(|t| {
        let area = platform::start_exclusion_probe().expect("could not start the recording exclusion probe");
        t.probe_area = Some(area);
        t.note = serde_json::json!([area.x, area.y, area.width, area.height]).to_string();
        1500
    })
    .step(move |t| {
        assert!(matches!(platform::exclusion_reply(), Ok(platform::ProbeReply::Ready)), "recording exclusion probe did not start");
        let area: [i32; 4] = serde_json::from_str(&t.note).expect("exclusion probe area");
        let json = api::draw_demo_sized(Rect::new(area[0], area[1], area[2], area[3]), 210, 130);
        assert!(!json.is_empty(), "could not start Screen Draw");
        t.note = json.clone();
        t.remember_windows();
        set(k.screen_draw, json);
        400
    })
    .step(|t| {
        let windows = t.new_windows();
        assert_eq!(windows.len(), 1, "the protected Screen Draw window was not created");
        assert_eq!(platform::exclusion_window_alpha(windows[0]), Some(0.0), "Screen Draw was revealed before recording acknowledged it");
        assert_eq!(platform::exclusion_window_ignores_input(windows[0]), Some(true), "hidden Screen Draw accepted input");
        assert!(jiffshot_capture::screen::virtual_screen().intersect(window_rect(windows[0])).is_none(), "hidden Screen Draw was placed on a display");
        super::overlay::feed_draw(api::KEY, 0.0, 0.0, i32::from(b'S'));
        super::overlay::feed_draw(api::PRESS, 52.0, 32.0, 0);
        super::overlay::feed_draw(api::MOVE, 157.0, 97.0, 0);
        super::overlay::feed_draw(api::RELEASE, 157.0, 97.0, 0);
        platform::exclusion_command(platform::ProbeCommand::Raw).expect("could not inspect the hidden window");
        1000
    })
    .step(|t| {
        let reply = platform::exclusion_reply().expect("could not inspect the hidden window");
        let platform::ProbeReply::Pixel(pixel) = reply else { panic!("unexpected hidden-window probe reply: {reply:?}") };
        assert!(pixel[1] > 160 && u16::from(pixel[1]) > u16::from(pixel[0].max(pixel[2])) + 100, "hidden Screen Draw leaked into capture: {pixel:?}");
        t.line("screen draw remains invisible before exclusion acknowledgment");
        platform::exclusion_command(platform::ProbeCommand::Refresh).expect("could not update the recording filter");
        1500
    })
    .step(|t| {
        assert!(matches!(platform::exclusion_reply(), Ok(platform::ProbeReply::Refreshed)), "recording did not acknowledge the exclusion");
        let windows = t.new_windows();
        assert_eq!(windows.len(), 1);
        assert_eq!(platform::exclusion_window_alpha(windows[0]), Some(1.0), "Screen Draw was not revealed after acknowledgment");
        assert_eq!(platform::exclusion_window_ignores_input(windows[0]), Some(false), "revealed Screen Draw did not accept input");
        platform::exclusion_command(platform::ProbeCommand::Raw).expect("could not inspect the revealed window");
        1000
    })
    .step(|t| {
        let reply = platform::exclusion_reply().expect("could not inspect the revealed window");
        let platform::ProbeReply::Pixel(pixel) = reply else { panic!("unexpected visible-window probe reply: {reply:?}") };
        assert!(pixel[2] > 160 && u16::from(pixel[2]) > u16::from(pixel[0].max(pixel[1])) + 80, "the revealed Screen Draw mark was not visible: {pixel:?}");
        platform::exclusion_command(platform::ProbeCommand::Filtered).expect("could not inspect the filtered recording");
        t.line("screen draw becomes visible after exclusion acknowledgment");
        1000
    })
    .step(move |t| {
        let reply = platform::exclusion_reply().expect("could not inspect the filtered recording");
        let platform::ProbeReply::Pixel(pixel) = reply else { panic!("unexpected recording probe reply: {reply:?}") };
        assert!(pixel[1] > 160 && u16::from(pixel[1]) > u16::from(pixel[0].max(pixel[2])) + 100, "Screen Draw leaked into the filtered recording: {pixel:?}");
        platform::exclusion_command(platform::ProbeCommand::Still).expect("could not inspect a still capture");
        t.line("screen draw is excluded from recording after being revealed");
        1500
    })
    .step(|_| {
        let platform::ProbeReply::Pixel(pixel) = platform::exclusion_reply().expect("could not inspect the still capture")
            else { panic!("unexpected still-capture probe reply") };
        assert!(pixel[1] > 160 && u16::from(pixel[1]) > u16::from(pixel[0].max(pixel[2])) + 100, "Screen Draw leaked into a still capture: {pixel:?}");
        platform::exclusion_command(platform::ProbeCommand::Frozen).expect("could not inspect a frozen desktop");
        1500
    })
    .step(|t| {
        let platform::ProbeReply::Pixel(pixel) = platform::exclusion_reply().expect("could not inspect the frozen desktop")
            else { panic!("unexpected frozen-desktop probe reply") };
        assert!(pixel[1] > 160 && u16::from(pixel[1]) > u16::from(pixel[0].max(pixel[2])) + 100, "Screen Draw leaked into a frozen desktop: {pixel:?}");
        let windows = t.new_windows();
        assert_eq!(windows.len(), 1);
        platform::stall_window_close(windows[0]).expect("could not observe the close notification");
        platform::exclusion_command(platform::ProbeCommand::WatchClose(windows[0] as u32)).expect("could not watch the recording while closing");
        t.line("screen draw is excluded from still captures and frozen desktops");
        300
    })
    .step(move |_| {
        assert!(matches!(platform::exclusion_reply(), Ok(platform::ProbeReply::Watching)), "the close probe did not start");
        api::draw_end();
        set(k.screen_draw, String::new());
        1500
    })
    .step(|t| {
        platform::exclusion_command(platform::ProbeCommand::StopWatching).expect("could not finish the close probe");
        t.probe_deadline = Some(Instant::now() + Duration::from_secs(15));
        0
    })
    .step(|t| {
        let platform::ProbeReply::CloseFrames { frames, leaked, first, registered } = platform::exclusion_reply().expect("the close probe did not finish")
            else { panic!("unexpected close probe reply") };
        assert!(platform::finish_window_close_stall().expect("the close observer is unavailable"), "the close notification was not observed");
        assert!(frames >= 10, "the close probe captured only {frames} frames");
        assert_eq!(leaked, 0, "Screen Draw leaked into {leaked} of {frames} frames while closing (first {first:?}, registered {registered})");
        t.line(format!("screen draw stayed excluded in {frames} frames while closing"));
        0
    })
    .step(move |t| {
        assert!(t.new_windows().is_empty(), "the Screen Draw window did not close");
        t.remember_windows();
        set(k.toast, "9101\u{1f}Recording notice\u{1f}Synthetic notification\u{1f}0".into());
        500
    })
    .step(|t| {
        let windows = t.new_windows();
        assert_eq!(windows.len(), 1, "the notification window was not created");
        assert_eq!(platform::exclusion_window_alpha(windows[0]), Some(0.0), "the notification was revealed before exclusion acknowledgment");
        assert_eq!(platform::exclusion_window_ignores_input(windows[0]), Some(true), "the hidden notification accepted input");
        assert!(jiffshot_capture::screen::virtual_screen().intersect(window_rect(windows[0])).is_none(), "the hidden notification was placed on a display");
        assert_eq!(affinity(windows[0]), 0x11, "the notification was not registered for exclusion");
        platform::exclusion_command(platform::ProbeCommand::Refresh).expect("could not update the notification filter");
        1500
    })
    .step(move |t| {
        assert!(matches!(platform::exclusion_reply(), Ok(platform::ProbeReply::Refreshed)), "recording did not acknowledge the notification");
        let windows = t.new_windows();
        assert_eq!(windows.len(), 1);
        assert_eq!(platform::exclusion_window_alpha(windows[0]), Some(1.0), "the notification was not revealed");
        assert!(jiffshot_capture::screen::virtual_screen().intersect(window_rect(windows[0])).is_some(), "the notification was not placed on a display");
        set(k.toast, "9102\u{1f}Replacement notice\u{1f}Still excluded\u{1f}0".into());
        500
    })
    .step(|t| {
        let windows = t.new_windows();
        assert_eq!(windows.len(), 1, "replacing a notification created another window");
        assert_eq!(platform::exclusion_window_alpha(windows[0]), Some(1.0), "the replacement notification disappeared");
        t.line("notifications wait for exclusion acknowledgment and can be replaced while visible");
        5500
    })
    .step(|t| {
        assert!(t.new_windows().is_empty(), "the notification did not time out");
        platform::exclusion_command(platform::ProbeCommand::Fresh).expect("could not start a recording with the notification hidden");
        1500
    })
    .step(move |t| {
        assert!(matches!(platform::exclusion_reply(), Ok(platform::ProbeReply::Fresh)), "a hidden notification prevented a new recording");
        t.remember_windows();
        set(k.widget, true);
        500
    })
    .step(|t| {
        let windows = t.new_windows();
        assert_eq!(windows.len(), 1, "the Capture Widget was not created");
        assert_eq!(platform::exclusion_window_alpha(windows[0]), Some(0.0), "the Capture Widget was revealed before exclusion acknowledgment");
        assert_eq!(platform::exclusion_window_ignores_input(windows[0]), Some(true), "the hidden Capture Widget accepted input");
        assert!(jiffshot_capture::screen::virtual_screen().intersect(window_rect(windows[0])).is_none(), "the hidden Capture Widget was placed on a display");
        assert_eq!(affinity(windows[0]), 0x11, "the Capture Widget was not registered for exclusion");
        platform::exclusion_command(platform::ProbeCommand::Refresh).expect("could not update the Capture Widget filter");
        1500
    })
    .step(move |t| {
        assert!(matches!(platform::exclusion_reply(), Ok(platform::ProbeReply::Refreshed)), "recording did not acknowledge the Capture Widget");
        let windows = t.new_windows();
        assert_eq!(windows.len(), 1);
        assert_eq!(platform::exclusion_window_alpha(windows[0]), Some(1.0), "the Capture Widget was not revealed");
        assert!(jiffshot_capture::screen::virtual_screen().intersect(window_rect(windows[0])).is_some(), "the Capture Widget was not placed on a display");
        super::extras::expand_widget_for_selftest().expect("could not expand the Capture Widget");
        500
    })
    .step(|t| {
        let area = t.probe_area.expect("the popup probe area is unavailable");
        let widget = *t.new_windows().first().expect("the Capture Widget is unavailable");
        platform::place_for_exclusion_probe(widget, area.x + 80, area.y + 80).expect("could not position the test Capture Widget");
        t.line(format!("widget probe owner: {}", platform::describe_window(widget)));
        let popup = prepare_popup(|| super::extras::widget_tooltip_for_selftest(true));
        super::extras::widget_tooltip_for_selftest(false).expect("could not close the pending Capture Widget tooltip");
        assert_popup_closed(popup);
        t.popup = Some(popup);
        300
    })
    .step(|t| {
        let old = t.popup.take().expect("the pending widget tooltip was not recorded");
        for window in t.new_windows() {
            t.line(format!("widget before reopening tooltip: {}", platform::describe_window(window)));
        }
        let popup = prepare_popup(|| super::extras::widget_tooltip_for_selftest(true));
        assert_ne!(popup.0, old.0, "a recreated Capture Widget tooltip reused a gate token");
        t.popup = Some(popup);
        platform::exclusion_command(platform::ProbeCommand::Flow).expect("could not update the Capture Widget tooltip filter");
        5000
    })
    .step(move |t| {
        let popup = t.popup.take().expect("the Capture Widget tooltip was not recorded");
        assert_popup_pixels(t, popup);
        super::extras::widget_tooltip_for_selftest(false).expect("could not close the Capture Widget tooltip");
        assert_popup_closed(popup);
        set(k.widget, false);
        t.line("Capture Widget tooltip stays hidden until exclusion and safely closes and recreates");
        300
    })
    .step(move |t| {
        assert!(t.new_windows().is_empty(), "the Capture Widget did not hide");
        set(k.widget, true);
        500
    })
    .step(|t| {
        let windows = t.new_windows();
        assert_eq!(windows.len(), 1, "the Capture Widget did not reopen");
        assert_eq!(platform::exclusion_window_alpha(windows[0]), Some(0.0), "the reopened Capture Widget bypassed exclusion acknowledgment");
        platform::exclusion_command(platform::ProbeCommand::Refresh).expect("could not update the reopened Capture Widget filter");
        1500
    })
    .step(move |t| {
        assert!(matches!(platform::exclusion_reply(), Ok(platform::ProbeReply::Refreshed)), "recording did not acknowledge the reopened Capture Widget");
        let windows = t.new_windows();
        assert_eq!(windows.len(), 1);
        assert_eq!(platform::exclusion_window_alpha(windows[0]), Some(1.0), "the reopened Capture Widget was not revealed");
        assert_eq!(super::hud::native_popup_macos::state_for_selftest(), (1, true), "the reopened Capture Widget lost its popup gate");
        super::extras::expand_widget_for_selftest().expect("could not expand the reopened Capture Widget");
        300
    })
    .step(|t| {
        let popup = prepare_popup(|| super::extras::widget_tooltip_for_selftest(true));
        t.popup = Some(popup);
        platform::exclusion_command(platform::ProbeCommand::Refresh).expect("could not update the reopened tooltip filter");
        1500
    })
    .step(move |t| {
        assert!(matches!(platform::exclusion_reply(), Ok(platform::ProbeReply::Refreshed)), "recording did not acknowledge the reopened tooltip");
        let popup = t.popup.take().expect("the reopened Capture Widget tooltip was not recorded");
        assert_popup_visible(popup);
        super::extras::widget_tooltip_for_selftest(false).expect("could not close the reopened Capture Widget tooltip");
        assert_popup_closed(popup);
        set(k.widget, false);
        t.line("Capture Widget and its reopened tooltip wait for exclusion acknowledgment");
        300
    })
    .step(|t| {
        assert!(t.new_windows().is_empty(), "the Capture Widget remained visible");
        platform::exclusion_command(platform::ProbeCommand::Fresh).expect("could not start a recording with the Capture Widget hidden");
        1500
    })
    .step(|_| {
        assert!(matches!(platform::exclusion_reply(), Ok(platform::ProbeReply::Fresh)), "a hidden Capture Widget prevented a new recording");
        0
    })
    .step(move |t| {
        assert!(t.new_windows().is_empty(), "the Screen Draw window did not close");
        let announcement: serde_json::Value = serde_json::from_str(&t.note).expect("Screen Draw announcement");
        let mut toolbar: serde_json::Value = serde_json::from_str(HUD_RECORDING).expect("recording toolbar fixture");
        toolbar["area"] = announcement["area"].clone();
        let mut hud: serde_json::Value = serde_json::from_str(HUD_PANEL).expect("capture controls fixture");
        hud["panels"].as_array_mut().expect("capture panels").push(toolbar);
        t.remember_windows();
        set(k.hud, hud.to_string());
        500
    })
    .step(|t| {
        let windows = t.new_windows();
        for &window in &windows {
            t.line(platform::describe_window(window));
        }
        assert_eq!(windows.len(), 3, "the capture panel, outline and toolbar were not created");
        for &window in &windows {
            assert_eq!(platform::exclusion_window_alpha(window), Some(0.0), "capture controls were revealed before exclusion acknowledgment");
            assert_eq!(affinity(window), 0x11, "capture controls were not registered for exclusion");
            assert_eq!(platform::exclusion_window_ignores_input(window), Some(true), "hidden capture controls accepted input");
            assert!(jiffshot_capture::screen::virtual_screen().intersect(window_rect(window)).is_none(), "hidden capture controls were placed on a display");
        }
        t.note = serde_json::to_string(&windows).expect("capture control window numbers");
        platform::exclusion_command(platform::ProbeCommand::Refresh).expect("could not update the capture controls filter");
        1500
    })
    .step(move |t| {
        assert!(matches!(platform::exclusion_reply(), Ok(platform::ProbeReply::Refreshed)), "recording did not acknowledge the capture controls");
        let windows = t.new_windows();
        for &window in &windows {
            t.line(platform::describe_window(window));
        }
        let controls: Vec<isize> = serde_json::from_str(&t.note).expect("capture control window numbers");
        for &window in &controls {
            assert!(windows.contains(&window), "a capture control window disappeared");
            assert_eq!(platform::exclusion_window_alpha(window), Some(1.0), "capture controls were not revealed after exclusion acknowledgment");
            let rect = window_rect(window);
            assert!(!rect.is_empty() && jiffshot_capture::screen::virtual_screen().intersect(rect).is_some(),
                "capture controls were not placed on a display: {rect:?}");
        }
        for &window in windows.iter().filter(|window| !controls.contains(window)) {
            assert_eq!(affinity(window), 0x11, "visible capture-control popup {window} is not registered for exclusion");
            assert!(jiffshot_capture::screen::exclusion_ready(window as u32), "capture-control popup {window} was revealed before exclusion acknowledgment");
        }
        super::hud::toolbar_tooltip_for_selftest(9003, false).expect("could not reset the toolbar tooltip");
        let area = t.probe_area.expect("the popup probe area is unavailable");
        let toolbar = *controls.iter().max_by_key(|&&window| window_rect(window).width).expect("the recording toolbar is unavailable");
        platform::place_for_exclusion_probe(toolbar, area.x + 30, area.y + 130).expect("could not position the test toolbar");
        300
    })
    .step(|t| {
        let popup = prepare_popup(|| super::hud::toolbar_tooltip_for_selftest(9003, true));
        t.popup = Some(popup);
        platform::exclusion_command(platform::ProbeCommand::Flow).expect("could not update the toolbar tooltip filter");
        5000
    })
    .step(|t| {
        let popup = t.popup.take().expect("the toolbar tooltip was not recorded");
        assert_popup_pixels(t, popup);
        super::hud::toolbar_tooltip_for_selftest(9003, false).expect("could not close the toolbar tooltip");
        assert_popup_closed(popup);
        let controls: Vec<isize> = serde_json::from_str(&t.note).expect("capture control window numbers");
        let area = t.probe_area.expect("the popup probe area is unavailable");
        let toolbar = *controls.iter().max_by_key(|&&window| window_rect(window).width).expect("the recording toolbar is unavailable");
        platform::place_for_exclusion_probe(toolbar, area.x - 350, area.y + 150).expect("could not position the test toolbar menu");
        t.line("recording toolbar tooltip waits for exclusion acknowledgment");
        300
    })
    .step(|t| {
        let popup = prepare_popup(|| super::hud::toolbar_menu_for_selftest(9003, true));
        super::hud::toolbar_menu_for_selftest(9003, false).expect("could not close the pending toolbar menu");
        assert_popup_closed(popup);
        t.popup = Some(popup);
        300
    })
    .step(|t| {
        let old = t.popup.take().expect("the pending toolbar menu was not recorded");
        let popup = prepare_popup(|| super::hud::toolbar_menu_for_selftest(9003, true));
        assert_ne!(popup.0, old.0, "a recreated toolbar menu reused a gate token");
        t.popup = Some(popup);
        platform::exclusion_command(platform::ProbeCommand::Flow).expect("could not update the toolbar menu filter");
        5000
    })
    .step(move |t| {
        let popup = t.popup.take().expect("the toolbar menu was not recorded");
        assert_popup_pixels(t, popup);
        platform::stall_window_close(popup.1 as isize).expect("could not observe the menu close notification");
        platform::exclusion_command(platform::ProbeCommand::WatchPopupClose(popup.1, t.popup_samples.clone()))
            .expect("could not watch the recording while closing the menu");
        t.popup = Some(popup);
        300
    })
    .step(|t| {
        assert!(matches!(platform::exclusion_reply(), Ok(platform::ProbeReply::Watching)), "the popup close probe did not start");
        let popup = t.popup.take().expect("the toolbar menu was not recorded");
        super::hud::toolbar_menu_for_selftest(9003, false).expect("could not close the toolbar menu");
        assert_popup_closed(popup);
        t.note = popup.1.to_string();
        1500
    })
    .step(|t| {
        platform::exclusion_command(platform::ProbeCommand::StopWatching).expect("could not finish the popup close probe");
        t.probe_deadline = Some(Instant::now() + Duration::from_secs(15));
        0
    })
    .step(move |t| {
        let platform::ProbeReply::CloseFrames { frames, leaked, first, registered } = platform::exclusion_reply().expect("the popup close probe did not finish")
            else { panic!("unexpected popup close probe reply") };
        assert!(platform::finish_window_close_stall().expect("the popup close observer is unavailable"), "the popup close notification was not observed");
        assert!(frames >= 10, "the popup close probe captured only {frames} frames");
        assert_eq!(leaked, 0, "the toolbar menu leaked into {leaked} of {frames} frames while closing (first {first:?}, registered {registered})");
        let number = t.note.parse::<u32>().expect("the closed toolbar menu number");
        assert!(jiffshot_capture::screen::is_excluded_window(number), "the closed popup was not retained while recording");
        assert!(!jiffshot_capture::screen::exclusion_ready(number), "the closed popup still has a live exclusion owner");
        set(k.hud, r#"{"panels":[],"borders":[]}"#.to_owned());
        t.line(format!("toolbar menu stayed excluded in {frames} frames while closing"));
        t.line("capture panel, outline, toolbar tooltip and native menu wait for exclusion acknowledgment");
        500
    })
    .step(|t| {
        assert!(t.new_windows().is_empty(), "the capture controls did not close");
        let editor = main_window().expect("the editor window is unavailable");
        assert_eq!(affinity(editor), 0, "the editor was incorrectly registered for exclusion");
        let area = t.probe_area.expect("the popup probe area is unavailable");
        let rect = window_rect(editor);
        platform::place_for_exclusion_probe(editor, area.x + (area.width - rect.width) / 2, area.y + (area.height - rect.height) / 2)
            .expect("could not position the test editor");
        t.remember_windows();
        let pending = super::hud::native_popup_macos::pending_for_selftest().expect("could not inspect popup registrations");
        editor_menu_for_selftest(true).expect("could not open a normal editor menu");
        let windows = t.new_windows();
        assert_eq!(windows.len(), 1, "the normal editor menu did not appear immediately");
        let number = u32::try_from(windows[0]).expect("the editor popup has no window number");
        assert_eq!(platform::exclusion_window_visible(number), Some(true), "the normal editor menu was gated");
        assert_eq!(affinity(windows[0]), 0, "the normal editor menu was incorrectly excluded");
        assert_eq!(super::hud::native_popup_macos::pending_for_selftest().expect("could not inspect popup registrations"), pending,
            "the normal editor menu registered with the capture popup gate");
        t.popup = Some((0, number));
        platform::exclusion_command(platform::ProbeCommand::Flow).expect("could not capture the normal editor menu");
        5000
    })
    .step(|t| {
        let (_, number) = t.popup.take().expect("the editor popup was not recorded");
        assert_editor_popup_pixels(t, number);
        editor_menu_for_selftest(false).expect("could not close the normal editor menu");
        platform::hide_exclusion_background().expect("could not uncover the editor");
        platform::front_for_exclusion_probe(main_window().expect("the editor window is unavailable"))
            .expect("could not bring the test editor forward");
        500
    })
    .step(|t| {
        let area = t.probe_area.expect("the popup probe area is unavailable");
        let editor = main_window().expect("the editor window is unavailable");
        assert_eq!(platform::window_at(area.x + area.width / 2, area.y + area.height / 4), Some(editor),
            "the editor is not under the recording probe");
        platform::exclusion_command(platform::ProbeCommand::Filtered).expect("could not capture the editor");
        1000
    })
    .step(|t| {
        let platform::ProbeReply::Pixel(pixel) = platform::exclusion_reply().expect("the editor recording probe did not finish")
            else { panic!("unexpected editor recording probe reply") };
        assert!(!(pixel[1] > 160 && u16::from(pixel[1]) > u16::from(pixel[0].max(pixel[2])) + 100),
            "the editor was missing from the recording: {pixel:?}");
        t.line("the normal editor window remains capturable");
        platform::finish_exclusion_probe().expect("could not stop the exclusion probe");
        500
    })
    .step(|t| {
        platform::join_exclusion_probe().expect("could not join the exclusion probe");
        let number = t.note.parse::<u32>().expect("the closed toolbar menu number");
        assert!(!jiffshot_capture::screen::is_excluded_window(number), "the retired popup remained excluded after recording");
        t.line("recording exclusion probe cleaned up");
        0
    });
    s
}

#[cfg(target_os = "macos")]
fn image_shutdown_script() -> Script {
    let mut script = Script(VecDeque::new());
    script.wait(800).step(|test| {
        super::scope().expect("the application scope is unavailable")
            .set_shutdown_mode(rustolonia::ShutdownMode::OnExplicitShutdown).expect("set explicit shutdown");
        let library = empty_scratch_library(test);
        if env("JIFFSHOT_UITEST_IMAGE_PUBLICATION_FAILURE").is_some() {
            deny_image_publication(library).expect("block publication of the synthetic capture");
        }
        let (ready, release) = jiffshot_workflow::start_image_shutdown_selftest()
            .expect("start the image shutdown fixture");
        *IMAGE_JOB_READY.lock().expect("image ready lock") = Some(ready);
        *IMAGE_JOB_RELEASE.lock().expect("image release lock") = Some(release);
        test.line("started an in-flight synthetic image capture");
        100
    });
    for attempt in 0..80 {
        script.step(move |test| {
            if !IMAGE_QUIT_REQUESTED.load(Ordering::Acquire) {
                let ready = IMAGE_JOB_READY.lock().expect("image ready lock");
                match ready.as_ref().expect("image ready receiver").try_recv() {
                    Ok(Ok(())) => {
                        platform::watch_application_termination().expect("observe AppKit termination");
                        IMAGE_QUIT_REQUESTED.store(true, Ordering::Release);
                        if env("JIFFSHOT_UITEST_IMAGE_EXIT_OVERLAP_NATIVE").is_some() {
                            crate::app::act(|app| app.exit()).expect("request explicit exit before native termination");
                            test.line("requested explicit exit before the native termination request");
                        }
                        test.line("requested AppKit termination while image publication was blocked");
                        test.flush();
                        FINAL_NATIVE_REQUESTED.store(true, Ordering::Release);
                        schedule_image_shutdown_probe(0).expect("schedule the image shutdown probe");
                        platform::request_application_termination().expect("request AppKit termination");
                    }
                    Ok(Err(error)) => panic!("the image shutdown fixture failed to start: {error}"),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => panic!("the image shutdown fixture disconnected"),
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                }
            } else if IMAGE_JOB_RELEASED.load(Ordering::Acquire)
                && env("JIFFSHOT_UITEST_IMAGE_PUBLICATION_FAILURE").is_some()
                && !IMAGE_FAILURE_VERIFIED.load(Ordering::Acquire)
                && matches!(crate::capture_engine_macos::result(), Ok(Some(Err(_))))
                && crate::termination_macos::pending_for_selftest().ok() == Some(None)
            {
                verify_image_publication_failure().expect("verify the capture publication failure");
                test.log = std::fs::read_to_string(test.path("ui-log.txt")).expect("read the image shutdown log");
                test.line("image publication failure cancelled native termination and was reported to the user");
                test.flush();
                if env("JIFFSHOT_UITEST_EXPECT_CANCEL_DURING_POST_RETRY").is_some() {
                    let log = std::fs::read_to_string(jiffshot_capture::paths::local_dir().join("capture.log"))
                        .expect("read shutdown dispatch diagnostics");
                    assert!(log.contains("could not post the native termination request; retrying: injected UI dispatcher failure"),
                        "the native request post did not fail");
                    assert!(!log.contains("queued the native termination request after retrying the UI dispatcher"),
                        "the native request callback was delivered before cancellation");
                    test.line("cancelled native termination while its request callback was still retrying");
                    test.flush();
                }
                if env("JIFFSHOT_UITEST_IMAGE_EXIT_OVERLAP_NATIVE").is_some() {
                    let token = crate::termination_macos::requests_for_selftest()
                        .first().expect("the original termination request").0;
                    crate::app::native_exit_requested(token);
                    assert_eq!(crate::termination_macos::pending_for_selftest().unwrap(), None,
                        "a stale termination callback must not create another request");
                    test.line("ignored a stale termination callback after cancellation");
                    test.flush();
                    if env("JIFFSHOT_UITEST_DELAY_CANCEL_STATE").is_some() {
                        crate::app::hold_cancelled_token_for_selftest(token).expect("hold the previous cancellation state");
                        let scope = super::scope().expect("the application scope is unavailable");
                        let delay = scope.delay(Duration::from_millis(300)).expect("schedule delayed cancellation state");
                        let poster = scope.clone();
                        scope.spawn(async move {
                            if let Err(error) = delay.await {
                                FAILED.store(true, Ordering::Release);
                                let message = format!("FAILED: could not delay cancellation state: {error}");
                                if let Err(log_error) = append_shutdown_log(&message) {
                                    eprintln!("ui self-test: {message}; could not write log: {log_error}");
                                }
                                return;
                            }
                            if let Err(error) = poster.post(move || crate::app::finish_cancelled_token_for_selftest(token)) {
                                FAILED.store(true, Ordering::Release);
                                let message = format!("FAILED: could not finish cancellation state: {error}");
                                if let Err(log_error) = append_shutdown_log(&message) {
                                    eprintln!("ui self-test: {message}; could not write log: {log_error}");
                                }
                            }
                        }).expect("start the delayed cancellation fixture");
                        test.line("delayed the previous cancellation state while requesting a new native quit");
                        test.flush();
                    }
                }
                IMAGE_FAILURE_VERIFIED.store(true, Ordering::Release);
                platform::request_application_termination().expect("retry AppKit termination");
            }
            if attempt == 79 {
                panic!("the image shutdown fixture did not reach its expected state");
            }
            100
        });
    }
    script
}

#[cfg(target_os = "macos")]
fn schedule_image_shutdown_probe(attempt: u32) -> std::result::Result<(), String> {
    let scope = super::scope().ok_or("the application scope is unavailable")?;
    let delay = scope.delay(Duration::from_millis(100)).map_err(|error| error.to_string())?;
    let poster = scope.clone();
    scope.spawn(async move {
        if let Err(error) = delay.await {
            release_image_shutdown_probe(Err(format!("could not wait for the image shutdown probe: {error}")));
            return;
        }
        if let Err(error) = poster.post(move || {
            let ready = (|| {
                let pending = crate::termination_macos::pending_for_selftest().map_err(|error| error.to_string())?;
                let stopping = jiffshot_workflow::image_shutdown_requested_for_selftest()?;
                if pending.is_none() || !stopping {
                    return Ok(false);
                }
                if !crate::capture_engine_macos::running()? || NATIVE_TERMINATION_OBSERVED.load(Ordering::Acquire) {
                    return Err("native termination did not wait for the in-flight capture".into());
                }
                Ok(true)
            })();
            match ready {
                Ok(true) => release_image_shutdown_probe(Ok(())),
                Ok(false) if attempt < 49 => {
                    if let Err(error) = schedule_image_shutdown_probe(attempt + 1) {
                        release_image_shutdown_probe(Err(error));
                    }
                }
                Ok(false) => release_image_shutdown_probe(Err("native termination did not begin capture shutdown".into())),
                Err(error) => release_image_shutdown_probe(Err(error)),
            }
        }) {
            release_image_shutdown_probe(Err(format!("could not post the image shutdown probe: {error}")));
        }
    }).map_err(|error| error.to_string())
}

#[cfg(target_os = "macos")]
fn release_image_shutdown_probe(result: std::result::Result<(), String>) {
    let line = match result {
        Ok(()) => "native termination waited for the in-flight capture while the UI remained responsive".to_owned(),
        Err(error) => {
            FAILED.store(true, Ordering::Release);
            format!("FAILED: {error}")
        }
    };
    if let Err(error) = append_shutdown_log(&line) {
        FAILED.store(true, Ordering::Release);
        eprintln!("ui self-test: {error}");
    }
    IMAGE_JOB_RELEASED.store(true, Ordering::Release);
    match IMAGE_JOB_RELEASE.lock() {
        Ok(mut release) => match release.take() {
            Some(sender) if sender.send(()).is_ok() => {}
            _ => {
                FAILED.store(true, Ordering::Release);
                eprintln!("ui self-test: could not release image publication");
            }
        },
        Err(_) => {
            FAILED.store(true, Ordering::Release);
            eprintln!("ui self-test: the image release lock is poisoned");
        }
    }
}

#[cfg(target_os = "macos")]
struct DeniedImagePublication {
    folder: PathBuf,
    permissions: Option<std::fs::Permissions>,
}

#[cfg(target_os = "macos")]
impl DeniedImagePublication {
    fn restore(&mut self) -> std::result::Result<(), String> {
        if let Some(permissions) = &self.permissions {
            std::fs::set_permissions(&self.folder, permissions.clone())
                .map_err(|error| format!("could not restore image library permissions: {error}"))?;
            self.permissions = None;
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl Drop for DeniedImagePublication {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("ui self-test: {error}");
        }
    }
}

#[cfg(target_os = "macos")]
fn deny_image_publication(folder: PathBuf) -> std::result::Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return Err("the image publication failure fixture requires a non-root account".into());
    }
    let permissions = std::fs::metadata(&folder).map_err(|error| error.to_string())?.permissions();
    let mut denied = permissions.clone();
    denied.set_mode(permissions.mode() & !0o222);
    std::fs::set_permissions(&folder, denied).map_err(|error| error.to_string())?;
    DENIED_IMAGE_PUBLICATION.with(|slot| {
        *slot.borrow_mut() = Some(DeniedImagePublication { folder, permissions: Some(permissions) });
    });
    Ok(())
}

#[cfg(target_os = "macos")]
fn verify_image_publication_failure() -> std::result::Result<(), String> {
    let error = crate::capture_engine_macos::result()?.ok_or("the capture engine is still running")?
        .err().ok_or("the capture engine did not report the publication error")?;
    if !error.contains("Could not save the capture") {
        return Err(format!("the capture save failure was not reported: {error}"));
    }
    let toast = sink().toast.get_untracked();
    if !toast.contains("Could not quit Jiffshot") || !toast.contains("Could not save the capture") {
        return Err(format!("the user-facing shutdown error omitted the capture failure: {toast}"));
    }
    let mut denied = DENIED_IMAGE_PUBLICATION.with(|slot| slot.borrow_mut().take())
        .ok_or("the image publication failure fixture is unavailable")?;
    denied.restore()
}

#[cfg(target_os = "macos")]
fn recording_shutdown_script() -> Script {
    let mut script = Script(VecDeque::new());
    script.wait(800);
    script.step(|test| {
        super::scope().expect("the application scope is unavailable")
            .set_shutdown_mode(rustolonia::ShutdownMode::OnExplicitShutdown).expect("could not set explicit shutdown");
        let library = empty_scratch_library(test);
        let folder = library.join("fixture-edits");
        std::fs::create_dir(&folder).expect("create the scratch capture folder");
        let image = folder.join("native-quit.png");
        jiffshot_doc::Pixmap::new(20, 15).expect("create a synthetic image")
            .save_png(&image).expect("save the synthetic image");
        crate::app::act(|app| {
            app.open_path(&image);
            app.resize_to(12, 9);
        }).expect("edit the synthetic capture");
        let area = platform::start_recording_shutdown_fixture().expect("create the synthetic recording fixture");
        let receiver = jiffshot_workflow::start_recording_shutdown_selftest(area)
            .expect("start the recording shutdown fixture");
        *RECORDING_QUIT_RESULT.lock().expect("recording result lock") = Some(receiver);
        test.line("started a synthetic recording before AppKit termination");
        1500
    });
    for attempt in 0..40 {
        script.step(move |test| {
            if RECORDING_QUIT_REQUESTED.load(Ordering::Acquire) {
                if env("JIFFSHOT_UITEST_RECORDING_PUBLICATION_FAILURE").is_some()
                    && !RECORDING_FAILURE_VERIFIED.load(Ordering::Acquire)
                {
                    if !matches!(crate::capture_engine_macos::result(), Ok(Some(Err(_))))
                        || crate::termination_macos::pending_for_selftest().ok() != Some(None)
                    {
                        if attempt == 39 {
                            panic!("native termination did not cancel after the recording publication failure");
                        }
                        return 250;
                    }
                    let line = verify_recording_publication_failure().expect("verify the recording recovery path");
                    test.line(line);
                    test.flush();
                    RECORDING_FAILURE_VERIFIED.store(true, Ordering::Release);
                    FINAL_NATIVE_REQUESTED.store(true, Ordering::Release);
                    platform::request_application_termination().expect("retry AppKit termination");
                }
                return 250;
            }
            let frames = jiffshot_workflow::recording_frames_for_selftest().expect("inspect the recording toolbar");
            if frames.is_some_and(|frames| frames >= 6) {
                RECORDING_QUIT_REQUESTED.store(true, Ordering::Release);
                test.line(format!("requested termination after {} recorded frames", frames.unwrap()));
                test.flush();
                if env("JIFFSHOT_UITEST_RECORDING_PUBLICATION_FAILURE").is_some() {
                    deny_recording_publication().expect("block publication of the staged recording");
                }
                if env("JIFFSHOT_UITEST_RECORDING_EXTERNAL_QUIT").is_some() {
                    request_external_quit().expect("request external termination");
                } else {
                    platform::watch_application_termination().expect("observe AppKit termination");
                    if env("JIFFSHOT_UITEST_RECORDING_PUBLICATION_FAILURE").is_none() {
                        FINAL_NATIVE_REQUESTED.store(true, Ordering::Release);
                    }
                    platform::request_application_termination().expect("request AppKit termination");
                }
            } else if attempt == 39 {
                panic!("the synthetic recording did not produce enough frames: {frames:?}");
            }
            250
        });
    }
    script
}

#[cfg(target_os = "macos")]
struct DeniedRecordingPublication {
    folder: PathBuf,
    permissions: Option<std::fs::Permissions>,
    staged: PathBuf,
}

#[cfg(target_os = "macos")]
impl DeniedRecordingPublication {
    fn restore(&mut self) -> std::result::Result<(), String> {
        if let Some(permissions) = &self.permissions {
            std::fs::set_permissions(&self.folder, permissions.clone())
                .map_err(|error| format!("could not restore recording folder permissions: {error}"))?;
            self.permissions = None;
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl Drop for DeniedRecordingPublication {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("ui self-test: {error}");
        }
    }
}

#[cfg(target_os = "macos")]
fn deny_recording_publication() -> std::result::Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return Err("the publication failure fixture requires a non-root account".into());
    }
    let root = PathBuf::from(std::env::var_os("JIFFSHOT_LIBRARY").ok_or("the scratch library is unavailable")?);
    let mut stages = Vec::new();
    for folder in std::fs::read_dir(&root).map_err(|error| error.to_string())? {
        let folder = folder.map_err(|error| error.to_string())?;
        if !folder.file_type().map_err(|error| error.to_string())?.is_dir() {
            continue;
        }
        let folder = folder.path();
        for entry in std::fs::read_dir(&folder).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            if entry.file_name().to_string_lossy().starts_with(".jiffshot-recording-")
                && entry.file_type().map_err(|error| error.to_string())?.is_dir()
            {
                stages.push((folder.clone(), entry.path().join("recording.mp4")));
            }
        }
    }
    let [(folder, staged)] = stages.as_slice() else {
        return Err(format!("expected one staged recording, found {}", stages.len()));
    };
    let permissions = std::fs::metadata(folder).map_err(|error| error.to_string())?.permissions();
    let mut denied = permissions.clone();
    denied.set_mode(permissions.mode() & !0o222);
    std::fs::set_permissions(folder, denied).map_err(|error| error.to_string())?;
    DENIED_RECORDING_PUBLICATION.with(|slot| {
        *slot.borrow_mut() = Some(DeniedRecordingPublication {
            folder: folder.clone(), permissions: Some(permissions), staged: staged.clone(),
        });
    });
    Ok(())
}

#[cfg(target_os = "macos")]
fn verify_recording_publication_failure() -> std::result::Result<String, String> {
    let mut denied = DENIED_RECORDING_PUBLICATION.with(|slot| slot.borrow_mut().take())
        .ok_or("the recording publication failure fixture is unavailable")?;
    let staged = denied.staged.clone();
    denied.restore()?;
    drop(denied);
    let receiver = RECORDING_QUIT_RESULT.lock().map_err(|_| "the recording result lock is poisoned".to_owned())?
        .take().ok_or("the recording result is unavailable")?;
    let error = receiver.try_recv().map_err(|error| format!("the recording did not finish: {error}"))?
        .err().ok_or("the recording unexpectedly published")?;
    let engine_error = crate::capture_engine_macos::result()?.ok_or("the capture engine is still running")?
        .err().ok_or("the capture engine did not report the publication error")?;
    let staged_name = staged.display().to_string();
    if !error.contains(&staged_name) || !engine_error.contains(&staged_name) {
        return Err(format!("the staged recovery path was not reported: {error}; {engine_error}"));
    }
    let toast = sink().toast.get_untracked();
    if !toast.contains(&staged_name) || !toast.contains("Could not quit Jiffshot") {
        return Err(format!("the user-facing shutdown error omitted the recovery path: {toast}"));
    }
    let edits = PathBuf::from(std::env::var_os("JIFFSHOT_LIBRARY").ok_or("the scratch library is unavailable")?)
        .join("fixture-edits/native-quit.jsd");
    if jiffshot_doc::io::load_document(&edits).map_err(|error| error.to_string())?.size() != (12, 9) {
        return Err("the edited synthetic capture was not saved before termination was cancelled".into());
    }
    let mut reader = jiffshot_capture::media::Reader::open(&staged)?;
    if reader.duration <= 0 || reader.next_video()?.is_none() {
        return Err("the preserved staged recording is not decodable".into());
    }
    Ok("recording publication failure cancelled native termination and reported a decodable recovery file".into())
}

#[cfg(target_os = "macos")]
fn request_external_quit() -> std::result::Result<(), String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let status = std::process::Command::new(executable).arg("--quit").status().map_err(|error| error.to_string())?;
    if status.success() { Ok(()) } else { Err(format!("external quit exited with {status}")) }
}

#[cfg(target_os = "macos")]
fn engine_quit_script() -> Script {
    let mut script = Script(VecDeque::new());
    script.wait(800);
    script.step(|_| {
        super::scope().expect("the application scope is unavailable")
            .set_shutdown_mode(rustolonia::ShutdownMode::OnExplicitShutdown).expect("set explicit shutdown");
        crate::app::act(|app| {
            app.new_image();
            app.resize_to(12, 9);
        }).expect("edit an untitled document");
        request_external_quit().expect("request external termination");
        500
    });
    script.step(|test| {
        assert!(crate::capture_engine_macos::running().expect("inspect the capture engine"),
            "external quit stopped the capture engine despite unsaved work");
        assert_eq!(crate::canvas::with_editor(|editor| editor.is_modified()), Some(true));
        test.line("external quit respected unsaved work and kept the capture engine running");
        let library = empty_scratch_library(test);
        let folder = library.join("2026-01");
        std::fs::create_dir(&folder).expect("create the scratch capture folder");
        let image = folder.join("native-quit.png");
        jiffshot_doc::Pixmap::new(20, 15).expect("create a synthetic image")
            .save_png(&image).expect("save the synthetic image");
        crate::app::act(|app| {
            app.open_path(&image);
            app.resize_to(12, 9);
        }).expect("edit the synthetic capture");
        test.line("requested capture-tray Exit");
        test.flush();
        jiffshot_workflow::TrayAction::Exit.post();
        0
    });
    script
}

#[cfg(target_os = "macos")]
fn tray_termination_script() -> Script {
    let mut script = Script(VecDeque::new());
    script.wait(800);
    script.step(|test| {
        let scope = super::scope().expect("the application scope is unavailable");
        scope.set_shutdown_mode(rustolonia::ShutdownMode::OnExplicitShutdown).expect("set explicit shutdown");
        let library = empty_scratch_library(test);
        let folder = library.join("2026-01");
        std::fs::create_dir(&folder).expect("create the scratch capture folder");
        let image = folder.join("native-quit.png");
        jiffshot_doc::Pixmap::new(20, 15).expect("create a synthetic image")
            .save_png(&image).expect("save the synthetic image");
        crate::app::act(|app| {
            app.open_path(&image);
            app.resize_to(12, 9);
        }).expect("edit the synthetic capture");
        for window in scope.windows() {
            if window.get_is_visible().expect("inspect a window") {
                window.hide().expect("hide a window");
            }
        }
        assert!(scope.windows().iter().all(|window| !window.get_is_visible().expect("inspect a window")),
            "a window remained visible");
        assert!(crate::capture_engine_macos::running().expect("inspect the capture engine"));
        platform::watch_application_termination().expect("observe AppKit termination");
        test.line("requested AppKit termination with no visible windows");
        test.flush();
        FINAL_NATIVE_REQUESTED.store(true, Ordering::Release);
        platform::request_application_termination().expect("request AppKit termination");
        0
    });
    script
}

#[cfg(target_os = "macos")]
fn panel_shutdown_script() -> Script {
    if env("JIFFSHOT_UITEST_PANEL_SHUTDOWN").as_deref() == Some("retry") {
        return panel_shutdown_retry_script();
    }
    let mut script = Script(VecDeque::new());
    script.wait(800).step(|test| {
        super::scope().expect("the application scope is unavailable")
            .set_shutdown_mode(rustolonia::ShutdownMode::OnExplicitShutdown).expect("set explicit shutdown");
        empty_scratch_library(test);
        let receiver = jiffshot_workflow::start_program_share_shutdown_selftest()
            .expect("start the automatic share on the capture engine");
        *PANEL_SHARE_RESULT.lock().expect("panel result lock") = Some(receiver);
        test.line("started an automatic share on the capture engine");
        test.flush();
        0
    });
    for attempt in 0..100 {
        script.step(move |test| {
            let count = jiffshot_capture::macos::active_file_panels_for_selftest().expect("inspect file panels");
            if count == 0 {
                assert!(attempt < 99, "the automatic share did not open its file panel");
                return 100;
            }
            assert_eq!(count, 1, "the automatic share opened unexpected file panels");
            test.line("requested quit with an automatic share panel open");
            test.flush();
            PANEL_QUIT_REQUESTED.store(true, Ordering::Release);
            if env("JIFFSHOT_UITEST_PANEL_SHUTDOWN").as_deref() == Some("explicit") {
                crate::app::act(|app| app.exit()).expect("request application exit");
            } else {
                platform::watch_application_termination().expect("observe AppKit termination");
                FINAL_NATIVE_REQUESTED.store(true, Ordering::Release);
                platform::request_application_termination().expect("request AppKit termination");
            }
            0
        });
    }
    script
}

#[cfg(target_os = "macos")]
fn check_panel_shutdown() -> std::result::Result<(), String> {
    if !matches!(crate::capture_engine_macos::result(), Ok(Some(Ok(())))) {
        return Err("the capture engine was not joined after closing the share panel".into());
    }
    check_panel_captures()
}

#[cfg(target_os = "macos")]
fn check_panel_captures() -> std::result::Result<(), String> {
    if jiffshot_capture::macos::active_file_panels_for_selftest().map_err(|error| error.to_string())? != 0 {
        return Err("a file panel remained open after the capture engine stopped".into());
    }
    let receiver = PANEL_SHARE_RESULT.lock().map_err(|_| "the panel result lock was poisoned")?
        .take().ok_or("the panel share result is unavailable")?;
    let messages = receiver.try_recv().map_err(|error| format!("the panel share did not finish: {error}"))??;
    if messages.len() != 2 || messages.iter().any(|message| {
        !message.contains("saved as") || message.contains("selected program") || message.contains("share failed")
    }) {
        return Err(format!("the two cancelled shares did not finish normally: {messages:?}"));
    }
    let library = PathBuf::from(std::env::var_os("JIFFSHOT_LIBRARY").ok_or("the scratch library is unavailable")?);
    let mut images = Vec::new();
    for folder in std::fs::read_dir(library).map_err(|error| error.to_string())? {
        let folder = folder.map_err(|error| error.to_string())?;
        if folder.file_type().map_err(|error| error.to_string())?.is_dir() {
            for entry in std::fs::read_dir(folder.path()).map_err(|error| error.to_string())? {
                let path = entry.map_err(|error| error.to_string())?.path();
                if path.extension().is_some_and(|extension| extension == "png")
                    && !path.file_name().unwrap_or_default().to_string_lossy().ends_with(".cursor.png")
                {
                    images.push(path);
                }
            }
        }
    }
    if images.len() != 2 {
        return Err(format!("expected two saved captures, found {}", images.len()));
    }
    for path in images {
        let image = image::open(&path).map_err(|error| error.to_string())?.to_rgba8();
        let (cursor, placement) = jiffshot_capture::library::cursor_paths(&path);
        let cursor = image::open(cursor).map_err(|error| error.to_string())?.to_rgba8();
        let placement: jiffshot_capture::library::CursorPlacement =
            serde_json::from_slice(&std::fs::read(placement).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        if image.dimensions() != (8, 8)
            || image.get_pixel(2, 3).0 != [60, 40, 20, 255]
            || cursor.dimensions() != (2, 2)
            || cursor.get_pixel(0, 0).0 != [200, 100, 50, 255]
            || (placement.x, placement.y) != (2.0, 3.0)
        {
            return Err(format!("the saved capture or its editable pointer was not preserved: {}", path.display()));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn panel_shutdown_retry_script() -> Script {
    let mut script = Script(VecDeque::new());
    script.wait(800).step(|test| {
        super::scope().expect("the application scope is unavailable")
            .set_shutdown_mode(rustolonia::ShutdownMode::OnExplicitShutdown).expect("set explicit shutdown");
        empty_scratch_library(test);
        let receiver = jiffshot_workflow::start_program_share_shutdown_selftest()
            .expect("start the automatic share on the capture engine");
        *PANEL_SHARE_RESULT.lock().expect("panel result lock") = Some(receiver);
        test.line("started an automatic share on the capture engine");
        0
    });
    let veto_requested = Rc::new(RefCell::new(false));
    for attempt in 0..50 {
        let veto_requested = veto_requested.clone();
        script.step(move |_| {
            if !*veto_requested.borrow() {
                let count = jiffshot_capture::macos::active_file_panels_for_selftest().expect("inspect file panels");
                if count == 0 {
                    assert!(attempt < 49, "the automatic share did not open its file panel");
                } else {
                    assert_eq!(count, 1, "the automatic share opened unexpected file panels");
                    crate::app::act(|app| {
                        app.new_image();
                        app.resize_to(12, 9);
                    }).expect("create synthetic unsaved edits");
                    *veto_requested.borrow_mut() = true;
                    platform::request_application_termination().expect("request AppKit termination with unsaved edits");
                }
            }
            100
        });
    }
    script.step(|test| {
        assert_eq!(crate::termination_macos::pending_for_selftest().unwrap(), None,
            "unsaved edits did not cancel native termination");
        assert!(crate::capture_engine_macos::running().expect("inspect the capture engine"),
            "unsaved edits stopped the capture engine");
        assert_eq!(jiffshot_capture::macos::active_file_panels_for_selftest().unwrap(), 1,
            "a rejected quit closed the pending share panel");
        assert_eq!(crate::canvas::with_editor(|editor| editor.is_modified()), Some(true));
        crate::canvas::with_editor(|editor| editor.mark_saved());
        test.line("unsaved edits cancelled quit without closing the share panel");
        0
    });
    for attempt in 0..50 {
        script.step(move |test| {
            if !PANEL_QUIT_REQUESTED.load(Ordering::Acquire) {
                let count = jiffshot_capture::macos::active_file_panels_for_selftest().expect("inspect file panels");
                if count == 0 {
                    assert!(attempt < 49, "the automatic share did not open its file panel");
                } else {
                    assert_eq!(count, 1, "the automatic share opened unexpected file panels");
                    test.line("requested quit with an automatic share panel open");
                    test.flush();
                    PANEL_QUIT_REQUESTED.store(true, Ordering::Release);
                    platform::request_application_termination().expect("request AppKit termination");
                }
            }
            100
        });
    }
    let reopened = Rc::new(RefCell::new(None::<std::sync::mpsc::Receiver<std::result::Result<Option<PathBuf>, String>>>));
    let started = reopened.clone();
    script.step(move |test| {
        assert!(matches!(crate::capture_engine_macos::result(),
            Ok(Some(Err(ref error))) if error.contains("injected file-panel shutdown failure")),
            "the injected capture-engine failure was not reported");
        assert_eq!(crate::termination_macos::pending_for_selftest().unwrap(), None,
            "the failed native quit was not cancelled");
        check_panel_captures().expect("preserve the two cancelled captures");
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new().name("jiffshot-panel-retry-fixture".into()).spawn(move || {
            let result = jiffshot_capture::os::choose_application_path("Verify the restored application picker", None)
                .map_err(|error| error.to_string());
            if sender.send(result).is_err() {
                eprintln!("ui self-test: the restored panel result was not received");
            }
        }).expect("start the restored application picker");
        *started.borrow_mut() = Some(receiver);
        test.line("failed quit reported the capture-engine error and cancelled native termination");
        0
    });
    let cancelled = Rc::new(RefCell::new(false));
    for attempt in 0..50 {
        let cancelled = cancelled.clone();
        script.step(move |_| {
            if !*cancelled.borrow() {
                let count = jiffshot_capture::macos::active_file_panels_for_selftest().expect("inspect restored panels");
                if count == 0 {
                    assert!(attempt < 49, "a file panel did not reopen after the failed quit");
                } else {
                    assert_eq!(count, 1, "the restored picker opened unexpected file panels");
                    jiffshot_capture::macos::suspend_file_panels_for_shutdown().expect("cancel the restored test picker");
                    jiffshot_capture::macos::resume_file_panels_after_failed_shutdown().expect("resume panels after the test cancellation");
                    *cancelled.borrow_mut() = true;
                }
            }
            100
        });
    }
    script.step(move |test| {
        assert!(*cancelled.borrow(), "the restored picker did not open");
        let result = reopened.borrow_mut().take().expect("the restored picker result is missing")
            .try_recv().expect("the restored picker did not finish").expect("the restored picker failed");
        assert_eq!(result, None, "the restored picker was not cancelled");
        test.line("a failed quit restored file panels and the next picker opened");
        test.flush();
        PANEL_RETRY_VERIFIED.store(true, Ordering::Release);
        crate::app::act(|app| app.exit()).expect("retry application exit");
        0
    });
    script
}

#[cfg(target_os = "macos")]
fn popup_shutdown_script(case: &str) -> Script {
    let (explicit, case) = case.strip_prefix("exit-").map_or((false, case), |case| (true, case));
    let (native, case) = case.strip_prefix("native-").map_or((false, case), |case| (true, case));
    let (with_engine, case) = case.strip_prefix("engine-").map_or((false, case), |case| (true, case));
    let (automatic, case) = case.strip_prefix("auto-").map_or((false, case), |case| (true, case));
    let (widget, pending, cancelled) = match case {
        "widget-pending" => (true, true, false),
        "widget-revealed" => (true, false, false),
        "toolbar-pending" => (false, true, false),
        "toolbar-revealed" => (false, false, false),
        "widget-cancelled-pending" => (true, true, true),
        "widget-cancelled-revealed" => (true, false, true),
        _ => panic!("unknown popup shutdown case: {case}"),
    };
    assert!((explicit as u8 + native as u8 + automatic as u8) <= 1, "a shutdown case has conflicting origins");
    assert!(!with_engine || native || explicit, "the capture engine fixture requires application termination");
    assert!(!explicit || with_engine, "the application exit fixture requires the capture engine");
    assert!(!((automatic || native || explicit) && cancelled), "cancelled-close cases require explicit shutdown");
    let mut s = Script(VecDeque::new());
    let k = sink();
    if native && env("JIFFSHOT_UITEST_EXIT_UNTITLED").is_some() {
        s.step(|_| {
            super::scope().expect("the application scope is unavailable")
                .set_shutdown_mode(rustolonia::ShutdownMode::OnExplicitShutdown).expect("set explicit shutdown");
            crate::app::act(|app| {
                app.new_image();
                app.resize_to(12, 9);
            }).expect("edit an untitled document");
            platform::request_application_termination().expect("request AppKit termination");
            500
        });
        s.step(|test| {
            assert!(crate::capture_engine_macos::running().expect("inspect the capture engine"));
            assert_eq!(crate::canvas::with_editor(|editor| editor.is_modified()), Some(true));
            assert_eq!(crate::termination_macos::pending_for_selftest().unwrap(), None);
            test.line("an untitled modified document cancelled native termination without stopping the capture engine");
            0
        });
    }
    s.step(move |test| {
        if automatic {
            super::scope().expect("the application scope is unavailable")
                .set_shutdown_mode(rustolonia::ShutdownMode::OnLastWindowClose).expect("could not set automatic shutdown");
        } else if native || explicit {
            super::scope().expect("the application scope is unavailable")
                .set_shutdown_mode(rustolonia::ShutdownMode::OnExplicitShutdown).expect("could not set explicit shutdown");
            let library = empty_scratch_library(test);
            if explicit && env("JIFFSHOT_UITEST_EXIT_UNTITLED").is_some() {
                crate::app::act(|app| {
                    app.new_image();
                    app.resize_to(12, 9);
                    app.exit();
                }).expect("could not request exit with an unsaved document");
                assert!(crate::capture_engine_macos::running().expect("could not inspect the capture engine"),
                    "an unsaved document did not prevent capture-engine shutdown");
                assert_eq!(crate::canvas::with_editor(|editor| editor.is_modified()), Some(true),
                    "the untitled document was marked saved");
                test.line("an untitled modified document cancelled exit without stopping the capture engine");
            }
            let folder = library.join("2026-01");
            std::fs::create_dir(&folder).expect("create the scratch capture folder");
            let image = folder.join("native-quit.png");
            jiffshot_doc::Pixmap::new(20, 15).expect("create a synthetic image")
                .save_png(&image).expect("save the synthetic image");
            crate::app::act(|app| {
                assert_eq!(app.library.root, library, "the app is not using the scratch library");
                app.open_path(&image);
                app.resize_to(12, 9);
            }).expect("edit the synthetic capture");
            assert_eq!(crate::canvas::with_editor(|editor| editor.is_modified()), Some(true));
            assert!(!image.with_extension("jsd").exists(), "the fixture was saved before termination");
        }
        if widget {
            set(k.widget, true);
        } else {
            let toolbar: serde_json::Value = serde_json::from_str(HUD_RECORDING).expect("recording toolbar fixture");
            set(k.hud, serde_json::json!({ "panels": [toolbar], "borders": [] }).to_string());
        }
        600
    });
    if (explicit || native) && env("JIFFSHOT_UITEST_EXIT_SAVE_FAILURE").is_some() {
        s.step(move |_| {
            let path = PathBuf::from(std::env::var_os("JIFFSHOT_LIBRARY").expect("scratch JIFFSHOT_LIBRARY"))
                .join("2026-01/native-quit.jsd");
            std::fs::create_dir(&path).expect("block the autosave destination");
            if native {
                platform::request_application_termination().expect("request AppKit termination");
            } else {
                crate::app::act(|app| app.exit()).expect("could not request application exit");
            }
            500
        });
        s.step(move |test| {
            assert!(crate::capture_engine_macos::running().expect("could not inspect the capture engine"),
                "autosave failure stopped the capture engine");
            assert_eq!(crate::canvas::with_editor(|editor| editor.is_modified()), Some(true),
                "autosave failure marked the document saved");
            let path = PathBuf::from(std::env::var_os("JIFFSHOT_LIBRARY").expect("scratch JIFFSHOT_LIBRARY"))
                .join("2026-01/native-quit.jsd");
            std::fs::remove_dir(&path).expect("unblock the autosave destination");
            if native {
                assert_eq!(crate::termination_macos::pending_for_selftest().unwrap(), None);
            }
            test.line("an autosave failure cancelled exit without stopping the capture engine");
            0
        });
    }
    if explicit && env("JIFFSHOT_UITEST_EXIT_PENDING_WORK").is_some() {
        s.step(|test| {
            crate::video::with_pending_export_for_selftest(|| {
                assert!(crate::video::pending_work(), "the pending export was not tracked");
                crate::app::act(|app| app.exit()).expect("could not request application exit");
                assert!(crate::capture_engine_macos::running().expect("could not inspect the capture engine"),
                    "a pending export did not prevent capture-engine shutdown");
            });
            assert!(!crate::video::pending_work(), "the pending export did not clear");
            test.line("an unfinished video export cancelled exit without stopping the capture engine");
            0
        });
    }
    if native && env("JIFFSHOT_UITEST_EXIT_PENDING_WORK").is_some() {
        s.step(|_| {
            TEST_PENDING_EXPORT.with(|slot| *slot.borrow_mut() = Some(crate::video::hold_pending_export_for_selftest()));
            platform::request_application_termination().expect("request AppKit termination");
            500
        });
        s.step(|test| {
            assert!(crate::video::pending_work(), "the pending export was not tracked");
            assert!(crate::capture_engine_macos::running().expect("inspect the capture engine"));
            assert_eq!(crate::termination_macos::pending_for_selftest().unwrap(), None);
            TEST_PENDING_EXPORT.with(|slot| { slot.borrow_mut().take(); });
            test.line("an unfinished video export cancelled native termination without stopping the capture engine");
            0
        });
    }
    if explicit {
        s.step(|_| {
            let (release, blocked) = std::sync::mpsc::channel();
            *EXIT_ENGINE_RELEASE.lock().expect("capture engine release lock") = Some(release);
            jiffshot_workflow::on_capture_thread(move || {
                EXIT_ENGINE_BLOCKED.store(true, Ordering::Release);
                if blocked.recv_timeout(Duration::from_secs(8)).is_err() {
                    eprintln!("ui self-test: the UI did not release the capture engine");
                }
            });
            300
        });
    }
    if widget {
        s.step(|_| {
            super::extras::expand_widget_for_selftest().expect("could not expand the Capture Widget");
            300
        });
    }
    s.step(move |t| {
        if with_engine {
            let log = std::fs::read_to_string(jiffshot_capture::paths::local_dir().join("capture.log"))
                .expect("capture engine log");
            assert!(log.contains("capture side started"), "the capture engine did not start");
            assert!(crate::capture_engine_macos::running().expect("could not inspect the capture engine"),
                "the capture engine is not running");
        }
        assert_eq!(super::hud::native_popup_macos::state_for_selftest(), (1, true), "the popup owner was not protected");
        let before = super::hud::native_popup_macos::pending_for_selftest().expect("could not inspect popup registrations");
        if widget {
            super::extras::widget_tooltip_for_selftest(true).expect("could not open the Capture Widget tooltip");
        } else {
            super::hud::toolbar_menu_for_selftest(9003, true).expect("could not open the toolbar menu");
        }
        let after = super::hud::native_popup_macos::pending_for_selftest().expect("could not inspect popup registrations");
        let added: Vec<_> = after.into_iter().filter(|entry| !before.contains(entry)).collect();
        assert_eq!(added.len(), 1, "opening a popup did not create exactly one gate token");
        let popup = added[0];
        assert_eq!(platform::exclusion_window_visible(popup.1), Some(false), "the popup was shown before acknowledgment");
        t.popup = Some(popup);
        t.gate_probe = Some(super::hud::native_popup_macos::drop_probe_for_selftest().expect("could not track the auxiliary gate"));
        if cancelled && pending {
            check_cancelled_popup(t, true);
        }
        if (automatic || native || explicit) && pending && !cancelled {
            request_shutdown_with_popup(t, widget, native, explicit);
        }
        if pending { 0 } else { 500 }
    });
    if !pending {
        s.step(move |t| {
            let popup = t.popup.expect("the popup was not recorded");
            assert_eq!(platform::exclusion_window_visible(popup.1), Some(true), "the popup was not revealed");
            if (automatic || native || explicit) && !cancelled {
                request_shutdown_with_popup(t, widget, native, explicit);
            }
            0
        });
    }
    if cancelled && !pending {
        s.step(|t| {
            check_cancelled_popup(t, false);
            0
        });
    }
    s
}

#[cfg(target_os = "macos")]
fn check_cancelled_popup(test: &mut Test, pending: bool) {
    let popup = test.popup.expect("the popup was not recorded");
    let scope = super::scope().expect("the application scope is unavailable");
    super::hud::native_popup_macos::retry_unloaded_owner_for_selftest(&scope)
        .expect("an unload failure did not prevent unsafe gate reuse");
    assert!(super::extras::try_close_widget_for_selftest().expect("could not try closing the Capture Widget"),
        "the Capture Widget did not cancel its close");
    assert_eq!(super::hud::native_popup_macos::state_for_selftest(), (1, true),
        "a failed clear or cancelled close removed the gate");
    assert!(super::hud::native_popup_macos::pending_for_selftest().expect("could not inspect popup registrations").contains(&popup),
        "a failed clear or cancelled close removed the popup token");
    assert_eq!(platform::exclusion_window_visible(popup.1), Some(!pending),
        "a failed clear or cancelled close changed the popup visibility");
    super::hud::native_popup_macos::cancel_shutdown_for_selftest(&scope)
        .expect("shutdown did not respect a cancelled protected owner close");
    assert!(!super::hud::native_popup_macos::stopping(), "a cancelled shutdown was not reset");
    assert_eq!(super::hud::native_popup_macos::state_for_selftest(), (1, true),
        "a cancelled shutdown removed the popup gate");
    assert!(super::hud::native_popup_macos::pending_for_selftest().expect("could not inspect popup registrations").contains(&popup),
        "a cancelled shutdown removed the popup token");

    super::extras::widget_tooltip_for_selftest(false).expect("could not close the old Capture Widget tooltip");
    assert_popup_closed(popup);
    super::hud::native_popup_macos::cancel_shutdown_for_selftest(&scope)
        .expect("shutdown cleared a visible owner after its popup closed");
    assert_eq!(super::hud::native_popup_macos::state_for_selftest(), (1, true),
        "a cancelled close removed protection from a visible owner");
    super::hud::native_popup_macos::reinstall_unloaded_owner_for_selftest(&scope)
        .expect("could not reinstall protection after the popup closed");
    assert!(test.gate_probe.take().expect("the old gate was not tracked").dropped(),
        "the old auxiliary gate survived replacement");
    assert_eq!(super::hud::native_popup_macos::state_for_selftest(), (1, true),
        "the reused owner was not protected");
    super::extras::widget_tooltip_for_selftest(true).expect("could not open a tooltip after reinstalling protection");
    let after = super::hud::native_popup_macos::pending_for_selftest().expect("could not inspect popup registrations");
    let [replacement] = after.as_slice() else { panic!("the reused owner did not create one popup token") };
    assert_eq!(platform::exclusion_window_visible(replacement.1), Some(false),
        "the reused owner's popup bypassed exclusion");
    test.popup = Some(*replacement);
    test.gate_probe = Some(super::hud::native_popup_macos::drop_probe_for_selftest().expect("could not track the replacement gate"));
    test.line("failed unload, cancelled shutdown and owner reuse preserved popup protection");
}

#[cfg(target_os = "macos")]
fn request_shutdown_with_popup(test: &mut Test, widget: bool, native: bool, explicit: bool) {
    if explicit {
        assert!(EXIT_ENGINE_BLOCKED.load(Ordering::Acquire), "the capture engine did not enter the test barrier");
        let release = EXIT_ENGINE_RELEASE.lock().expect("capture engine release lock").take()
            .expect("the capture engine release is unavailable");
        test.line("requested application exit while the capture engine was busy");
        test.flush();
        crate::app::act(|app| app.exit()).expect("could not request application exit");
        let scope = super::scope().expect("the application scope is unavailable");
        if env("JIFFSHOT_UITEST_EXIT_OVERLAP_NATIVE").is_some() {
            platform::watch_application_termination().expect("observe AppKit termination");
            let delay = scope.delay(Duration::from_millis(200)).expect("schedule the UI progress check");
            let poster = scope.clone();
            scope.spawn(async move {
                delay.await.expect("wait for the UI progress check");
                poster.post(move || {
                    if crate::termination_macos::pending_for_selftest().is_ok_and(|token| token.is_some())
                        && release.send(()).is_ok()
                    {
                        EXIT_UI_PROGRESS.store(true, Ordering::Release);
                    }
                }).expect("post the UI progress check");
            }).expect("start the UI progress check");
            FINAL_NATIVE_REQUESTED.store(true, Ordering::Release);
            platform::request_application_termination().expect("request AppKit termination");
        } else {
            scope.post(move || {
                if release.send(()).is_ok() {
                    EXIT_UI_PROGRESS.store(true, Ordering::Release);
                }
            }).expect("could not post the UI progress check");
        }
        return;
    }
    if native {
        test.line("requested AppKit termination with a protected popup");
        test.flush();
        platform::watch_application_termination().expect("observe AppKit termination");
        FINAL_NATIVE_REQUESTED.store(true, Ordering::Release);
        platform::request_application_termination().expect("request AppKit termination");
        return;
    }
    let scope = super::scope().expect("the application scope is unavailable");
    for window in scope.windows() {
        window.close().expect("could not close a mounted window");
    }
    if widget {
        super::extras::close_widget_for_selftest().expect("could not close the Capture Widget");
    } else {
        super::hud::close_toolbar_for_selftest(9003).expect("could not close the toolbar");
    }
    test.line("requested automatic shutdown with a protected popup");
}

/// The Capture window: each tab, a switch, then closed.
fn capture_window_selftest_action(action: String) {
    #[cfg(target_os = "macos")]
    if !crate::capture_engine_macos::running().expect("could not check the capture engine") {
        crate::capture_window::ui_only_selftest_action(&action);
        return;
    }
    super::capture().model.run(|model| model.run(action));
}

fn capture_window_steps(s: &mut Script) {
    let original_theme = Rc::new(RefCell::new(String::new()));
    s.step(|t| {
        t.remember_windows();
        if let Some(scope) = super::scope() {
            crate::capture_window::show(&scope);
        }
        400
    })
    .step(|t| {
        t.note = t.new_windows().first().map(|w| w.to_string()).unwrap_or_default();
        if t.note.is_empty() {
            t.line("capture window: not shown");
        }
        0
    });
    for tab in 0..3 {
        s.step(move |_| {
            capture_window_selftest_action(format!("tab:{tab}"));
            300
        })
        .step(move |t| {
            if let Ok(hwnd) = t.note.parse::<isize>() {
                t.shot_of(hwnd, &format!("ui-18-capture-tab{tab}.png"));
            }
            0
        });
    }
    let saved_theme = original_theme.clone();
    s.step(move |_| {
        *saved_theme.borrow_mut() = sink().theme.get_untracked();
        run("theme:Dark");
        300
    })
    .step(|t| {
        if let Ok(hwnd) = t.note.parse::<isize>() {
            t.shot_of(hwnd, "ui-18-capture-dark.png");
        }
        0
    })
    .step(|_| {
        run("theme:Light");
        300
    })
    .step(|t| {
        if let Ok(hwnd) = t.note.parse::<isize>() {
            t.shot_of(hwnd, "ui-18-capture-light.png");
        }
        0
    })
    .step(move |_| {
        run(format!("theme:{}", original_theme.borrow()));
        300
    });
    s.step(|_| {
        capture_window_selftest_action("tab:1".into());
        capture_window_selftest_action("switch:clipboard".into());
        300
    })
    .log(|_| format!("capture view: {}", super::capture().sink.view.get_untracked()))
    .step(|t| {
        capture_window_selftest_action("switch:clipboard".into());
        if let Ok(hwnd) = t.note.parse::<isize>() {
            close_window(hwnd);
        }
        200
    });
}

// ---- running ------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn awaiting_recording_shutdown() -> bool {
    env("JIFFSHOT_UITEST_RECORDING_SHUTDOWN").is_some()
        && RECORDING_QUIT_REQUESTED.load(Ordering::Acquire)
        && (env("JIFFSHOT_UITEST_RECORDING_PUBLICATION_FAILURE").is_none() || RECORDING_FAILURE_VERIFIED.load(Ordering::Acquire))
}

#[cfg(target_os = "macos")]
fn awaiting_image_shutdown() -> bool {
    env("JIFFSHOT_UITEST_IMAGE_SHUTDOWN").is_some()
        && IMAGE_JOB_RELEASED.load(Ordering::Acquire)
        && (env("JIFFSHOT_UITEST_IMAGE_PUBLICATION_FAILURE").is_none() || IMAGE_FAILURE_VERIFIED.load(Ordering::Acquire))
}

#[cfg(target_os = "macos")]
fn awaiting_panel_shutdown() -> bool {
    env("JIFFSHOT_UITEST_PANEL_SHUTDOWN").is_some_and(|case| {
        case != "retry" || PANEL_RETRY_VERIFIED.load(Ordering::Acquire)
    }) && PANEL_QUIT_REQUESTED.load(Ordering::Acquire)
}

fn next(runner: UiOnly<Rc<RefCell<Runner>>>) {
    #[cfg(target_os = "macos")]
    if awaiting_recording_shutdown() || awaiting_image_shutdown()
        || (awaiting_panel_shutdown() && !FAILED.load(Ordering::Acquire))
    {
        return;
    }
    #[cfg(target_os = "macos")]
    let probe_deadline = runner.get().borrow().test.probe_deadline;
    #[cfg(target_os = "macos")]
    if let Some(deadline) = probe_deadline {
        match platform::poll_exclusion_reply() {
            Ok(true) => runner.get().borrow_mut().test.probe_deadline = None,
            Ok(false) if Instant::now() < deadline => {
                schedule_next(runner, 50);
                return;
            }
            Ok(false) => {
                fail_runner(runner.get(), "the exclusion probe timed out".into());
                return;
            }
            Err(error) => {
                fail_runner(runner.get(), error);
                return;
            }
        }
    }
    let step = runner.get().borrow_mut().steps.pop_front();
    let Some(step) = step else {
        finish(runner.get());
        return;
    };
    let wait = {
        let mut r = runner.get().borrow_mut();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| step(&mut r.test)));
        let wait = match outcome {
            Ok(wait) => wait,
            Err(_) => {
                FAILED.store(true, Ordering::Release);
                r.test.line("FAILED: a step panicked");
                r.steps.clear();
                0
            }
        };
        #[cfg(target_os = "macos")]
        let flush = !FINAL_NATIVE_REQUESTED.load(Ordering::Acquire) || FAILED.load(Ordering::Acquire);
        #[cfg(not(target_os = "macos"))]
        let flush = true;
        if flush {
            r.test.flush();
        }
        wait
    };
    #[cfg(target_os = "macos")]
    if (awaiting_recording_shutdown() || awaiting_image_shutdown() || awaiting_panel_shutdown())
        && !FAILED.load(Ordering::Acquire)
    {
        return;
    }
    #[cfg(target_os = "macos")]
    if wait == 0 && env("JIFFSHOT_UITEST_RECORDING_SHUTDOWN").is_some() && runner.get().borrow().steps.is_empty() {
        if FAILED.load(Ordering::Acquire) {
            finish(runner.get());
        }
        return;
    }
    #[cfg(target_os = "macos")]
    if wait == 0 && env("JIFFSHOT_UITEST_IMAGE_SHUTDOWN").is_some() && runner.get().borrow().steps.is_empty() {
        if FAILED.load(Ordering::Acquire) {
            finish(runner.get());
        }
        return;
    }
    #[cfg(target_os = "macos")]
    if wait == 0 && env("JIFFSHOT_UITEST_TERMINATION_TRAY_ONLY").is_some() && runner.get().borrow().steps.is_empty() {
        if FAILED.load(Ordering::Acquire) {
            finish(runner.get());
        }
        return;
    }
    #[cfg(target_os = "macos")]
    if wait == 0 && env("JIFFSHOT_UITEST_ENGINE_QUIT").is_some() && runner.get().borrow().steps.is_empty() {
        if FAILED.load(Ordering::Acquire) {
            finish(runner.get());
        }
        return;
    }
    #[cfg(target_os = "macos")]
    if wait == 0 && env("JIFFSHOT_UITEST_POPUP_SHUTDOWN").is_some() && runner.get().borrow().steps.is_empty() {
        if FAILED.load(Ordering::Acquire) || !env("JIFFSHOT_UITEST_POPUP_SHUTDOWN")
            .is_some_and(|case| case.starts_with("auto-") || case.starts_with("native-") || case.starts_with("exit-"))
        {
            finish(runner.get());
        }
        return;
    }
    schedule_next(runner, wait);
}

fn schedule_next(runner: UiOnly<Rc<RefCell<Runner>>>, wait: u64) {
    let scope = runner.get().borrow().scope.clone();
    let delay = match scope.delay(Duration::from_millis(wait.max(1))) {
        Ok(delay) => delay,
        Err(error) => {
            fail_runner(runner.get(), format!("could not schedule the next step: {error:?}"));
            return;
        }
    };
    let poster = scope.clone();
    let task_runner = runner.clone();
    let spawned = scope.spawn(async move {
        if let Err(error) = delay.await {
            fail_runner(task_runner.get(), format!("could not wait for the next step: {error:?}"));
            return;
        }
        let next_runner = task_runner.clone();
        if let Err(error) = poster.post(move || next(next_runner)) {
            fail_runner(task_runner.get(), format!("could not post the next step: {error:?}"));
        }
    });
    if let Err(error) = spawned {
        fail_runner(runner.get(), format!("could not start the next step: {error:?}"));
    }
}

fn fail_runner(runner: &Rc<RefCell<Runner>>, message: String) {
    FAILED.store(true, Ordering::Release);
    eprintln!("ui self-test: {message}");
    {
        let mut r = runner.borrow_mut();
        r.test.line(format!("FAILED: {message}"));
        r.test.flush();
        r.steps.clear();
    }
    finish(runner);
}

fn finish(runner: &Rc<RefCell<Runner>>) {
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_LIBRARY_DRAG").is_some() {
        if let Err(error) = platform::stop_observing_native_file_drag() {
            FAILED.store(true, Ordering::Release);
            runner.borrow_mut().test.line(format!("FAILED: could not restore the native drag observer: {error}"));
        }
    }
    #[cfg(target_os = "macos")]
    if env("JIFFSHOT_UITEST_IMAGE_SHUTDOWN").is_some() {
        if let Some(release) = IMAGE_JOB_RELEASE.lock().expect("image release lock").take() {
            if release.send(()).is_err() {
                eprintln!("ui self-test: the image shutdown fixture had already stopped");
            }
        }
        if let Some(mut denied) = DENIED_IMAGE_PUBLICATION.with(|slot| slot.borrow_mut().take()) {
            if let Err(error) = denied.restore() {
                FAILED.store(true, Ordering::Release);
                runner.borrow_mut().test.line(format!("FAILED: {error}"));
            }
        }
    }
    let scope = {
        let r = runner.borrow();
        r.test.flush();
        r.scope.clone()
    };
    if env("JIFFSHOT_UITEST_KEEP").is_none() {
        #[cfg(target_os = "macos")]
        if let Err(error) = crate::capture_engine_macos::stop() {
            FAILED.store(true, Ordering::Release);
            eprintln!("ui self-test: {error}");
            let mut r = runner.borrow_mut();
            r.test.line(format!("FAILED: {error}"));
            r.test.flush();
        }
        // Hidden windows (Capture, the widget…) would keep it running.
        if let Err(error) = super::shutdown(&scope) {
            FAILED.store(true, Ordering::Release);
            eprintln!("ui self-test: could not shut down: {error:?}");
            if let Err(error) = scope.shutdown() {
                eprintln!("ui self-test: emergency shutdown failed: {error:?}");
            }
        }
        #[cfg(target_os = "macos")]
        if let Some(case) = env("JIFFSHOT_UITEST_POPUP_SHUTDOWN") {
            let mut r = runner.borrow_mut();
            let state = super::hud::native_popup_macos::state_for_selftest();
            let pending = super::hud::native_popup_macos::pending_for_selftest();
            let retired = r.test.popup.is_some_and(|(_, number)| !jiffshot_capture::screen::is_excluded_window(number));
            let released = r.test.gate_probe.as_ref().is_some_and(|probe| probe.dropped());
            if state != (0, false) || !matches!(pending, Ok(ref tokens) if tokens.is_empty()) || !retired || !released {
                FAILED.store(true, Ordering::Release);
                r.test.line(format!("FAILED: popup shutdown left state {state:?}, pending {pending:?}, retired {retired}, released {released}"));
            } else {
                r.test.line(format!("{case}: popup closed, exclusion retired and auxiliary gate released"));
            }
            r.test.flush();
        }
    }
}

#[cfg(target_os = "macos")]
pub(super) fn before_native_termination() -> std::result::Result<(), String> {
    if env("JIFFSHOT_UITEST_TERMINATION_OS_REASON").is_some() {
        let requests = crate::termination_macos::requests_for_selftest();
        if !requests.last().is_some_and(|(_, flags)| flags & 3 == 3) {
            return Err(format!("the self-directed logout-reason event was not classified as an OS termination: {requests:?}"));
        }
    }
    if env("JIFFSHOT_UITEST_PANEL_SHUTDOWN").is_some() {
        check_panel_shutdown()?;
        append_shutdown_log("pending and late share panels were cancelled after saving their captures")?;
    } else if env("JIFFSHOT_UITEST_IMAGE_SHUTDOWN").is_some() {
        if !IMAGE_QUIT_REQUESTED.load(Ordering::Acquire) || !IMAGE_JOB_RELEASED.load(Ordering::Acquire) {
            return Err("native termination reached acceptance before the image fixture was released".into());
        }
        if env("JIFFSHOT_UITEST_IMAGE_PUBLICATION_FAILURE").is_some() {
            if !IMAGE_FAILURE_VERIFIED.load(Ordering::Acquire)
                || !matches!(crate::capture_engine_macos::result(), Ok(Some(Err(_))))
            {
                return Err("the capture publication failure was not acknowledged before retrying".into());
            }
            let mut requests = crate::termination_macos::requests_for_selftest();
            requests.dedup_by_key(|request| request.0);
            if requests.len() != 2 || requests[0].0 == requests[1].0 {
                return Err(format!("the capture failure retry did not receive a fresh request token: {requests:?}"));
            }
            append_shutdown_log("native termination was retried after the image publication failure was reported")?;
        } else {
            check_image_shutdown()?;
            append_shutdown_log("AppKit termination saved the synthetic image and metadata before accepting termination")?;
        }
    } else if env("JIFFSHOT_UITEST_RECORDING_SHUTDOWN").is_some() {
        let result = if env("JIFFSHOT_UITEST_RECORDING_PUBLICATION_FAILURE").is_some() {
            if !RECORDING_FAILURE_VERIFIED.load(Ordering::Acquire) {
                Err("the recording publication failure was not acknowledged before retrying".into())
            } else if {
                let mut requests = crate::termination_macos::requests_for_selftest();
                requests.dedup_by_key(|request| request.0);
                requests.len() != 2 || requests[0].0 == requests[1].0
            } {
                Err("the retried termination did not receive a fresh request token".into())
            } else {
                Ok("native termination was retried after the recording recovery path was reported".into())
            }
        } else {
            check_recording_shutdown()
        };
        match result {
            Ok(line) => {
                append_shutdown_log(&line)?;
                platform::close_recording_shutdown_fixture();
            }
            Err(error) => {
                FAILED.store(true, Ordering::Release);
                append_shutdown_log(&format!("FAILED: recording shutdown: {error}"))?;
                return Err(error);
            }
        }
    } else if env("JIFFSHOT_UITEST_TERMINATION_TRAY_ONLY").is_some() {
        let scope = super::scope().ok_or("the application scope is unavailable")?;
        if scope.windows().iter().any(|window| window.get_is_visible().unwrap_or(true)) {
            return Err("a window became visible before tray-only termination".into());
        }
        if !matches!(crate::capture_engine_macos::result(), Ok(Some(Ok(())))) {
            return Err("the capture engine was not joined before tray-only termination".into());
        }
        let edits = PathBuf::from(std::env::var_os("JIFFSHOT_LIBRARY").ok_or("the scratch library is unavailable")?)
            .join("2026-01/native-quit.jsd");
        if jiffshot_doc::io::load_document(&edits).map_err(|error| error.to_string())?.size() != (12, 9) {
            return Err("the edited synthetic capture was not saved before tray-only termination".into());
        }
        append_shutdown_log("tray-only termination saved the document and joined the capture engine before acceptance")?;
    } else if let Some(case) = env("JIFFSHOT_UITEST_POPUP_SHUTDOWN")
        .filter(|case| case.starts_with("native-") || env("JIFFSHOT_UITEST_EXIT_OVERLAP_NATIVE").is_some())
    {
        let state = super::hud::native_popup_macos::state_for_selftest();
        let pending = super::hud::native_popup_macos::pending_for_selftest().map_err(|error| error.to_string())?;
        if state != (1, true) || pending.len() != 1 {
            return Err(format!("the native request closed its protected popup before cleanup: {state:?}, {pending:?}"));
        }
        let edits = PathBuf::from(std::env::var_os("JIFFSHOT_LIBRARY").ok_or("the scratch library is unavailable")?)
            .join("2026-01/native-quit.jsd");
        let saved = jiffshot_doc::io::load_document(&edits).map_err(|error| error.to_string())?;
        if saved.size() != (12, 9) {
            return Err("the edited synthetic capture was not saved before native termination".into());
        }
        if (case.starts_with("native-engine-") || case.starts_with("exit-engine-"))
            && !matches!(crate::capture_engine_macos::result(), Ok(Some(Ok(()))))
        {
            return Err("the capture engine was not joined before native termination".into());
        }
        let mut requests = crate::termination_macos::requests_for_selftest();
        requests.dedup_by_key(|request| request.0);
        let cancelled = ["JIFFSHOT_UITEST_EXIT_UNTITLED", "JIFFSHOT_UITEST_EXIT_SAVE_FAILURE", "JIFFSHOT_UITEST_EXIT_PENDING_WORK"]
            .iter().filter(|name| case.starts_with("native-") && env(name).is_some()).count();
        if requests.len() < cancelled + 1
            || requests.iter().map(|request| request.0).collect::<std::collections::HashSet<_>>().len() != requests.len()
        {
            return Err(format!("native cancellation/retry did not use fresh termination tokens: {requests:?}"));
        }
        append_shutdown_log("native termination cleanup finished before accepting the request")?;
    } else {
        return Ok(());
    }
    NATIVE_CLEANUP_VERIFIED.store(true, Ordering::Release);
    Ok(())
}

#[cfg(target_os = "macos")]
fn append_shutdown_log(line: &str) -> std::result::Result<(), String> {
    use std::io::Write as _;
    let dir = env("JIFFSHOT_UITEST").ok_or("the UI self-test directory is unavailable")?;
    std::fs::OpenOptions::new().append(true).open(Path::new(&dir).join("ui-log.txt"))
        .and_then(|mut file| writeln!(file, "{line}"))
        .map_err(|error| format!("could not write the UI shutdown result: {error}"))
}

#[cfg(target_os = "macos")]
pub(super) fn native_will_terminate() {
    NATIVE_TERMINATION_OBSERVED.store(true, Ordering::Release);
    if !NATIVE_CLEANUP_VERIFIED.load(Ordering::Acquire) {
        FAILED.store(true, Ordering::Release);
        if let Err(error) = append_shutdown_log("FAILED: AppKit terminated before cleanup was verified") {
            eprintln!("ui self-test: {error}");
        }
        return;
    }
    if env("JIFFSHOT_UITEST_IMAGE_SHUTDOWN").is_none()
        && env("JIFFSHOT_UITEST_RECORDING_SHUTDOWN").is_none()
        && env("JIFFSHOT_UITEST_TERMINATION_TRAY_ONLY").is_none()
    {
        report_popup_shutdown();
    }
    if let Err(error) = append_shutdown_log("AppKit WillTerminate observed after cleanup") {
        FAILED.store(true, Ordering::Release);
        eprintln!("ui self-test: {error}");
    }
}

#[cfg(target_os = "macos")]
fn check_image_shutdown() -> std::result::Result<(), String> {
    if !matches!(crate::capture_engine_macos::result(), Ok(Some(Ok(())))) {
        return Err("the capture engine did not finish before native termination".into());
    }
    let library = PathBuf::from(std::env::var_os("JIFFSHOT_LIBRARY").ok_or("the scratch library is unavailable")?);
    let mut images = Vec::new();
    for folder in std::fs::read_dir(library).map_err(|error| error.to_string())? {
        let folder = folder.map_err(|error| error.to_string())?;
        if !folder.file_type().map_err(|error| error.to_string())?.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(folder.path()).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            if entry.path().extension().is_some_and(|extension| extension == "png") {
                images.push(entry.path());
            }
        }
    }
    let [path] = images.as_slice() else {
        return Err(format!("expected one saved image, found {}", images.len()));
    };
    let image = jiffshot_doc::io::load_image(path).map_err(|error| error.to_string())?;
    if (image.width(), image.height()) != (16, 12)
        || image.data().chunks_exact(4).any(|pixel| pixel != [207, 38, 14, 255])
    {
        return Err("the saved synthetic capture has unexpected dimensions or pixels".into());
    }
    let meta = jiffshot_capture::library::read_meta_checked(path).map_err(|error| error.to_string())?
        .ok_or("the saved synthetic capture has no metadata")?;
    if meta.kind != jiffshot_capture::library::CaptureKind::Region || (meta.width, meta.height) != (16, 12) {
        return Err("the saved synthetic capture has unexpected metadata".into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn check_recording_shutdown() -> std::result::Result<String, String> {
    (|| {
        if !RECORDING_QUIT_REQUESTED.load(Ordering::Acquire) {
            return Err("AppKit termination was not requested during recording".into());
        }
        if !matches!(crate::capture_engine_macos::result(), Ok(Some(Ok(())))) {
            return Err("the capture engine did not stop before native termination".into());
        }
        let receiver = RECORDING_QUIT_RESULT.lock().map_err(|_| "the recording result lock is poisoned".to_owned())?
            .take().ok_or("the recording result is unavailable")?;
        let path = receiver.try_recv().map_err(|error| format!("the recording did not finish: {error}"))??
            .ok_or("the recording was discarded during AppKit termination")?;
        let edits = PathBuf::from(std::env::var_os("JIFFSHOT_LIBRARY").ok_or("the scratch library is unavailable")?)
            .join("fixture-edits/native-quit.jsd");
        let saved = jiffshot_doc::io::load_document(&edits).map_err(|error| error.to_string())?;
        if saved.size() != (12, 9) {
            return Err(format!("the synthetic capture was saved at the wrong size: {:?}", saved.size()));
        }
        let mut reader = jiffshot_capture::media::Reader::open(&path)?;
        if reader.duration <= 0 {
            return Err("the recording has no duration".into());
        }
        let mut frames = 0;
        let mut saw_red = false;
        while let Some(frame) = reader.next_video()? {
            frames += 1;
            let at = ((frame.height / 2 * frame.width + frame.width / 2) * 4) as usize;
            let pixel = frame.bgra.get(at..at + 3).ok_or("the recording frame is too small")?;
            saw_red |= u16::from(pixel[2]) > u16::from(pixel[0]) + 80
                && u16::from(pixel[2]) > u16::from(pixel[1]) + 80;
        }
        if frames < 2 || !saw_red {
            return Err("the saved recording did not contain the synthetic red fixture".into());
        }
        Ok(format!("AppKit termination saved the edited capture and finalized {frames} synthetic recording frames before accepting termination"))
    })()
}

#[cfg(target_os = "macos")]
pub(super) fn after_run() {
    if NATIVE_TERMINATION_OBSERVED.load(Ordering::Acquire) {
        return;
    }
    if let Some(case) = env("JIFFSHOT_UITEST_PANEL_SHUTDOWN") {
        let result = if case == "retry" && PANEL_RETRY_VERIFIED.load(Ordering::Acquire) {
            Ok(())
        } else if case == "explicit" {
            check_panel_shutdown()
        } else {
            Err("AppKit termination was not observed after file-panel cleanup".into())
        };
        let line = match result {
            Ok(()) => "pending and late share panels were cancelled after saving their captures".to_owned(),
            Err(error) => {
                FAILED.store(true, Ordering::Release);
                format!("FAILED: file-panel shutdown: {error}")
            }
        };
        if let Err(error) = append_shutdown_log(&line) {
            FAILED.store(true, Ordering::Release);
            eprintln!("ui self-test: {error}");
        }
        return;
    }
    if env("JIFFSHOT_UITEST_IMAGE_SHUTDOWN").is_some() {
        FAILED.store(true, Ordering::Release);
        if let Err(error) = append_shutdown_log("FAILED: AppKit termination was not observed after image cleanup") {
            eprintln!("ui self-test: {error}");
        }
        return;
    }
    if env("JIFFSHOT_UITEST_RECORDING_SHUTDOWN").is_some() {
        if env("JIFFSHOT_UITEST_RECORDING_EXTERNAL_QUIT").is_some() {
            let result = check_recording_shutdown();
            platform::close_recording_shutdown_fixture();
            let line = match result {
                Ok(line) => line.replace("AppKit termination", "External quit").replace("before accepting termination", "before the UI loop exited"),
                Err(error) => {
                    FAILED.store(true, Ordering::Release);
                    format!("FAILED: recording shutdown: {error}")
                }
            };
            if let Err(error) = append_shutdown_log(&line) {
                FAILED.store(true, Ordering::Release);
                eprintln!("ui self-test: {error}");
            }
            return;
        }
        FAILED.store(true, Ordering::Release);
        if let Err(error) = append_shutdown_log("FAILED: AppKit termination was not observed after recording cleanup") {
            eprintln!("ui self-test: {error}");
        }
        return;
    }
    if env("JIFFSHOT_UITEST_ENGINE_QUIT").is_some() {
        let result = (|| -> std::result::Result<(), String> {
            if !matches!(crate::capture_engine_macos::result(), Ok(Some(Ok(())))) {
                return Err("capture-tray Exit did not join the capture engine".into());
            }
            let path = PathBuf::from(std::env::var_os("JIFFSHOT_LIBRARY").ok_or("the scratch library is unavailable")?)
                .join("2026-01/native-quit.jsd");
            if jiffshot_doc::io::load_document(&path).map_err(|error| error.to_string())?.size() != (12, 9) {
                return Err("capture-tray Exit did not save the edited document".into());
            }
            Ok(())
        })();
        let line = match result {
            Ok(()) => "capture-tray Exit saved the document and joined the capture engine".to_owned(),
            Err(error) => {
                FAILED.store(true, Ordering::Release);
                format!("FAILED: {error}")
            }
        };
        if let Err(error) = append_shutdown_log(&line) {
            FAILED.store(true, Ordering::Release);
            eprintln!("ui self-test: {error}");
        }
        return;
    }
    report_popup_shutdown();
}

#[cfg(target_os = "macos")]
fn report_popup_shutdown() {
    use std::io::Write as _;
    let Some(case) = env("JIFFSHOT_UITEST_POPUP_SHUTDOWN")
        .filter(|case| case.starts_with("auto-") || case.starts_with("native-") || case.starts_with("exit-"))
    else { return };
    let Some(dir) = env("JIFFSHOT_UITEST") else { return };
    let state = super::hud::native_popup_macos::state_for_selftest();
    let pending = super::hud::native_popup_macos::pending_for_selftest();
    let released = super::hud::native_popup_macos::test_gate_dropped();
    let mut line = if state == (0, false) && matches!(pending, Ok(ref tokens) if tokens.is_empty()) && released {
        format!("{case}: application shutdown closed the popup and released the auxiliary gate")
    } else {
        FAILED.store(true, Ordering::Release);
        format!("FAILED: automatic shutdown left state {state:?}, pending {pending:?}, released {released}")
    };
    if case.starts_with("native-") || case.starts_with("exit-") {
        let edits = std::env::var_os("JIFFSHOT_LIBRARY").map(PathBuf::from)
            .map(|library| library.join("2026-01/native-quit.jsd"));
        let saved = edits.as_deref().ok_or_else(|| "the scratch library is unavailable".to_owned())
            .and_then(jiffshot_doc::io::load_document)
            .map(|document| document.size());
        if saved == Ok((12, 9)) {
            line.push_str("\napplication termination autosaved the synthetic capture");
        } else {
            FAILED.store(true, Ordering::Release);
            line.push_str(&format!("\nFAILED: application termination did not autosave the synthetic capture: {saved:?}"));
        }
    }
    if case.starts_with("native-engine-") || case.starts_with("exit-engine-") {
        match crate::capture_engine_macos::result() {
            Ok(Some(Ok(()))) => line.push_str("\napplication shutdown joined the capture engine"),
            result => {
                FAILED.store(true, Ordering::Release);
                line.push_str(&format!("\nFAILED: application shutdown did not join the capture engine: {result:?}"));
            }
        }
        if case.starts_with("exit-engine-") {
            if EXIT_UI_PROGRESS.load(Ordering::Acquire) {
                line.push_str("\nthe UI remained responsive while the capture engine stopped");
            } else {
                FAILED.store(true, Ordering::Release);
                line.push_str("\nFAILED: the UI did not remain responsive while the capture engine stopped");
            }
        }
    }
    match std::fs::OpenOptions::new().append(true).open(Path::new(&dir).join("ui-log.txt"))
        .and_then(|mut file| writeln!(file, "{line}"))
    {
        Ok(()) => {}
        Err(error) => {
            FAILED.store(true, Ordering::Release);
            eprintln!("ui self-test: could not record automatic shutdown: {error}");
        }
    }
}

/// Starts the walk, writing into `dir`. On the UI thread, once the editor
/// window is mounted.
pub fn start(scope: &AppScope, dir: &Path) {
    if let Err(error) = std::fs::create_dir_all(dir) {
        FAILED.store(true, Ordering::Release);
        eprintln!("ui self-test: could not create {}: {error}", dir.display());
        if env("JIFFSHOT_UITEST_KEEP").is_none() {
            if let Err(error) = super::shutdown(scope) {
                eprintln!("ui self-test: could not shut down: {error:?}");
            }
        }
        return;
    }
    let mut test = Test {
        dir: dir.to_path_buf(), log: String::new(), w: 0.0, h: 0.0, note: String::new(), windows_before: Vec::new(),
        #[cfg(target_os = "macos")]
        popup: None,
        #[cfg(target_os = "macos")]
        probe_area: None,
        #[cfg(target_os = "macos")]
        popup_samples: Vec::new(),
        #[cfg(target_os = "macos")]
        probe_deadline: None,
        #[cfg(target_os = "macos")]
        gate_probe: None,
    };
    test.line("started");
    let runner = Rc::new(RefCell::new(Runner { scope: scope.clone(), test, steps: script().0 }));
    next(UiOnly::new(runner));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_edits_require_an_existing_subdirectory_of_test_output() {
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("system clock before epoch").as_nanos();
        let root = std::env::temp_dir().join(format!("jiffshot-ui-test-{}-{unique}", std::process::id()));
        let output = root.join("output");
        let library = output.join("library");
        let other = root.join("other");
        std::fs::create_dir_all(&library).expect("create isolated library");
        std::fs::create_dir(&other).expect("create unrelated library");
        assert!(isolated_library(&library, &output));
        assert!(!isolated_library(&other, &output));
        assert!(!isolated_library(&output, &output));
        assert!(!isolated_library(&output.join("missing"), &output));
        std::fs::remove_dir(&library).expect("remove isolated library");
        std::fs::remove_dir(&output).expect("remove test output");
        std::fs::remove_dir(&other).expect("remove unrelated library");
        std::fs::remove_dir(&root).expect("remove test root");
    }
}
