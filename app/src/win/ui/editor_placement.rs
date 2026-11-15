//! Keeps the Windows editor inside one monitor's work area and remembers its placement.

use jiffshot_capture::{config, screen, Rect};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowPlacement, GetWindowRect, IsIconic, IsWindowVisible, IsZoomed,
    PostMessageW, SetWindowPlacement, SetWindowPos, ShowWindow, MINMAXINFO, SWP_NOACTIVATE,
    SWP_NOZORDER, SW_MAXIMIZE, SW_SHOWMAXIMIZED, WINDOWPLACEMENT, WM_APP, WM_DISPLAYCHANGE,
    WM_DPICHANGED, WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE, WM_GETMINMAXINFO, WM_MOVE, WM_NCDESTROY,
    WM_SETTINGCHANGE, WM_SHOWWINDOW, WM_SIZE, WS_OVERLAPPEDWINDOW,
};

const SECTION: &str = "editor_window";
const SUBCLASS_ID: usize = 0x4A53_4550;
const RECONCILE: u32 = WM_APP + 0x455;
/// Maximizes an already visible editor after the current message, never from inside
/// Avalonia's `Loaded` callback, which runs before Avalonia has shown the window.
const MAXIMIZE: u32 = WM_APP + 0x456;
const INITIAL_WORK_AREA_FRACTION: f64 = 0.9;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Bounds {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl Bounds {
    fn valid(self) -> bool {
        self.width > 0
            && self.height > 0
            && self.x.checked_add(self.width).is_some()
            && self.y.checked_add(self.height).is_some()
    }

    fn fit(self, work: Rect) -> Self {
        let width = self.width.clamp(1, work.width);
        let height = self.height.clamp(1, work.height);
        Self {
            x: self.x.clamp(work.x, work.right() - width),
            y: self.y.clamp(work.y, work.bottom() - height),
            width,
            height,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Placement {
    normal: Bounds,
    dpi: u32,
    maximized: bool,
}

#[derive(Clone, Copy)]
struct State {
    placement: Placement,
    last_saved: Option<Placement>,
    applying: bool,
    queued: bool,
    moving: bool,
}

thread_local! {
    static WINDOWS: RefCell<HashMap<isize, State>> = RefCell::new(HashMap::new());
}

fn initial(work: Rect, dpi: u32) -> Bounds {
    let scale = f64::from(dpi) / 96.0;
    let width = (1500.0 * scale)
        .min(f64::from(work.width) * INITIAL_WORK_AREA_FRACTION)
        .round() as i32;
    let height = (960.0 * scale)
        .min(f64::from(work.height) * INITIAL_WORK_AREA_FRACTION)
        .round() as i32;
    Bounds {
        x: work.x + (work.width - width) / 2,
        y: work.y + (work.height - height) / 2,
        width,
        height,
    }
    .fit(work)
}

fn restored(saved: Placement, work: Rect, dpi: u32) -> Bounds {
    let scale = f64::from(dpi) / f64::from(saved.dpi);
    Bounds {
        width: (f64::from(saved.normal.width) * scale)
            .round()
            .clamp(1.0, f64::from(work.width)) as i32,
        height: (f64::from(saved.normal.height) * scale)
            .round()
            .clamp(1.0, f64::from(work.height)) as i32,
        ..saved.normal
    }
    .fit(work)
}

fn monitor_for(bounds: Bounds, monitors: &[screen::Monitor]) -> Option<&screen::Monitor> {
    // Prefer the greatest overlap, not the combined virtual desktop or the window's top-left.
    monitors
        .iter()
        .filter(|monitor| !monitor.work.is_empty() && !monitor.rect.is_empty() && monitor.dpi > 0)
        .max_by_key(|monitor| {
            let work = monitor.rect;
            let right = i64::from(bounds.x) + i64::from(bounds.width);
            let bottom = i64::from(bounds.y) + i64::from(bounds.height);
            let overlap_x =
                (right.min(i64::from(work.right())) - i64::from(bounds.x.max(work.x))).max(0);
            let overlap_y =
                (bottom.min(i64::from(work.bottom())) - i64::from(bounds.y.max(work.y))).max(0);
            let dx = (i64::from(work.x) - right)
                .max(i64::from(bounds.x) - i64::from(work.right()))
                .max(0);
            let dy = (i64::from(work.y) - bottom)
                .max(i64::from(bounds.y) - i64::from(work.bottom()))
                .max(0);
            (
                overlap_x * overlap_y,
                -(i128::from(dx).pow(2) + i128::from(dy).pow(2)),
            )
        })
}

fn window_bounds(hwnd: HWND) -> io::Result<Bounds> {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: hwnd is a live window; rect is writable for the duration of the call.
    if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let bounds = Bounds {
        x: rect.left,
        y: rect.top,
        width: rect.right - rect.left,
        height: rect.bottom - rect.top,
    };
    if !bounds.valid() {
        return Err(io::Error::other("the editor window has invalid bounds"));
    }
    Ok(bounds)
}

fn load() -> Option<Placement> {
    let value = config::section(SECTION);
    if value.is_null() {
        return None;
    }
    match serde_json::from_value::<Placement>(value) {
        Ok(saved) if saved.normal.valid() && saved.dpi > 0 => Some(saved),
        Ok(_) => {
            eprintln!("Editor placement: saved bounds or DPI are invalid; using a fresh placement");
            None
        }
        Err(error) => {
            eprintln!("Editor placement: could not read saved placement: {error}");
            None
        }
    }
}

fn persist(placement: Placement) -> io::Result<()> {
    let value = serde_json::to_value(placement).map_err(io::Error::other)?;
    config::try_set_section(SECTION, value)
}

/// Where and how Avalonia should first show the editor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Startup {
    /// The restored frame's top-left in physical pixels, so the window opens
    /// (and maximizes) on the monitor it was saved on.
    pub x: i32,
    pub y: i32,
    /// The restored client size in device-independent pixels (Avalonia's
    /// `Width` and `Height`) at the target monitor's DPI.
    pub width: f64,
    pub height: f64,
    pub maximized: bool,
}

/// The saved placement to apply before the editor is first shown, if any.
///
/// `Loaded` (where `install` runs) is raised before Avalonia shows the window,
/// and the tray-started editor is not shown until it is opened. Avalonia's
/// `Show` then applies its own `Width`, `Height` and `WindowState`, so placement
/// made natively in `Loaded` is lost (the size) or breaks rendering
/// (`ShowWindow(SW_MAXIMIZE)` made the window visible without Avalonia ever
/// rendering into it). Giving Avalonia the saved values before `Show` lets it
/// show the window where, how large and how maximized it was saved.
pub(super) fn startup() -> Option<Startup> {
    let saved = load()?;
    let monitors = screen::monitors();
    let monitor = monitor_for(saved.normal, &monitors)?;
    Some(startup_for(saved, monitor.work, monitor.dpi))
}

fn startup_for(saved: Placement, work: Rect, dpi: u32) -> Startup {
    let normal = restored(saved, work, dpi);
    let (width, height) = client_dips(normal, dpi);
    Startup {
        x: normal.x,
        y: normal.y,
        width,
        height,
        maximized: saved.maximized,
    }
}

/// The client size, in device-independent pixels, of an editor whose outer frame
/// (including the invisible resize borders `GetWindowRect` reports) is `frame` at `dpi`.
fn client_dips(frame: Bounds, dpi: u32) -> (f64, f64) {
    let mut border = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: border is a valid writable RECT; the editor has the standard overlapped frame.
    unsafe { AdjustWindowRectExForDpi(&mut border, WS_OVERLAPPEDWINDOW, 0, 0, dpi) };
    let scale = f64::from(dpi) / 96.0;
    // Half a pixel more, so Avalonia's truncating `(int)(dips * scale)` gives back
    // exactly this client size instead of one pixel less on every restart.
    let dips = |frame: i32, border: i32| (f64::from((frame - border).max(1)) + 0.5) / scale;
    (
        dips(frame.width, border.right - border.left),
        dips(frame.height, border.bottom - border.top),
    )
}

/// Call before Avalonia shows a hidden editor again (opening it from the tray,
/// or restoring it after a capture): Avalonia re-shows a hidden window in the
/// state it last applied itself, which is not a maximize done in the window's
/// title bar, so the editor came back restored and its saved state was lost.
pub(super) fn before_show(scope: &rustolonia::AppScope, window: &rustolonia::Window) {
    use rustolonia::WindowState;
    let Ok(handle) = scope.platform_handle(window) else { return };
    let hwnd = handle as HWND;
    // SAFETY: the visibility query does not retain hwnd.
    if hwnd.is_null() || unsafe { IsWindowVisible(hwnd) } != 0 {
        return;
    }
    let Some(maximized) = WINDOWS.with(|windows| {
        windows.borrow().get(&handle).map(|state| state.placement.maximized)
    }) else {
        return;
    };
    let avalonia = window.window_state();
    crate::trace::line(&format!(
        "editor placement: before show maximized={maximized} avalonia={avalonia:?} zoomed={}",
        // SAFETY: the query does not retain hwnd.
        unsafe { IsZoomed(hwnd) } != 0
    ));
    if !maximized {
        return;
    }
    // Setting the state Avalonia already reports is ignored, so step through Normal:
    // while the window is hidden, Avalonia only records the state for its next Show.
    let result = match avalonia {
        Ok(WindowState::Maximized) => window
            .set_window_state(WindowState::Normal)
            .and_then(|()| window.set_window_state(WindowState::Maximized)),
        _ => window.set_window_state(WindowState::Maximized),
    };
    if let Err(error) = result {
        eprintln!("Editor placement: could not keep the editor maximized: {error}");
    }
    crate::trace::line(&format!(
        "editor placement: before show applied visible={} zoomed={}",
        // SAFETY: the queries do not retain hwnd.
        unsafe { IsWindowVisible(hwnd) } != 0,
        unsafe { IsZoomed(hwnd) } != 0
    ));
}

/// Sets the bounds a maximized window restores to, without changing its state.
fn set_restore_bounds(hwnd: HWND, normal: Bounds, monitor: &screen::Monitor) -> io::Result<()> {
    // SAFETY: WINDOWPLACEMENT is plain data; zero is a valid starting value.
    let mut placement: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
    placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
    // SAFETY: hwnd is live and placement is writable with its length set.
    if unsafe { GetWindowPlacement(hwnd, &mut placement) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // rcNormalPosition is in workspace coordinates: relative to the work area,
    // not the monitor, when a taskbar sits on the top or left edge.
    let (dx, dy) = (
        monitor.work.x - monitor.rect.x,
        monitor.work.y - monitor.rect.y,
    );
    placement.flags = 0;
    placement.showCmd = SW_SHOWMAXIMIZED as u32;
    placement.rcNormalPosition = RECT {
        left: normal.x - dx,
        top: normal.y - dy,
        right: normal.x - dx + normal.width,
        bottom: normal.y - dy + normal.height,
    };
    // SAFETY: hwnd is live and placement is fully initialized.
    if unsafe { SetWindowPlacement(hwnd, &placement) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Installs on the UI thread after Rustolonia has created the native editor window.
pub(super) fn install(hwnd: HWND) -> io::Result<()> {
    if WINDOWS.with(|windows| windows.borrow().contains_key(&(hwnd as isize))) {
        return Ok(());
    }
    let monitors = screen::monitors();
    let saved = load();
    let monitor = match saved {
        Some(saved) => monitor_for(saved.normal, &monitors),
        None => {
            let mut point = POINT { x: 0, y: 0 };
            // SAFETY: point is a valid writable POINT.
            if unsafe { GetCursorPos(&mut point) } == 0 {
                return Err(io::Error::last_os_error());
            }
            monitor_for(
                Bounds {
                    x: point.x,
                    y: point.y,
                    width: 1,
                    height: 1,
                },
                &monitors,
            )
        }
    }
    .ok_or_else(|| io::Error::other("no usable monitor is available for the editor"))?;
    let normal = match saved {
        Some(saved) => restored(saved, monitor.work, monitor.dpi),
        None => initial(monitor.work, monitor.dpi),
    };
    let placement = Placement {
        normal,
        dpi: monitor.dpi,
        maximized: saved.is_some_and(|saved| saved.maximized),
    };
    // SAFETY: the window belongs to this UI thread; the callback is static and removed on destruction.
    if unsafe { SetWindowSubclass(hwnd, Some(subclass), SUBCLASS_ID, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    WINDOWS.with(|windows| {
        windows.borrow_mut().insert(
            hwnd as isize,
            State {
                placement,
                last_saved: saved,
                applying: false,
                queued: false,
                moving: false,
            },
        );
    });
    // SAFETY: querying the maximized state does not retain hwnd.
    let zoomed = unsafe { IsZoomed(hwnd) } != 0;
    crate::trace::line(&format!(
        "editor placement: install maximized={} zoomed={zoomed}",
        placement.maximized
    ));
    if zoomed {
        // Avalonia already showed it maximized (see `startup`): only remember
        // where it restores to, without a second state change.
        set_restore_bounds(hwnd, normal, monitor)?;
    } else {
        place(hwnd, normal)?;
        if placement.maximized {
            // The state did not reach Avalonia before Show. Maximize once this
            // callback (and Avalonia's first layout pass) has returned.
            // SAFETY: the custom message contains no pointers and targets our live window.
            if unsafe { PostMessageW(hwnd, MAXIMIZE, 0, 0) } == 0 {
                eprintln!(
                    "Editor placement: could not queue maximizing: {}",
                    io::Error::last_os_error()
                );
            }
        }
    }
    queue(hwnd);
    Ok(())
}

fn place(hwnd: HWND, bounds: Bounds) -> io::Result<()> {
    WINDOWS.with(|windows| {
        if let Some(state) = windows.borrow_mut().get_mut(&(hwnd as isize)) {
            state.applying = true;
        }
    });
    let result = (|| {
        // A cross-monitor move can synchronously trigger Avalonia's DPI resize.
        // Repeat after that resize so the final outer frame is the fitted rectangle.
        for _ in 0..2 {
            // SAFETY: hwnd is live, bounds are valid physical-pixel coordinates, and no pointers are retained.
            if unsafe {
                SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    bounds.x,
                    bounds.y,
                    bounds.width,
                    bounds.height,
                    SWP_NOACTIVATE | SWP_NOZORDER,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            if window_bounds(hwnd)? == bounds {
                return Ok(());
            }
        }
        Err(io::Error::other(
            "the editor window could not be fitted to its monitor",
        ))
    })();
    WINDOWS.with(|windows| {
        if let Some(state) = windows.borrow_mut().get_mut(&(hwnd as isize)) {
            state.applying = false;
        }
    });
    result
}

fn queue(hwnd: HWND) {
    let post = WINDOWS.with(|windows| {
        let mut windows = windows.borrow_mut();
        let Some(state) = windows.get_mut(&(hwnd as isize)) else {
            return false;
        };
        if state.applying || state.queued || state.moving {
            return false;
        }
        state.queued = true;
        true
    });
    if post {
        // SAFETY: the custom message contains no pointers and targets our live window.
        if unsafe { PostMessageW(hwnd, RECONCILE, 0, 0) } == 0 {
            WINDOWS.with(|windows| {
                if let Some(state) = windows.borrow_mut().get_mut(&(hwnd as isize)) {
                    state.queued = false;
                }
            });
            eprintln!(
                "Editor placement: could not queue a placement update: {}",
                io::Error::last_os_error()
            );
        }
    }
}

fn reconcile(hwnd: HWND) -> io::Result<()> {
    let previous = WINDOWS.with(|windows| {
        let mut windows = windows.borrow_mut();
        let state = windows.get_mut(&(hwnd as isize))?;
        state.queued = false;
        Some(*state)
    });
    let Some(previous) = previous else {
        return Ok(());
    };
    if previous.moving {
        return Ok(());
    }
    // SAFETY: these queries do not retain the live window handle.
    if unsafe { IsIconic(hwnd) } != 0 {
        return Ok(());
    }
    // A hidden editor is never natively maximized; Avalonia applies the remembered state
    // when it shows it, so keep that state rather than saving "not maximized".
    // SAFETY: these queries do not retain hwnd.
    let maximized = if unsafe { IsWindowVisible(hwnd) } != 0 {
        (unsafe { IsZoomed(hwnd) }) != 0
    } else {
        previous.placement.maximized
    };
    let bounds = window_bounds(hwnd)?;
    let monitors = screen::monitors();
    let monitor = monitor_for(bounds, &monitors)
        .ok_or_else(|| io::Error::other("no usable monitor is available for the editor"))?;
    let normal = if maximized {
        restored(previous.placement, monitor.work, monitor.dpi)
    } else {
        bounds.fit(monitor.work)
    };
    if !maximized && normal != bounds {
        place(hwnd, normal)?;
    }
    // SAFETY: the DPI query does not retain hwnd.
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 {
        return Err(io::Error::last_os_error());
    }
    let placement = Placement {
        normal,
        dpi,
        maximized,
    };
    WINDOWS.with(|windows| {
        if let Some(state) = windows.borrow_mut().get_mut(&(hwnd as isize)) {
            state.placement = placement;
        }
    });
    if Some(placement) != previous.last_saved {
        persist(placement)?;
        WINDOWS.with(|windows| {
            if let Some(state) = windows.borrow_mut().get_mut(&(hwnd as isize)) {
                state.last_saved = Some(placement);
            }
        });
    }
    Ok(())
}

unsafe extern "system" fn subclass(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    _data: usize,
) -> LRESULT {
    if message == RECONCILE {
        if let Err(error) = reconcile(hwnd) {
            eprintln!("Editor placement: {error}");
        }
        return 0;
    }
    if message == MAXIMIZE {
        // A hidden editor keeps Avalonia's own state for its next Show; never show it from here.
        // SAFETY: these queries and the state change only use our live window.
        unsafe {
            if IsWindowVisible(hwnd) != 0 && IsZoomed(hwnd) == 0 && IsIconic(hwnd) == 0 {
                ShowWindow(hwnd, SW_MAXIMIZE);
            }
        }
        return 0;
    }
    if message == WM_NCDESTROY {
        WINDOWS.with(|windows| windows.borrow_mut().remove(&(hwnd as isize)));
        // SAFETY: this callback was installed with the same window and identifier.
        unsafe { RemoveWindowSubclass(hwnd, Some(subclass), id) };
    }
    if matches!(message, WM_ENTERSIZEMOVE | WM_EXITSIZEMOVE) {
        WINDOWS.with(|windows| {
            if let Some(state) = windows.borrow_mut().get_mut(&(hwnd as isize)) {
                state.moving = message == WM_ENTERSIZEMOVE;
            }
        });
    }
    // SAFETY: forward the original message to Avalonia and the other subclasses.
    let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
    if message == WM_GETMINMAXINFO && lparam != 0 {
        if let Ok(bounds) = window_bounds(hwnd) {
            if let Some(monitor) = monitor_for(bounds, &screen::monitors()) {
                // SAFETY: WM_GETMINMAXINFO provides a writable MINMAXINFO for this callback.
                let info = unsafe { &mut *(lparam as *mut MINMAXINFO) };
                info.ptMinTrackSize.x = info.ptMinTrackSize.x.min(
                    (f64::from(monitor.work.width) * INITIAL_WORK_AREA_FRACTION).round() as i32,
                );
                info.ptMinTrackSize.y = info.ptMinTrackSize.y.min(
                    (f64::from(monitor.work.height) * INITIAL_WORK_AREA_FRACTION).round() as i32,
                );
            }
        }
    }
    if matches!(message, WM_EXITSIZEMOVE | WM_DISPLAYCHANGE | WM_SETTINGCHANGE | WM_DPICHANGED | WM_SHOWWINDOW)
        // SAFETY: visibility is queried only while hwnd is live.
        || (matches!(message, WM_SIZE | WM_MOVE) && unsafe { IsWindowVisible(hwnd) } != 0)
    {
        queue(hwnd);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(work: Rect, dpi: u32) -> screen::Monitor {
        screen::Monitor {
            rect: work,
            work,
            dpi,
            primary: false,
            device: String::new(),
        }
    }

    #[test]
    fn first_open_centers_on_pointer_monitor_and_caps_preferred_size() {
        let monitors = [
            monitor(Rect::new(0, 0, 1920, 1040), 96),
            monitor(Rect::new(-2560, -200, 2560, 1400), 144),
        ];
        let target = monitor_for(
            Bounds {
                x: -1800,
                y: 0,
                width: 1,
                height: 1,
            },
            &monitors,
        )
        .unwrap();
        assert_eq!(
            initial(target.work, target.dpi),
            Bounds {
                x: -2405,
                y: -130,
                width: 2250,
                height: 1260
            }
        );
        assert_eq!(
            initial(monitors[0].work, 96),
            Bounds {
                x: 210,
                y: 52,
                width: 1500,
                height: 936
            }
        );
    }

    #[test]
    fn valid_saved_placement_is_preserved_and_round_trips_maximized_state() {
        let saved = Placement {
            normal: Bounds {
                x: 2100,
                y: 100,
                width: 1200,
                height: 800,
            },
            dpi: 120,
            maximized: true,
        };
        let decoded: Placement =
            serde_json::from_value(serde_json::to_value(saved).unwrap()).unwrap();
        assert_eq!(decoded, saved);
        assert_eq!(
            restored(decoded, Rect::new(1920, 0, 1920, 1040), 120),
            saved.normal
        );
    }

    #[test]
    fn startup_carries_the_fitted_position_and_maximized_state_to_avalonia() {
        let saved = Placement {
            normal: Bounds {
                x: 1170,
                y: 300,
                width: 1516,
                height: 999,
            },
            dpi: 96,
            maximized: true,
        };
        // At 96 DPI the standard frame adds 16 x 39 pixels around the client area.
        assert_eq!(
            startup_for(saved, Rect::new(0, 0, 3840, 1560), 96),
            Startup {
                x: 1170,
                y: 300,
                width: 1500.5,
                height: 960.5,
                maximized: true
            }
        );
        // A frame saved partly off a smaller work area opens fully inside it.
        assert_eq!(
            startup_for(
                Placement {
                    maximized: false,
                    ..saved
                },
                Rect::new(0, 0, 1920, 1040),
                96
            ),
            Startup {
                x: 404,
                y: 41,
                width: 1500.5,
                height: 960.5,
                maximized: false
            }
        );
        // Saved on a 150 % monitor that is gone: the frame is rescaled to the 100 %
        // monitor that remains and fitted against its right edge.
        assert_eq!(
            startup_for(
                Placement {
                    normal: Bounds {
                        x: 3840,
                        y: 0,
                        width: 1920,
                        height: 1020,
                    },
                    dpi: 144,
                    maximized: false,
                },
                Rect::new(0, 0, 3840, 1560),
                96
            ),
            Startup {
                x: 2560,
                y: 0,
                width: 1264.5,
                height: 641.5,
                maximized: false
            }
        );
    }

    #[test]
    fn startup_size_round_trips_through_avalonias_truncating_resize() {
        for dpi in [96, 120, 144, 168, 192, 240] {
            let mut border = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            // SAFETY: border is a valid writable RECT.
            unsafe { AdjustWindowRectExForDpi(&mut border, WS_OVERLAPPEDWINDOW, 0, 0, dpi) };
            let scale = f64::from(dpi) / 96.0;
            for (width, height) in [(1516, 999), (1901, 1013), (2275, 1499), (1000, 700), (3853, 2167)] {
                let frame = Bounds {
                    x: 0,
                    y: 0,
                    width,
                    height,
                };
                let (w, h) = client_dips(frame, dpi);
                // What Avalonia's WindowImpl.Resize does with them.
                let client = ((w * scale) as i32, (h * scale) as i32);
                assert_eq!(
                    (
                        client.0 + border.right - border.left,
                        client.1 + border.bottom - border.top
                    ),
                    (width, height),
                    "the frame must survive Avalonia's resize at {dpi} DPI"
                );
            }
        }
    }

    #[test]
    fn straddling_window_fits_the_monitor_with_greatest_overlap() {
        let monitors = [
            monitor(Rect::new(0, 0, 1920, 1040), 96),
            monitor(Rect::new(1920, 0, 1920, 1040), 96),
        ];
        let bounds = Bounds {
            x: 1800,
            y: 100,
            width: 1500,
            height: 900,
        };
        let target = monitor_for(bounds, &monitors).unwrap();
        assert_eq!(
            bounds.fit(target.work),
            Bounds {
                x: 1920,
                y: 100,
                width: 1500,
                height: 900
            }
        );
    }

    #[test]
    fn pointer_over_a_side_taskbar_stays_on_its_physical_monitor() {
        let monitors = [
            screen::Monitor {
                rect: Rect::new(0, 0, 1920, 1080),
                ..monitor(Rect::new(0, 0, 1840, 1080), 96)
            },
            monitor(Rect::new(1920, 0, 1920, 1080), 96),
        ];
        let target = monitor_for(
            Bounds {
                x: 1910,
                y: 500,
                width: 1,
                height: 1,
            },
            &monitors,
        )
        .unwrap();
        assert_eq!(target.work, monitors[0].work);
        assert_eq!(
            initial(target.work, 96),
            Bounds {
                x: 170,
                y: 60,
                width: 1500,
                height: 960
            }
        );
    }

    #[test]
    fn disconnected_monitor_and_changed_dpi_shrink_to_remaining_work_area() {
        let saved = Placement {
            normal: Bounds {
                x: 4000,
                y: -500,
                width: 1500,
                height: 960,
            },
            dpi: 96,
            maximized: false,
        };
        let monitors = [
            monitor(Rect::new(-1280, 40, 1280, 680), 144),
            monitor(Rect::new(-4000, 40, 1280, 680), 96),
        ];
        let target = monitor_for(saved.normal, &monitors).unwrap();
        assert_eq!(
            restored(saved, target.work, target.dpi),
            Bounds {
                x: -1280,
                y: 40,
                width: 1280,
                height: 680
            }
        );
        let work = Rect::new(0, 0, 3840, 2080);
        assert_eq!(
            restored(saved, work, 144),
            Bounds {
                x: 1590,
                y: 0,
                width: 2250,
                height: 1440
            }
        );
    }

    #[test]
    fn fitting_respects_taskbars_exact_edges_and_small_work_areas() {
        let work = Rect::new(-1000, -500, 800, 450);
        let bounds = Bounds {
            x: -990,
            y: -490,
            width: 790,
            height: 440,
        };
        assert_eq!(bounds.fit(work), bounds);
        assert_eq!(
            Bounds {
                x: -989,
                y: -489,
                ..bounds
            }
            .fit(work),
            bounds
        );
        assert_eq!(
            initial(work, 192),
            Bounds {
                x: -960,
                y: -478,
                width: 720,
                height: 405
            }
        );
        assert!(!Bounds {
            x: i32::MAX,
            y: 0,
            width: 1,
            height: 1
        }
        .valid());
        assert!(!Bounds { width: 0, ..bounds }.valid());
        assert!(monitor_for(bounds, &[]).is_none());
    }

    #[test]
    fn native_editor_fits_one_monitor_and_persists_across_recreation() {
        const CHILD: &str = "JIFFSHOT_PLACEMENT_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let directory = std::env::temp_dir().join(format!(
                "jiffshot-placement-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&directory).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "ui::editor_placement::tests::native_editor_fits_one_monitor_and_persists_across_recreation", "--nocapture"])
                .env(CHILD, "1")
                .env("JIFFSHOT_CONFIG_DIR", &directory)
                .output()
                .expect("run the native placement fixture with isolated settings");
            std::fs::remove_dir_all(&directory).unwrap();
            assert!(
                output.status.success(),
                "native placement fixture failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, SendMessageW, SW_SHOWMINNOACTIVE, WS_OVERLAPPEDWINDOW,
        };
        struct TestWindow(HWND);
        impl TestWindow {
            fn new() -> Self {
                let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
                // SAFETY: STATIC is a system class; the string is terminated and no creation data is retained.
                let hwnd = unsafe {
                    CreateWindowExW(
                        0,
                        class.as_ptr(),
                        std::ptr::null(),
                        WS_OVERLAPPEDWINDOW,
                        0,
                        0,
                        1200,
                        800,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null(),
                    )
                };
                assert!(
                    !hwnd.is_null(),
                    "create the hidden placement fixture: {}",
                    io::Error::last_os_error()
                );
                Self(hwnd)
            }
        }
        impl Drop for TestWindow {
            fn drop(&mut self) {
                // SAFETY: this test created the window on the current thread.
                unsafe { DestroyWindow(self.0) };
            }
        }
        screen::enable_per_monitor_dpi();
        config::try_set_section("editor", serde_json::json!({"recent": ["keep.png"]})).unwrap();
        let window = TestWindow::new();
        install(window.0).expect("install editor placement");
        // SAFETY: the live fixture's subclass handles this pointer-free message.
        unsafe { SendMessageW(window.0, RECONCILE, 0, 0) };
        let first = window_bounds(window.0).unwrap();
        let monitors = screen::monitors();
        let target = monitor_for(first, &monitors).unwrap();
        assert_eq!(
            first.fit(target.work),
            first,
            "the initial outer frame must fit one work area"
        );
        assert!(load().is_some(), "first opening must persist its placement");

        let oversized = Bounds {
            x: target.work.x - 100,
            y: target.work.y - 100,
            width: target.work.width + 200,
            height: target.work.height + 200,
        };
        // SAFETY: simulate only the fixture's native move/resize loop.
        unsafe { SendMessageW(window.0, WM_ENTERSIZEMOVE, 0, 0) };
        // SAFETY: move only the hidden fixture; Windows may cap its size to the native tracking limit.
        assert_ne!(
            unsafe {
                SetWindowPos(
                    window.0,
                    std::ptr::null_mut(),
                    oversized.x,
                    oversized.y,
                    oversized.width,
                    oversized.height,
                    SWP_NOACTIVATE | SWP_NOZORDER,
                )
            },
            0,
            "simulate stale, oversized editor bounds"
        );
        let stale = window_bounds(window.0).unwrap();
        let target = monitor_for(stale, &monitors).unwrap();
        assert_ne!(
            stale.fit(target.work),
            stale,
            "the fixture must initially cross a work-area boundary"
        );
        // SAFETY: these pointer-free messages target the live fixture's subclass.
        unsafe { SendMessageW(window.0, RECONCILE, 0, 0) };
        assert_eq!(
            window_bounds(window.0).unwrap(),
            stale,
            "placement must not fight an in-progress drag"
        );
        assert_eq!(
            load().unwrap().normal,
            first,
            "in-progress drag bounds must not be persisted"
        );
        // SAFETY: the live fixture owns the installed subclass.
        unsafe {
            SendMessageW(window.0, WM_EXITSIZEMOVE, 0, 0);
            SendMessageW(window.0, RECONCILE, 0, 0);
        }
        let fitted = window_bounds(window.0).unwrap();
        assert_eq!(
            fitted,
            stale.fit(target.work),
            "monitor changes must refit the complete outer frame"
        );
        let saved = load().unwrap();
        assert_eq!(
            saved.normal, fitted,
            "the fitted normal bounds must be saved"
        );
        assert_eq!(
            config::section("editor")["recent"][0],
            "keep.png",
            "placement saves must not replace editor preferences"
        );
        // SAFETY: minimize only this fixture without activating it, then request reconciliation.
        unsafe {
            ShowWindow(window.0, SW_SHOWMINNOACTIVE);
            SendMessageW(window.0, RECONCILE, 0, 0);
        }
        assert_eq!(
            load().unwrap(),
            saved,
            "minimized coordinates must not replace normal bounds"
        );
        drop(window);

        let window = TestWindow::new();
        install(window.0).expect("restore the editor placement");
        assert_eq!(
            window_bounds(window.0).unwrap(),
            saved.normal,
            "a new window must restore the saved normal placement"
        );
    }

    /// Regression: Avalonia raises `Loaded` (where `install` runs) before it shows the
    /// window, and the tray-started editor is mounted hidden. Maximizing there made the
    /// native window visible behind Avalonia's back, so it never started rendering and
    /// the editor opened as an empty frame.
    #[test]
    fn saved_maximized_state_never_shows_a_hidden_editor_and_maximizes_after_the_callback() {
        const CHILD: &str = "JIFFSHOT_PLACEMENT_MAXIMIZED_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let directory = std::env::temp_dir().join(format!(
                "jiffshot-placement-maximized-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&directory).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "ui::editor_placement::tests::saved_maximized_state_never_shows_a_hidden_editor_and_maximizes_after_the_callback", "--nocapture"])
                .env(CHILD, "1")
                .env("JIFFSHOT_CONFIG_DIR", &directory)
                .output()
                .expect("run the native maximized placement fixture with isolated settings");
            std::fs::remove_dir_all(&directory).unwrap();
            assert!(
                output.status.success(),
                "native maximized placement fixture failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, DispatchMessageW, PeekMessageW, TranslateMessage, MSG,
            PM_REMOVE, SW_SHOWNOACTIVATE, WS_OVERLAPPEDWINDOW,
        };
        struct TestWindow(HWND);
        impl Drop for TestWindow {
            fn drop(&mut self) {
                // SAFETY: this test created the window on the current thread.
                unsafe { DestroyWindow(self.0) };
            }
        }
        fn window() -> TestWindow {
            let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
            // SAFETY: STATIC is a system class; the string is terminated and no creation data is retained.
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    class.as_ptr(),
                    std::ptr::null(),
                    WS_OVERLAPPEDWINDOW,
                    0,
                    0,
                    1200,
                    800,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            };
            assert!(
                !hwnd.is_null(),
                "create the placement fixture: {}",
                io::Error::last_os_error()
            );
            TestWindow(hwnd)
        }
        fn pump() {
            // SAFETY: MSG is plain data; the loop only drains this thread's queue.
            unsafe {
                let mut message: MSG = std::mem::zeroed();
                while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        }

        screen::enable_per_monitor_dpi();
        let monitors = screen::monitors();
        let primary = monitors
            .iter()
            .find(|monitor| monitor.primary)
            .or(monitors.first())
            .expect("a monitor");
        let normal = initial(primary.work, primary.dpi);
        persist(Placement {
            normal,
            dpi: primary.dpi,
            maximized: true,
        })
        .unwrap();
        assert_eq!(
            startup().map(|startup| (startup.x, startup.y, startup.maximized)),
            Some((normal.x, normal.y, true)),
            "the saved maximized state must be handed to Avalonia before Show"
        );

        let hidden = window();
        install(hidden.0).expect("install on the hidden editor");
        pump();
        // SAFETY: queries on the live fixture.
        unsafe {
            assert_eq!(
                IsWindowVisible(hidden.0),
                0,
                "placement must never show a hidden editor"
            );
            assert_eq!(
                IsZoomed(hidden.0),
                0,
                "a hidden editor must not be maximized natively"
            );
        }
        assert!(
            load().unwrap().maximized,
            "a hidden editor must keep the saved maximized state"
        );
        drop(hidden);

        let shown = window();
        // SAFETY: show only this fixture, without activating it.
        unsafe { ShowWindow(shown.0, SW_SHOWNOACTIVATE) };
        install(shown.0).expect("install on the shown editor");
        // SAFETY: query on the live fixture.
        assert_eq!(
            unsafe { IsZoomed(shown.0) },
            0,
            "install must not change the window state inside Avalonia's Loaded callback"
        );
        pump();
        // SAFETY: query on the live fixture.
        assert_ne!(
            unsafe { IsZoomed(shown.0) },
            0,
            "the shown editor must be maximized afterwards"
        );
        pump();
        let saved = load().unwrap();
        assert!(saved.maximized, "the maximized state must persist");
        assert_eq!(
            saved.normal, normal,
            "maximizing must keep the restored bounds"
        );
    }
}
