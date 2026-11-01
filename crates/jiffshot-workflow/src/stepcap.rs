//! Step Capture: records a screenshot at each click, with a description of
//! what was clicked (from UI Automation), and assembles a step-by-step
//! guide: a numbered list of zoomed screenshots with the click marked and
//! editable captions. Typing is not recorded.

use crate::geometry::CaptureLayout;
use crate::hud;
use jiffshot_capture::{CaptureMapping, Frame, Rect};
use jiffshot_doc::object::{Kind, ShapeKind, Style};
use jiffshot_doc::stamps::Stamp;
use jiffshot_doc::text::Align;
use jiffshot_doc::{Box2, Document, Pt, Rgba};
use std::sync::Mutex;

/// One recorded click.
pub struct Step {
    pub frame: Frame,
    /// Where the frame sits on screen.
    pub area: Rect,
    /// The precise desktop-to-image mapping retained when the frame was captured.
    pub mapping: Option<CaptureMapping>,
    pub click: (i32, i32),
    #[cfg(target_os = "macos")]
    pub click_precise: Option<(f64, f64)>,
    pub right: bool,
    pub description: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ClickPosition {
    pub(crate) grid: (i32, i32),
    #[cfg(target_os = "macos")]
    pub(crate) precise: Option<(f64, f64)>,
    #[cfg(target_os = "macos")]
    logical: Option<(f64, f64)>,
}

impl ClickPosition {
    pub(crate) fn from_grid(grid: (i32, i32)) -> Self {
        Self {
            grid,
            #[cfg(target_os = "macos")]
            precise: None,
            #[cfg(target_os = "macos")]
            logical: None,
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn from_points(logical: (f64, f64), scale: f64) -> std::io::Result<Self> {
        if !scale.is_finite() || scale <= 0.0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid Step Capture display scale",
            ));
        }
        let precise = (logical.0 * scale, logical.1 * scale);
        let grid = crate::overlay::rounded_desktop_point(precise)?;
        Ok(Self {
            grid,
            precise: Some(precise),
            logical: Some(logical),
        })
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn reference(self) -> (f64, f64) {
        self.precise
            .unwrap_or((f64::from(self.grid.0), f64::from(self.grid.1)))
    }

    #[cfg(target_os = "macos")]
    fn logical(self, layout: &CaptureLayout) -> (f64, f64) {
        self.logical.unwrap_or((
            f64::from(self.grid.0) / layout.desktop_scale(),
            f64::from(self.grid.1) / layout.desktop_scale(),
        ))
    }
}

type QueuedClick = (std::io::Result<ClickPosition>, bool);

struct ClickQueue {
    pending: Vec<QueuedClick>,
    paused: bool,
}

static CLICKS: Mutex<ClickQueue> = Mutex::new(ClickQueue {
    pending: Vec::new(),
    paused: true,
});

#[cfg(target_os = "macos")]
#[path = "selftest_step_mac.rs"]
mod selftest;
#[cfg(target_os = "macos")]
pub(crate) use selftest::{fixture as selftest_fixture, run as selftest};

/// Queues a click unless recording is paused or it landed on one of our own
/// windows (the panel).
fn queue_click_if_active(
    point: std::io::Result<ClickPosition>,
    right: bool,
    ours: bool,
    active: impl FnOnce() -> bool,
) {
    if !ours {
        let mut clicks = CLICKS.lock().unwrap_or_else(|e| e.into_inner());
        if !clicks.paused && active() {
            clicks.pending.push((point, right));
        }
    }
}

#[cfg(any(windows, test))]
fn clicked(p: (i32, i32), right: bool, ours: bool) {
    queue_click_if_active(Ok(ClickPosition::from_grid(p)), right, ours, || true);
}

fn reset_clicks() {
    let mut clicks = CLICKS.lock().unwrap_or_else(|e| e.into_inner());
    clicks.pending.clear();
    clicks.paused = false;
}

fn pause_clicks(paused: bool) {
    CLICKS.lock().unwrap_or_else(|e| e.into_inner()).paused = paused;
}

fn drain_clicks() -> Vec<QueuedClick> {
    std::mem::take(&mut CLICKS.lock().unwrap_or_else(|e| e.into_inner()).pending)
}

#[cfg(windows)]
mod platform {
    use windows::Win32::Foundation::POINT as WPOINT;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation};
    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetAncestor, GetWindowThreadProcessId, SetWindowsHookExW,
        UnhookWindowsHookEx, WindowFromPoint, GA_ROOT, HC_ACTION, HHOOK, MSLLHOOKSTRUCT,
        WH_MOUSE_LL, WM_LBUTTONDOWN, WM_RBUTTONDOWN,
    };

    unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32
            && (wparam as u32 == WM_LBUTTONDOWN || wparam as u32 == WM_RBUTTONDOWN)
        {
            let info = &*(lparam as *const MSLLHOOKSTRUCT);
            let p = (info.pt.x, info.pt.y);
            let top = GetAncestor(WindowFromPoint(POINT { x: p.0, y: p.1 }), GA_ROOT);
            let mut process = 0;
            GetWindowThreadProcessId(top, &mut process);
            super::clicked(
                p,
                wparam as u32 == WM_RBUTTONDOWN,
                process == GetCurrentProcessId(),
            );
        }
        CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
    }

    /// Watches clicks (a low-level mouse hook, served by this thread's
    /// message loop) and describes what was clicked (UI Automation).
    pub struct Watcher {
        hook: HHOOK,
        automation: Option<IUIAutomation>,
    }

    impl Watcher {
        pub fn start() -> std::io::Result<Watcher> {
            // SAFETY: COM for UI Automation on this (the tray) thread, and a
            // hook removed on drop.
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                let automation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok();
                let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(hook), std::ptr::null_mut(), 0);
                if hook.is_null() {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(Watcher { hook, automation })
            }
        }

        /// The name and kind of the element at `p`.
        pub fn element_at(&self, p: (i32, i32)) -> (String, String) {
            element_at(self.automation.as_ref(), p)
        }
    }

    /// The name and kind of the UI Automation element at `p`.
    pub(crate) fn element_at(
        automation: Option<&IUIAutomation>,
        p: (i32, i32),
    ) -> (String, String) {
        let element =
            automation.and_then(|a| unsafe { a.ElementFromPoint(WPOINT { x: p.0, y: p.1 }).ok() });
        match &element {
            // SAFETY: plain property reads on a live element.
            Some(e) => unsafe {
                (
                    e.CurrentName().map(|s| s.to_string()).unwrap_or_default(),
                    e.CurrentLocalizedControlType()
                        .map(|s| s.to_string())
                        .unwrap_or_default(),
                )
            },
            None => (String::new(), String::new()),
        }
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            // SAFETY: removes the hook installed in `start`.
            unsafe { UnhookWindowsHookEx(self.hook) };
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_app_kit::{NSEvent, NSEventMask, NSEventType};
    use objc2_core_foundation::CGPoint;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::io;
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    thread_local! {
        static MONITORS: RefCell<HashMap<u64, Retained<AnyObject>>> = RefCell::new(HashMap::new());
    }

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventGetLocation(event: *const c_void) -> CGPoint;
    }

    fn remove(id: u64) {
        MONITORS.with(|monitors| {
            if let Some(monitor) = monitors.borrow_mut().remove(&id) {
                // SAFETY: this is the token returned by addGlobalMonitor, removed once.
                unsafe { NSEvent::removeMonitor(&monitor) };
            }
        });
    }

    fn install(id: u64, active: Arc<AtomicBool>, scale: f64) -> Result<(), String> {
        let handler = RcBlock::new(move |event: NonNull<NSEvent>| {
            if !active.load(Ordering::Acquire) {
                return;
            }
            // SAFETY: AppKit supplies a live event for the duration of this callback.
            let event = unsafe { event.as_ref() };
            let point = event
                .CGEvent()
                .ok_or_else(|| {
                    io::Error::other("a Step Capture mouse event had no Core Graphics event")
                })
                .and_then(|cg| {
                    // SAFETY: the retained Core Graphics event is live for this call.
                    let point = unsafe { CGEventGetLocation(Retained::as_ptr(&cg).cast()) };
                    super::ClickPosition::from_points((point.x, point.y), scale)
                });
            super::queue_click_if_active(
                point,
                event.r#type() == NSEventType::RightMouseDown,
                false,
                || active.load(Ordering::Acquire),
            );
        });
        let monitor = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(
            NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown,
            &handler,
        )
        .ok_or_else(|| "could not start the macOS global mouse monitor".to_owned())?;
        MONITORS.with(|monitors| {
            monitors.borrow_mut().insert(id, monitor);
        });
        Ok(())
    }

    /// AppKit's global monitor delivers other apps' mouse-down events
    /// without sampling or capturing clicks on Jiffshot's own panel.
    pub struct Watcher {
        id: u64,
        active: Arc<AtomicBool>,
    }

    impl Watcher {
        pub fn start(scale: f64) -> io::Result<Watcher> {
            if !jiffshot_capture::targets::request_accessibility() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Step Capture needs Accessibility permission to describe clicked controls. Allow Jiffshot in System Settings > Privacy & Security > Accessibility, then retry.",
                ));
            }
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let active = Arc::new(AtomicBool::new(true));
            if unsafe { libc::pthread_main_np() } != 0 {
                install(id, active.clone(), scale).map_err(io::Error::other)?;
                return Ok(Watcher { id, active });
            }
            enum Status {
                Pending,
                Ready(Result<(), String>),
                TimedOut,
            }
            let state = Arc::new((Mutex::new(Status::Pending), Condvar::new()));
            let main_state = state.clone();
            let main_active = active.clone();
            dispatch2::DispatchQueue::main().exec_async(move || {
                let result = install(id, main_active, scale);
                let (lock, wake) = &*main_state;
                let mut status = lock.lock().unwrap_or_else(|e| e.into_inner());
                match *status {
                    Status::Pending => *status = Status::Ready(result),
                    Status::TimedOut => {
                        if result.is_ok() {
                            remove(id);
                        }
                    }
                    Status::Ready(_) => unreachable!(),
                }
                wake.notify_all();
            });
            let (lock, wake) = &*state;
            let status = lock.lock().unwrap_or_else(|e| e.into_inner());
            let (mut status, _) = wake
                .wait_timeout_while(status, Duration::from_secs(2), |s| {
                    matches!(s, Status::Pending)
                })
                .unwrap_or_else(|e| e.into_inner());
            match std::mem::replace(&mut *status, Status::TimedOut) {
                Status::Ready(Ok(())) => Ok(Watcher { id, active }),
                Status::Ready(Err(error)) => Err(io::Error::other(error)),
                Status::Pending | Status::TimedOut => {
                    active.store(false, Ordering::Release);
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the macOS mouse monitor could not start on the main thread",
                    ))
                }
            }
        }
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            self.active.store(false, Ordering::Release);
            let id = self.id;
            if unsafe { libc::pthread_main_np() } != 0 {
                remove(id);
            } else {
                dispatch2::DispatchQueue::main().exec_async(move || remove(id));
            }
        }
    }
}

/// "Click the Save button", from what is at `p`.
#[cfg(windows)]
fn describe(watcher: &platform::Watcher, p: (i32, i32), right: bool) -> String {
    let (name, kind) = watcher.element_at(p);
    describe_element(name, kind, right)
}

/// "Click the Save button", from what UI Automation finds at `p`.
#[cfg(windows)]
pub(crate) fn describe_with(
    automation: Option<&windows::Win32::UI::Accessibility::IUIAutomation>,
    p: (i32, i32),
    right: bool,
) -> String {
    let (name, kind) = platform::element_at(automation, p);
    describe_element(name, kind, right)
}

fn describe_element(name: String, kind: String, right: bool) -> String {
    let verb = if right { "Right-click" } else { "Click" };
    let name: String = name.trim().chars().take(60).collect();
    let kind = kind.trim();
    match (name.is_empty(), kind.is_empty()) {
        (false, false) => format!("{verb} the \u{201C}{name}\u{201D} {kind}."),
        (false, true) => format!("{verb} \u{201C}{name}\u{201D}."),
        (true, false) => format!("{verb} the {kind}."),
        (true, true) => format!("{verb} here."),
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn describe_at(
    layout: &CaptureLayout,
    point: ClickPosition,
    right: bool,
) -> std::io::Result<String> {
    layout.validate()?;
    let (x, y) = point.logical(layout);
    let (name, kind) =
        jiffshot_capture::targets::element_at_points_checked(x, y)?.unwrap_or_default();
    layout.validate()?;
    Ok(describe_element(name, kind, right))
}

fn collect_pending(
    steps: &mut Vec<Step>,
    capture: &mut impl FnMut(ClickPosition, bool) -> std::io::Result<Step>,
) -> std::io::Result<()> {
    for (point, right) in drain_clicks() {
        steps.push(capture(point?, right)?);
    }
    Ok(())
}

fn record_with(
    mut panel: impl FnMut(bool, &mut dyn FnMut() -> Option<String>) -> Option<usize>,
    mut capture: impl FnMut(ClickPosition, bool) -> std::io::Result<Step>,
) -> std::io::Result<Option<Vec<Step>>> {
    let mut steps = Vec::new();
    let mut failure = None;
    let mut paused = false;
    loop {
        let mut tick = || {
            if failure.is_none() {
                failure = collect_pending(&mut steps, &mut capture).err();
            }
            if let Some(error) = &failure {
                return Some(format!("Step Capture failed: {error}. Stop or cancel."));
            }
            let n = steps.len();
            Some(format!(
                "Step Capture · {n} step{} recorded. Clicks on this panel are not recorded.",
                if n == 1 { "" } else { "s" }
            ))
        };
        let choice = panel(paused, &mut tick);
        drop(tick);
        if choice == Some(1) {
            paused = !paused;
            pause_clicks(paused);
            if paused && failure.is_none() {
                failure = collect_pending(&mut steps, &mut capture).err();
            }
            continue;
        }
        pause_clicks(true);
        if choice == Some(0) && failure.is_none() {
            failure = collect_pending(&mut steps, &mut capture).err();
        }
        CLICKS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .clear();
        if let Some(error) = failure {
            return Err(error);
        }
        return Ok((choice == Some(0) && !steps.is_empty()).then_some(steps));
    }
}

#[cfg(target_os = "macos")]
fn finish_on_shutdown(result: hud::PanelResult, shutting_down: bool) -> Option<usize> {
    match result {
        hud::PanelResult::Quit if shutting_down => Some(0),
        hud::PanelResult::Quit => None,
        hud::PanelResult::Answer(choice) => choice,
    }
}

/// Records until Stop or Esc; returns the steps (`None` when cancelled).
pub fn record() -> std::io::Result<Option<Vec<Step>>> {
    reset_clicks();
    let layout = CaptureLayout::current()?;
    #[cfg(target_os = "macos")]
    let _watcher = platform::Watcher::start(layout.desktop_scale())?;
    #[cfg(windows)]
    let watcher = platform::Watcher::start()?;
    let corner = layout
        .monitors()
        .into_iter()
        .find(|m| m.primary)
        .map(|m| m.work)
        .unwrap_or_else(|| layout.virtual_screen());
    let anchor = Rect::new(corner.right() - 380, corner.y + 8, 370, 1);
    let result = record_with(
        |paused, tick| {
            let buttons: &[&str] = if paused {
                &["Stop", "Resume", "Cancel"]
            } else {
                &["Stop", "Pause", "Cancel"]
            };
            let tick_panel = || {
                if layout.validate().is_err() {
                    hud::finish_panel(None);
                    return None;
                }
                tick()
            };
            #[cfg(target_os = "macos")]
            return finish_on_shutdown(
                hud::run_panel_until_quit(
                    anchor,
                    "Step Capture: click through the workflow.",
                    buttons,
                    120,
                    tick_panel,
                ),
                crate::shutdown_macos::requested(),
            );
            #[cfg(not(target_os = "macos"))]
            hud::run_panel(
                anchor,
                "Step Capture: click through the workflow.",
                buttons,
                120,
                tick_panel,
            )
        },
        |point, right| {
            // Let the click's immediate visual feedback settle before capturing.
            std::thread::sleep(std::time::Duration::from_millis(60));
            layout.validate()?;
            #[cfg(target_os = "macos")]
            let monitor = {
                let (x, y) = point.reference();
                Some(layout.monitor_at_precise(x, y)?)
            };
            #[cfg(windows)]
            let monitor = layout.monitor_at(point.grid.0, point.grid.1);
            let area = monitor
                .as_ref()
                .map(|m| m.rect)
                .unwrap_or_else(|| layout.virtual_screen());
            let capture_layout = match monitor {
                Some(monitor) => layout.clone().with_monitor(&monitor)?,
                None => layout.clone(),
            };
            let mapping = capture_layout.mapping(area)?;
            let frame = capture_layout.grab(area)?;
            #[cfg(target_os = "macos")]
            let description = describe_at(&layout, point, right)?;
            #[cfg(windows)]
            let description = describe(&watcher, point.grid, right);
            layout.validate()?;
            Ok(Step {
                frame,
                area,
                mapping: Some(mapping),
                click: point.grid,
                #[cfg(target_os = "macos")]
                click_precise: point.precise,
                right,
                description,
            })
        },
    );
    layout.validate()?;
    result
}

pub(crate) fn frame_to_pixmap(f: &Frame) -> Option<tiny_skia::Pixmap> {
    let mut rgba = Vec::with_capacity(f.pixels.len());
    for p in f.pixels.chunks_exact(4) {
        rgba.extend_from_slice(&[p[2], p[1], p[0], 255]);
    }
    tiny_skia::Pixmap::from_vec(rgba, tiny_skia::IntSize::from_wh(f.width, f.height)?)
}

/// Lays the steps out as an editable guide: a title, then for each step a
/// number, its caption and the screen zoomed around the click, with the
/// click circled and a pointer stamp on it.
pub fn guide(steps: &[Step]) -> std::io::Result<Document> {
    guide_titled(steps, "How to …")
}

pub fn guide_titled(steps: &[Step], title: &str) -> std::io::Result<Document> {
    const W: f32 = 1400.0;
    const SHOT: (f32, f32) = (1000.0, 560.0);
    let pad = 40.0;
    let row = SHOT.1 + pad * 1.5;
    let h = 140.0 + steps.len() as f32 * row + pad;
    let mut doc = Document::blank(W as u32, h as u32, Rgba::WHITE);
    let ink = Rgba::rgb(0x22, 0x26, 0x2E);
    let accent = Rgba::rgb(0x1E, 0x6F, 0xD9);
    let text = |size: f32, bold: bool, color: Rgba| Style {
        font_size: size,
        bold,
        text_color: color,
        width: 0.0,
        fill: Rgba::TRANSPARENT,
        align: Align::Left,
        ..Style::default()
    };
    doc.add(
        Kind::Text {
            rect: Box2::from_xywh(pad, 30.0, W - 2.0 * pad, 60.0),
            text: title.into(),
        },
        text(40.0, true, ink),
    );
    doc.add(
        Kind::Text {
            rect: Box2::from_xywh(pad, 88.0, W - 2.0 * pad, 30.0),
            text: format!("{} steps", steps.len()),
        },
        text(18.0, false, Rgba::rgb(0x6B, 0x72, 0x80)),
    );
    for (i, step) in steps.iter().enumerate() {
        let y = 140.0 + i as f32 * row;
        // The part of the screen around the click, at most SHOT in size.
        let (fw, fh) = (step.frame.width as f32, step.frame.height as f32);
        let mapping = match step.mapping {
            Some(mapping)
                if mapping.rect() == step.area
                    && mapping.size() == (step.frame.width, step.frame.height) =>
            {
                Some(mapping)
            }
            Some(_) => None,
            None => CaptureMapping::new(step.area, step.frame.width, step.frame.height),
        }
        .ok_or_else(|| {
            std::io::Error::other("a Step Capture image has invalid screen dimensions")
        })?;
        #[cfg(target_os = "macos")]
        let (cx, cy) = match step.click_precise {
            Some((x, y)) => mapping.image_point_precise(x, y).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "a Step Capture click position is invalid",
                )
            })?,
            None => mapping.image_point(step.click.0, step.click.1),
        };
        #[cfg(windows)]
        let (cx, cy) = mapping.image_point(step.click.0, step.click.1);
        let (cx, cy) = (cx as f32, cy as f32);
        let (vw, vh) = (SHOT.0.min(fw), SHOT.1.min(fh));
        let vx = (cx - vw / 2.0).clamp(0.0, fw - vw);
        let vy = (cy - vh / 2.0).clamp(0.0, fh - vh);
        let crop = step
            .frame
            .crop(Rect::new(vx as i32, vy as i32, vw as i32, vh as i32));
        let x0 = pad + 70.0;
        if let Some(pm) = crop.as_ref().and_then(frame_to_pixmap) {
            let rect = Box2::from_xywh(x0, y, pm.width() as f32, pm.height() as f32);
            let style = Style {
                width: 1.5,
                stroke: Rgba::rgb(0xD0, 0xD4, 0xDA),
                shadow: true,
                ..Style::default()
            };
            doc.add(
                Kind::Image {
                    rect,
                    image: jiffshot_doc::object::ImageRef::new(format!("step-{}", i + 1), pm),
                },
                style,
            );
            let (px, py) = (x0 + cx - vx, y + cy - vy);
            let ring = Style {
                stroke: if step.right {
                    Rgba::rgb(0xF5, 0x9E, 0x0B)
                } else {
                    Rgba::RED
                },
                width: 4.0,
                fill: Rgba::TRANSPARENT,
                ..Style::default()
            };
            doc.add(
                Kind::Shape {
                    rect: Box2::around(Pt::new(px, py), 22.0, 22.0),
                    shape: ShapeKind::Ellipse,
                },
                ring,
            );
            doc.add(
                Kind::Stamp {
                    rect: Box2::from_xywh(px - 6.0, py - 4.0, 34.0, 40.0),
                    stamp: Stamp::Pointer,
                },
                Style::default(),
            );
        }
        let badge = Style {
            fill: accent,
            text_color: Rgba::WHITE,
            width: 0.0,
            font_size: 24.0,
            ..Style::default()
        };
        doc.add(
            Kind::Step {
                center: Pt::new(pad + 26.0, y + 26.0),
                radius: 24.0,
                label: (i + 1).to_string(),
                shape: ShapeKind::Ellipse,
            },
            badge,
        );
        doc.add(
            Kind::Text {
                rect: Box2::from_xywh(x0 + SHOT.0 + 24.0, y, W - x0 - SHOT.0 - 24.0 - pad, 120.0),
                text: step.description.clone(),
            },
            text(22.0, false, ink),
        );
    }
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    static RECORD_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(target_os = "macos")]
    #[test]
    fn queued_clicks_keep_the_event_position_and_report_invalid_events() {
        let _record = RECORD_TEST_LOCK.lock().unwrap();
        reset_clicks();
        let left = ClickPosition::from_points((-0.13, 1.0), 2.0).unwrap();
        let right = ClickPosition::from_points((-0.12, 1.0), 2.0).unwrap();
        assert_eq!(left.grid, right.grid);
        assert_eq!(left.reference(), (-0.26, 2.0));
        assert_eq!(right.reference(), (-0.24, 2.0));
        assert!(ClickPosition::from_points((f64::NAN, 0.0), 2.0).is_err());
        assert!(ClickPosition::from_points((1.0, 0.0), f64::INFINITY).is_err());
        assert!(ClickPosition::from_points((1.0, 0.0), 0.0).is_err());

        queue_click_if_active(Ok(left), false, false, || true);
        queue_click_if_active(Ok(right), true, false, || true);
        let steps = record_with(
            |_, _| Some(0),
            |point, right| {
                Ok(Step {
                    frame: Frame::new(1, 1),
                    area: Rect::new(0, 0, 1, 1),
                    mapping: None,
                    click: point.grid,
                    click_precise: point.precise,
                    right,
                    description: String::new(),
                })
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            steps
                .iter()
                .map(|step| (step.click_precise, step.right))
                .collect::<Vec<_>>(),
            [(Some((-0.26, 2.0)), false), (Some((-0.24, 2.0)), true),]
        );

        reset_clicks();
        queue_click_if_active(
            Err(std::io::Error::other("mouse event has no position")),
            false,
            false,
            || true,
        );
        let error = record_with(
            |_, _| Some(0),
            |_, _| panic!("invalid events must not be captured"),
        )
        .err()
        .expect("invalid event must fail Step Capture");
        assert!(error.to_string().contains("mouse event has no position"));
    }

    #[test]
    fn delayed_clicks_cannot_cross_pause_or_watcher_lifetimes() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{mpsc, Arc};

        let _record = RECORD_TEST_LOCK.lock().unwrap();
        reset_clicks();
        std::thread::scope(|scope| {
            let mut queue = CLICKS.lock().unwrap();
            let (started, ready) = mpsc::channel();
            let click = scope.spawn(move || {
                started.send(()).unwrap();
                clicked((1, 2), false, false);
            });
            ready.recv().unwrap();
            queue.paused = true;
            drop(queue);
            click.join().unwrap();
        });
        assert!(drain_clicks().is_empty());

        reset_clicks();
        let active = Arc::new(AtomicBool::new(true));
        std::thread::scope(|scope| {
            let mut queue = CLICKS.lock().unwrap();
            let (started, ready) = mpsc::channel();
            let old_watcher = active.clone();
            let click = scope.spawn(move || {
                assert!(old_watcher.load(Ordering::Acquire));
                started.send(()).unwrap();
                queue_click_if_active(Ok(ClickPosition::from_grid((3, 4))), false, false, || {
                    old_watcher.load(Ordering::Acquire)
                });
            });
            ready.recv().unwrap();
            queue.paused = true;
            active.store(false, Ordering::Release);
            queue.pending.clear();
            queue.paused = false;
            drop(queue);
            click.join().unwrap();
        });
        assert!(drain_clicks().is_empty());
        pause_clicks(true);
    }

    #[test]
    fn pause_and_stop_collect_pending_clicks_without_recording_paused_or_cancelled_clicks() {
        let _record = RECORD_TEST_LOCK.lock().unwrap();
        #[cfg(target_os = "macos")]
        {
            assert_eq!(finish_on_shutdown(hud::PanelResult::Quit, true), Some(0));
            assert_eq!(finish_on_shutdown(hud::PanelResult::Quit, false), None);
            assert_eq!(
                finish_on_shutdown(hud::PanelResult::Answer(None), true),
                None
            );
            assert_eq!(
                finish_on_shutdown(hud::PanelResult::Answer(Some(2)), true),
                Some(2)
            );
        }
        reset_clicks();
        let mut panel_count = 0;
        let capture = |point: ClickPosition, right| {
            Ok(Step {
                frame: Frame::new(1, 1),
                area: Rect::new(0, 0, 1, 1),
                mapping: None,
                click: point.grid,
                #[cfg(target_os = "macos")]
                click_precise: point.precise,
                right,
                description: String::new(),
            })
        };
        let result = record_with(
            |paused, tick| {
                panel_count += 1;
                match panel_count {
                    1 => {
                        assert!(!paused);
                        clicked((1, 1), false, false);
                        assert!(tick().unwrap().contains("1 step recorded"));
                        clicked((2, 2), false, false);
                        clicked((9, 9), false, true);
                        Some(1)
                    }
                    2 => {
                        assert!(paused);
                        clicked((3, 3), false, false);
                        Some(1)
                    }
                    3 => {
                        assert!(!paused);
                        clicked((4, 4), true, false);
                        Some(0)
                    }
                    _ => panic!("unexpected panel"),
                }
            },
            capture,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            result
                .iter()
                .map(|step| (step.click, step.right))
                .collect::<Vec<_>>(),
            [((1, 1), false), ((2, 2), false), ((4, 4), true)]
        );

        pause_clicks(false);
        let cancelled = record_with(
            |_, _| {
                clicked((5, 5), false, false);
                Some(2)
            },
            |_, _| panic!("cancellation must not capture pending clicks"),
        )
        .unwrap();
        assert!(cancelled.is_none());
        assert!(CLICKS.lock().unwrap().pending.is_empty());

        pause_clicks(false);
        let error = record_with(
            |_, _| {
                clicked((6, 6), false, false);
                Some(0)
            },
            |_, _| Err(std::io::Error::other("capture failed")),
        )
        .err()
        .expect("capture errors must be reported");
        assert_eq!(error.to_string(), "capture failed");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn shutdown_preserves_recorded_steps_but_concurrent_cancellation_discards_them() {
        let _record = RECORD_TEST_LOCK.lock().unwrap();
        for cancellation in [None, Some(None), Some(Some(2))] {
            reset_clicks();
            clicked((7, 5), false, false);
            let result = record_with(
                |paused, tick| {
                    assert!(!paused);
                    let mut requested = false;
                    let result = hud::run_panel_until_quit(
                        Rect::new(0, 0, 16, 12),
                        "Synthetic Step Capture",
                        &["Stop", "Pause", "Cancel"],
                        1,
                        || {
                            let status = tick();
                            if !requested {
                                assert!(status
                                    .as_deref()
                                    .is_some_and(|text| text.contains("1 step recorded")));
                                requested = true;
                                if let Some(answer) = cancellation {
                                    hud::finish_panel(answer);
                                }
                                // SAFETY: this test owns the current thread's message queue.
                                unsafe { crate::sys::PostQuitMessage(0) };
                            }
                            status
                        },
                    );
                    finish_on_shutdown(result, true)
                },
                |point, right| {
                    let mut frame = Frame::new(16, 12);
                    for pixel in frame.pixels.chunks_exact_mut(4) {
                        pixel.copy_from_slice(&[14, 38, 207, 255]);
                    }
                    Ok(Step {
                        frame,
                        area: Rect::new(0, 0, 16, 12),
                        mapping: None,
                        click: point.grid,
                        click_precise: point.precise,
                        right,
                        description: "Click the synthetic control.".into(),
                    })
                },
            )
            .unwrap();
            let mut message: crate::sys::MSG = unsafe { std::mem::zeroed() };
            // SAFETY: reads the quit message left for the outer capture loop.
            assert_ne!(
                unsafe {
                    crate::sys::PeekMessageW(
                        &mut message,
                        std::ptr::null_mut(),
                        0,
                        0,
                        crate::sys::PM_REMOVE,
                    )
                },
                0
            );
            assert_eq!(message.message, crate::sys::WM_QUIT);
            if cancellation.is_some() {
                assert!(
                    result.is_none(),
                    "a user cancellation must discard the guide"
                );
            } else {
                let steps = result.expect("shutdown must preserve recorded steps");
                assert_eq!(steps.len(), 1);
                let document = guide(&steps).unwrap();
                assert!(document.objects.iter().any(
                    |object| matches!(&object.kind, Kind::Step { label, .. } if label == "1")
                ));
                assert!(document.objects.iter().any(|object| matches!(&object.kind, Kind::Text { text, .. } if text == "Click the synthetic control.")));
                assert!(document
                    .objects
                    .iter()
                    .any(|object| matches!(&object.kind, Kind::Image { .. })));
            }
        }
    }

    #[test]
    fn guide_has_a_numbered_row_per_step() {
        let step = |x: i32, text: &str| {
            let mut frame = Frame::new(1600, 900);
            frame.pixels.iter_mut().for_each(|b| *b = 0xE0);
            Step {
                frame,
                area: Rect::new(0, 0, 1600, 900),
                mapping: None,
                click: (x, 450),
                #[cfg(target_os = "macos")]
                click_precise: None,
                right: false,
                description: text.to_owned(),
            }
        };
        let doc = guide(&[
            step(100, "Click the \u{201C}File\u{201D} menu item."),
            step(1500, "Click the \u{201C}Save\u{201D} button."),
        ])
        .unwrap();
        let steps: Vec<_> = doc
            .objects
            .iter()
            .filter_map(|o| match &o.kind {
                Kind::Step { label, .. } => Some(label.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(steps, ["1", "2"]);
        let images = doc
            .objects
            .iter()
            .filter(|o| matches!(o.kind, Kind::Image { .. }))
            .count();
        assert_eq!(images, 2);
        assert!(doc
            .objects
            .iter()
            .any(|o| matches!(&o.kind, Kind::Text { text, .. } if text.contains("Save"))));
        let flat = doc.flatten();
        assert_eq!(flat.width(), 1400);
        if let Some(out) = std::env::var_os("JIFFSHOT_GUIDE_OUT") {
            flat.save_png(out).unwrap();
        }
    }

    #[test]
    fn guide_places_the_marker_in_mapped_image_pixels() {
        let step = Step {
            frame: Frame::new(400, 200),
            area: Rect::new(-200, 100, 800, 400),
            mapping: None,
            click: (599, 499),
            #[cfg(target_os = "macos")]
            click_precise: None,
            right: false,
            description: String::new(),
        };
        let doc = guide(&[step]).unwrap();
        let ring = doc
            .objects
            .iter()
            .find_map(|object| match &object.kind {
                Kind::Shape {
                    rect,
                    shape: ShapeKind::Ellipse,
                } => Some(rect),
                _ => None,
            })
            .expect("click marker");
        assert_eq!(ring.center(), Pt::new(509.0, 339.0));

        let invalid = Step {
            frame: Frame::new(0, 0),
            area: Rect::default(),
            mapping: None,
            click: (0, 0),
            #[cfg(target_os = "macos")]
            click_precise: None,
            right: false,
            description: String::new(),
        };
        assert!(guide(&[invalid]).is_err());
    }

    #[test]
    fn guide_uses_the_retained_fractional_display_mapping() {
        let area = Rect::new(0, 0, 800, 400);
        let mapping =
            CaptureMapping::with_desktop_bounds(area, 400, 200, [0.4, 0.0, 800.4, 400.0]).unwrap();
        let step = Step {
            frame: Frame::new(400, 200),
            area,
            mapping: Some(mapping),
            click: (200, 100),
            #[cfg(target_os = "macos")]
            click_precise: None,
            right: false,
            description: String::new(),
        };
        let doc = guide_titled(&[step], "Fractional display").unwrap();
        let ring = doc
            .objects
            .iter()
            .find_map(|object| match &object.kind {
                Kind::Shape {
                    rect,
                    shape: ShapeKind::Ellipse,
                } => Some(rect),
                _ => None,
            })
            .unwrap();
        assert_eq!(ring.center(), Pt::new(209.0, 190.0));
        assert!(doc.objects.iter().any(
            |object| matches!(&object.kind, Kind::Text { text, .. } if text == "Fractional display")
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn guide_marks_the_unrounded_click_in_the_retained_image_mapping() {
        let area = Rect::new(0, 0, 800, 400);
        let mapping =
            CaptureMapping::with_desktop_bounds(area, 400, 200, [0.4, 0.0, 800.4, 400.0]).unwrap();
        let mut step = Step {
            frame: Frame::new(400, 200),
            area,
            mapping: Some(mapping),
            click: (200, 100),
            click_precise: Some((200.49, 100.0)),
            right: false,
            description: String::new(),
        };
        let doc = guide(std::slice::from_ref(&step)).unwrap();
        let ring = doc
            .objects
            .iter()
            .find_map(|object| match &object.kind {
                Kind::Shape {
                    rect,
                    shape: ShapeKind::Ellipse,
                } => Some(rect),
                _ => None,
            })
            .expect("click marker");
        assert_eq!(ring.center(), Pt::new(210.0, 190.0));
        step.click_precise = Some((f64::NAN, 100.0));
        assert_eq!(
            guide(&[step]).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
}
