//! The macOS control server shares the upstream framed JSON protocol.

#[path = "control/document.rs"]
mod document;
#[path = "control/capture_macos.rs"]
mod capture;
#[path = "control/find_macos.rs"]
mod find;
#[path = "control/video.rs"]
mod video;
#[path = "control/steps_macos.rs"]
mod steps;
#[path = "control/record_macos.rs"]
mod record;
#[path = "control/input_macos.rs"]
mod input;

use jiffshot_control::{Error, ErrorCode, Request, Response};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::time::Instant;

type Reply = Result<Value, Error>;

const COMMANDS: &[(&str, &str)] = &[
    ("status", "server, protocol and what is running"),
    ("monitors", "displays and their rectangles"),
    ("windows", "top-level windows; off-screen entries require Accessibility"),
    ("elements", "Accessibility controls in a window"),
    ("cursor", "where the pointer is"),
    ("screenshot", "capture a screen, window, region or the desktop to PNG"),
    ("capture.scroll", "scroll and stitch a region or window to PNG"),
    ("ocr", "text on screen with its rectangles"),
    ("focus", "bring a window to the front"),
    ("wait.window", "wait for a window to appear or go away"),
    ("wait.text", "wait for text to appear on screen or go away"),
    ("open", "open a capture in the editor, or show the editor"),
    ("quit", "request a graceful Jiffshot exit"),
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
    ("steps.status", "the current step-by-step guide"),
    ("steps.start", "start a step-by-step guide"),
    ("steps.add", "capture and describe a guide step"),
    ("steps.finish", "save the guide as an image and editable document"),
    ("steps.cancel", "discard the current guide"),
    ("click", "click a point, OCR text or Accessibility control"),
    ("move", "move the pointer"),
    ("drag", "drag from one point to another"),
    ("scroll", "turn the mouse wheel"),
    ("type", "type Unicode text"),
    ("key", "press a key combination"),
    ("record.status", "the current recording"),
    ("record.start", "start a screen recording"),
    ("record.pause", "pause a recording"),
    ("record.resume", "resume a recording"),
    ("record.options", "change live recording options"),
    ("record.annotate", "add a live recording annotation"),
    ("record.clear", "clear live recording annotations"),
    ("record.stop", "stop and save a recording"),
    ("record.cancel", "discard a recording"),
];

pub(crate) fn start() -> std::io::Result<jiffshot_control::Server> {
    let server = jiffshot_control::serve_owned_with_completion(handle)?;
    match jiffshot_control::pipe_name() {
        Ok(name) => crate::log(&format!("control: listening on {name}")),
        Err(error) => crate::log(&format!("control: could not report the socket path: {error}")),
    }
    Ok(server)
}

pub(crate) fn shutdown_recording() -> Result<(), String> {
    record::shutdown()
}

fn handle(request: Request) -> (Response, Option<jiffshot_control::Completion>) {
    objc2::rc::autoreleasepool(|_| handle_in_pool(request))
}

fn handle_in_pool(request: Request) -> (Response, Option<jiffshot_control::Completion>) {
    let started = Instant::now();
    let reply = if crate::shutdown_macos::requested() {
        Err(Error::new(ErrorCode::Conflict, "Jiffshot is shutting down"))
    } else {
        Args::new(request.args).and_then(|args| dispatch(&request.command, &args))
    };
    let ms = started.elapsed().as_millis();
    match reply {
        Ok(result) => {
            crate::log(&format!("control: {} ok ({ms} ms)", request.command));
            let completion: Option<jiffshot_control::Completion> = if request.command == "quit" {
                Some(Box::new(|| match crate::hooks() {
                    Some(hooks) => (hooks.quit)(),
                    None => crate::log("control: could not request exit: the application is unavailable"),
                }))
            } else {
                None
            };
            (Response::ok(&request.id, result), completion)
        }
        Err(error) => {
            crate::log(&format!("control: {} failed ({ms} ms): {error}", request.command));
            (Response::err(&request.id, error), None)
        }
    }
}

fn dispatch(command: &str, a: &Args) -> Reply {
    match command {
        "status" => {
            a.only(&[])?;
            let commands: Map<String, Value> = COMMANDS.iter().map(|(command, description)| (command.to_string(), json!(description))).collect();
            Ok(json!({
                "app": "jiffshot",
                "version": env!("CARGO_PKG_VERSION"),
                "protocol": jiffshot_control::PROTOCOL,
                "pid": std::process::id(),
                "session": null,
                "pipe": jiffshot_control::pipe_name().map_err(internal)?,
                "library": jiffshot_capture::config::library_root(),
                "recording": record::status()?,
                "steps": steps::status()?,
                "commands": commands,
            }))
        }
        "monitors" => {
            a.only(&[])?;
            let layout = jiffshot_capture::screen::DisplayLayout::current().map_err(internal)?;
            let monitors: Vec<Value> = layout.monitors().iter().enumerate().map(|(index, monitor)| json!({
                "index": index,
                "device": monitor.device,
                "rect": monitor.rect,
                "work": monitor.work,
                "dpi": monitor.dpi,
                "scale": monitor.scale(),
                "primary": monitor.primary,
            })).collect();
            Ok(json!({ "monitors": monitors, "desktop": layout.virtual_screen() }))
        }
        "cursor" => capture::cursor(a),
        "windows" => find::list(a),
        "elements" => find::elements(a),
        "screenshot" => capture::screenshot(a),
        "ocr" => capture::ocr(a),
        "focus" => input::dispatch(command, a),
        "wait.window" => find::wait_window(a),
        "wait.text" => capture::wait_text(a),
        "click" | "move" | "drag" | "scroll" | "type" | "key" | "capture.scroll" => input::dispatch(command, a),
        "open" => open(a),
        "quit" => {
            a.only(&[])?;
            crate::hooks().ok_or_else(|| Error::new(ErrorCode::Unavailable, "the application is not available"))?;
            Ok(json!({ "quitting": true }))
        }
        command if command.starts_with("doc.") => document::dispatch(command, a),
        command if command.starts_with("video.") => video::dispatch(command, a),
        command if command.starts_with("steps.") => steps::dispatch(command, a),
        command if command.starts_with("record.") => record::dispatch(command, a),
        _ => Err(Error::new(ErrorCode::Unsupported, format!("unknown or unavailable command {command:?}; `status` lists the commands"))),
    }
}

pub(crate) fn selftest_request(command: &str, args: Value) -> Result<Value, String> {
    let args = Args::new(args).map_err(|error| error.to_string())?;
    objc2::rc::autoreleasepool(|_| dispatch(command, &args)).map_err(|error| error.to_string())
}

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

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidRequest, message)
}

fn not_found(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::NotFound, message)
}

fn internal(message: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::Internal, message.to_string())
}

fn point_json(point: (i32, i32)) -> Value {
    json!({ "x": point.0, "y": point.1 })
}

struct Args(Map<String, Value>);

impl Args {
    fn new(value: Value) -> Result<Self, Error> {
        match value {
            Value::Null => Ok(Self(Map::new())),
            Value::Object(map) => Ok(Self(map)),
            _ => Err(invalid("arguments must be a JSON object")),
        }
    }

    fn only(&self, allowed: &[&str]) -> Result<(), Error> {
        match self.0.keys().find(|key| !allowed.contains(&key.as_str())) {
            None => Ok(()),
            Some(key) if allowed.is_empty() => Err(invalid(format!("unexpected argument {key:?}: this command takes none"))),
            Some(key) => Err(invalid(format!("unexpected argument {key:?}; expected one of: {}", allowed.join(", ")))),
        }
    }

    fn string(&self, key: &str) -> Result<Option<String>, Error> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.clone())),
            Some(value @ (Value::Number(_) | Value::Bool(_))) => Ok(Some(value.to_string())),
            Some(_) => Err(invalid(format!("{key} must be text"))),
        }
    }

    fn has(&self, key: &str) -> bool {
        self.0.get(key).is_some_and(|value| !value.is_null())
    }

    fn flag(&self, key: &str) -> Result<Option<bool>, Error> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(value)) => Ok(Some(*value)),
            Some(Value::String(value)) if matches!(value.as_str(), "true" | "1" | "yes") => Ok(Some(true)),
            Some(Value::String(value)) if matches!(value.as_str(), "false" | "0" | "no") => Ok(Some(false)),
            Some(Value::Number(value)) => Ok(Some(value.as_f64() != Some(0.0))),
            Some(_) => Err(invalid(format!("{key} must be true or false"))),
        }
    }

    fn bool(&self, key: &str) -> Result<bool, Error> {
        Ok(self.flag(key)?.unwrap_or(false))
    }
    fn number(&self, key: &str) -> Result<Option<f64>, Error> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(value)) => Ok(value.as_f64()),
            Some(Value::String(value)) => value.trim().parse().map(Some).map_err(|_| invalid(format!("{key} must be a number, not {value:?}"))),
            Some(_) => Err(invalid(format!("{key} must be a number"))),
        }
    }

    fn int(&self, key: &str, min: i64, max: i64) -> Result<Option<i64>, Error> {
        match self.number(key)? {
            None => Ok(None),
            Some(value) if value.fract() == 0.0 && (min as f64..=max as f64).contains(&value) => Ok(Some(value as i64)),
            Some(value) => Err(invalid(format!("{key} must be a whole number from {min} to {max}, not {value}"))),
        }
    }

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
            Some(Value::String(value)) => {
                let parts: Vec<Option<f64>> = value.split(',').map(|part| part.trim().parse().ok()).collect();
                match parts.as_slice() {
                    [x, y] => pair(*x, *y),
                    _ => Err(bad()),
                }
            }
            Some(Value::Array(value)) if value.len() == 2 => pair(value[0].as_f64(), value[1].as_f64()),
            Some(Value::Object(value)) => pair(value.get("x").and_then(Value::as_f64), value.get("y").and_then(Value::as_f64)),
            Some(_) => Err(bad()),
        }
    }

    fn out(&self, key: &str, extensions: &[&str]) -> Result<Option<PathBuf>, Error> {
        let Some(text) = self.string(key)? else { return Ok(None) };
        let path = PathBuf::from(&text);
        if !path.is_absolute() {
            return Err(invalid(format!("{key} must be an absolute path (the CLI makes paths absolute), not {text:?}")));
        }
        let extension = path.extension().and_then(|extension| extension.to_str()).unwrap_or("").to_ascii_lowercase();
        if !extensions.contains(&extension.as_str()) {
            return Err(invalid(format!("{key} must end in .{}", extensions.join(" or ."))));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| invalid(format!("cannot create {}: {error}", parent.display())))?;
        }
        Ok(Some(path))
    }
}
