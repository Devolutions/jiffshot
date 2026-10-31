//! The control server: lets `jiffshot-cli` (and through it scripts and AI
//! agents) drive the running Jiffshot over its per-user pipe (see the
//! `jiffshot-control` crate). It takes screenshots, records video, builds
//! step-by-step guides, and plays mouse and keyboard input into other apps,
//! finding what to click by window, OCR text or UI Automation name.
//!
//! Each request runs on its own thread; recordings and guides outlive
//! requests in the statics below. Commands and their arguments are listed
//! in `docs/cli.md`.

mod find;
mod input;
mod document;
mod video;

use crate::{log, recorder, stepcap, workflow};
use input::Button;
use jiffshot_capture::library::{CaptureKind, Library};
use jiffshot_capture::media::VideoSpec;
use jiffshot_capture::{screen, Rect};
use jiffshot_control::{Error, ErrorCode, Request, Response};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

type Reply = Result<Value, Error>;

/// Commands with a one-line summary, reported by `status`.
pub const COMMANDS: &[(&str, &str)] = &[
    ("status", "server, protocol and what is running"),
    ("monitors", "displays and their rectangles"),
    ("windows", "visible top-level windows (filter by title, process, class)"),
    ("cursor", "where the pointer is"),
    ("screenshot", "capture a screen, window, region or the desktop to PNG"),
    ("capture.scroll", "bounded automatic vertical or horizontal scroll-and-stitch capture"),
    ("record.start", "start recording a target to MP4; returns once frames flow"),
    ("record.status", "the running recording (or the last one)"),
    ("record.pause", "pause the recording"),
    ("record.resume", "resume the recording"),
    ("record.options", "change cursor, click rings, camera and audio while recording"),
    ("record.annotate", "add a live annotation to recorded frames"),
    ("record.clear", "remove live recording annotations"),
    ("record.stop", "stop and finalize the MP4; returns its metadata"),
    ("record.cancel", "stop and delete the recording"),
    ("steps.start", "begin a step-by-step guide"),
    ("steps.add", "add a step: the screen at a point, with a caption"),
    ("steps.status", "the guide being built"),
    ("steps.finish", "lay out and save the guide (PNG and editable .jsd)"),
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
    ("wait.text", "wait for text to appear on screen (or go away)"),
    ("open", "open a capture in the editor, or show the editor"),
    ("doc.tools", "annotation tools, variants, styles, effects and export formats"),
    ("doc.list", "headless editable document sessions"),
    ("doc.new", "create a blank editable document"),
    ("doc.open", "load an image or editable .jsd into a document session"),
    ("doc.status", "document objects, selection, size and undo/redo state"),
    ("doc.add", "add a typed annotation object"),
    ("doc.draw", "use an editor tool with document-space pointer points"),
    ("doc.update", "edit an annotation's geometry, style or rotation"),
    ("doc.image", "insert an image into the document"),
    ("doc.select", "select objects by id"),
    ("doc.action", "undo, redo, grouping, stacking, alignment and transforms"),
    ("doc.crop", "crop the image and annotations"),
    ("doc.resize", "resample or resize the canvas"),
    ("doc.effect", "apply a named image effect"),
    ("doc.ocr", "recognize text in the edited document"),
    ("doc.simplify", "replace text with bars and reduce image colors"),
    ("doc.redact", "cover recognized sensitive patterns with editable redactions"),
    ("doc.combine", "combine document images or build an editable template"),
    ("doc.save", "save an editable .jsd document"),
    ("doc.export", "export flattened PNG, JPEG, BMP, GIF, WebP or PDF"),
    ("doc.close", "release a document session"),
    ("video.info", "inspect a saved video"),
    ("video.frame", "extract a video frame to PNG"),
    ("video.export", "trim, resize and annotate a video; export MP4 or GIF"),
    ("video.join", "join video segments with optional timed overlays"),
    ("quit", "exit Jiffshot"),
];

/// Starts serving; logs and gives up when the pipe cannot be created
/// (another Jiffshot owns it).
pub fn start() {
    match jiffshot_control::serve(handle) {
        Ok(()) => log(&format!("control: listening on {}", jiffshot_control::pipe_name().unwrap_or_default())),
        Err(error) => log(&format!("control: not available: {error}")),
    }
}

fn handle(request: Request) -> Response {
    // SAFETY: COM (UI Automation) for this connection's thread.
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
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
            log(&format!("control: {} failed ({ms} ms): {error}", request.command));
            Response::err(&request.id, error)
        }
    }
}

fn dispatch(command: &str, a: &Args) -> Reply {
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
        command if command.starts_with("video.") => video::dispatch(command, a),
        _ => Err(Error::new(ErrorCode::Unsupported, format!("unknown command {command:?}; `status` lists the commands"))),
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
            Some(k) if allowed.is_empty() => Err(invalid(format!("unexpected argument {k:?}: this command takes none"))),
            Some(k) => Err(invalid(format!("unexpected argument {k:?}; expected one of: {}", allowed.join(", ")))),
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
            Some(Value::String(s)) => s.trim().parse().map(Some).map_err(|_| invalid(format!("{key} must be a number, not {s:?}"))),
            Some(_) => Err(invalid(format!("{key} must be a number"))),
        }
    }

    fn int(&self, key: &str, min: i64, max: i64) -> Result<Option<i64>, Error> {
        match self.number(key)? {
            None => Ok(None),
            Some(n) if n.fract() == 0.0 && (min as f64..=max as f64).contains(&n) => Ok(Some(n as i64)),
            Some(n) => Err(invalid(format!("{key} must be a whole number from {min} to {max}, not {n}"))),
        }
    }

    fn millis(&self, key: &str, default: u64) -> Result<Duration, Error> {
        Ok(Duration::from_millis(self.int(key, 0, 3_600_000)?.map_or(default, |n| n as u64)))
    }

    /// A screen point: `"X,Y"`, `[X, Y]` or `{"x": X, "y": Y}`.
    fn point(&self, key: &str) -> Result<Option<(i32, i32)>, Error> {
        let bad = || invalid(format!("{key} must be a point X,Y in screen pixels"));
        let pair = |x: Option<f64>, y: Option<f64>| match (x, y) {
            (Some(x), Some(y)) if x.is_finite() && y.is_finite()
                && (i32::MIN as f64..=i32::MAX as f64).contains(&x)
                && (i32::MIN as f64..=i32::MAX as f64).contains(&y) => Ok(Some((x.round() as i32, y.round() as i32))),
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
            Some(Value::Object(o)) => pair(o.get("x").and_then(Value::as_f64), o.get("y").and_then(Value::as_f64)),
            Some(_) => Err(bad()),
        }
    }

    /// An absolute output path whose folder exists (created if need be),
    /// with one of `extensions`.
    fn out(&self, key: &str, extensions: &[&str]) -> Result<Option<PathBuf>, Error> {
        let Some(text) = self.string(key)? else { return Ok(None) };
        let path = PathBuf::from(&text);
        if !path.is_absolute() {
            return Err(invalid(format!("{key} must be an absolute path (the CLI makes paths absolute), not {text:?}")));
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        if !extensions.contains(&ext.as_str()) {
            return Err(invalid(format!("{key} must end in .{}", extensions.join(" or ."))));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| invalid(format!("cannot create {}: {e}", parent.display())))?;
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
const TARGET: &[&str] = &["screen", "desktop", "region", "window", "title", "process", "class", "no_activate"];

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
        Some(Value::Number(n)) => Some(n.as_i64().ok_or_else(|| invalid("window must be a window handle"))? as isize),
        Some(Value::String(s)) => {
            let s = s.trim();
            let parsed = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                Some(hex) => i64::from_str_radix(hex, 16).ok(),
                None => s.parse().ok(),
            };
            Some(parsed.ok_or_else(|| invalid(format!("window must be a window handle (decimal or 0x hex), not {s:?}")))? as isize)
        }
        Some(_) => return Err(invalid("window must be a window handle")),
    };
    let text = |k: &str| a.0.get(k).and_then(Value::as_str);
    Ok(find::Query { hwnd, title: text("title"), process: text("process"), class: text("class"), all: false })
}

/// The window the target arguments name, brought to the front unless
/// `no_activate`.
fn target_window(a: &Args, activate: bool) -> Result<find::Window, Error> {
    let query = window_query(a)?;
    if query.is_empty() {
        return Err(invalid("name a window with window, title, process or class"));
    }
    if let Some(h) = query.hwnd {
        if !find::alive(h) {
            return Err(not_found(format!("no window {h:#x}")));
        }
    }
    let window = find::windows(&query).into_iter().next().ok_or_else(|| not_found("no visible window matches; `windows` lists them"))?;
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
    let primary = || monitors.iter().find(|m| m.primary).or(monitors.first()).cloned().ok_or_else(|| internal("no displays"));
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
            .ok_or_else(|| invalid("screen must be a display index (from `monitors`), primary or cursor"))?;
            monitors.get(index as usize).cloned().ok_or_else(|| not_found(format!("no display {index}; there are {}", monitors.len())))
        }
    }
}

/// What the target arguments pick; `fallback` when there are none.
fn target(a: &Args, fallback: CaptureKind) -> Result<Target, Error> {
    let desktop = screen::virtual_screen();
    let clip = |r: Rect| r.intersect(desktop).filter(|r| !r.is_empty()).ok_or_else(|| invalid(format!("{r:?} is off screen")));
    if let Some(region) = a.string("region")? {
        let rect = crate::parse_rect(&region).ok_or_else(|| invalid(format!("region must be X,Y,W,H, not {region:?}")))?;
        return Ok(Target { rect: clip(rect)?, kind: CaptureKind::Region, window: None });
    }
    if a.bool("desktop")? {
        return Ok(Target { rect: desktop, kind: CaptureKind::Desktop, window: None });
    }
    if !window_query(a)?.is_empty() {
        let window = target_window(a, true)?;
        if window.minimized {
            return Err(Error::new(ErrorCode::Conflict, format!("window {:?} is minimized", window.title)));
        }
        return Ok(Target { rect: clip(window.rect)?, kind: CaptureKind::Window, window: Some(window) });
    }
    if let Some(spec) = a.0.get("screen").filter(|v| !v.is_null()) {
        return Ok(Target { rect: monitor(spec)?.rect, kind: CaptureKind::Monitor, window: None });
    }
    match fallback {
        CaptureKind::Desktop => Ok(Target { rect: desktop, kind: CaptureKind::Desktop, window: None }),
        _ => Ok(Target { rect: monitor(&Value::Bool(true))?.rect, kind: CaptureKind::Monitor, window: None }),
    }
}

fn target_json(t: &Target) -> Value {
    json!({ "rect": t.rect, "kind": t.kind, "window": t.window })
}

// Status -------------------------------------------------------------------

fn status() -> Reply {
    let commands: Map<String, Value> = COMMANDS.iter().map(|(c, d)| (c.to_string(), json!(d))).collect();
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
        None => Library::new(jiffshot_capture::config::library_root()).save(&capture.frame, &capture.meta).map_err(internal)?,
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
    a.only(&with(&[TARGET, &["out", "horizontal", "max_frames", "timeout_ms", "settle_ms", "notches"]]))?;
    let out = a.out("out", &["png"])?;
    let horizontal = a.bool("horizontal")?;
    let max_frames = a.int("max_frames", 2, 128)?.unwrap_or(32) as usize;
    let timeout = a.millis("timeout_ms", 30_000)?;
    let settle = a.millis("settle_ms", 300)?;
    if timeout.is_zero() || settle.is_zero() { return Err(invalid("timeout_ms and settle_ms must be positive")); }
    let notches = a.int("notches", 1, 20)?.unwrap_or(3) as i32;
    let t = target(a, CaptureKind::Scrolling)?;
    let mut capture = workflow::immediate(CaptureKind::Scrolling, t.rect, false).map_err(internal)?;
    if let Some(w) = &t.window { capture.meta.window_title = Some(w.title.clone()); capture.meta.process = Some(w.process.clone()); }
    let pixels_per_frame = u64::from(capture.frame.width) * u64::from(capture.frame.height);
    if pixels_per_frame > 64_000_000 { return Err(invalid("scroll capture target exceeds 64 million pixels")); }
    let transform = |f: jiffshot_capture::Frame| if horizontal { crate::modes::transpose(&f) } else { f };
    let mut frames = vec![transform(capture.frame)];
    struct RestoreCursor((i32, i32));
    impl Drop for RestoreCursor {
        fn drop(&mut self) { input::move_to(self.0, Duration::ZERO); }
    }
    let _restore = RestoreCursor(input::cursor());
    input::move_to(t.rect.center(), Duration::ZERO);
    if input::cursor() != t.rect.center() { return Err(internal("could not position the cursor inside the scroll target")); }
    std::thread::sleep(settle.min(timeout));
    frames[0] = transform(screen::grab(t.rect).map_err(internal)?);
    let started = Instant::now();
    let mut stopped = "frame_limit";
    while frames.len() < max_frames {
        if started.elapsed().saturating_add(settle) > timeout { stopped = "deadline"; break; }
        if pixels_per_frame * (frames.len() as u64 + 1) > 64_000_000 { stopped = "pixel_limit"; break; }
        if !input::scroll(if horizontal { notches } else { -notches }, horizontal) { return Err(internal("Windows rejected scroll input")); }
        std::thread::sleep(settle);
        let next = transform(screen::grab(t.rect).map_err(internal)?);
        let previous = frames.last().ok_or_else(|| internal("no initial scroll frame"))?;
        if next.pixels == previous.pixels { stopped = "no_movement"; break; }
        if jiffshot_capture::stitch::vertical_step(previous, &next).is_none() {
            return Err(invalid("scroll frames have no matching overlap; use a content-only region, fewer notches or a longer settle_ms"));
        }
        frames.push(next);
    }
    let frame_count = frames.len();
    let stitched = jiffshot_capture::stitch::stitch_vertical(&frames).ok_or_else(|| internal("could not stitch scroll frames"))?;
    capture.frame = transform(stitched);
    capture.meta.width = capture.frame.width;
    capture.meta.height = capture.frame.height;
    let path = match out {
        Some(path) => { capture.frame.save_png(&path).map_err(internal)?; path }
        None => Library::new(jiffshot_capture::config::library_root()).save(&capture.frame, &capture.meta).map_err(internal)?,
    };
    Ok(json!({"path":path,"width":capture.frame.width,"height":capture.frame.height,"frames":frame_count,
        "stopped":stopped,"limited":stopped != "no_movement","captured_at":capture.meta.captured_at,"target":target_json(&t)}))
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
    let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
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
        None => json!({ "active": false, "finalizing": FINALIZING.load(Ordering::Acquire), "tray_recording": recorder::is_recording() && !FINALIZING.load(Ordering::Acquire), "last": *LAST_RECORDING.lock().unwrap() }),
    }
}

fn record_start(a: &Args) -> Reply {
    a.only(&with(&[TARGET, &["out", "fps", "audio", "no_cursor", "clicks", "highlight", "max_seconds", "key", "webcam", "webcam_corner", "webcam_circle", "webcam_size", "noise_gate"]]))?;
    let _lifecycle = RECORDING_LIFECYCLE.lock().expect("recording lifecycle lock poisoned");
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
            return Err(Error::new(ErrorCode::Conflict, format!("recording {} is already running; stop it first", r.id)));
        }
    }
    if recorder::is_recording() {
        return Err(Error::new(ErrorCode::Conflict, "a tray recording is running; finish it before starting a CLI recording"));
    }
    let audio = a.string("audio")?.unwrap_or_else(|| "none".into());
    let audio = if audio == "mic" { "microphone".to_string() } else { audio };
    if !matches!(audio.as_str(), "none" | "system" | "microphone" | "both") {
        return Err(invalid("audio must be none, system, microphone or both"));
    }
    let fps = a.int("fps", 5, 60)?.unwrap_or(30) as u32;
    let max = a.number("max_seconds")?.map(|s| {
        if !s.is_finite() || !(0.001..=86_400.0).contains(&s) {
            Err(invalid("max_seconds must be finite and between 0.001 and 86400"))
        } else { Ok(Duration::from_secs_f64(s)) }
    }).transpose()?;
    let path = match a.out("out", &["mp4"])? {
        Some(p) => p,
        None => Library::new(jiffshot_capture::config::library_root()).recording_path().map_err(internal)?,
    };
    let t = target(a, CaptureKind::Monitor)?;
    if path.exists() { return Err(Error::new(ErrorCode::Conflict, "recording output already exists; choose a new path")); }
    let mut options = recorder::Options::audio_only(fps, &audio);
    options.cursor = !a.bool("no_cursor")?;
    options.click_rings = a.bool("clicks")?;
    options.cursor_highlight = a.bool("highlight")?;
    let corner = a.string("webcam_corner")?.unwrap_or_else(|| "bottom-right".into());
    validate_corner(&corner)?;
    options.webcam_corner = corner.clone();
    options.webcam = a.bool("webcam")?.then_some(corner);
    options.webcam_circle = a.bool("webcam_circle")?;
    let size = a.number("webcam_size")?.unwrap_or(0.25);
    if !size.is_finite() || !(0.05..=0.75).contains(&size) { return Err(invalid("webcam_size must be 0.05..0.75 of the recording width")); }
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
            return Err(Error::new(ErrorCode::Timeout, "the recording did not start within 10 s"));
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
    let recording = Recording { id: new_id("rec"), key, path, target: target_json(&t), fps, started_at: jiffshot_capture::library::timestamp(), control, worker };
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

fn current_recording<'a>(slot: &'a mut Option<Recording>, a: &Args) -> Result<&'a mut Recording, Error> {
    let id = a.string("id")?;
    match slot {
        None => Err(not_found("no recording is running")),
        Some(r) if id.as_ref().is_some_and(|id| *id != r.id) => Err(not_found(format!("recording {} is the one running", r.id))),
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
    if r.worker.is_finished() { Err(Error::new(ErrorCode::Conflict, "recording has ended; use record.stop to collect the finalized result")) }
    else { Ok(()) }
}

fn validate_corner(corner: &str) -> Result<(), Error> {
    if matches!(corner, "top-left" | "top-right" | "bottom-left" | "bottom-right") { Ok(()) }
    else { Err(invalid("webcam_corner must be top-left, top-right, bottom-left or bottom-right")) }
}

fn record_options(a: &Args) -> Reply {
    a.only(&["id", "cursor", "highlight", "clicks", "webcam", "webcam_circle", "webcam_corner", "system", "microphone"])?;
    // Validate everything before changing any switch.
    let switches = ["cursor", "highlight", "clicks", "webcam", "webcam_circle", "system", "microphone"]
        .iter().map(|k| a.flag(k)).collect::<Result<Vec<_>, _>>()?;
    let corner = a.string("webcam_corner")?;
    if let Some(corner) = &corner { validate_corner(corner)?; }
    let mut slot = RECORDING.lock().expect("recording lock poisoned");
    let r = current_recording(&mut slot, a)?;
    require_live_recording(r)?;
    let c = &r.control;
    for (flag, value) in [&c.cursor, &c.highlight, &c.clicks, &c.webcam, &c.webcam_circle, &c.system, &c.microphone].into_iter().zip(switches) {
        if let Some(value) = value { flag.store(value, Ordering::Release); }
    }
    if let Some(corner) = corner { *c.webcam_corner.lock().expect("webcam corner lock poisoned") = corner; }
    Ok(recording_json(r))
}

fn record_annotate(a: &Args) -> Reply {
    a.only(&["id", "kind", "style", "duration_ms"])?;
    let kind = document::kind(a.0.get("kind").ok_or_else(|| invalid("kind is required; use the same schema as doc.add"))?)?;
    let style = document::style(a.0.get("style"), jiffshot_doc::object::Style::default())?;
    let duration = a.millis("duration_ms", 0)?;
    let mut slot = RECORDING.lock().expect("recording lock poisoned");
    let r = current_recording(&mut slot, a)?;
    require_live_recording(r)?;
    let mark = recorder::add_annotation(&r.control, kind, style, duration).map_err(invalid)?;
    Ok(json!({"id": r.id, "annotation_id": mark, "coordinates": "recording pixels", "duration_ms": duration.as_millis()}))
}

fn record_clear(a: &Args) -> Reply {
    a.only(&["id"])?;
    let mut slot = RECORDING.lock().expect("recording lock poisoned");
    let r = current_recording(&mut slot, a)?;
    require_live_recording(r)?;
    recorder::clear_annotations(&r.control);
    Ok(json!({"id": r.id, "cleared": true}))
}

/// Stops the recording and waits until the MP4 is finalized (or deleted).
fn record_stop(a: &Args, cancel: bool) -> Reply {
    a.only(&["id"])?;
    let _lifecycle = RECORDING_LIFECYCLE.lock().expect("recording lifecycle lock poisoned");
    struct Finalizing;
    impl Drop for Finalizing {
        fn drop(&mut self) { FINALIZING.store(false, Ordering::Release); }
    }
    let recording = {
        let mut slot = RECORDING.lock().unwrap();
        current_recording(&mut slot, a)?;
        FINALIZING.store(true, Ordering::Release);
        slot.take().unwrap()
    };
    let _finalizing = Finalizing;
    let Recording { id, key, path, target, fps, started_at, control, worker } = recording;
    control.paused.store(false, Ordering::Release);
    control.stop.store(true, Ordering::Release);
    let outcome = worker.join().unwrap_or_else(|_| Err("the recorder crashed".into()));
    let (frames, duration) = (control.frames.load(Ordering::Relaxed), control.millis.load(Ordering::Relaxed));
    if cancel {
        if path.exists() { std::fs::remove_file(&path).map_err(internal)?; }
        return Ok(json!({ "id": id, "cancelled": true, "frames": frames, "duration_ms": duration }));
    }
    let spec = outcome.map_err(|e| internal(format!("the recording failed: {e}")))?;
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
    title: String,
    steps: Vec<stepcap::Step>,
}

static GUIDE: Mutex<Option<Guide>> = Mutex::new(None);

fn steps_status() -> Value {
    match &*GUIDE.lock().unwrap() {
        None => json!({ "active": false }),
        Some(g) => json!({
            "active": true,
            "id": g.id,
            "title": g.title,
            "steps": g.steps.iter().enumerate().map(|(i, s)| json!({ "index": i + 1, "description": s.description, "at": point_json(s.click) })).collect::<Vec<_>>(),
        }),
    }
}

fn steps_start(a: &Args) -> Reply {
    a.only(&["title", "restart"])?;
    let mut guide = GUIDE.lock().unwrap();
    if let Some(g) = guide.as_ref() {
        if !a.bool("restart")? {
            return Err(Error::new(ErrorCode::Conflict, format!("guide {} is in progress ({} steps); finish, cancel or restart it", g.id, g.steps.len())));
        }
    }
    let title = a.string("title")?.unwrap_or_else(|| "How to …".into());
    *guide = Some(Guide { id: new_id("guide"), title: title.clone(), steps: Vec::new() });
    drop(guide);
    Ok(steps_status())
}

/// Adds a step at `at`: the display there as it looks now, captioned with
/// `description` (default: UI Automation's name of the control there).
fn add_step(at: (i32, i32), right: bool, description: Option<String>) -> Result<Value, Error> {
    if GUIDE.lock().unwrap().is_none() {
        return Err(not_found("no guide is in progress; run steps.start first"));
    }
    let area = screen::monitor_at(at.0, at.1).map(|m| m.rect).ok_or_else(|| invalid(format!("{at:?} is off screen")))?;
    let frame = screen::grab(area).map_err(internal)?;
    let description = description.filter(|d| !d.trim().is_empty()).unwrap_or_else(|| find::describe_click(at, right));
    let mut guide = GUIDE.lock().unwrap();
    let g = guide.as_mut().ok_or_else(|| not_found("the guide was cancelled"))?;
    g.steps.push(stepcap::Step { frame, area, click: at, right, description: description.clone() });
    Ok(json!({ "index": g.steps.len(), "description": description, "at": point_json(at) }))
}

fn steps_add(a: &Args) -> Reply {
    a.only(&["at", "right", "description"])?;
    let at = a.point("at")?.unwrap_or_else(input::cursor);
    add_step(at, a.bool("right")?, a.string("description")?)
}

fn steps_finish(a: &Args) -> Reply {
    a.only(&["out", "title", "open"])?;
    let out = a.out("out", &["png"])?;
    let guide = GUIDE.lock().unwrap().take().ok_or_else(|| not_found("no guide is in progress"))?;
    if guide.steps.is_empty() {
        *GUIDE.lock().unwrap() = Some(guide);
        return Err(Error::new(ErrorCode::Conflict, "the guide has no steps yet"));
    }
    let title = a.string("title")?.unwrap_or_else(|| guide.title.clone());
    let doc = stepcap::guide_titled(&guide.steps, &title);
    let frame = workflow::frame_from_pixmap(&doc.flatten());
    let rect = Rect::new(0, 0, frame.width as i32, frame.height as i32);
    let capture = workflow::from_frame(CaptureKind::Steps, rect, frame);
    let path = match out {
        Some(path) => {
            capture.frame.save_png(&path).map_err(internal)?;
            path
        }
        None => Library::new(jiffshot_capture::config::library_root()).save(&capture.frame, &capture.meta).map_err(internal)?,
    };
    let document = path.with_extension(jiffshot_capture::library::EDITS_EXT);
    jiffshot_doc::io::save_document(&doc, &document).map_err(internal)?;
    let opened = a.bool("open")? && crate::hooks().is_some_and(|h| (h.open_in_editor)(&path));
    Ok(json!({
        "id": guide.id,
        "title": title,
        "path": path,
        "document": document,
        "width": capture.frame.width,
        "height": capture.frame.height,
        "opened": opened,
        "steps": guide.steps.iter().enumerate().map(|(i, s)| json!({ "index": i + 1, "description": s.description, "at": point_json(s.click) })).collect::<Vec<_>>(),
    }))
}

fn steps_cancel(a: &Args) -> Reply {
    a.only(&[])?;
    let guide = GUIDE.lock().unwrap().take().ok_or_else(|| not_found("no guide is in progress"))?;
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
        let area = if has_target(a) { target(a, CaptureKind::Monitor)?.rect } else { screen::virtual_screen() };
        let lines = find::read(area).map_err(internal)?;
        let found = find::find_text(&lines, &phrase);
        let hit = found.get(nth - 1).ok_or_else(|| {
            not_found(format!("text {phrase:?} not found ({} matches); on screen: {}", found.len(), snippet(lines.iter().map(|l| l.text.clone()))))
        })?;
        return Ok((shift(hit.center), json!({ "text": hit })));
    }
    if let Some(name) = a.string("element")? {
        let window = target_window(a, true).map_err(|e| if e.code == ErrorCode::InvalidRequest { invalid("element needs a window: give window, title, process or class") } else { e })?;
        let all = find::elements(window.hwnd, 5000).map_err(internal)?;
        let names = snippet(all.iter().filter(|e| !e.name.is_empty()).map(|e| format!("{} ({})", e.name, e.control_type)));
        let control_type = a.string("type")?;
        let found = find::find_elements(all, &name, control_type.as_deref());
        let hit = found.get(nth - 1).ok_or_else(|| not_found(format!("element {name:?} not found ({} matches); controls: {names}", found.len())))?;
        return Ok((shift(hit.center), json!({ "element": hit })));
    }
    Err(invalid("give at (X,Y), text (to find by OCR) or element (a UI Automation name)"))
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
    a.only(&with(&[TARGET, POINTER, &["button", "right", "middle", "double", "count", "step", "description", "glide_ms", "settle_ms"]]))?;
    let _input = INPUT.lock().unwrap();
    let (at, matched) = locate(a)?;
    let button = button(a)?;
    let count = if a.bool("double")? { 2 } else { a.int("count", 1, 3)?.unwrap_or(1) as u32 };
    let guiding = GUIDE.lock().unwrap().is_some();
    let step = match a.flag("step")? {
        Some(true) if !guiding => return Err(not_found("step needs a guide in progress; run steps.start first")),
        Some(s) => s,
        None => guiding,
    };
    let glide = glide(a)?;
    // The step shows the screen as it is about to be clicked, pointer included.
    let step = if step {
        input::move_to(at, glide);
        Some(add_step(at, button == Button::Right, a.string("description")?)?)
    } else {
        None
    };
    if !input::click(at, button, count, glide) {
        return Err(internal("Windows refused the input (is a higher-privileged window in front?)"));
    }
    std::thread::sleep(a.millis("settle_ms", 250)?);
    Ok(json!({ "at": point_json(at), "button": format!("{button:?}").to_lowercase(), "count": count, "matched": matched, "step": step }))
}

fn move_pointer(a: &Args) -> Reply {
    a.only(&with(&[TARGET, POINTER, &["glide_ms"]]))?;
    let _input = INPUT.lock().unwrap();
    let (at, matched) = locate(a)?;
    input::move_to(at, glide(a)?);
    Ok(json!({ "at": point_json(at), "matched": matched }))
}

fn drag(a: &Args) -> Reply {
    a.only(&["from", "to", "button", "right", "middle", "glide_ms", "settle_ms"])?;
    let _input = INPUT.lock().unwrap();
    let from = a.point("from")?.ok_or_else(|| invalid("from is required"))?;
    let to = a.point("to")?.ok_or_else(|| invalid("to is required"))?;
    if !input::drag(from, to, button(a)?, a.millis("glide_ms", 500)?) {
        return Err(internal("Windows refused the input"));
    }
    std::thread::sleep(a.millis("settle_ms", 250)?);
    Ok(json!({ "from": point_json(from), "to": point_json(to) }))
}

fn scroll(a: &Args) -> Reply {
    a.only(&with(&[TARGET, POINTER, &["amount", "horizontal", "glide_ms", "settle_ms"]]))?;
    let _input = INPUT.lock().unwrap();
    let amount = a.int("amount", -1000, 1000)?.ok_or_else(|| invalid("amount is required: notches, positive scrolls down (or right)"))? as i32;
    let horizontal = a.bool("horizontal")?;
    let at = if a.has("at") || a.has("text") || a.has("element") {
        let (at, _) = locate(a)?;
        input::move_to(at, glide(a)?);
        at
    } else {
        if has_target(a) {
            let t = target(a, CaptureKind::Monitor)?;
            let center = (t.rect.x + t.rect.width / 2, t.rect.y + t.rect.height / 2);
            input::move_to(center, glide(a)?);
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
    let text = a.string("text")?.ok_or_else(|| invalid("text is required"))?;
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
    let keys = a.string("keys")?.ok_or_else(|| invalid("keys is required, e.g. ctrl+s"))?;
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
        Some(phrase) => Ok(json!({ "target": target_json(&t), "matches": find::find_text(&lines, &phrase) })),
        None => Ok(json!({ "target": target_json(&t), "lines": find::lines(&lines) })),
    }
}

fn elements(a: &Args) -> Reply {
    a.only(&with(&[TARGET, &["name", "type", "max"]]))?;
    let window = target_window(a, !a.bool("no_activate")?)?;
    let max = a.int("max", 1, 5000)?.unwrap_or(300) as usize;
    let mut all = find::elements(window.hwnd, if a.has("name") { 5000 } else { max }).map_err(internal)?;
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
        return Err(invalid("name a window with window, title, process or class"));
    }
    let gone = a.bool("gone")?;
    let timeout = a.millis("timeout_ms", 10_000)?;
    let started = Instant::now();
    loop {
        let found = find::windows(&query).into_iter().next();
        match (&found, gone) {
            (Some(w), false) => return Ok(json!({ "window": w, "waited_ms": started.elapsed().as_millis() as u64 })),
            (None, true) => return Ok(json!({ "gone": true, "waited_ms": started.elapsed().as_millis() as u64 })),
            _ => {}
        }
        if started.elapsed() >= timeout {
            let what = if gone { "still open" } else { "did not appear" };
            return Err(Error::new(ErrorCode::Timeout, format!("the window {what} within {} ms", timeout.as_millis())));
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn wait_text(a: &Args) -> Reply {
    a.only(&with(&[TARGET, &["text", "gone", "timeout_ms", "interval_ms"]]))?;
    let phrase = a.string("text")?.ok_or_else(|| invalid("text is required"))?;
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
            return Ok(json!({ "matches": found, "gone": gone, "waited_ms": started.elapsed().as_millis() as u64, "target": target_json(&t) }));
        }
        if started.elapsed() >= timeout {
            let what = if gone { "still shows" } else { "did not show" };
            return Err(Error::new(
                ErrorCode::Timeout,
                format!("the screen {what} {phrase:?} within {} ms; on screen: {}", timeout.as_millis(), snippet(lines.iter().map(|l| l.text.clone()))),
            ));
        }
        std::thread::sleep(interval);
    }
}

// App ----------------------------------------------------------------------

fn open(a: &Args) -> Reply {
    a.only(&["path"])?;
    let hooks = crate::hooks().ok_or_else(|| Error::new(ErrorCode::Unavailable, "the editor is not available"))?;
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
        return Err(Error::new(ErrorCode::Conflict, format!("recording {} is running; stop or cancel it first", r.id)));
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

    fn args(v: Value) -> Args {
        Args::new(v).unwrap()
    }

    #[test]
    fn arguments_are_read_leniently_and_checked_strictly() {
        let a = args(json!({ "at": "10, 20", "n": "5", "f": "true", "p": [1.4, 2.6], "o": { "x": 3, "y": 4 } }));
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
        assert_eq!(args(json!({ "out": png })).out("out", &["png"]).unwrap(), Some(png.clone()));
        assert!(args(json!({ "out": "a.png" })).out("out", &["png"]).is_err());
        assert!(args(json!({ "out": dir.join("a.jpg") })).out("out", &["png"]).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_commands_and_arguments_are_refused() {
        assert_eq!(dispatch("nope", &args(Value::Null)).unwrap_err().code, ErrorCode::Unsupported);
        assert_eq!(dispatch("monitors", &args(json!({ "x": 1 }))).unwrap_err().code, ErrorCode::InvalidRequest);
        assert_eq!(dispatch("record.stop", &args(Value::Null)).unwrap_err().code, ErrorCode::NotFound);
        assert_eq!(dispatch("steps.finish", &args(Value::Null)).unwrap_err().code, ErrorCode::NotFound);
        assert_eq!(dispatch("key", &args(json!({ "keys": "ctrl+nosuchkey" }))).unwrap_err().code, ErrorCode::InvalidRequest);
        assert_eq!(dispatch("capture.scroll", &args(json!({"timeout_ms":0}))).unwrap_err().code, ErrorCode::InvalidRequest);
        assert_eq!(dispatch("capture.scroll", &args(json!({"max_frames":129}))).unwrap_err().code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn every_command_is_listed() {
        for (command, _) in COMMANDS {
            let error = dispatch(command, &args(json!({ "__probe": 1 }))).err();
            assert!(error.is_none_or(|e| e.code != ErrorCode::Unsupported), "{command} is not dispatched");
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
    #[ignore = "requires JIFFSHOT_CLI_E2E pointing to a built CLI and a free control pipe"]
    fn cli_edits_artifacts_over_real_ipc_without_desktop_input() {
        let cli = std::env::var_os("JIFFSHOT_CLI_E2E").expect("set JIFFSHOT_CLI_E2E to the built CLI");
        jiffshot_control::serve(handle).expect("claim the test control pipe");
        let dir = std::env::temp_dir().join(format!("jiffshot-cli-artifacts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test artifact directory");
        let script = dir.join("edit.json");
        let jsd = dir.join("annotated.jsd");
        let png = dir.join("annotated.png");
        let source = dir.join("source.mp4");
        let edited = dir.join("edited.mp4");
        let still = dir.join("frame.png");
        let gif = dir.join("demo.gif");
        let joined = dir.join("joined.mp4");
        struct FixtureFiles { files: Vec<PathBuf>, directory: PathBuf }
        impl Drop for FixtureFiles {
            fn drop(&mut self) {
                for file in &self.files { let _ = std::fs::remove_file(file); }
                let _ = std::fs::remove_dir(&self.directory);
            }
        }
        let _fixtures = FixtureFiles { files: vec![script.clone(), jsd.clone(), png.clone(), source.clone(), edited.clone(), still.clone(), gif.clone(), joined.clone()], directory: dir.clone() };
        let spec = VideoSpec::for_size(320, 200, 10);
        let mut writer = jiffshot_capture::media::Writer::create(&source, spec, None).expect("synthetic video writer");
        let pixels = vec![0u8; 320 * 200 * 4];
        for index in 0..20 {
            writer.write_frame(&pixels, index * 1_000_000, 1_000_000).expect("synthetic frame");
        }
        writer.finish().expect("finalize synthetic video");
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
                {"command":"video.info","args":{"path":edited}}
            ],
            "finally": [
                {"command":"doc.close","args":{"id":{"$ref":"document.id"}}},
                {"command":"doc.close","args":{"id":{"$ref":"reopened.id"}}}
            ]
        })).expect("encode test script")).expect("write test script");
        let output = std::process::Command::new(cli).args(["run", script.to_str().expect("script path"), "--no-launch"]).output().expect("run real CLI");
        assert!(output.status.success(), "CLI failed: {}", String::from_utf8_lossy(&output.stderr));
        let report: Value = serde_json::from_slice(&output.stdout).expect("JSON stdout");
        assert_eq!(report["ok"], true);
        assert_eq!(report["result"]["steps"][7]["result"]["width"], 300);
        assert_eq!(report["result"]["steps"][7]["result"]["height"], 180);
        assert_eq!(report["result"]["steps"][7]["result"]["objects"].as_array().expect("objects").len(), 2);
        assert!(report["result"]["finally"].as_array().expect("cleanup").iter().all(|step| step["ok"] == true));
        let doc = jiffshot_doc::io::open(&jsd).expect("saved editable artifact");
        let exported = jiffshot_doc::io::load_image(&png).expect("exported PNG artifact");
        assert_eq!(exported.data(), doc.flatten().data());
        assert_eq!(report["result"]["steps"][13]["result"]["width"], 160);
        assert_eq!(report["result"]["steps"][13]["result"]["height"], 100);
        let frame = jiffshot_doc::io::load_image(&still).expect("extracted video frame");
        assert_eq!((frame.width(), frame.height()), (160, 100));
        assert!(frame.pixels().iter().any(|p| p.red() > 100), "overlay must be visible in the extracted encoded frame");
        assert!(std::fs::metadata(&edited).expect("edited video").len() > 0);
        assert!(std::fs::metadata(&gif).expect("animated GIF").len() > 0);
        std::fs::remove_file(script).expect("remove script");
        std::fs::remove_file(jsd).expect("remove document");
        std::fs::remove_file(png).expect("remove export");
        for file in [source, edited, still, gif, joined] { std::fs::remove_file(file).expect("remove media fixture"); }
        std::fs::remove_dir(dir).expect("remove empty artifact directory");
    }
}
