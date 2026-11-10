//! The control server: lets `jiffshot-cli` (and through it scripts and AI
//! agents) drive the running Jiffshot over its per-user pipe (see the
//! `jiffshot-control` crate). It takes screenshots, records video, builds
//! step-by-step guides, and plays mouse and keyboard input into other apps,
//! finding what to click by window, OCR text or UI Automation name.
//!
//! Each request runs on its own thread; recordings and guides outlive
//! requests in the statics below. Commands and their arguments are listed
//! in `docs/cli.md`.

mod document;
mod find;
mod input;

use crate::{log, recorder, recording_marks, stepcap, workflow};
use input::Button;
use jiffshot_capture::library::{CaptureKind, Library};
use jiffshot_capture::media::VideoSpec;
use jiffshot_capture::{screen, Rect};
use jiffshot_control::{Error, ErrorCode, Request, Response};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

type Reply = Result<Value, Error>;

/// Commands with a one-line summary, reported by `status`.
pub const COMMANDS: &[(&str, &str)] = &[
    ("status", "server, protocol and what is running"),
    ("monitors", "displays and their rectangles"),
    (
        "windows",
        "visible top-level windows (filter by title, process, class)",
    ),
    ("cursor", "where the pointer is"),
    (
        "screenshot",
        "capture a screen, window, region or the desktop to PNG",
    ),
    (
        "capture.scroll",
        "bounded automatic vertical or horizontal scroll-and-stitch capture",
    ),
    (
        "record.start",
        "start recording a target to MP4; returns once frames flow",
    ),
    ("record.status", "the running recording (or the last one)"),
    ("record.pause", "pause the recording"),
    ("record.resume", "resume the recording"),
    (
        "record.options",
        "change cursor, click rings, camera and audio while recording",
    ),
    (
        "record.annotate",
        "add a live annotation to recorded frames",
    ),
    ("record.mark", "mark a kept or discarded recording range"),
    ("record.clear", "remove live recording annotations"),
    (
        "record.stop",
        "stop and finalize the MP4; returns its metadata",
    ),
    ("record.cancel", "stop and delete the recording"),
    ("steps.start", "begin a step-by-step guide"),
    (
        "steps.add",
        "add a step: the screen at a point, with a caption",
    ),
    ("steps.status", "the guide being built"),
    (
        "steps.finish",
        "lay out and save the guide (PNG and editable .jsd)",
    ),
    ("steps.cancel", "discard the guide"),
    ("click", "click a point, OCR text or UI Automation element"),
    ("move", "move the pointer"),
    ("drag", "drag from one point to another"),
    ("scroll", "turn the mouse wheel"),
    ("type", "type text"),
    ("key", "press a key combination such as ctrl+s"),
    ("focus", "restore and bring a window to the front"),
    ("ocr", "text on screen with its rectangles"),
    ("elements", "UI Automation controls in a window"),
    ("wait.window", "wait for a window to appear (or go away)"),
    (
        "wait.text",
        "wait for text to appear on screen (or go away)",
    ),
    ("open", "open a capture in the editor, or show the editor"),
    (
        "doc.tools",
        "annotation tools, variants, styles, effects and export formats",
    ),
    ("doc.list", "headless editable document sessions"),
    ("doc.new", "create a blank editable document"),
    (
        "doc.open",
        "load an image or editable .jsd into a document session",
    ),
    (
        "doc.status",
        "document objects, selection, size and undo/redo state",
    ),
    ("doc.add", "add a typed annotation object"),
    (
        "doc.draw",
        "use an editor tool with document-space pointer points",
    ),
    (
        "doc.update",
        "edit an annotation's geometry, style or rotation",
    ),
    ("doc.image", "insert an image into the document"),
    ("doc.select", "select objects by id"),
    (
        "doc.action",
        "undo, redo, grouping, stacking, alignment and transforms",
    ),
    ("doc.crop", "crop the image and annotations"),
    ("doc.resize", "resample or resize the canvas"),
    ("doc.effect", "apply a named image effect"),
    ("doc.ocr", "recognize text in the edited document"),
    (
        "doc.simplify",
        "replace text with bars and reduce image colors",
    ),
    (
        "doc.redact",
        "cover recognized sensitive patterns with editable redactions",
    ),
    (
        "doc.combine",
        "combine document images or build an editable template",
    ),
    ("doc.save", "save an editable .jsd document"),
    (
        "doc.export",
        "export flattened PNG, JPEG, BMP, GIF, WebP or PDF",
    ),
    ("doc.close", "release a document session"),
    (
        "video.info",
        "inspect metadata; optionally decode frames for an exact count and CFR",
    ),
    (
        "video.verify",
        "decode a saved video and report its verified frame count",
    ),
    ("video.frame", "extract a video frame to PNG"),
    (
        "video.export",
        "trim, fit, resize and annotate a video; export MP4 or GIF",
    ),
    (
        "video.join",
        "join and retime video segments with timed overlays",
    ),
    (
        "video.edit",
        "execute a JSON video edit plan and export MP4/GIF outputs",
    ),
    ("quit", "exit Jiffshot"),
];

/// Starts serving; logs and gives up when the pipe cannot be created
/// (another Jiffshot owns it).
pub fn start() {
    match jiffshot_control::serve(handle) {
        Ok(()) => log(&format!(
            "control: listening on {}",
            jiffshot_control::pipe_name().unwrap_or_default()
        )),
        Err(error) => log(&format!("control: not available: {error}")),
    }
}

fn handle(request: Request) -> Response {
    // SAFETY: COM (UI Automation) for this connection's thread.
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
    }
    let started = Instant::now();
    let reply = Args::new(request.args).and_then(|args| dispatch(&request.command, &args));
    let ms = started.elapsed().as_millis();
    match reply {
        Ok(result) => {
            log(&format!("control: {} ok ({ms} ms)", request.command));
            Response::ok(&request.id, result)
        }
        Err(error) => {
            log(&format!(
                "control: {} failed ({ms} ms): {error}",
                request.command
            ));
            Response::err(&request.id, error)
        }
    }
}

fn dispatch(command: &str, a: &Args) -> Reply {
    if matches!(
        command,
        "click" | "move" | "drag" | "scroll" | "type" | "key"
    ) {
        if let Some(recording) = RECORDING.lock().unwrap().as_ref() {
            recorder::note_activity(&recording.control).map_err(internal)?;
        }
    }
    match command {
        "status" => a.only(&[]).and_then(|_| status()),
        "monitors" => a.only(&[]).map(|_| monitors()),
        "windows" => list_windows(a),
        "cursor" => a.only(&[]).map(|_| point_json(input::cursor())),
        "screenshot" => screenshot(a),
        "capture.scroll" => capture_scroll(a),
        "record.start" => record_start(a),
        "record.status" => a.only(&[]).map(|_| record_status()),
        "record.pause" => record_pause(a, true),
        "record.resume" => record_pause(a, false),
        "record.options" => record_options(a),
        "record.annotate" => record_annotate(a),
        "record.mark" => record_mark(a),
        "record.clear" => record_clear(a),
        "record.stop" => record_stop(a, false),
        "record.cancel" => record_stop(a, true),
        "steps.start" => steps_start(a),
        "steps.add" => steps_add(a),
        "steps.status" => a.only(&[]).map(|_| steps_status()),
        "steps.finish" => steps_finish(a),
        "steps.cancel" => steps_cancel(a),
        "click" => click(a),
        "move" => move_pointer(a),
        "drag" => drag(a),
        "scroll" => scroll(a),
        "type" => type_text(a),
        "key" => key(a),
        "focus" => focus(a),
        "ocr" => ocr(a),
        "elements" => elements(a),
        "wait.window" => wait_window(a),
        "wait.text" => wait_text(a),
        "open" => open(a),
        "quit" => quit(a),
        command if command.starts_with("doc.") => document::dispatch(command, a),
        command if command.starts_with("video.") => {
            jiffshot_video::dispatch(command, Value::Object(a.0.clone()), &|| false)
        }
        _ => Err(Error::new(
            ErrorCode::Unsupported,
            format!("unknown command {command:?}; `status` lists the commands"),
        )),
    }
}

// Arguments ----------------------------------------------------------------

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidRequest, message)
}

fn not_found(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::NotFound, message)
}

fn internal(message: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::Internal, message.to_string())
}

/// A request's arguments: a JSON object, read with lenient types (the CLI
/// passes numbers and booleans as typed, scripts may pass strings).
struct Args(Map<String, Value>);

impl Args {
    fn new(value: Value) -> Result<Args, Error> {
        match value {
            Value::Null => Ok(Args(Map::new())),
            Value::Object(map) => Ok(Args(map)),
            _ => Err(invalid("arguments must be a JSON object")),
        }
    }

    /// Refuses keys other than `allowed`, so typos fail loudly.
    fn only(&self, allowed: &[&str]) -> Result<(), Error> {
        match self.0.keys().find(|k| !allowed.contains(&k.as_str())) {
            None => Ok(()),
            Some(k) if allowed.is_empty() => Err(invalid(format!(
                "unexpected argument {k:?}: this command takes none"
            ))),
            Some(k) => Err(invalid(format!(
                "unexpected argument {k:?}; expected one of: {}",
                allowed.join(", ")
            ))),
        }
    }

    fn has(&self, key: &str) -> bool {
        self.0.get(key).is_some_and(|v| !v.is_null())
    }

    fn string(&self, key: &str) -> Result<Option<String>, Error> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(v @ (Value::Number(_) | Value::Bool(_))) => Ok(Some(v.to_string())),
            Some(_) => Err(invalid(format!("{key} must be text"))),
        }
    }

    fn flag(&self, key: &str) -> Result<Option<bool>, Error> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(Value::String(s)) if matches!(s.as_str(), "true" | "1" | "yes") => Ok(Some(true)),
            Some(Value::String(s)) if matches!(s.as_str(), "false" | "0" | "no") => Ok(Some(false)),
            Some(Value::Number(n)) => Ok(Some(n.as_f64() != Some(0.0))),
            Some(_) => Err(invalid(format!("{key} must be true or false"))),
        }
    }

    fn bool(&self, key: &str) -> Result<bool, Error> {
        Ok(self.flag(key)?.unwrap_or(false))
    }

    fn number(&self, key: &str) -> Result<Option<f64>, Error> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(n)) => Ok(n.as_f64()),
            Some(Value::String(s)) => s
                .trim()
                .parse()
                .map(Some)
                .map_err(|_| invalid(format!("{key} must be a number, not {s:?}"))),
            Some(_) => Err(invalid(format!("{key} must be a number"))),
        }
    }

    fn int(&self, key: &str, min: i64, max: i64) -> Result<Option<i64>, Error> {
        match self.number(key)? {
            None => Ok(None),
            Some(n) if n.fract() == 0.0 && (min as f64..=max as f64).contains(&n) => {
                Ok(Some(n as i64))
            }
            Some(n) => Err(invalid(format!(
                "{key} must be a whole number from {min} to {max}, not {n}"
            ))),
        }
    }

    fn millis(&self, key: &str, default: u64) -> Result<Duration, Error> {
        Ok(Duration::from_millis(
            self.int(key, 0, 3_600_000)?.map_or(default, |n| n as u64),
        ))
    }

    /// A screen point: `"X,Y"`, `[X, Y]` or `{"x": X, "y": Y}`.
    fn point(&self, key: &str) -> Result<Option<(i32, i32)>, Error> {
        let bad = || invalid(format!("{key} must be a point X,Y in screen pixels"));
        let pair = |x: Option<f64>, y: Option<f64>| match (x, y) {
            (Some(x), Some(y))
                if x.is_finite()
                    && y.is_finite()
                    && (i32::MIN as f64..=i32::MAX as f64).contains(&x)
                    && (i32::MIN as f64..=i32::MAX as f64).contains(&y) =>
            {
                Ok(Some((x.round() as i32, y.round() as i32)))
            }
            _ => Err(bad()),
        };
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => {
                let parts: Vec<Option<f64>> = s.split(',').map(|p| p.trim().parse().ok()).collect();
                match parts.as_slice() {
                    [x, y] => pair(*x, *y),
                    _ => Err(bad()),
                }
            }
            Some(Value::Array(v)) if v.len() == 2 => pair(v[0].as_f64(), v[1].as_f64()),
            Some(Value::Object(o)) => pair(
                o.get("x").and_then(Value::as_f64),
                o.get("y").and_then(Value::as_f64),
            ),
            Some(_) => Err(bad()),
        }
    }

    /// An absolute output path whose folder exists (created if need be),
    /// with one of `extensions`.
    fn out(&self, key: &str, extensions: &[&str]) -> Result<Option<PathBuf>, Error> {
        let Some(text) = self.string(key)? else {
            return Ok(None);
        };
        let path = PathBuf::from(&text);
        if !path.is_absolute() {
            return Err(invalid(format!(
                "{key} must be an absolute path (the CLI makes paths absolute), not {text:?}"
            )));
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !extensions.contains(&ext.as_str()) {
            return Err(invalid(format!(
                "{key} must end in .{}",
                extensions.join(" or .")
            )));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| invalid(format!("cannot create {}: {e}", parent.display())))?;
        }
        Ok(Some(path))
    }
}

fn with(keys: &[&[&'static str]]) -> Vec<&'static str> {
    keys.concat()
}

fn point_json(p: (i32, i32)) -> Value {
    json!({ "x": p.0, "y": p.1 })
}

fn to_json(value: impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

// Targets ------------------------------------------------------------------

/// Arguments that pick what to capture or where to look.
const TARGET: &[&str] = &[
    "screen",
    "desktop",
    "region",
    "window",
    "title",
    "process",
    "class",
    "no_activate",
];

struct Target {
    rect: Rect,
    kind: CaptureKind,
    window: Option<find::Window>,
}

fn has_target(a: &Args) -> bool {
    TARGET.iter().any(|k| *k != "no_activate" && a.has(k))
}

fn window_query(a: &Args) -> Result<find::Query<'_>, Error> {
    let hwnd = match a.0.get("window") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => Some(
            n.as_i64()
                .ok_or_else(|| invalid("window must be a window handle"))? as isize,
        ),
        Some(Value::String(s)) => {
            let s = s.trim();
            let parsed = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                Some(hex) => i64::from_str_radix(hex, 16).ok(),
                None => s.parse().ok(),
            };
            Some(parsed.ok_or_else(|| {
                invalid(format!(
                    "window must be a window handle (decimal or 0x hex), not {s:?}"
                ))
            })? as isize)
        }
        Some(_) => return Err(invalid("window must be a window handle")),
    };
    let text = |k: &str| a.0.get(k).and_then(Value::as_str);
    Ok(find::Query {
        hwnd,
        title: text("title"),
        process: text("process"),
        class: text("class"),
        all: false,
    })
}

/// The window the target arguments name, brought to the front unless
/// `no_activate`.
fn target_window(a: &Args, activate: bool) -> Result<find::Window, Error> {
    let query = window_query(a)?;
    if query.is_empty() {
        return Err(invalid(
            "name a window with window, title, process or class",
        ));
    }
    if let Some(h) = query.hwnd {
        if !find::alive(h) {
            return Err(not_found(format!("no window {h:#x}")));
        }
    }
    let window = find::windows(&query)
        .into_iter()
        .next()
        .ok_or_else(|| not_found("no visible window matches; `windows` lists them"))?;
    if !activate || a.bool("no_activate")? {
        return Ok(window);
    }
    find::activate(window.hwnd);
    // Its bounds after restoring.
    let mut q = find::Query::default();
    q.hwnd = Some(window.hwnd);
    Ok(find::windows(&q).into_iter().next().unwrap_or(window))
}

fn monitor(spec: &Value) -> Result<screen::Monitor, Error> {
    let monitors = screen::monitors();
    let primary = || {
        monitors
            .iter()
            .find(|m| m.primary)
            .or(monitors.first())
            .cloned()
            .ok_or_else(|| internal("no displays"))
    };
    match spec {
        Value::String(s) if s.eq_ignore_ascii_case("primary") => primary(),
        Value::String(s) if s.eq_ignore_ascii_case("cursor") => {
            let p = input::cursor();
            screen::monitor_at(p.0, p.1).ok_or_else(|| internal("no display under the pointer"))
        }
        Value::Bool(true) => primary(),
        _ => {
            let index = match spec {
                Value::Number(n) => n.as_u64(),
                Value::String(s) => s.trim().parse().ok(),
                _ => None,
            }
            .ok_or_else(|| {
                invalid("screen must be a display index (from `monitors`), primary or cursor")
            })?;
            monitors.get(index as usize).cloned().ok_or_else(|| {
                not_found(format!("no display {index}; there are {}", monitors.len()))
            })
        }
    }
}

/// What the target arguments pick; `fallback` when there are none.
fn target(a: &Args, fallback: CaptureKind) -> Result<Target, Error> {
    let desktop = screen::virtual_screen();
    let clip = |r: Rect| {
        r.intersect(desktop)
            .filter(|r| !r.is_empty())
            .ok_or_else(|| invalid(format!("{r:?} is off screen")))
    };
    if let Some(region) = a.string("region")? {
        let rect = crate::parse_rect(&region)
            .ok_or_else(|| invalid(format!("region must be X,Y,W,H, not {region:?}")))?;
        return Ok(Target {
            rect: clip(rect)?,
            kind: CaptureKind::Region,
            window: None,
        });
    }
    if a.bool("desktop")? {
        return Ok(Target {
            rect: desktop,
            kind: CaptureKind::Desktop,
            window: None,
        });
    }
    if !window_query(a)?.is_empty() {
        let window = target_window(a, true)?;
        if window.minimized {
            return Err(Error::new(
                ErrorCode::Conflict,
                format!("window {:?} is minimized", window.title),
            ));
        }
        return Ok(Target {
            rect: clip(window.rect)?,
            kind: CaptureKind::Window,
            window: Some(window),
        });
    }
    if let Some(spec) = a.0.get("screen").filter(|v| !v.is_null()) {
        return Ok(Target {
            rect: monitor(spec)?.rect,
            kind: CaptureKind::Monitor,
            window: None,
        });
    }
    match fallback {
        CaptureKind::Desktop => Ok(Target {
            rect: desktop,
            kind: CaptureKind::Desktop,
            window: None,
        }),
        _ => Ok(Target {
            rect: monitor(&Value::Bool(true))?.rect,
            kind: CaptureKind::Monitor,
            window: None,
        }),
    }
}

fn target_json(t: &Target) -> Value {
    json!({ "rect": t.rect, "kind": t.kind, "window": t.window })
}

// Status -------------------------------------------------------------------

fn status() -> Reply {
    let commands: Map<String, Value> = COMMANDS
        .iter()
        .map(|(c, d)| (c.to_string(), json!(d)))
        .collect();
    Ok(json!({
        "app": "jiffshot",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": jiffshot_control::PROTOCOL,
        "pid": std::process::id(),
        "session": jiffshot_control::session_id(),
        "pipe": jiffshot_control::pipe_name().unwrap_or_default(),
        "library": jiffshot_capture::config::library_root(),
        "recording": record_status(),
        "steps": steps_status(),
        "commands": commands,
    }))
}

fn monitors() -> Value {
    let list: Vec<Value> = screen::monitors()
        .iter()
        .enumerate()
        .map(|(i, m)| json!({ "index": i, "device": m.device, "rect": m.rect, "work": m.work, "dpi": m.dpi, "scale": m.scale(), "primary": m.primary }))
        .collect();
    json!({ "monitors": list, "desktop": screen::virtual_screen() })
}

fn list_windows(a: &Args) -> Reply {
    a.only(&["window", "title", "process", "class", "all"])?;
    let mut query = window_query(a)?;
    query.all = a.bool("all")?;
    Ok(json!({ "windows": find::windows(&query) }))
}

// Screenshots --------------------------------------------------------------

fn screenshot(a: &Args) -> Reply {
    a.only(&with(&[TARGET, &["out", "cursor", "delay_ms"]]))?;
    let out = a.out("out", &["png"])?;
    std::thread::sleep(a.millis("delay_ms", 0)?);
    let t = target(a, CaptureKind::Monitor)?;
    let mut capture = workflow::immediate(t.kind, t.rect, a.bool("cursor")?).map_err(internal)?;
    if let Some(w) = &t.window {
        capture.meta.window_title = Some(w.title.clone());
        capture.meta.process = Some(w.process.clone());
    }
    let path = match out {
        Some(path) => {
            capture.frame.save_png(&path).map_err(internal)?;
            path
        }
        None => Library::new(jiffshot_capture::config::library_root())
            .save(&capture.frame, &capture.meta)
            .map_err(internal)?,
    };
    Ok(json!({
        "path": path,
        "width": capture.frame.width,
        "height": capture.frame.height,
        "captured_at": capture.meta.captured_at,
        "target": target_json(&t),
    }))
}

fn capture_scroll(a: &Args) -> Reply {
    a.only(&with(&[
        TARGET,
        &[
            "out",
            "horizontal",
            "max_frames",
            "timeout_ms",
            "settle_ms",
            "notches",
        ],
    ]))?;
    let out = a.out("out", &["png"])?;
    let horizontal = a.bool("horizontal")?;
    let max_frames = a.int("max_frames", 2, 128)?.unwrap_or(32) as usize;
    let timeout = a.millis("timeout_ms", 30_000)?;
    let settle = a.millis("settle_ms", 300)?;
    if timeout.is_zero() || settle.is_zero() {
        return Err(invalid("timeout_ms and settle_ms must be positive"));
    }
    let notches = a.int("notches", 1, 20)?.unwrap_or(3) as i32;
    let t = target(a, CaptureKind::Scrolling)?;
    let mut capture =
        workflow::immediate(CaptureKind::Scrolling, t.rect, false).map_err(internal)?;
    if let Some(w) = &t.window {
        capture.meta.window_title = Some(w.title.clone());
        capture.meta.process = Some(w.process.clone());
    }
    let pixels_per_frame = u64::from(capture.frame.width) * u64::from(capture.frame.height);
    if pixels_per_frame > 64_000_000 {
        return Err(invalid("scroll capture target exceeds 64 million pixels"));
    }
    let transform = |f: jiffshot_capture::Frame| {
        if horizontal {
            crate::modes::transpose(&f)
        } else {
            f
        }
    };
    let mut frames = vec![transform(capture.frame)];
    struct RestoreCursor((i32, i32));
    impl Drop for RestoreCursor {
        fn drop(&mut self) {
            let _ = input::move_to(self.0, Duration::ZERO);
        }
    }
    let _restore = RestoreCursor(input::cursor());
    if !input::move_to(t.rect.center(), Duration::ZERO) {
        return Err(internal(
            "could not position the cursor inside the scroll target",
        ));
    }
    if input::cursor() != t.rect.center() {
        return Err(internal(
            "could not position the cursor inside the scroll target",
        ));
    }
    std::thread::sleep(settle.min(timeout));
    frames[0] = transform(screen::grab(t.rect).map_err(internal)?);
    let started = Instant::now();
    let mut stopped = "frame_limit";
    while frames.len() < max_frames {
        if started.elapsed().saturating_add(settle) > timeout {
            stopped = "deadline";
            break;
        }
        if pixels_per_frame * (frames.len() as u64 + 1) > 64_000_000 {
            stopped = "pixel_limit";
            break;
        }
        if !input::scroll(if horizontal { notches } else { -notches }, horizontal) {
            return Err(internal("Windows rejected scroll input"));
        }
        std::thread::sleep(settle);
        let next = transform(screen::grab(t.rect).map_err(internal)?);
        let previous = frames
            .last()
            .ok_or_else(|| internal("no initial scroll frame"))?;
        if next.pixels == previous.pixels {
            stopped = "no_movement";
            break;
        }
        if jiffshot_capture::stitch::vertical_step(previous, &next).is_none() {
            return Err(invalid(
                "scroll frames have no matching overlap; use a content-only region, fewer notches or a longer settle_ms",
            ));
        }
        frames.push(next);
    }
    let frame_count = frames.len();
    let stitched = jiffshot_capture::stitch::stitch_vertical(&frames)
        .ok_or_else(|| internal("could not stitch scroll frames"))?;
    capture.frame = transform(stitched);
    capture.meta.width = capture.frame.width;
    capture.meta.height = capture.frame.height;
    let path = match out {
        Some(path) => {
            capture.frame.save_png(&path).map_err(internal)?;
            path
        }
        None => Library::new(jiffshot_capture::config::library_root())
            .save(&capture.frame, &capture.meta)
            .map_err(internal)?,
    };
    Ok(
        json!({"path":path,"width":capture.frame.width,"height":capture.frame.height,"frames":frame_count,
        "stopped":stopped,"limited":stopped != "no_movement","captured_at":capture.meta.captured_at,"target":target_json(&t)}),
    )
}

// Recording ----------------------------------------------------------------

struct Recording {
    id: String,
    key: Option<String>,
    path: PathBuf,
    target: Value,
    fps: u32,
    started_at: String,
    control: Arc<recorder::Control>,
    worker: JoinHandle<Result<VideoSpec, String>>,
}

static RECORDING: Mutex<Option<Recording>> = Mutex::new(None);
static LAST_RECORDING: Mutex<Option<Value>> = Mutex::new(None);
static RECORDING_LIFECYCLE: Mutex<()> = Mutex::new(());
static FINALIZING: AtomicBool = AtomicBool::new(false);

fn new_id(prefix: &str) -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    format!("{prefix}-{millis:x}")
}

fn recording_json(r: &Recording) -> Value {
    let c = &r.control;
    json!({
        "active": true,
        "id": r.id,
        "key": r.key,
        "path": r.path,
        "target": r.target,
        "fps": r.fps,
        "started_at": r.started_at,
        "frames": c.frames.load(Ordering::Relaxed),
        "duration_ms": c.millis.load(Ordering::Relaxed),
        "paused": c.paused.load(Ordering::Relaxed),
        // Ended by itself (max_seconds or an error): `record.stop` collects it.
        "ended": r.worker.is_finished(),
        "error": c.error.lock().unwrap().clone(),
        "audio_note": c.audio.lock().unwrap().clone(),
        "system_silent": c.system_silent.load(Ordering::Relaxed),
        "microphone_silent": c.microphone_silent.load(Ordering::Relaxed),
        "options": {
            "cursor": c.cursor.load(Ordering::Relaxed), "highlight": c.highlight.load(Ordering::Relaxed),
            "clicks": c.clicks.load(Ordering::Relaxed), "webcam": c.webcam.load(Ordering::Relaxed),
            "webcam_circle": c.webcam_circle.load(Ordering::Relaxed),
            "webcam_corner": c.webcam_corner.lock().unwrap().clone(),
            "system": c.system.load(Ordering::Relaxed), "microphone": c.microphone.load(Ordering::Relaxed),
        },
    })
}

fn record_status() -> Value {
    match &*RECORDING.lock().unwrap() {
        Some(r) => recording_json(r),
        None => {
            json!({ "active": false, "finalizing": FINALIZING.load(Ordering::Acquire), "tray_recording": recorder::is_recording() && !FINALIZING.load(Ordering::Acquire), "last": *LAST_RECORDING.lock().unwrap() })
        }
    }
}

fn record_start(a: &Args) -> Reply {
    a.only(&with(&[
        TARGET,
        &[
            "out",
            "fps",
            "audio",
            "no_cursor",
            "clicks",
            "highlight",
            "max_seconds",
            "key",
            "webcam",
            "webcam_corner",
            "webcam_circle",
            "webcam_size",
            "noise_gate",
        ],
    ]))?;
    let _lifecycle = RECORDING_LIFECYCLE
        .lock()
        .expect("recording lifecycle lock poisoned");
    let key = a.string("key")?;
    {
        let current = RECORDING.lock().unwrap();
        if let Some(r) = current.as_ref() {
            // The same start again (a retry after a lost reply): not a second recording.
            if key.is_some() && r.key == key {
                let mut v = recording_json(r);
                v["already_started"] = json!(true);
                return Ok(v);
            }
            return Err(Error::new(
                ErrorCode::Conflict,
                format!("recording {} is already running; stop it first", r.id),
            ));
        }
    }
    if recorder::is_recording() {
        return Err(Error::new(
            ErrorCode::Conflict,
            "a tray recording is running; finish it before starting a CLI recording",
        ));
    }
    let audio = a.string("audio")?.unwrap_or_else(|| "none".into());
    let audio = if audio == "mic" {
        "microphone".to_string()
    } else {
        audio
    };
    if !matches!(audio.as_str(), "none" | "system" | "microphone" | "both") {
        return Err(invalid("audio must be none, system, microphone or both"));
    }
    let fps = a.int("fps", 5, 60)?.unwrap_or(30) as u32;
    let max = a
        .number("max_seconds")?
        .map(|s| {
            if !s.is_finite() || !(0.001..=86_400.0).contains(&s) {
                Err(invalid(
                    "max_seconds must be finite and between 0.001 and 86400",
                ))
            } else {
                Ok(Duration::from_secs_f64(s))
            }
        })
        .transpose()?;
    let path = match a.out("out", &["mp4"])? {
        Some(p) => p,
        None => Library::new(jiffshot_capture::config::library_root())
            .recording_path()
            .map_err(internal)?,
    };
    let t = target(a, CaptureKind::Monitor)?;
    if path.exists() {
        return Err(Error::new(
            ErrorCode::Conflict,
            "recording output already exists; choose a new path",
        ));
    }
    if recording_marks::sidecar_path(&path).exists() {
        return Err(Error::new(
            ErrorCode::Conflict,
            "recording marker metadata already exists; choose a new output path",
        ));
    }
    let mut options = recorder::Options::audio_only(fps, &audio);
    options.cursor = !a.bool("no_cursor")?;
    options.click_rings = a.bool("clicks")?;
    options.cursor_highlight = a.bool("highlight")?;
    let corner = a
        .string("webcam_corner")?
        .unwrap_or_else(|| "bottom-right".into());
    validate_corner(&corner)?;
    options.webcam_corner = corner.clone();
    options.webcam = a.bool("webcam")?.then_some(corner);
    options.webcam_circle = a.bool("webcam_circle")?;
    let size = a.number("webcam_size")?.unwrap_or(0.25);
    if !size.is_finite() || !(0.05..=0.75).contains(&size) {
        return Err(invalid(
            "webcam_size must be 0.05..0.75 of the recording width",
        ));
    }
    options.webcam_size = size as f32;
    options.noise_gate = a.bool("noise_gate")?;
    let control = Arc::new(recorder::Control::new(&options));
    let (c, p, rect) = (control.clone(), path.clone(), t.rect);
    let worker = std::thread::Builder::new()
        .name("jiffshot-control-record".into())
        .spawn(move || recorder::record_to(&p, rect, &options, &c))
        .map_err(internal)?;
    // Ready once frames are being written, so the caller can start acting.
    let started = Instant::now();
    while control.frames.load(Ordering::Relaxed) == 0 && !worker.is_finished() {
        if started.elapsed() > Duration::from_secs(10) {
            control.stop.store(true, Ordering::Release);
            let _ = worker.join();
            let _ = std::fs::remove_file(&path);
            return Err(Error::new(
                ErrorCode::Timeout,
                "the recording did not start within 10 s",
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if worker.is_finished() {
        let error = match worker.join() {
            Ok(Err(e)) => e,
            _ => "the recorder stopped at once".into(),
        };
        let _ = std::fs::remove_file(&path);
        return Err(internal(format!("could not record: {error}")));
    }
    let recording = Recording {
        id: new_id("rec"),
        key,
        path,
        target: target_json(&t),
        fps,
        started_at: jiffshot_capture::library::timestamp(),
        control,
        worker,
    };
    if let Some(max) = max {
        let c = recording.control.clone();
        std::thread::spawn(move || {
            std::thread::sleep(max);
            c.stop.store(true, Ordering::Release);
        });
    }
    let reply = recording_json(&recording);
    *RECORDING.lock().unwrap() = Some(recording);
    Ok(reply)
}

fn current_recording<'a>(
    slot: &'a mut Option<Recording>,
    a: &Args,
) -> Result<&'a mut Recording, Error> {
    let id = a.string("id")?;
    match slot {
        None => Err(not_found("no recording is running")),
        Some(r) if id.as_ref().is_some_and(|id| *id != r.id) => {
            Err(not_found(format!("recording {} is the one running", r.id)))
        }
        Some(r) => Ok(r),
    }
}

fn record_pause(a: &Args, pause: bool) -> Reply {
    a.only(&["id"])?;
    let mut slot = RECORDING.lock().unwrap();
    let r = current_recording(&mut slot, a)?;
    require_live_recording(r)?;
    r.control.paused.store(pause, Ordering::Release);
    Ok(recording_json(r))
}

fn require_live_recording(r: &Recording) -> Result<(), Error> {
    if r.worker.is_finished() {
        Err(Error::new(
            ErrorCode::Conflict,
            "recording has ended; use record.stop to collect the finalized result",
        ))
    } else {
        Ok(())
    }
}

fn validate_corner(corner: &str) -> Result<(), Error> {
    if matches!(
        corner,
        "top-left" | "top-right" | "bottom-left" | "bottom-right"
    ) {
        Ok(())
    } else {
        Err(invalid(
            "webcam_corner must be top-left, top-right, bottom-left or bottom-right",
        ))
    }
}

fn record_options(a: &Args) -> Reply {
    a.only(&[
        "id",
        "cursor",
        "highlight",
        "clicks",
        "webcam",
        "webcam_circle",
        "webcam_corner",
        "system",
        "microphone",
    ])?;
    // Validate everything before changing any switch.
    let switches = [
        "cursor",
        "highlight",
        "clicks",
        "webcam",
        "webcam_circle",
        "system",
        "microphone",
    ]
    .iter()
    .map(|k| a.flag(k))
    .collect::<Result<Vec<_>, _>>()?;
    let corner = a.string("webcam_corner")?;
    if let Some(corner) = &corner {
        validate_corner(corner)?;
    }
    let mut slot = RECORDING.lock().expect("recording lock poisoned");
    let r = current_recording(&mut slot, a)?;
    require_live_recording(r)?;
    let c = &r.control;
    for (flag, value) in [
        &c.cursor,
        &c.highlight,
        &c.clicks,
        &c.webcam,
        &c.webcam_circle,
        &c.system,
        &c.microphone,
    ]
    .into_iter()
    .zip(switches)
    {
        if let Some(value) = value {
            flag.store(value, Ordering::Release);
        }
    }
    if let Some(corner) = corner {
        *c.webcam_corner.lock().expect("webcam corner lock poisoned") = corner;
    }
    Ok(recording_json(r))
}

fn record_annotate(a: &Args) -> Reply {
    a.only(&["id", "kind", "style", "duration_ms"])?;
    let kind = document::kind(
        a.0.get("kind")
            .ok_or_else(|| invalid("kind is required; use the same schema as doc.add"))?,
    )?;
    let style = document::style(a.0.get("style"), jiffshot_doc::object::Style::default())?;
    let duration = a.millis("duration_ms", 0)?;
    let mut slot = RECORDING.lock().expect("recording lock poisoned");
    let r = current_recording(&mut slot, a)?;
    require_live_recording(r)?;
    let mark = recorder::add_annotation(&r.control, kind, style, duration).map_err(invalid)?;
    Ok(
        json!({"id": r.id, "annotation_id": mark, "coordinates": "recording pixels", "duration_ms": duration.as_millis()}),
    )
}

fn record_clear(a: &Args) -> Reply {
    a.only(&["id"])?;
    let mut slot = RECORDING.lock().expect("recording lock poisoned");
    let r = current_recording(&mut slot, a)?;
    require_live_recording(r)?;
    recorder::clear_annotations(&r.control).map_err(internal)?;
    Ok(json!({"id": r.id, "cleared": true}))
}

fn record_mark(a: &Args) -> Reply {
    a.only(&["id", "label", "discard"])?;
    let label = a.string("label")?;
    let discard = a.bool("discard")?;
    let mut slot = RECORDING.lock().unwrap();
    let recording = current_recording(&mut slot, a)?;
    require_live_recording(recording)?;
    if label.is_some() == discard {
        return Err(invalid("provide exactly one of label or discard=true"));
    }
    let at_ms =
        recorder::add_marker(&recording.control, label.as_deref(), discard).map_err(invalid)?;
    Ok(json!({
        "id": recording.id,
        "at_ms": at_ms,
        "label": label,
        "discard": discard
    }))
}

/// Stops the recording and waits until the MP4 is finalized (or deleted).
fn record_stop(a: &Args, cancel: bool) -> Reply {
    a.only(&["id"])?;
    let _lifecycle = RECORDING_LIFECYCLE
        .lock()
        .expect("recording lifecycle lock poisoned");
    struct Finalizing;
    impl Drop for Finalizing {
        fn drop(&mut self) {
            FINALIZING.store(false, Ordering::Release);
        }
    }
    let recording = {
        let mut slot = RECORDING.lock().unwrap();
        current_recording(&mut slot, a)?;
        FINALIZING.store(true, Ordering::Release);
        slot.take().unwrap()
    };
    let _finalizing = Finalizing;
    let Recording {
        id,
        key,
        path,
        target,
        fps,
        started_at,
        control,
        worker,
    } = recording;
    control.paused.store(false, Ordering::Release);
    control.stop.store(true, Ordering::Release);
    let outcome = worker
        .join()
        .unwrap_or_else(|_| Err("the recorder crashed".into()));
    let (frames, duration) = (
        control.frames.load(Ordering::Relaxed),
        control.millis.load(Ordering::Relaxed),
    );
    if cancel {
        if path.exists() {
            std::fs::remove_file(&path).map_err(internal)?;
        }
        return Ok(
            json!({ "id": id, "cancelled": true, "frames": frames, "duration_ms": duration }),
        );
    }
    let spec = outcome.map_err(|e| internal(format!("the recording failed: {e}")))?;
    let markers = control
        .markers
        .lock()
        .map_err(|_| {
            internal(format!(
                "recording saved to {}, but markers are unavailable",
                path.display()
            ))
        })?
        .clone();
    let captions = control
        .captions
        .lock()
        .map_err(|_| {
            internal(format!(
                "recording saved to {}, but captions are unavailable",
                path.display()
            ))
        })?
        .clone();
    let activities = control
        .activities
        .lock()
        .map_err(|_| {
            internal(format!(
                "recording saved to {}, but activity times are unavailable",
                path.display()
            ))
        })?
        .clone();
    recording_marks::write(
        &path,
        &recording_marks::RecordingMarks::new(duration, fps, spec.width, spec.height, markers)
            .with_captions(captions)
            .with_activities(activities)
            .with_activity_tracking_complete(
                !control.activity_tracking_overflowed.load(Ordering::Acquire)
                    && !control
                        .activity_tracking_unavailable
                        .load(Ordering::Acquire),
            ),
    )
    .map_err(|error| {
        internal(format!(
            "recording saved to {}, but marker metadata could not be saved: {error}",
            path.display()
        ))
    })?;
    let result = json!({
        "id": id,
        "key": key,
        "path": path,
        "target": target,
        "width": spec.width,
        "height": spec.height,
        "fps": fps,
        "frames": frames,
        "duration_ms": duration,
        "markers": control.markers.lock().unwrap().len(),
        "size_bytes": std::fs::metadata(&path).map_err(internal)?.len(),
        "started_at": started_at,
        "audio_note": control.audio.lock().unwrap().clone(),
        "system_silent": control.system_silent.load(Ordering::Relaxed),
        "microphone_silent": control.microphone_silent.load(Ordering::Relaxed),
    });
    *LAST_RECORDING.lock().unwrap() = Some(result.clone());
    Ok(result)
}

// Step-by-step guides ------------------------------------------------------

struct Guide {
    id: String,
    // Guide IDs can repeat within a millisecond.
    generation: u64,
    title: String,
    steps: Vec<stepcap::Step>,
}

static GUIDE: Mutex<Option<Guide>> = Mutex::new(None);
static NEXT_GUIDE_GENERATION: AtomicU64 = AtomicU64::new(1);

fn guide_summary(guide: Option<&Guide>) -> Value {
    match guide {
        None => json!({ "active": false }),
        Some(g) => json!({
            "active": true,
            "id": g.id,
            "title": g.title,
            "steps": g.steps.iter().enumerate().map(|(i, s)| json!({ "index": i + 1, "description": s.description, "at": point_json(s.click) })).collect::<Vec<_>>(),
        }),
    }
}

fn steps_status() -> Value {
    guide_summary(GUIDE.lock().unwrap().as_ref())
}

fn steps_start(a: &Args) -> Reply {
    start_guide(&GUIDE, a)
}

fn start_guide(slot: &Mutex<Option<Guide>>, a: &Args) -> Reply {
    a.only(&["title", "restart"])?;
    let mut guide = slot.lock().unwrap();
    if let Some(g) = guide.as_ref() {
        if !a.bool("restart")? {
            return Err(Error::new(
                ErrorCode::Conflict,
                format!(
                    "guide {} is in progress ({} steps); finish, cancel or restart it",
                    g.id,
                    g.steps.len()
                ),
            ));
        }
    }
    let title = a.string("title")?.unwrap_or_else(|| "How to …".into());
    *guide = Some(Guide {
        id: new_id("guide"),
        generation: NEXT_GUIDE_GENERATION.fetch_add(1, Ordering::Relaxed),
        title,
        steps: Vec::new(),
    });
    Ok(guide_summary(guide.as_ref()))
}

fn capture_step(
    at: (i32, i32),
    right: bool,
    description: Option<String>,
) -> Result<stepcap::Step, Error> {
    let area = screen::monitor_at(at.0, at.1)
        .map(|m| m.rect)
        .ok_or_else(|| invalid(format!("{at:?} is off screen")))?;
    let frame = screen::grab(area).map_err(internal)?;
    let description = description
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| find::describe_click(at, right));
    Ok(stepcap::Step {
        frame,
        area,
        mapping: None,
        click: at,
        right,
        description,
    })
}

fn record_step_caption(description: &str) -> Result<(), Error> {
    let slot = RECORDING
        .lock()
        .map_err(|_| internal("the recording state is unavailable"))?;
    if let Some(recording) = slot.as_ref() {
        require_live_recording(recording)?;
        recorder::add_step_caption(&recording.control, description).map_err(invalid)?;
    }
    Ok(())
}

fn push_step(guide: &mut Guide, step: stepcap::Step) -> Value {
    let at = step.click;
    let description = step.description.clone();
    guide.steps.push(step);
    json!({ "index": guide.steps.len(), "description": description, "at": point_json(at) })
}

fn guide_generation(slot: &Mutex<Option<Guide>>) -> Result<Option<u64>, Error> {
    let slot = slot
        .lock()
        .map_err(|_| internal("the guide state is unavailable"))?;
    Ok(slot.as_ref().map(|guide| guide.generation))
}

fn captured_guide(slot: &mut Option<Guide>, generation: u64) -> Result<&mut Guide, Error> {
    match slot.as_mut() {
        Some(guide) if guide.generation == generation => Ok(guide),
        _ => Err(Error::new(
            ErrorCode::Conflict,
            "the guide changed while capturing the step",
        )),
    }
}

fn add_step_with(
    slot: &Mutex<Option<Guide>>,
    capture: impl FnOnce() -> Result<stepcap::Step, Error>,
    record_caption: impl FnOnce(&str) -> Result<(), Error>,
) -> Result<Value, Error> {
    let generation = guide_generation(slot)?
        .ok_or_else(|| not_found("no guide is in progress; run steps.start first"))?;
    let step = capture()?;
    let mut slot = slot
        .lock()
        .map_err(|_| internal("the guide state is unavailable"))?;
    let guide = captured_guide(&mut slot, generation)?;
    record_caption(&step.description)?;
    Ok(push_step(guide, step))
}

/// Adds a step at `at`: the display there as it looks now, captioned with
/// `description` (default: UI Automation's name of the control there).
fn add_step(at: (i32, i32), right: bool, description: Option<String>) -> Result<Value, Error> {
    add_step_with(
        &GUIDE,
        || capture_step(at, right, description),
        record_step_caption,
    )
}

fn click_with_step_and_capture(
    slot: &Mutex<Option<Guide>>,
    requested: Option<bool>,
    capture: impl FnOnce() -> Result<stepcap::Step, Error>,
    perform: impl FnOnce() -> Result<(), Error>,
    record_caption: impl FnOnce(&str) -> Result<(), Error>,
) -> Result<Option<Value>, Error> {
    if requested == Some(false) {
        perform()?;
        return Ok(None);
    }
    let Some(generation) = guide_generation(slot)? else {
        if requested == Some(true) {
            return Err(not_found(
                "step needs a guide in progress; run steps.start first",
            ));
        }
        perform()?;
        return Ok(None);
    };
    let step = capture()?;
    let mut slot = slot
        .lock()
        .map_err(|_| internal("the guide state is unavailable"))?;
    let guide = captured_guide(&mut slot, generation)?;
    perform()?;
    let description = step.description.clone();
    let result = push_step(guide, step);
    record_caption(&description).map_err(|error| {
        Error::new(
            error.code,
            format!(
                "the click and guide step succeeded, but recording the step caption failed: {}",
                error.message
            ),
        )
    })?;
    Ok(Some(result))
}

fn steps_add(a: &Args) -> Reply {
    a.only(&["at", "right", "description"])?;
    let at = a.point("at")?.unwrap_or_else(input::cursor);
    add_step(at, a.bool("right")?, a.string("description")?)
}

fn finish_guide_with<T>(
    slot: &Mutex<Option<Guide>>,
    finish: impl FnOnce(&Guide) -> Result<T, Error>,
) -> Result<T, Error> {
    let mut slot = slot
        .lock()
        .map_err(|_| internal("the guide state is unavailable"))?;
    let guide = slot
        .as_ref()
        .ok_or_else(|| not_found("no guide is in progress"))?;
    if guide.steps.is_empty() {
        return Err(Error::new(
            ErrorCode::Conflict,
            "the guide has no steps yet",
        ));
    }
    let result = finish(guide)?;
    *slot = None;
    Ok(result)
}

fn save_guide(
    guide: &Guide,
    out: Option<PathBuf>,
    title: Option<String>,
    write_document: impl FnOnce(&Path, &[u8]) -> jiffshot_doc::io::Result<()>,
) -> Result<(Value, PathBuf), Error> {
    if let Some(path) = &out {
        let editable = path.with_extension(jiffshot_capture::library::EDITS_EXT);
        if editable.is_dir() {
            return Err(internal(format!(
                "the editable guide path is a directory: {}",
                editable.display()
            )));
        }
    }
    let title = title.unwrap_or_else(|| guide.title.clone());
    let doc = stepcap::guide_titled(&guide.steps, &title).map_err(internal)?;
    let document_bytes = jiffshot_doc::io::document_bytes(&doc).map_err(internal)?;
    let frame = workflow::frame_from_pixmap(&doc.flatten());
    let rect = Rect::new(0, 0, frame.width as i32, frame.height as i32);
    let capture = workflow::from_frame(CaptureKind::Steps, rect, frame);
    let path = match out {
        Some(path) => {
            let png = capture.frame.encode_png_vec().map_err(internal)?;
            jiffshot_doc::io::write_atomic(&path, &png).map_err(internal)?;
            path
        }
        None => Library::new(jiffshot_capture::config::library_root())
            .save(&capture.frame, &capture.meta)
            .map_err(internal)?,
    };
    let document = path.with_extension(jiffshot_capture::library::EDITS_EXT);
    write_document(&document, &document_bytes).map_err(|error| {
        internal(format!(
            "the guide image was saved to {}, but its editable document could not be saved: {error}",
            path.display()
        ))
    })?;
    let result = json!({
        "id": guide.id,
        "title": title,
        "path": &path,
        "document": document,
        "width": capture.frame.width,
        "height": capture.frame.height,
        "opened": false,
        "steps": guide.steps.iter().enumerate().map(|(i, s)| json!({ "index": i + 1, "description": s.description, "at": point_json(s.click) })).collect::<Vec<_>>(),
    });
    Ok((result, path))
}

fn steps_finish(a: &Args) -> Reply {
    a.only(&["out", "title", "open"])?;
    let out = a.out("out", &["png"])?;
    let title = a.string("title")?;
    let open = a.bool("open")?;
    let (mut result, path) = finish_guide_with(&GUIDE, |guide| {
        save_guide(guide, out, title, jiffshot_doc::io::write_atomic)
    })?;
    result["opened"] =
        json!(open && crate::hooks().is_some_and(|hooks| (hooks.open_in_editor)(&path)));
    Ok(result)
}

fn steps_cancel(a: &Args) -> Reply {
    a.only(&[])?;
    let guide = GUIDE
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| not_found("no guide is in progress"))?;
    Ok(json!({ "id": guide.id, "cancelled": true, "steps": guide.steps.len() }))
}

// Input --------------------------------------------------------------------

/// One input sequence at a time, so concurrent requests do not interleave.
static INPUT: Mutex<()> = Mutex::new(());

const POINTER: &[&str] = &["at", "text", "element", "type", "nth", "offset"];

fn recording() -> bool {
    RECORDING.lock().unwrap().is_some()
}

/// Pointer moves glide while recording, so the video shows them.
fn glide(a: &Args) -> Result<Duration, Error> {
    a.millis("glide_ms", if recording() { 400 } else { 0 })
}

fn snippet(texts: impl Iterator<Item = String>) -> String {
    let mut s = String::new();
    for t in texts {
        if s.len() > 400 {
            s.push_str(" …");
            break;
        }
        if !s.is_empty() {
            s.push_str(" | ");
        }
        s.push_str(&t);
    }
    s
}

/// Where the pointer arguments point: `at`, OCR `text` or UI Automation
/// `element` (in the target window), then `offset`. Returns the point and
/// what was matched.
fn locate(a: &Args) -> Result<((i32, i32), Value), Error> {
    let nth = a.int("nth", 1, 1000)?.unwrap_or(1) as usize;
    let offset = a.point("offset")?.unwrap_or((0, 0));
    let shift = |p: (i32, i32)| (p.0 + offset.0, p.1 + offset.1);
    if let Some(p) = a.point("at")? {
        return Ok((shift(p), Value::Null));
    }
    if let Some(phrase) = a.string("text")? {
        let area = if has_target(a) {
            target(a, CaptureKind::Monitor)?.rect
        } else {
            screen::virtual_screen()
        };
        let lines = find::read(area).map_err(internal)?;
        let found = find::find_text(&lines, &phrase);
        let hit = found.get(nth - 1).ok_or_else(|| {
            not_found(format!(
                "text {phrase:?} not found ({} matches); on screen: {}",
                found.len(),
                snippet(lines.iter().map(|l| l.text.clone()))
            ))
        })?;
        return Ok((shift(hit.center), json!({ "text": hit })));
    }
    if let Some(name) = a.string("element")? {
        let window = target_window(a, true).map_err(|e| {
            if e.code == ErrorCode::InvalidRequest {
                invalid("element needs a window: give window, title, process or class")
            } else {
                e
            }
        })?;
        let all = find::elements(window.hwnd, 5000).map_err(internal)?;
        let names = snippet(
            all.iter()
                .filter(|e| !e.name.is_empty())
                .map(|e| format!("{} ({})", e.name, e.control_type)),
        );
        let control_type = a.string("type")?;
        let found = find::find_elements(all, &name, control_type.as_deref());
        let hit = found.get(nth - 1).ok_or_else(|| {
            not_found(format!(
                "element {name:?} not found ({} matches); controls: {names}",
                found.len()
            ))
        })?;
        return Ok((shift(hit.center), json!({ "element": hit })));
    }
    Err(invalid(
        "give at (X,Y), text (to find by OCR) or element (a UI Automation name)",
    ))
}

fn button(a: &Args) -> Result<Button, Error> {
    if a.bool("right")? {
        return Ok(Button::Right);
    }
    if a.bool("middle")? {
        return Ok(Button::Middle);
    }
    match a.string("button")? {
        None => Ok(Button::Left),
        Some(b) => Button::parse(&b).ok_or_else(|| invalid("button must be left, right or middle")),
    }
}

fn click(a: &Args) -> Reply {
    a.only(&with(&[
        TARGET,
        POINTER,
        &[
            "button",
            "right",
            "middle",
            "double",
            "count",
            "step",
            "description",
            "glide_ms",
            "settle_ms",
        ],
    ]))?;
    let _input = INPUT.lock().unwrap();
    let (at, matched) = locate(a)?;
    let button = button(a)?;
    let count = if a.bool("double")? {
        2
    } else {
        a.int("count", 1, 3)?.unwrap_or(1) as u32
    };
    let requested = a.flag("step")?;
    let glide = glide(a)?;
    // The step shows the screen as it is about to be clicked, pointer included.
    let step = click_with_step_and_capture(
        &GUIDE,
        requested,
        || {
            if !input::move_to(at, glide) {
                return Err(internal("Windows could not position the cursor"));
            }
            capture_step(at, button == Button::Right, a.string("description")?)
        },
        || {
            if !input::click(at, button, count, glide) {
                return Err(internal(
                    "Windows could not complete the input; it may have partially occurred (is a higher-privileged window in front?)",
                ));
            }
            Ok(())
        },
        record_step_caption,
    )?;
    std::thread::sleep(a.millis("settle_ms", 250)?);
    Ok(
        json!({ "at": point_json(at), "button": format!("{button:?}").to_lowercase(), "count": count, "matched": matched, "step": step }),
    )
}

fn move_pointer(a: &Args) -> Reply {
    a.only(&with(&[TARGET, POINTER, &["glide_ms"]]))?;
    let _input = INPUT.lock().unwrap();
    let (at, matched) = locate(a)?;
    if !input::move_to(at, glide(a)?) {
        return Err(internal("Windows could not position the cursor"));
    }
    Ok(json!({ "at": point_json(at), "matched": matched }))
}

fn drag(a: &Args) -> Reply {
    a.only(&[
        "from",
        "to",
        "button",
        "right",
        "middle",
        "glide_ms",
        "settle_ms",
    ])?;
    let _input = INPUT.lock().unwrap();
    let from = a
        .point("from")?
        .ok_or_else(|| invalid("from is required"))?;
    let to = a.point("to")?.ok_or_else(|| invalid("to is required"))?;
    if !input::drag(from, to, button(a)?, a.millis("glide_ms", 500)?) {
        return Err(internal("Windows refused the input"));
    }
    std::thread::sleep(a.millis("settle_ms", 250)?);
    Ok(json!({ "from": point_json(from), "to": point_json(to) }))
}

fn scroll(a: &Args) -> Reply {
    a.only(&with(&[
        TARGET,
        POINTER,
        &["amount", "horizontal", "glide_ms", "settle_ms"],
    ]))?;
    let _input = INPUT.lock().unwrap();
    let amount = a
        .int("amount", -1000, 1000)?
        .ok_or_else(|| invalid("amount is required: notches, positive scrolls down (or right)"))?
        as i32;
    let horizontal = a.bool("horizontal")?;
    let at = if a.has("at") || a.has("text") || a.has("element") {
        let (at, _) = locate(a)?;
        if !input::move_to(at, glide(a)?) {
            return Err(internal("Windows could not position the cursor"));
        }
        at
    } else {
        if has_target(a) {
            let t = target(a, CaptureKind::Monitor)?;
            let center = (t.rect.x + t.rect.width / 2, t.rect.y + t.rect.height / 2);
            if !input::move_to(center, glide(a)?) {
                return Err(internal("Windows could not position the cursor"));
            }
        }
        input::cursor()
    };
    // The wheel scrolls up for positive notches (and right when horizontal).
    if !input::scroll(if horizontal { amount } else { -amount }, horizontal) {
        return Err(internal("Windows refused the input"));
    }
    std::thread::sleep(a.millis("settle_ms", 250)?);
    Ok(json!({ "at": point_json(at), "amount": amount, "horizontal": horizontal }))
}

/// Brings the target window to the front when one is named.
fn focus_target(a: &Args) -> Result<Value, Error> {
    if window_query(a)?.is_empty() {
        return Ok(Value::Null);
    }
    Ok(to_json(target_window(a, true)?))
}

fn type_text(a: &Args) -> Reply {
    a.only(&with(&[TARGET, &["text", "delay_ms", "settle_ms"]]))?;
    let _input = INPUT.lock().unwrap();
    let text = a
        .string("text")?
        .ok_or_else(|| invalid("text is required"))?;
    if text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(invalid(
            "text contains a control character other than newline or tab",
        ));
    }
    let window = focus_target(a)?;
    let delay = a.millis("delay_ms", if recording() { 35 } else { 0 })?;
    if !input::type_text(&text, delay) {
        return Err(internal("Windows refused the input"));
    }
    std::thread::sleep(a.millis("settle_ms", 100)?);
    Ok(json!({ "typed": text.chars().count(), "window": window }))
}

fn key(a: &Args) -> Reply {
    a.only(&with(&[TARGET, &["keys", "repeat", "settle_ms"]]))?;
    let _input = INPUT.lock().unwrap();
    let keys = a
        .string("keys")?
        .ok_or_else(|| invalid("keys is required, e.g. ctrl+s"))?;
    let repeat = a.int("repeat", 1, 100)?.unwrap_or(1) as u32;
    let window = focus_target(a)?;
    match input::press(&keys, repeat) {
        Ok(true) => {}
        Ok(false) => return Err(internal("Windows refused the input")),
        Err(e) => return Err(invalid(e)),
    }
    std::thread::sleep(a.millis("settle_ms", 150)?);
    Ok(json!({ "keys": keys, "repeat": repeat, "window": window }))
}

fn focus(a: &Args) -> Reply {
    a.only(TARGET)?;
    let _input = INPUT.lock().unwrap();
    let window = target_window(a, false)?;
    let foreground = find::activate(window.hwnd);
    let mut q = find::Query::default();
    q.hwnd = Some(window.hwnd);
    let window = find::windows(&q).into_iter().next().unwrap_or(window);
    Ok(json!({ "window": window, "foreground": foreground }))
}

// Looking ------------------------------------------------------------------

fn ocr(a: &Args) -> Reply {
    a.only(&with(&[TARGET, &["find"]]))?;
    let t = target(a, CaptureKind::Desktop)?;
    let lines = find::read(t.rect).map_err(internal)?;
    match a.string("find")? {
        Some(phrase) => {
            Ok(json!({ "target": target_json(&t), "matches": find::find_text(&lines, &phrase) }))
        }
        None => Ok(json!({ "target": target_json(&t), "lines": find::lines(&lines) })),
    }
}

fn elements(a: &Args) -> Reply {
    a.only(&with(&[TARGET, &["name", "type", "max"]]))?;
    let window = target_window(a, !a.bool("no_activate")?)?;
    let max = a.int("max", 1, 5000)?.unwrap_or(300) as usize;
    let mut all =
        find::elements(window.hwnd, if a.has("name") { 5000 } else { max }).map_err(internal)?;
    let control_type = a.string("type")?;
    if let Some(name) = a.string("name")? {
        all = find::find_elements(all, &name, control_type.as_deref());
    } else if let Some(t) = &control_type {
        all.retain(|e| e.control_type.eq_ignore_ascii_case(t));
    }
    all.truncate(max);
    Ok(json!({ "window": window, "elements": all }))
}

fn wait_window(a: &Args) -> Reply {
    a.only(&["window", "title", "process", "class", "gone", "timeout_ms"])?;
    let query = window_query(a)?;
    if query.is_empty() {
        return Err(invalid(
            "name a window with window, title, process or class",
        ));
    }
    let gone = a.bool("gone")?;
    let timeout = a.millis("timeout_ms", 10_000)?;
    let started = Instant::now();
    loop {
        let found = find::windows(&query).into_iter().next();
        match (&found, gone) {
            (Some(w), false) => {
                return Ok(
                    json!({ "window": w, "waited_ms": started.elapsed().as_millis() as u64 }),
                );
            }
            (None, true) => {
                return Ok(
                    json!({ "gone": true, "waited_ms": started.elapsed().as_millis() as u64 }),
                );
            }
            _ => {}
        }
        if started.elapsed() >= timeout {
            let what = if gone { "still open" } else { "did not appear" };
            return Err(Error::new(
                ErrorCode::Timeout,
                format!("the window {what} within {} ms", timeout.as_millis()),
            ));
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn wait_text(a: &Args) -> Reply {
    a.only(&with(&[
        TARGET,
        &["text", "gone", "timeout_ms", "interval_ms"],
    ]))?;
    let phrase = a
        .string("text")?
        .ok_or_else(|| invalid("text is required"))?;
    let gone = a.bool("gone")?;
    let timeout = a.millis("timeout_ms", 10_000)?;
    let interval = a.millis("interval_ms", 400)?;
    let started = Instant::now();
    // A target window comes forward once; after that it is only looked at.
    let mut quiet = Args(a.0.clone());
    quiet.0.insert("no_activate".into(), json!(true));
    let mut first = true;
    loop {
        let t = target(if first { a } else { &quiet }, CaptureKind::Desktop)?;
        first = false;
        let lines = find::read(t.rect).map_err(internal)?;
        let found = find::find_text(&lines, &phrase);
        if found.is_empty() == gone {
            return Ok(
                json!({ "matches": found, "gone": gone, "waited_ms": started.elapsed().as_millis() as u64, "target": target_json(&t) }),
            );
        }
        if started.elapsed() >= timeout {
            let what = if gone { "still shows" } else { "did not show" };
            return Err(Error::new(
                ErrorCode::Timeout,
                format!(
                    "the screen {what} {phrase:?} within {} ms; on screen: {}",
                    timeout.as_millis(),
                    snippet(lines.iter().map(|l| l.text.clone()))
                ),
            ));
        }
        std::thread::sleep(interval);
    }
}

// App ----------------------------------------------------------------------

fn open(a: &Args) -> Reply {
    a.only(&["path"])?;
    let hooks = crate::hooks()
        .ok_or_else(|| Error::new(ErrorCode::Unavailable, "the editor is not available"))?;
    match a.string("path")? {
        None => {
            (hooks.show_editor)();
            Ok(json!({ "shown": true }))
        }
        Some(path) => {
            let path = Path::new(&path);
            if !path.is_absolute() || !path.exists() {
                return Err(not_found(format!("no file {}", path.display())));
            }
            if !(hooks.open_in_editor)(path) {
                return Err(internal(format!("could not open {}", path.display())));
            }
            Ok(json!({ "opened": path }))
        }
    }
}

fn quit(a: &Args) -> Reply {
    a.only(&[])?;
    if let Some(r) = RECORDING.lock().unwrap().as_ref() {
        return Err(Error::new(
            ErrorCode::Conflict,
            format!("recording {} is running; stop or cancel it first", r.id),
        ));
    }
    // After this reply is on its way.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(200));
        crate::stop_engine();
    });
    Ok(json!({ "quitting": true }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn args(v: Value) -> Args {
        Args::new(v).unwrap()
    }

    fn test_guide() -> Mutex<Option<Guide>> {
        Mutex::new(Some(Guide {
            id: "guide-original".into(),
            generation: 1,
            title: "Test guide".into(),
            steps: Vec::new(),
        }))
    }

    fn test_step() -> stepcap::Step {
        stepcap::Step {
            frame: jiffshot_capture::Frame::new(4, 4),
            area: Rect::new(0, 0, 4, 4),
            mapping: None,
            click: (2, 3),
            right: false,
            description: "Open settings".into(),
        }
    }

    struct GuideFixture {
        directory: PathBuf,
    }

    impl GuideFixture {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!(
                "jiffshot-guide-finish-{}-{}",
                std::process::id(),
                NEXT_GUIDE_GENERATION.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&directory).unwrap();
            Self { directory }
        }

        fn output(&self) -> PathBuf {
            self.directory.join("guide.png")
        }

        fn editable(&self) -> PathBuf {
            self.output()
                .with_extension(jiffshot_capture::library::EDITS_EXT)
        }
    }

    impl Drop for GuideFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.output());
            let editable = self.editable();
            let _ = std::fs::remove_file(&editable);
            let _ = std::fs::remove_dir(&editable);
            let _ = std::fs::remove_dir(&self.directory);
        }
    }

    #[test]
    fn concurrent_guide_starts_return_their_own_snapshot() {
        let guide = Mutex::new(None);
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            for title in ["First guide", "Second guide"] {
                let guide = &guide;
                let barrier = &barrier;
                scope.spawn(move || {
                    let options = args(json!({ "title": title, "restart": true }));
                    barrier.wait();
                    for _ in 0..5_000 {
                        let result = start_guide(guide, &options).unwrap();
                        assert_eq!(result["active"], true);
                        assert_eq!(result["title"], title);
                        assert_eq!(result["steps"], json!([]));
                    }
                });
            }
        });
    }

    #[test]
    fn guided_click_records_a_step_only_after_successful_input() {
        let guide = test_guide();
        let recording = recorder::Control::default();
        recording.set_annotation_size(80, 60).unwrap();
        recording.millis.store(100, Ordering::Release);
        let error = click_with_step_and_capture(
            &guide,
            Some(true),
            || Ok(test_step()),
            || Err(internal("input failed")),
            |text| recorder::add_step_caption(&recording, text).map_err(invalid),
        )
        .unwrap_err();
        assert!(error.message.contains("input failed"));
        assert!(guide.lock().unwrap().as_ref().unwrap().steps.is_empty());
        assert!(recording.captions.lock().unwrap().is_empty());

        let result = click_with_step_and_capture(
            &guide,
            Some(true),
            || Ok(test_step()),
            || {
                recording.millis.store(250, Ordering::Release);
                Ok(())
            },
            |text| recorder::add_step_caption(&recording, text).map_err(invalid),
        )
        .unwrap();
        assert_eq!(
            result,
            Some(json!({
                "index": 1,
                "description": "Open settings",
                "at": { "x": 2, "y": 3 },
            }))
        );
        assert_eq!(guide.lock().unwrap().as_ref().unwrap().steps.len(), 1);
        let captions = recording.captions.lock().unwrap();
        assert_eq!(captions.len(), 1);
        assert_eq!(captions[0].text, "Open settings");
        assert_eq!(captions[0].at_ms, 250);
        assert_eq!(captions[0].duration_ms, None);
        assert!(!captions[0].burned_in);
    }

    #[test]
    fn guided_click_reports_caption_failure_after_preserving_the_step() {
        let guide = test_guide();
        let clicked = Cell::new(false);
        let error = click_with_step_and_capture(
            &guide,
            Some(true),
            || Ok(test_step()),
            || {
                clicked.set(true);
                Ok(())
            },
            |_| Err(internal("caption history is full")),
        )
        .unwrap_err();
        assert!(clicked.get());
        assert!(error.message.contains("click and guide step succeeded"));
        assert!(error.message.contains("caption history is full"));
        let guard = guide.lock().unwrap();
        let steps = &guard.as_ref().unwrap().steps;
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].description, "Open settings");
    }

    #[test]
    fn guided_step_rejects_a_cancelled_or_restarted_guide_before_clicking() {
        for restart in [false, true] {
            let guide = test_guide();
            let clicked = Cell::new(false);
            let captioned = Cell::new(false);
            let error = click_with_step_and_capture(
                &guide,
                Some(true),
                || {
                    let mut slot = guide
                        .try_lock()
                        .expect("capture must not hold the guide lock");
                    *slot = restart.then(|| Guide {
                        id: "guide-original".into(),
                        generation: 2,
                        title: "Replacement guide".into(),
                        steps: Vec::new(),
                    });
                    Ok(test_step())
                },
                || {
                    clicked.set(true);
                    Ok(())
                },
                |_| {
                    captioned.set(true);
                    Ok(())
                },
            )
            .unwrap_err();
            assert_eq!(error.code, ErrorCode::Conflict, "restart={restart}");
            assert!(error.message.contains("guide changed"));
            assert!(!clicked.get(), "restart={restart}");
            assert!(!captioned.get(), "restart={restart}");
            let guard = guide.lock().unwrap();
            if restart {
                let replacement = guard.as_ref().unwrap();
                assert_eq!(replacement.title, "Replacement guide");
                assert!(replacement.steps.is_empty());
            } else {
                assert!(guard.is_none());
            }
        }
    }

    #[test]
    fn guided_step_serializes_the_click_and_caption_with_its_guide() {
        use std::sync::TryLockError;

        let guide = test_guide();
        let input_locked = Cell::new(false);
        let caption_locked = Cell::new(false);
        let result = click_with_step_and_capture(
            &guide,
            Some(true),
            || Ok(test_step()),
            || {
                input_locked.set(matches!(guide.try_lock(), Err(TryLockError::WouldBlock)));
                Ok(())
            },
            |_| {
                caption_locked.set(matches!(guide.try_lock(), Err(TryLockError::WouldBlock)));
                Ok(())
            },
        )
        .unwrap();
        assert!(input_locked.get());
        assert!(caption_locked.get());
        assert_eq!(result.unwrap()["index"], 1);
        let guard = guide.lock().unwrap();
        let original = guard.as_ref().unwrap();
        assert_eq!(original.id, "guide-original");
        assert_eq!(original.steps[0].description, "Open settings");
    }

    #[test]
    fn explicit_guide_step_preserves_state_when_caption_recording_fails() {
        let guide = test_guide();
        let error = add_step_with(
            &guide,
            || Ok(test_step()),
            |_| Err(internal("caption history is full")),
        )
        .unwrap_err();
        assert!(error.message.contains("caption history is full"));
        assert!(guide.lock().unwrap().as_ref().unwrap().steps.is_empty());
    }

    #[test]
    fn explicit_guide_step_rejects_a_cancelled_or_restarted_guide() {
        for restart in [false, true] {
            let guide = test_guide();
            let captioned = Cell::new(false);
            let error = add_step_with(
                &guide,
                || {
                    let mut slot = guide
                        .try_lock()
                        .expect("capture must not hold the guide lock");
                    *slot = restart.then(|| Guide {
                        id: "guide-original".into(),
                        generation: 2,
                        title: "Replacement guide".into(),
                        steps: Vec::new(),
                    });
                    Ok(test_step())
                },
                |_| {
                    captioned.set(true);
                    Ok(())
                },
            )
            .unwrap_err();
            assert_eq!(error.code, ErrorCode::Conflict, "restart={restart}");
            assert!(!captioned.get(), "restart={restart}");
            let guard = guide.lock().unwrap();
            if restart {
                let replacement = guard.as_ref().unwrap();
                assert_eq!(replacement.title, "Replacement guide");
                assert!(replacement.steps.is_empty());
            } else {
                assert!(guard.is_none());
            }
        }
    }

    #[test]
    fn guided_click_requires_a_guide_when_explicitly_requested() {
        let guide = Mutex::new(None);
        let clicked = Cell::new(false);
        let error = click_with_step_and_capture(
            &guide,
            Some(true),
            || panic!("a missing guide must not be captured"),
            || {
                clicked.set(true);
                Ok(())
            },
            |_| panic!("a missing guide must not record a caption"),
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::NotFound);
        assert!(!clicked.get());

        let result = click_with_step_and_capture(
            &guide,
            None,
            || panic!("an unguided click must not capture a step"),
            || {
                clicked.set(true);
                Ok(())
            },
            |_| panic!("an unguided click must not record a caption"),
        )
        .unwrap();
        assert!(clicked.get());
        assert_eq!(result, None);
    }

    #[test]
    fn guided_click_can_explicitly_skip_the_step() {
        let guide = test_guide();
        let clicked = Cell::new(false);
        let result = click_with_step_and_capture(
            &guide,
            Some(false),
            || panic!("a disabled guide step must not be captured"),
            || {
                clicked.set(true);
                Ok(())
            },
            |_| panic!("a disabled guide step must not record a caption"),
        )
        .unwrap();
        assert!(clicked.get());
        assert_eq!(result, None);
        assert!(guide.lock().unwrap().as_ref().unwrap().steps.is_empty());
    }

    #[test]
    fn guide_finish_keeps_its_state_on_failure_and_serializes_finalization() {
        use std::sync::TryLockError;

        let guide = test_guide();
        let error = finish_guide_with(&guide, |_| -> Result<(), Error> {
            panic!("an empty guide must not be saved")
        })
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Conflict);
        assert!(guide.lock().unwrap().as_ref().unwrap().steps.is_empty());

        guide
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .steps
            .push(test_step());
        let error = finish_guide_with(&guide, |_| -> Result<(), Error> {
            assert!(matches!(guide.try_lock(), Err(TryLockError::WouldBlock)));
            Err(internal("render failed"))
        })
        .unwrap_err();
        assert!(error.message.contains("render failed"));
        let guard = guide.lock().unwrap();
        let original = guard.as_ref().unwrap();
        assert_eq!(original.id, "guide-original");
        assert_eq!(original.steps[0].description, "Open settings");
    }

    #[test]
    fn guide_finish_can_retry_after_editable_output_failures() {
        let fixture = GuideFixture::new();
        let output = fixture.output();
        let editable = fixture.editable();
        std::fs::create_dir(&editable).unwrap();
        let guide = test_guide();
        guide
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .steps
            .push(test_step());

        let error = finish_guide_with(&guide, |guide| {
            save_guide(
                guide,
                Some(output.clone()),
                None,
                jiffshot_doc::io::write_atomic,
            )
        })
        .unwrap_err();
        assert!(error.message.contains("editable guide path is a directory"));
        assert!(!output.exists());
        assert_eq!(guide.lock().unwrap().as_ref().unwrap().steps.len(), 1);

        std::fs::remove_dir(&editable).unwrap();
        let document_attempted = Cell::new(false);
        let error = finish_guide_with(&guide, |guide| {
            save_guide(guide, Some(output.clone()), None, |path, bytes| {
                assert_eq!(path, editable);
                assert!(!bytes.is_empty());
                document_attempted.set(true);
                Err("simulated document write failure".into())
            })
        })
        .unwrap_err();
        assert!(document_attempted.get());
        assert!(error.message.contains(output.to_string_lossy().as_ref()));
        assert!(error.message.contains("simulated document write failure"));
        assert!(output.is_file());
        assert!(!editable.exists());
        assert_eq!(guide.lock().unwrap().as_ref().unwrap().steps.len(), 1);

        let (result, path) = finish_guide_with(&guide, |guide| {
            save_guide(
                guide,
                Some(output.clone()),
                None,
                jiffshot_doc::io::write_atomic,
            )
        })
        .unwrap();
        assert_eq!(path, output);
        assert_eq!(result["opened"], false);
        assert_eq!(result["steps"][0]["description"], "Open settings");
        assert!(guide.lock().unwrap().is_none());
        let image = jiffshot_capture::Frame::load_png(&output).unwrap();
        let document = jiffshot_doc::io::load_document(&editable).unwrap();
        assert_eq!(
            (image.width, image.height),
            (document.width(), document.height())
        );
        assert!(!document.objects.is_empty());
    }

    #[test]
    #[ignore = "requires JIFFSHOT_CLI_E2E and, without test-control, a free control pipe"]
    fn cli_guide_finish_retries_after_output_failure_without_desktop_input() {
        let cli = std::env::var_os("JIFFSHOT_CLI_E2E").expect("set JIFFSHOT_CLI_E2E to the CLI");
        #[cfg(feature = "test-control")]
        let suffix = format!("guide-{}", std::process::id());
        #[cfg(feature = "test-control")]
        jiffshot_control::serve_with_test_suffix(&suffix, handle).expect("claim the test pipe");
        #[cfg(not(feature = "test-control"))]
        jiffshot_control::serve(handle).expect("claim the test pipe");
        let fixture = GuideFixture::new();
        let output = fixture.output();
        let editable = fixture.editable();
        std::fs::create_dir(&editable).unwrap();
        let run = |arguments: &[&str]| {
            let mut command = std::process::Command::new(&cli);
            command.args(arguments).arg("--no-launch");
            #[cfg(feature = "test-control")]
            command.env(jiffshot_control::TEST_PIPE_SUFFIX_ENV, &suffix);
            let output = command.output().expect("run CLI command");
            let response: Value = serde_json::from_slice(&output.stdout).expect("JSON response");
            (output.status.success(), response)
        };

        let (succeeded, started) = run(&["steps", "start", "Test guide"]);
        assert!(succeeded, "{started}");
        let id = started["result"]["id"].as_str().expect("guide ID");
        {
            let mut slot = GUIDE.lock().unwrap();
            let guide = slot.as_mut().expect("active guide");
            assert_eq!(guide.id, id);
            guide.steps.push(test_step());
        }
        let output_text = output.to_str().expect("fixture path");
        let (succeeded, failed) = run(&["steps", "finish", "--out", output_text]);
        assert!(!succeeded, "{failed}");
        assert_eq!(failed["error"]["code"], "internal");
        assert!(failed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("editable guide path is a directory"));
        assert!(!output.exists());
        let (succeeded, status) = run(&["steps", "status"]);
        assert!(succeeded, "{status}");
        assert_eq!(status["result"]["id"], id);
        assert_eq!(status["result"]["steps"][0]["description"], "Open settings");

        std::fs::remove_dir(&editable).unwrap();
        let (succeeded, finished) = run(&["steps", "finish", "--out", output_text]);
        assert!(succeeded, "{finished}");
        assert_eq!(finished["result"]["id"], id);
        assert_eq!(
            finished["result"]["steps"][0]["description"],
            "Open settings"
        );
        let image = jiffshot_capture::Frame::load_png(&output).unwrap();
        let document = jiffshot_doc::io::load_document(&editable).unwrap();
        assert_eq!(
            (image.width, image.height),
            (document.width(), document.height())
        );
        let (succeeded, status) = run(&["steps", "status"]);
        assert!(succeeded, "{status}");
        assert_eq!(status["result"]["active"], false);
    }

    #[test]
    fn arguments_are_read_leniently_and_checked_strictly() {
        let a = args(
            json!({ "at": "10, 20", "n": "5", "f": "true", "p": [1.4, 2.6], "o": { "x": 3, "y": 4 } }),
        );
        assert_eq!(a.point("at").unwrap(), Some((10, 20)));
        assert_eq!(a.point("p").unwrap(), Some((1, 3)));
        assert_eq!(a.point("o").unwrap(), Some((3, 4)));
        assert_eq!(a.int("n", 0, 10).unwrap(), Some(5));
        assert!(a.int("n", 6, 10).is_err());
        assert!(a.bool("f").unwrap());
        assert!(!a.bool("missing").unwrap());
        assert!(a.point("n").is_err());
        assert!(a.only(&["at", "n", "f", "p"]).is_err());
        assert!(a.only(&["at", "n", "f", "p", "o"]).is_ok());
        assert!(Args::new(json!([1])).is_err());
        assert!(args(json!({"at":"NaN,1"})).point("at").is_err());
        assert!(args(json!({"at":"1e300,1"})).point("at").is_err());
    }

    #[test]
    fn output_paths_must_be_absolute_with_the_right_extension() {
        let dir = std::env::temp_dir().join("jiffshot-control-test");
        let png = dir.join("a.png");
        assert_eq!(
            args(json!({ "out": png })).out("out", &["png"]).unwrap(),
            Some(png.clone())
        );
        assert!(args(json!({ "out": "a.png" }))
            .out("out", &["png"])
            .is_err());
        assert!(args(json!({ "out": dir.join("a.jpg") }))
            .out("out", &["png"])
            .is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_commands_and_arguments_are_refused() {
        assert_eq!(
            dispatch("nope", &args(Value::Null)).unwrap_err().code,
            ErrorCode::Unsupported
        );
        assert_eq!(
            dispatch("monitors", &args(json!({ "x": 1 })))
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            dispatch("record.stop", &args(Value::Null))
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
        assert_eq!(
            dispatch("steps.finish", &args(Value::Null))
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
        assert_eq!(
            dispatch("key", &args(json!({ "keys": "ctrl+nosuchkey" })))
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            dispatch("capture.scroll", &args(json!({"timeout_ms":0})))
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            dispatch("capture.scroll", &args(json!({"max_frames":129})))
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn every_command_is_listed() {
        for (command, _) in COMMANDS {
            let error = dispatch(command, &args(json!({ "__probe": 1 }))).err();
            assert!(
                error.is_none_or(|e| e.code != ErrorCode::Unsupported),
                "{command} is not dispatched"
            );
        }
    }

    #[test]
    fn recording_status_reports_each_silent_source_and_keeps_capture_notes() {
        let control = Arc::new(recorder::Control::default());
        *control.audio.lock().unwrap() = Some("no webcam: unavailable".into());
        let recording = Recording {
            id: "recording-test".into(),
            key: None,
            path: PathBuf::from("test.mp4"),
            target: Value::Null,
            fps: 30,
            started_at: String::new(),
            control: control.clone(),
            worker: std::thread::spawn(|| Err("no recording started".into())),
        };
        let status = recording_json(&recording);
        assert_eq!(status["system_silent"], false);
        assert_eq!(status["microphone_silent"], false);
        assert_eq!(status["audio_note"], "no webcam: unavailable");

        control.system_silent.store(true, Ordering::Relaxed);
        let status = recording_json(&recording);
        assert_eq!(status["system_silent"], true);
        assert_eq!(status["microphone_silent"], false);
        assert_eq!(status["audio_note"], "no webcam: unavailable");

        control.system_silent.store(false, Ordering::Relaxed);
        control.microphone_silent.store(true, Ordering::Relaxed);
        let status = recording_json(&recording);
        assert_eq!(status["system_silent"], false);
        assert_eq!(status["microphone_silent"], true);
        assert_eq!(status["audio_note"], "no webcam: unavailable");
        assert!(recording.worker.join().expect("test worker").is_err());
    }

    #[test]
    #[ignore = "requires JIFFSHOT_CLI_E2E and, without test-control, a free control pipe"]
    fn cli_edits_artifacts_over_real_ipc_without_desktop_input() {
        let cli =
            std::env::var_os("JIFFSHOT_CLI_E2E").expect("set JIFFSHOT_CLI_E2E to the built CLI");
        #[cfg(feature = "test-control")]
        let suffix = format!("cli-{}", std::process::id());
        #[cfg(feature = "test-control")]
        jiffshot_control::serve_with_test_suffix(&suffix, handle)
            .expect("claim the test control pipe");
        #[cfg(not(feature = "test-control"))]
        jiffshot_control::serve(handle).expect("claim the test control pipe");
        let dir =
            std::env::temp_dir().join(format!("jiffshot-cli-artifacts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test artifact directory");
        let script = dir.join("run.json");
        let plan = dir.join("edit.json");
        let jsd = dir.join("annotated.jsd");
        let png = dir.join("annotated.png");
        let source = dir.join("source.mp4");
        let edited = dir.join("edited.mp4");
        let still = dir.join("frame.png");
        let gif = dir.join("demo.gif");
        let joined = dir.join("joined.mp4");
        let planned_mp4 = dir.join("planned.mp4");
        let planned_gif = dir.join("planned.gif");
        struct FixtureFiles {
            files: Vec<PathBuf>,
            directory: PathBuf,
        }
        impl Drop for FixtureFiles {
            fn drop(&mut self) {
                for file in &self.files {
                    let _ = std::fs::remove_file(file);
                }
                let _ = std::fs::remove_dir(&self.directory);
            }
        }
        let _fixtures = FixtureFiles {
            files: vec![
                script.clone(),
                plan.clone(),
                jsd.clone(),
                png.clone(),
                source.clone(),
                edited.clone(),
                still.clone(),
                gif.clone(),
                joined.clone(),
                planned_mp4.clone(),
                planned_gif.clone(),
            ],
            directory: dir.clone(),
        };
        let spec = VideoSpec::for_size(320, 200, 10);
        let mut writer = jiffshot_capture::media::Writer::create(&source, spec, None)
            .expect("synthetic video writer");
        let pixels = vec![0u8; 320 * 200 * 4];
        for index in 0..20 {
            writer
                .write_frame(&pixels, index * 1_000_000, 1_000_000)
                .expect("synthetic frame");
        }
        writer.finish().expect("finalize synthetic video");
        std::fs::write(
            &plan,
            serde_json::to_vec(&json!({
                "fps": 10,
                "duration": 1.0,
                "transition": 0.2,
                "mute": true,
                "segments": [
                    {"path":"source.mp4","from":0,"to":1,"speed":2},
                    {"path":"source.mp4","from":1,"to":2}
                ],
                "outputs": [
                    {"format":"mp4","path":"planned.mp4"},
                    {"format":"gif","path":"planned.gif","fps":5,"max_width":120}
                ]
            }))
            .expect("encode video edit plan"),
        )
        .expect("write video edit plan");
        std::fs::write(&script, serde_json::to_vec(&json!({
            "version": 1,
            "steps": [
                {"command":"doc.new","args":{"width":320,"height":200},"save_as":"document"},
                {"command":"doc.draw","args":{"id":{"$ref":"document.id"},"tool":"Arrow","from":"20,30","to":"180,80"}},
                {"command":"doc.draw","args":{"id":{"$ref":"document.id"},"tool":"Text","from":"20,100","to":"280,180","text":"Agent demo"}},
                {"command":"doc.crop","args":{"id":{"$ref":"document.id"},"x":10,"y":10,"width":300,"height":180}},
                {"command":"doc.save","args":{"id":{"$ref":"document.id"},"out":jsd}},
                {"command":"doc.export","args":{"id":{"$ref":"document.id"},"out":png}},
                {"command":"doc.open","args":{"path":jsd},"save_as":"reopened"},
                {"command":"doc.status","args":{"id":{"$ref":"reopened.id"}}},
                {"command":"video.info","args":{"path":source}},
                {"command":"video.export","args":{"path":source,"out":edited,"from":0.2,"to":1.2,"fps":10,"width":160,"height":100,"mute":true,
                    "overlays":[{"path":jsd,"x":0,"y":0,"from":0,"to":0.5}]}},
                {"command":"video.frame","args":{"path":edited,"at":0.2,"out":still}},
                {"command":"video.join","args":{"segments":[{"path":edited,"from":0,"to":0.5},{"path":edited,"from":0.5,"to":1}],"out":joined,"fps":10,"mute":true}},
                {"command":"video.export","args":{"path":joined,"out":gif,"fps":10}},
                {"command":"video.info","args":{"path":edited}},
                {"command":"video.edit","args":{"plan":plan}},
                {"command":"video.verify","args":{"path":planned_mp4}}
            ],
            "finally": [
                {"command":"doc.close","args":{"id":{"$ref":"document.id"}}},
                {"command":"doc.close","args":{"id":{"$ref":"reopened.id"}}}
            ]
        })).expect("encode test script")).expect("write test script");
        let mut command = std::process::Command::new(cli);
        command.args(["run", script.to_str().expect("script path"), "--no-launch"]);
        #[cfg(feature = "test-control")]
        command.env(jiffshot_control::TEST_PIPE_SUFFIX_ENV, suffix);
        let output = command.output().expect("run real CLI");
        assert!(
            output.status.success(),
            "CLI failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout).expect("JSON stdout");
        assert_eq!(report["ok"], true);
        assert_eq!(report["result"]["steps"][7]["result"]["width"], 300);
        assert_eq!(report["result"]["steps"][7]["result"]["height"], 180);
        assert_eq!(
            report["result"]["steps"][7]["result"]["objects"]
                .as_array()
                .expect("objects")
                .len(),
            2
        );
        assert!(report["result"]["finally"]
            .as_array()
            .expect("cleanup")
            .iter()
            .all(|step| step["ok"] == true));
        let doc = jiffshot_doc::io::open(&jsd).expect("saved editable artifact");
        let exported = jiffshot_doc::io::load_image(&png).expect("exported PNG artifact");
        assert_eq!(exported.data(), doc.flatten().data());
        assert_eq!(report["result"]["steps"][13]["result"]["width"], 160);
        assert_eq!(report["result"]["steps"][13]["result"]["height"], 100);
        assert_eq!(report["result"]["steps"][14]["result"][0]["format"], "mp4");
        assert_eq!(report["result"]["steps"][14]["result"][0]["frames"], 10);
        assert_eq!(report["result"]["steps"][14]["result"][1]["format"], "gif");
        assert_eq!(report["result"]["steps"][15]["result"]["frames"], 10);
        assert_eq!(
            report["result"]["steps"][15]["result"]["constant_frame_rate"],
            true
        );
        let frame = jiffshot_doc::io::load_image(&still).expect("extracted video frame");
        assert_eq!((frame.width(), frame.height()), (160, 100));
        assert!(
            frame.pixels().iter().any(|p| p.red() > 100),
            "overlay must be visible in the extracted encoded frame"
        );
        assert!(std::fs::metadata(&edited).expect("edited video").len() > 0);
        assert!(std::fs::metadata(&gif).expect("animated GIF").len() > 0);
        assert!(std::fs::metadata(&planned_gif).expect("plan GIF").len() > 0);
        std::fs::remove_file(script).expect("remove script");
        std::fs::remove_file(plan).expect("remove video edit plan");
        std::fs::remove_file(jsd).expect("remove document");
        std::fs::remove_file(png).expect("remove export");
        for file in [source, edited, still, gif, joined, planned_mp4, planned_gif] {
            std::fs::remove_file(file).expect("remove media fixture");
        }
        std::fs::remove_dir(dir).expect("remove empty artifact directory");
    }

    #[test]
    #[cfg(feature = "test-control")]
    #[ignore = "requires an interactive Windows desktop and JIFFSHOT_CLI_E2E"]
    fn cli_pause_resume_records_contiguous_live_frames_and_markers() {
        let cli =
            std::env::var_os("JIFFSHOT_CLI_E2E").expect("set JIFFSHOT_CLI_E2E to the test CLI");
        let suffix = format!("record-{}", std::process::id());
        jiffshot_control::serve_with_test_suffix(&suffix, handle).expect("claim the test pipe");
        let window = crate::selftest_scroll::RecordingTestWindow::open().expect("open test window");
        let rect = window.rect;
        let region = format!("{},{},{},{}", rect.x, rect.y, rect.width, rect.height);
        let directory =
            std::env::temp_dir().join(format!("jiffshot-cli-recording-{}", std::process::id()));
        std::fs::create_dir(&directory).expect("create recording fixture directory");
        let video = directory.join("pause-resume.mp4");
        let sidecar = recording_marks::sidecar_path(&video);
        struct Fixture {
            video: PathBuf,
            sidecar: PathBuf,
            directory: PathBuf,
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.video);
                let _ = std::fs::remove_file(&self.sidecar);
                let _ = std::fs::remove_dir(&self.directory);
            }
        }
        let _fixture = Fixture {
            video: video.clone(),
            sidecar,
            directory,
        };
        let run = |arguments: &[&str]| -> Value {
            let output = std::process::Command::new(&cli)
                .args(arguments)
                .arg("--no-launch")
                .env(jiffshot_control::TEST_PIPE_SUFFIX_ENV, &suffix)
                .output()
                .expect("run CLI command");
            assert!(
                output.status.success(),
                "CLI {arguments:?} failed: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let response: Value = serde_json::from_slice(&output.stdout).expect("CLI JSON");
            assert_eq!(response["ok"], true, "{response}");
            response["result"].clone()
        };
        let output = video.to_str().expect("fixture path");
        let wall_started = Instant::now();
        let started = run(&[
            "record",
            "start",
            "--region",
            &region,
            "--out",
            output,
            "--fps",
            "10",
            "--audio",
            "none",
            "--no-cursor",
        ]);
        assert_eq!(started["active"], true);
        struct CancelOnDrop(bool);
        impl Drop for CancelOnDrop {
            fn drop(&mut self) {
                if self.0 {
                    let _ = handle(Request::new("record.cancel", json!({})));
                }
            }
        }
        let mut recording = CancelOnDrop(true);
        let wait_for_frames = |minimum: u64| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let status = run(&["record", "status"]);
                assert_eq!(status["ended"], false, "recording ended: {status}");
                if status["frames"].as_u64().unwrap_or(0) >= minimum {
                    break status;
                }
                assert!(Instant::now() < deadline, "recording stalled: {status}");
                std::thread::sleep(Duration::from_millis(100));
            }
        };
        wait_for_frames(8);
        let annotation = run(&[
            "record",
            "annotate",
            "--kind",
            r#"{"type":"Text","rect":{"x0":10,"y0":10,"x1":180,"y1":70},"text":"Pause-free caption"}"#,
            "--style",
            r#"{"font_size":20}"#,
        ]);
        assert_eq!(annotation["annotation_id"], 1);
        wait_for_frames(12);
        run(&["record", "mark", "--label", "before"]);
        assert_eq!(run(&["record", "pause"])["paused"], true);
        std::thread::sleep(Duration::from_millis(250));
        let paused = run(&["record", "status"]);
        let paused_frames = paused["frames"].as_u64().expect("paused frame count");
        let paused_ms = paused["duration_ms"].as_u64().expect("paused time");
        let pause_started = Instant::now();
        std::thread::sleep(Duration::from_millis(1_200));
        let still_paused = run(&["record", "status"]);
        assert_eq!(still_paused["frames"], paused_frames, "no paused frames");
        assert_eq!(still_paused["duration_ms"], paused_ms, "pause-free clock");
        assert_eq!(run(&["record", "clear"])["cleared"], true);
        window.scroll(800);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(run(&["record", "resume"])["paused"], false);
        let pause_duration = pause_started.elapsed();
        wait_for_frames(paused_frames + 8);
        run(&["record", "mark", "--label", "after"]);
        let stopped = run(&["record", "stop"]);
        recording.0 = false;
        let wall_duration = wall_started.elapsed();
        assert_eq!(stopped["markers"], 2);

        let metadata = jiffshot_capture::media::post::verify(&video).expect("verify recording");
        assert_eq!(metadata.constant_frame_rate, Some(true));
        assert!(
            metadata.duration + pause_duration.as_secs_f64() < wall_duration.as_secs_f64() + 0.5,
            "recorded {:.2}s over {:.2}s with {:.2}s paused",
            metadata.duration,
            wall_duration.as_secs_f64(),
            pause_duration.as_secs_f64()
        );
        let mut reader = jiffshot_capture::media::Reader::open(&video).expect("recording reader");
        let before = reader
            .frame_at(paused_ms.saturating_sub(250) as i64 * 10_000)
            .expect("decode pre-pause frame")
            .expect("pre-pause frame exists");
        let after = reader
            .frame_at((paused_ms + 200) as i64 * 10_000)
            .expect("decode resumed frame")
            .expect("resumed frame exists");
        let changed = before
            .bgra
            .chunks_exact(4)
            .zip(after.bgra.chunks_exact(4))
            .filter(|(left, right)| left[0].abs_diff(right[0]) > 25)
            .count();
        assert!(
            changed > before.bgra.len() / 4 / 20,
            "the resumed frame must show the newly scrolled test window: {changed} pixels changed"
        );
        let marks = recording_marks::read(&video)
            .expect("read recording sidecar")
            .expect("recording sidecar exists");
        assert_eq!(marks.markers().len(), 2);
        assert_eq!(marks.captions().len(), 1);
        assert_eq!(marks.captions()[0].text, "Pause-free caption");
        assert!(marks.captions()[0].burned_in);
        assert_eq!(
            marks.captions()[0].duration_ms,
            Some(paused_ms.saturating_sub(marks.captions()[0].at_ms))
        );
        assert!(
            marks.activities_complete(),
            "physical input tracking was available"
        );
    }
}
