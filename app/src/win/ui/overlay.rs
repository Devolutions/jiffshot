//! The selection overlay (the `overlay` signal), one window per monitor, and
//! Screen Draw (the `screen_draw` signal).
//!
//! The overlay's windows are made ahead of time (at startup, see
//! [`install`]) and only hidden natively between captures, so putting the
//! overlay up is loading the new screenshot, letting it render, and a
//! native show, not a full Avalonia show. They are taken down as soon as the
//! selection is made, and again when the capture side says so. The capture
//! side owns the selection; the windows send it their input and draw what
//! it says (see [`overlay_api`]).

pub(super) mod overlay_api;
mod overlay_view;

use super::hud::native;
use jiffshot_capture::{CaptureMapping, Rect};
use overlay_api as api;
use overlay_view::{MonitorWindow, OverlayMonitor, Scene};
use rustolonia::pixel_surface::{Cursor, Interpolation, MouseButton, PixelFormat, Placement, PointerKind, SurfaceInput};
use rustolonia::{AppScope, Brush, Color, Effect, PixelSurface, Result, Window, WindowDecorations};
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// How long a freshly loaded screenshot gets to render before the windows
/// show, so the last capture's never flashes. The host has no "frame
/// rendered" notification; a timer this short completes on a later
/// dispatcher turn, measured at 20–30 ms after the load (about a frame).
const FRAME_WAIT: Duration = Duration::from_millis(1);

/// Screen Draw's fading marks are re-rendered this often.
const FADE_TICK: Duration = Duration::from_millis(40);

/// Screen Draw's background: barely there, so the pointer stays on it.
#[cfg(windows)]
const DRAW_BACKGROUND: u32 = 0x0100_0000;

thread_local! {
    static HOST: RefCell<Host> = RefCell::new(Host::default());
    static REDRAW_POSTED: Cell<bool> = const { Cell::new(false) };
    static DRAW: RefCell<Option<ScreenDraw>> = const { RefCell::new(None) };
}

#[cfg(target_os = "macos")]
pub(super) fn session_for_selftest() -> Option<u64> {
    HOST.with(|host| host.borrow().session.as_ref().map(|session| session.id))
}

/// One showing of the overlay: the capture side's session, its monitors and
/// what they show.
struct Session {
    id: u64,
    monitors: Vec<OverlayMonitor>,
    scene: Scene,
}

/// The native window frame in desktop coordinates and the captured image's
/// rectangle within that window, in DIPs.
#[derive(Clone, Copy, Debug, PartialEq)]
struct SurfaceViewport {
    frame: [f64; 4],
    content: [f64; 4],
}

impl SurfaceViewport {
    fn new(mapping: CaptureMapping, scale: f64) -> std::result::Result<Self, String> {
        if !scale.is_finite() || scale <= 0.0 {
            return Err("invalid capture window scale".to_owned());
        }
        #[cfg(target_os = "macos")]
        let frame = native::mapped_frame_bounds(mapping, scale).map_err(|error| error.to_string())?;
        #[cfg(windows)]
        let frame = mapping.desktop_bounds();
        let [left, top, right, bottom] = mapping.desktop_bounds();
        let content = [(left - frame[0]) / scale, (top - frame[1]) / scale, (right - left) / scale, (bottom - top) / scale];
        if !content.iter().all(|value| value.is_finite()) || content[2] <= 0.0 || content[3] <= 0.0 {
            return Err("invalid capture surface geometry".to_owned());
        }
        Ok(Self { frame, content })
    }

    fn placement(self) -> Placement {
        #[cfg(target_os = "macos")]
        {
            let [x, y, width, height] = self.content;
            Placement::Rect { x, y, width, height }
        }
        #[cfg(windows)]
        {
            Placement::Fill
        }
    }
}

impl Session {
    /// Sends input (desktop pixels) and takes the new state; true once the
    /// selection is made.
    fn input(&mut self, kind: i32, x: f64, y: f64, value: i32, mods: i32) -> bool {
        self.scene.state = api::input(self.id, kind, x, y, value, mods);
        self.done()
    }

    fn done(&self) -> bool {
        self.scene.state.has(api::FLAG_DONE)
    }

    /// Fetches the texts, and the points when their counts changed.
    fn refresh(&mut self) -> std::result::Result<(), String> {
        if self.done() {
            return Ok(());
        }
        let (id, st) = (self.id, self.scene.state);
        self.scene.title = api::text(id, api::TEXT_TITLE)?;
        self.scene.size = api::text(id, api::TEXT_SIZE)?;
        if self.scene.hint.is_empty() {
            self.scene.hint = api::text(id, api::TEXT_HINT)?;
        }
        let path_len = st.path_count.max(0) as usize * 2;
        if self.scene.path.len() != path_len {
            self.scene.path = api::points(id, api::POINTS_PATH, path_len);
        }
        let regions_len = st.region_count.max(0) as usize * 4;
        if self.scene.regions.len() != regions_len {
            self.scene.regions = api::points(id, api::POINTS_REGIONS, regions_len);
        }
        if st.has(api::FLAG_GUIDES) {
            self.scene.guides = api::guide_rectangles(id)?;
        } else {
            self.scene.guides.clear();
        }
        Ok(())
    }
}

/// The overlay's windows and the session showing, on the UI thread.
#[derive(Default)]
struct Host {
    scope: Option<AppScope>,
    windows: Vec<MonitorWindow>,
    session: Option<Session>,
    next_window_id: u64,
}

impl Host {
    fn scope(&self) -> Option<AppScope> {
        self.scope.clone()
    }

    /// The window over `monitor`, made (and shown once, hidden) if needed.
    fn window_for(&mut self, scope: &AppScope, monitor: OverlayMonitor) -> std::result::Result<usize, String> {
        let viewport = SurfaceViewport::new(monitor.mapping, monitor.scale)?;
        if let Some(i) = self.windows.iter().position(|w| w.rect == monitor.rect) {
            if self.windows[i].hwnd(scope) != 0 {
                self.windows[i].set_monitor(monitor, viewport);
                return Ok(i);
            }
            self.windows.remove(i).close();
        }
        let rect = monitor.rect;
        self.next_window_id += 1;
        let id = self.next_window_id;
        let window = MonitorWindow::new(scope, id, monitor, viewport, move |input| Ok(on_input(rect, input)))
            .map_err(|error| format!("{error:?}"))?;
        // Shown once, cloaked so nothing flashes, then hidden natively:
        // Avalonia keeps it as a live window from then on. Shown on its own
        // screen, it takes that screen's scale now rather than on the first
        // capture.
        let initial_handle = {
            let handle = window.hwnd(scope);
            #[cfg(target_os = "macos")]
            if handle == 0 {
                window.close();
                return Err("the native selection window cannot be cloaked before its initial show".to_owned());
            }
            native::cloak(handle, true);
            #[cfg(target_os = "macos")]
            if !crate::mac_window::window(handle).is_some_and(|native| native.alphaValue() == 0.0) {
                window.close();
                native::forget_cloak(handle);
                return Err("the selection window could not be cloaked before its initial show".to_owned());
            }
            #[cfg(windows)]
            if handle != 0 {
                if let Err(error) = place_monitor(handle, monitor, viewport, false) {
                    window.close();
                    return Err(format!("before the initial show: {error}"));
                }
            }
            #[cfg(target_os = "macos")]
            {
                // Avalonia clamps a window's size to the work area until it
                // has been shown. Set only its position until that first show.
                let result = initial_monitor_position(viewport, monitor.scale).and_then(|position| {
                    window.window.set_position(position)
                        .map_err(|error| format!("could not position the cloaked selection window: {error:?}"))
                });
                if let Err(error) = result {
                    window.close();
                    native::forget_cloak(handle);
                    return Err(error);
                }
            }
            handle
        };
        if let Err(error) = window.window.show() {
            window.close();
            #[cfg(target_os = "macos")]
            native::forget_cloak(initial_handle);
            return Err(format!("could not show the cloaked selection window: {error:?}"));
        }
        let hwnd = window.hwnd(scope);
        if hwnd == 0 {
            window.close();
            #[cfg(target_os = "macos")]
            native::forget_cloak(initial_handle);
            return Err("the native selection window is unavailable".to_owned());
        }
        #[cfg(target_os = "macos")]
        if hwnd != initial_handle || !crate::mac_window::window(hwnd).is_some_and(|native| native.alphaValue() == 0.0) {
            native::hide(hwnd);
            window.close();
            native::forget_cloak(initial_handle);
            return Err("the selection window changed or lost its cloak during its initial show".to_owned());
        }
        #[cfg(windows)]
        let _ = initial_handle;
        native::cloak(hwnd, true);
        if let Err(error) = place_monitor(hwnd, monitor, viewport, true) {
            window.close();
            #[cfg(target_os = "macos")]
            native::forget_cloak(initial_handle);
            return Err(format!("after the initial show: {error}"));
        }
        if let Err(error) = scope.post(move || {
            if let Some((handle, monitor, viewport)) = window_geometry(id) {
                if let Err(error) = place_monitor(handle, monitor, viewport, false) {
                    native::hide(handle);
                    native::cloak(handle, false);
                    fail_window_placement(id, error);
                    return;
                }
                native::hide(handle);
                native::cloak(handle, false);
            }
        }) {
            window.close();
            #[cfg(target_os = "macos")]
            native::forget_cloak(initial_handle);
            return Err(format!("{error:?}"));
        }
        self.windows.push(window);
        Ok(self.windows.len() - 1)
    }

    /// Draws the session (if any) on every window.
    fn draw(&mut self) -> std::result::Result<(), String> {
        let Some(session) = self.session.as_mut() else { return Ok(()) };
        session.refresh()?;
        for window in &mut self.windows {
            if session.monitors.iter().any(|m| m.rect == window.rect) {
                window.draw(&session.scene).map_err(|error| format!("{error:?}"))?;
            }
        }
        Ok(())
    }

    /// Input on the window over `rect`; returns whether it was handled and
    /// whether the selection is now made.
    fn input(&mut self, rect: Rect, input: &SurfaceInput) -> (bool, bool) {
        let Some(window) = self.windows.iter_mut().find(|w| w.rect == rect) else {
            return (false, false);
        };
        if let SurfaceInput::Resize(metrics) = input {
            // Windows moves and resizes a window whose screen scale
            // changed; put it back (after Avalonia has applied the size
            // Windows suggests).
            let changed = window.reported_scaling != 0.0 && window.reported_scaling != metrics.scaling;
            window.reported_scaling = metrics.scaling;
            if changed {
                if let Some(scope) = &self.scope {
                    let id = window.id;
                    let _ = scope.post(move || {
                        if let Some((handle, monitor, viewport)) = window_geometry(id) {
                            if let Err(error) = place_monitor(handle, monitor, viewport, false) {
                                fail_window_placement(id, error);
                            }
                        }
                    });
                }
                schedule_redraw(self.scope.as_ref());
            }
            return (false, false);
        }
        let Some(session) = self.session.as_mut().filter(|s| !s.done()) else {
            return (false, false);
        };
        let (kind, x, y, value, mods) = match input {
            SurfaceInput::Pointer(p) => {
                let (x, y) = window.desktop(p.x, p.y);
                let mods = api::modifiers(p.modifiers);
                match p.kind {
                    PointerKind::Pressed if p.button == MouseButton::Right => (api::CANCEL, x, y, 0, 0),
                    PointerKind::Pressed if p.button == MouseButton::Left => (api::PRESS, x, y, 0, mods),
                    PointerKind::Moved => (api::MOVE, x, y, 0, mods),
                    PointerKind::Released if p.button == MouseButton::Left => (api::RELEASE, x, y, 0, mods),
                    PointerKind::Wheel if p.delta_y != 0.0 => (api::WHEEL, x, y, if p.delta_y > 0.0 { 120 } else { -120 }, 0),
                    PointerKind::Pressed | PointerKind::Released => return (true, false),
                    _ => return (false, false),
                }
            }
            // Keys go to the window that has the focus, at the pointer.
            SurfaceInput::Key { down, key, modifiers } => {
                let vk = if *down { api::virtual_key(*key) } else { 0 };
                let [cx, cy] = session.scene.state.cursor;
                (
                    if vk != 0 { api::KEY } else { api::REFRESH },
                    cx as f64,
                    cy as f64,
                    vk,
                    api::modifiers(*modifiers),
                )
            }
            _ => return (false, false),
        };
        if session.input(kind, x, y, value, mods) {
            return (true, true);
        }
        schedule_redraw(self.scope.as_ref());
        (true, false)
    }
}

#[cfg(target_os = "macos")]
fn initial_monitor_position(viewport: SurfaceViewport, scale: f64) -> std::result::Result<(i32, i32), String> {
    if !scale.is_finite() || scale <= 0.0 {
        return Err("invalid initial selection window scale".to_owned());
    }
    let coordinate = |value: f64| {
        let value = (value / scale).round();
        (value.is_finite() && value >= i32::MIN as f64 && value <= i32::MAX as f64)
            .then_some(value as i32)
            .ok_or_else(|| "invalid initial selection window position".to_owned())
    };
    Ok((coordinate(viewport.frame[0])?, coordinate(viewport.frame[1])?))
}

fn place_monitor(handle: isize, monitor: OverlayMonitor, viewport: SurfaceViewport, show: bool) -> std::result::Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        native::place_mapped_checked(handle, monitor.mapping, monitor.scale, viewport.frame, show).map_err(|error| error.to_string())
    }
    #[cfg(windows)]
    {
        let _ = viewport;
        native::place(handle, monitor.rect, show);
        Ok(())
    }
}

/// Resolves a still-owned overlay window at the time a queued action runs.
fn window_geometry(id: u64) -> Option<(isize, OverlayMonitor, SurfaceViewport)> {
    HOST.with(|h| {
        let host = h.try_borrow().ok()?;
        let scope = host.scope.as_ref()?;
        let window = host.windows.iter().find(|window| window.id == id)?;
        let handle = window.hwnd(scope);
        (handle != 0).then(|| (handle, window.monitor(), window.viewport))
    })
}

fn fail_window_placement(window_id: u64, error: String) {
    let session = HOST.with(|host| {
        let host = host.try_borrow().ok()?;
        let window = host.windows.iter().find(|window| window.id == window_id)?;
        host.session.as_ref().filter(|session| session.monitors.iter().any(|monitor| monitor.rect == window.rect)).map(|session| session.id)
    });
    if let Some(id) = session {
        fail_overlay(id, format!("could not place a selection window: {error}"));
    } else {
        eprintln!("overlay: could not place a prepared window: {error}");
    }
}

/// Redraws every window once the input queued now is in: the pointer can
/// report many moves per frame.
fn schedule_redraw(scope: Option<&AppScope>) {
    if REDRAW_POSTED.with(|p| p.replace(true)) {
        return;
    }
    let posted = scope.is_some_and(|scope| scope.post(redraw).is_ok());
    if !posted {
        REDRAW_POSTED.with(|p| p.set(false));
    }
}

fn redraw() {
    REDRAW_POSTED.with(|p| p.set(false));
    let (busy, failure) = HOST.with(|h| match h.try_borrow_mut() {
        Ok(mut host) => {
            let failure = host.draw().err().and_then(|error| {
                host.session.as_ref().map(|session| (session.id, format!("could not draw the selection overlay: {error}")))
            });
            (None, failure)
        }
        Err(_) => (h.try_borrow().ok().and_then(|host| host.scope()), None),
    });
    if let Some((id, message)) = failure {
        fail_overlay(id, message);
    }
    if let Some(scope) = busy {
        schedule_redraw(Some(&scope));
    }
}

fn fail_overlay(id: u64, message: String) {
    eprintln!("overlay: {message}");
    api::fail(id, message);
    end_matching(Some(id));
}

/// The input surface's handler for the window over `rect`.
fn on_input(rect: Rect, input: &SurfaceInput) -> bool {
    let (handled, done) = HOST.with(|h| match h.try_borrow_mut() {
        Ok(mut host) => host.input(rect, input),
        Err(_) => (false, false),
    });
    if done {
        // The selection is made: hide at once.
        end();
    }
    handled
}

fn parse_mapping(value: &Value, rect: Rect, width: u32, height: u32) -> Option<CaptureMapping> {
    let Some(bounds) = value.get("bounds") else {
        return CaptureMapping::new(rect, width, height);
    };
    let bounds = bounds.as_array()?;
    if bounds.len() != 4 {
        return None;
    }
    let edges = [bounds.first()?, bounds.get(1)?, bounds.get(2)?, bounds.get(3)?].map(Value::as_f64);
    CaptureMapping::with_desktop_bounds(rect, width, height, [edges[0]?, edges[1]?, edges[2]?, edges[3]?])
}

/// The announcement contains precise desktop bounds, native image size and
/// desktop scale for each monitor.
fn parse_announcement(json: &str) -> Option<(u64, Vec<OverlayMonitor>, (i32, i32))> {
    let root: Value = serde_json::from_str(json).ok()?;
    let id = root.get("id")?.as_u64()?;
    let int = |v: &Value| i32::try_from(v.as_i64()?).ok();
    let monitors = root
        .get("monitors")?
        .as_array()?
        .iter()
        .map(|m| {
            let r = m.get("rect")?.as_array()?;
            if r.len() != 4 {
                return None;
            }
            let [x, y, w, h] = [r.first()?, r.get(1)?, r.get(2)?, r.get(3)?].map(int);
            let rect = Rect::new(x?, y?, w?, h?);
            rect.x.checked_add(rect.width)?;
            rect.y.checked_add(rect.height)?;
            let pixels = m.get("pixels")?.as_array()?;
            if pixels.len() != 2 {
                return None;
            }
            let width = u32::try_from(pixels.first()?.as_u64()?).ok()?;
            let height = u32::try_from(pixels.get(1)?.as_u64()?).ok()?;
            let mapping = parse_mapping(m, rect, width, height)?;
            let size = (width as usize).checked_mul(height as usize)?.checked_mul(4)?;
            if size > isize::MAX as usize {
                return None;
            }
            let scale = m.get("scale")?.as_f64().filter(|scale| scale.is_finite() && *scale > 0.0)?;
            Some(OverlayMonitor {
                rect,
                scale,
                pixels: (width, height),
                mapping,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let cursor = root.get("cursor")?.as_array()?;
    if cursor.len() != 2 {
        return None;
    }
    Some((id, monitors, (int(cursor.first()?)?, int(cursor.get(1)?)?)))
}

/// The announcement JSON, or empty to take the overlay down.
fn apply(json: &str) {
    if json.is_empty() {
        end();
        return;
    }
    let Some((id, monitors, cursor)) = parse_announcement(json) else {
        if let Some(id) = serde_json::from_str::<Value>(json).ok().and_then(|root| root.get("id")?.as_u64()) {
            fail_overlay(id, "invalid selection overlay display information".into());
        } else {
            eprintln!("overlay: an invalid announcement without a session ID");
        }
        return;
    };
    if !api::pending(id) {
        return;
    }
    if monitors.is_empty() {
        fail_overlay(id, "no displays are available for the selection overlay".into());
        return;
    }
    end();
    let shown = HOST.with(|h| -> std::result::Result<Option<AppScope>, String> {
        let mut host = h.try_borrow_mut().map_err(|_| "the selection overlay is busy".to_owned())?;
        let scope = host.scope().ok_or_else(|| "the selection overlay is not initialized".to_owned())?;
        let mut session = Session {
            id,
            monitors: monitors.clone(),
            scene: Scene::default(),
        };
        session.scene.help = api::text(id, api::TEXT_HELP)?.split('\n').map(str::to_owned).collect();
        if session.input(api::REFRESH, cursor.0 as f64, cursor.1 as f64, 0, 0) {
            return Ok(None);
        }
        host.session = Some(session);
        for &monitor in &monitors {
            let i = host.window_for(&scope, monitor)
                .map_err(|error| format!("could not create the selection window for {:?}: {error:?}", monitor.rect))?;
            if host.windows[i].hwnd(&scope) == 0 {
                return Err(format!("the native selection window for {:?} is unavailable", monitor.rect));
            }
            match host.windows[i].load(id) {
                Ok(true) => {}
                Ok(false) => return Ok(None),
                Err(error) => return Err(format!("could not load the selection screenshot: {error:?}")),
            }
        }
        // Windows of monitors that are gone.
        host.windows.retain(|w| {
            let keep = monitors.iter().any(|m| m.rect == w.rect);
            if !keep {
                w.close();
            }
            keep
        });
        host.draw().map_err(|error| format!("could not draw the selection overlay: {error}"))?;
        Ok(Some(scope))
    });
    let scope = match shown {
        Ok(Some(scope)) => scope,
        Ok(None) => {
            end_matching(Some(id));
            return;
        }
        Err(message) => {
            fail_overlay(id, message);
            return;
        }
    };
    // Show once the new screenshot is drawn, so the last one never flashes.
    let waiter = scope.clone();
    // Scope tasks are polled on the UI thread.
    let started = scope.spawn(async move {
        if let Ok(delay) = waiter.delay(FRAME_WAIT) {
            let _ = delay.await;
        }
        show(id, cursor);
    });
    if started.is_err() {
        show(id, cursor);
    }
}

/// Puts session `id`'s windows on screen, the one under the pointer with
/// the keyboard.
fn show(id: u64, cursor: (i32, i32)) {
    let Some(windows) = HOST.with(|h| {
        let host = h.try_borrow().ok()?;
        let session = host.session.as_ref().filter(|s| s.id == id && !s.done())?;
        let scope = host.scope()?;
        let mut windows = Vec::new();
        let mut focus = None;
        let mut fallback = None;
        for w in host.windows.iter().filter(|w| session.monitors.iter().any(|m| m.rect == w.rect)) {
            let hwnd = w.hwnd(&scope);
            if hwnd == 0 {
                return Some(Err(format!("the native selection window for {:?} is unavailable", w.rect)));
            }
            windows.push((hwnd, w.monitor(), w.viewport));
            if fallback.is_none() {
                fallback = Some((hwnd, w.input.clone()));
            }
            if w.rect.contains(cursor.0, cursor.1) {
                focus = Some((hwnd, w.input.clone()));
            }
        }
        if windows.is_empty() || session.monitors.iter().any(|monitor| !host.windows.iter().any(|window| window.rect == monitor.rect)) {
            return Some(Err("a selection window is missing for an available display".to_owned()));
        }
        Some(Ok((windows, focus.or(fallback))))
    }) else {
        return;
    };
    let (windows, focus) = match windows {
        Ok(windows) => windows,
        Err(message) => {
            fail_overlay(id, message);
            return;
        }
    };
    for (hwnd, monitor, viewport) in windows {
        if let Err(error) = place_monitor(hwnd, monitor, viewport, true) {
            fail_overlay(id, format!("could not place the selection window: {error}"));
            return;
        }
    }
    if let Some((hwnd, input)) = focus {
        native::activate(hwnd);
        if let Err(error) = input.focus() {
            eprintln!("overlay: could not focus the selection window: {error:?}");
        }
    }
    api::shown(id);
}

/// Takes the overlay down (the windows stay, hidden).
fn end() {
    end_matching(None);
}

fn end_matching(id: Option<u64>) {
    let hwnds = HOST.with(|h| {
        let Ok(mut host) = h.try_borrow_mut() else { return Vec::new() };
        if id.is_some_and(|id| host.session.as_ref().is_none_or(|session| session.id != id)) {
            return Vec::new();
        }
        if host.session.take().is_none() {
            return Vec::new();
        }
        let Some(scope) = host.scope() else { return Vec::new() };
        host.windows
            .iter_mut()
            .map(|w| {
                let _ = w.unload();
                w.hwnd(&scope)
            })
            .collect()
    });
    for hwnd in hwnds {
        native::hide(hwnd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn initial_selection_position_uses_desktop_points() {
        let viewport = SurfaceViewport { frame: [-2562.0, -6.0, 0.0, 1600.0], content: [0.0, 0.0, 1281.0, 803.0] };
        assert_eq!(initial_monitor_position(viewport, 2.0).unwrap(), (-1281, -3));
        assert!(initial_monitor_position(viewport, 0.0).is_err());
        assert!(initial_monitor_position(viewport, -1.0).is_err());
        assert!(initial_monitor_position(viewport, f64::NAN).is_err());
        let outside = SurfaceViewport { frame: [f64::MAX, 0.0, f64::MAX, 10.0], ..viewport };
        assert!(initial_monitor_position(outside, 2.0).is_err());
    }

    #[test]
    fn announcement_requires_complete_native_monitor_dimensions() {
        let json = r#"{"id":42,"cursor":[-4,2],"monitors":[
            {"rect":[-6,0,6,4],"pixels":[3,2],"scale":2},
            {"rect":[0,0,4,4],"pixels":[4,4],"scale":2}
        ]}"#;
        let (id, monitors, cursor) = parse_announcement(json).unwrap();
        assert_eq!(id, 42);
        assert_eq!(cursor, (-4, 2));
        assert_eq!(monitors[0].rect, Rect::new(-6, 0, 6, 4));
        assert_eq!(monitors[0].pixels, (3, 2));
        assert_eq!(monitors[1].pixels, (4, 4));

        let mut root: Value = serde_json::from_str(json).unwrap();
        root["monitors"][1]["pixels"] = serde_json::json!([0, 4]);
        assert!(parse_announcement(&root.to_string()).is_none());
        root["monitors"][1]["pixels"] = serde_json::json!([4, 4]);
        root["monitors"][1]["scale"] = serde_json::json!(0);
        assert!(parse_announcement(&root.to_string()).is_none());
        root["monitors"][1]["scale"] = serde_json::json!(2);
        root["monitors"][1]["rect"] = serde_json::json!([2147483647_i64, 0, 4, 4]);
        assert!(parse_announcement(&root.to_string()).is_none());
    }

    #[test]
    fn overlay_and_screen_draw_preserve_precise_desktop_bounds() {
        let bounds = [0.5, -0.5, 4.5, 3.5];
        let mut monitor = serde_json::json!({
            "id": 8, "cursor": [1, 0],
            "monitors": [{"rect": [1, -1, 4, 5], "pixels": [4, 4], "bounds": bounds, "scale": 2}]
        });
        let (_, monitors, _) = parse_announcement(&monitor.to_string()).unwrap();
        assert_eq!(monitors[0].mapping.desktop_bounds(), bounds);
        assert_eq!(monitors[0].mapping.pixel_center(0, 0), (1.0, 0.0));
        monitor["monitors"][0]["bounds"] = serde_json::json!([0.4, -0.5, 4.5, 3.5]);
        assert!(parse_announcement(&monitor.to_string()).is_none());

        let mut draw = serde_json::json!({
            "id": 9, "area": [1, -1, 4, 5], "pixels": [4, 4], "bounds": bounds
        });
        let (_, mapping) = parse_draw_announcement(&draw.to_string()).unwrap();
        assert_eq!(mapping.desktop_bounds(), bounds);
        draw["bounds"] = serde_json::json!([0.5, -0.5, 0.5, 3.5]);
        assert!(parse_draw_announcement(&draw.to_string()).is_none());
    }

    #[test]
    fn invalid_or_missing_displays_fail_only_the_matching_overlay_session() {
        let announcement = api::demo(0);
        let (id, monitors, _) = parse_announcement(&announcement).unwrap();
        struct Demo(u64);
        impl Drop for Demo {
            fn drop(&mut self) {
                api::end_demo(self.0);
            }
        }
        let _demo = Demo(id);
        HOST.with(|host| {
            host.borrow_mut().session = Some(Session { id, monitors, scene: Scene::default() });
        });

        assert!(!api::fail(id + 1, "stale failure".into()));
        assert!(api::pending(id));
        let mut stale: Value = serde_json::from_str(&announcement).unwrap();
        stale["id"] = serde_json::json!(id + 1);
        apply(&stale.to_string());
        stale["monitors"][0]["pixels"] = serde_json::json!([0, 1]);
        apply(&stale.to_string());
        assert!(HOST.with(|host| host.borrow().session.as_ref().is_some_and(|session| session.id == id)));
        assert!(api::pending(id));
        stale["id"] = serde_json::json!(id);
        apply(&stale.to_string());
        assert!(!api::pending(id));
        assert!(HOST.with(|host| host.borrow().session.is_none()));
        assert!(!api::fail(id, "later failure".into()));

        let next = api::demo(0);
        let (next_id, _, _) = parse_announcement(&next).unwrap();
        let _next_demo = Demo(next_id);
        apply(&format!(r#"{{"id":{next_id},"monitors":[],"cursor":[0,0]}}"#));
        assert!(!api::pending(next_id));
    }

    #[test]
    fn screen_draw_announcement_requires_valid_recording_dimensions() {
        let json = r#"{"id":7,"area":[-400,30,400,200],"pixels":[200,100]}"#;
        let (id, mapping) = parse_draw_announcement(json).unwrap();
        assert_eq!(id, 7);
        assert_eq!(mapping.rect(), Rect::new(-400, 30, 400, 200));
        assert_eq!(mapping.size(), (200, 100));
        let mut root: Value = serde_json::from_str(json).unwrap();
        root["pixels"] = serde_json::json!([0, 100]);
        assert!(parse_draw_announcement(&root.to_string()).is_none());
        root["pixels"] = serde_json::json!([200, 100]);
        root["area"] = serde_json::json!([2147483647_i64, 0, 4, 4]);
        assert!(parse_draw_announcement(&root.to_string()).is_none());
        root["area"] = serde_json::json!([-400, 30, 400, 200]);
        root["pixels"] = serde_json::json!([2147483647_i64, 2147483647_i64]);
        assert!(parse_draw_announcement(&root.to_string()).is_none());
    }

    #[test]
    fn screen_draw_pointer_uses_the_preview_and_recording_sizes() {
        assert_eq!(draw_pointer_point(50.0, 25.0, (200.0, 100.0), (100, 80)), Some((25.0, 20.0)));
        assert_eq!(draw_pointer_point(101.25, 30.0, (202.5, 120.0), (300, 180)), Some((150.0, 45.0)));
        assert_eq!(draw_pointer_point(-5.0, 110.0, (200.0, 100.0), (100, 80)), Some((-2.5, 88.0)));
        assert!(draw_pointer_point(1.0, 1.0, (0.0, 100.0), (100, 80)).is_none());
        assert!(draw_pointer_point(f64::NAN, 1.0, (200.0, 100.0), (100, 80)).is_none());
        let viewport = SurfaceViewport {
            frame: [-2.0, -4.0, 204.0, 100.0],
            content: [0.25, 1.5, 100.0, 50.0],
        };
        assert_eq!(draw_viewport_point(50.25, 26.5, viewport, (200, 100)), Some((100.0, 50.0)));
        assert_eq!(draw_viewport_point(0.0, 1.0, viewport, (200, 100)), Some((-0.5, -1.0)));
    }
}

/// Makes (or keeps) a hidden window per screen, so the first capture is
/// quick too.
fn prepare() {
    #[cfg(target_os = "macos")]
    let layout = match jiffshot_capture::screen::DisplayLayout::current() {
        Ok(layout) => layout,
        Err(error) => {
            eprintln!("overlay: could not prepare selection windows: {error}");
            return;
        }
    };
    #[cfg(target_os = "macos")]
    let monitors = layout.monitors().to_vec();
    #[cfg(windows)]
    let monitors = jiffshot_capture::screen::monitors();
    HOST.with(|h| {
        let Ok(mut host) = h.try_borrow_mut() else { return };
        let Some(scope) = host.scope() else { return };
        for m in monitors {
            #[cfg(target_os = "macos")]
            let mapping = layout.monitor_mapping(&m);
            #[cfg(windows)]
            let mapping = CaptureMapping::identity(m.rect)
                .ok_or_else(|| std::io::Error::other("invalid monitor dimensions"));
            let mapping = match mapping {
                Ok(mapping) => mapping,
                Err(error) => {
                    eprintln!("overlay: no window for {:?}: {error}", m.rect);
                    continue;
                }
            };
            #[cfg(target_os = "macos")]
            let scale = layout.desktop_scale();
            #[cfg(windows)]
            let scale = m.desktop_scale();
            if let Err(error) = host.window_for(
                &scope,
                OverlayMonitor {
                    rect: m.rect,
                    scale,
                    pixels: mapping.size(),
                    mapping,
                },
            ) {
                eprintln!("overlay: no window for {:?}: {error:?}", m.rect);
            }
        }
    });
}

// ---- Screen Draw ------------------------------------------------------------------

/// Screen Draw: a see-through window over the area being recorded, showing
/// the marks the capture side renders and sending it the pointer and keys.
/// It is left out of captures; the recorder draws the marks into the video.
struct ScreenDraw {
    id: u64,
    mapping: CaptureMapping,
    viewport: SurfaceViewport,
    #[cfg(target_os = "macos")]
    scale: f64,
    window: Window,
    surface: PixelSurface,
    /// The marks, BGRA, at the recording's size.
    pixels: Vec<u8>,
    /// Cleared when the window goes, which ends the fade ticks.
    alive: Arc<AtomicBool>,
}

impl ScreenDraw {
    /// Copies the marks onto the window.
    fn refresh(&mut self) -> std::result::Result<bool, String> {
        let (w, h) = self.mapping.size();
        self.pixels.resize(w as usize * h as usize * 4, 0);
        if !api::draw_copy(self.id, &mut self.pixels, w, h)? {
            return Ok(false);
        }
        self
            .surface
            .set_layer(0, w, h, PixelFormat::Bgra8)
            .and_then(|_| self.surface.write_layer(0, 0, 0, w, h, &self.pixels, w as usize * 4))
            .and_then(|_| self.surface.place_layer(0, self.viewport.placement(), Interpolation::None, 1.0))
            .map_err(|error| format!("could not display the Screen Draw preview: {error:?}"))?;
        Ok(true)
    }

    fn close(&self) {
        self.alive.store(false, Ordering::Release);
        let _ = self.surface.detach();
        let _ = self.window.close();
    }
}

#[cfg(any(test, target_os = "macos"))]
fn draw_pointer_point(x: f64, y: f64, view: (f64, f64), pixels: (u32, u32)) -> Option<(f64, f64)> {
    if !x.is_finite() || !y.is_finite() || !view.0.is_finite() || !view.1.is_finite() || view.0 <= 0.0 || view.1 <= 0.0 {
        return None;
    }
    let point = (x * f64::from(pixels.0) / view.0, y * f64::from(pixels.1) / view.1);
    (point.0.is_finite() && point.1.is_finite() && point.0.abs() <= f32::MAX as f64 && point.1.abs() <= f32::MAX as f64)
        .then_some(point)
}

#[cfg(any(test, target_os = "macos"))]
fn draw_viewport_point(x: f64, y: f64, viewport: SurfaceViewport, pixels: (u32, u32)) -> Option<(f64, f64)> {
    let [left, top, width, height] = viewport.content;
    draw_pointer_point(x - left, y - top, (width, height), pixels)
}

/// Screen Draw's input; true when handled.
fn on_draw_input(id: u64, input: &SurfaceInput) -> bool {
    let mut cancel = false;
    let mut ended = false;
    let mut failure = None;
    let handled = DRAW.with(|d| {
        let Ok(mut current) = d.try_borrow_mut() else { return false };
        let Some(draw) = current.as_mut().filter(|d| d.id == id) else { return false };
        let changed = match input {
            SurfaceInput::Pointer(p) => {
                #[cfg(windows)]
                let s = draw.surface.metrics().map(|m| m.scaling).unwrap_or(1.0);
                #[cfg(windows)]
                let (x, y) = (p.x * s, p.y * s);
                #[cfg(target_os = "macos")]
                let (x, y) = {
                    let metrics = match draw.surface.metrics() {
                        Ok(metrics) => metrics,
                        Err(error) => {
                            failure = Some(format!("could not read Screen Draw surface geometry: {error:?}"));
                            return true;
                        }
                    };
                    let [left, top, right, bottom] = draw.viewport.frame;
                    if jiffshot_capture::screen::scale_factor() != draw.scale
                        || (metrics.width - (right - left) / draw.scale).abs() > 0.01
                        || (metrics.height - (bottom - top) / draw.scale).abs() > 0.01
                    {
                        failure = Some("the Screen Draw window geometry changed".to_owned());
                        return true;
                    }
                    let Some(point) = draw_viewport_point(p.x, p.y, draw.viewport, draw.mapping.size()) else {
                        failure = Some("invalid Screen Draw surface geometry".to_owned());
                        return true;
                    };
                    point
                };
                match p.kind {
                    PointerKind::Pressed if p.button == MouseButton::Right => {
                        cancel = true;
                        api::draw_input(id, api::CANCEL, x, y, 0)
                    }
                    PointerKind::Pressed => api::draw_input(id, api::PRESS, x, y, 0),
                    PointerKind::Moved => api::draw_input(id, api::MOVE, x, y, 0),
                    PointerKind::Released if p.button == MouseButton::Left => api::draw_input(id, api::RELEASE, x, y, 0),
                    _ => return false,
                }
            }
            SurfaceInput::Key { down: true, key, .. } => match api::virtual_key(*key) {
                0 => return false,
                vk => {
                    api::draw_input(id, api::KEY, 0.0, 0.0, vk);
                    true
                }
            },
            _ => return false,
        };
        if changed && !cancel {
            match draw.refresh() {
                Ok(true) => {}
                Ok(false) => ended = true,
                Err(error) => failure = Some(error),
            }
        }
        true
    });
    if let Some(message) = failure {
        fail_draw(id, message);
    } else if cancel || ended {
        close_draw_matching(Some(id));
    }
    handled
}

/// Input for the overlay showing (desktop pixels), as its windows would send
/// it, for the UI self-test; the new state, or `None` with no overlay.
pub(super) fn feed(kind: i32, x: f64, y: f64, value: i32, mods: i32) -> Option<api::OverlayState> {
    let (id, state, scope, refresh) = HOST.with(|h| {
        let mut host = h.try_borrow_mut().ok()?;
        let scope = host.scope();
        let session = host.session.as_mut()?;
        session.input(kind, x, y, value, mods);
        let refresh = session.refresh();
        Some((session.id, session.scene.state, scope, refresh))
    })?;
    if let Err(error) = refresh {
        fail_overlay(id, format!("could not draw the selection overlay: {error}"));
        return None;
    }
    schedule_redraw(scope.as_ref());
    Some(state)
}

#[cfg(target_os = "macos")]
pub(super) fn guides_for_selftest() -> Option<Vec<(Rect, (i32, i32))>> {
    HOST.with(|host| host.try_borrow().ok()?.session.as_ref().map(|session| session.scene.guides.clone()))
}

#[cfg(target_os = "macos")]
pub(super) fn viewport_for_selftest(rect: Rect) -> Option<([f64; 4], [f64; 4])> {
    HOST.with(|host| {
        let host = host.try_borrow().ok()?;
        let viewport = host.windows.iter().find(|window| window.rect == rect)?.viewport;
        Some((viewport.frame, viewport.content))
    })
}

#[cfg(target_os = "macos")]
pub(super) struct SelectionWindowForSelftest {
    pub number: u32,
    pub bounds: [f64; 4],
    pub viewport: ([f64; 4], [f64; 4]),
    pub surface: (f64, f64),
    pub scale: f64,
}

#[cfg(target_os = "macos")]
pub(super) fn windows_for_selftest() -> std::result::Result<Vec<SelectionWindowForSelftest>, String> {
    HOST.with(|host| {
        let host = host.try_borrow().map_err(|_| "the selection overlay is busy")?;
        let session = host.session.as_ref().ok_or("the selection overlay is not active")?;
        let scope = host.scope.as_ref().ok_or("the selection overlay is not initialized")?;
        session.monitors.iter().map(|monitor| {
            let window = host.windows.iter().find(|window| window.rect == monitor.rect)
                .ok_or("a selection window is missing")?;
            let number = crate::mac_window::number(window.hwnd(scope)).ok_or("the selection window is unavailable")?;
            let metrics = window.input.metrics().map_err(|error| format!("could not read selection surface metrics: {error:?}"))?;
            Ok(SelectionWindowForSelftest {
                number,
                bounds: monitor.mapping.desktop_bounds(),
                viewport: (window.viewport.frame, window.viewport.content),
                surface: (metrics.width, metrics.height),
                scale: monitor.scale,
            })
        }).collect()
    })
}

#[cfg(target_os = "macos")]
pub(super) fn cursor_for_selftest() -> Option<[i32; 2]> {
    HOST.with(|host| host.try_borrow().ok()?.session.as_ref().map(|session| session.scene.state.cursor))
}

#[cfg(target_os = "macos")]
pub(super) fn draw_viewport_for_selftest(id: u64) -> Option<([f64; 4], [f64; 4])> {
    DRAW.with(|draw| {
        let draw = draw.try_borrow().ok()?;
        let viewport = draw.as_ref().filter(|draw| draw.id == id)?.viewport;
        Some((viewport.frame, viewport.content))
    })
}

#[cfg(target_os = "macos")]
pub(super) fn draw_surface_for_selftest(id: u64) -> std::result::Result<(f64, f64, f64), String> {
    DRAW.with(|draw| {
        let draw = draw.try_borrow().map_err(|_| "the Screen Draw window is busy")?;
        let draw = draw.as_ref().filter(|draw| draw.id == id).ok_or("the Screen Draw window is unavailable")?;
        let metrics = draw.surface.metrics().map_err(|error| format!("could not read Screen Draw surface metrics: {error:?}"))?;
        Ok((metrics.width, metrics.height, draw.scale))
    })
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
pub(super) struct GuideVisual {
    pub monitor: Rect,
    pub scale: f64,
    pub bounds: [f64; 4],
    pub label: Option<String>,
    pub visible: bool,
}

#[cfg(target_os = "macos")]
pub(super) fn rendered_guides_for_selftest(index: usize) -> std::result::Result<Vec<GuideVisual>, String> {
    HOST.with(|host| {
        let host = host.try_borrow().map_err(|_| "the selection overlay is busy")?;
        let session = host.session.as_ref().ok_or("the selection overlay is not active")?;
        host.windows.iter()
            .filter(|window| session.monitors.iter().any(|monitor| monitor.rect == window.rect))
            .map(|window| window.guide_for_selftest(index)
                .map_err(|error| format!("{error:?}"))?
                .ok_or_else(|| "the guide has not been drawn".to_owned()))
            .collect()
    })
}

/// Input for Screen Draw (recording pixels), for
/// the UI self-test.
pub(super) fn feed_draw(kind: i32, x: f64, y: f64, value: i32) {
    let mut failure = None;
    let mut ended = None;
    DRAW.with(|d| {
        if let Ok(mut current) = d.try_borrow_mut() {
            if let Some(draw) = current.as_mut() {
                if api::draw_input(draw.id, kind, x, y, value) || kind == api::KEY {
                    match draw.refresh() {
                        Ok(true) => {}
                        Ok(false) => ended = Some(draw.id),
                        Err(error) => failure = Some((draw.id, error)),
                    }
                }
            }
        }
    });
    if let Some((id, message)) = failure {
        fail_draw(id, message);
    } else if let Some(id) = ended {
        close_draw_matching(Some(id));
    }
}

fn close_draw() {
    close_draw_matching(None);
}

fn close_draw_matching(id: Option<u64>) {
    if let Some(draw) = DRAW.with(|d| {
        let mut current = d.try_borrow_mut().ok()?;
        if current.as_ref().is_some_and(|draw| id.is_none_or(|id| id == draw.id)) {
            current.take()
        } else {
            None
        }
    }) {
        draw.close();
    }
}

fn fail_draw(id: u64, message: String) {
    if api::draw_failed(id, message.clone()) {
        eprintln!("screen draw: {message}");
    }
    close_draw_matching(Some(id));
}

fn parse_draw_announcement(json: &str) -> Option<(u64, CaptureMapping)> {
    let root: Value = serde_json::from_str(json).ok()?;
    let id = root.get("id")?.as_u64()?;
    let area = root.get("area")?.as_array()?;
    let pixels = root.get("pixels")?.as_array()?;
    if area.len() != 4 || pixels.len() != 2 {
        return None;
    }
    let int = |v: &Value| i32::try_from(v.as_i64()?).ok();
    let [x, y, w, h] = [area.first()?, area.get(1)?, area.get(2)?, area.get(3)?].map(int);
    let rect = Rect::new(x?, y?, w?, h?);
    rect.x.checked_add(rect.width)?;
    rect.y.checked_add(rect.height)?;
    let width = u32::try_from(pixels.first()?.as_u64()?).ok()?;
    let height = u32::try_from(pixels.get(1)?.as_u64()?).ok()?;
    let mapping = parse_mapping(&root, rect, width, height)?;
    let size = (width as usize).checked_mul(height as usize)?.checked_mul(4)?;
    (size <= isize::MAX as usize).then_some((id, mapping))
}

/// A mapped recording area shows Screen Draw; an empty announcement ends it.
fn apply_draw(scope: &AppScope, json: &str) {
    if json.is_empty() {
        let Ok(still_active) = DRAW.with(|d| d.try_borrow().map(|current| current.as_ref().is_some_and(|draw| api::draw_pending(draw.id)))) else {
            eprintln!("screen draw: the window is busy");
            return;
        };
        if !still_active {
            close_draw();
        }
        return;
    }
    let Some((id, mapping)) = parse_draw_announcement(json) else {
        if let Some(id) = serde_json::from_str::<Value>(json).ok().and_then(|root| root.get("id")?.as_u64()) {
            fail_draw(id, "invalid Screen Draw geometry".to_owned());
        } else {
            eprintln!("screen draw: an invalid announcement without a session ID");
        }
        return;
    };
    if !api::draw_pending(id) {
        return;
    }
    let Ok(already_shown) = DRAW.with(|d| d.try_borrow().map(|current| current.as_ref().is_some_and(|draw| draw.id == id))) else {
        fail_draw(id, "the Screen Draw window is busy".to_owned());
        return;
    };
    if already_shown {
        return;
    }
    close_draw();
    if let Err(error) = open_draw(scope, id, mapping) {
        fail_draw(id, error);
    }
}

fn open_draw(scope: &AppScope, id: u64, mapping: CaptureMapping) -> std::result::Result<(), String> {
    let area = mapping.rect();
    let (_, scale) = native::screen_of(area);
    let viewport = SurfaceViewport::new(mapping, scale)?;
    let (surface, window) = (|| -> Result<_> {
        let surface = scope.create_pixel_surface(move |input| Ok(on_draw_input(id, input)))?;
        #[cfg(windows)]
        surface.set_background(Some(DRAW_BACKGROUND))?;
        #[cfg(target_os = "macos")]
        surface.set_background(Some(0x00FF_FFFF))?;
        surface.set_cursor(Cursor::Cross)?;
        let [left, top, right, bottom] = viewport.frame;
        let window = Window::new()?
            .title("Jiffshot Screen Draw")?
            .window_decorations(WindowDecorations::None)?
            .show_in_taskbar(false)?
            .can_resize(false)?
            .width((right - left) / scale)?
            .height((bottom - top) / scale)?
            .position((area.x, area.y))?
            .background(Brush::solid(Color::new(0, 0, 0, 0)))?
            .content(Some(surface.control()))?;
        scope.set_transparent(&window, true)?;
        scope.set_topmost(&window, true)?;
        Ok((surface, window))
    })().map_err(|error| format!("could not create the Screen Draw window: {error:?}"))?;
    #[cfg(target_os = "macos")]
    let protected = match native::show_protected(scope, &window, false) {
        Ok(number) => number,
        Err(error) => {
            let _ = surface.detach();
            return Err(format!("could not protect the Screen Draw window: {error}"));
        }
    };
    #[cfg(windows)]
    if let Err(error) = window.show() {
        let _ = surface.detach();
        return Err(format!("could not show the Screen Draw window: {error:?}"));
    }
    let hwnd = native::handle(scope, &window);
    if hwnd == 0 {
        let _ = surface.detach();
        let _ = window.close();
        return Err("the native Screen Draw window is unavailable".to_owned());
    }
    #[cfg(windows)]
    {
        native::place(hwnd, area, false);
        native::exclude_from_capture(scope, &window);
    }
    #[cfg(target_os = "macos")]
    let reveal_window = window.clone();
    let alive = Arc::new(AtomicBool::new(true));
    let mut draw = ScreenDraw {
        id,
        mapping,
        viewport,
        #[cfg(target_os = "macos")]
        scale,
        window,
        surface: surface.clone(),
        pixels: Vec::new(),
        alive: alive.clone(),
    };
    match draw.refresh() {
        Ok(true) => {}
        Ok(false) => {
            draw.close();
            return Ok(());
        }
        Err(error) => {
            draw.close();
            return Err(error);
        }
    }
    #[cfg(windows)]
    {
        native::activate(hwnd);
        if let Err(error) = surface.focus() {
            draw.close();
            return Err(format!("could not focus the Screen Draw window: {error:?}"));
        }
    }
    let mut pending = Some(draw);
    let installed = DRAW.with(|d| {
        if let Ok(mut d) = d.try_borrow_mut() {
            *d = pending.take();
            true
        } else {
            false
        }
    });
    if !installed {
        if let Some(draw) = pending {
            draw.close();
        }
        return Err("the Screen Draw window is busy".to_owned());
    }
    #[cfg(target_os = "macos")]
    {
        let focus_scope = scope.clone();
        let focus_window = reveal_window.clone();
        let focus_surface = surface.clone();
        let position_scope = scope.clone();
        let position_window = reveal_window.clone();
        if let Err(error) = native::reveal_when_excluded(
            scope,
            &reveal_window,
            protected,
            false,
            move || {
                let hwnd = native::handle(&position_scope, &position_window);
                if hwnd == 0 {
                    return Err("the native Screen Draw window is unavailable".to_owned());
                }
                if jiffshot_capture::screen::scale_factor() != scale {
                    return Err("the display scale changed before Screen Draw was shown".to_owned());
                }
                native::place_mapped_checked(hwnd, mapping, scale, viewport.frame, false).map_err(|error| error.to_string())
            },
            move || {
                crate::mac_window::activate(native::handle(&focus_scope, &focus_window))?;
                focus_surface.focus().map_err(|error| format!("could not focus the Screen Draw window: {error:?}"))
            },
            move |error| fail_draw(id, error),
        ) {
            close_draw_matching(Some(id));
            return Err(format!("could not schedule the Screen Draw window: {error}"));
        }
    }
    // Re-render fading marks while the window is up.
    let ticker = scope.clone();
    let spawned = scope.spawn(async move {
        while alive.load(Ordering::Acquire) {
            match ticker.delay(FADE_TICK) {
                Ok(delay) => {
                    let _ = delay.await;
                }
                Err(error) => {
                    fail_draw(id, format!("could not schedule Screen Draw updates: {error:?}"));
                    break;
                }
            }
            if let Err(error) = ticker.post(move || {
                let mut ended = false;
                let mut failure = None;
                DRAW.with(|d| {
                    if let Ok(mut d) = d.try_borrow_mut() {
                        if let Some(draw) = d.as_mut().filter(|d| d.id == id) {
                            if api::draw_tick(id) {
                                match draw.refresh() {
                                    Ok(true) => {}
                                    Ok(false) => ended = true,
                                    Err(error) => failure = Some(error),
                                }
                            }
                        }
                    }
                });
                if let Some(message) = failure {
                    fail_draw(id, message);
                } else if ended {
                    close_draw_matching(Some(id));
                }
            }) {
                fail_draw(id, format!("could not update Screen Draw: {error:?}"));
                break;
            }
        }
    });
    if let Err(error) = spawned {
        close_draw_matching(Some(id));
        return Err(format!("could not start Screen Draw updates: {error:?}"));
    }
    Ok(())
}

/// Starts following the main view model's signals. Called once, on the UI
/// thread, after the main window is mounted.
pub fn install(scope: &AppScope) -> Result<()> {
    HOST.with(|h| h.borrow_mut().scope = Some(scope.clone()));
    let sink = crate::ui::main().sink;
    // Straight away (this is the UI thread): the overlay should come up as
    // fast as it can.
    Effect::new(move || {
        let json = sink.overlay.get();
        apply(&json);
        Ok(())
    })?;
    let draw_scope = scope.clone();
    Effect::new(move || {
        let json = sink.screen_draw.get();
        let scope = draw_scope.clone();
        draw_scope.post(move || apply_draw(&scope, &json))
    })?;
    // The overlay's windows, made while idle so the first capture is quick.
    if std::env::var_os("JIFFSHOT_UITEST").is_none() {
        scope.post(prepare)?;
    }
    Ok(())
}
