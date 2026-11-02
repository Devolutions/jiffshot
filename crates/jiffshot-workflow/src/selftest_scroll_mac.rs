//! The scrolling, menu and recording self-tests on macOS.

use jiffshot_capture::{media, screen, CaptureMapping, Frame, Rect};
use objc2::rc::Retained;
use objc2::{AllocAnyThread, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSEvent, NSEventModifierFlags, NSEventType, NSImage,
    NSImageScaling, NSImageView, NSMenu, NSScreen, NSScrollView, NSStatusWindowLevel, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSData, NSNumber, NSPoint, NSRect, NSSize, NSString};
use std::cell::RefCell;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

thread_local! {
    static FIXTURE: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
    static EXCLUSION_FIXTURE: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
    static SCROLL_FIXTURE: RefCell<Option<(Retained<NSScrollView>, bool)>> = const { RefCell::new(None) };
    static MENU_FIXTURE: RefCell<Option<Retained<NSMenu>>> = const { RefCell::new(None) };
}

const SCROLL_WIDTH: u32 = 320;
const SCROLL_HEIGHT: u32 = 240;
const SCROLL_HEADER: u32 = 24;
const VIEWPORT_HEIGHT: u32 = SCROLL_HEIGHT - SCROLL_HEADER;

#[derive(Clone, Copy)]
enum ScrollMode {
    Vertical,
    Horizontal,
    Both,
}

impl ScrollMode {
    fn from_env() -> io::Result<Self> {
        match std::env::var("JIFFSHOT_SCROLL_AXES").as_deref() {
            Err(std::env::VarError::NotPresent) | Ok("vertical") => Ok(Self::Vertical),
            Ok("horizontal") => Ok(Self::Horizontal),
            Ok("both") => Ok(Self::Both),
            _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "JIFFSHOT_SCROLL_AXES must be vertical, horizontal, or both")),
        }
    }

    fn page_size(self) -> (u32, u32) {
        match self {
            Self::Vertical => (SCROLL_WIDTH, 1_300),
            Self::Horizontal => (960, VIEWPORT_HEIGHT),
            Self::Both => (960, 650),
        }
    }

    fn filename(self) -> &'static str {
        match self {
            Self::Vertical => "scrolling.png",
            Self::Horizontal => "scrolling-horizontal.png",
            Self::Both => "scrolling-both.png",
        }
    }
}

fn scroll_page(width: u32, height: u32) -> Frame {
    let mut page = Frame::new(width, height);
    let stride = page.stride();
    for y in 0..height as usize {
        for x in 0..width as usize {
            let mut n = ((x / 3) as u32).wrapping_mul(0x9E37_79B9) ^ ((y / 3) as u32).wrapping_mul(0x85EB_CA6B);
            n ^= n >> 15;
            n = n.wrapping_mul(0x2C1B_3C6D);
            let ink = y % 26 < 14 && x > 15 && x < width as usize - 30 && (n >> 28) > 6;
            let v = if ink { (n >> 24) as u8 / 3 } else { 250 };
            page.pixels[y * stride + x * 4..y * stride + x * 4 + 4]
                .copy_from_slice(&[v, v, if ink { 120 } else { 250 }, 255]);
        }
    }
    page
}

fn check_scroll_dimensions(out: &Frame, expected_width: u32, expected_height: u32) -> io::Result<()> {
    if out.width.abs_diff(expected_width) > 4 || out.height.abs_diff(expected_height) > 4 {
        return Err(io::Error::other(format!(
            "scrolling capture measured {}x{}, expected {}x{}",
            out.width, out.height, expected_width, expected_height
        )));
    }
    Ok(())
}

fn check_scroll_page(out: &Frame, rect: Rect, header: usize, scale: (f64, f64), page_width: u32, page_height: u32) -> io::Result<()> {
    let max_x = page_width - SCROLL_WIDTH;
    let max_y = page_height - VIEWPORT_HEIGHT;
    let positions = |max: u32, step: u32| {
        let mut offsets: Vec<u32> = (0..max).step_by(step as usize).collect();
        offsets.push(max);
        offsets
    };
    for y in positions(max_y, VIEWPORT_HEIGHT) {
        for x in positions(max_x, SCROLL_WIDTH) {
            dispatch2::DispatchQueue::main().exec_sync(move || {
                SCROLL_FIXTURE.with(|fixture| {
                    let fixture = fixture.borrow();
                    let (scroll, flipped) = fixture.as_ref().expect("scrolling fixture remains open");
                    let scroll_y = if *flipped { y as f64 } else { (max_y - y) as f64 };
                    let clip = scroll.contentView();
                    clip.scrollToPoint(NSPoint::new(x as f64, scroll_y));
                    scroll.reflectScrolledClipView(&clip);
                    scroll.display();
                });
            });
            std::thread::sleep(Duration::from_millis(100));
            let reference = screen::grab(rect)?;
            let dx = (x as f64 * scale.0).round() as usize;
            let dy = (y as f64 * scale.1).round() as usize;
            for row in 0..reference.height as usize {
                let out_row = if row < header { row } else { row + dy };
                let start = out_row * out.stride() + dx * 4;
                let actual = out.pixels.get(start..start + reference.stride()).ok_or_else(|| io::Error::other("the stitched page is too small"))?;
                let expected = &reference.pixels[row * reference.stride()..(row + 1) * reference.stride()];
                if !actual.chunks_exact(4).zip(expected.chunks_exact(4)).all(|(a, b)| a[..3] == b[..3]) {
                    return Err(io::Error::other(format!("the stitched page differs from the reference at ({x}, {y}), row {row}")));
                }
            }
        }
    }
    Ok(())
}

fn check_appkit_backing(mtm: MainThreadMarker) -> io::Result<()> {
    let layout = screen::DisplayLayout::current()?;
    let mut matched = Vec::new();
    for screen in NSScreen::screens(mtm).iter() {
        let id = screen.deviceDescription().objectForKey(&NSString::from_str("NSScreenNumber"))
            .and_then(|value| value.downcast::<NSNumber>().ok())
            .map(|number| number.unsignedIntValue())
            .filter(|&id| id != 0)
            .ok_or_else(|| io::Error::other("an AppKit screen has no display ID"))?;
        let device = format!("display-{id}");
        let Some(monitor) = layout.monitors().iter().find(|monitor| monitor.device == device) else {
            continue;
        };
        let backing = screen.convertRectToBacking(screen.frame()).size;
        if !backing.width.is_finite() || !backing.height.is_finite() || backing.width <= 0.0 || backing.height <= 0.0 {
            return Err(io::Error::other(format!("{device} has invalid AppKit backing dimensions")));
        }
        let expected = (backing.width.round() as u32, backing.height.round() as u32);
        let actual = layout.monitor_mapping(monitor)?.size();
        if actual != expected {
            return Err(io::Error::other(format!("{device} capture size {actual:?} does not match AppKit backing {expected:?}")));
        }
        matched.push(device);
    }
    if let Some(monitor) = layout.monitors().iter().find(|monitor| !matched.contains(&monitor.device)) {
        return Err(io::Error::other(format!("{} has no matching AppKit screen", monitor.device)));
    }
    layout.validate()?;
    println!("AppKit backing dimensions match {} active display(s)", matched.len());
    Ok(())
}

fn check_capture_pixels(label: &str, expected: &Frame, still: &Frame, recording: &[u8]) -> io::Result<()> {
    if (still.width, still.height) != (expected.width, expected.height) {
        return Err(io::Error::other(format!("{label} still capture has unexpected dimensions")));
    }
    let compare = |kind: &str, actual: &[u8], tolerance: u8| {
        if actual.len() != expected.pixels.len() {
            return Err(io::Error::other(format!("{label} {kind} capture has an unexpected buffer size")));
        }
        if let Some((index, (a, b))) = expected.pixels.chunks_exact(4).zip(actual.chunks_exact(4)).enumerate()
            .find(|(_, (a, b))| a[..3].iter().zip(&b[..3]).any(|(a, b)| a.abs_diff(*b) > tolerance))
        {
            let differences = expected.pixels.chunks_exact(4).zip(actual.chunks_exact(4))
                .map(|(a, b)| a[..3].iter().zip(&b[..3]).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or_default());
            let maximum = differences.clone().max().unwrap_or_default();
            let differing = differences.filter(|difference| *difference > tolerance).count();
            return Err(io::Error::other(format!(
                "{label} {kind} capture at {}x{} differs from the frozen image at ({}, {}): {:?} vs {:?}; {differing} pixels exceed tolerance {tolerance}, maximum difference {maximum}",
                expected.width, expected.height,
                index % expected.width as usize, index / expected.width as usize, &a[..3], &b[..3]
            )));
        }
        Ok(())
    };
    compare("still", &still.pixels, 0)?;
    // Screenshots are redrawn through Core Graphics; streams deliver raw 8-bit pixels.
    compare("stream", recording, 2)?;
    Ok(())
}

fn check_capture_mapping(layout: &screen::DisplayLayout, frozen: &screen::FrozenDesktop, mapping: CaptureMapping) -> io::Result<()> {
    let expected = frozen.render(mapping)?;
    let still = layout.grab_mapped(mapping)?;
    let mut stream = screen::Grabber::for_mapping(mapping, layout)?;
    check_capture_pixels(&format!("reference-grid {:?}", mapping.rect()), &expected, &still, stream.grab()?)
}

fn check_logical_capture_mapping(
    layout: &screen::DisplayLayout,
    frozen: &screen::FrozenDesktop,
    mapping: screen::LogicalCaptureMapping,
) -> io::Result<()> {
    let expected = frozen.render_logical(mapping)?;
    let still = layout.grab_logical(mapping)?;
    let mut stream = screen::Grabber::for_logical(mapping, layout)?;
    check_capture_pixels(&format!("logical {:?}", mapping.bounds().edges()), &expected, &still, stream.grab()?)
}

fn with_window_fixture_path<T>(root: &Path, run: impl FnOnce(&Path) -> io::Result<T>) -> io::Result<T> {
    use std::os::unix::fs::DirBuilderExt;

    let mut created = None;
    for attempt in 0..100 {
        let directory = root.join(format!(".jiffshot-window-fixture-{}-{attempt}", std::process::id()));
        match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {
                created = Some(directory);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let directory = created.ok_or_else(|| io::Error::new(io::ErrorKind::AlreadyExists, "could not reserve a window fixture directory"))?;
    let path = directory.join("window.png");
    let result = run(&path);
    let mut cleanup = Vec::new();
    if let Err(error) = std::fs::remove_file(&path) {
        if error.kind() != io::ErrorKind::NotFound {
            cleanup.push(format!("could not remove {}: {error}", path.display()));
        }
    }
    if let Err(error) = std::fs::remove_dir(&directory) {
        cleanup.push(format!("could not remove {}: {error}", directory.display()));
    }
    if cleanup.is_empty() {
        result
    } else {
        let cleanup = cleanup.join("; ");
        match result {
            Ok(_) => Err(io::Error::other(cleanup)),
            Err(error) => Err(io::Error::other(format!("{error}; {cleanup}"))),
        }
    }
}

fn check_synthetic_cursor(base: &Frame, actual: &Frame, mapping: CaptureMapping, bounds: [f64; 4]) -> io::Result<()> {
    if mapping.size() != (base.width, base.height) || (actual.width, actual.height) != (base.width, base.height) {
        return Err(io::Error::other("the synthetic cursor capture has incorrect dimensions"));
    }
    let mut covered = 0;
    for y in 0..base.height {
        for x in 0..base.width {
            let (px, py) = mapping.pixel_center(x, y);
            let inside = px >= bounds[0] && px < bounds[2] && py >= bounds[1] && py < bounds[3];
            let expected = if inside {
                covered += 1;
                Some((245, 11, 7))
            } else {
                base.rgb(x as i32, y as i32)
            };
            if actual.rgb(x as i32, y as i32) != expected {
                return Err(io::Error::other(format!("the synthetic cursor differs at output pixel ({x}, {y})")));
            }
        }
    }
    if covered == 0 {
        return Err(io::Error::other("the synthetic cursor did not cover any output pixel"));
    }
    Ok(())
}

fn check_frozen_fixture(rect: Rect, page_width: u32, page_height: u32, window_id: isize, output_dir: &Path) -> io::Result<()> {
    let layout = screen::DisplayLayout::current()?;
    let mapping = layout.mapping(rect)?;
    let started = Instant::now();
    let frozen = layout.freeze()?;
    let grabbed = started.elapsed();
    let before = frozen.render(mapping)?;
    let rendered = started.elapsed() - grabbed;
    let current = layout.grab(rect)?;
    if before.pixels != current.pixels {
        return Err(io::Error::other("the frozen fixture differs from an immediate capture"));
    }
    let offset = Rect::new(rect.x + 1, rect.y + 1, rect.width - 3, rect.height - 3);
    for (area, width, height) in [
        (rect, mapping.size().0, mapping.size().1),
        (rect, 173, 137),
        (rect, 961, 721),
        (offset, 197, 149),
    ] {
        let mapping = CaptureMapping::new(area, width, height).ok_or_else(|| io::Error::other("invalid capture fixture mapping"))?;
        check_capture_mapping(&layout, &frozen, mapping)?;
    }
    let precise = CaptureMapping::with_desktop_bounds(rect, 197, 149, [
        rect.x as f64 + 0.25,
        rect.y as f64 - 0.25,
        rect.right() as f64 + 0.25,
        rect.bottom() as f64 - 0.25,
    ]).ok_or_else(|| io::Error::other("invalid precise capture fixture mapping"))?;
    check_capture_mapping(&layout, &frozen, precise)?;

    let scale = layout.desktop_scale();
    let left = rect.x as f64 / scale + 0.125;
    let top = rect.y as f64 / scale + 0.125;
    let bounds = screen::LogicalBounds::new([
        left,
        top,
        rect.right() as f64 / scale - 0.125,
        rect.bottom() as f64 / scale - 0.125,
    ])?;
    let small = screen::LogicalBounds::new([left + 0.5, top + 0.5, left + 1.0, top + 1.0])?;
    for logical in [
        layout.logical_mapping(bounds)?,
        screen::LogicalCaptureMapping::new(bounds, (197, 149))?,
        layout.logical_mapping(small)?,
    ] {
        check_logical_capture_mapping(&layout, &frozen, logical)?;
    }

    let window = jiffshot_capture::targets::windows(&layout)?.into_iter().find(|window| window.hwnd == window_id)
        .ok_or_else(|| io::Error::other("the synthetic fixture window was not found"))?;
    let (window_rect, window_bounds) = window.clipped_to_layout(&layout)?;
    let expected_window = frozen.render_logical(layout.logical_mapping(window_bounds)?)?;
    with_window_fixture_path(output_dir, |window_path| {
        let response = crate::control_macos::selftest_request("screenshot", serde_json::json!({
            "window": window_id,
            "no_activate": true,
            "out": window_path,
        })).map_err(io::Error::other)?;
        let saved_window = Frame::load_png(window_path)?;
        if !same_rgb(&saved_window, &expected_window)
            || response["width"] != expected_window.width
            || response["height"] != expected_window.height
            || response["target"]["rect"] != serde_json::json!(window_rect)
            || response["target"]["window"]["hwnd"] != window_id
        {
            return Err(io::Error::other(format!("the control window capture differs from the frozen fixture: {response}")));
        }
        Ok(())
    })?;

    let desktop = layout.virtual_screen();
    let edges = bounds.edges().map(|edge| (edge * scale).round() as i32);
    let target_rect = Rect::from_ltrb(edges[0], edges[1], edges[2], edges[3]);
    let targets = jiffshot_capture::targets::Targets { list: vec![jiffshot_capture::targets::Target {
        hwnd: window_id, rect: target_rect, title: "Synthetic fixture".into(), class: "Fixture".into(),
        process_id: std::process::id(), depth: 0, parent: None, logical_bounds: Some(bounds),
    }] };
    let mut overlay = crate::overlay::Overlay::new(
        Frame::new(0, 0),
        (desktop.x, desktop.y),
        targets,
        layout.monitors().to_vec(),
        rect.center(),
    )?
    .with_display_layout(crate::geometry::CaptureLayout::from_display(layout.clone()))
    .with_frozen(frozen, desktop);
    let desktop_bounds = layout.logical_virtual_bounds()?;
    let desktop_mapping = layout.logical_mapping(desktop_bounds)?;
    let desktop_selection = crate::overlay::Selection::Desktop(desktop);
    let desktop_layout = overlay.display_layout.as_ref().expect("the fixture retains its display")
        .for_selection(&desktop_selection, &overlay.targets)?;
    let expected_desktop = overlay.frozen.as_ref().expect("the fixture retains its frozen desktop").render_logical(desktop_mapping)?;
    let desktop_capture = crate::workflow::from_overlay(&overlay, &desktop_selection, None, false, false)?
        .ok_or_else(|| io::Error::other("the logical desktop selection produced no capture"))?;
    if desktop_capture.frame.pixels != expected_desktop.pixels
        || desktop_capture.meta.kind != jiffshot_capture::library::CaptureKind::Desktop
    {
        return Err(io::Error::other("the logical desktop selection does not match the frozen capture"));
    }
    let (desktop_recording, spec) = crate::recorder::recording_geometry(desktop, 30, &desktop_layout).map_err(io::Error::other)?;
    if desktop_recording.desktop_bounds() != desktop_bounds.edges().map(|edge| edge * scale) {
        return Err(io::Error::other("the desktop recording lost its logical bounds"));
    }
    let logical_recording = screen::LogicalCaptureMapping::new(desktop_bounds, (spec.width, spec.height))?;
    let expected_recording = overlay.frozen.as_ref().expect("the fixture retains its frozen desktop").render_logical(logical_recording)?;
    let still_recording = layout.grab_logical(logical_recording)?;
    let mut stream = desktop_layout.grabber_for_mapping(desktop_recording)?;
    let recorded = Frame { width: spec.width, height: spec.height, pixels: stream.grab()?.to_vec(), alpha: false };
    drop(stream);
    let sample = desktop_recording.image_rect(rect).intersect(Rect::new(0, 0, spec.width as i32, spec.height as i32))
        .map(|area| area.inflate(-4)).filter(|area| !area.is_empty())
        .ok_or_else(|| io::Error::other("the desktop recording does not cover the synthetic fixture"))?;
    let expected_sample = expected_recording.crop(sample).ok_or_else(|| io::Error::other("could not crop the frozen desktop fixture"))?;
    let still_sample = still_recording.crop(sample).ok_or_else(|| io::Error::other("could not crop the still desktop fixture"))?;
    let recorded_sample = recorded.crop(sample).ok_or_else(|| io::Error::other("could not crop the recorded desktop fixture"))?;
    check_capture_pixels("logical desktop recording fixture", &expected_sample, &still_sample, &recorded_sample.pixels)?;

    let selection = crate::overlay::Selection::Window { rect: target_rect, target: 0 };
    let selected_layout = overlay.display_layout.as_ref().expect("the fixture retains its display")
        .for_selection(&selection, &overlay.targets)?;
    let logical = layout.logical_mapping(bounds)?;
    let expected = overlay.frozen.as_ref().expect("the fixture retains its frozen desktop").render_logical(logical)?;
    let capture = crate::workflow::from_overlay(&overlay, &selection, None, false, false)?
        .ok_or_else(|| io::Error::other("the logical window selection produced no capture"))?;
    let view = overlay.view()?;
    if capture.frame.pixels != expected.pixels || capture.meta.kind != jiffshot_capture::library::CaptureKind::Window
        || view.lit_kind != 1 || view.size != format!("{} × {}", expected.width, expected.height)
    {
        return Err(io::Error::other("the logical window selection or its preview does not match the frozen capture"));
    }
    let cursor_bounds = [
        (left + 5.375) * scale,
        (top + 5.625) * scale,
        (left + 8.375) * scale,
        (top + 7.625) * scale,
    ];
    let cursor = screen::CursorSnapshot::from_image_with_bounds(
        Frame { width: 6, height: 4, pixels: vec![7, 11, 245, 255].repeat(24), alpha: true },
        cursor_bounds,
    )?;
    let flat = crate::workflow::from_overlay(&overlay, &selection, Some(&cursor), true, false)?
        .ok_or_else(|| io::Error::other("the synthetic cursor capture produced no image"))?;
    let separate = crate::workflow::from_overlay(&overlay, &selection, Some(&cursor), true, true)?
        .ok_or_else(|| io::Error::other("the editable synthetic cursor capture produced no image"))?;
    let (pointer, position) = separate.cursor.as_ref().ok_or_else(|| io::Error::other("the synthetic cursor was not kept editable"))?;
    if separate.frame.pixels != expected.pixels {
        return Err(io::Error::other("the editable synthetic cursor changed the frozen background"));
    }
    let mut recomposed = separate.frame.clone();
    recomposed.composite_over(pointer, (i64::from(position.0), i64::from(position.1)))?;
    if recomposed.pixels != flat.frame.pixels {
        return Err(io::Error::other("the editable and flattened synthetic cursors differ"));
    }
    check_synthetic_cursor(&expected, &flat.frame, selected_layout.mapping(target_rect)?, cursor_bounds)?;
    let still = selected_layout.grab(target_rect)?;
    if still.pixels != expected.pixels {
        return Err(io::Error::other("the logical window's immediate and frozen captures differ"));
    }
    let (recording, spec) = crate::recorder::recording_geometry(target_rect, 30, &selected_layout).map_err(io::Error::other)?;
    let logical_recording = screen::LogicalCaptureMapping::new(bounds, (spec.width, spec.height))?;
    let expected_recording = overlay.frozen.as_ref().expect("the fixture retains its frozen desktop").render_logical(logical_recording)?;
    let mut recording_cursor = expected_recording.clone();
    cursor.draw_onto_mapped(&mut recording_cursor, recording)?;
    check_synthetic_cursor(&expected_recording, &recording_cursor, recording, cursor_bounds)?;
    let still_recording = layout.grab_logical(logical_recording)?;
    let mut stream = selected_layout.grabber_for_mapping(recording)?;
    check_capture_pixels("logical window recording", &expected_recording, &still_recording, stream.grab()?)?;
    drop(stream);

    let id = u64::MAX - 2;
    struct EndOverlay(u64);
    impl Drop for EndOverlay {
        fn drop(&mut self) {
            crate::ui_api::overlay_end_demo(self.0);
        }
    }
    {
        let mut session = crate::overlay::session();
        if session.is_some() {
            return Err(io::Error::other("another selection is active during the frozen fixture"));
        }
        *session = Some(crate::overlay::Session::new(id, overlay));
    }
    let _end_overlay = EndOverlay(id);
    let mut sampled_fixture = false;
    for monitor in layout.monitors() {
        let native = layout.monitor_mapping(monitor)?;
        let (width, height) = native.size();
        let stride = width as usize * 4;
        let mut pixels = vec![0; stride * height as usize];
        if !crate::ui_api::overlay_copy(id, monitor.rect, width, height, &mut pixels, stride).map_err(io::Error::other)? {
            return Err(io::Error::other("the frozen overlay ended before its monitor was copied"));
        }
        let rendered = {
            let session = crate::overlay::session();
            session.as_ref().expect("the fixture session is active").overlay.frozen.as_ref()
                .expect("the fixture has a frozen desktop").render(native)?
        };
        if !pixels.chunks_exact(4).zip(rendered.pixels.chunks_exact(4)).all(|(a, b)| a[..3] == b[..3]) {
            return Err(io::Error::other("the overlay monitor differs from its native frozen image"));
        }
        let selected = {
            let session = crate::overlay::session();
            let overlay = &session.as_ref().expect("the fixture session is active").overlay;
            crate::workflow::from_overlay(overlay, &crate::overlay::Selection::Monitor(monitor.rect), None, false, false)?
                .ok_or_else(|| io::Error::other("the frozen monitor selection produced no capture"))?
        };
        if selected.frame.pixels != rendered.pixels {
            return Err(io::Error::other("the selected monitor differs from its native frozen image"));
        }
        let (x, y) = rect.center();
        if monitor.rect.contains(x, y) {
            let (px, py) = native.image_point(x, y);
            let at = py as usize * stride + px as usize * 4;
            let state = crate::ui_api::overlay_input(id, -1, x as f64, y as f64, 0, 0);
            if !state.has(crate::ui_api::FLAG_COLOR)
                || state.color != [pixels[at + 2] as i32, pixels[at + 1] as i32, pixels[at] as i32]
            {
                return Err(io::Error::other("the overlay color does not match the native monitor pixel"));
            }
            let area = native.image_rect(rect).intersect(Rect::new(0, 0, width as i32, height as i32))
                .ok_or_else(|| io::Error::other("the frozen fixture has no native pixels"))?;
            let mut adjacent = None;
            'pixels: for py in area.y..area.bottom() {
                for px in area.x..area.right() - 1 {
                    let at = py as usize * stride + px as usize * 4;
                    if pixels[at..at + 3] == pixels[at + 4..at + 7] {
                        continue;
                    }
                    let first = native.pixel_center(px as u32, py as u32);
                    let second = native.pixel_center(px as u32 + 1, py as u32);
                    let boundary = (first.0 + second.0) / 2.0;
                    let offset = ((second.0 - first.0) / 4.0).min(0.2);
                    if (boundary - offset).round() != (boundary + offset).round() {
                        continue;
                    }
                    let color = |at: usize| [pixels[at + 2] as i32, pixels[at + 1] as i32, pixels[at] as i32];
                    adjacent = Some(((boundary - offset, first.1), (boundary + offset, first.1), color(at), color(at + 4)));
                    break 'pixels;
                }
            }
            let (left, right, left_color, right_color) = adjacent
                .ok_or_else(|| io::Error::other("the fixture has no adjacent native pixels with the same rounded pointer position"))?;
            let first = crate::ui_api::overlay_input(id, crate::ui_api::INPUT_MOVE, left.0, left.1, 0, 0);
            let second = crate::ui_api::overlay_input(id, crate::ui_api::INPUT_MOVE, right.0, right.1, 0, 0);
            if first.cursor != second.cursor || first.cursor_precise != [left.0, left.1]
                || second.cursor_precise != [right.0, right.1]
                || !first.has(crate::ui_api::FLAG_COLOR) || !second.has(crate::ui_api::FLAG_COLOR)
                || first.color != left_color || second.color != right_color
            {
                return Err(io::Error::other("the frozen overlay did not sample both fractional pointer positions"));
            }
            sampled_fixture = true;
        }
    }
    if !sampled_fixture {
        return Err(io::Error::other("the frozen fixture is not on an available monitor"));
    }
    let (left, top, right, bottom) = (rect.x, rect.y, rect.right() - 1, rect.bottom() - 1);
    crate::ui_api::overlay_input(id, crate::ui_api::INPUT_PRESS, left as f64, top as f64, 0, 0);
    crate::ui_api::overlay_input(id, crate::ui_api::INPUT_MOVE, right as f64, bottom as f64, 0, 0);
    let state = crate::ui_api::overlay_input(id, crate::ui_api::INPUT_RELEASE, right as f64, bottom as f64, 0, 0);
    if !state.has(crate::ui_api::FLAG_DONE) {
        return Err(io::Error::other("the frozen overlay did not finish selecting the fixture"));
    }
    {
        let session = crate::overlay::session();
        let overlay = &session.as_ref().expect("the fixture session is active").overlay;
        let selection = overlay.outcome.as_ref().and_then(Option::as_ref)
            .ok_or_else(|| io::Error::other("the frozen overlay did not return a selection"))?;
        if selection.rect() != rect {
            return Err(io::Error::other("the frozen overlay selected different fixture bounds"));
        }
        let capture = crate::workflow::from_overlay(overlay, selection, None, false, false)?
            .ok_or_else(|| io::Error::other("the frozen overlay produced no capture"))?;
        if capture.frame.pixels != before.pixels {
            return Err(io::Error::other("the frozen overlay capture differs from the selected fixture"));
        }
    }

    let max_x = page_width - SCROLL_WIDTH;
    let max_y = page_height - VIEWPORT_HEIGHT;
    dispatch2::DispatchQueue::main().exec_sync(move || {
        SCROLL_FIXTURE.with(|fixture| {
            let fixture = fixture.borrow();
            let (scroll, flipped) = fixture.as_ref().expect("scrolling fixture remains open");
            let clip = scroll.contentView();
            clip.scrollToPoint(NSPoint::new(max_x as f64, if *flipped { max_y as f64 } else { 0.0 }));
            scroll.reflectScrolledClipView(&clip);
            scroll.display();
        });
    });
    std::thread::sleep(Duration::from_millis(150));
    if layout.grab(rect)?.pixels == before.pixels {
        return Err(io::Error::other("the scrolling fixture did not change after it was frozen"));
    }
    let after_scroll = {
        let session = crate::overlay::session();
        session.as_ref().expect("the fixture session is active").overlay.frozen.as_ref()
            .expect("the fixture has a frozen desktop").render(mapping)?
    };
    if after_scroll.pixels != before.pixels {
        return Err(io::Error::other("the frozen fixture changed after the window scrolled"));
    }
    dispatch2::DispatchQueue::main().exec_sync(move || {
        SCROLL_FIXTURE.with(|fixture| {
            let fixture = fixture.borrow();
            let (scroll, flipped) = fixture.as_ref().expect("scrolling fixture remains open");
            let clip = scroll.contentView();
            clip.scrollToPoint(NSPoint::new(0.0, if *flipped { 0.0 } else { max_y as f64 }));
            scroll.reflectScrolledClipView(&clip);
            scroll.display();
        });
    });
    std::thread::sleep(Duration::from_millis(150));
    println!(
        "reference-grid and precise logical frozen, still and stream mappings match; desktop and window selections and recordings retain logical bounds; fractional cursors match in flattened, editable and recording mappings; fractional overlay colors sample native pixels; control window capture and native overlay pixels remain frozen after scrolling (grab {:.0} ms, render {:.0} ms)",
        grabbed.as_secs_f64() * 1000.0,
        rendered.as_secs_f64() * 1000.0
    );
    Ok(())
}

fn fixture_images(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut images = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            for item in std::fs::read_dir(entry.path())? {
                let path = item?.path();
                if path.extension().is_some_and(|ext| ext == "png") {
                    images.push(path);
                }
            }
        }
    }
    Ok(images)
}

fn same_rgb(a: &Frame, b: &Frame) -> bool {
    a.width == b.width && a.height == b.height
        && a.pixels.chunks_exact(4).zip(b.pixels.chunks_exact(4)).all(|(a, b)| a[..3] == b[..3])
}

fn save_fixture_capture(root: &Path, capture: crate::workflow::Capture, settings: &crate::settings::Settings) -> io::Result<()> {
    let before = fixture_images(root)?;
    let expected = capture.frame.clone();
    let rect = capture.meta.rect;
    let outcome = crate::workflow::finish(capture, settings, 0);
    if outcome.is_error {
        return Err(io::Error::other(outcome.message));
    }
    let added: Vec<_> = fixture_images(root)?.into_iter().filter(|path| !before.contains(path)).collect();
    if added.len() != 1 {
        return Err(io::Error::other("the saved-area fixture did not publish exactly one capture"));
    }
    let meta = jiffshot_capture::library::read_meta(&added[0]).ok_or_else(|| io::Error::other("the saved-area capture has no metadata"))?;
    if meta.kind != jiffshot_capture::library::CaptureKind::Region || meta.rect != rect
        || (meta.width, meta.height) != (expected.width, expected.height)
    {
        return Err(io::Error::other("the saved-area capture metadata does not match the captured rectangle"));
    }
    if !same_rgb(&Frame::load_png(&added[0])?, &expected) {
        return Err(io::Error::other("the published saved-area PNG differs from the captured pixels"));
    }
    Ok(())
}

fn check_saved_area_captures(rect: Rect) -> io::Result<()> {
    use crate::geometry::{self, CaptureLayout, SavedAreaKind};

    for name in ["JIFFSHOT_LIBRARY", "JIFFSHOT_CONFIG_DIR", "JIFFSHOT_LOCAL_DIR"] {
        if std::env::var_os(name).is_none_or(|value| value.is_empty()) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{name} must name an isolated test directory")));
        }
    }
    let root = PathBuf::from(std::env::var_os("JIFFSHOT_LIBRARY").expect("the library override was checked"));
    std::fs::create_dir_all(&root)?;
    if std::fs::read_dir(&root)?.next().transpose()?.is_some() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "the saved-area fixture requires an empty library"));
    }
    let layout = CaptureLayout::current()?;
    let (width, height) = layout.mapping(rect)?.size();
    let fixed = Rect::new(
        rect.x,
        rect.y,
        i32::try_from(width).map_err(|_| io::Error::other("the fixture width is too large"))?,
        i32::try_from(height).map_err(|_| io::Error::other("the fixture height is too large"))?,
    );
    let mut settings = crate::settings::Settings::default();
    settings.open_in_editor = false;
    settings.copy_to_clipboard = false;
    settings.include_cursor = false;
    settings.share = "none".into();
    settings.capture_effects.clear();
    settings.capture_window.fixed_at = Some((fixed.x, fixed.y));
    settings.fixed_size = (fixed.width, fixed.height);
    settings.schedule_area = Some((rect.x, rect.y, rect.width, rect.height));

    let result = (|| {
        let (fixed_area, fixed_layout) = layout.fixed_at(fixed.x, fixed.y, width, height)?;
        let fixed_reference = fixed_layout.grab(fixed_area)?;
        let (r, g, b) = fixed_reference.rgb((width / 2) as i32, (height / 20) as i32)
            .ok_or_else(|| io::Error::other("the synthetic fixture has no header pixel"))?;
        if b < 100 || b <= r.saturating_add(30) || b <= g.saturating_add(30) {
            return Err(io::Error::other(format!("the synthetic fixture is not visible at the capture location: ({r}, {g}, {b})")));
        }
        geometry::confirm_saved_area_in_layout(SavedAreaKind::Fixed, fixed, layout.clone())?;
        let fixed_ticket = geometry::saved_area_ticket(SavedAreaKind::Fixed, fixed)?;
        let capture = crate::tray::capture_fixed_rect(&settings, &fixed_ticket)?;
        if capture.meta.rect != fixed_area || !same_rgb(&capture.frame, &fixed_reference) {
            return Err(io::Error::other("the fixed capture differs from the synthetic fixture"));
        }
        save_fixture_capture(&root, capture, &settings)?;
        geometry::confirm_saved_area_in_layout(SavedAreaKind::Fixed, fixed, layout.clone())?;
        if crate::tray::capture_fixed_rect(&settings, &fixed_ticket).is_ok() {
            return Err(io::Error::other("a replaced fixed confirmation still allowed capture"));
        }

        geometry::confirm_saved_area_in_layout(SavedAreaKind::Scheduled, rect, layout.clone())?;
        let scheduled_ticket = geometry::saved_area_ticket(SavedAreaKind::Scheduled, rect)?;
        let scheduled_reference = layout.grab(rect)?;
        let capture = crate::tray::scheduled_capture(&settings, Some(&scheduled_ticket))?;
        if capture.meta.rect != rect || !same_rgb(&capture.frame, &scheduled_reference) {
            return Err(io::Error::other("the scheduled capture differs from the synthetic fixture"));
        }
        save_fixture_capture(&root, capture, &settings)?;
        geometry::confirm_saved_area_in_layout(SavedAreaKind::Scheduled, rect, layout.clone())?;
        if crate::tray::scheduled_capture(&settings, Some(&scheduled_ticket)).is_ok() {
            return Err(io::Error::other("a replaced scheduled confirmation still allowed capture"));
        }
        println!("fixed and scheduled captures match the synthetic window, PNGs and metadata; stale confirmations are rejected");
        Ok(())
    })();
    let clear_fixed = geometry::clear_saved_area(SavedAreaKind::Fixed);
    let clear_scheduled = geometry::clear_saved_area(SavedAreaKind::Scheduled);
    if let Err(error) = &clear_fixed {
        eprintln!("could not clear the fixed-area fixture: {error}");
    }
    if let Err(error) = &clear_scheduled {
        eprintln!("could not clear the scheduled-area fixture: {error}");
    }
    result?;
    clear_fixed?;
    clear_scheduled
}

pub fn run(dir: &Path) -> io::Result<()> {
    let mode = ScrollMode::from_env()?;
    let frozen_only = std::env::var_os("JIFFSHOT_FROZEN_ONLY").is_some();
    let saved_areas_only = std::env::var_os("JIFFSHOT_SAVED_AREAS_CAPTURE_ONLY").is_some();
    let (page_width, page_height) = mode.page_size();
    std::fs::create_dir_all(dir)?;
    let mtm = MainThreadMarker::new().ok_or_else(|| io::Error::other("the scrolling fixture must run on the main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    if app.activationPolicy() != NSApplicationActivationPolicy::Regular && !app.setActivationPolicy(NSApplicationActivationPolicy::Regular) {
        return Err(io::Error::other("could not activate the scrolling fixture"));
    }
    screen::refresh_work_areas();
    check_appkit_backing(mtm)?;
    let monitor = screen::monitors().into_iter().find(|monitor| monitor.primary).ok_or_else(|| io::Error::other("no primary display"))?;
    let work = screen::rect_to_points(monitor.work);
    if work.size.width < SCROLL_WIDTH as f64 + 80.0 || work.size.height < SCROLL_HEIGHT as f64 + 80.0 {
        return Err(io::Error::other("the primary display is too small for the scrolling fixture"));
    }
    let (x, y) = (work.origin.x + 40.0, work.origin.y + 40.0);
    let (width, height) = (SCROLL_WIDTH as f64, SCROLL_HEIGHT as f64);
    let frame = NSRect::new(NSPoint::new(x, screen::main_height_points() - y - height), NSSize::new(width, height));
    // SAFETY: the window and its views are created and used on the AppKit main thread.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: the retained window remains owned until it is closed below.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setOpaque(true);
    window.setBackgroundColor(Some(&NSColor::blueColor()));
    window.setTitle(&NSString::from_str("Jiffshot Scrolling Fixture"));
    window.setLevel(NSStatusWindowLevel);
    let image_data = NSData::with_bytes(&scroll_page(page_width, page_height).encode_png_vec()?);
    let image = NSImage::initWithData(NSImage::alloc(), &image_data).ok_or_else(|| io::Error::other("could not create the scrolling page"))?;
    image.setSize(NSSize::new(page_width as f64, page_height as f64));
    let page_view = NSImageView::initWithFrame(
        NSImageView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(page_width as f64, page_height as f64)),
    );
    page_view.setImage(Some(&image));
    page_view.setImageScaling(NSImageScaling::ScaleAxesIndependently);
    let viewport_height = VIEWPORT_HEIGHT as f64;
    let scroll = NSScrollView::initWithFrame(
        NSScrollView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, viewport_height)),
    );
    scroll.setHasVerticalScroller(false);
    scroll.setHasHorizontalScroller(false);
    scroll.setDocumentView(Some(&page_view));
    let parent = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height)));
    parent.addSubview(&scroll);
    window.setContentView(Some(&parent));
    let clip = scroll.contentView();
    let flipped = page_view.isFlipped();
    let top = if flipped { 0.0 } else { page_height as f64 - viewport_height };
    clip.scrollToPoint(NSPoint::new(0.0, top));
    scroll.reflectScrolledClipView(&clip);
    SCROLL_FIXTURE.with(|fixture| *fixture.borrow_mut() = Some((scroll, flipped)));
    window.orderFrontRegardless();
    let window_id = window.windowNumber();
    if window_id <= 0 {
        return Err(io::Error::other("the scrolling fixture has no window number"));
    }
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let points = objc2_core_foundation::CGRect {
        origin: objc2_core_foundation::CGPoint { x, y },
        size: objc2_core_foundation::CGSize { width, height },
    };
    let rect = screen::rect_from_points(points);
    let (capture_width, capture_height) = screen::DisplayLayout::current()?.mapping(rect)?.size();
    let scale = (capture_width as f64 / SCROLL_WIDTH as f64, capture_height as f64 / SCROLL_HEIGHT as f64);
    let header = (SCROLL_HEADER as f64 * scale.1).round() as usize;
    let expected_width = (page_width as f64 * scale.0).round() as u32;
    let expected_height = ((page_height + SCROLL_HEADER) as f64 * scale.1).round() as u32;
    let path = dir.join(mode.filename());
    let deadline_path = dir.join("deadline.png");
    let output_dir = std::path::absolute(dir)?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let outcome = std::panic::catch_unwind(|| -> io::Result<()> {
            std::thread::sleep(Duration::from_millis(500));
            if saved_areas_only {
                return check_saved_area_captures(rect);
            }
            check_frozen_fixture(rect, page_width, page_height, window_id, &output_dir)?;
            if frozen_only {
                return Ok(());
            }
            let started = Instant::now();
            let out = if std::env::var_os("JIFFSHOT_SCROLL_CONTROL_SELFTEST").is_some() {
                let horizontal = match mode {
                    ScrollMode::Vertical => false,
                    ScrollMode::Horizontal => true,
                    ScrollMode::Both => return Err(io::Error::other("the control scrolling fixture needs one axis")),
                };
                let original = screen::try_cursor_pos()?;
                let deadline = crate::control_macos::selftest_request("capture.scroll", serde_json::json!({
                    "window": window_id,
                    "no_activate": true,
                    "horizontal": horizontal,
                    "timeout_ms": 20,
                    "settle_ms": 260,
                    "out": deadline_path,
                })).map_err(io::Error::other)?;
                if deadline["stopped"] != "deadline" || deadline["frames"] != 1 || deadline["limited"] != true
                    || screen::try_cursor_pos()? != original
                {
                    return Err(io::Error::other(format!("the control scrolling deadline was not respected: {deadline}")));
                }
                let offset = Rect::new(rect.x + 1, rect.y + 1, rect.width - 4, rect.height - 4);
                let fractional = crate::control_macos::selftest_request("capture.scroll", serde_json::json!({
                    "region": format!("{},{},{},{}", offset.x, offset.y, offset.width, offset.height),
                    "timeout_ms": 20,
                    "settle_ms": 260,
                    "out": deadline_path,
                })).map_err(io::Error::other)?;
                if fractional["stopped"] != "deadline" || screen::try_cursor_pos()? != original {
                    return Err(io::Error::other(format!("the fractional pointer target was not restored: {fractional}")));
                }
                let limited = crate::control_macos::selftest_request("capture.scroll", serde_json::json!({
                    "window": window_id,
                    "no_activate": true,
                    "horizontal": horizontal,
                    "max_frames": 2,
                    "timeout_ms": 10_000,
                    "settle_ms": 260,
                    "notches": 3,
                })).map_err(io::Error::other)?;
                if limited["stopped"] != "frame_limit" || limited["frames"] != 2 || limited["limited"] != true
                    || limited["target"]["window"]["hwnd"] != window_id || screen::try_cursor_pos()? != original
                {
                    return Err(io::Error::other(format!("the control scrolling frame limit was not respected: {limited}")));
                }
                let limited_path = limited["path"].as_str().ok_or_else(|| io::Error::other("the limited capture has no path"))?;
                let limited_frame = Frame::load_png(Path::new(limited_path))?;
                let limited_meta = jiffshot_capture::library::read_meta_checked(Path::new(limited_path))?
                    .ok_or_else(|| io::Error::other("the limited capture has no metadata"))?;
                let partial = if horizontal {
                    limited_frame.width > capture_width && limited_frame.width < expected_width
                } else {
                    limited_frame.height > capture_height && limited_frame.height < expected_height
                };
                if !partial || limited_meta.kind != jiffshot_capture::library::CaptureKind::Scrolling
                    || limited_meta.rect != rect || limited_meta.width != limited_frame.width
                    || limited_meta.height != limited_frame.height
                    || limited_meta.window_title.as_deref() != Some("Jiffshot Scrolling Fixture")
                    || limited_meta.process.as_deref().is_none_or(str::is_empty)
                {
                    return Err(io::Error::other(format!("the limited capture or its metadata is incorrect: {limited}; {limited_meta:?}")));
                }
                dispatch2::DispatchQueue::main().exec_sync(move || {
                    SCROLL_FIXTURE.with(|fixture| {
                        let fixture = fixture.borrow();
                        let (scroll, flipped) = fixture.as_ref().expect("scrolling fixture remains open");
                        let clip = scroll.contentView();
                        let top = if *flipped { 0.0 } else { (page_height - VIEWPORT_HEIGHT) as f64 };
                        clip.scrollToPoint(NSPoint::new(0.0, top));
                        scroll.reflectScrolledClipView(&clip);
                        scroll.display();
                    });
                });
                std::thread::sleep(Duration::from_millis(100));
                let result = crate::control_macos::selftest_request("capture.scroll", serde_json::json!({
                    "window": window_id,
                    "no_activate": true,
                    "out": path,
                    "horizontal": horizontal,
                    "max_frames": 128,
                    "timeout_ms": 90_000,
                    "settle_ms": 260,
                    "notches": 3,
                })).map_err(io::Error::other)?;
                if screen::try_cursor_pos()? != original {
                    return Err(io::Error::other("the control scrolling capture did not restore the pointer"));
                }
                if result["stopped"] != "no_movement" || result["frames"].as_u64().unwrap_or(0) < 2
                    || result["limited"] != false || result["target"]["window"]["hwnd"] != window_id
                {
                    return Err(io::Error::other(format!("the control scrolling capture did not reach the end: {result}")));
                }
                Frame::load_png(&path)?
            } else {
                match mode {
                    ScrollMode::Vertical => crate::modes::scrolling(rect)?,
                    ScrollMode::Horizontal => crate::modes::scrolling_horizontal(rect)?,
                    ScrollMode::Both => crate::modes::scrolling_both(rect)?,
                }
            };
            check_scroll_dimensions(&out, expected_width, expected_height)?;
            check_scroll_page(&out, rect, header, scale, page_width, page_height)?;
            out.save_png(&path)?;
            println!(
                "scrolling capture {}x{} in {:.1} s; the entire page matches",
                out.width,
                out.height,
                started.elapsed().as_secs_f64()
            );
            println!("wrote {}", path.display());
            Ok(())
        })
        .unwrap_or_else(|_| Err(io::Error::other("the scrolling self-test panicked")));
        let _ = tx.send(outcome);
        dispatch2::DispatchQueue::main().exec_async(stop_app);
    });
    app.run();
    SCROLL_FIXTURE.with(|fixture| *fixture.borrow_mut() = None);
    window.close();
    rx.recv_timeout(Duration::from_secs(5)).map_err(|_| io::Error::other("the scrolling self-test did not finish"))?
}

struct MenuProcess {
    child: Child,
    reaped: bool,
}

impl Drop for MenuProcess {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub fn menus(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let child = Command::new(std::env::current_exe()?)
        .arg("--internal-menu-fixture")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut child = MenuProcess { child, reaped: false };
    let stdout = child.child.stdout.take().ok_or_else(|| io::Error::other("the menu fixture has no output"))?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = io::BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = tx.send(result);
    });
    let ready = rx.recv_timeout(Duration::from_secs(10)).map_err(|_| io::Error::other("the menu fixture did not start"))??;
    if ready.trim() != "READY" {
        return Err(io::Error::other("the menu fixture exited before opening"));
    }
    std::thread::sleep(Duration::from_millis(700));
    let found = crate::modes::open_menus();
    if !found.iter().any(|rect| {
        let (x, y) = rect.center();
        jiffshot_capture::targets::window_owner_at(x, y) == Some(child.child.id())
    }) {
        return Err(io::Error::other(format!("the native fixture menu was not found; detected {found:?}")));
    }
    let frame = crate::modes::menus()?.ok_or_else(|| io::Error::other("the menu capture returned no image"))?;
    if !frame.alpha || !frame.pixels.chunks_exact(4).any(|pixel| pixel[3] != 0) {
        return Err(io::Error::other("the menu capture is empty or missing transparency"));
    }
    let path = dir.join("menu.png");
    frame.save_png(&path)?;
    let input = child.child.stdin.as_mut().ok_or_else(|| io::Error::other("the menu fixture has no input"))?;
    writeln!(input, "close")?;
    input.flush()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.child.try_wait()? {
            child.reaped = true;
            if !status.success() {
                return Err(io::Error::other(format!("the menu fixture exited with {status}")));
            }
            break;
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("the menu fixture did not close"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("captured a native menu at {}x{} with alpha", frame.width, frame.height);
    println!("wrote {}", path.display());
    Ok(())
}

pub fn menu_fixture() -> io::Result<()> {
    let mtm = MainThreadMarker::new().ok_or_else(|| io::Error::other("the menu fixture must run on the main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    if app.activationPolicy() != NSApplicationActivationPolicy::Regular && !app.setActivationPolicy(NSApplicationActivationPolicy::Regular) {
        return Err(io::Error::other("could not activate the menu fixture"));
    }
    screen::refresh_work_areas();
    let work = screen::monitors()
        .into_iter()
        .find(|monitor| monitor.primary)
        .map(|monitor| screen::rect_to_points(monitor.work))
        .ok_or_else(|| io::Error::other("no primary display"))?;
    let (width, height) = (320.0, 240.0);
    let (x, y) = (work.origin.x + 40.0, work.origin.y + 40.0);
    let frame = NSRect::new(NSPoint::new(x, screen::main_height_points() - y - height), NSSize::new(width, height));
    // SAFETY: the window and menu are created and used on the AppKit main thread.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: the retained window remains owned until it is closed below.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setBackgroundColor(Some(&NSColor::blueColor()));
    window.setLevel(NSStatusWindowLevel);
    window.orderFrontRegardless();
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let view = window.contentView().ok_or_else(|| io::Error::other("the menu fixture has no view"))?;
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Jiffshot menu test"));
    menu.setAutoenablesItems(false);
    for label in ["New capture", "Scrolling capture", "Panoramic capture", "Open library"] {
        // SAFETY: no action selector is installed for these inert fixture items.
        unsafe { menu.addItemWithTitle_action_keyEquivalent(&NSString::from_str(label), None, &NSString::from_str("")) };
    }
    MENU_FIXTURE.with(|fixture| *fixture.borrow_mut() = Some(menu.clone()));
    std::thread::spawn(|| {
        let mut line = String::new();
        let _ = io::stdin().lock().read_line(&mut line);
        dispatch2::DispatchQueue::main().exec_async(|| {
            MENU_FIXTURE.with(|fixture| {
                if let Some(menu) = fixture.borrow().as_ref() {
                    menu.cancelTracking();
                }
            });
        });
    });
    println!("READY");
    io::stdout().flush()?;
    menu.popUpMenuPositioningItem_atLocation_inView(None, NSPoint::new(40.0, height - 40.0), Some(&view));
    MENU_FIXTURE.with(|fixture| *fixture.borrow_mut() = None);
    window.close();
    Ok(())
}

pub fn hotkeys() -> io::Result<()> {
    use crate::sys;

    let mtm = MainThreadMarker::new().ok_or_else(|| io::Error::other("the hotkey self-test must run on the main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let outcome = std::panic::catch_unwind(|| -> io::Result<()> {
            const VK_F19: u32 = 0x82;
            const MODIFIERS: u32 = sys::MOD_CONTROL | sys::MOD_ALT | sys::MOD_SHIFT | sys::MOD_WIN;
            struct Registered(i32);
            impl Drop for Registered {
                fn drop(&mut self) {
                    // SAFETY: this guard is dropped on the thread that registered the key.
                    unsafe { sys::UnregisterHotKey(std::ptr::null_mut(), self.0) };
                }
            }
            let register = |id| {
                // SAFETY: null registers the hotkey to this thread's message queue.
                if unsafe { sys::RegisterHotKey(std::ptr::null_mut(), id, MODIFIERS, VK_F19) } == 0 {
                    return Err(io::Error::other("could not register the test hotkey"));
                }
                Ok(Registered(id))
            };
            let trigger = |id| -> io::Result<()> {
                sys::deliver_selftest_hotkey(id)?;
                let deadline = Instant::now() + Duration::from_secs(3);
                while Instant::now() < deadline {
                    // SAFETY: the message pointer is valid and this thread owns the queue.
                    let mut msg: sys::MSG = unsafe { std::mem::zeroed() };
                    if unsafe { sys::PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, sys::PM_REMOVE) } != 0 && msg.message == sys::WM_HOTKEY {
                        if msg.wParam != id as usize || msg.lParam != ((VK_F19 as isize) << 16 | MODIFIERS as isize) {
                            return Err(io::Error::other("the hotkey was delivered with an incorrect id or key combination"));
                        }
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(io::Error::other("the registered hotkey did not reach its message queue"))
            };

            let key = register(481)?;
            // SAFETY: this only tries duplicate registrations on the current thread.
            unsafe {
                if sys::RegisterHotKey(std::ptr::null_mut(), 482, MODIFIERS, VK_F19) != 0 {
                    sys::UnregisterHotKey(std::ptr::null_mut(), 482);
                    return Err(io::Error::other("a duplicate hotkey was accepted"));
                }
                if sys::RegisterHotKey(std::ptr::null_mut(), 481, MODIFIERS, 0x83) != 0 {
                    sys::UnregisterHotKey(std::ptr::null_mut(), 481);
                    return Err(io::Error::other("a duplicate hotkey id was accepted"));
                }
            }
            trigger(481)?;
            drop(key);
            let _key = register(482)?;
            trigger(482)?;
            println!("Carbon registration, collision handling, and in-process delivery passed; physical-key delivery is not tested");
            Ok(())
        })
        .unwrap_or_else(|_| Err(io::Error::other("the hotkey self-test panicked")));
        let _ = tx.send(outcome);
        dispatch2::DispatchQueue::main().exec_async(stop_app);
    });
    app.run();
    rx.recv_timeout(Duration::from_secs(5)).map_err(|_| io::Error::other("the hotkey self-test did not finish"))?
}

pub(crate) fn stop_app() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    app.stop(None);
    if let Some(event) = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
        NSEventType::ApplicationDefined,
        NSPoint::new(0.0, 0.0),
        NSEventModifierFlags::empty(),
        0.0,
        0,
        None,
        0,
        0,
        0,
    ) {
        app.postEvent_atStart(&event, false);
    }
}

fn fixture_color(blue: bool) {
    FIXTURE.with(|fixture| {
        if let Some(window) = fixture.borrow().as_ref() {
            let color = if blue { NSColor::blueColor() } else { NSColor::redColor() };
            window.setBackgroundColor(Some(&color));
            window.display();
        }
    });
}

fn require_system_audio_signal() -> Result<(), String> {
    let capture = media::capture_audio(false)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut peak = 0i32;
    while Instant::now() < deadline {
        if let Ok(block) = capture.samples.recv_timeout(Duration::from_millis(100)) {
            peak = peak.max(block.samples.iter().map(|&sample| (sample as i32).abs()).max().unwrap_or(0));
            if peak > 0 {
                println!("system audio input peak {peak}");
                return Ok(());
            }
        }
        if let Some(error) = capture.failure() {
            return Err(format!("system audio stopped: {error}"));
        }
    }
    Err("system audio input had no non-silent samples".into())
}

struct AudioWindow {
    start: u64,
    end: u64,
    energy: u128,
    samples: u64,
}

impl AudioWindow {
    fn new(start_ms: u64, end_ms: u64, rate: u32) -> Result<AudioWindow, String> {
        if start_ms >= end_ms {
            return Err("the recording was too short to check an audio toggle".into());
        }
        Ok(AudioWindow { start: start_ms * u64::from(rate) / 1_000, end: end_ms * u64::from(rate) / 1_000, energy: 0, samples: 0 })
    }

    fn observe(&mut self, first: u64, pcm: &[i16]) {
        for (index, frame) in pcm.chunks_exact(2).enumerate() {
            let at = first + index as u64;
            if (self.start..self.end).contains(&at) {
                for sample in frame {
                    self.energy += u128::from(sample.unsigned_abs()).pow(2);
                    self.samples += 1;
                }
            }
        }
    }

    fn rms(&self) -> Result<f64, String> {
        if self.samples == 0 {
            return Err("the recorded audio did not cover a toggle-check interval".into());
        }
        Ok((self.energy as f64 / self.samples as f64).sqrt())
    }
}

fn check_live_exclusions(rect: Rect) -> Result<(), String> {
    let layout = screen::DisplayLayout::current().map_err(|error| error.to_string())?;
    let mapping = layout.mapping(rect).map_err(|error| error.to_string())?;
    let (width, height) = mapping.size();
    let mut grabber = screen::Grabber::for_mapping(mapping, &layout).map_err(|error| error.to_string())?;
    let center = ((height as usize / 2 * width as usize) + width as usize / 2) * 4;
    let wait_for = |grabber: &mut screen::Grabber, green: bool| -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last = [0; 3];
        while Instant::now() < deadline {
            let frame = grabber.grab().map_err(|error| error.to_string())?;
            let pixel = frame.get(center..center + 3).ok_or("the exclusion fixture is outside the recording")?;
            last.copy_from_slice(pixel);
            let matches = if green {
                pixel[1] > 160 && u16::from(pixel[1]) > u16::from(pixel[0].max(pixel[2])) + 100
            } else {
                pixel[2] > 160 && u16::from(pixel[2]) > u16::from(pixel[0].max(pixel[1])) + 100
            };
            if matches {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(format!("{}; center BGRA was {last:?}",
            if green { "the recording did not show the synthetic overlay" } else { "the recording did not exclude the synthetic overlay" }))
    };
    wait_for(&mut grabber, false)?;
    let (tx, rx) = mpsc::channel();
    dispatch2::DispatchQueue::main().exec_sync(move || {
        let result = (|| -> Result<u32, String> {
            let marker = MainThreadMarker::new().ok_or("the exclusion fixture must run on the main thread")?;
            let frame = FIXTURE.with(|fixture| fixture.borrow().as_ref().map(|window| window.frame())).ok_or("the recording fixture is unavailable")?;
            // SAFETY: this fixture is created, retained and closed on the AppKit main thread.
            let window = unsafe { NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(marker), frame, NSWindowStyleMask::Borderless, NSBackingStoreType::Buffered, false) };
            unsafe { window.setReleasedWhenClosed(false) };
            window.setOpaque(true);
            window.setBackgroundColor(Some(&NSColor::greenColor()));
            window.setLevel(NSStatusWindowLevel + 1);
            window.setAlphaValue(0.0);
            window.orderFrontRegardless();
            let number = u32::try_from(window.windowNumber()).map_err(|_| "the exclusion fixture has no window number")?;
            EXCLUSION_FIXTURE.with(|slot| *slot.borrow_mut() = Some(window));
            Ok(number)
        })();
        let _ = tx.send(result);
    });
    let number = rx.recv().map_err(|_| "the exclusion fixture did not start")??;
    struct CloseFixture(u32);
    impl Drop for CloseFixture {
        fn drop(&mut self) {
            screen::exclude_window(self.0, false);
            dispatch2::DispatchQueue::main().exec_sync(|| {
                EXCLUSION_FIXTURE.with(|slot| {
                    if let Some(window) = slot.borrow_mut().take() {
                        window.close();
                    }
                });
            });
        }
    }
    let _fixture = CloseFixture(number);
    screen::exclude_window(number, true);
    if screen::exclusion_ready(number) {
        return Err("an active recording acknowledged the exclusion before updating its filter".into());
    }
    grabber.refresh_exclusions().map_err(|error| error.to_string())?;
    if !screen::exclusion_ready(number) {
        return Err("the recording did not acknowledge the hidden window exclusion".into());
    }
    dispatch2::DispatchQueue::main().exec_sync(|| {
        EXCLUSION_FIXTURE.with(|slot| {
            if let Some(window) = slot.borrow().as_ref() {
                window.setAlphaValue(1.0);
            }
        });
    });
    for _ in 0..10 {
        let frame = grabber.grab().map_err(|error| error.to_string())?;
        let pixel = frame.get(center..center + 3).ok_or("the exclusion fixture is outside the recording")?;
        if !(pixel[2] > 160 && u16::from(pixel[2]) > u16::from(pixel[0].max(pixel[1])) + 100) {
            return Err(format!("an excluded window appeared in a recording frame: {pixel:?}"));
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    screen::exclude_window(number, false);
    wait_for(&mut grabber, true)?;
    screen::exclude_window(number, true);
    wait_for(&mut grabber, false)?;
    screen::exclude_window(number, false);
    wait_for(&mut grabber, true)?;
    dispatch2::DispatchQueue::main().exec_sync(|| {
        EXCLUSION_FIXTURE.with(|slot| {
            if let Some(window) = slot.borrow().as_ref() {
                window.setAlphaValue(0.0);
            }
        });
    });
    wait_for(&mut grabber, false)?;
    screen::exclude_window(number, true);
    wait_for(&mut grabber, false)?;
    screen::exclude_window(number, false);
    wait_for(&mut grabber, false)?;
    println!("live recording exclusion added and removed successfully");
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PointerPixels {
    highlight: bool,
    cursor: bool,
    click: bool,
}

impl PointerPixels {
    fn observe(&mut self, frame: &media::VideoFrame, (x, y): (u32, u32)) -> Result<(), String> {
        if x < 50 || y < 38 || x + 12 >= frame.width || y + 38 >= frame.height
            || frame.bgra.len() != frame.width as usize * frame.height as usize * 4
        {
            return Err("invalid decoded pointer fixture dimensions".into());
        }
        let pixel = |x: u32, y: u32| {
            let i = (y as usize * frame.width as usize + x as usize) * 4;
            [frame.bgra[i], frame.bgra[i + 1], frame.bgra[i + 2]]
        };
        let halo = pixel(x - 5, y);
        let background = pixel(x - 50, y);
        self.highlight |= u16::from(halo[1]) > u16::from(background[1]) + 45;
        let cursor = pixel(x + 5, y + 5);
        self.cursor |= cursor[1] > 140 && cursor[0] < 80 && cursor[2] < 80;
        // Stay inside the highlight so a blue background cannot look like a blue ring.
        for dy in -22i32..=22 {
            for dx in -22i32..=-8 {
                let distance = dx * dx + dy * dy;
                if !(64..=484).contains(&distance) {
                    continue;
                }
                let ring = pixel((x as i32 + dx) as u32, (y as i32 + dy) as u32);
                self.click |= u16::from(ring[1]) > u16::from(halo[1]) + 15
                    && (ring[0].abs_diff(halo[0]) > 25 || ring[2].abs_diff(halo[2]) > 25);
            }
        }
        Ok(())
    }

    fn verify(&self, expected: &Self) -> Result<(), String> {
        if self != expected {
            return Err(format!("decoded synthetic pointer effects differ: expected {expected:?}, observed {self:?}"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SyntheticPointerMode {
    All,
    NoHighlight,
    NoCursor,
    NoClick,
}

impl SyntheticPointerMode {
    fn from_env() -> Result<Option<Self>, String> {
        match std::env::var("JIFFSHOT_RECORD_SYNTHETIC_POINTER") {
            Err(std::env::VarError::NotPresent) => Ok(None),
            Ok(value) => match value.as_str() {
                "1" | "all" => Ok(Some(Self::All)),
                "no-highlight" => Ok(Some(Self::NoHighlight)),
                "no-cursor" => Ok(Some(Self::NoCursor)),
                "no-click" => Ok(Some(Self::NoClick)),
                _ => Err("JIFFSHOT_RECORD_SYNTHETIC_POINTER must be all, no-highlight, no-cursor or no-click".into()),
            },
            Err(error) => Err(format!("invalid JIFFSHOT_RECORD_SYNTHETIC_POINTER: {error}")),
        }
    }

    fn expected(self) -> PointerPixels {
        PointerPixels {
            highlight: self != Self::NoHighlight,
            cursor: self != Self::NoCursor,
            click: self != Self::NoClick,
        }
    }
}

fn record_fixture(path: &Path, rect: Rect, seconds: f32) -> Result<(), String> {
    let audio = std::env::var("JIFFSHOT_RECORD_AUDIO").unwrap_or_else(|_| "system".into());
    if !["system", "microphone", "both", "none"].contains(&audio.as_str()) {
        return Err(format!("unknown recording audio source: {audio}"));
    }
    let require_signal = std::env::var_os("JIFFSHOT_TEST_REQUIRE_AUDIO_SIGNAL").is_some();
    let switch = std::env::var_os("JIFFSHOT_RECORD_SWITCH").is_some();
    let verify_switch = std::env::var_os("JIFFSHOT_RECORD_VERIFY_SWITCH").is_some();
    let verify_pause = std::env::var_os("JIFFSHOT_RECORD_VERIFY_PAUSE").is_some();
    if verify_switch && (!switch || !require_signal || audio != "system") {
        return Err("audio toggle verification requires system audio, JIFFSHOT_RECORD_SWITCH and JIFFSHOT_TEST_REQUIRE_AUDIO_SIGNAL".into());
    }
    if verify_pause && (switch || !require_signal || audio != "system") {
        return Err("pause verification requires system audio and JIFFSHOT_TEST_REQUIRE_AUDIO_SIGNAL, without source switching".into());
    }
    if require_signal && matches!(audio.as_str(), "system" | "both") {
        require_system_audio_signal()?;
    }
    if std::env::var_os("JIFFSHOT_TEST_LIVE_EXCLUSIONS").is_some() {
        check_live_exclusions(rect)?;
    }
    let synthetic_pointer = SyntheticPointerMode::from_env()?;
    let draw = std::env::var_os("JIFFSHOT_RECORD_DRAW").is_some();
    if synthetic_pointer.is_some() && (switch || draw || std::env::var_os("JIFFSHOT_RECORD_FX").is_some()) {
        return Err("synthetic pointer verification cannot be combined with source switching, Screen Draw or webcam effects".into());
    }
    let mut options = crate::recorder::Options::audio_only(30, &audio);
    let pointer_fx = synthetic_pointer.is_some() || std::env::var_os("JIFFSHOT_RECORD_POINTER_FX").is_some();
    if pointer_fx {
        options.cursor_highlight = true;
        options.click_rings = true;
    }
    if std::env::var_os("JIFFSHOT_RECORD_FX").is_some() {
        options.cursor_highlight = true;
        options.click_rings = true;
        options.noise_gate = true;
        options.webcam = Some("bottom-right".into());
    }
    if let Some(mode) = synthetic_pointer {
        let expected = mode.expected();
        options.cursor_highlight = expected.highlight;
        options.cursor = expected.cursor;
        options.click_rings = expected.click;
    }
    let mut layout = crate::geometry::CaptureLayout::current().map_err(|error| error.to_string())?;
    if let Some(size) = std::env::var_os("JIFFSHOT_RECORD_OUTPUT_SIZE") {
        let size = size.to_str().ok_or("JIFFSHOT_RECORD_OUTPUT_SIZE is not valid UTF-8")?;
        let (width, height) = size.split_once('x').ok_or("JIFFSHOT_RECORD_OUTPUT_SIZE must be WIDTHxHEIGHT")?;
        let width: u32 = width.parse().map_err(|_| "invalid recording test width")?;
        let height: u32 = height.parse().map_err(|_| "invalid recording test height")?;
        if !(2..=8192).contains(&width) || !(2..=8192).contains(&height) {
            return Err("recording test dimensions must be between 2 and 8192".into());
        }
        layout = layout.with_output_size(rect, width, height).map_err(|error| error.to_string())?;
    }
    let (mapping, _) = crate::recorder::recording_geometry(rect, options.fps, &layout)?;
    let (width, height) = mapping.size();
    let position = (width / 3, height / 3);
    let fixture = if synthetic_pointer.is_some() {
        if width < 180 || height < 120 {
            return Err("synthetic pointer verification requires an output of at least 180x120".into());
        }
        let [left, top, right, bottom] = mapping.desktop_bounds();
        let step = ((right - left) / width as f64, (bottom - top) / height as f64);
        let point = (
            left + (position.0 as f64 + 0.125) * step.0,
            top + (position.1 as f64 + 0.125) * step.1,
        );
        let cursor = screen::CursorSnapshot::from_image_with_bounds(
            Frame { width: 12, height: 12, pixels: vec![0, 255, 0, 255].repeat(144), alpha: true },
            [point.0, point.1, point.0 + 12.0 * step.0, point.1 + 12.0 * step.1],
        ).map_err(|error| error.to_string())?;
        Some(crate::recorder::SelftestPointer { position: point, cursor })
    } else {
        None
    };
    if draw {
        if !crate::screendraw::show(mapping, Duration::ZERO) {
            return Err("another Screen Draw session is active".into());
        }
        if let Some(state) = crate::screendraw::state().as_mut() {
            let (width, height) = mapping.size();
            state.key(b'S' as u16);
            state.press(jiffshot_doc::Pt::new((width / 4) as f32, (height / 4) as f32));
            state.moved(jiffshot_doc::Pt::new((width * 3 / 4) as f32, (height * 3 / 4) as f32));
            state.release();
        }
    }
    let control = Arc::new(crate::recorder::Control::new(&options));
    let worker_control = control.clone();
    let path_buf = path.to_path_buf();
    let worker = std::thread::spawn(move || {
        if let Some(pointer) = fixture {
            crate::recorder::record_with_selftest_pointer(&path_buf, rect, &options, &worker_control, &layout, pointer)
        } else {
            crate::recorder::record_to_in_layout(&path_buf, rect, &options, &worker_control, &layout)
        }
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    while control.frames.load(Ordering::Acquire) == 0 && !worker.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let started = Instant::now();
    let initial_millis = control.millis.load(Ordering::Relaxed);
    let mut blue = false;
    let mut system_switches = Vec::new();
    let mut pause_switches = Vec::new();
    while started.elapsed().as_secs_f32() < seconds && !worker.is_finished() {
        blue = !blue;
        dispatch2::DispatchQueue::main().exec_async(move || fixture_color(blue));
        if switch {
            let third = started.elapsed().as_secs_f32() / seconds * 3.0;
            let enabled = !(1.0..2.0).contains(&third) && matches!(audio.as_str(), "system" | "both");
            if control.system.swap(enabled, Ordering::AcqRel) != enabled {
                system_switches.push((enabled, control.millis.load(Ordering::Relaxed)));
            }
            control.cursor.store(third < 2.0, Ordering::Relaxed);
        }
        if verify_pause {
            let third = started.elapsed().as_secs_f32() / seconds * 3.0;
            let paused = (1.0..2.0).contains(&third);
            if control.paused.swap(paused, Ordering::AcqRel) != paused {
                pause_switches.push((paused, Instant::now(), control.millis.load(Ordering::Relaxed)));
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let ended = Instant::now();
    control.stop.store(true, Ordering::Release);
    let result = worker.join().map_err(|_| "the recording thread panicked".to_owned()).and_then(|r| r);
    if draw {
        crate::screendraw::hide();
    }
    let spec = result?;
    let frames = control.frames.load(Ordering::Acquire);
    if frames < 2 {
        return Err("the recording did not receive enough screen frames".into());
    }
    if let Some(note) = control.audio.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return Err(format!("recording source failed: {note}"));
    }
    if verify_pause {
        let [(true, paused, _), (false, resumed, _)] = pause_switches.as_slice() else {
            return Err("expected one pause/resume transition".into());
        };
        let expected = (ended - started).saturating_sub(*resumed - *paused).as_millis() as u64;
        let recorded = control.millis.load(Ordering::Relaxed).saturating_sub(initial_millis);
        if recorded.abs_diff(expected) > 350 {
            return Err(format!("pause duration was not removed: expected {expected} ms, recorded {recorded} ms"));
        }
        println!("pause removed {:.2} s from the recording timeline", (*resumed - *paused).as_secs_f64());
    }
    let mut reader = media::Reader::open(path)?;
    if (reader.width, reader.height) != (spec.width, spec.height) || reader.duration <= 0 {
        return Err("the recorded video has incorrect dimensions or duration".into());
    }
    if audio != "none" && reader.audio.is_none() {
        return Err("the recording is missing its audio track".into());
    }
    let mut first: Option<Vec<[u8; 3]>> = None;
    let mut changed = false;
    let mut saw_draw = !draw;
    let mut pointer_pixels = PointerPixels::default();
    while let Some(frame) = reader.next_video()? {
        if synthetic_pointer.is_some() {
            pointer_pixels.observe(&frame, position)?;
        }
        let at = ((frame.height / 2 * frame.width + frame.width / 2) * 4) as usize;
        let pixel = &frame.bgra[at..at + 3];
        if draw {
            let mark_at = ((frame.height / 4 * frame.width + frame.width / 2) * 4) as usize;
            let mark = &frame.bgra[mark_at..mark_at + 3];
            saw_draw |= u16::from(pixel[0]) > u16::from(pixel[2]) + 80
                && u16::from(mark[2]) > u16::from(mark[0]) + 80
                && u16::from(mark[2]) > u16::from(mark[1]) + 80;
        }
        let mut positions = vec![(frame.width / 2, frame.height / 2)];
        if pointer_fx {
            positions.extend([
                (frame.width / 4, frame.height / 4),
                (frame.width * 3 / 4, frame.height / 4),
                (frame.width / 4, frame.height * 3 / 4),
                (frame.width * 3 / 4, frame.height * 3 / 4),
            ]);
        }
        let colors: Vec<[u8; 3]> = positions.into_iter().map(|(x, y)| {
            let at = ((y * frame.width + x) * 4) as usize;
            [frame.bgra[at], frame.bgra[at + 1], frame.bgra[at + 2]]
        }).collect();
        if let Some(previous) = &first {
            changed |= previous.iter().zip(&colors).any(|(before, after)| {
                before.iter().zip(after).map(|(&a, &b)| (a as i32 - b as i32).abs()).sum::<i32>() > 80
            });
        } else {
            first = Some(colors);
        }
    }
    if !changed {
        return Err("the recorded frames did not show the changing fixture".into());
    }
    if !saw_draw {
        return Err("the recorded video did not contain the Screen Draw box at its expected pixel position".into());
    }
    if draw {
        println!("Screen Draw box verified in decoded recording pixels");
    }
    if let Some(mode) = synthetic_pointer {
        pointer_pixels.verify(&mode.expected())?;
        println!("synthetic fractional pointer effects verified in decoded recording pixels: {pointer_pixels:?}");
    }
    if audio != "none" {
        reader.select_video(false)?;
        reader.select_audio(true)?;
        reader.seek(0)?;
        let mut samples = 0usize;
        let mut peak = 0i32;
        let mut windows = if verify_switch {
            let [(false, off), (true, on)] = system_switches.as_slice() else {
                return Err(format!("expected one off/on transition, got {system_switches:?}"));
            };
            let rate = reader.audio.ok_or("the recording is missing its audio format")?.sample_rate;
            let end = (reader.duration / 10_000).max(0) as u64;
            Some([
                AudioWindow::new(off.saturating_sub(800), off.saturating_sub(250), rate)?,
                AudioWindow::new(off + 300, on.saturating_sub(300), rate)?,
                AudioWindow::new(on + 750, (on + 1_250).min(end.saturating_sub(50)), rate)?,
            ])
        } else {
            None
        };
        let mut pause_windows = if verify_pause {
            let [(true, _, join), (false, _, _)] = pause_switches.as_slice() else {
                return Err("expected one pause/resume transition".into());
            };
            let rate = reader.audio.ok_or("the recording is missing its audio format")?.sample_rate;
            Some([
                AudioWindow::new(join.saturating_sub(400), join.saturating_sub(150), rate)?,
                AudioWindow::new(join + 150, join + 400, rate)?,
            ])
        } else {
            None
        };
        while let Some((time, block)) = reader.next_audio()? {
            samples += block.len();
            peak = peak.max(block.iter().map(|&sample| (sample as i32).abs()).max().unwrap_or(0));
            if windows.is_some() || pause_windows.is_some() {
                let rate = reader.audio.ok_or("the recording is missing its audio format")?.sample_rate;
                let first = (i128::from(time.max(0)) * i128::from(rate) / i128::from(media::HNS_PER_SECOND)) as u64;
                if let Some(windows) = &mut windows {
                    for window in windows {
                        window.observe(first, &block);
                    }
                }
                if let Some(windows) = &mut pause_windows {
                    for window in windows {
                        window.observe(first, &block);
                    }
                }
            }
        }
        if samples == 0 {
            return Err("the recorded audio track had no samples".into());
        }
        if require_signal && peak == 0 {
            return Err("the recorded audio samples were silent".into());
        }
        if let Some([before, disabled, resumed]) = windows {
            let (before, disabled, resumed) = (before.rms()?, disabled.rms()?, resumed.rms()?);
            println!("system-audio toggle RMS: before {before:.1}, disabled {disabled:.1}, resumed {resumed:.1}");
            if before < 20.0 || resumed < 20.0 || disabled > (before.min(resumed) * 0.12).max(60.0) {
                return Err("system-audio toggle did not produce the expected audible/silent/audible intervals".into());
            }
        }
        if let Some([before, after]) = pause_windows {
            let (before, after) = (before.rms()?, after.rms()?);
            println!("pause-join audio RMS: before {before:.1}, after {after:.1}");
            if before < 20.0 || after < 20.0 {
                return Err("the recording lost system audio around its pause/resume join".into());
            }
        }
        println!("read {samples} audio samples, peak {peak}");
    }
    println!(
        "recorded {}x{} at {} fps: {frames} frames; read back {:.2} s, audio {:?}; frames changed",
        spec.width,
        spec.height,
        spec.fps,
        reader.duration as f64 / media::HNS_PER_SECOND as f64,
        reader.audio
    );
    println!("wrote {}", path.display());
    Ok(())
}

fn workflow_library_state(root: &Path) -> Result<(Vec<std::path::PathBuf>, Vec<std::path::PathBuf>), String> {
    let mut stages = Vec::new();
    let mut recordings = Vec::new();
    for folder in std::fs::read_dir(root).map_err(|error| error.to_string())? {
        let folder = folder.map_err(|error| error.to_string())?;
        if !folder.file_type().map_err(|error| error.to_string())?.is_dir() {
            return Err(format!("unexpected file in the recording test library: {}", folder.path().display()));
        }
        for entry in std::fs::read_dir(folder.path()).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            if entry.file_name().to_string_lossy().starts_with(".jiffshot-recording-") {
                stages.push(entry.path());
            } else if entry.path().extension().is_some_and(|extension| extension == "mp4") {
                recordings.push(entry.path());
            } else {
                return Err(format!("unexpected recording test file: {}", entry.path().display()));
            }
        }
    }
    Ok((stages, recordings))
}

fn check_workflow_video(path: &Path) -> Result<usize, String> {
    let mut reader = media::Reader::open(path)?;
    if reader.duration <= 0 {
        return Err("the recording has no duration".into());
    }
    let mut frames = 0;
    let mut saw_fixture = false;
    while let Some(frame) = reader.next_video()? {
        frames += 1;
        let at = ((frame.height / 2 * frame.width + frame.width / 2) * 4) as usize;
        let pixel = &frame.bgra[at..at + 3];
        saw_fixture |= u16::from(pixel[2]) > u16::from(pixel[0]) + 80
            && u16::from(pixel[2]) > u16::from(pixel[1]) + 80;
    }
    if frames < 2 || !saw_fixture {
        return Err("the recording did not contain the synthetic red fixture".into());
    }
    Ok(frames)
}

fn record_shutdown_fixture(dir: &Path, rect: Rect) -> Result<(), String> {
    let library = dir.join("workflow-library");
    for (name, expected) in [
        ("JIFFSHOT_LIBRARY", library.clone()),
        ("JIFFSHOT_CONFIG_DIR", dir.join("workflow-config")),
        ("JIFFSHOT_LOCAL_DIR", dir.join("workflow-local")),
    ] {
        if std::env::var_os(name).as_deref() != Some(expected.as_os_str()) {
            return Err(format!("{name} must be set to {} for this self-test", expected.display()));
        }
    }
    std::fs::create_dir(&library).map_err(|error| format!("could not create the empty test library {}: {error}", library.display()))?;
    let requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hud_requested = requested.clone();
    let hud_errors = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let observed_errors = hud_errors.clone();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let hooks = crate::Hooks {
        open_in_editor: Box::new(|_| false),
        show_editor: Box::new(|| {}),
        quit: Box::new(|| {}),
        notify: Box::new(|title, text, problem| {
            if problem {
                eprintln!("recording shutdown self-test: {title}: {text}");
            }
        }),
        state: Box::new(move |_| { let _ = ready_tx.try_send(()); }),
        startup_status: Box::new(|_| {}),
        show_capture_window: Box::new(|| {}),
        show_text: Box::new(|_| {}),
        hud: Box::new(move |json| {
            let value = match serde_json::from_str::<serde_json::Value>(&json) {
                Ok(value) => value,
                Err(error) => {
                    hud_errors.lock().unwrap().push(format!("invalid toolbar state: {error}"));
                    crate::stop_engine();
                    return;
                }
            };
            let recording = value["panels"].as_array().into_iter().flatten()
                .any(|panel| panel["toolbar"]["frames"].as_u64().unwrap_or(0) >= 6);
            if recording && !hud_requested.swap(true, std::sync::atomic::Ordering::AcqRel) {
                crate::stop_engine();
            }
        }),
        overlay: Box::new(|_| {}),
        screen_draw: Box::new(|_| {}),
    };
    let engine = crate::start_engine(None, hooks).map_err(|error| format!("could not start the capture engine: {error}"))?;
    if ready_rx.recv_timeout(Duration::from_secs(15)).is_err() {
        crate::stop_engine();
        return Err("the recording shutdown test capture engine did not start".into());
    }
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let posted = crate::post_capture_thread(move || {
        let result = (|| {
            let settings = crate::settings::Settings {
                countdown: 0,
                record_audio: "none".into(),
                record_cursor: false,
                cursor_highlight: false,
                click_rings: false,
                webcam: None,
                ..Default::default()
            };
            let layout = crate::geometry::CaptureLayout::current().map_err(|error| error.to_string())?;
            crate::recorder::record_in_layout(rect, &settings, layout)
        })();
        let _ = done_tx.send(result);
    });
    if !posted {
        crate::stop_engine();
        return Err("could not start the recording on the capture thread".into());
    }
    let outcome = done_rx.recv_timeout(Duration::from_secs(150))
        .map_err(|_| "the recording shutdown self-test timed out".to_owned());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !engine.is_finished() {
        if outcome.is_err() || !requested.load(std::sync::atomic::Ordering::Acquire) {
            crate::stop_engine();
        }
        if Instant::now() >= deadline {
            return Err("the capture engine did not exit after the recording shutdown".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let code = engine.join().map_err(|_| "the capture engine panicked".to_owned())??;
    if code != 0 {
        return Err(format!("the capture engine exited with status {code}"));
    }
    let errors = observed_errors.lock().unwrap();
    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    let path = outcome??.ok_or("the recording was discarded during shutdown")?;
    if !requested.load(std::sync::atomic::Ordering::Acquire) {
        return Err("the shutdown was not requested during an active recording".into());
    }
    let (stages, recordings) = workflow_library_state(&library)?;
    if !stages.is_empty() || recordings.as_slice() != [path.as_path()] {
        return Err(format!("shutdown left {} stages and unexpected recordings: {recordings:?}", stages.len()));
    }
    let frames = check_workflow_video(&path)?;
    println!("capture-engine shutdown published a recording with {frames} decoded synthetic fixture frames");
    Ok(())
}

fn record_workflow_fixture(dir: &Path, rect: Rect) -> Result<(), String> {
    use std::collections::HashSet;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    // SAFETY: this only queries the process's effective user.
    if unsafe { libc::geteuid() } == 0 {
        return Err("the recording workflow self-test needs a non-root account to check publication failure".into());
    }
    let library = dir.join("workflow-library");
    for (name, expected) in [
        ("JIFFSHOT_LIBRARY", library.clone()),
        ("JIFFSHOT_CONFIG_DIR", dir.join("workflow-config")),
        ("JIFFSHOT_LOCAL_DIR", dir.join("workflow-local")),
    ] {
        if std::env::var_os(name).as_deref() != Some(expected.as_os_str()) {
            return Err(format!("{name} must be set to {} for this self-test", expected.display()));
        }
    }
    std::fs::create_dir(&library).map_err(|error| format!("could not create the empty test library {}: {error}", library.display()))?;

    #[derive(Default)]
    struct Observations {
        sent: HashSet<u64>,
        actions: usize,
        errors: Vec<String>,
        restore_permissions: Option<(std::path::PathBuf, std::fs::Permissions)>,
    }
    let observations = Arc::new(Mutex::new(Observations::default()));
    let hud_observations = observations.clone();
    let hud_library = library.clone();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let hooks = crate::Hooks {
        open_in_editor: Box::new(|_| false),
        show_editor: Box::new(|| {}),
        quit: Box::new(|| {}),
        notify: Box::new(|title, text, problem| {
            if problem {
                eprintln!("recording workflow self-test: {title}: {text}");
            }
        }),
        state: Box::new(move |_| { let _ = ready_tx.try_send(()); }),
        startup_status: Box::new(|_| {}),
        show_capture_window: Box::new(|| {}),
        show_text: Box::new(|_| {}),
        hud: Box::new(move |json| {
            let value: serde_json::Value = match serde_json::from_str(&json) {
                Ok(value) => value,
                Err(error) => {
                    hud_observations.lock().unwrap().errors.push(format!("invalid toolbar state: {error}"));
                    return;
                }
            };
            for panel in value["panels"].as_array().into_iter().flatten() {
                let Some(id) = panel["id"].as_u64() else { continue };
                if panel["toolbar"]["frames"].as_u64().unwrap_or(0) < 6 {
                    continue;
                }
                let mut observations = hud_observations.lock().unwrap();
                if !observations.sent.insert(id) {
                    continue;
                }
                let expected_recordings = usize::from(observations.actions >= 3);
                match workflow_library_state(&hud_library) {
                    Ok((stages, recordings)) if stages.len() == 1 && recordings.len() == expected_recordings => {
                        if observations.actions == 3 {
                            let denied = (|| -> Result<_, String> {
                                let folder = stages[0].parent().ok_or("the recording stage has no parent")?.to_path_buf();
                                let original = std::fs::metadata(&folder).map_err(|error| error.to_string())?.permissions();
                                let mut readonly = original.clone();
                                readonly.set_mode(original.mode() & !0o222);
                                std::fs::set_permissions(&folder, readonly).map_err(|error| error.to_string())?;
                                Ok((folder, original))
                            })();
                            match denied {
                                Ok(restore) => observations.restore_permissions = Some(restore),
                                Err(error) => observations.errors.push(format!("could not simulate publication failure: {error}")),
                            }
                        }
                    }
                    Ok((stages, recordings)) => observations.errors.push(format!(
                        "expected one active stage and {expected_recordings} published recordings, found {} stages and {} recordings",
                        stages.len(), recordings.len()
                    )),
                    Err(error) => observations.errors.push(error),
                }
                let action = ["restart", "cancel", "stop", "stop"].get(observations.actions).copied().unwrap_or("cancel");
                observations.actions += 1;
                drop(observations);
                crate::panel_action(id, action);
            }
        }),
        overlay: Box::new(|_| {}),
        screen_draw: Box::new(|_| {}),
    };
    let engine = crate::start_engine(None, hooks).map_err(|error| format!("could not start the capture engine: {error}"))?;
    if ready_rx.recv_timeout(Duration::from_secs(15)).is_err() {
        crate::stop_engine();
        return Err("the recording test capture engine did not start".into());
    }

    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let worker_observations = observations.clone();
    crate::on_capture_thread(move || {
        let result = std::panic::catch_unwind(|| -> Result<(), String> {
            let settings = crate::settings::Settings {
                countdown: 0,
                record_audio: "none".into(),
                record_cursor: false,
                cursor_highlight: false,
                click_rings: false,
                webcam: None,
                ..Default::default()
            };
            let layout = crate::geometry::CaptureLayout::current().map_err(|error| error.to_string())?;
            if crate::recorder::record_in_layout(rect, &settings, layout.clone())?.is_some() {
                return Err("a cancelled recording was published".into());
            }
            let (stages, recordings) = workflow_library_state(&library)?;
            if !stages.is_empty() || !recordings.is_empty() {
                return Err(format!("restart/cancel left {} stages and {} recordings", stages.len(), recordings.len()));
            }
            let path = crate::recorder::record_in_layout(rect, &settings, layout.clone())?
                .ok_or("the stopped recording was not published")?;
            let (stages, recordings) = workflow_library_state(&library)?;
            if !stages.is_empty() || recordings.as_slice() != [path.as_path()] {
                return Err(format!("stop left {} stages and unexpected recordings: {recordings:?}", stages.len()));
            }
            let frames = check_workflow_video(&path)?;
            let failed = crate::recorder::record_in_layout(rect, &settings, layout);
            let restore = worker_observations.lock().unwrap().restore_permissions.take()
                .ok_or("the recording destination was not made read-only")?;
            std::fs::set_permissions(&restore.0, restore.1)
                .map_err(|error| format!("could not restore permissions on {}: {error}", restore.0.display()))?;
            let error = match failed {
                Err(error) => error,
                Ok(_) => return Err("recording publication unexpectedly succeeded in a read-only folder".into()),
            };
            let (stages, recordings) = workflow_library_state(&library)?;
            if stages.len() != 1 || recordings.as_slice() != [path.as_path()] {
                return Err(format!("failed publication left {} stages and unexpected recordings: {recordings:?}", stages.len()));
            }
            let recoverable = stages[0].join("recording.mp4");
            if !error.contains(&recoverable.display().to_string()) {
                return Err(format!("publication error did not report {}: {error}", recoverable.display()));
            }
            let recovered_frames = check_workflow_video(&recoverable)?;
            let observations = worker_observations.lock().unwrap();
            if observations.actions != 4 || !observations.errors.is_empty() {
                return Err(format!("recording toolbar observations: {} actions; {:?}", observations.actions, observations.errors));
            }
            println!(
                "production recording restart, cancel, stop and publication failure verified; decoded {frames} published and {recovered_frames} recoverable fixture frames"
            );
            Ok(())
        })
        .unwrap_or_else(|_| Err("the recording workflow self-test panicked".into()));
        let restore = worker_observations.lock().unwrap().restore_permissions.take();
        let result = if let Some((folder, permissions)) = restore {
            match std::fs::set_permissions(&folder, permissions) {
                Ok(()) => result,
                Err(error) => Err(format!("could not restore permissions on {}: {error}", folder.display())),
            }
        } else {
            result
        };
        let _ = done_tx.send(result);
    });
    let outcome = done_rx.recv_timeout(Duration::from_secs(60))
        .map_err(|_| "the recording workflow self-test timed out".to_owned());
    crate::stop_engine();
    if outcome.is_ok() {
        let code = engine.join().map_err(|_| "the capture engine panicked".to_owned())??;
        if code != 0 {
            return Err(format!("the capture engine exited with status {code}"));
        }
    }
    outcome?
}

pub fn record(dir: &Path, seconds: f32) -> io::Result<()> {
    if !seconds.is_finite() || seconds < 0.5 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "recording self-test needs at least 0.5 seconds"));
    }
    std::fs::create_dir_all(dir)?;
    let path = dir.join("recording.mp4");
    if path.exists() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "recording.mp4 already exists in the output directory"));
    }
    let mtm = MainThreadMarker::new().ok_or_else(|| io::Error::other("the recording fixture must run on the main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    if app.activationPolicy() != NSApplicationActivationPolicy::Regular && !app.setActivationPolicy(NSApplicationActivationPolicy::Regular) {
        return Err(io::Error::other("could not activate the recording fixture"));
    }
    screen::refresh_work_areas();
    let work = screen::monitors().into_iter().find(|monitor| monitor.primary).ok_or_else(|| io::Error::other("no primary display"))?.work;
    let work = screen::rect_to_points(work);
    let (width, height) = ((work.size.width - 80.0).min(480.0), (work.size.height - 80.0).min(320.0));
    if width < 100.0 || height < 100.0 {
        return Err(io::Error::other("the primary display is too small for the recording fixture"));
    }
    let x = work.origin.x + 40.0;
    let y = work.origin.y + 40.0;
    let frame = NSRect::new(NSPoint::new(x, screen::main_height_points() - y - height), NSSize::new(width, height));
    // SAFETY: the window is created and used on the AppKit main thread.
    let window = unsafe { NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), frame, NSWindowStyleMask::Borderless, NSBackingStoreType::Buffered, false) };
    // SAFETY: the retained window remains owned until it is closed below.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setOpaque(true);
    window.setLevel(NSStatusWindowLevel);
    FIXTURE.with(|fixture| *fixture.borrow_mut() = Some(window.clone()));
    fixture_color(false);
    window.orderFrontRegardless();
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let points = objc2_core_foundation::CGRect { origin: objc2_core_foundation::CGPoint { x, y }, size: objc2_core_foundation::CGSize { width, height } };
    let workflow = std::env::var_os("JIFFSHOT_RECORD_WORKFLOW_TEST").is_some();
    let shutdown = std::env::var_os("JIFFSHOT_RECORD_SHUTDOWN_TEST").is_some();
    if workflow && shutdown {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "select only one recording workflow self-test"));
    }
    let rect = if workflow || shutdown {
        screen::rect_from_points(objc2_core_foundation::CGRect {
            origin: objc2_core_foundation::CGPoint { x: x + 10.0, y: y + 10.0 },
            size: objc2_core_foundation::CGSize { width: width - 20.0, height: height - 20.0 },
        })
    } else {
        screen::rect_from_points(points)
    };
    let workflow_dir = dir.to_path_buf();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        let outcome = std::panic::catch_unwind(|| {
            if shutdown {
                record_shutdown_fixture(&workflow_dir, rect)
            } else if workflow {
                record_workflow_fixture(&workflow_dir, rect)
            } else {
                record_fixture(&path, rect, seconds)
            }
        })
            .unwrap_or_else(|_| Err("the recording self-test panicked".to_owned()));
        let _ = tx.send(outcome);
        dispatch2::DispatchQueue::main().exec_async(stop_app);
    });
    app.run();
    FIXTURE.with(|fixture| {
        if let Some(window) = fixture.borrow_mut().take() {
            window.close();
        }
    });
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|_| io::Error::other("the recording self-test did not finish"))?
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_pixel_check_distinguishes_each_effect_from_the_fixture_background() {
        let video = |frame: &Frame| media::VideoFrame { time: 0, width: frame.width, height: frame.height, bgra: frame.pixels.clone() };
        let observe = |frame: &Frame| {
            let mut observed = PointerPixels::default();
            observed.observe(&video(frame), (80, 60)).unwrap();
            observed
        };
        for color in [[0, 0, 255, 255], [255, 0, 0, 255]] {
            let frame = Frame { width: 240, height: 180, pixels: color.repeat(240 * 180), alpha: false };
            assert_eq!(observe(&frame), PointerPixels::default());
            let mut highlighted = frame.clone();
            crate::vfx::highlight(&mut highlighted, (80, 60));
            assert_eq!(observe(&highlighted), PointerPixels { highlight: true, cursor: false, click: false });
            let mut cursor = frame.clone();
            let image = Frame { width: 12, height: 12, pixels: vec![0, 255, 0, 255].repeat(144), alpha: true };
            cursor.composite_over(&image, (80, 60)).unwrap();
            assert_eq!(observe(&cursor), PointerPixels { highlight: false, cursor: true, click: false });
            let mut clicked = frame.clone();
            let mut clicks = crate::vfx::Clicks::default();
            clicks.update_buttons((80, 60), true, false);
            clicks.draw(&mut clicked);
            assert_eq!(observe(&clicked), PointerPixels { highlight: false, cursor: false, click: true });
            assert!(observe(&highlighted).verify(&SyntheticPointerMode::All.expected()).is_err());
        }
    }

    #[test]
    fn window_fixture_uses_an_owned_directory_and_reports_cleanup_failures() {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("jiffshot-window-fixture-test-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let existing = root.join(format!(".jiffshot-window-fixture-{}-0", std::process::id()));
        std::fs::create_dir(&existing).unwrap();
        let canary = existing.join("window.png");
        std::fs::write(&canary, b"preserve").unwrap();

        with_window_fixture_path(&root, |path| {
            assert_ne!(path, canary);
            std::fs::write(path, b"fixture")?;
            Ok(())
        }).unwrap();
        assert_eq!(std::fs::read(&canary).unwrap(), b"preserve");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);

        let mut displaced = None;
        let error = with_window_fixture_path(&root, |path| -> io::Result<()> {
            std::fs::write(path, b"fixture")?;
            let extra = path.parent().unwrap().join("keep");
            std::fs::write(&extra, b"preserve")?;
            displaced = Some(extra);
            Err(io::Error::other("capture failed"))
        }).unwrap_err().to_string();
        assert!(error.contains("capture failed") && error.contains("could not remove"));
        let displaced = displaced.unwrap();
        assert_eq!(std::fs::read(&displaced).unwrap(), b"preserve");
        std::fs::remove_file(&displaced).unwrap();
        std::fs::remove_dir(displaced.parent().unwrap()).unwrap();
        std::fs::remove_file(&canary).unwrap();
        std::fs::remove_dir(&existing).unwrap();
        std::fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn audio_window_measures_only_its_requested_frames() {
        let mut window = AudioWindow::new(100, 300, 10).unwrap();
        window.observe(0, &[1, 1, 2, 2, 3, 3, 4, 4]);
        assert_eq!(window.samples, 4);
        assert!((window.rms().unwrap() - 6.5_f64.sqrt()).abs() < 1e-10);
        assert!(AudioWindow::new(100, 100, 10).is_err());
        assert!(AudioWindow::new(0, 100, 10).unwrap().rms().is_err());
    }
}
