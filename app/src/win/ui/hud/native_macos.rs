//! Native placement and capture exclusion for macOS overlay windows.

use jiffshot_capture::{screen, CaptureMapping, Rect};
use objc2_app_kit::NSStatusWindowLevel;
use objc2_foundation::{NSPoint, NSRect, NSSize};
use rustolonia::{AppScope, Window};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};

thread_local! {
    static CLOAKED: RefCell<HashMap<isize, f64>> = RefCell::new(HashMap::new());
}

pub fn handle(scope: &AppScope, window: &Window) -> isize {
    let handle = scope.platform_handle(window).unwrap_or(0);
    if crate::mac_window::window(handle).is_some() { handle } else { 0 }
}

pub fn capturable() -> bool {
    std::env::var_os("JIFFSHOT_HUD_CAPTURABLE").is_some()
}

#[derive(Clone, Copy)]
pub struct ProtectedWindow {
    number: u32,
    ticket: screen::ExclusionTicket,
}

/// Creates the native window without exposing it before recording filters
/// include it. The caller places it only when its exclusion is acknowledged.
pub fn show_protected(scope: &AppScope, window: &Window, click_through: bool) -> io::Result<Option<ProtectedWindow>> {
    let result = (|| {
        if !capturable() {
            scope.set_transparent(window, true).map_err(io::Error::other)?;
            window.set_opacity(0.0).map_err(io::Error::other)?;
            window.set_enabled(false).map_err(io::Error::other)?;
            window.set_position((-32000, -32000)).map_err(io::Error::other)?;
        }
        window.show().map_err(io::Error::other)?;
        let hwnd = handle(scope, window);
        let native = crate::mac_window::window(hwnd).ok_or_else(|| io::Error::other("the native capture window is unavailable"))?;
        if !capturable() {
            native.setAlphaValue(0.0);
        }
        crate::mac_window::prepare(hwnd, click_through).map_err(io::Error::other)?;
        if capturable() {
            return Ok(None);
        }
        native.setIgnoresMouseEvents(true);
        let (number, ticket) = crate::mac_window::protect(hwnd).map_err(io::Error::other)?;
        Ok(Some(ProtectedWindow { number, ticket }))
    })();
    if result.is_err() {
        if let Err(error) = window.hide() {
            eprintln!("could not hide a failed capture window: {error}");
        }
        if let Err(error) = window.close() {
            eprintln!("could not close a failed capture window: {error}");
        }
        if let Err(error) = super::native_popup_macos::discard_failed_owner(window) {
            eprintln!("could not clear a failed capture window's popup gate: {error}");
        }
    }
    result
}

/// Reveals a protected window on the UI dispatcher after all active streams
/// have acknowledged its exclusion, without blocking the dispatcher.
pub fn reveal_when_excluded(
    scope: &AppScope,
    window: &Window,
    protected: Option<ProtectedWindow>,
    click_through: bool,
    position: impl FnOnce() -> std::result::Result<(), String> + Send + 'static,
    ready: impl FnOnce() -> std::result::Result<(), String> + Send + 'static,
    failed: impl FnOnce(String) + Send + 'static,
) -> io::Result<()> {
    let target = window.clone();
    let dispatcher = scope.clone();
    scope.spawn(async move {
        let deadline = Instant::now() + Duration::from_secs(30);
        let outcome = loop {
            match target.get_is_visible() {
                Ok(false) => return,
                Ok(true) => {}
                Err(error) => break Err(format!("could not inspect the capture window: {error}")),
            }
            let hwnd = handle(&dispatcher, &target);
            if hwnd == 0 {
                break Err("the native capture window closed or was recreated".to_owned());
            }
            if let Some(protected) = protected {
                if crate::mac_window::protection(hwnd).is_some_and(|ticket| ticket != protected.ticket) {
                    return;
                }
                if crate::mac_window::number(hwnd) != Some(protected.number)
                    || crate::mac_window::protection(hwnd) != Some(protected.ticket)
                    || !screen::is_exclusion_registered(protected.ticket)
                {
                    break Err("the capture window exclusion is no longer available".to_owned());
                }
                if !screen::exclusion_ready(protected.number) {
                    if Instant::now() >= deadline {
                        break Err("recording did not acknowledge the capture window exclusion".to_owned());
                    }
                    match dispatcher.delay(Duration::from_millis(25)) {
                        Ok(delay) => {
                            if let Err(error) = delay.await {
                                break Err(format!("could not wait for the capture window exclusion: {error}"));
                            }
                        }
                        Err(error) => break Err(format!("could not schedule the capture window exclusion: {error}")),
                    }
                    continue;
                }
                let Some(native) = crate::mac_window::window(hwnd) else {
                    break Err("the native capture window closed or was recreated".to_owned());
                };
                if let Err(error) = position() {
                    break Err(error);
                }
                if let Err(error) = target.set_enabled(true) {
                    break Err(format!("could not enable the capture window: {error}"));
                }
                if let Err(error) = target.set_opacity(1.0) {
                    break Err(format!("could not reveal the capture window: {error}"));
                }
                native.setAlphaValue(1.0);
                native.setIgnoresMouseEvents(click_through);
            } else if let Err(error) = position() {
                break Err(error);
            }
            break ready();
        };
        if let Err(error) = outcome {
            if let Err(hide_error) = target.hide() {
                eprintln!("could not hide a failed capture window: {hide_error}");
            }
            if let Err(close_error) = target.close() {
                eprintln!("could not close a failed capture window: {close_error}");
            }
            if let Err(clear_error) = super::native_popup_macos::discard_failed_owner(&target) {
                eprintln!("could not clear a failed capture window's popup gate: {clear_error}");
            }
            failed(error);
        }
    }).map_err(io::Error::other)
}

pub fn size(scope: &AppScope, window: &Window) -> io::Result<Option<(f64, f64)>> {
    let native = crate::mac_window::window(handle(scope, window)).ok_or_else(|| io::Error::other("the native capture window is unavailable"))?;
    let size = native.frame().size;
    if size.width <= 0.0 || size.height <= 0.0 {
        return Ok(None);
    }
    Ok(Some((size.width, size.height)))
}

pub fn position(scope: &AppScope, window: &Window, x: i32, y: i32) -> io::Result<()> {
    let native = crate::mac_window::window(handle(scope, window)).ok_or_else(|| io::Error::other("the native capture window is unavailable"))?;
    let (x, y) = screen::to_points(x as f64, y as f64);
    native.setFrameTopLeftPoint(NSPoint::new(x, screen::main_height_points() - y));
    Ok(())
}

pub fn place_checked(handle: isize, rect: Rect, show: bool) -> io::Result<()> {
    if rect.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "the capture window rectangle is empty"));
    }
    let r = screen::rect_to_points(rect);
    place_points_checked(handle, r.origin.x, r.origin.y, r.size.width, r.size.height, show)
}

#[derive(Clone, Copy, Debug)]
struct MappedFrame {
    desktop: [f64; 4],
    appkit: [f64; 4],
}

fn mapped_frame_at_height(mapping: CaptureMapping, scale: f64, screen_height: f64) -> io::Result<MappedFrame> {
    let [left, top, right, bottom] = mapping.desktop_bounds();
    if !scale.is_finite() || scale <= 0.0 || !screen_height.is_finite() || screen_height <= 0.0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid desktop geometry"));
    }
    // AppKit snaps frame origins down and sizes up to whole points. Compute
    // an enclosing frame explicitly so fractional edges are not clipped.
    let x = (left / scale).floor();
    let y = (screen_height - bottom / scale).floor();
    let right = (right / scale).ceil();
    let upper = (screen_height - top / scale).ceil();
    let frame = MappedFrame {
        desktop: [x * scale, (screen_height - upper) * scale, right * scale, (screen_height - y) * scale],
        appkit: [x, y, right - x, upper - y],
    };
    if !frame.desktop.iter().chain(frame.appkit.iter()).all(|value| value.is_finite())
        || frame.appkit[2] <= 0.0 || frame.appkit[3] <= 0.0
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid mapped capture window geometry"));
    }
    Ok(frame)
}

pub fn mapped_frame_bounds(mapping: CaptureMapping, scale: f64) -> io::Result<[f64; 4]> {
    Ok(mapped_frame_at_height(mapping, scale, screen::main_height_points())?.desktop)
}

/// Encloses a mapped image; its surface must be positioned within this frame.
pub fn place_mapped_checked(handle: isize, mapping: CaptureMapping, scale: f64, expected: [f64; 4], show: bool) -> io::Result<()> {
    let frame = mapped_frame_at_height(mapping, scale, screen::main_height_points())?;
    if frame.desktop != expected {
        return Err(io::Error::other("the display geometry changed before the capture window was placed"));
    }
    let window = crate::mac_window::window(handle).ok_or_else(|| io::Error::other("the native capture window is unavailable"))?;
    let [x, y, width, height] = frame.appkit;
    // At normal window levels AppKit clamps smaller frames below the menu bar.
    window.setLevel(NSStatusWindowLevel);
    window.setFrame_display(NSRect::new(NSPoint::new(x, y), NSSize::new(width, height)), true);
    let actual = window.frame();
    if (actual.origin.x - x).abs() > 0.000_001
        || (actual.origin.y - y).abs() > 0.000_001
        || (actual.size.width - width).abs() > 0.000_001
        || (actual.size.height - height).abs() > 0.000_001
    {
        return Err(io::Error::other(format!(
            "AppKit could not place the capture window at its enclosing frame: requested ({x}, {y}, {width}, {height}), got ({}, {}, {}, {}), min {:?}, max {:?}, content min {:?}, content max {:?}",
            actual.origin.x, actual.origin.y, actual.size.width, actual.size.height,
            window.minSize(), window.maxSize(), window.contentMinSize(), window.contentMaxSize()
        )));
    }
    if show {
        window.orderFrontRegardless();
    }
    Ok(())
}

fn place_points_checked(handle: isize, x: f64, y: f64, width: f64, height: f64, show: bool) -> io::Result<()> {
    if ![x, y, width, height].iter().all(|value| value.is_finite()) || width <= 0.0 || height <= 0.0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid capture window geometry"));
    }
    let window = crate::mac_window::window(handle).ok_or_else(|| io::Error::other("the native capture window is unavailable"))?;
    let frame = NSRect::new(
        NSPoint::new(x, screen::main_height_points() - y - height),
        NSSize::new(width, height),
    );
    window.setLevel(NSStatusWindowLevel);
    window.setFrame_display(frame, true);
    if show {
        window.orderFrontRegardless();
    }
    Ok(())
}

pub fn hide(handle: isize) {
    if let Some(window) = crate::mac_window::window(handle) {
        window.orderOut(None);
    }
}

pub fn cloak(handle: isize, cloaked: bool) {
    let Some(window) = crate::mac_window::window(handle) else { return };
    CLOAKED.with(|values| {
        let mut values = values.borrow_mut();
        if cloaked {
            values.entry(handle).or_insert_with(|| window.alphaValue());
            window.setAlphaValue(0.0);
        } else if let Some(alpha) = values.remove(&handle) {
            window.setAlphaValue(alpha);
        }
    });
}

pub(crate) fn forget_cloak(handle: isize) {
    CLOAKED.with(|values| {
        values.borrow_mut().remove(&handle);
    });
}

pub(in crate::ui) fn is_cloaked(handle: isize) -> bool {
    CLOAKED.with(|values| values.borrow().contains_key(&handle))
}

pub fn activate(handle: isize) {
    if handle != 0 {
        if let Err(error) = crate::mac_window::activate(handle) {
            eprintln!("could not activate a capture window: {error}");
        }
    }
}

pub fn screen_of(rect: Rect) -> (Rect, f64) {
    match screen::monitor_at(rect.x + rect.width / 2, rect.y + rect.height / 2) {
        Some(monitor) => (monitor.work, screen::scale_factor()),
        None => (rect, screen::scale_factor()),
    }
}

pub fn beside(area: Rect, work: Rect, x: i32, width: i32, height: i32, gap: i32) -> (i32, i32) {
    let x = x.clamp(work.x, (work.right() - width).max(work.x));
    let below = area.bottom() + gap;
    let above = area.y - gap - height;
    let y = if below + height <= work.bottom() {
        below
    } else if above >= work.y {
        above
    } else {
        area.bottom() - height - gap
    };
    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapped_frames_enclose_fractional_and_negative_edges() {
        let cases: [([f64; 4], f64, f64, [f64; 4]); 3] = [
            ([0.25, 0.25, 1280.25, 720.25], 2.0, 1117.0, [0.0, 0.0, 1282.0, 722.0]),
            ([-8.49, -3.49, -0.49, 2.51], 2.0, 1117.0, [-10.0, -4.0, 0.0, 4.0]),
            ([1.5, 2.5, 6.5, 7.5], 1.5, 1000.25, [1.5, 1.875, 7.5, 7.875]),
        ];
        for (bounds, scale, height, expected) in cases {
            let rect = Rect::from_ltrb(
                bounds[0].round() as i32,
                bounds[1].round() as i32,
                bounds[2].round() as i32,
                bounds[3].round() as i32,
            );
            let mapping = CaptureMapping::with_desktop_bounds(rect, 5, 3, bounds).unwrap();
            let frame = mapped_frame_at_height(mapping, scale, height).unwrap();
            assert_eq!(frame.desktop, expected);
            assert!(frame.desktop[0] <= bounds[0] && frame.desktop[1] <= bounds[1]);
            assert!(frame.desktop[2] >= bounds[2] && frame.desktop[3] >= bounds[3]);
            assert!(frame.appkit.iter().all(|value| value.fract() == 0.0));
        }
    }

    #[test]
    fn mapped_frames_reject_invalid_geometry() {
        let mapping = CaptureMapping::identity(Rect::new(0, 0, 10, 10)).unwrap();
        for (scale, height) in [(0.0, 100.0), (f64::NAN, 100.0), (1.0, 0.0), (1.0, f64::INFINITY)] {
            assert!(mapped_frame_at_height(mapping, scale, height).is_err());
        }
    }
}
