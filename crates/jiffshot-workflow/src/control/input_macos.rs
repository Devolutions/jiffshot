//! Serialized macOS pointer and keyboard automation for the control server.

use super::{capture, find, internal, invalid, not_found, point_json, steps, Args, Reply};
use jiffshot_capture::macos::ffi::{
    CFTypeRef, CGEventCreate, CGEventCreateKeyboardEvent, CGEventCreateMouseEvent,
    CGEventCreateScrollWheelEvent2, CGEventGetLocation, CGEventKeyboardSetUnicodeString,
    CGEventPost, CGEventSetFlags, CGEventSetIntegerValueField, CGEventSourceButtonState,
    CGEventSourceFlagsState, CGEventSourceKeyState, CGWarpMouseCursorPosition, Owned,
    kCGEventSourceStateCombinedSessionState, kCGHIDEventTap, kCGScrollEventUnitLine,
};
use jiffshot_capture::screen::DisplayLayout;
use jiffshot_capture::targets::{self, Window};
use jiffshot_control::{Error, ErrorCode};
use objc2_core_foundation::CGPoint;
use serde_json::{json, Value};
use std::cell::Cell;
use std::ffi::c_void;
use std::ptr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

static INPUT: Mutex<()> = Mutex::new(());
thread_local! {
    static INPUT_HELD: Cell<bool> = const { Cell::new(false) };
}

pub(super) fn serialized<T>(action: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
    INPUT_HELD.with(|held| {
        if held.get() {
            return action();
        }
        let _lock = INPUT.lock().map_err(|_| internal("the input automation state is unavailable"))?;
        held.set(true);
        struct Clear<'a>(&'a Cell<bool>);
        impl Drop for Clear<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let _clear = Clear(held);
        action()
    })
}

const POINTER: &[&str] = &["at", "text", "element", "type", "nth", "offset"];
const MOUSE_MOVED: u32 = 5;
const CLICK_STATE: u32 = 1;
const SHIFT: Modifier = Modifier { code: 0x38, flag: 1 << 17 };
const CONTROL: Modifier = Modifier { code: 0x3b, flag: 1 << 18 };
const OPTION: Modifier = Modifier { code: 0x3a, flag: 1 << 19 };
const COMMAND: Modifier = Modifier { code: 0x37, flag: 1 << 20 };
const MODIFIER_FLAGS: u64 = SHIFT.flag | CONTROL.flag | OPTION.flag | COMMAND.flag;
const SCROLL_PIXEL_LIMIT: u64 = 64_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    fn number(self) -> u32 {
        match self {
            Self::Left => 0,
            Self::Right => 1,
            Self::Middle => 2,
        }
    }

    fn events(self) -> (u32, u32, u32) {
        match self {
            Self::Left => (1, 2, 6),
            Self::Right => (3, 4, 7),
            Self::Middle => (25, 26, 27),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Middle => "middle",
        }
    }
}

fn button(a: &Args) -> Result<Button, Error> {
    if a.bool("right")? {
        return Ok(Button::Right);
    }
    if a.bool("middle")? {
        return Ok(Button::Middle);
    }
    match a.string("button")?.as_deref().map(str::to_ascii_lowercase).as_deref() {
        None | Some("left") => Ok(Button::Left),
        Some("right") => Ok(Button::Right),
        Some("middle") => Ok(Button::Middle),
        Some(_) => Err(invalid("button must be left, right or middle")),
    }
}

fn duration(a: &Args, key: &str, default: u64) -> Result<Duration, Error> {
    Ok(Duration::from_millis(a.int(key, 0, 3_600_000)?.unwrap_or(default as i64) as u64))
}

fn glide(a: &Args) -> Result<Duration, Error> {
    duration(a, "glide_ms", if crate::recorder::is_recording() { 400 } else { 0 })
}

fn prepare() -> Result<(), Error> {
    jiffshot_capture::os::require_event_posting()
        .map_err(|error| Error::new(ErrorCode::Conflict, error.to_string()))?;
    check()
}

fn check() -> Result<(), Error> {
    if crate::shutdown_macos::requested() {
        return Err(Error::new(ErrorCode::Conflict, "Jiffshot is shutting down"));
    }
    if !jiffshot_capture::os::event_posting_allowed() {
        return Err(Error::new(ErrorCode::Conflict, "Accessibility permission to post input was revoked"));
    }
    Ok(())
}

fn wait(delay: Duration) -> Result<(), Error> {
    let started = Instant::now();
    loop {
        check()?;
        let remaining = delay.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Ok(());
        }
        std::thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

fn checked_point(layout: &DisplayLayout, point: (i32, i32)) -> Result<CGPoint, Error> {
    let (x, y) = layout.native_point(point.0, point.1).map_err(|error| {
        if error.kind() == std::io::ErrorKind::InvalidInput {
            invalid(error.to_string())
        } else {
            Error::new(ErrorCode::Conflict, error.to_string())
        }
    })?;
    Ok(CGPoint { x, y })
}

fn current_native() -> Result<CGPoint, Error> {
    // SAFETY: the returned event is owned and only queried while it is live.
    let event = Owned(unsafe { CGEventCreate(ptr::null_mut()) }.cast_const());
    if event.0.is_null() {
        return Err(internal("could not read the current pointer position"));
    }
    Ok(unsafe { CGEventGetLocation(event.0.cast_mut()) })
}

fn current_point(layout: &DisplayLayout) -> Result<(i32, i32), Error> {
    layout.validate().map_err(internal)?;
    let point = desktop_point(layout, current_native()?)?;
    layout.validate().map_err(internal)?;
    Ok(point)
}

fn desktop_point(layout: &DisplayLayout, point: CGPoint) -> Result<(i32, i32), Error> {
    let scale = layout.desktop_scale();
    let values = [point.x * scale, point.y * scale];
    if !values.iter().all(|value| value.is_finite() && (i32::MIN as f64..=i32::MAX as f64).contains(&value.round())) {
        return Err(internal("the current pointer position is invalid"));
    }
    Ok((values[0].round() as i32, values[1].round() as i32))
}

fn ensure_buttons_released() -> Result<(), Error> {
    for button in [Button::Left, Button::Right, Button::Middle] {
        // SAFETY: this is a plain query of the current session's input state.
        if unsafe { CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, button.number()) } {
            return Err(Error::new(ErrorCode::Conflict, "release the held mouse buttons before running input automation"));
        }
    }
    Ok(())
}

fn ensure_modifiers_released() -> Result<(), Error> {
    // SAFETY: this is a plain query of the current session's input state.
    let flags = unsafe { CGEventSourceFlagsState(kCGEventSourceStateCombinedSessionState) };
    if flags & MODIFIER_FLAGS != 0 {
        return Err(Error::new(ErrorCode::Conflict, "release held modifier keys before running input automation"));
    }
    Ok(())
}

fn mouse_event(kind: u32, button: Button, point: CGPoint, count: u32, warp: bool) -> Result<(), Error> {
    // SAFETY: Core Graphics owns the new event until `Owned` releases it.
    let event = Owned(unsafe { CGEventCreateMouseEvent(ptr::null_mut(), kind, point, button.number()) }.cast_const());
    if event.0.is_null() {
        return Err(internal("could not create a mouse event"));
    }
    // SAFETY: the event is live and the click count is a documented field.
    unsafe {
        CGEventSetIntegerValueField(event.0.cast_mut(), CLICK_STATE, i64::from(count));
        if warp {
            let status = CGWarpMouseCursorPosition(point);
            if status != 0 {
                return Err(internal(format!("could not move the pointer (Core Graphics error {status})")));
            }
        }
        CGEventPost(kCGHIDEventTap, event.0.cast_mut());
    }
    Ok(())
}

fn move_to(layout: &DisplayLayout, to: (i32, i32), duration: Duration, dragging: Option<Button>) -> Result<(), Error> {
    checked_point(layout, to)?;
    let from = current_point(layout)?;
    let duration = if from == to { Duration::ZERO } else { duration };
    let button = dragging.unwrap_or(Button::Left);
    let kind = dragging.map_or(MOUSE_MOVED, |button| button.events().2);
    let start = Instant::now();
    let mut previous = None;
    loop {
        check()?;
        let t = if duration.is_zero() { 1.0 } else { (start.elapsed().as_secs_f64() / duration.as_secs_f64()).min(1.0) };
        let eased = if t < 0.5 { 2.0 * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(2) / 2.0 };
        let point = (
            (f64::from(from.0) + (f64::from(to.0) - f64::from(from.0)) * eased).round() as i32,
            (f64::from(from.1) + (f64::from(to.1) - f64::from(from.1)) * eased).round() as i32,
        );
        if previous != Some(point) || t >= 1.0 {
            let native = checked_point(layout, point)?;
            mouse_event(kind, button, native, 1, true)?;
            previous = Some(point);
        }
        if t >= 1.0 {
            return Ok(());
        }
        wait(Duration::from_millis(8).min(duration.saturating_sub(start.elapsed())))?;
    }
}

struct PressedButton {
    button: Button,
    point: CGPoint,
    count: u32,
    down: bool,
}

impl PressedButton {
    fn press(button: Button, point: CGPoint, count: u32) -> Result<Self, Error> {
        check()?;
        mouse_event(button.events().0, button, point, count, false)?;
        Ok(Self { button, point, count, down: true })
    }

    fn release(&mut self) -> Result<(), Error> {
        let point = match current_native() {
            Ok(point) => point,
            Err(error) => {
                crate::log(&format!("control: could not read the pointer while releasing a button: {error}"));
                self.point
            }
        };
        mouse_event(self.button.events().1, self.button, point, self.count, false)?;
        self.down = false;
        Ok(())
    }
}

impl Drop for PressedButton {
    fn drop(&mut self) {
        if self.down {
            if let Err(error) = self.release() {
                crate::log(&format!("control: could not release an injected mouse button: {error}"));
            }
        }
    }
}

fn shifted(point: (i32, i32), offset: (i32, i32)) -> Result<(i32, i32), Error> {
    point.0.checked_add(offset.0).zip(point.1.checked_add(offset.1))
        .ok_or_else(|| invalid("pointer offset overflows screen coordinates"))
}

fn locate(a: &Args, layout: &DisplayLayout) -> Result<((i32, i32), Value, Option<Window>), Error> {
    let nth = a.int("nth", 1, 1000)?.unwrap_or(1) as usize;
    let offset = a.point("offset")?.unwrap_or((0, 0));
    if let Some(point) = a.point("at")? {
        return Ok((shifted(point, offset)?, Value::Null, None));
    }
    if let Some(phrase) = a.string("text")? {
        let target = capture::target(a, true)?;
        let lines = capture::recognize(&target)?;
        let found = capture::find_text(&lines, &phrase);
        let hit = found.get(nth - 1).ok_or_else(|| not_found(format!("text {phrase:?} not found ({} matches)", found.len())))?;
        return Ok((shifted(hit.center, offset)?, json!({ "text": hit }), target.window));
    }
    if let Some(name) = a.string("element")? {
        let (window, hit) = find::element_target(a, layout, &name, nth)?;
        return Ok((shifted(hit.center, offset)?, json!({ "element": hit }), Some(window)));
    }
    Err(invalid("give at (X,Y), text (to find by OCR) or element (an Accessibility name)"))
}

fn ensure_target_hit(window: Option<&Window>, layout: &DisplayLayout, point: (i32, i32)) -> Result<(), Error> {
    if let Some(window) = window {
        if !targets::window_at_point(window, layout, point).map_err(internal)? {
            return Err(Error::new(ErrorCode::Conflict,
                "the selected window moved, disappeared or is covered at the target point; no further input can be sent there"));
        }
    }
    Ok(())
}

fn allowed<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut names: Vec<&'a str> = capture::TARGET.iter().chain(POINTER).copied().collect();
    names.extend_from_slice(extra);
    names
}

fn click(a: &Args) -> Reply {
    a.only(&allowed(&["button", "right", "middle", "double", "count", "step", "description", "glide_ms", "settle_ms"]))?;
    let button = button(a)?;
    let count = a.int("count", 1, 3)?.unwrap_or(1) as u32;
    let count = if a.bool("double")? { 2 } else { count };
    let requested = a.flag("step")?;
    let description = a.string("description")?;
    let glide = glide(a)?;
    let settle = duration(a, "settle_ms", 250)?;
    steps::require_for_click(requested)?;
    prepare()?;
    let layout = DisplayLayout::current().map_err(internal)?;
    let (at, matched, window) = locate(a, &layout)?;
    checked_point(&layout, at)?;
    ensure_buttons_released()?;
    ensure_target_hit(window.as_ref(), &layout, at)?;
    move_to(&layout, at, glide, None)?;
    wait(Duration::from_millis(40))?;
    let step = steps::click_with_step(requested, at, button == Button::Right, description, || {
        for index in 0..count {
            if index > 0 {
                wait(Duration::from_millis(50))?;
            }
            ensure_target_hit(window.as_ref(), &layout, at)?;
            if current_point(&layout)? != at {
                return Err(Error::new(ErrorCode::Conflict, "the pointer moved while the click was being prepared"));
            }
            let point = checked_point(&layout, at)?;
            let mut pressed = PressedButton::press(button, point, index + 1)?;
            wait(Duration::from_millis(30))?;
            pressed.release()?;
        }
        Ok(())
    })?;
    wait(settle)?;
    Ok(json!({ "at": point_json(at), "button": button.name(), "count": count, "matched": matched, "step": step }))
}

fn move_pointer(a: &Args) -> Reply {
    a.only(&allowed(&["glide_ms"]))?;
    let glide = glide(a)?;
    prepare()?;
    let layout = DisplayLayout::current().map_err(internal)?;
    let (at, matched, window) = locate(a, &layout)?;
    ensure_buttons_released()?;
    ensure_target_hit(window.as_ref(), &layout, at)?;
    move_to(&layout, at, glide, None)?;
    Ok(json!({ "at": point_json(at), "matched": matched }))
}

fn drag(a: &Args) -> Reply {
    a.only(&["from", "to", "button", "right", "middle", "glide_ms", "settle_ms"])?;
    let from = a.point("from")?.ok_or_else(|| invalid("from is required"))?;
    let to = a.point("to")?.ok_or_else(|| invalid("to is required"))?;
    let button = button(a)?;
    let glide = duration(a, "glide_ms", 500)?;
    let settle = duration(a, "settle_ms", 250)?;
    prepare()?;
    let layout = DisplayLayout::current().map_err(internal)?;
    checked_point(&layout, from)?;
    checked_point(&layout, to)?;
    ensure_buttons_released()?;
    move_to(&layout, from, glide / 2, None)?;
    wait(Duration::from_millis(60))?;
    let mut pressed = PressedButton::press(button, checked_point(&layout, from)?, 1)?;
    wait(Duration::from_millis(80))?;
    move_to(&layout, to, glide.max(Duration::from_millis(250)), Some(button))?;
    wait(Duration::from_millis(80))?;
    pressed.release()?;
    wait(settle)?;
    Ok(json!({ "from": point_json(from), "to": point_json(to) }))
}

fn scroll(a: &Args) -> Reply {
    a.only(&allowed(&["amount", "horizontal", "glide_ms", "settle_ms"]))?;
    let amount = a.int("amount", -1000, 1000)?.ok_or_else(|| invalid("amount is required"))? as i32;
    let horizontal = a.bool("horizontal")?;
    let glide = glide(a)?;
    let settle = duration(a, "settle_ms", 250)?;
    prepare()?;
    let layout = DisplayLayout::current().map_err(internal)?;
    let pointed = a.has("at") || a.has("text") || a.has("element");
    let targeted = capture::TARGET.iter().any(|key| *key != "no_activate" && a.has(key));
    let (at, window) = if pointed {
        let (at, _, window) = locate(a, &layout)?;
        (Some(at), window)
    } else if targeted {
        let target = capture::target(a, false)?;
        (Some(target.rect.center()), target.window)
    } else {
        (None, None)
    };
    let at = match at {
        Some(at) => {
            ensure_buttons_released()?;
            ensure_target_hit(window.as_ref(), &layout, at)?;
            move_to(&layout, at, glide, None)?;
            wait_for_posted_pointer(&layout, at, checked_point(&layout, at)?)?;
            at
        }
        None => current_point(&layout)?,
    };
    for _ in 0..amount.unsigned_abs() {
        check()?;
        layout.validate().map_err(internal)?;
        ensure_target_hit(window.as_ref(), &layout, at)?;
        ensure_buttons_released()?;
        if current_point(&layout)? != at {
            return Err(Error::new(ErrorCode::Conflict, "the pointer moved; no further scroll events were sent"));
        }
        let (vertical, horizontal_delta) = if horizontal { (0, -amount.signum()) } else { (-amount.signum(), 0) };
        post_scroll(vertical, horizontal_delta)?;
        wait(Duration::from_millis(30))?;
    }
    wait(settle)?;
    Ok(json!({ "at": point_json(at), "amount": amount, "horizontal": horizontal }))
}

fn post_scroll(vertical: i32, horizontal: i32) -> Result<(), Error> {
    // SAFETY: Core Graphics owns the new scroll event until it is released.
    let event = Owned(unsafe {
        CGEventCreateScrollWheelEvent2(ptr::null_mut(), kCGScrollEventUnitLine, 2, vertical, horizontal, 0)
    }.cast_const());
    if event.0.is_null() {
        return Err(internal("could not create a scroll event"));
    }
    // SAFETY: the event remains live through posting.
    unsafe { CGEventPost(kCGHIDEventTap, event.0.cast_mut()) };
    Ok(())
}

fn wait_for_posted_pointer(layout: &DisplayLayout, expected: (i32, i32), native: CGPoint) -> Result<(), Error> {
    let started = Instant::now();
    loop {
        layout.validate().map_err(internal)?;
        let actual_native = current_native()?;
        let actual = desktop_point(layout, actual_native)?;
        if actual == expected {
            return Ok(());
        }
        if (actual_native.x - native.x).abs() > 1.0 || (actual_native.y - native.y).abs() > 1.0 {
            return Err(Error::new(ErrorCode::Conflict, "the pointer moved while a mouse event was being delivered"));
        }
        if started.elapsed() >= Duration::from_millis(250) {
            return Err(Error::new(ErrorCode::Conflict, format!(
                "the pointer did not reach its requested position: expected {expected:?}, observed {actual:?}"
            )));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct RestorePointer<'a> {
    layout: &'a DisplayLayout,
    original: (i32, i32),
    original_native: CGPoint,
    expected: (i32, i32),
    armed: bool,
}

impl RestorePointer<'_> {
    fn restore(&mut self) -> Result<(), Error> {
        if !self.armed {
            return Ok(());
        }
        self.armed = false;
        if current_point(self.layout)? != self.expected {
            return Err(Error::new(ErrorCode::Conflict, "the pointer moved; its original position was not restored"));
        }
        checked_point(self.layout, self.original)?;
        mouse_event(MOUSE_MOVED, Button::Left, self.original_native, 1, true)?;
        wait_for_posted_pointer(self.layout, self.original, self.original_native)
    }
}

impl Drop for RestorePointer<'_> {
    fn drop(&mut self) {
        if self.armed {
            if let Err(error) = self.restore() {
                crate::log(&format!("control: could not restore the pointer after scrolling capture: {error}"));
            }
        }
    }
}

fn check_scroll_position(layout: &DisplayLayout, expected: (i32, i32)) -> Result<(), Error> {
    if current_point(layout)? != expected {
        return Err(Error::new(ErrorCode::Conflict, "the pointer moved; scrolling capture stopped"));
    }
    Ok(())
}

fn wait_scroll(layout: &DisplayLayout, expected: (i32, i32), delay: Duration) -> Result<(), Error> {
    let started = Instant::now();
    loop {
        check()?;
        check_scroll_position(layout, expected)?;
        let remaining = delay.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Ok(());
        }
        std::thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

fn scroll_pixel_limit_exceeded(pixels_per_frame: u64, frame_count: usize) -> bool {
    pixels_per_frame.checked_mul(frame_count as u64).is_none_or(|pixels| pixels > SCROLL_PIXEL_LIMIT)
}

fn capture_scroll(a: &Args) -> Reply {
    let mut names = capture::TARGET.to_vec();
    names.extend_from_slice(&["out", "horizontal", "max_frames", "timeout_ms", "settle_ms", "notches"]);
    a.only(&names)?;
    let out = a.out("out", &["png"])?;
    let horizontal = a.bool("horizontal")?;
    let max_frames = a.int("max_frames", 2, 128)?.unwrap_or(32) as usize;
    let timeout = duration(a, "timeout_ms", 30_000)?;
    let settle = duration(a, "settle_ms", 300)?;
    if timeout.is_zero() || settle.is_zero() {
        return Err(invalid("timeout_ms and settle_ms must be positive"));
    }
    let notches = a.int("notches", 1, 20)?.unwrap_or(3) as i32;
    prepare()?;
    let target = capture::target(a, false)?;
    let layout = target.layout.display();
    let mapping = target.layout.mapping(target.rect).map_err(internal)?;
    let (width, height) = mapping.size();
    let pixels_per_frame = u64::from(width) * u64::from(height);
    if scroll_pixel_limit_exceeded(pixels_per_frame, 1) {
        return Err(invalid("scroll capture target exceeds 64 million pixels"));
    }
    let crate::workflow::Capture { frame: initial_frame, mut meta, .. } = crate::workflow::immediate_in_layout(
        jiffshot_capture::library::CaptureKind::Scrolling, target.rect, false, &target.layout,
    ).map_err(internal)?;
    drop(initial_frame);
    if let Some(window) = &target.window {
        meta.window_title = Some(window.title.clone());
        meta.process = Some(window.process.clone());
    }
    layout.validate().map_err(internal)?;
    let original_native = current_native()?;
    let original = desktop_point(layout, original_native)?;
    checked_point(layout, original)?;
    let center = target.rect.center();
    let center_native = checked_point(layout, center)?;
    ensure_buttons_released()?;
    ensure_modifiers_released()?;
    ensure_target_hit(target.window.as_ref(), layout, center)?;
    move_to(layout, center, Duration::ZERO, None)?;
    let mut restore = RestorePointer { layout, original, original_native, expected: center, armed: true };
    let outcome = (|| {
        wait_for_posted_pointer(layout, center, center_native)?;
        let started = Instant::now();
        wait_scroll(layout, center, settle.min(timeout))?;
        ensure_target_hit(target.window.as_ref(), layout, center)?;
        let transform = |frame: jiffshot_capture::Frame| if horizontal { crate::modes::transpose(&frame) } else { frame };
        let first = target.layout.grab(target.rect).map_err(internal)?;
        check_scroll_position(layout, center)?;
        ensure_target_hit(target.window.as_ref(), layout, center)?;
        let mut frames = vec![transform(first)];
        let mut stopped = "frame_limit";
        while frames.len() < max_frames {
            check()?;
            if started.elapsed().saturating_add(settle) > timeout {
                stopped = "deadline";
                break;
            }
            if scroll_pixel_limit_exceeded(pixels_per_frame, frames.len() + 1) {
                stopped = "pixel_limit";
                break;
            }
            ensure_buttons_released()?;
            ensure_modifiers_released()?;
            ensure_target_hit(target.window.as_ref(), layout, center)?;
            check_scroll_position(layout, center)?;
            let amount = -notches * 3;
            post_scroll(if horizontal { 0 } else { amount }, if horizontal { amount } else { 0 })?;
            wait_scroll(layout, center, settle)?;
            ensure_target_hit(target.window.as_ref(), layout, center)?;
            let next = target.layout.grab(target.rect).map_err(internal)?;
            check_scroll_position(layout, center)?;
            ensure_target_hit(target.window.as_ref(), layout, center)?;
            let next = transform(next);
            let previous = frames.last().ok_or_else(|| internal("no initial scroll frame"))?;
            if next.pixels == previous.pixels {
                stopped = "no_movement";
                break;
            }
            if jiffshot_capture::stitch::vertical_step(previous, &next).is_none() {
                return Err(invalid("scroll frames have no matching overlap; use a content-only region, fewer notches or a longer settle_ms"));
            }
            frames.push(next);
        }
        let count = frames.len();
        let stitched = jiffshot_capture::stitch::stitch_vertical(&frames)
            .ok_or_else(|| internal("could not stitch scroll frames"))?;
        check()?;
        check_scroll_position(layout, center)?;
        ensure_target_hit(target.window.as_ref(), layout, center)?;
        Ok((transform(stitched), count, stopped))
    })();
    let (frame, count, stopped) = match (outcome, restore.restore()) {
        (Ok(result), Ok(())) => result,
        (Err(mut error), Err(restoration)) => {
            error.message = format!("{}; pointer restoration failed: {}", error.message, restoration.message);
            return Err(error);
        }
        (Err(error), Ok(())) | (Ok(_), Err(error)) => return Err(error),
    };
    check()?;
    target.layout.validate().map_err(internal)?;
    meta.width = frame.width;
    meta.height = frame.height;
    let path = match out {
        Some(path) => {
            frame.save_png(&path).map_err(internal)?;
            path
        }
        None => jiffshot_capture::library::Library::new(jiffshot_capture::config::library_root())
            .save(&frame, &meta).map_err(internal)?,
    };
    Ok(json!({
        "path": path, "width": frame.width, "height": frame.height,
        "frames": count, "stopped": stopped, "limited": stopped != "no_movement",
        "captured_at": meta.captured_at, "target": target.json(),
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Modifier {
    code: u16,
    flag: u64,
}

#[derive(Debug, PartialEq, Eq)]
enum Key {
    Virtual(u16, Option<Modifier>),
    Character(char),
}

#[derive(Debug, PartialEq, Eq)]
struct Combo {
    modifiers: Vec<Modifier>,
    key: Key,
}

fn modifier(name: &str) -> Option<Modifier> {
    match name {
        "shift" => Some(SHIFT),
        "ctrl" | "control" => Some(CONTROL),
        "alt" | "option" | "menu" => Some(OPTION),
        "cmd" | "command" | "win" | "windows" | "meta" | "super" => Some(COMMAND),
        _ => None,
    }
}

fn named_key(name: &str) -> Option<Key> {
    if let Some(modifier) = modifier(name) {
        return Some(Key::Virtual(modifier.code, Some(modifier)));
    }
    let code = match name {
        "enter" | "return" => 0x24,
        "tab" => 0x30,
        "esc" | "escape" => 0x35,
        "space" => 0x31,
        "backspace" => 0x33,
        "delete" | "del" => 0x75,
        "insert" | "ins" => 0x72,
        "home" => 0x73,
        "end" => 0x77,
        "pageup" | "pgup" => 0x74,
        "pagedown" | "pgdn" => 0x79,
        "left" => 0x7b,
        "right" => 0x7c,
        "down" => 0x7d,
        "up" => 0x7e,
        "printscreen" | "prtsc" => 0x69,
        "capslock" => 0x39,
        "plus" => return Some(Key::Character('+')),
        "minus" => return Some(Key::Character('-')),
        _ => {
            let number = name.strip_prefix('f')?.parse::<u16>().ok()?;
            if !(1..=20).contains(&number) {
                return None;
            }
            crate::sys::mac_key_code(0x70 + number - 1)?
        }
    };
    Some(Key::Virtual(code, None))
}

fn parse_combo(input: &str) -> Result<Combo, Error> {
    let parts: Vec<String> = input.split('+').map(|part| part.trim().to_lowercase()).collect();
    if parts.iter().any(String::is_empty) {
        return Err(invalid(format!("bad key combination {input:?}")));
    }
    let mut modifiers = Vec::new();
    for part in &parts[..parts.len() - 1] {
        let value = modifier(part).ok_or_else(|| invalid(format!("{part:?} is not a modifier in {input:?}")))?;
        if modifiers.contains(&value) {
            return Err(invalid(format!("duplicate modifier {part:?} in {input:?}")));
        }
        modifiers.push(value);
    }
    let last = &parts[parts.len() - 1];
    let key = if let Some(key) = named_key(last) {
        key
    } else {
        let mut chars = last.chars();
        match (chars.next(), chars.next()) {
            (Some(ch), None) if !ch.is_control() => Key::Character(ch),
            _ => return Err(invalid(format!("unknown key {last:?} in {input:?}"))),
        }
    };
    if let Key::Virtual(_, Some(value)) = key {
        if modifiers.contains(&value) {
            return Err(invalid(format!("the main key repeats a modifier in {input:?}")));
        }
    }
    Ok(Combo { modifiers, key })
}

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    static kTISPropertyUnicodeKeyLayoutData: CFTypeRef;
    fn TISCopyCurrentKeyboardLayoutInputSource() -> CFTypeRef;
    fn TISGetInputSourceProperty(source: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    fn LMGetKbdType() -> u8;
    fn UCKeyTranslate(
        layout: *const c_void,
        key: u16,
        action: u16,
        modifiers: u32,
        keyboard_type: u32,
        options: u32,
        dead_key_state: *mut u32,
        maximum: usize,
        actual: *mut usize,
        output: *mut u16,
    ) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
    fn CFDataGetLength(data: CFTypeRef) -> isize;
}

fn resolve_character_on_main(character: char) -> Result<(u16, Vec<Modifier>), String> {
    // SAFETY: Carbon returns an owned input source and its borrowed layout data.
    let source = Owned(unsafe { TISCopyCurrentKeyboardLayoutInputSource() });
    if source.0.is_null() {
        return Err("the current keyboard layout is unavailable".into());
    }
    let data = unsafe { TISGetInputSourceProperty(source.0, kTISPropertyUnicodeKeyLayoutData) };
    if data.is_null() {
        return Err("the current keyboard layout has no Unicode mapping".into());
    }
    // SAFETY: the property is documented as CFData and is retained by `source`.
    let (bytes, length, keyboard_type) = unsafe { (CFDataGetBytePtr(data), CFDataGetLength(data), LMGetKbdType()) };
    if bytes.is_null() || length <= 0 {
        return Err("the current keyboard layout is empty".into());
    }
    let mut wanted = [0u16; 2];
    let wanted = character.encode_utf16(&mut wanted);
    for (state, modifiers) in [
        (0, &[][..]),
        (2, &[SHIFT][..]),
        (8, &[OPTION][..]),
        (10, &[SHIFT, OPTION][..]),
    ] {
        for key in 0..=127 {
            let mut output = [0u16; 255];
            let mut length = 0usize;
            let mut dead = 0u32;
            // SAFETY: all output buffers are valid, and the source retains the layout.
            let status = unsafe {
                UCKeyTranslate(bytes.cast(), key, 0, state, u32::from(keyboard_type), 1,
                    &mut dead, output.len(), &mut length, output.as_mut_ptr())
            };
            if status == 0 && length == wanted.len() && output[..length] == *wanted {
                return Ok((key, modifiers.to_vec()));
            }
        }
    }
    Err(format!("the current keyboard layout has no key for {character:?}"))
}

fn resolve_character(character: char) -> Result<(u16, Vec<Modifier>), Error> {
    if objc2::MainThreadMarker::new().is_some() {
        return resolve_character_on_main(character).map_err(invalid);
    }
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let skip = cancelled.clone();
    dispatch2::DispatchQueue::main().exec_async(move || {
        if !skip.load(std::sync::atomic::Ordering::Acquire) {
            let result = resolve_character_on_main(character);
            if sender.send(result).is_err() {
                crate::log("control: keyboard layout lookup finished after its requester stopped waiting");
            }
        }
    });
    receiver.recv_timeout(Duration::from_secs(3)).map_err(|error| {
        cancelled.store(true, std::sync::atomic::Ordering::Release);
        internal(format!("could not inspect the current keyboard layout: {error}"))
    })?.map_err(invalid)
}

fn focused_target(a: &Args) -> Result<Option<Window>, Error> {
    if !find::specified(a) {
        return Ok(None);
    }
    let layout = DisplayLayout::current().map_err(internal)?;
    let window = find::selected(a, &layout, true)?;
    check_focus(Some(&window))?;
    Ok(Some(window))
}

fn check_focus(window: Option<&Window>) -> Result<(), Error> {
    check()?;
    if let Some(window) = window {
        let focused = targets::window_has_input_focus(window).map_err(internal)?;
        if !focused {
            return Err(Error::new(ErrorCode::Conflict,
                "the selected window no longer has keyboard focus; no further keys were sent"));
        }
    }
    Ok(())
}

fn key_event(code: u16, down: bool, flags: u64, unicode: &[u16]) -> Result<(), Error> {
    // SAFETY: Core Graphics owns the new event until it is released.
    let event = Owned(unsafe { CGEventCreateKeyboardEvent(ptr::null_mut(), code, down) }.cast_const());
    if event.0.is_null() {
        return Err(internal("could not create a keyboard event"));
    }
    // SAFETY: the event and Unicode buffer remain live through posting.
    unsafe {
        CGEventSetFlags(event.0.cast_mut(), flags);
        if !unicode.is_empty() {
            CGEventKeyboardSetUnicodeString(event.0.cast_mut(), unicode.len(), unicode.as_ptr());
        }
        CGEventPost(kCGHIDEventTap, event.0.cast_mut());
    }
    Ok(())
}

struct HeldKey {
    code: u16,
    flag: u64,
    unicode: Vec<u16>,
}

struct PressedKeys {
    flags: u64,
    held: Vec<HeldKey>,
}

impl PressedKeys {
    fn new(codes: impl IntoIterator<Item = u16>) -> Result<Self, Error> {
        // SAFETY: these are plain queries of the current session's input state.
        let flags = unsafe { CGEventSourceFlagsState(kCGEventSourceStateCombinedSessionState) };
        if flags & MODIFIER_FLAGS != 0 {
            return Err(Error::new(ErrorCode::Conflict, "release held modifier keys before running input automation"));
        }
        for code in codes {
            if unsafe { CGEventSourceKeyState(kCGEventSourceStateCombinedSessionState, code) } {
                return Err(Error::new(ErrorCode::Conflict, "release held keys before running input automation"));
            }
        }
        Ok(Self { flags, held: Vec::new() })
    }

    fn press(&mut self, code: u16, flag: u64, unicode: &[u16]) -> Result<(), Error> {
        let flags = self.flags | flag;
        key_event(code, true, flags, unicode)?;
        self.flags = flags;
        self.held.push(HeldKey { code, flag, unicode: unicode.to_vec() });
        Ok(())
    }

    fn release_with(&mut self, emit: &mut impl FnMut(u16, bool, u64, &[u16]) -> Result<(), Error>) -> Result<(), Error> {
        let Some(key) = self.held.last() else { return Ok(()) };
        let flags = self.flags & !key.flag;
        emit(key.code, false, flags, &key.unicode)?;
        self.flags = flags;
        self.held.pop();
        Ok(())
    }

    fn release_all(&mut self) -> Result<(), Error> {
        self.release_all_with(&mut key_event)
    }

    fn release_all_with(&mut self, emit: &mut impl FnMut(u16, bool, u64, &[u16]) -> Result<(), Error>) -> Result<(), Error> {
        let mut failure = None;
        while !self.held.is_empty() {
            if let Err(error) = self.release_with(emit) {
                if failure.is_none() {
                    failure = Some(error);
                }
                if let Err(error) = self.release_with(emit) {
                    crate::log(&format!("control: could not release an injected key after retrying: {error}"));
                    let key = self.held.pop().expect("the failed release still has a key");
                    self.flags &= !key.flag;
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

impl Drop for PressedKeys {
    fn drop(&mut self) {
        if let Err(error) = self.release_all() {
            crate::log(&format!("control: could not release injected keys: {error}"));
        }
    }
}

fn tap(window: Option<&Window>, modifiers: &[Modifier], code: u16, flag: u64, unicode: &[u16]) -> Result<(), Error> {
    check_focus(window)?;
    let mut pressed = PressedKeys::new(modifiers.iter().map(|modifier| modifier.code).chain(std::iter::once(code)))?;
    for modifier in modifiers {
        pressed.press(modifier.code, modifier.flag, &[])?;
    }
    check()?;
    pressed.press(code, flag, unicode)?;
    pressed.release_all()?;
    Ok(())
}

fn type_text(a: &Args) -> Reply {
    let mut names = capture::TARGET.to_vec();
    names.extend_from_slice(&["text", "delay_ms", "settle_ms"]);
    a.only(&names)?;
    let text = a.string("text")?.ok_or_else(|| invalid("text is required"))?;
    if text.chars().any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t')) {
        return Err(invalid("text contains a control character other than newline or tab"));
    }
    let delay = duration(a, "delay_ms", if crate::recorder::is_recording() { 35 } else { 0 })?;
    let settle = duration(a, "settle_ms", 100)?;
    prepare()?;
    let window = focused_target(a)?;
    for character in text.chars() {
        match character {
            '\r' => continue,
            '\n' => tap(window.as_ref(), &[], 0x24, 0, &[])?,
            '\t' => tap(window.as_ref(), &[], 0x30, 0, &[])?,
            character => {
                let mut units = [0u16; 2];
                tap(window.as_ref(), &[], 0, 0, character.encode_utf16(&mut units))?;
            }
        }
        if !delay.is_zero() {
            wait(delay)?;
        }
    }
    wait(settle)?;
    Ok(json!({ "typed": text.chars().count(), "window": window }))
}

fn key(a: &Args) -> Reply {
    let mut names = capture::TARGET.to_vec();
    names.extend_from_slice(&["keys", "repeat", "settle_ms"]);
    a.only(&names)?;
    let keys = a.string("keys")?.ok_or_else(|| invalid("keys is required, e.g. cmd+s"))?;
    let combo = parse_combo(&keys)?;
    let repeat = a.int("repeat", 1, 100)?.unwrap_or(1) as u32;
    let settle = duration(a, "settle_ms", 150)?;
    prepare()?;
    let window = focused_target(a)?;
    let mut modifiers = combo.modifiers;
    let (code, flag) = match combo.key {
        Key::Virtual(code, modifier) => (code, modifier.map_or(0, |modifier| modifier.flag)),
        Key::Character(character) => {
            let (code, implied) = resolve_character(character)?;
            for modifier in implied {
                if !modifiers.contains(&modifier) {
                    modifiers.push(modifier);
                }
            }
            (code, 0)
        }
    };
    for index in 0..repeat {
        if index > 0 {
            wait(Duration::from_millis(40))?;
        }
        tap(window.as_ref(), &modifiers, code, flag, &[])?;
    }
    wait(settle)?;
    Ok(json!({ "keys": keys, "repeat": repeat, "window": window }))
}

pub(super) fn dispatch(command: &str, a: &Args) -> Reply {
    serialized(|| match command {
        "focus" => find::focus(a),
        "click" => click(a),
        "move" => move_pointer(a),
        "drag" => drag(a),
        "scroll" => scroll(a),
        "capture.scroll" => capture_scroll(a),
        "type" => type_text(a),
        "key" => key(a),
        _ => Err(invalid(format!("unknown input command {command:?}"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mac_modifiers_named_keys_and_characters() {
        assert_eq!(parse_combo("cmd+Shift+A").unwrap(), Combo {
            modifiers: vec![COMMAND, SHIFT], key: Key::Character('a'),
        });
        assert_eq!(parse_combo("ctrl+option+f12").unwrap(), Combo {
            modifiers: vec![CONTROL, OPTION], key: Key::Virtual(0x6f, None),
        });
        assert_eq!(parse_combo("command").unwrap(), Combo {
            modifiers: vec![], key: Key::Virtual(COMMAND.code, Some(COMMAND)),
        });
        assert_eq!(parse_combo("cmd+plus").unwrap().key, Key::Character('+'));
        for input in ["", "cmd++a", "a+b", "cmd+win+a", "shift+shift", "f21", "unknown"] {
            assert_eq!(parse_combo(input).unwrap_err().code, ErrorCode::InvalidRequest, "{input}");
        }
    }

    #[test]
    fn pointer_offsets_cannot_overflow() {
        assert_eq!(shifted((3, -4), (-2, 6)).unwrap(), (1, 2));
        assert!(shifted((i32::MAX, 0), (1, 0)).is_err());
        assert!(shifted((0, i32::MIN), (0, -1)).is_err());
    }

    #[test]
    fn scroll_capture_checks_exact_pixel_budget_before_allocating() {
        assert!(!scroll_pixel_limit_exceeded(64_000_000, 1));
        assert!(scroll_pixel_limit_exceeded(64_000_001, 1));
        assert!(!scroll_pixel_limit_exceeded(500_000, 128));
        assert!(scroll_pixel_limit_exceeded(500_001, 128));
        assert!(scroll_pixel_limit_exceeded(u64::MAX, 2));
    }

    #[test]
    fn scroll_capture_rejects_bad_limits_before_input() {
        for args in [
            json!({"max_frames": 1}),
            json!({"max_frames": 129}),
            json!({"timeout_ms": 0}),
            json!({"settle_ms": 0}),
            json!({"notches": 0}),
            json!({"notches": 21}),
        ] {
            let args = Args::new(args).unwrap();
            assert_eq!(capture_scroll(&args).unwrap_err().code, ErrorCode::InvalidRequest);
        }
    }

    #[test]
    fn releases_keys_in_reverse_order_and_retries_failed_releases() {
        let mut keys = PressedKeys {
            flags: SHIFT.flag | COMMAND.flag,
            held: vec![
                HeldKey { code: COMMAND.code, flag: COMMAND.flag, unicode: Vec::new() },
                HeldKey { code: SHIFT.code, flag: SHIFT.flag, unicode: Vec::new() },
                HeldKey { code: 0, flag: 0, unicode: vec![b'a' as u16] },
            ],
        };
        let mut events = Vec::new();
        let mut emit = |code, down, flags, unicode: &[u16]| {
            events.push((code, down, flags, unicode.to_vec()));
            if events.len() == 1 {
                Err(internal("simulated release failure"))
            } else {
                Ok(())
            }
        };
        assert!(keys.release_all_with(&mut emit).is_err());
        assert!(keys.held.is_empty());
        assert_eq!(keys.flags, 0);
        assert_eq!(events, vec![
            (0, false, SHIFT.flag | COMMAND.flag, vec![b'a' as u16]),
            (0, false, SHIFT.flag | COMMAND.flag, vec![b'a' as u16]),
            (SHIFT.code, false, COMMAND.flag, Vec::new()),
            (COMMAND.code, false, 0, Vec::new()),
        ]);
    }
}
