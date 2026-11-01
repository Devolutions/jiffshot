//! What the app's overlay and Screen Draw windows call, on the UI thread:
//! input in, what to draw out.

use crate::overlay::{self, View};
use crate::screendraw;
use crate::sys::SetCursorPos;
use jiffshot_capture::Rect;
use jiffshot_doc::Pt;

#[cfg(target_os = "macos")]
use crate::geometry::CaptureLayout;
#[cfg(target_os = "macos")]
use jiffshot_capture::macos::ffi::CGWarpMouseCursorPosition;
#[cfg(target_os = "macos")]
use objc2_core_foundation::CGPoint;

pub const INPUT_PRESS: i32 = 0;
pub const INPUT_MOVE: i32 = 1;
pub const INPUT_RELEASE: i32 = 2;
pub const INPUT_WHEEL: i32 = 3;
pub const INPUT_KEY: i32 = 4;
pub const INPUT_CANCEL: i32 = 5;
// Any other kind only refreshes the state (and the modifier keys).

pub const MOD_SHIFT: i32 = 1;
pub const MOD_CTRL: i32 = 2;

pub const FLAG_DONE: i32 = 1;
pub const FLAG_MAGNIFIER: i32 = 2;
pub const FLAG_HELP: i32 = 4;
pub const FLAG_GUIDES: i32 = 8;
pub const FLAG_COLOR: i32 = 16;
pub const FLAG_DRAGGING: i32 = 32;

/// Texts for [`overlay_text`].
pub const TEXT_TITLE: i32 = 0;
pub const TEXT_SIZE: i32 = 1;
pub const TEXT_HINT: i32 = 2;
pub const TEXT_HELP: i32 = 3;

/// Integer lists for [`overlay_points`].
pub const POINTS_PATH: i32 = 0;
pub const POINTS_REGIONS: i32 = 1;
pub const POINTS_GUIDES: i32 = 2;

/// What the overlay shows, in desktop pixels (see [`View`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct OverlayState {
    pub flags: i32,
    pub cursor: [i32; 2],
    #[cfg(target_os = "macos")]
    pub cursor_precise: [f64; 2],
    pub lit: [i32; 4],
    pub lit_kind: i32,
    pub monitor: [i32; 4],
    pub scale: f32,
    pub color: [i32; 3],
    pub path_count: i32,
    pub region_count: i32,
}

impl OverlayState {
    pub fn has(&self, flag: i32) -> bool {
        self.flags & flag != 0
    }

    pub fn lit(&self) -> Rect {
        let [x, y, w, h] = self.lit;
        Rect::new(x, y, w, h)
    }

    pub fn monitor(&self) -> Rect {
        let [x, y, w, h] = self.monitor;
        Rect::new(x, y, w, h)
    }
}

fn state_of(view: &View, done: bool, path: usize, regions: usize) -> OverlayState {
    let flag = |on: bool, f: i32| if on { f } else { 0 };
    let r = |r: Rect| [r.x, r.y, r.width, r.height];
    OverlayState {
        flags: flag(done, FLAG_DONE)
            | flag(view.magnifier, FLAG_MAGNIFIER)
            | flag(view.help, FLAG_HELP)
            | flag(view.guides, FLAG_GUIDES)
            | flag(view.color.is_some(), FLAG_COLOR)
            | flag(view.dragging, FLAG_DRAGGING),
        cursor: [view.cursor.0, view.cursor.1],
        #[cfg(target_os = "macos")]
        cursor_precise: [view.cursor_precise.0, view.cursor_precise.1],
        lit: r(view.lit),
        lit_kind: view.lit_kind,
        monitor: r(view.monitor),
        scale: view.scale,
        color: view
            .color
            .map_or([0; 3], |(r, g, b)| [r as i32, g as i32, b as i32]),
        path_count: path as i32,
        region_count: regions as i32,
    }
}

#[cfg(target_os = "macos")]
fn move_selection_cursor_with(
    layout: &CaptureLayout,
    x: f64,
    y: f64,
    warp: impl FnOnce(f64, f64) -> std::io::Result<(f64, f64)>,
) -> std::io::Result<(f64, f64)> {
    let scale = layout.desktop_scale();
    let point = (x / scale, y / scale);
    if !point.0.is_finite() || !point.1.is_finite() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid selection cursor position",
        ));
    }
    layout.validate()?;
    let actual = warp(point.0, point.1)?;
    let position = (actual.0 * scale, actual.1 * scale);
    overlay::rounded_desktop_point(position)?;
    layout.validate()?;
    Ok(position)
}

/// Input for overlay `session` (desktop pixels; `value` is the wheel delta
/// or the virtual key), then the state. A session that is over answers
/// [`FLAG_DONE`].
pub fn overlay_input(
    session: u64,
    kind: i32,
    x: f64,
    y: f64,
    value: i32,
    mods: i32,
) -> OverlayState {
    #[cfg(target_os = "macos")]
    {
        overlay_input_with(session, kind, x, y, value, mods, |x, y| {
            // SAFETY: Core Graphics accepts a plain global point.
            let result = unsafe { CGWarpMouseCursorPosition(CGPoint { x, y }) };
            if result != 0 {
                return Err(std::io::Error::other(format!(
                    "Core Graphics cursor move failed ({result})"
                )));
            }
            jiffshot_capture::screen::try_cursor_pos_points()
        })
    }
    #[cfg(windows)]
    {
        overlay_input_with(session, kind, x, y, value, mods)
    }
}

fn overlay_input_with(
    session: u64,
    kind: i32,
    x: f64,
    y: f64,
    value: i32,
    mods: i32,
    #[cfg(target_os = "macos")] warp: impl FnOnce(f64, f64) -> std::io::Result<(f64, f64)>,
) -> OverlayState {
    let mut guard = overlay::session();
    let Some(s) = guard.as_mut().filter(|s| s.id == session) else {
        return OverlayState {
            flags: FLAG_DONE,
            ..OverlayState::default()
        };
    };
    if s.overlay.outcome.is_some() {
        return OverlayState {
            flags: FLAG_DONE,
            ..OverlayState::default()
        };
    }
    #[cfg(target_os = "macos")]
    if s.overlay.outcome.is_none() {
        if let Some(error) = s
            .overlay
            .display_layout
            .as_ref()
            .and_then(|layout| layout.validate().err())
        {
            s.fail(error.to_string());
            drop(guard);
            crate::on_capture_thread(|| {});
            return OverlayState {
                flags: FLAG_DONE,
                ..OverlayState::default()
            };
        }
    }
    let o = &mut s.overlay;
    let shift = mods & MOD_SHIFT != 0;
    o.set_modifiers(shift, mods & MOD_CTRL != 0);
    #[cfg(windows)]
    let (cx, cy) = (x.round() as i32 - o.origin.0, y.round() as i32 - o.origin.1);
    #[cfg(target_os = "macos")]
    let (cx, cy) = if matches!(kind, INPUT_PRESS | INPUT_MOVE | INPUT_RELEASE | INPUT_WHEEL) {
        match overlay::client_point((x, y), o.origin) {
            Ok(point) => point,
            Err(error) => {
                s.fail(format!("invalid selection pointer position: {error}"));
                drop(guard);
                crate::on_capture_thread(|| {});
                return OverlayState {
                    flags: FLAG_DONE,
                    ..OverlayState::default()
                };
            }
        }
    } else {
        (0, 0)
    };
    let mut input_failure = None;
    match kind {
        INPUT_PRESS => {
            o.pointer_pressed(cx, cy);
            #[cfg(target_os = "macos")]
            o.remember_pointer_position((x, y));
        }
        INPUT_MOVE => {
            o.pointer_moved(cx, cy);
            #[cfg(target_os = "macos")]
            o.remember_pointer_position((x, y));
        }
        INPUT_RELEASE => {
            #[cfg(windows)]
            let result = o.pointer_released(cx, cy);
            #[cfg(target_os = "macos")]
            let result = o.pointer_released_precise(cx, cy, (x, y));
            if let Err(error) = result {
                input_failure = Some(format!("selection geometry: {error}"));
            }
        }
        INPUT_WHEEL => {
            #[cfg(windows)]
            let result = o.wheel(value);
            #[cfg(target_os = "macos")]
            let result = o.wheel_precise(cx, cy, (x, y), value);
            if let Err(error) = result {
                input_failure = Some(format!("selection geometry: {error}"));
            }
        }
        INPUT_KEY => {
            let target = match o.key(value as u16, shift) {
                Ok(target) => target,
                Err(error) => {
                    input_failure = Some(format!("selection geometry: {error}"));
                    None
                }
            };
            if let Some((_nx, _ny)) = target {
                #[cfg(windows)]
                // SAFETY: plain cursor move.
                let moved = unsafe { SetCursorPos(_nx + o.origin.0, _ny + o.origin.1) } != 0;
                #[cfg(target_os = "macos")]
                {
                    let result = match (o.nudge_position(), &o.display_layout) {
                        (Some((x, y)), Some(layout)) => {
                            move_selection_cursor_with(layout, x, y, warp)
                        }
                        (Some(position), None) => overlay::rounded_desktop_point(position)
                            .and_then(|(x, y)| {
                                // SAFETY: plain cursor move for an overlay without a retained layout.
                                if unsafe { SetCursorPos(x, y) } != 0 {
                                    Ok((f64::from(x), f64::from(y)))
                                } else {
                                    Err(std::io::Error::other("cursor move failed"))
                                }
                            }),
                        (None, _) => Err(std::io::Error::other(
                            "selection cursor target is unavailable",
                        )),
                    }
                    .and_then(|position| {
                        overlay::client_point(position, o.origin).map(|client| (position, client))
                    });
                    match result {
                        Ok((position, (x, y))) => {
                            // CGWarpMouseCursorPosition does not generate a move event.
                            o.pointer_moved(x, y);
                            o.remember_pointer_position(position);
                        }
                        Err(error) => {
                            input_failure =
                                Some(format!("could not move the selection cursor: {error}"));
                            o.nudge_failed();
                        }
                    }
                }
                #[cfg(windows)]
                if !moved {
                    eprintln!("could not move the selection cursor");
                }
            }
        }
        INPUT_CANCEL => o.cancel(),
        _ => {}
    }
    if let Some(error) = input_failure {
        s.fail_input(error);
        drop(guard);
        crate::on_capture_thread(|| {});
        return OverlayState {
            flags: FLAG_DONE,
            ..OverlayState::default()
        };
    }
    let o = &mut s.overlay;
    let done = o.outcome.is_some();
    let view = match o.view() {
        Ok(view) => view,
        Err(error) => {
            s.fail_input(format!("selection geometry: {error}"));
            drop(guard);
            crate::on_capture_thread(|| {});
            return OverlayState {
                flags: FLAG_DONE,
                ..OverlayState::default()
            };
        }
    };
    let state = state_of(&view, done, o.path().count(), o.regions().count());
    drop(guard);
    if done {
        // Wake the capture thread, which waits for the outcome.
        crate::on_capture_thread(|| {});
    }
    state
}

/// Fails the active overlay when its windows cannot be shown or updated.
/// Stale sessions and selections that have already finished are left alone.
pub fn overlay_failed(session: u64, message: String) -> bool {
    let failed = overlay::session()
        .as_mut()
        .filter(|s| s.id == session)
        .is_some_and(|s| s.fail(message));
    if failed {
        crate::on_capture_thread(|| {});
    }
    failed
}

/// Whether this session is still waiting for a selection.
pub fn overlay_pending(session: u64) -> bool {
    overlay::session()
        .as_ref()
        .is_some_and(|s| s.id == session && s.overlay.outcome.is_none())
}

/// One of the overlay's texts ([`TEXT_TITLE`]…); the help's lines are
/// joined by '\n'.
pub fn overlay_text(session: u64, which: i32) -> Result<String, String> {
    let mut guard = overlay::session();
    let s = guard
        .as_mut()
        .filter(|s| s.id == session)
        .ok_or("the selection session has ended")?;
    let view = s.overlay.view().map_err(|error| error.to_string())?;
    Ok(match which {
        TEXT_TITLE => view.title.unwrap_or_default(),
        TEXT_SIZE => view.size,
        TEXT_HINT => view.hint.to_owned(),
        _ => overlay::HELP.join("\n"),
    })
}

/// One of the overlay's integer lists ([`POINTS_PATH`]…): the freehand path
/// (x, y), the multi-area regions (x, y, w, h) or the video guides (w, h).
pub fn overlay_points(session: u64, which: i32) -> Vec<i32> {
    let guard = overlay::session();
    let Some(s) = guard.as_ref().filter(|s| s.id == session) else {
        return Vec::new();
    };
    match which {
        POINTS_PATH => s.overlay.path().flat_map(|(x, y)| [x, y]).collect(),
        POINTS_REGIONS => s
            .overlay
            .regions()
            .flat_map(|r| [r.x, r.y, r.width, r.height])
            .collect(),
        _ => overlay::GUIDES.iter().flat_map(|&(w, h)| [w, h]).collect(),
    }
}

/// Video guide rectangles in desktop coordinates, with their pixel dimensions.
pub fn overlay_guide_rectangles(session: u64) -> Result<Vec<(Rect, (i32, i32))>, String> {
    let guard = overlay::session();
    let s = guard
        .as_ref()
        .filter(|s| s.id == session)
        .ok_or("the selection session has ended")?;
    s.overlay
        .guide_rectangles()
        .map_err(|error| error.to_string())
}

/// Copies one monitor's frozen screenshot as BGRA into `dst`, with the
/// requested pixel dimensions and rows `stride` bytes apart. Returns false
/// when the session has ended.
pub fn overlay_copy(
    session: u64,
    r: Rect,
    width: u32,
    height: u32,
    dst: &mut [u8],
    stride: usize,
) -> Result<bool, String> {
    let guard = overlay::session();
    let Some(s) = guard.as_ref().filter(|s| s.id == session) else {
        return Ok(false);
    };
    let row = (width as usize)
        .checked_mul(4)
        .ok_or("invalid overlay dimensions")?;
    let required = stride
        .checked_mul(height as usize)
        .ok_or("invalid overlay buffer size")?;
    if r.is_empty() || width == 0 || height == 0 || stride < row || dst.len() < required {
        return Err("invalid overlay dimensions or buffer".into());
    }
    #[cfg(target_os = "macos")]
    if let Some(frozen) = &s.overlay.frozen {
        frozen
            .copy_native_monitor(r, width, height, dst, stride)
            .map_err(|error| error.to_string())?;
        return Ok(true);
    }
    let (w, h) = (r.width, r.height);
    if (width, height) != (w as u32, h as u32) {
        return Err("the overlay dimensions do not match the snapshot".into());
    }
    let snap = &s.overlay.snapshot;
    let (ox, oy) = (r.x - s.overlay.origin.0, r.y - s.overlay.origin.1);
    let src_stride = snap.stride();
    // Rows split across cores: this is between the hotkey and the overlay
    // appearing.
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    let per = (h as usize).div_ceil(threads);
    std::thread::scope(|scope| {
        for (band, rows) in dst[..required].chunks_mut(per * stride).enumerate() {
            scope.spawn(move || {
                for (i, line) in rows.chunks_mut(stride).enumerate() {
                    let line = &mut line[..w as usize * 4];
                    line.fill(0);
                    let sy = oy + (band * per + i) as i32;
                    if sy < 0 || sy >= snap.height as i32 {
                        continue;
                    }
                    let (x0, x1) = (ox.max(0), (ox + w).min(snap.width as i32));
                    if x1 > x0 {
                        let from = sy as usize * src_stride + x0 as usize * 4;
                        let at = (x0 - ox) as usize * 4;
                        line[at..at + (x1 - x0) as usize * 4]
                            .copy_from_slice(&snap.pixels[from..from + (x1 - x0) as usize * 4]);
                    }
                }
            });
        }
    });
    Ok(true)
}

/// The overlay is on screen (for the timing log).
pub fn overlay_shown(session: u64) {
    let guard = overlay::session();
    if let Some(started) = guard
        .as_ref()
        .filter(|s| s.id == session)
        .and_then(|s| s.overlay.started)
    {
        crate::log(&format!(
            "overlay on screen {:.0} ms after the hotkey",
            started.elapsed().as_secs_f64() * 1000.0
        ));
    }
}

/// Starts a demo overlay (see `overlay::demo`) and returns its
/// announcement. For the UI self-test.
pub fn overlay_demo(mode: i32) -> String {
    overlay::demo(mode)
}

/// Ends a demo overlay.
pub fn overlay_end_demo(session: u64) {
    let mut guard = overlay::session();
    if guard.as_ref().is_some_and(|s| s.id == session) {
        *guard = None;
    }
}

/// Input for Screen Draw `session` (recording pixels); true when the marks
/// changed.
pub fn draw_input(session: u64, kind: i32, x: f64, y: f64, value: i32) -> bool {
    let p = Pt::new(x as f32, y as f32);
    let ended = {
        let mut guard = screendraw::state();
        let Some(s) = guard.as_mut().filter(|s| s.id == session) else {
            return false;
        };
        match kind {
            INPUT_PRESS => s.press(p),
            INPUT_MOVE => return s.moved(p),
            INPUT_RELEASE => s.release(),
            INPUT_KEY => {
                s.key(value as u16);
                return false;
            }
            INPUT_CANCEL => {}
            _ => return false,
        }
        kind == INPUT_CANCEL
    };
    if ended {
        screendraw::hide();
    }
    true
}

/// Re-renders fading marks; true when they changed.
pub fn draw_tick(session: u64) -> bool {
    screendraw::state()
        .as_mut()
        .filter(|s| s.id == session)
        .is_some_and(|s| s.tick())
}

/// Whether this Screen Draw session is still active.
pub fn draw_pending(session: u64) -> bool {
    screendraw::state()
        .as_ref()
        .is_some_and(|s| s.id == session)
}

/// Ends and reports a failure only if it belongs to the active session.
pub fn draw_failed(session: u64, message: String) -> bool {
    screendraw::fail(session, &message)
}

/// Copies the marks (premultiplied BGRA, `w` × `h`, tightly packed) into
/// `dst`, clear where there are none; false when the session is over.
pub fn draw_copy(session: u64, dst: &mut [u8], w: u32, h: u32) -> Result<bool, String> {
    let (image, expected) = {
        let guard = screendraw::state();
        let Some(s) = guard.as_ref().filter(|s| s.id == session) else {
            return Ok(false);
        };
        (s.image.clone(), s.mapping.size())
    };
    if (w, h) != expected {
        return Err("Screen Draw preview dimensions do not match the recording".to_owned());
    }
    let stride = (w as usize)
        .checked_mul(4)
        .ok_or("invalid Screen Draw buffer size")?;
    let required = stride
        .checked_mul(h as usize)
        .ok_or("invalid Screen Draw buffer size")?;
    if dst.len() < required {
        return Err("the Screen Draw buffer is too small".to_owned());
    }
    for (row, line) in dst[..required].chunks_exact_mut(stride).enumerate() {
        line.fill(0);
        let Some(image) = &image else { continue };
        let src = &image.data()[row * stride..(row + 1) * stride];
        for (d, s) in line.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
            d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
        }
    }
    Ok(true)
}

/// Starts Screen Draw over `r` without a recording and returns its
/// announcement. For the UI self-test.
pub fn draw_demo(r: Rect) -> String {
    let Some(mapping) = jiffshot_capture::CaptureMapping::identity(r) else {
        eprintln!("screen draw: invalid demo dimensions");
        return String::new();
    };
    draw_demo_mapping(mapping)
}

/// Starts a Screen Draw demo with an explicit output size.
pub fn draw_demo_sized(r: Rect, width: u32, height: u32) -> String {
    let Some(mapping) = jiffshot_capture::CaptureMapping::new(r, width, height) else {
        eprintln!("screen draw: invalid demo dimensions");
        return String::new();
    };
    draw_demo_mapping(mapping)
}

fn draw_demo_mapping(mapping: jiffshot_capture::CaptureMapping) -> String {
    if !screendraw::show(mapping, std::time::Duration::ZERO) {
        eprintln!("screen draw: another drawing session is active");
        return String::new();
    }
    let guard = screendraw::state();
    let Some(s) = guard.as_ref() else {
        return String::new();
    };
    screendraw::announcement(s.id, s.mapping)
}

/// Ends Screen Draw.
pub fn draw_end() {
    screendraw::hide();
}

#[cfg(test)]
static OVERLAY_SESSION_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod copy_tests {
    use super::*;
    use jiffshot_capture::{targets::Targets, Frame};

    #[test]
    fn synthetic_overlay_copy_checks_dimensions_and_copies_negative_origins() {
        let _lock = OVERLAY_SESSION_TEST
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let id = u64::MAX - 1;
        struct Cleanup(u64);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                overlay_end_demo(self.0);
            }
        }
        let _cleanup = Cleanup(id);
        let pixels: Vec<u8> = (0..32).collect();
        let frame = Frame {
            width: 4,
            height: 2,
            pixels: pixels.clone(),
            alpha: false,
        };
        let overlay =
            overlay::Overlay::new(frame, (-4, 4), Targets::default(), Vec::new(), (-3, 4)).unwrap();
        {
            let mut session = overlay::session();
            assert!(session.is_none());
            *session = Some(overlay::Session::new(id, overlay));
        }

        let rect = Rect::new(-3, 4, 2, 2);
        let mut copied = [0xaa; 24];
        assert_eq!(overlay_copy(id, rect, 2, 2, &mut copied, 12), Ok(true));
        assert_eq!(&copied[..8], &pixels[4..12]);
        assert_eq!(&copied[12..20], &pixels[20..28]);
        assert_eq!(&copied[8..12], &[0xaa; 4]);
        assert!(overlay_copy(id, rect, 3, 2, &mut copied, 12).is_err());
        assert!(overlay_copy(id, rect, 2, 2, &mut copied, 7).is_err());
        assert!(overlay_copy(id, rect, 2, 2, &mut copied[..23], 12).is_err());
        assert_eq!(overlay_copy(id - 1, rect, 2, 2, &mut copied, 12), Ok(false));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::geometry::CaptureLayout;
    use crate::sys::{VK_LEFT, VK_RIGHT};
    use jiffshot_capture::{
        screen,
        targets::{Target, Targets},
        Frame,
    };

    #[test]
    fn selection_cursor_uses_retained_geometry_and_rejects_stale_layouts() {
        let layout = CaptureLayout::current().unwrap();
        let scale = layout.desktop_scale();
        let mut actual = None;
        let position = move_selection_cursor_with(&layout, -17.25, 29.125, |x, y| {
            actual = Some((x, y));
            Ok((x, y))
        })
        .unwrap();
        assert_eq!(actual, Some((-17.25 / scale, 29.125 / scale)));
        assert_eq!(position, (-17.25, 29.125));

        let stale = layout.clone().invalidated_for_test();
        let mut called = false;
        let error = move_selection_cursor_with(&stale, 1.0, 2.0, |_, _| {
            called = true;
            Ok((1.0, 2.0))
        })
        .unwrap_err();
        assert!(!called);
        assert!(error.to_string().contains("display configuration changed"));

        let error = move_selection_cursor_with(&layout, 1.0, 2.0, |_, _| {
            Err(std::io::Error::other(
                "Core Graphics cursor move failed (1002)",
            ))
        })
        .unwrap_err();
        assert!(error.to_string().contains("1002"));
        let error = move_selection_cursor_with(&layout, f64::NAN, 2.0, |_, _| {
            panic!("invalid coordinates must not be sent to Quartz")
        })
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        let error = move_selection_cursor_with(&layout, 1.0, 2.0, |_, _| Ok((f64::INFINITY, 2.0)))
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn selection_input_warps_to_precise_nudges_without_moving_the_system_pointer() {
        let _lock = OVERLAY_SESSION_TEST
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let id = u64::MAX - 3;
        struct Cleanup(u64);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                overlay_end_demo(self.0);
            }
        }
        let _cleanup = Cleanup(id);
        let layout = CaptureLayout::current().unwrap();
        let monitors = layout.monitors();
        let center = monitors
            .iter()
            .find(|monitor| monitor.primary)
            .unwrap()
            .rect
            .center();
        let origin = (center.0 - 50, center.1 - 50);
        let initial = (f64::from(center.0) + 0.25, f64::from(center.1) - 0.25);
        let rounded = overlay::rounded_desktop_point(initial).unwrap();
        let scale = layout.desktop_scale();
        let unit = layout
            .desktop_pixel_step_precise(initial.0, initial.1)
            .unwrap();
        let mut overlay = overlay::Overlay::new(
            Frame::new(100, 100),
            origin,
            Targets::default(),
            monitors,
            rounded,
        )
        .unwrap()
        .with_display_layout(layout);
        overlay.remember_pointer_position(initial);
        {
            let mut session = overlay::session();
            assert!(session.is_none());
            *session = Some(overlay::Session::new(id, overlay));
        }

        let assert_precise = |actual: [f64; 2], expected: (f64, f64)| {
            assert!((actual[0] - expected.0).abs() < 1e-10);
            assert!((actual[1] - expected.1).abs() < 1e-10);
        };
        for pixels in 1..=3 {
            let expected = (initial.0 + unit.0 * f64::from(pixels), initial.1);
            let mut actual = None;
            let state = overlay_input_with(id, INPUT_KEY, 0.0, 0.0, VK_RIGHT as i32, 0, |x, y| {
                actual = Some((x, y));
                Ok((x, y))
            });
            let actual = actual.expect("the key must produce a cursor target");
            assert!((actual.0 - expected.0 / scale).abs() < 1e-10);
            assert!((actual.1 - expected.1 / scale).abs() < 1e-10);
            let rounded = overlay::rounded_desktop_point(expected).unwrap();
            assert_eq!(state.cursor, [rounded.0, rounded.1]);
            assert_precise(state.cursor_precise, expected);
            assert!(!state.has(FLAG_DONE));
        }

        let clamped = (initial.0 + unit.0 * 3.25, initial.1);
        let state = overlay_input_with(id, INPUT_KEY, 0.0, 0.0, VK_RIGHT as i32, 0, |_, _| {
            Ok((clamped.0 / scale, clamped.1 / scale))
        });
        let rounded = overlay::rounded_desktop_point(clamped).unwrap();
        assert_eq!(state.cursor, [rounded.0, rounded.1]);
        assert_precise(state.cursor_precise, clamped);
        let expected = (clamped.0 - unit.0, clamped.1);
        let mut actual = None;
        overlay_input_with(id, INPUT_KEY, 0.0, 0.0, VK_LEFT as i32, 0, |x, y| {
            actual = Some((x, y));
            Ok((x, y))
        });
        let actual = actual.expect("a nudge after clamping must use the actual cursor position");
        assert!((actual.0 - expected.0 / scale).abs() < 1e-10);
        assert!((actual.1 - expected.1 / scale).abs() < 1e-10);

        let mouse = (f64::from(center.0) - 0.375, f64::from(center.1) + 1.25);
        let state = overlay_input_with(id, INPUT_MOVE, mouse.0, mouse.1, 0, 0, |_, _| {
            panic!("pointer input must not warp")
        });
        let rounded = overlay::rounded_desktop_point(mouse).unwrap();
        assert_eq!(state.cursor, [rounded.0, rounded.1]);
        assert_precise(state.cursor_precise, mouse);
        let expected = (mouse.0 - 10.0 * unit.0, mouse.1);
        let mut actual = None;
        let state = overlay_input_with(
            id,
            INPUT_KEY,
            0.0,
            0.0,
            VK_LEFT as i32,
            MOD_SHIFT,
            |x, y| {
                actual = Some((x, y));
                Ok((x, y))
            },
        );
        let actual = actual.expect("Shift+Left must produce a cursor target");
        assert!((actual.0 - expected.0 / scale).abs() < 1e-10);
        assert!((actual.1 - expected.1 / scale).abs() < 1e-10);
        let rounded = overlay::rounded_desktop_point(expected).unwrap();
        assert_eq!(state.cursor, [rounded.0, rounded.1]);
        assert_precise(state.cursor_precise, expected);

        let state = overlay_input_with(id, INPUT_MOVE, f64::NAN, 0.0, 0, 0, |_, _| {
            panic!("invalid input must not warp")
        });
        assert!(state.has(FLAG_DONE));
        assert!(!overlay_pending(id));
    }

    #[test]
    fn selection_input_uses_the_fractional_release_before_selecting_a_monitor() {
        let _lock = OVERLAY_SESSION_TEST
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let id = u64::MAX - 4;
        struct Cleanup(u64);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                overlay_end_demo(self.0);
            }
        }
        let _cleanup = Cleanup(id);
        let monitors = [0, 100]
            .map(|x| screen::Monitor {
                rect: Rect::new(x, 0, 100, 100),
                work: Rect::new(x, 0, 100, 100),
                dpi: 96,
                primary: x == 0,
                device: format!("display-{x}"),
            })
            .to_vec();
        let overlay = overlay::Overlay::new(
            Frame::new(200, 100),
            (0, 0),
            Targets::default(),
            monitors,
            (100, 50),
        )
        .unwrap();
        {
            let mut session = overlay::session();
            assert!(session.is_none());
            *session = Some(overlay::Session::new(id, overlay));
        }
        overlay_input_with(id, INPUT_PRESS, 100.4, 50.0, 0, 0, |_, _| {
            panic!("pointer input must not warp")
        });
        let state = overlay_input_with(id, INPUT_RELEASE, 99.6, 50.0, 0, 0, |_, _| {
            panic!("pointer input must not warp")
        });
        assert!(state.has(FLAG_DONE));
        assert_eq!(state.cursor, [100, 50]);
        assert_eq!(state.cursor_precise, [99.6, 50.0]);
        let session = overlay::session();
        assert!(matches!(
            &session.as_ref().unwrap().overlay.outcome,
            Some(Some(overlay::Selection::Monitor(rect))) if *rect == Rect::new(0, 0, 100, 100)
        ));
    }

    #[test]
    fn selection_wheel_uses_its_fractional_position_and_selectable_parent() {
        let _lock = OVERLAY_SESSION_TEST
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let id = u64::MAX - 6;
        struct Cleanup(u64);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                overlay_end_demo(self.0);
            }
        }
        let _cleanup = Cleanup(id);
        let layout = CaptureLayout::current().unwrap();
        let monitors = layout.monitors();
        let (x, y) = monitors
            .iter()
            .find(|monitor| monitor.primary)
            .unwrap()
            .rect
            .center();
        let scale = layout.desktop_scale();
        let seam = f64::from(x) + 0.125;
        let target = |left: f64, right: f64, title: &str, depth, parent| {
            let edges = [left, f64::from(y - 20), right, f64::from(y + 20)];
            Target {
                hwnd: 0,
                rect: Rect::from_ltrb(
                    edges[0].round() as i32,
                    edges[1] as i32,
                    edges[2].round() as i32,
                    edges[3] as i32,
                ),
                title: title.into(),
                class: String::new(),
                process_id: 0,
                depth,
                parent,
                logical_bounds: Some(
                    screen::LogicalBounds::new(edges.map(|edge| edge / scale)).unwrap(),
                ),
            }
        };
        let targets = Targets {
            list: vec![
                target(f64::from(x - 30), seam, "Left", 0, None),
                target(f64::from(x - 15), seam, "Left child", 3, Some(0)),
                target(seam, f64::from(x + 30), "Right", 0, None),
                target(seam, f64::from(x + 15), "Right child", 4, Some(2)),
            ],
        };
        let origin = (x - 50, y - 50);
        let overlay =
            overlay::Overlay::new(Frame::new(100, 100), origin, targets, monitors, (x, y))
                .unwrap()
                .with_display_layout(layout);
        {
            let mut session = overlay::session();
            assert!(session.is_none());
            *session = Some(overlay::Session::new(id, overlay));
        }

        let left = f64::from(x) + 0.1;
        let right = f64::from(x) + 0.15;
        assert_eq!(left.round(), right.round());
        overlay_input_with(id, INPUT_MOVE, left, f64::from(y), 0, 0, |_, _| {
            panic!("pointer input must not warp")
        });
        assert!(overlay_text(id, TEXT_TITLE)
            .unwrap()
            .starts_with("Left child"));
        let state = overlay_input_with(id, INPUT_WHEEL, right, f64::from(y), 120, 0, |_, _| {
            panic!("wheel input must not warp")
        });
        assert_eq!(state.cursor_precise, [right, f64::from(y)]);
        assert_eq!(overlay_text(id, TEXT_TITLE).unwrap(), "Right");
        for _ in 0..3 {
            overlay_input_with(id, INPUT_WHEEL, right, f64::from(y), 120, 0, |_, _| {
                panic!("wheel input must not warp")
            });
        }
        overlay_input_with(id, INPUT_WHEEL, right, f64::from(y), -120, 0, |_, _| {
            panic!("wheel input must not warp")
        });
        assert!(overlay_text(id, TEXT_TITLE)
            .unwrap()
            .starts_with("Right child"));
        let state = overlay_input_with(id, INPUT_WHEEL, f64::NAN, f64::from(y), 120, 0, |_, _| {
            panic!("invalid input must not warp")
        });
        assert!(state.has(FLAG_DONE));
        assert!(!overlay_pending(id));
    }

    #[test]
    fn stale_selection_input_ends_the_overlay_without_moving_the_pointer() {
        let _lock = OVERLAY_SESSION_TEST
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let id = u64::MAX - 2;
        struct Cleanup(u64);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                overlay_end_demo(self.0);
            }
        }
        let _cleanup = Cleanup(id);
        for (kind, value) in [
            (INPUT_MOVE, 0),
            (INPUT_PRESS, 0),
            (INPUT_RELEASE, 0),
            (INPUT_WHEEL, 120),
            (INPUT_KEY, VK_RIGHT as i32),
        ] {
            let layout = CaptureLayout::current().unwrap().invalidated_for_test();
            let overlay = overlay::Overlay::new(
                Frame::new(20, 20),
                (0, 0),
                Targets::default(),
                layout.monitors(),
                (5, 5),
            )
            .unwrap()
            .with_display_layout(layout);
            {
                let mut session = overlay::session();
                assert!(session.is_none());
                *session = Some(overlay::Session::new(id, overlay));
            }
            let state = overlay_input_with(id, kind, 5.25, 5.0, value, 0, |_, _| {
                panic!("stale input must not warp")
            });
            assert!(state.has(FLAG_DONE));
            assert!(!overlay_pending(id));
            overlay_end_demo(id);
        }
    }

    #[test]
    fn invalid_preview_geometry_ends_the_selection_with_an_error() {
        let _lock = OVERLAY_SESSION_TEST
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let id = u64::MAX - 5;
        struct Cleanup(u64);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                overlay_end_demo(self.0);
            }
        }
        let _cleanup = Cleanup(id);
        let layout = CaptureLayout::current().unwrap();
        let center = layout
            .monitors()
            .iter()
            .find(|monitor| monitor.primary)
            .unwrap()
            .rect
            .center();
        let origin = (center.0 - 50, center.1 - 50);
        let mut overlay = overlay::Overlay::new(
            Frame::new(100, 100),
            origin,
            Targets::default(),
            layout.monitors(),
            center,
        )
        .unwrap()
        .with_display_layout(layout);
        overlay.remember_pointer_position((f64::NAN, 0.0));
        {
            let mut session = overlay::session();
            assert!(session.is_none());
            *session = Some(overlay::Session::new(id, overlay));
        }
        assert!(overlay_text(id, TEXT_SIZE).is_err());
        let state = overlay_input_with(id, 6, 0.0, 0.0, 0, 0, |_, _| {
            panic!("refresh must not warp")
        });
        assert!(state.has(FLAG_DONE));
        assert!(!overlay_pending(id));
        assert!(matches!(
            overlay::session().as_ref().unwrap().overlay.outcome,
            Some(None)
        ));
    }

    #[test]
    #[ignore = "moves the cursor on a macOS desktop"]
    fn arrow_keys_update_the_selection_without_a_mouse_event() {
        let _lock = OVERLAY_SESSION_TEST
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        struct Restore {
            cursor: (i32, i32),
            session: Option<u64>,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                let mut session = overlay::session();
                if session
                    .as_ref()
                    .is_some_and(|session| Some(session.id) == self.session)
                {
                    *session = None;
                }
                drop(session);
                // SAFETY: restore the cursor after the isolated test.
                if unsafe { SetCursorPos(self.cursor.0, self.cursor.1) } == 0 {
                    eprintln!("could not restore the test cursor");
                }
            }
        }

        let mut restore = Restore {
            cursor: screen::cursor_pos(),
            session: None,
        };
        let layout = CaptureLayout::current().unwrap();
        let monitors = layout.monitors();
        let monitor = monitors.iter().find(|monitor| monitor.primary).unwrap();
        let center = monitor.rect.center();
        // SAFETY: move to a known point inside the primary display.
        assert_ne!(unsafe { SetCursorPos(center.0, center.1) }, 0);
        let start = screen::cursor_pos();
        let unit = layout
            .desktop_pixel_step_precise(f64::from(start.0), f64::from(start.1))
            .unwrap()
            .0;
        let tolerance = layout.desktop_scale().ceil() as u32;
        let origin = (start.0 - 50, start.1 - 50);
        let overlay = overlay::Overlay::new(
            Frame::new(100, 100),
            origin,
            Targets::default(),
            monitors,
            start,
        )
        .unwrap()
        .with_display_layout(layout);
        let id = u64::MAX;
        {
            let mut session = overlay::session();
            assert!(session.is_none());
            *session = Some(overlay::Session::new(id, overlay));
            restore.session = Some(id);
        }
        for pixels in 1..=3 {
            let state = overlay_input(id, INPUT_KEY, 0.0, 0.0, VK_RIGHT as i32, 0);
            let expected = start.0 + (unit * pixels as f64).round() as i32;
            assert_eq!(state.cursor, [expected, start.1]);
            let actual = screen::cursor_pos();
            assert!(
                actual.0.abs_diff(expected) <= tolerance && actual.1.abs_diff(start.1) <= tolerance
            );
        }
        assert_ne!(screen::cursor_pos().0, start.0);
        let state = overlay_input(id, INPUT_KEY, 0.0, 0.0, VK_LEFT as i32, MOD_SHIFT);
        let expected = start.0 + (unit * -7.0).round() as i32;
        assert_eq!(state.cursor, [expected, start.1]);
        let actual = screen::cursor_pos();
        assert!(
            actual.0.abs_diff(expected) <= tolerance && actual.1.abs_diff(start.1) <= tolerance
        );
    }
}
