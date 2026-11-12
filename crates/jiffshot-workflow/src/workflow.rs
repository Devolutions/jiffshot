//! What happens after a capture: crop, optional cursor, library save,
//! clipboard and hand-off to the editor. The slow parts (PNG encoding, the
//! clipboard copy) run on a worker thread so the next hotkey is never blocked.

use crate::geometry::CaptureLayout;
use crate::overlay::{Overlay, Selection};
use crate::settings::Settings;
use jiffshot_capture::library::{
    timestamp, CaptureKind, CaptureMeta, CursorPlacement, Library, SavedCapture,
};
use jiffshot_capture::screen::CursorSnapshot;
use jiffshot_capture::targets::process_name;
use jiffshot_capture::{clipboard, CaptureMapping, Frame, Rect};
use std::borrow::Cow;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct Capture {
    /// Identifies a foreground Capture-window image request, if any.
    pub request_id: Option<u64>,
    pub frame: Frame,
    pub meta: CaptureMeta,
    /// The pointer as its own image and where it goes (capture pixels),
    /// when it is kept as a separate object.
    pub cursor: Option<(Frame, (i32, i32))>,
    /// The top-level window it came from, to look up a browser's address.
    pub window: Option<isize>,
}

fn pixmap_for_frame(frame: &Frame) -> io::Result<tiny_skia::Pixmap> {
    frame.validate()?;
    let size = tiny_skia::IntSize::from_wh(frame.width, frame.height)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid image dimensions"))?;
    let mut rgba = Vec::with_capacity(frame.pixels.len());
    for pixel in frame.pixels.chunks_exact(4) {
        let alpha = if frame.alpha { pixel[3] } else { 255 };
        let premultiply = |value: u8| ((u32::from(value) * u32::from(alpha) + 127) / 255) as u8;
        rgba.extend_from_slice(&[
            premultiply(pixel[2]),
            premultiply(pixel[1]),
            premultiply(pixel[0]),
            alpha,
        ]);
    }
    tiny_skia::Pixmap::from_vec(rgba, size)
        .ok_or_else(|| io::Error::other("could not create an image for capture output"))
}

fn image_for_delivery(
    capture: &Capture,
    placement: Option<CursorPlacement>,
) -> io::Result<Cow<'_, Frame>> {
    let (cursor, placement) = match (&capture.cursor, placement) {
        (None, None) => return Ok(Cow::Borrowed(&capture.frame)),
        (Some((cursor, _)), Some(placement)) => (cursor, placement),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "missing editable pointer bounds",
            ))
        }
    };
    let [x, y, width, height] = placement.geometry((cursor.width, cursor.height))?;
    if width == cursor.width as f32
        && height == cursor.height as f32
        && x.fract() == 0.0
        && y.fract() == 0.0
        && x >= i32::MIN as f32
        && x <= i32::MAX as f32
        && y >= i32::MIN as f32
        && y <= i32::MAX as f32
    {
        let mut image = capture.frame.clone();
        image.composite_over(cursor, (i64::from(x as i32), i64::from(y as i32)))?;
        return Ok(Cow::Owned(image));
    }
    let mut image = pixmap_for_frame(&capture.frame)?;
    let cursor = pixmap_for_frame(cursor)?;
    let rect = jiffshot_doc::Box2::from_xywh(x, y, width, height);
    jiffshot_doc::object::draw_image(&mut image, tiny_skia::Transform::identity(), &rect, &cursor);
    Ok(Cow::Owned(frame_from_pixmap(&image)))
}

fn apply_capture_effects(
    capture: &mut Capture,
    names: &[String],
) -> io::Result<Option<CursorPlacement>> {
    let initial = capture
        .cursor
        .as_ref()
        .map(|(_, position)| CursorPlacement::pixels(*position));
    if names.is_empty() {
        return Ok(initial);
    }
    let mut doc = jiffshot_doc::Document::new(pixmap_for_frame(&capture.frame)?);
    let id = if let (Some((cursor, _)), Some(position)) = (&capture.cursor, initial) {
        let [x, y, width, height] = position.geometry((cursor.width, cursor.height))?;
        let kind = jiffshot_doc::object::Kind::Image {
            rect: jiffshot_doc::Box2::from_xywh(x, y, width, height),
            image: jiffshot_doc::object::ImageRef {
                key: "cursor".to_owned(),
                pixmap: None,
            },
        };
        Some(doc.add(
            kind,
            jiffshot_doc::object::Style {
                width: 0.0,
                ..Default::default()
            },
        ))
    } else {
        None
    };
    apply_named_effects(&mut doc, names);
    let placement = match (id, &capture.cursor) {
        (Some(id), Some((cursor, _))) => {
            let rect = doc
                .get(id)
                .and_then(|object| object.rect())
                .ok_or_else(|| io::Error::other("capture effects lost the editable pointer"))?;
            let placement = CursorPlacement {
                x: f64::from(rect.x0),
                y: f64::from(rect.y0),
                width: Some(f64::from(rect.width())),
                height: Some(f64::from(rect.height())),
            };
            placement.geometry((cursor.width, cursor.height))?;
            Some(placement)
        }
        (None, None) => None,
        _ => {
            return Err(io::Error::other(
                "capture effects lost the editable pointer",
            ))
        }
    };
    capture.frame = frame_from_pixmap(doc.background());
    Ok(placement)
}

fn temporary_flattened_share(path: &Path, image: &Frame) -> Result<PathBuf, String> {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or("the capture filename is unavailable")?;
    #[cfg(target_os = "macos")]
    let target = jiffshot_capture::share::temporary_export_path(stem, "png")?;
    #[cfg(windows)]
    let target = windows_temporary_export_path(stem, "png")?;
    let output = std::fs::File::options()
        .write(true)
        .create_new(true)
        .open(&target)
        .map_err(|error| {
            format!(
                "could not create the temporary share {}: {error}",
                target.display()
            )
        })?;
    let mut output = io::BufWriter::with_capacity(1 << 20, output);
    encode_and_flush(image, &mut output).map_err(|error| {
        format!(
            "could not prepare the temporary share {}; an incomplete file may remain: {error}",
            target.display()
        )
    })?;
    Ok(target)
}

#[cfg(windows)]
fn windows_temporary_export_path(stem: &str, ext: &str) -> Result<PathBuf, String> {
    use std::os::windows::ffi::OsStrExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

    if stem.is_empty()
        || matches!(stem, "." | "..")
        || stem.contains(['/', '\\', ':', '\0'])
        || ext.is_empty()
        || ext.contains(['/', '\\', ':', '\0'])
    {
        return Err("invalid temporary sharing filename".into());
    }

    struct Descriptor(*mut core::ffi::c_void);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: the descriptor was allocated by Windows and is owned here.
            unsafe { LocalFree(self.0) };
        }
    }

    let sid = jiffshot_control::user_sid()
        .map_err(|error| format!("could not identify the sharing user: {error}"))?;
    let sddl = jiffshot_capture::wide(&format!("D:P(A;OICI;FA;;;{sid})"));
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: the SDDL string is NUL-terminated and the output is freed by Descriptor.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(format!("could not protect the temporary share: {}", io::Error::last_os_error()));
    }
    let descriptor = Descriptor(descriptor);
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("could not prepare a temporary share: {error}"))?
        .as_nanos();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for _ in 0..32 {
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "jiffshot-share-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        let wide: Vec<u16> = directory.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: the path is NUL-terminated and the descriptor outlives this call.
        if unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) } != 0 {
            return Ok(directory.join(format!("{stem}.{ext}")));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(format!("could not prepare a temporary sharing folder: {error}"));
        }
    }
    Err("could not create a unique temporary sharing folder".into())
}

fn encode_and_flush(image: &Frame, output: &mut impl Write) -> io::Result<()> {
    image.encode_png(&mut *output)?;
    output.flush()
}

fn meta(kind: CaptureKind, rect: Rect, frame: &Frame, cursor_included: bool) -> CaptureMeta {
    CaptureMeta {
        kind,
        rect,
        width: frame.width,
        height: frame.height,
        captured_at: timestamp(),
        window_title: None,
        process: None,
        cursor_included,
        url: None,
        tags: Vec::new(),
        pinned: false,
    }
}

#[cfg(windows)]
const BROWSERS: &[&str] = &[
    "chrome.exe",
    "msedge.exe",
    "firefox.exe",
    "brave.exe",
    "opera.exe",
    "vivaldi.exe",
];
#[cfg(target_os = "macos")]
const BROWSERS: &[&str] = &[
    "Safari",
    "Google Chrome",
    "firefox",
    "Microsoft Edge",
    "Brave Browser",
    "Opera",
    "Vivaldi",
    "Arc",
    "Chromium",
];

/// The address in a browser window's address bar (UI Automation: the
/// first edit box, which every major browser puts in its toolbar).
#[cfg(windows)]
pub fn browser_url(hwnd: isize) -> Option<String> {
    use windows::Win32::Foundation::HWND as WHWND;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, TreeScope_Descendants, UIA_ControlTypePropertyId,
        UIA_EditControlTypeId, UIA_ValueValuePropertyId,
    };
    // SAFETY: COM calls on this (worker) thread with valid arguments.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()?;
        let window = automation.ElementFromHandle(WHWND(hwnd as *mut _)).ok()?;
        let edit = automation
            .CreatePropertyCondition(
                UIA_ControlTypePropertyId,
                &VARIANT::from(UIA_EditControlTypeId.0),
            )
            .ok()?;
        let bar = window.FindFirst(TreeScope_Descendants, &edit).ok()?;
        let value = bar.GetCurrentPropertyValue(UIA_ValueValuePropertyId).ok()?;
        let text = value.to_string().trim().to_owned();
        if text.is_empty() || text.contains(' ') {
            return None;
        }
        Some(if text.contains("://") || text.starts_with("about:") {
            text
        } else {
            format!("https://{text}")
        })
    }
}

/// The address a browser window shows (Accessibility: its document, or the
/// address field in its toolbar).
#[cfg(target_os = "macos")]
pub fn browser_url(window: isize) -> Option<String> {
    jiffshot_capture::targets::browser_url(window)
}

pub(crate) fn cursor_intersects(cursor: &CursorSnapshot, mapping: CaptureMapping) -> bool {
    #[cfg(target_os = "macos")]
    return cursor.intersects_mapping(mapping);
    #[cfg(windows)]
    mapping.rect().contains(cursor.x, cursor.y)
}

fn add_cursor(
    frame: &mut Frame,
    mapping: CaptureMapping,
    cursor: Option<&CursorSnapshot>,
    include: bool,
) -> io::Result<bool> {
    #[cfg(windows)]
    let rect = mapping.rect();
    match cursor {
        Some(cursor) if include && cursor_intersects(cursor, mapping) => {
            #[cfg(target_os = "macos")]
            cursor.draw_onto_mapped(frame, mapping)?;
            #[cfg(windows)]
            cursor.draw_onto(frame, (rect.x, rect.y))?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn capture_cursor(
    frame: &mut Frame,
    mapping: CaptureMapping,
    cursor: Option<&CursorSnapshot>,
    include: bool,
    as_object: bool,
) -> io::Result<(bool, Option<(Frame, (i32, i32))>)> {
    match cursor {
        Some(cursor) if include && as_object && cursor_intersects(cursor, mapping) => {
            #[cfg(target_os = "macos")]
            let image = cursor.mapped_image(mapping)?;
            #[cfg(windows)]
            let image = (
                cursor.image()?,
                (cursor.x - mapping.rect().x, cursor.y - mapping.rect().y),
            );
            Ok((true, Some(image)))
        }
        _ => Ok((add_cursor(frame, mapping, cursor, include)?, None)),
    }
}

/// Makes pixels outside `keep` (screen coordinates) transparent.
fn mask_frame(frame: &mut Frame, mapping: CaptureMapping, keep: impl Fn(f64, f64) -> bool) {
    frame.alpha = true;
    let stride = frame.stride();
    for y in 0..frame.height as i32 {
        for x in 0..frame.width as i32 {
            let (screen_x, screen_y) = mapping.pixel_center(x as u32, y as u32);
            let a = if keep(screen_x, screen_y) { 255 } else { 0 };
            frame.pixels[y as usize * stride + x as usize * 4 + 3] = a;
        }
    }
}

/// Even-odd point-in-polygon at a pixel center.
fn inside(points: &[(i32, i32)], px: f64, py: f64) -> bool {
    let n = points.len();
    let mut c = false;
    for i in 0..n {
        let (a, b) = (points[i], points[(i + n - 1) % n]);
        let (ax, ay, bx, by) = (a.0 as f64, a.1 as f64, b.0 as f64, b.1 as f64);
        if (ay > py) != (by > py) && px < (bx - ax) * (py - ay) / (by - ay) + ax {
            c = !c;
        }
    }
    c
}

/// Crops the overlay's frozen snapshot to the selection.
/// `cursor_object`: keep the pointer as a separate image (for the editor)
/// instead of drawing it in.
pub fn from_overlay(
    overlay: &Overlay,
    selection: &Selection,
    cursor: Option<&CursorSnapshot>,
    include_cursor: bool,
    cursor_object: bool,
) -> io::Result<Option<Capture>> {
    let rect = selection.rect();
    #[cfg(target_os = "macos")]
    let selected_layout = overlay
        .display_layout
        .as_ref()
        .map(|layout| layout.for_selection(selection, &overlay.targets))
        .transpose()?;
    #[cfg(target_os = "macos")]
    let mapping = match (&selected_layout, selection) {
        (Some(layout), _) => layout.mapping(rect)?,
        (
            None,
            Selection::Fixed {
                size: (width, height),
                ..
            },
        ) => CaptureMapping::new(rect, *width, *height).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "invalid fixed-size capture")
        })?,
        (None, _) => CaptureMapping::identity(rect).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "empty capture rectangle")
        })?,
    };
    #[cfg(windows)]
    let mapping = CaptureMapping::identity(rect)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty capture rectangle"))?;
    #[cfg(target_os = "macos")]
    let mut frame = if let Some(frozen) = &overlay.frozen {
        match &selected_layout {
            Some(layout) => layout.render_frozen(frozen, rect)?,
            None => frozen.render(mapping)?,
        }
    } else {
        if overlay.display_layout.is_some()
            && mapping.size() != (rect.width as u32, rect.height as u32)
        {
            return Err(io::Error::other(
                "the native-resolution frozen desktop is unavailable",
            ));
        }
        let Some(frame) = overlay
            .snapshot
            .crop(rect.offset(-overlay.origin.0, -overlay.origin.1))
        else {
            return Ok(None);
        };
        frame
    };
    #[cfg(windows)]
    let Some(mut frame) = overlay
        .snapshot
        .crop(rect.offset(-overlay.origin.0, -overlay.origin.1))
    else {
        return Ok(None);
    };
    let (cursor_included, separate) =
        capture_cursor(&mut frame, mapping, cursor, include_cursor, cursor_object)?;
    // Freehand and multi-area captures keep only their shapes.
    match selection {
        Selection::Freehand { points, .. } => {
            mask_frame(&mut frame, mapping, |x, y| inside(points, x, y));
        }
        Selection::Multi { regions, .. } => {
            mask_frame(&mut frame, mapping, |x, y| {
                regions
                    .iter()
                    .any(|r| r.contains(x.floor() as i32, y.floor() as i32))
            });
        }
        _ => {}
    }
    let kind = match selection {
        Selection::Freehand { .. } | Selection::Multi { .. } => CaptureKind::Region,
        Selection::Region(_) => CaptureKind::Region,
        Selection::Window { .. } => CaptureKind::Window,
        Selection::Monitor(_) => CaptureKind::Monitor,
        Selection::Desktop(_) => CaptureKind::Desktop,
        #[cfg(target_os = "macos")]
        Selection::Fixed { .. } => CaptureKind::Region,
    };
    let mut meta = meta(kind, rect, &frame, cursor_included);
    // The window it came from: the one chosen, or the one under the
    // middle of a region.
    let target = match selection {
        Selection::Window { target, .. } => Some(*target),
        _ => overlay
            .targets
            .hit(rect.x + rect.width / 2, rect.y + rect.height / 2, 0),
    };
    let mut window = None;
    if let Some(target) = target {
        let top = overlay.targets.top_level(target);
        meta.window_title = Some(top.title.clone()).filter(|t| !t.is_empty());
        meta.process = process_name(top.process_id);
        window = Some(top.hwnd);
    }
    Ok(Some(Capture {
        request_id: None,
        frame,
        meta,
        cursor: separate,
        window,
    }))
}

/// Wraps a frame made some other way (scrolling, panoramic, menus).
pub fn from_frame(kind: CaptureKind, rect: Rect, frame: Frame) -> Capture {
    let meta = meta(kind, rect, &frame, false);
    Capture {
        request_id: None,
        frame,
        meta,
        cursor: None,
        window: None,
    }
}

/// Grabs a screen rectangle immediately (full screen and repeat-region
/// hotkeys).
pub fn immediate(kind: CaptureKind, rect: Rect, include_cursor: bool) -> std::io::Result<Capture> {
    immediate_in_layout(kind, rect, include_cursor, &CaptureLayout::current()?)
}

pub(crate) fn immediate_in_layout(
    kind: CaptureKind,
    rect: Rect,
    include_cursor: bool,
    layout: &CaptureLayout,
) -> std::io::Result<Capture> {
    immediate_in_layout_with_cursor(kind, rect, include_cursor, false, layout)
}

pub(crate) fn immediate_in_layout_with_cursor(
    kind: CaptureKind,
    rect: Rect,
    include_cursor: bool,
    cursor_as_object: bool,
    layout: &CaptureLayout,
) -> std::io::Result<Capture> {
    layout.validate()?;
    let mapping = layout.mapping(rect)?;
    #[cfg(windows)]
    let cursor = include_cursor.then(CursorSnapshot::take).flatten();
    #[cfg(target_os = "macos")]
    let cursor = include_cursor
        .then(|| CursorSnapshot::take_in_layout(layout.display()))
        .flatten();
    let mut frame = layout.grab(rect)?;
    layout.validate()?;
    let (cursor_included, separate) = capture_cursor(
        &mut frame,
        mapping,
        cursor.as_ref(),
        include_cursor,
        cursor_as_object,
    )?;
    let meta = meta(kind, rect, &frame, cursor_included);
    Ok(Capture {
        request_id: None,
        frame,
        meta,
        cursor: separate,
        window: None,
    })
}

/// Applies effects (by the editor's names) to a capture.
pub fn apply_effects(frame: &mut Frame, names: &[String]) -> io::Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    let mut doc = jiffshot_doc::Document::new(pixmap_for_frame(frame)?);
    apply_named_effects(&mut doc, names);
    *frame = frame_from_pixmap(&doc.flatten());
    Ok(())
}

fn apply_named_effects(doc: &mut jiffshot_doc::Document, names: &[String]) {
    for name in names {
        if let Some(effect) = jiffshot_doc::effects::by_name(name, jiffshot_doc::Rgba::BLACK) {
            jiffshot_doc::effects::apply(doc, effect);
        }
    }
}

/// Finds the editor: the configured path, then next to this executable, then
/// the development build under `app/artifacts`.
pub fn find_editor(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = configured {
        return path.exists().then(|| path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("JIFFSHOT_EDITOR").map(PathBuf::from) {
        if path.exists() {
            return Some(path);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent()?.to_path_buf();
    let beside = dir.join(format!("jiffshot{}", std::env::consts::EXE_SUFFIX));
    if beside.exists() {
        return Some(beside);
    }
    for _ in 0..5 {
        let dev = dir
            .join("app")
            .join("artifacts")
            .join("win-x64")
            .join("jiffshot.exe");
        if dev.exists() {
            return Some(dev);
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

/// Starts `program file` without inheriting any of our handles.
/// `std::process::Command` always enables handle inheritance, which would let
/// the editor hold our caller's pipes (or the tray's) open for its lifetime.
#[cfg(windows)]
fn launch_detached(program: &Path, file: &Path) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW,
    };
    let mut command_line =
        jiffshot_capture::wide(&format!("\"{}\" \"{}\"", program.display(), file.display()));
    let program = jiffshot_capture::wide(&program.to_string_lossy());
    // SAFETY: NUL-terminated strings, a mutable command-line buffer as
    // CreateProcessW requires, and both returned handles closed.
    unsafe {
        // Explicitly empty standard handles, so a console-subsystem editor
        // build cannot pick up our caller's console or pipes either.
        let mut startup: STARTUPINFOW = std::mem::zeroed();
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        startup.dwFlags = windows_sys::Win32::System::Threading::STARTF_USESTDHANDLES;
        let mut info: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessW(
            program.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            0,
            std::ptr::null(),
            std::ptr::null(),
            &startup,
            &mut info,
        );
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        CloseHandle(info.hThread);
        CloseHandle(info.hProcess);
    }
    Ok(())
}

/// Starts `program file` in its own session with no standard handles of
/// ours, and reaps it so it never lingers as a zombie.
#[cfg(not(windows))]
fn launch_detached(program: &Path, file: &Path) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let mut child = Command::new(program)
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

pub struct Outcome {
    pub message: String,
    pub is_error: bool,
    /// True when the editor took over, so no notification is needed.
    pub opened_editor: bool,
    pub saved_path: Option<PathBuf>,
}

/// Saves, copies and opens a capture according to `settings`.
pub fn finish(capture: Capture, settings: &Settings, owner: isize) -> Outcome {
    let library = Library::new(jiffshot_capture::config::library_root());
    finish_in_library(capture, settings, owner, &library)
}

fn finish_in_library(
    capture: Capture,
    settings: &Settings,
    owner: isize,
    library: &Library,
) -> Outcome {
    finish_in_library_with(
        capture,
        settings,
        owner,
        library,
        clipboard::copy,
        share_from,
    )
}

fn finish_in_library_with(
    mut capture: Capture,
    settings: &Settings,
    owner: isize,
    library: &Library,
    copy: impl Fn(&Frame, isize) -> io::Result<()>,
    share_capture: impl Fn(&str, &Path, &Path, isize) -> Result<Option<String>, String>,
) -> Outcome {
    let started = Instant::now();
    let (cursor_placement, effect_error) =
        match apply_capture_effects(&mut capture, &settings.capture_effects) {
            Ok(placement) => (placement, None),
            Err(error) => (
                capture
                    .cursor
                    .as_ref()
                    .map(|(_, position)| CursorPlacement::pixels(*position)),
                Some(error),
            ),
        };
    capture.meta.width = capture.frame.width;
    capture.meta.height = capture.frame.height;
    if let (Some(hwnd), Some(process)) = (capture.window, capture.meta.process.as_deref()) {
        if BROWSERS.iter().any(|b| process.eq_ignore_ascii_case(b)) {
            capture.meta.url = browser_url(hwnd);
        }
    }
    let (w, h) = (capture.frame.width, capture.frame.height);
    let saved_capture = match library.save_with_cursor(
        &capture.frame,
        &capture.meta,
        capture
            .cursor
            .as_ref()
            .zip(cursor_placement)
            .map(|((image, _), position)| (image, position)),
    ) {
        Ok(saved) => saved,
        Err(error) => {
            if !settings.open_in_editor {
                if let (Some(id), Some(hooks)) = (capture.request_id, crate::hooks()) {
                    (hooks.image_finished)(id, None, true, true);
                }
            }
            return Outcome {
                message: format!("Could not save the capture: {error}"),
                is_error: true,
                opened_editor: false,
                saved_path: None,
            }
        }
    };
    let (path, cursor_error) = match saved_capture {
        SavedCapture::Complete(path) => (path, None),
        SavedCapture::CursorFailed { path, error } => (path, Some(error)),
    };
    let cursor_saved = cursor_error.is_none();
    let saved = started.elapsed();
    let mut notes = Vec::new();
    let mut is_error = false;
    if let Some(error) = effect_error {
        is_error = true;
        notes.push(format!("capture effects could not be applied: {error}"));
    }
    if let Some(error) = cursor_error {
        is_error = true;
        notes.push(format!(
            "the editable pointer could not be saved for {}: {error}",
            path.display()
        ));
    }
    let delivery = if settings.copy_to_clipboard || !matches!(settings.share.as_str(), "" | "none")
    {
        match image_for_delivery(&capture, cursor_placement) {
            Ok(image) => Some(image),
            Err(error) => {
                is_error = true;
                notes.push(format!(
                    "could not prepare the editable pointer for clipboard or sharing: {error}"
                ));
                None
            }
        }
    } else {
        None
    };
    let clipboard_frame = delivery.as_deref();
    if settings.copy_to_clipboard {
        if let Some(frame) = clipboard_frame {
            match copy(frame, owner) {
                Ok(()) => notes.push("copied to the clipboard".to_owned()),
                Err(error) => {
                    is_error = true;
                    notes.push(format!("clipboard failed: {error}"));
                }
            }
        }
    }
    let mut opened_editor = false;
    if cursor_saved && settings.open_in_editor && crate::hooks().is_some() {
        opened_editor = open_editor(settings, &path);
    } else if cursor_saved
        && settings.open_in_editor
        && jiffshot_capture::ipc::send_to_editor(&path)
    {
        opened_editor = true;
    } else if cursor_saved && settings.open_in_editor {
        match find_editor(settings.editor.as_deref()) {
            Some(editor) => match launch_detached(&editor, &path) {
                Ok(()) => opened_editor = true,
                Err(error) => {
                    is_error = true;
                    notes.push(format!("editor failed to start: {error}"));
                }
            },
            None => notes.push("editor not found".to_owned()),
        }
    }
    let share_result = if matches!(settings.share.as_str(), "" | "none") {
        None
    } else {
        match delivery.as_ref() {
            Some(Cow::Borrowed(_)) => Some(share_capture(&settings.share, &path, &path, owner)),
            Some(Cow::Owned(image)) if settings.share == "clipboard" => Some(
                copy(image, owner)
                    .map(|()| Some("copied to the clipboard".to_owned()))
                    .map_err(|error| error.to_string()),
            ),
            Some(Cow::Owned(image)) => Some(
                temporary_flattened_share(&path, image)
                    .and_then(|export| share_capture(&settings.share, &export, &path, owner)),
            ),
            None => None,
        }
    };
    if let Some(result) = share_result {
        match result {
            Ok(Some(note)) => notes.push(note),
            Ok(None) => {}
            Err(error) => {
                is_error = true;
                notes.push(format!("share failed: {error}"));
            }
        }
    }
    let file = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut message = format!("{w} × {h} saved as {file}");
    if !notes.is_empty() {
        message = format!("{message}; {}", notes.join("; "));
    }
    if !settings.open_in_editor {
        if let (Some(id), Some(hooks)) = (capture.request_id, crate::hooks()) {
            (hooks.image_finished)(id, Some(&path), is_error, true);
        }
    }
    crate::log(&format!(
        "capture {w}x{h} saved in {:.0} ms, finished in {:.0} ms: {message}",
        saved.as_secs_f64() * 1000.0,
        started.elapsed().as_secs_f64() * 1000.0
    ));
    Outcome {
        message,
        is_error,
        opened_editor,
        saved_path: Some(path),
    }
}

/// A tiny-skia image (premultiplied RGBA) as a capture frame (BGRA).
pub fn frame_from_pixmap(pm: &tiny_skia::Pixmap) -> Frame {
    let mut frame = Frame::new(pm.width(), pm.height());
    frame.alpha = pm.pixels().iter().any(|p| p.alpha() < 255);
    for (d, p) in frame.pixels.chunks_exact_mut(4).zip(pm.pixels()) {
        let c = p.demultiply();
        d.copy_from_slice(&[c.blue(), c.green(), c.red(), c.alpha()]);
    }
    frame
}

/// Starts the editor with `path` (when no editor is running to take it).
/// Opens `path` in the editor: in this process when Jiffshot's app runs it,
/// otherwise in a running Jiffshot or a new one.
pub fn open_editor(settings: &Settings, path: &Path) -> bool {
    if let Some(hooks) = crate::hooks() {
        return (hooks.open_in_editor)(path);
    }
    jiffshot_capture::ipc::send_to_editor(path)
        || find_editor(settings.editor.as_deref())
            .is_some_and(|e| launch_detached(&e, path).is_ok())
}

/// After a recording: opens it in the editor (or says where it is).
pub fn finish_video(path: &Path, _rect: Rect, settings: &Settings, owner: isize) -> Outcome {
    let name = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut opened_editor = false;
    if settings.open_in_editor {
        opened_editor = open_editor(settings, path);
    }
    let mut message = format!("Recording saved as {name}");
    let mut is_error = false;
    match share(&settings.video_share, path, owner) {
        Ok(Some(note)) => message = format!("{message}; {note}"),
        Ok(None) => {}
        Err(error) => {
            is_error = true;
            message = format!("{message}; share failed: {error}");
        }
    }
    Outcome {
        message,
        is_error,
        opened_editor,
        saved_path: Some(path.to_path_buf()),
    }
}

/// Share destinations for captures, as (label, setting) pairs.
pub const SHARES: &[(&str, &str)] = &[
    ("None", "none"),
    ("Word", "word"),
    ("PowerPoint", "powerpoint"),
    ("Excel", "excel"),
    ("OneNote", "onenote"),
    ("Email", "email"),
    ("File…", "file"),
    ("Program…", "program"),
    ("Clipboard", "clipboard"),
    ("Printer", "print"),
];
/// The ones that take a video.
pub const VIDEO_SHARES: &[(&str, &str)] = &[
    ("None", "none"),
    ("PowerPoint", "powerpoint"),
    ("Email", "email"),
    ("File…", "file"),
    ("Program…", "program"),
];

/// Sends a saved capture to `to` (a [`SHARES`] setting); says what it did.
pub fn share(to: &str, path: &Path, owner: isize) -> Result<Option<String>, String> {
    share_from(to, path, path, owner)
}

fn share_from(
    to: &str,
    path: &Path,
    suggested_path: &Path,
    owner: isize,
) -> Result<Option<String>, String> {
    use jiffshot_capture::share;
    let office = |exe: &str, app: &str| {
        share::open_in(owner, exe, path).map(|_| Some(format!("opened in {app}")))
    };
    match to {
        "" | "none" => Ok(None),
        "file" => match save_copy_dialog(owner, suggested_path)? {
            Some(target) => copy_flattened_share(path, suggested_path, &target)
                .map(|()| Some(format!("copied to {}", target.display()))),
            None => Ok(None),
        },
        "email" => {
            let subject = suggested_path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            share::email(owner, path, &subject).map(|completed| {
                completed.then(|| {
                    if cfg!(windows) {
                        "opened a new message"
                    } else {
                        "email sharing completed"
                    }
                    .to_owned()
                })
            })
        }
        "word" => office("winword.exe", "Word"),
        "powerpoint" => office("powerpnt.exe", "PowerPoint"),
        "excel" => office("excel.exe", "Excel"),
        "onenote" => office("onenote.exe", "OneNote"),
        "paint" => office("mspaint.exe", "Paint"),
        "clipboard" => {
            let image = jiffshot_doc::io::load_image(path).map_err(|e| e.to_string())?;
            jiffshot_capture::clipboard::copy(&frame_from_pixmap(&image), owner)
                .map(|_| Some("copied to the clipboard".to_owned()))
                .map_err(|e| e.to_string())
        }
        "program" => share::open_with(owner, path).map(|opened| {
            (opened && cfg!(target_os = "macos"))
                .then(|| "opened in the selected program".to_owned())
        }),
        "print" => share::print(owner, path).map(|started| {
            started.then(|| {
                if cfg!(windows) {
                    "sent to the printer"
                } else {
                    "print dialog completed"
                }
                .to_owned()
            })
        }),
        other => Err(format!("unknown share destination {other}")),
    }
}

fn copy_flattened_share(source: &Path, original: &Path, target: &Path) -> Result<(), String> {
    let same_file_message = if source == original {
        "Choose a different file than the saved capture."
    } else {
        "Choose a different file: the capture and its editable pointer must remain separate."
    };
    let mut source_file = std::fs::File::open(source)
        .map_err(|error| format!("could not open {}: {error}", source.display()))?;
    let source_meta = source_file
        .metadata()
        .map_err(|error| format!("could not inspect {}: {error}", source.display()))?;
    let mut protected_paths = vec![original.to_path_buf()];
    protected_paths.extend(jiffshot_capture::library::companions(original));
    for path in &protected_paths {
        if same_protected_path(path, target).map_err(|error| {
            format!("could not inspect {}: {error}", target.display())
        })? {
            return Err(same_file_message.to_owned());
        }
    }
    let mut protected = Vec::new();
    for (index, path) in protected_paths.iter().enumerate() {
        match std::fs::File::open(path) {
            Ok(file) => protected.push(file_identity(&file).map_err(|error| {
                format!("could not inspect {}: {error}", path.display())
            })?),
            Err(error) if index != 0 && error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("could not inspect {}: {error}", path.display())),
        }
    }
    let source_identity = file_identity(&source_file)
        .map_err(|error| format!("could not inspect {}: {error}", source.display()))?;
    let mut target_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(target)
        .map_err(|error| format!("could not open {}: {error}", target.display()))?;
    let target_identity = file_identity(&target_file)
        .map_err(|error| format!("could not inspect {}: {error}", target.display()))?;
    if protected.contains(&target_identity) || target_identity == source_identity {
        return Err(same_file_message.to_owned());
    }
    target_file
        .set_len(0)
        .and_then(|()| io::copy(&mut source_file, &mut target_file).map(|_| ()))
        .and_then(|()| target_file.set_permissions(source_meta.permissions()))
        .map_err(|error| {
            format!(
                "could not finish sharing to {}: {error}; an incomplete file may remain there",
                target.display()
            )
        })
}

fn same_protected_path(path: &Path, target: &Path) -> io::Result<bool> {
    if path == target {
        return Ok(true);
    }
    #[cfg(windows)]
    {
        let (Some(name), Some(target_name)) = (path.file_name(), target.file_name()) else {
            return Ok(false);
        };
        if name.to_string_lossy().to_lowercase() != target_name.to_string_lossy().to_lowercase() {
            return Ok(false);
        }
        let parent = |path: &Path| {
            path.parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
                .canonicalize()
        };
        let first = parent(path)?;
        let second = parent(target)?;
        return Ok(first.to_string_lossy().to_lowercase() == second.to_string_lossy().to_lowercase());
    }
    #[cfg(target_os = "macos")]
    Ok(false)
}

#[cfg(target_os = "macos")]
fn file_identity(file: &std::fs::File) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn file_identity(file: &std::fs::File) -> io::Result<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: the live file handle and output buffer remain valid during the call.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), info.as_mut_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful call initializes the complete structure.
    let info = unsafe { info.assume_init() };
    Ok((
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

/// Asks where to save a copy of `path` (same type).
#[cfg(windows)]
fn save_copy_dialog(owner: isize, path: &Path) -> Result<Option<PathBuf>, String> {
    use jiffshot_capture::{from_wide, wide};
    use windows_sys::Win32::UI::Controls::Dialogs::{
        GetSaveFileNameW, OFN_OVERWRITEPROMPT, OPENFILENAMEW,
    };
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_else(|| "png".into());
    let mut buf = vec![0u16; 1024];
    let name = wide(
        &path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    buf[..name.len().min(1023)].copy_from_slice(&name[..name.len().min(1023)]);
    let filter: Vec<u16> = format!("{} file (*.{ext}) *.{ext}  ", ext.to_uppercase())
        .encode_utf16()
        .collect();
    let def = wide(&ext);
    // SAFETY: a plain C struct; zero is valid for every field.
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = owner as _;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.lpstrDefExt = def.as_ptr();
    ofn.Flags = OFN_OVERWRITEPROMPT;
    // SAFETY: every pointer in `ofn` outlives the modal call.
    let ok = unsafe { GetSaveFileNameW(&mut ofn) };
    Ok((ok != 0).then(|| PathBuf::from(from_wide(&buf))))
}

/// Asks where to save a copy of `path`.
#[cfg(target_os = "macos")]
fn save_copy_dialog(_owner: isize, path: &Path) -> Result<Option<PathBuf>, String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Some(mut chosen) =
        jiffshot_capture::os::choose_save_path("Save a copy as", &name, path.parent())
            .map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    if chosen.extension().is_none() {
        if let Some(ext) = path.extension() {
            chosen.set_extension(ext);
        }
    }
    Ok(Some(chosen))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    struct TestSecurityDescriptor(*mut core::ffi::c_void);

    #[cfg(windows)]
    impl Drop for TestSecurityDescriptor {
        fn drop(&mut self) {
            // SAFETY: Windows allocated the descriptor and this guard owns it.
            unsafe { windows_sys::Win32::Foundation::LocalFree(self.0) };
        }
    }

    #[cfg(windows)]
    fn test_security_descriptor(sddl: &str) -> TestSecurityDescriptor {
        use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;

        let text = jiffshot_capture::wide(sddl);
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: the SDDL string is NUL-terminated and the descriptor is owned below.
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    text.as_ptr(),
                    1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            },
            0,
            "could not create test ACL {sddl}: {}",
            io::Error::last_os_error()
        );
        TestSecurityDescriptor(descriptor)
    }

    #[cfg(windows)]
    fn dacl_grants_only_sid(descriptor: &TestSecurityDescriptor, sid: &str) -> io::Result<bool> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
        use windows_sys::Win32::Security::{
            EqualSid, GetAce, GetSecurityDescriptorDacl, IsValidSid, ACCESS_ALLOWED_ACE,
            ACE_HEADER, ACL,
        };

        // WinNT.h defines ACCESS_ALLOWED_ACE_TYPE as zero.
        const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

        let mut present = 0;
        let mut defaulted = 0;
        let mut acl: *mut ACL = std::ptr::null_mut();
        // SAFETY: descriptor is live and the output pointers are valid.
        if unsafe {
            GetSecurityDescriptorDacl(
                descriptor.0,
                &mut present,
                &mut acl,
                &mut defaulted,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if present == 0 || acl.is_null() {
            return Ok(false);
        }
        // SAFETY: the DACL belongs to the live security descriptor.
        if unsafe { (*acl).AceCount } != 1 {
            return Ok(false);
        }
        let mut entry = std::ptr::null_mut();
        // SAFETY: the DACL is live and the output pointer is valid.
        if unsafe { GetAce(acl, 0, &mut entry) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: GetAce returned a valid ACE within the DACL.
        let header = unsafe { &*entry.cast::<ACE_HEADER>() };
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE
            || usize::from(header.AceSize) < std::mem::size_of::<ACCESS_ALLOWED_ACE>()
        {
            return Ok(false);
        }
        // SAFETY: the ACE is an access-allowed ACE of at least the fixed size.
        let actual = unsafe { std::ptr::addr_of!((*entry.cast::<ACCESS_ALLOWED_ACE>()).SidStart) }
            .cast_mut()
            .cast();
        // SAFETY: the SID is part of the access-allowed ACE returned by Windows.
        if unsafe { IsValidSid(actual) } == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid ACL trustee SID"));
        }
        let mut expected = std::ptr::null_mut();
        let sid = jiffshot_capture::wide(sid);
        // SAFETY: the SID string is NUL-terminated and the output pointer is valid.
        if unsafe { ConvertStringSidToSidW(sid.as_ptr(), &mut expected) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both SIDs are valid and live; Windows allocated expected.
        let matches = unsafe { EqualSid(actual, expected) } != 0;
        // SAFETY: Windows allocated the expected SID.
        unsafe { LocalFree(expected) };
        Ok(matches)
    }

    #[cfg(windows)]
    fn windows_test_library(name: &str) -> (Library, PathBuf) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "jiffshot-{name}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        (Library::new(root.clone()), root)
    }

    #[cfg(windows)]
    #[test]
    fn windows_capture_effects_keep_straight_alpha_colors() {
        let mut frame = Frame {
            width: 1,
            height: 1,
            pixels: vec![20, 40, 120, 128],
            alpha: true,
        };
        apply_effects(&mut frame, &["Invert".into()]).unwrap();
        let (red, green, blue) = frame.rgb(0, 0).unwrap();
        assert!((i16::from(red) - 135).abs() <= 2, "red was {red}");
        assert!((i16::from(green) - 215).abs() <= 2, "green was {green}");
        assert!((i16::from(blue) - 235).abs() <= 2, "blue was {blue}");
        assert_eq!(frame.pixels[3], 128);
    }

    #[cfg(windows)]
    #[test]
    fn windows_capture_effects_move_the_editable_cursor() {
        let (library, root) = windows_test_library("cursor-effects");
        let frame = Frame::new(40, 20);
        let mut capture = from_frame(CaptureKind::Region, frame.bounds(), frame);
        capture.cursor = Some((Frame::new(2, 2), (3, 4)));
        let settings = Settings {
            copy_to_clipboard: false,
            open_in_editor: false,
            capture_effects: vec!["Border".into()],
            share: "none".into(),
            ..Settings::default()
        };
        let outcome = finish_in_library(capture, &settings, 0, &library);
        assert!(!outcome.is_error, "{}", outcome.message);
        let path = library.recent(10).pop().unwrap();
        let placement: CursorPlacement =
            serde_json::from_slice(&std::fs::read(path.with_extension("cursor.json")).unwrap())
                .unwrap();
        assert_eq!(placement.geometry((2, 2)).unwrap(), [11.0, 12.0, 2.0, 2.0]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_delivery_flattens_the_cursor_without_changing_the_library_image() {
        use std::cell::RefCell;

        let (library, root) = windows_test_library("cursor-delivery");
        let frame = Frame {
            width: 3,
            height: 1,
            pixels: vec![10, 20, 30, 0].repeat(3),
            alpha: false,
        };
        let mut capture = from_frame(CaptureKind::Region, frame.bounds(), frame);
        capture.cursor = Some((
            Frame { width: 1, height: 1, pixels: vec![0, 0, 255, 255], alpha: true },
            (1, 0),
        ));
        let settings = Settings {
            copy_to_clipboard: true,
            open_in_editor: false,
            share: "file".into(),
            ..Settings::default()
        };
        let copied = RefCell::new(None);
        let shared = RefCell::new(None);
        let outcome = finish_in_library_with(
            capture,
            &settings,
            0,
            &library,
            |image, _| {
                *copied.borrow_mut() = Some(image.clone());
                Ok(())
            },
            |_, source, suggestion, _| {
                *shared.borrow_mut() = Some((
                    source.to_path_buf(),
                    suggestion.to_path_buf(),
                    Frame::load_png(source).unwrap(),
                ));
                Ok(Some("shared".into()))
            },
        );
        assert!(!outcome.is_error, "{}", outcome.message);
        let copied = copied.into_inner().unwrap();
        let (source, suggestion, shared) = shared.into_inner().unwrap();
        assert_eq!(copied.rgb(1, 0), Some((255, 0, 0)));
        assert_eq!(shared.rgb(1, 0), Some((255, 0, 0)));
        assert_ne!(source, suggestion);
        assert_eq!(Frame::load_png(&suggestion).unwrap().rgb(1, 0), Some((30, 20, 10)));
        std::fs::remove_dir_all(source.parent().unwrap()).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_flattened_share_protects_the_capture_and_its_companions() {
        let (_, root) = windows_test_library("cursor-share-protection");
        let source = root.join("flattened.png");
        let original = root.join("original.png");
        let pointer = original.with_extension("cursor.png");
        let hardlink = root.join("hardlink.png");
        let pointer_link = root.join("pointer-link.png");
        let absent_sidecar = original.with_extension("JSD");
        std::fs::write(&source, b"flattened").unwrap();
        std::fs::write(&original, b"background").unwrap();
        std::fs::write(&pointer, b"editable pointer").unwrap();
        std::fs::hard_link(&original, &hardlink).unwrap();
        std::fs::hard_link(&pointer, &pointer_link).unwrap();
        for target in [&original, &hardlink, &pointer, &pointer_link, &source, &absent_sidecar] {
            assert!(copy_flattened_share(&source, &original, target).is_err(), "{}", target.display());
            assert_eq!(std::fs::read(&original).unwrap(), b"background");
            assert_eq!(std::fs::read(&pointer).unwrap(), b"editable pointer");
            assert_eq!(std::fs::read(&source).unwrap(), b"flattened");
        }
        assert!(!absent_sidecar.exists());
        let target = root.join("copy.png");
        std::fs::write(&target, b"existing longer file").unwrap();
        copy_flattened_share(&source, &original, &target).unwrap();
        assert_eq!(std::fs::read(target).unwrap(), b"flattened");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_temporary_shares_are_private_to_the_current_user() {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
        use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;

        assert!(windows_temporary_export_path("../capture", "png").is_err());
        let target = windows_temporary_export_path("capture", "png").unwrap();
        std::fs::write(&target, b"private").unwrap();
        let sid = jiffshot_control::user_sid().unwrap();
        for path in [target.parent().unwrap(), target.as_path()] {
            let mut name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let mut descriptor = std::ptr::null_mut();
            // SAFETY: name is NUL-terminated and the returned descriptor is freed below.
            let status = unsafe {
                GetNamedSecurityInfoW(
                    name.as_mut_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut descriptor,
                )
            };
            assert_eq!(status, 0, "could not read ACL for {}", path.display());
            let descriptor = TestSecurityDescriptor(descriptor);
            assert!(
                dacl_grants_only_sid(&descriptor, &sid).unwrap(),
                "ACL for {} does not grant access only to {sid}",
                path.display()
            );
        }
        std::fs::remove_file(&target).unwrap();
        std::fs::remove_dir(target.parent().unwrap()).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_acl_matches_well_known_sid_aliases() {
        let descriptor = test_security_descriptor("D:P(A;OICI;FA;;;BA)");
        assert!(dacl_grants_only_sid(&descriptor, "S-1-5-32-544").unwrap());
        assert!(!dacl_grants_only_sid(&descriptor, "S-1-5-18").unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn windows_acl_rejects_additional_or_missing_trustees() {
        for sddl in [
            "D:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)",
            "D:P(D;OICI;FA;;;BA)",
            "D:P",
        ] {
            let descriptor = test_security_descriptor(sddl);
            assert!(!dacl_grants_only_sid(&descriptor, "S-1-5-32-544").unwrap(), "{sddl}");
        }
    }

    #[test]
    fn cursor_finish_reports_a_pointer_save_failure_and_keeps_the_capture() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "jiffshot-cursor-finish-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let library = Library::new(root.clone());
        let frame = Frame::new(4, 3);
        let mut capture = from_frame(CaptureKind::Region, frame.bounds(), frame);
        capture.cursor = Some((Frame::new(0, 0), (1, 2)));
        let settings = Settings {
            copy_to_clipboard: false,
            open_in_editor: false,
            ..Settings::default()
        };

        let outcome = finish_in_library(capture, &settings, 0, &library);
        assert!(outcome.is_error);
        assert!(!outcome.opened_editor);
        assert!(
            outcome
                .message
                .contains("editable pointer could not be saved"),
            "{}",
            outcome.message
        );
        let paths = library.recent(10);
        assert_eq!(paths.len(), 1);
        assert!(outcome.message.contains(&paths[0].display().to_string()));
        assert_eq!(Frame::load_png(&paths[0]).unwrap().width, 4);
        assert!(paths[0].with_extension("json").exists());
        assert!(
            !jiffshot_capture::library::read_meta(&paths[0])
                .unwrap()
                .cursor_included
        );
        assert!(!paths[0].with_extension("cursor.png").exists());
        assert!(!paths[0].with_extension("cursor.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cursor_finish_saves_a_reopenable_pointer() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "jiffshot-cursor-finish-ok-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let library = Library::new(root.clone());
        let frame = Frame::new(4, 3);
        let mut capture = from_frame(CaptureKind::Region, frame.bounds(), frame);
        capture.cursor = Some((Frame::new(2, 1), (-1, 2)));
        let settings = Settings {
            copy_to_clipboard: false,
            open_in_editor: false,
            ..Settings::default()
        };

        let outcome = finish_in_library(capture, &settings, 0, &library);
        assert!(!outcome.is_error, "{}", outcome.message);
        let paths = library.recent(10);
        assert_eq!(paths.len(), 1);
        assert!(
            jiffshot_capture::library::read_meta(&paths[0])
                .unwrap()
                .cursor_included
        );
        let cursor = Frame::load_png(&paths[0].with_extension("cursor.png")).unwrap();
        assert_eq!((cursor.width, cursor.height), (2, 1));
        let position: CursorPlacement =
            serde_json::from_slice(&std::fs::read(paths[0].with_extension("cursor.json")).unwrap())
                .unwrap();
        assert_eq!(position, CursorPlacement::pixels((-1, 2)));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cursor_finish_flattens_delivery_but_keeps_the_library_editable() {
        use std::cell::RefCell;
        use std::os::unix::fs::PermissionsExt;

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "jiffshot-cursor-delivery-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let library = Library::new(root.clone());
        let frame = Frame {
            width: 3,
            height: 1,
            pixels: vec![10, 20, 30, 0].repeat(3),
            alpha: false,
        };
        let mut capture = from_frame(CaptureKind::Region, frame.bounds(), frame);
        capture.cursor = Some((
            Frame {
                width: 2,
                height: 1,
                pixels: vec![0, 0, 255, 255, 0, 255, 0, 128],
                alpha: true,
            },
            (1, 0),
        ));
        let settings = Settings {
            copy_to_clipboard: true,
            open_in_editor: false,
            share: "file".to_owned(),
            ..Settings::default()
        };
        let copied = RefCell::new(Vec::new());
        let shared = RefCell::new(None);
        let outcome = finish_in_library_with(
            capture,
            &settings,
            0,
            &library,
            |image, _| {
                copied.borrow_mut().push(image.clone());
                Ok(())
            },
            |destination, source, suggestion, _| {
                assert_eq!(destination, "file");
                assert_ne!(source, suggestion);
                assert_eq!(source.file_name(), suggestion.file_name());
                assert!(suggestion.starts_with(&root));
                assert_eq!(
                    std::fs::metadata(source.parent().unwrap())
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o077,
                    0
                );
                *shared.borrow_mut() = Some((
                    source.to_path_buf(),
                    suggestion.to_path_buf(),
                    Frame::load_png(source).unwrap(),
                ));
                Ok(Some("shared".to_owned()))
            },
        );
        assert!(!outcome.is_error, "{}", outcome.message);
        let copied = copied.into_inner();
        assert_eq!(copied.len(), 1);
        let (source, suggestion, shared_image) = shared.into_inner().unwrap();
        assert!(
            source.exists(),
            "the destination may read the file after the share call returns"
        );
        for image in [&copied[0], &shared_image] {
            assert_eq!(image.rgb(0, 0), Some((30, 20, 10)));
            assert_eq!(image.rgb(1, 0), Some((255, 0, 0)));
            assert_eq!(image.rgb(2, 0), Some((15, 138, 5)));
        }
        let saved = Frame::load_png(&suggestion).unwrap();
        assert_eq!(saved.rgb(1, 0), Some((30, 20, 10)));
        assert_eq!(saved.rgb(2, 0), Some((30, 20, 10)));
        assert!(
            jiffshot_capture::library::read_meta(&suggestion)
                .unwrap()
                .cursor_included
        );
        let cursor =
            jiffshot_doc::io::load_image(&suggestion.with_extension("cursor.png")).unwrap();
        assert_eq!(cursor.pixel(1, 0).unwrap().alpha(), 128);
        let position: CursorPlacement = serde_json::from_slice(
            &std::fs::read(suggestion.with_extension("cursor.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(position, CursorPlacement::pixels((1, 0)));

        std::fs::remove_file(&source).unwrap();
        std::fs::remove_dir(source.parent().unwrap()).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cursor_clipboard_share_uses_the_flattened_frame_without_a_file_share() {
        use std::cell::RefCell;

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "jiffshot-cursor-clipboard-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let library = Library::new(root.clone());
        let frame = Frame::new(1, 1);
        let mut capture = from_frame(CaptureKind::Region, frame.bounds(), frame);
        capture.cursor = Some((
            Frame {
                width: 1,
                height: 1,
                pixels: vec![0, 0, 255, 255],
                alpha: true,
            },
            (0, 0),
        ));
        let settings = Settings {
            copy_to_clipboard: false,
            open_in_editor: false,
            share: "clipboard".to_owned(),
            ..Settings::default()
        };
        let copied = RefCell::new(None);
        let outcome = finish_in_library_with(
            capture,
            &settings,
            0,
            &library,
            |image, _| {
                *copied.borrow_mut() = Some(image.clone());
                Ok(())
            },
            |_, _, _, _| panic!("clipboard sharing must not use a temporary file"),
        );
        assert!(!outcome.is_error, "{}", outcome.message);
        assert_eq!(copied.into_inner().unwrap().rgb(0, 0), Some((255, 0, 0)));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_cursor_is_not_silently_omitted_from_delivery() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "jiffshot-cursor-invalid-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let library = Library::new(root.clone());
        let frame = Frame::new(1, 1);
        let mut capture = from_frame(CaptureKind::Region, frame.bounds(), frame);
        capture.cursor = Some((Frame::new(0, 0), (0, 0)));
        let settings = Settings {
            copy_to_clipboard: true,
            open_in_editor: false,
            share: "file".to_owned(),
            ..Settings::default()
        };
        let outcome = finish_in_library_with(
            capture,
            &settings,
            0,
            &library,
            |_, _| panic!("an unflattened capture must not reach the clipboard"),
            |_, _, _, _| panic!("an unflattened capture must not be shared"),
        );
        assert!(outcome.is_error);
        assert!(
            outcome
                .message
                .contains("could not prepare the editable pointer"),
            "{}",
            outcome.message
        );
        assert_eq!(library.recent(10).len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cursor_moves_with_border_and_shadow_effects() {
        use std::cell::RefCell;

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "jiffshot-cursor-effects-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let library = Library::new(root.clone());
        let frame = Frame::new(40, 20);
        let mut capture = from_frame(CaptureKind::Region, frame.bounds(), frame);
        capture.cursor = Some((
            Frame {
                width: 2,
                height: 2,
                pixels: vec![0, 0, 255, 255].repeat(4),
                alpha: true,
            },
            (3, 4),
        ));
        let settings = Settings {
            copy_to_clipboard: true,
            open_in_editor: false,
            capture_effects: vec!["Border".into(), "Shadow".into()],
            share: "none".into(),
            ..Settings::default()
        };
        let copied = RefCell::new(None);
        let outcome = finish_in_library_with(
            capture,
            &settings,
            0,
            &library,
            |image, _| {
                *copied.borrow_mut() = Some(image.clone());
                Ok(())
            },
            |_, _, _, _| panic!("sharing was not requested"),
        );
        assert!(!outcome.is_error, "{}", outcome.message);
        let path = library.recent(10).pop().unwrap();
        let position: CursorPlacement =
            serde_json::from_slice(&std::fs::read(path.with_extension("cursor.json")).unwrap())
                .unwrap();
        assert_eq!(position.geometry((2, 2)).unwrap(), [19.0, 20.0, 2.0, 2.0]);
        let image = copied.into_inner().unwrap();
        assert_eq!((image.width, image.height), (86, 66));
        assert_eq!(image.rgb(19, 20), Some((255, 0, 0)));
        let meta = jiffshot_capture::library::read_meta(&path).unwrap();
        assert_eq!((meta.width, meta.height), (image.width, image.height));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cursor_scale_effect_preserves_fractional_editor_geometry_in_delivery() {
        use std::cell::RefCell;

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "jiffshot-cursor-scale-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let library = Library::new(root.clone());
        let frame = Frame {
            width: 3840,
            height: 4,
            pixels: vec![10, 20, 30, 0].repeat(3840 * 4),
            alpha: false,
        };
        let mut capture = from_frame(CaptureKind::Region, frame.bounds(), frame);
        capture.cursor = Some((
            Frame {
                width: 2,
                height: 2,
                pixels: vec![0, 0, 255, 255].repeat(4),
                alpha: true,
            },
            (3, 1),
        ));
        let settings = Settings {
            copy_to_clipboard: true,
            open_in_editor: false,
            capture_effects: vec!["Scale to 1920".into()],
            share: "none".into(),
            ..Settings::default()
        };
        let copied = RefCell::new(None);
        let outcome = finish_in_library_with(
            capture,
            &settings,
            0,
            &library,
            |image, _| {
                *copied.borrow_mut() = Some(image.clone());
                Ok(())
            },
            |_, _, _, _| panic!("sharing was not requested"),
        );
        assert!(!outcome.is_error, "{}", outcome.message);
        let path = library.recent(10).pop().unwrap();
        let position: CursorPlacement =
            serde_json::from_slice(&std::fs::read(path.with_extension("cursor.json")).unwrap())
                .unwrap();
        let [x, y, width, height] = position.geometry((2, 2)).unwrap();
        assert_eq!([x, y, width, height], [1.5, 0.5, 1.0, 1.0]);
        let background = jiffshot_doc::Pixmap::load_png(&path).unwrap();
        let cursor = jiffshot_doc::Pixmap::load_png(&path.with_extension("cursor.png")).unwrap();
        let mut editor = jiffshot_doc::Document::new(background);
        editor.add(
            jiffshot_doc::object::Kind::Image {
                rect: jiffshot_doc::Box2::from_xywh(x, y, width, height),
                image: jiffshot_doc::object::ImageRef::new("cursor", cursor),
            },
            jiffshot_doc::object::Style {
                width: 0.0,
                ..Default::default()
            },
        );
        let expected = frame_from_pixmap(&editor.flatten());
        let copied = copied.into_inner().unwrap();
        assert_eq!((copied.width, copied.height), (1920, 2));
        assert_eq!(copied.pixels, expected.pixels);
        assert_ne!(copied.pixels, Frame::load_png(&path).unwrap().pixels);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn capture_without_a_cursor_shares_the_original_file() {
        use std::cell::RefCell;

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "jiffshot-no-cursor-share-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let library = Library::new(root.clone());
        let frame = Frame::new(1, 1);
        let capture = from_frame(CaptureKind::Region, frame.bounds(), frame);
        let settings = Settings {
            copy_to_clipboard: false,
            open_in_editor: false,
            share: "file".into(),
            ..Settings::default()
        };
        let shared = RefCell::new(None);
        let outcome = finish_in_library_with(
            capture,
            &settings,
            0,
            &library,
            |_, _| panic!("clipboard copying was not requested"),
            |_, source, suggestion, _| {
                assert_eq!(source, suggestion);
                *shared.borrow_mut() = Some(source.to_path_buf());
                Ok(None)
            },
        );
        assert!(!outcome.is_error, "{}", outcome.message);
        assert_eq!(shared.into_inner().unwrap(), library.recent(10)[0]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn flattened_file_share_cannot_replace_the_editable_capture() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "jiffshot-cursor-copy-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let source = root.join("flattened.png");
        let original = root.join("original.png");
        let hardlink = root.join("hardlink.png");
        let link = root.join("link.png");
        let pointer = original.with_extension("cursor.png");
        let pointer_link = root.join("pointer-link.png");
        std::fs::write(&source, b"flattened").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&original, b"background").unwrap();
        std::fs::write(&pointer, b"editable pointer").unwrap();
        std::fs::hard_link(&original, &hardlink).unwrap();
        symlink(&original, &link).unwrap();
        symlink(&pointer, &pointer_link).unwrap();
        for target in [
            &original,
            &hardlink,
            &link,
            &pointer,
            &pointer_link,
            &source,
            &original.with_extension("jsd"),
        ] {
            assert!(copy_flattened_share(&source, &original, target).is_err());
            assert_eq!(std::fs::read(&original).unwrap(), b"background");
            assert_eq!(std::fs::read(&pointer).unwrap(), b"editable pointer");
            assert_eq!(std::fs::read(&source).unwrap(), b"flattened");
        }
        assert!(!original.with_extension("jsd").exists());
        let target = root.join("copy.png");
        std::fs::write(&target, b"existing longer file").unwrap();
        copy_flattened_share(&source, &original, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"flattened");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
        for same in [&original, &hardlink, &link, &pointer, &pointer_link] {
            assert!(copy_flattened_share(&original, &original, same).is_err());
            assert_eq!(std::fs::read(&original).unwrap(), b"background");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn capture_effects_convert_straight_alpha_before_processing() {
        let frame = Frame {
            width: 1,
            height: 1,
            pixels: vec![30, 60, 120, 128],
            alpha: true,
        };
        let mut capture = from_frame(CaptureKind::Region, frame.bounds(), frame);
        assert!(apply_capture_effects(&mut capture, &["Invert".into()])
            .unwrap()
            .is_none());
        assert_eq!(capture.frame.pixels[3], 128);
        for (actual, expected) in capture.frame.pixels[..3].iter().zip([225i16, 195, 135]) {
            assert!(
                (i16::from(*actual) - expected).abs() <= 1,
                "{:?}",
                capture.frame.pixels
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cursor_capture_respects_the_editable_object_setting() {
        let cursor = CursorSnapshot::from_image(
            Frame {
                width: 2,
                height: 2,
                pixels: vec![0, 0, 255, 255].repeat(4),
                alpha: true,
            },
            (102, 50),
        )
        .unwrap();
        let mapping = CaptureMapping::new(Rect::new(100, 50, 8, 4), 4, 2).unwrap();
        let mut frame = Frame::new(4, 2);
        let original = frame.pixels.clone();
        let (included, separate) =
            capture_cursor(&mut frame, mapping, Some(&cursor), true, true).unwrap();
        let (pointer, position) = separate.unwrap();
        assert!(included);
        assert_eq!((pointer.width, pointer.height, position), (1, 1, (1, 0)));
        assert_eq!(frame.pixels, original);

        let (included, separate) =
            capture_cursor(&mut frame, mapping, Some(&cursor), true, false).unwrap();
        assert!(included && separate.is_none());
        assert_eq!(frame.rgb(1, 0), Some((255, 0, 0)));
        let mut frame = Frame::new(4, 2);
        let (included, separate) =
            capture_cursor(&mut frame, mapping, Some(&cursor), false, true).unwrap();
        assert!(!included && separate.is_none());
        assert_eq!(frame.pixels, original);
        assert!(CursorSnapshot::from_image(Frame::new(0, 0), (0, 0)).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn fractional_cursor_capture_matches_the_editable_object() {
        let cursor = CursorSnapshot::from_image_with_bounds(
            Frame {
                width: 2,
                height: 2,
                pixels: vec![0, 0, 255, 255].repeat(4),
                alpha: true,
            },
            [102.3, 50.625, 104.3, 52.625],
        )
        .unwrap();
        let mapping = CaptureMapping::with_desktop_bounds(
            Rect::new(100, 50, 8, 4),
            4,
            2,
            [100.25, 50.25, 108.25, 54.25],
        )
        .unwrap();
        let mut frame = Frame::new(4, 2);
        let (included, separate) =
            capture_cursor(&mut frame, mapping, Some(&cursor), true, true).unwrap();
        let (pointer, position) = separate.unwrap();
        assert!(included);
        assert_eq!((pointer.width, pointer.height, position), (1, 1, (1, 0)));
        assert_eq!(frame.rgb(1, 0), Some((0, 0, 0)));

        let (included, separate) =
            capture_cursor(&mut frame, mapping, Some(&cursor), true, false).unwrap();
        assert!(included && separate.is_none());
        assert_eq!(frame.rgb(1, 0), Some((255, 0, 0)));
        assert_eq!(frame.rgb(0, 0), Some((0, 0, 0)));
    }

    #[test]
    fn temporary_share_reports_a_flush_failure() {
        struct FailingFlush {
            flushed: bool,
        }
        impl Write for FailingFlush {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                self.flushed = true;
                Err(io::Error::other("simulated flush failure"))
            }
        }
        let mut output = FailingFlush { flushed: false };
        let error = encode_and_flush(&Frame::new(1, 1), &mut output).unwrap_err();
        assert!(output.flushed);
        assert!(error.to_string().contains("simulated flush failure"));
    }

    #[test]
    fn freehand_mask_uses_the_mapped_pixel_center() {
        let mapping = CaptureMapping::new(Rect::new(0, 0, 2, 2), 4, 4).unwrap();
        let mut frame = Frame::new(4, 4);
        let triangle = [(0, 0), (2, 0), (0, 2)];
        mask_frame(&mut frame, mapping, |x, y| inside(&triangle, x, y));
        let alpha = |x: usize, y: usize| frame.pixels[(y * frame.width as usize + x) * 4 + 3];
        assert!(frame.alpha);
        assert_eq!(alpha(0, 2), 255);
        assert_eq!(alpha(1, 2), 0);
        assert_eq!(alpha(0, 3), 0);
    }

    #[test]
    fn multi_area_mask_uses_desktop_coordinates_after_scaling() {
        let mapping = CaptureMapping::new(Rect::new(-4, 2, 8, 2), 4, 2).unwrap();
        let mut frame = Frame::new(4, 2);
        let regions = [Rect::new(-4, 2, 2, 2), Rect::new(2, 2, 2, 2)];
        mask_frame(&mut frame, mapping, |x, y| {
            regions
                .iter()
                .any(|r| r.contains(x.floor() as i32, y.floor() as i32))
        });
        let alphas: Vec<_> = frame.pixels.chunks_exact(4).map(|p| p[3]).collect();
        assert_eq!(alphas, [255, 0, 0, 255, 255, 0, 0, 255]);
    }
}
