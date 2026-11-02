//! Bounded Accessibility enumeration for the control server.

use super::*;
use objc2_app_kit::NSWindow;
use objc2_foundation::NSPoint;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const MAX_NODES: usize = 5000;
const MAX_DEPTH: u32 = 32;
const MAX_WINDOWS: usize = 512;
const MAX_STRING_UNITS: isize = 8192;
const WALK_TIMEOUT: Duration = Duration::from_secs(12);
const MAIN_BATCH: usize = 8;
const AX_ATTRIBUTE_UNSUPPORTED: i32 = -25205;
const AX_API_DISABLED: i32 = -25211;
const AX_NO_VALUE: i32 = -25212;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementGetTypeID() -> usize;
    fn AXValueGetTypeID() -> usize;
    fn AXUIElementGetAttributeValueCount(element: AXUIElementRef, attribute: CFTypeRef, count: *mut isize) -> i32;
    fn AXUIElementCopyAttributeValues(
        element: AXUIElementRef,
        attribute: CFTypeRef,
        index: isize,
        maximum: isize,
        values: *mut CFTypeRef,
    ) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFGetTypeID(object: CFTypeRef) -> usize;
    fn CFHash(object: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFStringGetLength(string: CFTypeRef) -> isize;
    fn CFArrayGetTypeID() -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
}

#[derive(Clone, Debug, Serialize)]
pub struct AccessibleElement {
    pub name: String,
    pub control_type: String,
    pub automation_id: String,
    pub rect: Rect,
    pub center: (i32, i32),
    /// `None` means the application did not provide an enabled state.
    pub enabled: Option<bool>,
}

#[derive(Debug)]
pub struct AccessibleElements {
    pub elements: Vec<AccessibleElement>,
    /// More nodes or results existed than the bounded walk could inspect.
    pub truncated: bool,
}

fn ax_error(action: &str, status: i32) -> io::Error {
    let kind = match status {
        AX_API_DISABLED => io::ErrorKind::PermissionDenied,
        -25204 => io::ErrorKind::WouldBlock,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, format!("{action} (Accessibility error {status})"))
}

fn optional_attribute(element: AXUIElementRef, name: &str) -> io::Result<Option<Ax>> {
    let key = NSString::from_str(name);
    let mut value = std::ptr::null();
    // SAFETY: the element and CFString are live, and a successful value is owned.
    let status = unsafe { AXUIElementCopyAttributeValue(element, Retained::as_ptr(&key) as CFTypeRef, &mut value) };
    let value = Ax(value);
    if matches!(status, AX_ATTRIBUTE_UNSUPPORTED | AX_NO_VALUE) {
        return Ok(None);
    }
    if status != 0 {
        return Err(ax_error(&format!("could not read {name}"), status));
    }
    if value.0.is_null() {
        return Err(io::Error::other(format!("{name} returned a null Accessibility value")));
    }
    Ok(Some(value))
}

fn optional_string(element: AXUIElementRef, name: &str) -> io::Result<Option<String>> {
    let Some(value) = optional_attribute(element, name)? else { return Ok(None) };
    // SAFETY: the object is retained and its type is checked before conversion.
    unsafe {
        if CFGetTypeID(value.0) != CFStringGetTypeID() {
            return Err(io::Error::other(format!("{name} is not an Accessibility string")));
        }
        let length = CFStringGetLength(value.0);
        if !(0..=MAX_STRING_UNITS).contains(&length) {
            return Err(io::Error::other(format!("{name} exceeds the Accessibility string limit")));
        }
        Ok(Some((&*(value.0 as *const NSString)).to_string()))
    }
}

pub(super) fn optional_bool(element: AXUIElementRef, name: &str) -> io::Result<Option<bool>> {
    let Some(value) = optional_attribute(element, name)? else { return Ok(None) };
    // SAFETY: both CFBoolean and CFNumber are toll-free bridged to NSNumber.
    unsafe {
        let kind = CFGetTypeID(value.0);
        if kind != CFBooleanGetTypeID() && kind != CFNumberGetTypeID() {
            return Err(io::Error::other(format!("{name} is not an Accessibility Boolean")));
        }
        Ok(Some((&*(value.0 as *const NSNumber)).boolValue()))
    }
}

fn optional_rect(element: AXUIElementRef) -> io::Result<Option<CGRect>> {
    let Some(position) = optional_attribute(element, "AXPosition")? else { return Ok(None) };
    let Some(size) = optional_attribute(element, "AXSize")? else { return Ok(None) };
    let mut point = objc2_core_foundation::CGPoint { x: 0.0, y: 0.0 };
    let mut dimensions = objc2_core_foundation::CGSize { width: 0.0, height: 0.0 };
    // SAFETY: both retained objects are checked AXValues and output buffers match their kinds.
    let valid = unsafe {
        CFGetTypeID(position.0) == AXValueGetTypeID()
            && CFGetTypeID(size.0) == AXValueGetTypeID()
            && AXValueGetValue(position.0, AX_VALUE_CGPOINT, (&mut point as *mut objc2_core_foundation::CGPoint).cast())
            && AXValueGetValue(size.0, AX_VALUE_CGSIZE, (&mut dimensions as *mut objc2_core_foundation::CGSize).cast())
    };
    if !valid {
        return Err(io::Error::other("an Accessibility element has invalid position or size values"));
    }
    Ok(Some(rect(point.x, point.y, dimensions.width, dimensions.height)))
}

fn array_elements(element: AXUIElementRef, name: &str, limit: usize) -> io::Result<(Vec<Ax>, bool)> {
    let key = NSString::from_str(name);
    let mut count = 0isize;
    // SAFETY: the element, key and count pointer are valid.
    let status = unsafe { AXUIElementGetAttributeValueCount(element, Retained::as_ptr(&key) as CFTypeRef, &mut count) };
    if matches!(status, AX_ATTRIBUTE_UNSUPPORTED | AX_NO_VALUE) {
        return Ok((Vec::new(), false));
    }
    if status != 0 {
        return Err(ax_error(&format!("could not count {name}"), status));
    }
    let count = usize::try_from(count).map_err(|_| io::Error::other(format!("{name} has a negative element count")))?;
    let take = count.min(limit);
    if take == 0 {
        return Ok((Vec::new(), count > 0));
    }
    let mut values = std::ptr::null();
    // SAFETY: the requested range is bounded by the reported count.
    let status = unsafe {
        AXUIElementCopyAttributeValues(element, Retained::as_ptr(&key) as CFTypeRef, 0, take as isize, &mut values)
    };
    let values = Ax(values);
    if status != 0 {
        return Err(ax_error(&format!("could not enumerate {name}"), status));
    }
    if values.0.is_null() || unsafe { CFGetTypeID(values.0) != CFArrayGetTypeID() } {
        return Err(io::Error::other(format!("{name} did not return an Accessibility array")));
    }
    // SAFETY: the CFArray is toll-free bridged to NSArray and remains live.
    let array = unsafe { &*(values.0 as *const NSArray) };
    if array.len() != take {
        return Err(io::Error::other(format!("{name} changed while it was being enumerated")));
    }
    let mut elements = Vec::with_capacity(take);
    for item in array.iter() {
        let item = Retained::as_ptr(&item) as CFTypeRef;
        // SAFETY: the array retains the item, and its CF type is checked before retaining it.
        if unsafe { CFGetTypeID(item) != AXUIElementGetTypeID() } {
            return Err(io::Error::other(format!("{name} contains a non-Accessibility element")));
        }
        elements.push(Ax(unsafe { CFRetain(item) }));
    }
    Ok((elements, count > take))
}

fn timeout(element: AXUIElementRef) -> io::Result<()> {
    // SAFETY: the element is live and the timeout is finite.
    let status = unsafe { AXUIElementSetMessagingTimeout(element, 0.25) };
    if status != 0 {
        return Err(ax_error("could not set the Accessibility messaging timeout", status));
    }
    Ok(())
}

pub(super) fn selected_window(app: AXUIElementRef, window: &Window, deadline: Instant, require_title: bool) -> io::Result<Ax> {
    if require_title && window.title.is_empty() {
        return Err(io::Error::other("the off-screen window has no title for Accessibility matching"));
    }
    let (windows, truncated) = array_elements(app, "AXWindows", MAX_WINDOWS)?;
    if truncated {
        return Err(io::Error::other("the application has too many Accessibility windows to identify one safely"));
    }
    let mut matches = Vec::new();
    for candidate in windows {
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "Accessibility window matching timed out"));
        }
        timeout(candidate.0)?;
        if optional_rect(candidate.0)?.is_some_and(|bounds| same_native_bounds(bounds, window.native_bounds)) {
            matches.push(candidate);
        }
    }
    if (require_title || matches.len() > 1) && !window.title.is_empty() {
        let mut titled = Vec::new();
        for candidate in matches {
            if optional_string(candidate.0, "AXTitle")?.as_deref() == Some(window.title.as_str()) {
                titled.push(candidate);
            }
        }
        matches = titled;
    }
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(io::Error::new(io::ErrorKind::NotFound, "the selected window is not available through Accessibility")),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "the selected window matches more than one Accessibility window")),
    }
}

fn intersect(left: Rect, right: Rect) -> Option<Rect> {
    if left.is_empty() || right.is_empty() {
        return None;
    }
    let x = i64::from(left.x).max(i64::from(right.x));
    let y = i64::from(left.y).max(i64::from(right.y));
    let end_x = (i64::from(left.x) + i64::from(left.width)).min(i64::from(right.x) + i64::from(right.width));
    let end_y = (i64::from(left.y) + i64::from(left.height)).min(i64::from(right.y) + i64::from(right.height));
    if end_x <= x || end_y <= y {
        return None;
    }
    Some(Rect::new(i32::try_from(x).ok()?, i32::try_from(y).ok()?,
        i32::try_from(end_x - x).ok()?, i32::try_from(end_y - y).ok()?))
}

fn selected_still_visible(window: &Window, layout: &DisplayLayout) -> io::Result<bool> {
    Ok(windows(layout)?.into_iter().any(|current| {
        current.hwnd == window.hwnd && current.pid == window.pid
            && same_native_bounds(current.native_bounds, window.native_bounds)
    }))
}

struct Node {
    element: Ax,
    depth: u32,
    private_text: bool,
}

struct Walk {
    window: Window,
    layout: DisplayLayout,
    pending: Vec<Node>,
    seen: HashMap<usize, Vec<Ax>>,
    result: Vec<AccessibleElement>,
    visited: usize,
    max_results: usize,
    truncated: bool,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
    shutting_down: fn() -> bool,
}

enum Progress {
    Pending,
    Done(AccessibleElements),
}

impl Walk {
    fn new(
        window: Window,
        layout: DisplayLayout,
        max_results: usize,
        deadline: Instant,
        cancelled: Arc<AtomicBool>,
        shutting_down: fn() -> bool,
    ) -> io::Result<Self> {
        if !selected_still_visible(&window, &layout)? {
            return Err(io::Error::other("the selected window moved or disappeared before Accessibility enumeration"));
        }
        let app = application_element(window.pid)?;
        let root = selected_window(app.0, &window, deadline, false)?;
        Ok(Self {
            window,
            layout,
            pending: vec![Node { element: root, depth: 0, private_text: false }],
            seen: HashMap::new(),
            result: Vec::new(),
            visited: 0,
            max_results,
            truncated: false,
            deadline,
            cancelled,
            shutting_down,
        })
    }

    fn check(&self) -> io::Result<()> {
        if self.cancelled.load(Ordering::Acquire) || (self.shutting_down)() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "Accessibility enumeration was cancelled"));
        }
        if !accessibility_trusted() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Accessibility permission was revoked"));
        }
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "Accessibility enumeration timed out"));
        }
        Ok(())
    }

    fn advance(&mut self, batch: usize) -> io::Result<Progress> {
        for _ in 0..batch {
            self.check()?;
            let Some(node) = self.pending.pop() else { return self.finish() };
            // SAFETY: the element remains retained while hashing and comparing it.
            let hash = unsafe { CFHash(node.element.0) };
            let bucket = self.seen.entry(hash).or_default();
            if bucket.iter().any(|seen| unsafe { CFEqual(seen.0, node.element.0) }) {
                continue;
            }
            bucket.push(Ax(unsafe { CFRetain(node.element.0) }));
            self.visited += 1;
            timeout(node.element.0)?;

            let role = optional_string(node.element.0, "AXRole")?.unwrap_or_default();
            let subrole = optional_string(node.element.0, "AXSubrole")?.unwrap_or_default();
            let private_text = node.private_text || editable_accessibility_role(&role, &subrole);
            if node.depth > 0 {
                if let Some(bounds) = optional_rect(node.element.0)?
                    .and_then(|bounds| window_rect(bounds, self.layout.desktop_scale()))
                    .and_then(|bounds| intersect(bounds, self.window.rect))
                    .and_then(|bounds| intersect(bounds, self.layout.virtual_screen()))
                {
                    let control_type = optional_string(node.element.0, "AXRoleDescription")?
                        .filter(|value| !value.is_empty())
                        .unwrap_or_else(|| role.strip_prefix("AX").unwrap_or(&role).to_lowercase());
                    let mut name = String::new();
                    let mut automation_id = String::new();
                    if !private_text {
                        for attribute in ["AXTitle", "AXDescription", "AXHelp"] {
                            if let Some(value) = optional_string(node.element.0, attribute)?.filter(|value| !value.trim().is_empty()) {
                                name = value;
                                break;
                            }
                        }
                        automation_id = optional_string(node.element.0, "AXIdentifier")?.unwrap_or_default();
                    }
                    self.result.push(AccessibleElement {
                        name, control_type, automation_id, rect: bounds,
                        center: bounds.center(),
                        enabled: optional_bool(node.element.0, "AXEnabled")?,
                    });
                }
            }
            let room = if node.depth >= MAX_DEPTH {
                0
            } else {
                MAX_NODES.saturating_sub(self.visited.saturating_add(self.pending.len()))
            };
            let (children, truncated) = array_elements(node.element.0, "AXChildren", room)?;
            self.truncated |= truncated;
            self.pending.extend(children.into_iter().rev().map(|element| Node {
                element, depth: node.depth + 1, private_text,
            }));
            if self.result.len() >= self.max_results {
                self.truncated |= !self.pending.is_empty();
                return self.finish();
            }
        }
        Ok(Progress::Pending)
    }

    fn finish(&mut self) -> io::Result<Progress> {
        self.check()?;
        self.layout.validate()?;
        if !selected_still_visible(&self.window, &self.layout)? {
            return Err(io::Error::other("the selected window moved or disappeared during Accessibility enumeration"));
        }
        Ok(Progress::Done(AccessibleElements {
            elements: std::mem::take(&mut self.result),
            truncated: self.truncated,
        }))
    }
}

thread_local! {
    static MAIN_WALKS: RefCell<HashMap<u64, Walk>> = RefCell::new(HashMap::new());
}

static NEXT_WALK: AtomicU64 = AtomicU64::new(1);
type WalkSender = mpsc::SyncSender<io::Result<AccessibleElements>>;

fn send_result(sender: WalkSender, result: io::Result<AccessibleElements>, cancelled: &AtomicBool) {
    if sender.send(result).is_err() && !cancelled.load(Ordering::Acquire) {
        eprintln!("Accessibility enumeration finished after its requester stopped waiting");
    }
}

fn schedule_main_walk(id: u64, sender: WalkSender, cancelled: Arc<AtomicBool>) {
    dispatch2::DispatchQueue::main().exec_async(move || {
        objc2::rc::autoreleasepool(|_| {
            let Some(mut walk) = MAIN_WALKS.with(|walks| walks.borrow_mut().remove(&id)) else { return };
            match walk.advance(MAIN_BATCH) {
                Ok(Progress::Pending) if !cancelled.load(Ordering::Acquire) => {
                    MAIN_WALKS.with(|walks| { walks.borrow_mut().insert(id, walk); });
                    schedule_main_walk(id, sender, cancelled);
                }
                Ok(Progress::Pending) => {}
                Ok(Progress::Done(result)) => send_result(sender, Ok(result), &cancelled),
                Err(error) => send_result(sender, Err(error), &cancelled),
            }
        });
    });
}

/// Enumerates visible descendants without reading editable text values or labels.
///
/// In-process Accessibility calls are sliced on the main thread; callers must
/// invoke this from a worker thread. Partial results are marked `truncated`.
pub fn accessibility_elements(
    window: &Window,
    layout: &DisplayLayout,
    max_results: usize,
    shutting_down: fn() -> bool,
) -> io::Result<AccessibleElements> {
    if !(1..=MAX_NODES).contains(&max_results) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Accessibility result limit must be from 1 to 5000"));
    }
    if MainThreadMarker::new().is_some() {
        return Err(io::Error::other("Accessibility enumeration must be requested from a worker thread"));
    }
    if !request_accessibility() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Jiffshot needs Accessibility permission to enumerate controls"));
    }
    layout.validate()?;
    let deadline = Instant::now() + WALK_TIMEOUT;
    let cancelled = Arc::new(AtomicBool::new(false));
    if window.pid != std::process::id() {
        let mut walk = Walk::new(window.clone(), layout.clone(), max_results, deadline, cancelled, shutting_down)?;
        loop {
            if let Progress::Done(result) = walk.advance(64)? {
                return Ok(result);
            }
        }
    }

    let id = NEXT_WALK.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .map_err(|_| io::Error::other("Accessibility request identifiers are exhausted"))?;
    let (sender, receiver) = mpsc::sync_channel(1);
    let skip = cancelled.clone();
    let window = window.clone();
    let layout = layout.clone();
    dispatch2::DispatchQueue::main().exec_async(move || {
        objc2::rc::autoreleasepool(|_| {
            if skip.load(Ordering::Acquire) {
                return;
            }
            match Walk::new(window, layout, max_results, deadline, skip.clone(), shutting_down) {
                Ok(walk) => {
                    MAIN_WALKS.with(|walks| { walks.borrow_mut().insert(id, walk); });
                    schedule_main_walk(id, sender, skip);
                }
                Err(error) => send_result(sender, Err(error), &skip),
            }
        });
    });
    loop {
        if shutting_down() {
            cancelled.store(true, Ordering::Release);
            return Err(io::Error::new(io::ErrorKind::Interrupted, "Accessibility enumeration was cancelled"));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            cancelled.store(true, Ordering::Release);
            return Err(io::Error::new(io::ErrorKind::TimedOut, "Accessibility enumeration timed out"));
        }
        match receiver.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("Accessibility enumeration stopped without a result"));
            }
        }
    }
}

fn window_number_at_point(x: f64, y: f64) -> io::Result<isize> {
    let marker = MainThreadMarker::new().ok_or_else(|| io::Error::other("window hit testing must run on the main thread"))?;
    let point = NSPoint::new(x, crate::screen::main_height_points() - y);
    Ok(NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(point, 0, marker))
}

/// Checks that a selected, unmoved window is the actual on-screen hit target.
pub fn window_at_point(window: &Window, layout: &DisplayLayout, point: (i32, i32)) -> io::Result<bool> {
    let (x, y) = layout.native_point(point.0, point.1)?;
    if !selected_still_visible(window, layout)? {
        return Ok(false);
    }
    let number = if MainThreadMarker::new().is_some() {
        window_number_at_point(x, y)?
    } else {
        let (sender, receiver) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let skip = cancelled.clone();
        dispatch2::DispatchQueue::main().exec_async(move || {
            if !skip.load(Ordering::Acquire) {
                let result = window_number_at_point(x, y);
                if sender.send(result).is_err() && !skip.load(Ordering::Acquire) {
                    eprintln!("window hit testing finished after its requester stopped waiting");
                }
            }
        });
        receiver.recv_timeout(Duration::from_secs(3)).unwrap_or_else(|error| {
            cancelled.store(true, Ordering::Release);
            Err(io::Error::other(format!("could not wait for window hit testing: {error}")))
        })?
    };
    layout.validate()?;
    Ok(number == window.hwnd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersections_do_not_overflow_and_remain_inside_both_rectangles() {
        assert_eq!(intersect(Rect::new(-5, 2, 20, 10), Rect::new(0, 0, 10, 10)), Some(Rect::new(0, 2, 10, 8)));
        assert_eq!(intersect(Rect::new(i32::MIN, 0, 1, 1), Rect::new(i32::MAX - 1, 0, 1, 1)), None);
        assert_eq!(intersect(Rect::new(0, 0, 0, 1), Rect::new(0, 0, 1, 1)), None);
    }
}
