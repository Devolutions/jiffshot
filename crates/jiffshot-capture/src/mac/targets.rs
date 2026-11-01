//! Capture targets under the cursor on macOS: on-screen windows from the
//! window server in z-order and, when Jiffshot may use Accessibility, the
//! panes, toolbars and controls inside them, so the overlay can snap to a
//! window or a part of it.
//!
//! The list is a snapshot taken when the overlay opens, matching the frozen
//! screen image. `hwnd` holds the window server's window number for
//! top-level targets and 0 for elements inside them.

use crate::macos::ffi::*;
use crate::screen::{rect_from_points, DisplayLayout, LogicalBounds};
use crate::Rect;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::MainThreadMarker;
use objc2_app_kit::NSWorkspace;
use objc2_core_foundation::CGRect;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSURL};
use serde::Serialize;
use std::ffi::c_void;
use std::io;

#[path = "targets/control_elements.rs"]
mod control_elements;
pub use control_elements::{accessibility_elements, window_at_point, AccessibleElement, AccessibleElements};

#[derive(Clone, Debug)]
pub struct Target {
    pub hwnd: isize,
    /// Screen rectangle, clipped to its top-level window.
    pub rect: Rect,
    pub title: String,
    /// The owning app's name for windows, the accessibility role inside
    /// them.
    pub class: String,
    pub process_id: u32,
    /// 0 for top-level windows.
    pub depth: u32,
    /// Index of the parent target, `None` for top-level windows.
    pub parent: Option<usize>,
    /// Exact clipped Quartz bounds retained with the target's reference-grid rectangle.
    pub logical_bounds: Option<LogicalBounds>,
}

/// Every target, top-level windows in z-order (topmost first), each followed
/// by its descendants in z-order.
#[derive(Clone, Debug, Default)]
pub struct Targets {
    pub list: Vec<Target>,
}

fn matches_parent_geometry(parent: &Target, rect: Rect, bounds: LogicalBounds) -> bool {
    parent.logical_bounds.map_or(parent.rect == rect, |parent_bounds| parent_bounds == bounds)
}

const MAX_DEPTH: u32 = 6;
const MAX_CHILDREN: usize = 4000;
/// Windows whose accessibility tree is walked (the topmost ones; the rest
/// only offer their frame).
const MAX_WALKED: usize = 16;
const MIN_SIZE: i32 = 8;

/// Owners that make up the desktop and system chrome rather than windows a
/// user would capture.
const SYSTEM_OWNERS: &[&str] = &["Window Server", "Dock", "WindowManager", "Control Center", "Notification Center"];

type AXUIElementRef = *const c_void;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: CFTypeRef) -> bool;
    static kAXTrustedCheckOptionPrompt: CFTypeRef;
    fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(element: AXUIElementRef, attribute: CFTypeRef, value: *mut CFTypeRef) -> i32;
    fn AXUIElementSetAttributeValue(element: AXUIElementRef, attribute: CFTypeRef, value: CFTypeRef) -> i32;
    fn AXUIElementPerformAction(element: AXUIElementRef, action: CFTypeRef) -> i32;
    fn AXUIElementSetMessagingTimeout(element: AXUIElementRef, seconds: f32) -> i32;
    fn AXValueGetValue(value: CFTypeRef, kind: u32, out: *mut c_void) -> bool;
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCopyElementAtPosition(element: AXUIElementRef, x: f32, y: f32, out: *mut AXUIElementRef) -> i32;
}

const AX_VALUE_CGPOINT: u32 = 1;
const AX_VALUE_CGSIZE: u32 = 2;

/// Whether Jiffshot may read other apps' accessibility trees (needed for
/// panes inside windows, scrolling capture and step capture details).
pub fn accessibility_trusted() -> bool {
    // SAFETY: a plain query.
    unsafe { AXIsProcessTrusted() }
}

/// Requests Accessibility access when a feature actually needs it.
pub fn request_accessibility() -> bool {
    if accessibility_trusted() {
        return true;
    }
    let yes = NSNumber::new_bool(true);
    // SAFETY: the documented key is a CFString, bridged to NSString, and the
    // dictionary and its Boolean remain alive for the trust query.
    unsafe {
        let key = &*(kAXTrustedCheckOptionPrompt as *const NSString);
        let options = NSDictionary::from_slices(&[key], &[&*yes]);
        AXIsProcessTrustedWithOptions((&*options as *const NSDictionary<NSString, NSNumber>).cast())
    }
}

fn number(dict: &NSDictionary<NSString, AnyObject>, key: &str) -> Option<f64> {
    let value = dict.objectForKey(&NSString::from_str(key))?;
    value.downcast::<NSNumber>().ok().map(|n| n.doubleValue())
}

fn string(dict: &NSDictionary<NSString, AnyObject>, key: &str) -> String {
    dict.objectForKey(&NSString::from_str(key))
        .and_then(|v| v.downcast::<NSString>().ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn bounds(dict: &NSDictionary<NSString, AnyObject>) -> Option<objc2_core_foundation::CGRect> {
    let value = dict.objectForKey(&NSString::from_str("kCGWindowBounds"))?;
    let b = value.downcast::<NSDictionary>().ok()?;
    // SAFETY: the window server's bounds dictionary maps strings to numbers.
    let b: Retained<NSDictionary<NSString, AnyObject>> = unsafe { Retained::cast_unchecked(b) };
    Some(rect(number(&b, "X")?, number(&b, "Y")?, number(&b, "Width")?, number(&b, "Height")?))
}

fn checked_window_list_with_options(options: u32) -> io::Result<Vec<Retained<NSDictionary<NSString, AnyObject>>>> {
    // SAFETY: the returned CFArray is owned (released by `Owned`) and
    // toll-free bridged to NSArray of NSDictionary.
    unsafe {
        let list = Owned(CGWindowListCopyWindowInfo(options | kCGWindowListExcludeDesktopElements, kCGNullWindowID));
        if list.0.is_null() {
            return Err(io::Error::other("could not enumerate windows"));
        }
        let array = &*(list.0 as *const NSArray<NSDictionary<NSString, AnyObject>>);
        Ok(array.iter().collect())
    }
}

/// On-screen windows front to back, as window-server dictionaries.
fn checked_window_list() -> io::Result<Vec<Retained<NSDictionary<NSString, AnyObject>>>> {
    checked_window_list_with_options(kCGWindowListOptionOnScreenOnly)
}

fn window_list() -> Vec<Retained<NSDictionary<NSString, AnyObject>>> {
    checked_window_list().unwrap_or_else(|error| {
        eprintln!("{error}");
        Vec::new()
    })
}

struct Ax(CFTypeRef);

impl Drop for Ax {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: owns one reference from a Copy/Create call.
            unsafe { CFRelease(self.0) }
        }
    }
}

fn ax_attr(element: AXUIElementRef, name: &str) -> Option<Ax> {
    let key = NSString::from_str(name);
    let mut value: CFTypeRef = std::ptr::null();
    // SAFETY: NSString is toll-free bridged to CFString; the value is owned.
    let err = unsafe { AXUIElementCopyAttributeValue(element, Retained::as_ptr(&key) as CFTypeRef, &mut value) };
    (err == 0 && !value.is_null()).then_some(Ax(value))
}

fn ax_rect(element: AXUIElementRef) -> Option<objc2_core_foundation::CGRect> {
    let (pos, size) = (ax_attr(element, "AXPosition")?, ax_attr(element, "AXSize")?);
    let mut p = objc2_core_foundation::CGPoint { x: 0.0, y: 0.0 };
    let mut s = objc2_core_foundation::CGSize { width: 0.0, height: 0.0 };
    // SAFETY: AXValues of the requested kinds written into matching structs.
    let ok = unsafe { AXValueGetValue(pos.0, AX_VALUE_CGPOINT, &mut p as *mut _ as *mut c_void) && AXValueGetValue(size.0, AX_VALUE_CGSIZE, &mut s as *mut _ as *mut c_void) };
    ok.then(|| rect(p.x, p.y, s.width, s.height))
}

fn ax_string(element: AXUIElementRef, name: &str) -> String {
    let Some(v) = ax_attr(element, name) else { return String::new() };
    // SAFETY: the attribute is checked to be a string before use.
    let obj = unsafe { &*(v.0 as *const AnyObject) };
    obj.downcast_ref::<NSString>().map(|s| s.to_string()).unwrap_or_default()
}

fn ax_url_string(element: AXUIElementRef, name: &str) -> Option<String> {
    let value = ax_attr(element, name)?;
    // SAFETY: AX attribute values are Core Foundation objects, bridged to
    // Foundation before checking their concrete type.
    let obj = unsafe { &*(value.0 as *const AnyObject) };
    if let Some(string) = obj.downcast_ref::<NSString>() {
        return Some(string.to_string());
    }
    obj.downcast_ref::<NSURL>()?.absoluteString().map(|s| s.to_string())
}

fn ax_children(element: AXUIElementRef) -> Vec<Ax> {
    let Some(v) = ax_attr(element, "AXChildren") else { return Vec::new() };
    // SAFETY: AXChildren is a CFArray of AXUIElements (toll-free bridged).
    let obj = unsafe { &*(v.0 as *const AnyObject) };
    let Some(array) = obj.downcast_ref::<NSArray>() else { return Vec::new() };
    array
        .iter()
        // SAFETY: each element is retained for the returned `Ax`.
        .map(|e| Ax(unsafe { CFRetain(Retained::as_ptr(&e) as CFTypeRef) }))
        .collect()
}

/// A top-level window as reported by the macOS window server.
#[derive(Clone, Debug, Serialize)]
pub struct Window {
    pub hwnd: isize,
    pub title: String,
    /// macOS reports the owning application's name instead of a Win32 class.
    pub class: String,
    pub pid: u32,
    pub process: String,
    pub rect: Rect,
    pub minimized: bool,
    pub on_screen: bool,
    pub foreground: bool,
    #[serde(skip)]
    layer: i32,
    #[serde(skip)]
    native_bounds: CGRect,
    #[serde(skip)]
    alpha: f64,
}

impl Window {
    pub fn is_regular(&self) -> bool {
        self.layer == 0 && !SYSTEM_OWNERS.contains(&self.class.as_str())
    }

    pub fn is_visible_candidate(&self) -> bool {
        self.alpha > 0.0
    }

    /// Returns this snapshot's visible bounds without rounding to the reference grid.
    pub fn clipped_to_layout(&self, layout: &DisplayLayout) -> io::Result<(Rect, LogicalBounds)> {
        layout.validate()?;
        let clip = logical_clip(layout.virtual_screen(), layout)?;
        let (rect, bounds) = clipped_geometry(self.native_bounds, clip, layout.desktop_scale())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the window is outside the active desktop"))?;
        if self.rect.intersect(layout.virtual_screen()) != Some(rect) {
            return Err(io::Error::other("the window geometry no longer matches the retained display layout"));
        }
        layout.validate()?;
        Ok((rect, bounds))
    }
}

fn window_rect(bounds: CGRect, scale: f64) -> Option<Rect> {
    grid_rect([
        bounds.origin.x,
        bounds.origin.y,
        bounds.origin.x + bounds.size.width,
        bounds.origin.y + bounds.size.height,
    ], scale)
}

fn grid_rect(edges: [f64; 4], scale: f64) -> Option<Rect> {
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let edges = edges.map(|edge| edge * scale);
    if !edges.iter().all(|edge| edge.is_finite() && (i32::MIN as f64..=i32::MAX as f64).contains(&edge.round())) {
        return None;
    }
    let [left, top, right, bottom] = edges.map(|edge| edge.round() as i32);
    let width = i32::try_from(i64::from(right) - i64::from(left)).ok()?;
    let height = i32::try_from(i64::from(bottom) - i64::from(top)).ok()?;
    (width > 0 && height > 0).then(|| Rect::new(left, top, width, height))
}

fn logical_clip(clip: Rect, layout: &DisplayLayout) -> io::Result<LogicalBounds> {
    let right = clip.x.checked_add(clip.width);
    let bottom = clip.y.checked_add(clip.height);
    let (Some(right), Some(bottom)) = (right, bottom) else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid target clipping rectangle"));
    };
    if clip.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid target clipping rectangle"));
    }
    let desktop = layout.virtual_screen();
    let desktop_right = desktop.x.checked_add(desktop.width)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid desktop rectangle"))?;
    let desktop_bottom = desktop.y.checked_add(desktop.height)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid desktop rectangle"))?;
    let desktop_bounds = layout.logical_virtual_bounds()?;
    let [desktop_left, desktop_top, desktop_right_edge, desktop_bottom_edge] = desktop_bounds.edges();
    let scale = layout.desktop_scale();
    let bounds = LogicalBounds::new([
        if clip.x == desktop.x { desktop_left } else { clip.x as f64 / scale },
        if clip.y == desktop.y { desktop_top } else { clip.y as f64 / scale },
        if right == desktop_right { desktop_right_edge } else { right as f64 / scale },
        if bottom == desktop_bottom { desktop_bottom_edge } else { bottom as f64 / scale },
    ])?;
    bounds.intersection(desktop_bounds)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the target clipping rectangle is outside the active desktop"))
}

fn clipped_geometry(frame: CGRect, clip: LogicalBounds, scale: f64) -> Option<(Rect, LogicalBounds)> {
    let bounds = LogicalBounds::new([
        frame.origin.x,
        frame.origin.y,
        frame.origin.x + frame.size.width,
        frame.origin.y + frame.size.height,
    ]).ok()?.intersection(clip)?;
    Some((grid_rect(bounds.edges(), scale)?, bounds))
}

/// Enumerates on-screen windows in z-order, retaining the supplied display grid.
pub fn windows(layout: &DisplayLayout) -> io::Result<Vec<Window>> {
    read_windows(layout, false)
}

/// Enumerates window-server candidates, including off-screen records. Their
/// minimized state must be verified with `offscreen_minimized` before use.
pub fn window_candidates(layout: &DisplayLayout) -> io::Result<Vec<Window>> {
    read_windows(layout, true)
}

fn read_windows(layout: &DisplayLayout, include_offscreen: bool) -> io::Result<Vec<Window>> {
    if !crate::macos::sck::screen_access(true) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied,
            "Jiffshot needs Screen Recording permission to read window titles"));
    }
    layout.validate()?;
    let front_pid = NSWorkspace::sharedWorkspace().frontmostApplication()
        .and_then(|app| u32::try_from(app.processIdentifier()).ok());
    let mut marked_foreground = false;
    let mut names = std::collections::HashMap::new();
    let mut windows = Vec::new();
    let options = if include_offscreen { kCGWindowListOptionAll } else { kCGWindowListOptionOnScreenOnly };
    for dict in checked_window_list_with_options(options)? {
        let Some(id) = number(&dict, "kCGWindowNumber").filter(|id| *id > 0.0 && *id <= u32::MAX as f64) else { continue };
        let Some(pid) = number(&dict, "kCGWindowOwnerPID").filter(|pid| *pid > 0.0 && *pid <= i32::MAX as f64) else { continue };
        let alpha = number(&dict, "kCGWindowAlpha").unwrap_or(1.0);
        if !include_offscreen && alpha <= 0.0 {
            continue;
        }
        let Some(native_bounds) = bounds(&dict) else { continue };
        let Some(rect) = window_rect(native_bounds, layout.desktop_scale()) else { continue };
        let (pid, layer) = (pid as u32, number(&dict, "kCGWindowLayer").unwrap_or(0.0) as i32);
        let title = string(&dict, "kCGWindowName");
        let on_screen = !include_offscreen || number(&dict, "kCGWindowIsOnscreen").unwrap_or(0.0) != 0.0;
        let foreground = on_screen && alpha > 0.0 && !marked_foreground && layer == 0 && !title.is_empty() && Some(pid) == front_pid;
        marked_foreground |= foreground;
        let process = names.entry(pid).or_insert_with(|| process_name(pid).unwrap_or_default()).clone();
        windows.push(Window {
            hwnd: id as isize,
            title,
            class: string(&dict, "kCGWindowOwnerName"),
            pid,
            process,
            rect,
            minimized: false,
            on_screen,
            foreground,
            layer,
            native_bounds,
            alpha,
        });
    }
    layout.validate()?;
    Ok(windows)
}

fn same_native_bounds(a: CGRect, b: CGRect) -> bool {
    (a.origin.x - b.origin.x).abs() < 2.0
        && (a.origin.y - b.origin.y).abs() < 2.0
        && (a.size.width - b.size.width).abs() < 2.0
        && (a.size.height - b.size.height).abs() < 2.0
}

fn unique_window_identity(window: &Window, candidates: &[Window]) -> bool {
    if window.title.is_empty() {
        return false;
    }
    let mut matching = candidates.iter().filter(|other| {
        other.pid == window.pid && other.title == window.title
            && same_native_bounds(other.native_bounds, window.native_bounds)
    });
    matching.next().is_some_and(|candidate| candidate.hwnd == window.hwnd) && matching.next().is_none()
}

pub fn ensure_unique_window_identity(window: &Window, candidates: &[Window]) -> io::Result<()> {
    if unique_window_identity(window, candidates) {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidData, "the window does not have a unique title and bounds"))
    }
}

/// Reads the state only when the window-server record maps unambiguously to
/// an Accessibility window. `candidates` must be the complete snapshot.
pub fn offscreen_minimized(window: &Window, candidates: &[Window]) -> io::Result<bool> {
    if window.on_screen {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "the selected window is already on screen"));
    }
    if !accessibility_trusted() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied,
            "Jiffshot needs Accessibility permission to identify off-screen windows"));
    }
    ensure_unique_window_identity(window, candidates)?;
    if window.pid == std::process::id() && MainThreadMarker::new().is_none() {
        let window = window.clone();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let skip = cancelled.clone();
        dispatch2::DispatchQueue::main().exec_async(move || {
            if !skip.load(std::sync::atomic::Ordering::Acquire) {
                let result = offscreen_minimized_on_thread(&window);
                if sender.send(result).is_err() {
                    eprintln!("the off-screen window check finished after its requester stopped waiting");
                }
            }
        });
        return receiver.recv_timeout(std::time::Duration::from_secs(3)).unwrap_or_else(|error| {
            cancelled.store(true, std::sync::atomic::Ordering::Release);
            Err(io::Error::other(format!("could not wait for the off-screen window check: {error}")))
        });
    }
    offscreen_minimized_on_thread(window)
}

fn offscreen_minimized_on_thread(window: &Window) -> io::Result<bool> {
    let app = application_element(window.pid)?;
    let selected = matching_ax_window_named(app.0, window, true)?;
    control_elements::optional_bool(selected.0, "AXMinimized")?
        .ok_or_else(|| io::Error::other("the window does not report its minimized state through Accessibility"))
}

fn ax_operation_error(operation: &str, status: i32) -> io::Error {
    let kind = if status == -25204 { io::ErrorKind::WouldBlock } else { io::ErrorKind::Other };
    io::Error::new(kind, format!("{operation} (Accessibility error {status})"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowActivation {
    Restoring,
    Raised,
}

/// Restores or raises one unambiguously matched Accessibility window. A
/// restored window must be allowed to appear before it can be raised.
pub fn activate_window(window: &Window, require_title: bool, allow_restore: bool) -> io::Result<WindowActivation> {
    if !request_accessibility() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied,
            "Jiffshot needs Accessibility permission to activate a window"));
    }
    if window.pid == std::process::id() && MainThreadMarker::new().is_none() {
        let window = window.clone();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let skip = cancelled.clone();
        dispatch2::DispatchQueue::main().exec_async(move || {
            if !skip.load(std::sync::atomic::Ordering::Acquire) {
                let result = activate_window_on_thread(&window, require_title, allow_restore);
                if sender.send(result).is_err() {
                    eprintln!("the window activation finished after its requester stopped waiting");
                }
            }
        });
        return receiver.recv_timeout(std::time::Duration::from_secs(10)).unwrap_or_else(|error| {
            cancelled.store(true, std::sync::atomic::Ordering::Release);
            Err(io::Error::other(format!("could not wait for window activation; it may still complete: {error}")))
        });
    }
    activate_window_on_thread(window, require_title, allow_restore)
}

fn activate_window_on_thread(window: &Window, require_title: bool, allow_restore: bool) -> io::Result<WindowActivation> {
    let app = application_element(window.pid)?;
    let selected = matching_ax_window_named(app.0, window, require_title || !window.on_screen)?;
    let target = selected.0;
    match control_elements::optional_bool(target, "AXMinimized")? {
        Some(true) => {
            if !allow_restore {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "the selected window was minimized again while being restored"));
            }
            let minimized = NSString::from_str("AXMinimized");
            let no = NSNumber::new_bool(false);
            // SAFETY: the matched window, key and Boolean are live.
            let status = unsafe { AXUIElementSetAttributeValue(target, Retained::as_ptr(&minimized) as CFTypeRef, Retained::as_ptr(&no) as CFTypeRef) };
            if status != 0 {
                return Err(ax_operation_error("could not restore the minimized window", status));
            }
            return Ok(WindowActivation::Restoring);
        }
        Some(false) => {}
        None if !window.on_screen => {
            return Err(io::Error::other("the window does not report its minimized state through Accessibility"));
        }
        None => {}
    }
    let frontmost = NSString::from_str("AXFrontmost");
    let yes = NSNumber::new_bool(true);
    // SAFETY: the live app, key and Boolean are valid Accessibility objects.
    let status = unsafe { AXUIElementSetAttributeValue(app.0, Retained::as_ptr(&frontmost) as CFTypeRef, Retained::as_ptr(&yes) as CFTypeRef) };
    if status != 0 {
        return Err(ax_operation_error("could not activate the window's application", status));
    }
    let raise = NSString::from_str("AXRaise");
    // SAFETY: the matched window and action remain live for the call.
    let status = unsafe { AXUIElementPerformAction(target, Retained::as_ptr(&raise) as CFTypeRef) };
    if status != 0 {
        return Err(ax_operation_error("could not raise the selected window", status));
    }
    Ok(WindowActivation::Raised)
}

fn application_element(pid: u32) -> io::Result<Ax> {
    // SAFETY: creates an owned element for the window's process.
    let app = Ax(unsafe { AXUIElementCreateApplication(pid as i32) });
    if app.0.is_null() {
        return Err(io::Error::other("could not access the window's application"));
    }
    // SAFETY: the element is live, and the timeout is finite.
    let status = unsafe { AXUIElementSetMessagingTimeout(app.0, 0.5) };
    if status != 0 {
        return Err(io::Error::other(format!("could not set the Accessibility timeout (error {status})")));
    }
    Ok(app)
}

fn matching_ax_window(app: AXUIElementRef, window: &Window) -> io::Result<Ax> {
    matching_ax_window_named(app, window, false)
}

fn matching_ax_window_named(app: AXUIElementRef, window: &Window, require_title: bool) -> io::Result<Ax> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    control_elements::selected_window(app, window, deadline, require_title)
}

/// Checks both the frontmost application and the exact Accessibility window.
pub fn window_has_input_focus(window: &Window) -> io::Result<bool> {
    if !accessibility_trusted() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied,
            "Jiffshot needs Accessibility permission to verify the focused window"));
    }
    if window.pid == std::process::id() && MainThreadMarker::new().is_none() {
        let window = window.clone();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let skip = cancelled.clone();
        dispatch2::DispatchQueue::main().exec_async(move || {
            if !skip.load(std::sync::atomic::Ordering::Acquire) {
                let result = window_has_input_focus_on_thread(&window);
                if sender.send(result).is_err() {
                    eprintln!("the window focus check finished after its requester stopped waiting");
                }
            }
        });
        return receiver.recv_timeout(std::time::Duration::from_secs(3)).unwrap_or_else(|error| {
            cancelled.store(true, std::sync::atomic::Ordering::Release);
            Err(io::Error::other(format!("could not wait for the window focus check: {error}")))
        });
    }
    window_has_input_focus_on_thread(window)
}

fn window_has_input_focus_on_thread(window: &Window) -> io::Result<bool> {
    let front_pid = NSWorkspace::sharedWorkspace().frontmostApplication()
        .and_then(|app| u32::try_from(app.processIdentifier()).ok());
    if front_pid != Some(window.pid) {
        return Ok(false);
    }
    let app = application_element(window.pid)?;
    let selected = matching_ax_window(app.0, window)?;
    let focused = ax_attr(app.0, "AXFocusedWindow")
        .ok_or_else(|| io::Error::other("the application's focused window is unavailable through Accessibility"))?;
    // SAFETY: both values are live Accessibility objects.
    Ok(unsafe { CFEqual(selected.0, focused.0) })
}

impl Targets {
    /// Enumerates windows on screen, skipping `exclude` (window numbers of
    /// the overlay and Jiffshot's own panels) and anything outside `clip`.
    pub fn enumerate(clip: Rect, exclude: &[isize]) -> Targets {
        match DisplayLayout::current().and_then(|layout| Self::enumerate_in_layout(clip, exclude, &layout)) {
            Ok(targets) => targets,
            Err(error) => {
                eprintln!("could not enumerate capture targets: {error}");
                Targets::default()
            }
        }
    }

    /// Enumerates targets using the same retained display layout as a capture.
    pub fn enumerate_in_layout(clip: Rect, exclude: &[isize], layout: &DisplayLayout) -> io::Result<Targets> {
        layout.validate()?;
        let clip = logical_clip(clip, layout)?;
        let scale = layout.desktop_scale();
        let mut targets = Targets::default();
        let own = std::process::id();
        let trusted = accessibility_trusted();
        let mut walked = 0;
        for dict in checked_window_list()? {
            let layer = number(&dict, "kCGWindowLayer").unwrap_or(0.0);
            let id = number(&dict, "kCGWindowNumber").unwrap_or(0.0) as isize;
            let pid = number(&dict, "kCGWindowOwnerPID").unwrap_or(0.0) as u32;
            let owner = string(&dict, "kCGWindowOwnerName");
            if layer != 0.0 || exclude.contains(&id) || pid == own || SYSTEM_OWNERS.contains(&owner.as_str()) {
                continue;
            }
            if number(&dict, "kCGWindowAlpha").unwrap_or(1.0) <= 0.0 {
                continue;
            }
            let Some(frame) = bounds(&dict) else { continue };
            let Some((r, logical_bounds)) = clipped_geometry(frame, clip, scale) else { continue };
            if r.width < MIN_SIZE || r.height < MIN_SIZE {
                continue;
            }
            targets.list.push(Target {
                hwnd: id, rect: r, title: string(&dict, "kCGWindowName"), class: owner, process_id: pid,
                depth: 0, parent: None, logical_bounds: Some(logical_bounds),
            });
            let index = targets.list.len() - 1;
            if trusted && walked < MAX_WALKED {
                walked += 1;
                targets.walk_window(pid, frame, index, logical_bounds, scale);
            }
        }
        layout.validate()?;
        Ok(targets)
    }

    /// Finds the accessibility window matching `frame` and adds its parts.
    fn walk_window(&mut self, pid: u32, frame: objc2_core_foundation::CGRect, index: usize, clip: LogicalBounds, scale: f64) {
        // SAFETY: creates an owned application element.
        let app = Ax(unsafe { AXUIElementCreateApplication(pid as i32) });
        if app.0.is_null() {
            return;
        }
        // SAFETY: a valid element; a short timeout keeps a hung app from
        // stalling the overlay.
        unsafe { AXUIElementSetMessagingTimeout(app.0, 0.25) };
        let Some(windows) = ax_attr(app.0, "AXWindows") else { return };
        // SAFETY: AXWindows is a CFArray of elements.
        let obj = unsafe { &*(windows.0 as *const AnyObject) };
        let Some(array) = obj.downcast_ref::<NSArray>() else { return };
        for w in array.iter() {
            let element = Retained::as_ptr(&w) as AXUIElementRef;
            if ax_rect(element).is_some_and(|bounds| same_native_bounds(bounds, frame)) {
                self.children(element, index, clip, scale, 1);
                break;
            }
        }
    }

    fn children(&mut self, parent: AXUIElementRef, parent_index: usize, clip: LogicalBounds, scale: f64, depth: u32) {
        if depth > MAX_DEPTH {
            return;
        }
        for child in ax_children(parent) {
            if self.list.len() >= MAX_CHILDREN {
                return;
            }
            let Some((r, logical_bounds)) = ax_rect(child.0).and_then(|bounds| clipped_geometry(bounds, clip, scale)) else { continue };
            if matches_parent_geometry(&self.list[parent_index], r, logical_bounds) {
                // Full-size groups add nothing to select; look through them.
                self.children(child.0, parent_index, logical_bounds, scale, depth + 1);
            } else if r.width >= MIN_SIZE && r.height >= MIN_SIZE {
                let pid = self.list[parent_index].process_id;
                let title = ax_string(child.0, "AXTitle");
                let title = if title.is_empty() { ax_string(child.0, "AXDescription") } else { title };
                let role = ax_string(child.0, "AXRole");
                self.list.push(Target {
                    hwnd: 0, rect: r, title, class: role, process_id: pid, depth, parent: Some(parent_index),
                    logical_bounds: Some(logical_bounds),
                });
                let index = self.list.len() - 1;
                self.children(child.0, index, logical_bounds, scale, depth + 1);
            }
        }
    }

    /// The target under a screen point: the deepest visible child of the
    /// topmost window there, moved `levels_up` steps towards its top-level
    /// window. Returns its index.
    pub fn hit(&self, x: i32, y: i32, levels_up: u32) -> Option<usize> {
        self.hit_matching(levels_up, |target| target.rect.contains(x, y))
    }

    /// Hit-tests an unrounded reference-grid point against retained Quartz bounds.
    pub fn hit_precise(&self, x: f64, y: f64, scale: f64, levels_up: u32) -> io::Result<Option<usize>> {
        let valid_grid = |value: f64| value.is_finite() && value.round() >= i32::MIN as f64 && value.round() <= i32::MAX as f64;
        if !valid_grid(x) || !valid_grid(y) || !scale.is_finite() || scale <= 0.0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid target hit-test coordinates"));
        }
        let (logical_x, logical_y) = (x / scale, y / scale);
        if !logical_x.is_finite() || !logical_y.is_finite() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid target hit-test coordinates"));
        }
        let grid = (x.round() as i32, y.round() as i32);
        Ok(self.hit_matching(levels_up, |target| match target.logical_bounds {
            Some(bounds) => {
                let [left, top, right, bottom] = bounds.edges();
                logical_x >= left && logical_x < right && logical_y >= top && logical_y < bottom
            }
            None => target.rect.contains(grid.0, grid.1),
        }))
    }

    fn hit_matching(&self, levels_up: u32, contains: impl Fn(&Target) -> bool) -> Option<usize> {
        let top = self.list.iter().position(|t| t.depth == 0 && contains(t))?;
        let end = self.list[top + 1..]
            .iter()
            .position(|t| t.depth == 0)
            .map_or(self.list.len(), |i| top + 1 + i);
        let mut best = top;
        for (i, t) in self.list.iter().enumerate().take(end).skip(top + 1) {
            // Siblings come in z-order, so the first match at a depth wins.
            if t.depth > self.list[best].depth && contains(t) && self.is_ancestor(best, i) {
                best = i;
            }
        }
        for _ in 0..levels_up {
            match self.list[best].parent {
                Some(parent) => best = parent,
                None => break,
            }
        }
        Some(best)
    }

    fn is_ancestor(&self, ancestor: usize, mut index: usize) -> bool {
        while let Some(parent) = self.list[index].parent {
            if parent == ancestor {
                return true;
            }
            index = parent;
        }
        false
    }

    /// How many levels the target at `index` has above it.
    pub fn depth_of(&self, mut index: usize) -> u32 {
        let mut depth = 0;
        while let Some(parent) = self.list[index].parent {
            depth += 1;
            index = parent;
        }
        depth
    }

    /// The top-level window that contains the target at `index`.
    pub fn top_level(&self, mut index: usize) -> &Target {
        while let Some(parent) = self.list[index].parent {
            index = parent;
        }
        &self.list[index]
    }
}

/// The window server's level for open menus (`kCGPopUpMenuWindowLevel`).
const POP_UP_MENU_LEVEL: f64 = 101.0;

/// Screen rectangles of the menus, drop-downs and context menus open now,
/// topmost first.
pub fn open_menus() -> Vec<Rect> {
    let own = std::process::id();
    window_list()
        .into_iter()
        .filter(|d| number(d, "kCGWindowLayer") == Some(POP_UP_MENU_LEVEL) && number(d, "kCGWindowOwnerPID").unwrap_or(0.0) as u32 != own)
        .filter_map(|d| bounds(&d).map(rect_from_points))
        .filter(|r| r.width > 8 && r.height > 8)
        .collect()
}

/// The process that owns the topmost on-screen window at a screen point
/// (desktop pixels), of any level.
pub fn window_owner_at(x: i32, y: i32) -> Option<u32> {
    window_list()
        .into_iter()
        .filter(|d| number(d, "kCGWindowAlpha").unwrap_or(1.0) > 0.0)
        .find(|d| bounds(d).map(rect_from_points).is_some_and(|r| r.contains(x, y)))
        .and_then(|d| number(&d, "kCGWindowOwnerPID"))
        .map(|pid| pid as u32)
}

fn editable_accessibility_role(role: &str, subrole: &str) -> bool {
    role.is_empty()
        || matches!(role, "AXTextField" | "AXTextArea" | "AXComboBox" | "AXSearchField" | "AXSecureTextField")
        || subrole == "AXSecureTextField"
}

fn step_element_description(mut read: impl FnMut(&str) -> String) -> (String, String) {
    let role = read("AXRole");
    let subrole = read("AXSubrole");
    let kind = read("AXRoleDescription");
    if editable_accessibility_role(&role, &subrole) {
        return (String::new(), kind);
    }

    for attribute in ["AXTitle", "AXDescription", "AXHelp"] {
        let name = read(attribute);
        if !name.trim().is_empty() {
            return (name, kind);
        }
    }
    (String::new(), kind)
}

/// What Accessibility says is at a screen point (desktop pixels): its name
/// and its kind ("button", "text field"), either possibly empty; `None`
/// without the Accessibility permission or anything there. Editable text
/// controls are described only by their kind, and values are never read.
pub fn element_at(x: i32, y: i32) -> Option<(String, String)> {
    match element_at_checked(x, y) {
        Ok(element) => element,
        Err(error) => {
            eprintln!("could not describe the clicked control: {error}");
            None
        }
    }
}

/// Hit-tests a control without making in-process AppKit accessibility calls off the main thread.
pub fn element_at_checked(x: i32, y: i32) -> io::Result<Option<(String, String)>> {
    let (x, y) = crate::screen::to_points(x as f64, y as f64);
    element_at_points_checked(x, y)
}

/// Hit-tests a control at an unrounded global Quartz point.
pub fn element_at_points_checked(x: f64, y: f64) -> io::Result<Option<(String, String)>> {
    if !x.is_finite() || !y.is_finite() || !(x as f32).is_finite() || !(y as f32).is_finite() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid Accessibility hit-test point"));
    }
    if !accessibility_trusted() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Jiffshot needs Accessibility permission to describe a control"));
    }
    if MainThreadMarker::new().is_none() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let skip = cancelled.clone();
        dispatch2::DispatchQueue::main().exec_async(move || {
            if !skip.load(std::sync::atomic::Ordering::Acquire) {
                let result = element_at_on_main(x, y);
                if sender.send(result).is_err() {
                    eprintln!("the Accessibility hit test finished after its requester stopped waiting");
                }
            }
        });
        return receiver.recv_timeout(std::time::Duration::from_secs(3)).unwrap_or_else(|error| {
            cancelled.store(true, std::sync::atomic::Ordering::Release);
            Err(io::Error::other(format!("could not wait for the Accessibility hit test: {error}")))
        });
    }
    element_at_on_main(x, y)
}

fn element_at_on_main(x: f64, y: f64) -> io::Result<Option<(String, String)>> {
    // SAFETY: an owned system-wide element and an owned hit-test result.
    let element = unsafe {
        let system = Ax(AXUIElementCreateSystemWide());
        if system.0.is_null() {
            return Err(io::Error::other("could not create the system-wide Accessibility element"));
        }
        let status = AXUIElementSetMessagingTimeout(system.0, 0.25);
        if status != 0 {
            return Err(io::Error::other(format!("could not set the Accessibility timeout (error {status})")));
        }
        let mut out: AXUIElementRef = std::ptr::null();
        let status = AXUIElementCopyElementAtPosition(system.0, x as f32, y as f32, &mut out);
        if status == -25212 {
            return Ok(None);
        }
        if status != 0 || out.is_null() {
            return Err(io::Error::other(format!("Accessibility hit testing failed (error {status})")));
        }
        let element = Ax(out);
        let status = AXUIElementSetMessagingTimeout(element.0, 0.25);
        if status != 0 {
            return Err(io::Error::other(format!("could not set the Accessibility timeout (error {status})")));
        }
        element
    };
    Ok(Some(step_element_description(|attribute| ax_string(element.0, attribute))))
}

fn known_address_field(role: &str, subrole: &str, identifier: &str, description: &str) -> bool {
    role == "AXTextField"
        && subrole != "AXSecureTextField"
        && (matches!(identifier.to_ascii_lowercase().as_str(), "urlbar-input" | "omnibox")
            || matches!(
                description.trim().to_ascii_lowercase().as_str(),
                "address and search bar" | "address bar" | "smart search field" | "search or enter website name"
            ))
}

fn browser_address_in_tree<T>(
    root: T,
    mut attribute: impl FnMut(&T, &str) -> String,
    mut children: impl FnMut(&T) -> Vec<T>,
) -> Option<String> {
    fn walk<T>(
        node: T,
        depth: u32,
        budget: &mut u32,
        in_toolbar: bool,
        attribute: &mut impl FnMut(&T, &str) -> String,
        children: &mut impl FnMut(&T) -> Vec<T>,
    ) -> Option<String> {
        if depth > 10 || *budget == 0 {
            return None;
        }
        *budget -= 1;
        let role = attribute(&node, "AXRole");
        if role == "AXWebArea" {
            return None;
        }
        let in_toolbar = in_toolbar || role == "AXToolbar";
        if in_toolbar && role == "AXTextField" {
            let subrole = attribute(&node, "AXSubrole");
            let identifier = attribute(&node, "AXIdentifier");
            let description = attribute(&node, "AXDescription");
            if known_address_field(&role, &subrole, &identifier, &description) {
                return Some(attribute(&node, "AXValue"));
            }
        }
        children(&node).into_iter().find_map(|child| walk(child, depth + 1, budget, in_toolbar, attribute, children))
    }

    walk(root, 0, &mut 400, false, &mut attribute, &mut children)
}

fn page_url(text: &str) -> Option<String> {
    let text = text.trim();
    (text.starts_with("https://") || text.starts_with("http://") || text.starts_with("about:")).then(|| text.to_owned())
}

/// The address a browser window (by window number) shows. Prefer an explicit
/// document URL; only read a text value from an identified address field in
/// the browser's toolbar, never from arbitrary page or form controls.
pub fn browser_url(window: isize) -> Option<String> {
    if !accessibility_trusted() {
        return None;
    }
    let dict = window_list().into_iter().find(|d| number(d, "kCGWindowNumber").unwrap_or(0.0) as isize == window)?;
    let pid = number(&dict, "kCGWindowOwnerPID")? as i32;
    let frame = bounds(&dict)?;
    // SAFETY: an owned application element.
    let app = Ax(unsafe { AXUIElementCreateApplication(pid) });
    if app.0.is_null() {
        return None;
    }
    // SAFETY: as in `walk_window`.
    unsafe { AXUIElementSetMessagingTimeout(app.0, 0.25) };
    let windows = ax_attr(app.0, "AXWindows")?;
    // SAFETY: AXWindows is a CFArray of elements.
    let array = unsafe { &*(windows.0 as *const AnyObject) }.downcast_ref::<NSArray>()?;
    let matching = array.iter().find(|w| {
        ax_rect(Retained::as_ptr(w) as AXUIElementRef).is_some_and(|a| (a.origin.x - frame.origin.x).abs() < 2.0 && (a.origin.y - frame.origin.y).abs() < 2.0)
    })?;
    let element = Retained::as_ptr(&matching) as AXUIElementRef;
    for attribute in ["AXDocument", "AXURL"] {
        if let Some(url) = ax_url_string(element, attribute).and_then(|value| page_url(&value)) {
            return Some(url);
        }
    }

    // SAFETY: `matching` keeps the element alive, and the added retain is
    // released by `Ax` after the bounded traversal.
    let root = Ax(unsafe { CFRetain(element) });
    let text = browser_address_in_tree(root, |node, attribute| ax_string(node.0, attribute), |node| ax_children(node.0))?;
    let text = text.trim();
    if text.is_empty() || text.chars().any(char::is_whitespace) {
        return None;
    }
    page_url(text).or_else(|| (!text.contains("://") && text.contains('.')).then(|| format!("https://{text}")))
}

/// The executable file name of a process, e.g. `Safari`.
pub fn process_name(process_id: u32) -> Option<String> {
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer size is passed with it.
    let len = unsafe { libc::proc_pidpath(process_id as i32, buffer.as_mut_ptr() as *mut c_void, buffer.len() as u32) };
    if len <= 0 {
        return None;
    }
    let path = String::from_utf8_lossy(&buffer[..len as usize]).into_owned();
    path.rsplit('/').next().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(rect: Rect, depth: u32, parent: Option<usize>) -> Target {
        Target { hwnd: 0, rect, title: String::new(), class: String::new(), process_id: 0, depth, parent, logical_bounds: None }
    }

    #[test]
    fn this_process_has_a_name() {
        assert!(process_name(std::process::id()).is_some_and(|n| !n.is_empty()));
    }

    #[test]
    fn accessibility_hit_test_rejects_invalid_logical_points() {
        for (x, y) in [(f64::NAN, 1.0), (1.0, f64::INFINITY), (f64::MAX, 1.0)] {
            assert_eq!(element_at_points_checked(x, y).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn window_bounds_use_the_retained_grid_and_reject_invalid_geometry() {
        assert_eq!(window_rect(rect(-10.25, 4.25, 20.5, 10.5), 2.0), Some(Rect::new(-21, 9, 42, 21)));
        assert_eq!(window_rect(rect(1.0, 2.0, 0.0, 5.0), 2.0), None);
        assert_eq!(window_rect(rect(f64::NAN, 2.0, 3.0, 4.0), 1.0), None);
        assert_eq!(window_rect(rect(i32::MAX as f64, 0.0, 1.0, 1.0), 2.0), None);
    }

    #[test]
    fn clipped_targets_preserve_logical_edges_across_reference_scales() {
        let clip = LogicalBounds::new([-5.125, -3.375, 9.875, 8.625]).unwrap();
        let frame = rect(-5.5, -2.875, 6.75, 4.125);
        let expected = [-5.125, -2.875, 1.25, 1.25];
        for scale in [1.0, 2.0, 4.0] {
            let (reference, logical) = clipped_geometry(frame, clip, scale).unwrap();
            assert_eq!(logical.edges(), expected);
            assert_eq!(Some(reference), grid_rect(expected, scale));
        }
        assert!(clipped_geometry(rect(20.0, 20.0, 1.0, 1.0), clip, 2.0).is_none());
        assert!(clipped_geometry(rect(f64::NAN, 0.0, 1.0, 1.0), clip, 2.0).is_none());
        assert!(clipped_geometry(rect(0.0, 0.0, 1.0, 1.0), clip, 0.0).is_none());
    }

    #[test]
    fn window_snapshot_clips_with_exact_retained_display_edges() {
        let layout = DisplayLayout::current().expect("an active display");
        let monitor = layout.monitors().iter().find(|monitor| monitor.primary).expect("a primary display");
        let scale = layout.desktop_scale();
        let [left, top, _, _] = layout.monitor_mapping(monitor).unwrap().desktop_bounds().map(|edge| edge / scale);
        let native_bounds = rect(left + 10.125, top + 10.125, 40.25, 30.75);
        let window = Window {
            hwnd: 1,
            title: "Fixture".into(),
            class: "Fixture app".into(),
            pid: 7,
            process: "fixture".into(),
            rect: window_rect(native_bounds, scale).unwrap(),
            minimized: false,
            on_screen: true,
            foreground: false,
            layer: 0,
            native_bounds,
            alpha: 1.0,
        };
        let (reference, logical) = window.clipped_to_layout(&layout).unwrap();
        assert_eq!(reference, window.rect);
        assert_eq!(logical.edges(), [
            native_bounds.origin.x,
            native_bounds.origin.y,
            native_bounds.origin.x + native_bounds.size.width,
            native_bounds.origin.y + native_bounds.size.height,
        ]);
        let mut mismatched = window;
        mismatched.rect.x += 1;
        assert!(mismatched.clipped_to_layout(&layout).is_err());
    }

    #[test]
    fn offscreen_identity_rejects_duplicate_titles_and_bounds() {
        let window = Window {
            hwnd: 1,
            title: "Fixture".into(),
            class: "Fixture app".into(),
            pid: 7,
            process: "fixture".into(),
            rect: Rect::new(0, 0, 100, 100),
            minimized: false,
            on_screen: false,
            foreground: false,
            layer: 0,
            native_bounds: rect(0.0, 0.0, 100.0, 100.0),
            alpha: 1.0,
        };
        assert!(unique_window_identity(&window, &[window.clone()]));
        let mut duplicate = window.clone();
        duplicate.hwnd = 2;
        duplicate.on_screen = true;
        duplicate.alpha = 0.0;
        assert!(!unique_window_identity(&window, &[window.clone(), duplicate.clone()]));
        duplicate.pid = 8;
        assert!(unique_window_identity(&window, &[window.clone(), duplicate.clone()]));
        duplicate.pid = window.pid;
        assert!(!unique_window_identity(&window, &[duplicate.clone()]));
        duplicate.native_bounds = rect(3.0, 0.0, 100.0, 100.0);
        assert!(unique_window_identity(&window, &[window.clone(), duplicate]));
        let mut untitled = window.clone();
        untitled.title.clear();
        assert!(!unique_window_identity(&untitled, &[untitled.clone()]));
    }

    #[test]
    fn step_descriptions_never_read_values_or_names_of_text_inputs() {
        for (role, subrole) in [
            ("AXTextField", ""),
            ("AXTextArea", ""),
            ("AXComboBox", ""),
            ("AXSearchField", ""),
            ("AXSecureTextField", ""),
            ("AXTextField", "AXSecureTextField"),
            ("AXUnknown", "AXSecureTextField"),
            ("", ""),
        ] {
            let mut queried = Vec::new();
            let result = step_element_description(|attribute| {
                queried.push(attribute.to_owned());
                match attribute {
                    "AXRole" => role.into(),
                    "AXSubrole" => subrole.into(),
                    "AXRoleDescription" => "text field".into(),
                    _ => panic!("read an input's {attribute}"),
                }
            });
            assert_eq!(result, (String::new(), "text field".into()));
            assert_eq!(queried, ["AXRole", "AXSubrole", "AXRoleDescription"]);
        }
    }

    #[test]
    fn step_descriptions_use_only_non_value_accessibility_labels() {
        let mut queried = Vec::new();
        let result = step_element_description(|attribute| {
            queried.push(attribute.to_owned());
            match attribute {
                "AXRole" => "AXButton".into(),
                "AXRoleDescription" => "button".into(),
                "AXDescription" => "Save".into(),
                "AXValue" => panic!("read a value"),
                _ => String::new(),
            }
        });
        assert_eq!(result, ("Save".into(), "button".into()));
        assert_eq!(queried, ["AXRole", "AXSubrole", "AXRoleDescription", "AXTitle", "AXDescription"]);
    }

    #[test]
    fn browser_address_only_reads_an_identified_nonsecure_toolbar_field() {
        let mut queried = Vec::new();
        let address = browser_address_in_tree(
            0,
            |node, attribute| {
                queried.push((*node, attribute.to_owned()));
                match (*node, attribute) {
                    (0, "AXRole") => "AXWindow",
                    (1, "AXRole") => "AXWebArea",
                    (2, "AXRole") | (4, "AXRole") | (5, "AXRole") | (6, "AXRole") => "AXTextField",
                    (3, "AXRole") => "AXToolbar",
                    (5, "AXSubrole") => "AXSecureTextField",
                    (5, "AXDescription") | (6, "AXDescription") => "Address and search bar",
                    (6, "AXValue") => "https://example.test/page",
                    (_, "AXValue") => panic!("read an unrelated or secure field"),
                    _ => "",
                }
                .to_owned()
            },
            |node| match *node {
                0 => vec![1, 2, 3],
                1 => vec![2],
                3 => vec![4, 5, 6],
                _ => Vec::new(),
            },
        );
        assert_eq!(address.as_deref(), Some("https://example.test/page"));
        assert!(!queried.iter().any(|(node, attribute)| *node == 2 && attribute != "AXRole"));
        assert_eq!(queried.iter().filter(|(_, attribute)| attribute == "AXValue").map(|(node, _)| *node).collect::<Vec<_>>(), [6]);
    }

    #[test]
    fn browser_address_does_not_guess_at_an_unidentified_field() {
        let mut queried = Vec::new();
        let address = browser_address_in_tree(
            0,
            |node, attribute| {
                queried.push((*node, attribute.to_owned()));
                match (*node, attribute) {
                    (0, "AXRole") => "AXToolbar",
                    (1, "AXRole") => "AXTextField",
                    (1, "AXDescription") => "Search this page",
                    (_, "AXValue") => panic!("read an unidentified field"),
                    _ => "",
                }
                .to_owned()
            },
            |node| if *node == 0 { vec![1] } else { Vec::new() },
        );
        assert_eq!(address, None);
        assert!(!queried.iter().any(|(_, attribute)| attribute == "AXValue"));
        assert_eq!(page_url(" file:///secret "), None);
        assert_eq!(page_url(" https://example.test/ "), Some("https://example.test/".into()));
    }

    #[test]
    fn hit_prefers_deepest_child_of_topmost_window() {
        let targets = Targets {
            list: vec![
                target(Rect::new(0, 0, 100, 100), 0, None),   // 0: front window
                target(Rect::new(0, 0, 100, 20), 1, Some(0)), // 1: toolbar
                target(Rect::new(0, 20, 100, 80), 1, Some(0)), // 2: body
                target(Rect::new(10, 30, 30, 30), 2, Some(2)), // 3: button in body
                target(Rect::new(0, 0, 400, 400), 0, None),   // 4: window behind
            ],
        };
        assert_eq!(targets.hit(15, 35, 0), Some(3));
        assert_eq!(targets.hit(15, 35, 1), Some(2));
        assert_eq!(targets.hit(15, 35, 9), Some(0));
        assert_eq!(targets.hit(50, 5, 0), Some(1));
        assert_eq!(targets.hit(200, 200, 0), Some(4));
        assert_eq!(targets.hit(500, 500, 0), None);
    }

    #[test]
    fn precise_hits_use_logical_target_edges_at_any_reference_scale() {
        let make = |bounds: [f64; 4], scale: f64, depth, parent| {
            let edges = bounds.map(|edge| (edge * scale).round() as i32);
            let mut item = target(Rect::from_ltrb(edges[0], edges[1], edges[2], edges[3]), depth, parent);
            item.logical_bounds = Some(LogicalBounds::new(bounds).unwrap());
            item
        };
        for scale in [1.0, 2.0, 4.0] {
            let targets = Targets { list: vec![
                make([-10.375, -4.25, 0.11, 6.25], scale, 0, None),
                make([-2.7, -1.0, -0.13, 3.0], scale, 1, Some(0)),
                make([0.11, -4.25, 10.25, 6.25], scale, 0, None),
                make([-10.0, -3.0, 10.0, 5.0], scale, 0, None),
            ] };
            let point = |x: f64, y: f64, levels| targets.hit_precise(x * scale, y * scale, scale, levels).unwrap();
            assert_eq!((0.105_f64 * scale).round(), (0.115_f64 * scale).round());
            assert_eq!(point(0.105, 1.0, 0), Some(0));
            assert_eq!(point(0.11, 1.0, 0), Some(2));
            assert_eq!(point(0.115, 1.0, 0), Some(2));
            assert_eq!(point(-0.135, 1.0, 0), Some(1));
            assert_eq!(point(-0.125, 1.0, 0), Some(0));
            assert_eq!(point(-0.135, 1.0, 1), Some(0));
            assert_eq!(point(11.0, 1.0, 0), None);
        }

        let fallback = Targets { list: vec![target(Rect::new(-5, -5, 10, 10), 0, None)] };
        assert_eq!(fallback.hit_precise(4.49, 0.0, 2.0, 0).unwrap(), Some(0));
        assert_eq!(fallback.hit_precise(4.5, 0.0, 2.0, 0).unwrap(), None);
        for (x, y, scale) in [(f64::NAN, 0.0, 1.0), (0.0, f64::INFINITY, 1.0), (0.0, 0.0, 0.0), (0.0, 0.0, f64::NAN)] {
            assert_eq!(fallback.hit_precise(x, y, scale, 0).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn fractional_child_is_not_flattened_into_an_identically_rounded_parent() {
        let rect = Rect::new(0, 0, 10, 10);
        let parent_bounds = LogicalBounds::new([0.1, 0.1, 9.9, 9.9]).unwrap();
        let child_bounds = LogicalBounds::new([0.2, 0.2, 9.8, 9.8]).unwrap();
        let mut parent = target(rect, 0, None);
        parent.logical_bounds = Some(parent_bounds);
        assert!(matches_parent_geometry(&parent, rect, parent_bounds));
        assert!(!matches_parent_geometry(&parent, rect, child_bounds));
        parent.logical_bounds = None;
        assert!(matches_parent_geometry(&parent, rect, child_bounds));
    }

    #[test]
    fn selectable_depth_excludes_flattened_accessibility_groups() {
        let rect = Rect::new(0, 0, 100, 100);
        let targets = Targets { list: vec![
            target(rect, 0, None),
            target(rect, 3, Some(0)),
            target(rect, 6, Some(1)),
        ] };
        assert_eq!(targets.depth_of(0), 0);
        assert_eq!(targets.depth_of(1), 1);
        assert_eq!(targets.depth_of(2), 2);
        assert_eq!(targets.hit(50, 50, 1), Some(1));
        assert_eq!(targets.hit(50, 50, 2), Some(0));
    }
}
