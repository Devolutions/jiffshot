//! The Linux control server that `jiffshot-cli` talks to.

use super::capture::{
    self, CaptureCursor, RecordingAudio, RecordingCommand, RecordingFrameRate, RecordingState,
    RecordingWebcam,
};
use jiffshot_control::{Error, ErrorCode, Request, Response};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const COMMANDS: &[(&str, &str)] = &[
    ("status", "server, protocol, library folder and recording"),
    (
        "screenshot",
        "capture a monitor or window through the desktop portal",
    ),
    ("ocr", "recognize text in an image file"),
    ("library.list", "the newest saved captures"),
    ("library.search", "search saved captures by text, tag or filter"),
    ("library.tag", "replace the tags of a saved capture"),
    ("library.pin", "pin or unpin a saved capture"),
    ("record.status", "the current recording"),
    ("record.start", "start a portal screen recording"),
    ("record.pause", "pause or resume the recording"),
    ("record.stop", "stop and save the recording"),
    ("record.cancel", "discard the recording"),
];

const RECORD_START_TIMEOUT: Duration = Duration::from_secs(60);
const RECORD_FINISH_TIMEOUT: Duration = Duration::from_secs(300);
const STATE_TIMEOUT: Duration = Duration::from_secs(5);

type Reply = Result<Value, Error>;
type Outcome = Result<Option<PathBuf>, String>;

struct ActiveRecording {
    commands: async_channel::Sender<RecordingCommand>,
    state: Arc<Mutex<RecordingState>>,
    finished: Receiver<Outcome>,
}

static RECORDING: Mutex<Option<ActiveRecording>> = Mutex::new(None);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Starts the per-user control socket. Another running instance owns it, in
/// which case this one keeps working without a control server.
pub fn start() -> Option<jiffshot_control::Server> {
    match jiffshot_control::serve_owned(handle) {
        Ok(server) => Some(server),
        Err(error) => {
            eprintln!("jiffshot: control socket unavailable: {error}");
            None
        }
    }
}

fn handle(request: Request) -> Response {
    match dispatch(&request.command, &request.args) {
        Ok(result) => Response::ok(&request.id, result),
        Err(error) => Response::err(&request.id, error),
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidRequest, message)
}

fn internal(message: impl ToString) -> Error {
    Error::new(ErrorCode::Internal, message.to_string())
}

/// Arguments from the CLI arrive as strings; accept native JSON types too.
struct Args<'a>(&'a Map<String, Value>);

impl<'a> Args<'a> {
    fn new(value: &'a Value, allowed: &[&str]) -> Result<Self, Error> {
        static EMPTY: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);
        let map = match value {
            Value::Null => &*EMPTY,
            Value::Object(map) => map,
            _ => return Err(invalid("arguments must be an object")),
        };
        if let Some(key) = map.keys().find(|key| !allowed.contains(&key.as_str())) {
            return Err(invalid(format!("unknown argument: {key}")));
        }
        Ok(Self(map))
    }

    fn text(&self, key: &str) -> Result<Option<&'a str>, Error> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => Ok(Some(text)),
            Some(_) => Err(invalid(format!("{key} must be a string"))),
        }
    }

    fn flag(&self, key: &str) -> Result<bool, Error> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(false),
            Some(Value::Bool(value)) => Ok(*value),
            Some(Value::String(text)) if text == "true" => Ok(true),
            Some(Value::String(text)) if text == "false" => Ok(false),
            Some(_) => Err(invalid(format!("{key} must be true or false"))),
        }
    }

    fn count(&self, key: &str) -> Result<Option<usize>, Error> {
        let parsed = match self.0.get(key) {
            None | Some(Value::Null) => return Ok(None),
            Some(Value::Number(number)) => number.as_u64().map(|n| n as usize),
            Some(Value::String(text)) => text.parse().ok(),
            Some(_) => None,
        };
        parsed
            .map(Some)
            .ok_or_else(|| invalid(format!("{key} must be a whole number")))
    }

    /// `cursor` includes the pointer, `no_cursor` hides it (the default).
    fn cursor(&self) -> Result<CaptureCursor, Error> {
        let shown = self.flag("cursor")?;
        if shown && self.flag("no_cursor")? {
            return Err(invalid("choose either cursor or no_cursor"));
        }
        Ok(CaptureCursor::from_included(shown))
    }
}

/// Whether a recording started here or through the control socket is running.
pub fn recording_active() -> bool {
    lock(&RECORDING).is_some()
}

/// Starts a recording with the given audio source ("none", "system",
/// "microphone" or "both") and cursor visibility.
pub fn start_recording_with(audio: &str, cursor: bool, webcam: bool) -> Result<(), String> {
    let args = if cursor {
        json!({"audio": audio, "cursor": true, "webcam": webcam})
    } else {
        json!({"audio": audio, "no_cursor": true, "webcam": webcam})
    };
    dispatch("record.start", &args)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Stops the recording; `None` when it was cancelled.
pub fn stop_recording() -> Result<Option<PathBuf>, String> {
    let reply = dispatch("record.stop", &json!({})).map_err(|error| error.to_string())?;
    Ok(reply["path"].as_str().map(PathBuf::from))
}

fn dispatch(command: &str, args: &Value) -> Reply {
    match command {
        "status" => {
            Args::new(args, &[])?;
            let commands: Map<String, Value> = COMMANDS
                .iter()
                .map(|(name, description)| ((*name).to_owned(), json!(description)))
                .collect();
            Ok(json!({
                "app": "jiffshot",
                "version": env!("CARGO_PKG_VERSION"),
                "protocol": jiffshot_control::PROTOCOL,
                "pid": std::process::id(),
                "pipe": jiffshot_control::pipe_name().map_err(internal)?,
                "library": capture::capture_directory().ok(),
                "recording": recording_status(),
                "commands": commands,
            }))
        }
        "screenshot" => screenshot(&Args::new(args, &["out", "cursor", "no_cursor"])?),
        "open" => open(&Args::new(args, &["path"])?),
        "ocr" => ocr(&Args::new(args, &["path"])?),
        "library.list" => library_list(&Args::new(args, &["limit"])?),
        "library.search" => library_search(&Args::new(args, &["query", "filter", "limit"])?),
        "library.tag" => library_tag(&Args::new(args, &["path", "tags"])?),
        "library.pin" => library_pin(&Args::new(args, &["path"])?),
        "record.status" => {
            Args::new(args, &[])?;
            Ok(recording_status())
        }
        "record.start" => record_start(&Args::new(args, &["audio", "fps", "cursor", "no_cursor", "webcam"])?),
        "record.pause" => {
            Args::new(args, &[])?;
            record_pause()
        }
        "record.stop" => {
            Args::new(args, &[])?;
            record_finish(RecordingCommand::Stop)
        }
        "record.cancel" => {
            Args::new(args, &[])?;
            record_finish(RecordingCommand::Cancel)
        }
        other => Err(Error::new(
            ErrorCode::Unsupported,
            format!("unknown command: {other}"),
        )),
    }
}

fn screenshot(args: &Args) -> Reply {
    let cursor = args.cursor()?;
    let document = futures_lite::future::block_on(capture::capture_monitor(cursor))
        .map_err(|error| Error::new(ErrorCode::Unavailable, error))?;
    let path = match args.text("out")? {
        Some(out) => {
            let path = PathBuf::from(out);
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                std::fs::create_dir_all(parent).map_err(internal)?;
            }
            jiffshot_doc::io::export(&document, &path).map_err(internal)?;
            path
        }
        None => capture::save_capture(&document).map_err(internal)?,
    };
    Ok(json!({
        "path": absolute(&path),
        "width": document.width(),
        "height": document.height(),
    }))
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

type OpenHandler = Box<dyn Fn(PathBuf) + Send>;

static OPEN_HANDLER: Mutex<Option<OpenHandler>> = Mutex::new(None);

/// Registers how the UI opens a file requested through the control socket.
pub fn set_open_handler(handler: impl Fn(PathBuf) + Send + 'static) {
    *lock(&OPEN_HANDLER) = Some(Box::new(handler));
}

fn open(args: &Args) -> Reply {
    let path = args
        .text("path")?
        .ok_or_else(|| invalid("open needs a PATH"))?;
    let path = PathBuf::from(path);
    if !path.is_file() {
        return Err(Error::new(
            ErrorCode::NotFound,
            format!("no file at {}", path.display()),
        ));
    }
    match lock(&OPEN_HANDLER).as_ref() {
        Some(handler) => handler(path.clone()),
        None => {
            return Err(Error::new(
                ErrorCode::Unavailable,
                "the editor is not ready yet",
            ))
        }
    }
    Ok(json!({ "path": path }))
}

fn ocr(args: &Args) -> Reply {
    let path = args
        .text("path")?
        .ok_or_else(|| invalid("ocr needs --path IMAGE"))?;
    let path = Path::new(path);
    if !path.is_file() {
        return Err(Error::new(
            ErrorCode::NotFound,
            format!("no image at {}", path.display()),
        ));
    }
    let image = jiffshot_doc::io::open(path)
        .map_err(|error| invalid(format!("could not read {}: {error}", path.display())))?
        .flatten();
    let lines = super::ocr::Recognizer::new()
        .and_then(|mut recognizer| recognizer.recognize(&image))
        .map_err(internal)?;
    let text = lines
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let lines: Vec<Value> = lines
        .iter()
        .map(|line| json!({ "text": line.text, "rect": line.area }))
        .collect();
    Ok(json!({ "text": text, "lines": lines }))
}

fn library_list(args: &Args) -> Reply {
    let limit = args.count("limit")?.unwrap_or(8);
    let captures = capture::recent_captures().map_err(internal)?;
    let paths: Vec<_> = captures.iter().take(limit).collect();
    Ok(json!({ "captures": paths }))
}

fn library_search(args: &Args) -> Reply {
    use super::library::CaptureFilter;
    let filter = match args.text("filter")?.unwrap_or("all") {
        "all" => CaptureFilter::All,
        "images" => CaptureFilter::Images,
        "videos" => CaptureFilter::Videos,
        "pinned" => CaptureFilter::Pinned,
        "tagged" => CaptureFilter::Tagged,
        other => {
            return Err(invalid(format!(
                "unknown filter {other}; use all, images, videos, pinned or tagged"
            )))
        }
    };
    let limit = args.count("limit")?.unwrap_or(50);
    let captures = super::library::search_captures(args.text("query")?.unwrap_or(""), filter)
        .map_err(internal)?;
    let paths: Vec<_> = captures.iter().take(limit).collect();
    Ok(json!({ "captures": paths }))
}

/// A capture inside the library folder; sidecar edits are limited to those.
fn library_capture(args: &Args) -> Result<PathBuf, Error> {
    let path = args
        .text("path")?
        .ok_or_else(|| invalid("this command needs --path CAPTURE"))?;
    let missing = || {
        Error::new(
            ErrorCode::NotFound,
            format!("no saved capture at {path}"),
        )
    };
    let path = std::fs::canonicalize(absolute(Path::new(path))).map_err(|_| missing())?;
    let library = std::fs::canonicalize(capture::capture_directory().map_err(internal)?)
        .map_err(internal)?;
    if !path.is_file() || !path.starts_with(&library) {
        return Err(missing());
    }
    Ok(path)
}

fn library_tag(args: &Args) -> Reply {
    let path = library_capture(args)?;
    let tags = args
        .text("tags")?
        .ok_or_else(|| invalid("library tag needs --tags TEXT (empty removes tags)"))?;
    let tags = super::library::set_tags(&path, tags).map_err(internal)?;
    Ok(json!({ "path": path, "tags": tags }))
}

fn library_pin(args: &Args) -> Reply {
    let path = library_capture(args)?;
    let pinned = super::library::toggle_pin(&path).map_err(internal)?;
    Ok(json!({ "path": path, "pinned": pinned }))
}

fn recording_status() -> Value {
    match lock(&RECORDING).as_ref() {
        Some(recording) => {
            json!({ "active": true, "state": state_name(*lock(&recording.state)) })
        }
        None => json!({ "active": false }),
    }
}

fn state_name(state: RecordingState) -> &'static str {
    match state {
        RecordingState::Starting => "starting",
        RecordingState::Recording => "recording",
        RecordingState::Paused => "paused",
        RecordingState::Stopping => "stopping",
        RecordingState::Restarting => "restarting",
        RecordingState::Cancelling => "cancelling",
    }
}

fn record_audio(name: Option<&str>) -> Result<RecordingAudio, Error> {
    match name.unwrap_or("none") {
        "none" => Ok(RecordingAudio::Disabled),
        "system" => Ok(RecordingAudio::System),
        "microphone" | "mic" => Ok(RecordingAudio::Microphone),
        "both" => Ok(RecordingAudio::SystemAndMicrophone),
        other => Err(invalid(format!(
            "unknown audio {other}; use none, system, microphone or both"
        ))),
    }
}

fn record_start(args: &Args) -> Reply {
    let audio = record_audio(args.text("audio")?)?;
    let cursor = args.cursor()?;
    let webcam = if args.flag("webcam")? {
        RecordingWebcam::Enabled(super::settings::webcam_configuration().map_err(internal)?)
    } else {
        RecordingWebcam::Disabled
    };
    let frame_rate = match args.text("fps")? {
        Some(fps) => RecordingFrameRate::from_stored_name(fps)
            .map_err(|_| invalid("fps must be source, 15, 24, 30 or 60"))?,
        None => RecordingFrameRate::default(),
    };

    let mut slot = lock(&RECORDING);
    if slot.is_some() {
        return Err(Error::new(
            ErrorCode::Conflict,
            "a recording is already running",
        ));
    }
    let (commands, receiver) = async_channel::unbounded();
    let state = Arc::new(Mutex::new(RecordingState::Starting));
    let (done, finished) = mpsc::channel();
    let observed = Arc::clone(&state);
    std::thread::Builder::new()
        .name("control-record".into())
        .spawn(move || {
            let outcome = futures_lite::future::block_on(capture::record_monitor_with_audio(
                receiver,
                audio,
                webcam,
                cursor,
                frame_rate,
                move |next| *lock(&observed) = next,
            ));
            let _ = done.send(outcome);
        })
        .map_err(internal)?;
    *slot = Some(ActiveRecording {
        commands,
        state: Arc::clone(&state),
        finished,
    });
    drop(slot);

    // The portal may ask for consent, so wait until recording really begins.
    let started = Instant::now();
    loop {
        if *lock(&state) == RecordingState::Recording {
            return Ok(recording_status());
        }
        let finished_early = lock(&RECORDING)
            .as_ref()
            .and_then(|recording| recording.finished.try_recv().ok());
        if let Some(outcome) = finished_early {
            *lock(&RECORDING) = None;
            return Err(match outcome {
                Err(error) => Error::new(ErrorCode::Unavailable, error),
                Ok(_) => Error::new(ErrorCode::Conflict, "the recording was cancelled"),
            });
        }
        if started.elapsed() > RECORD_START_TIMEOUT {
            return Err(Error::new(
                ErrorCode::Timeout,
                "the recording has not started; check for a pending desktop prompt, then use record.status",
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn no_recording() -> Error {
    Error::new(ErrorCode::Conflict, "no recording is running")
}

pub(super) fn record_pause() -> Reply {
    let (commands, state) = {
        let slot = lock(&RECORDING);
        let recording = slot.as_ref().ok_or_else(no_recording)?;
        (recording.commands.clone(), Arc::clone(&recording.state))
    };
    let before = *lock(&state);
    if !matches!(before, RecordingState::Recording | RecordingState::Paused) {
        return Err(Error::new(
            ErrorCode::Conflict,
            format!("cannot pause while {}", state_name(before)),
        ));
    }
    commands
        .try_send(RecordingCommand::TogglePause)
        .map_err(|_| no_recording())?;
    let started = Instant::now();
    while started.elapsed() < STATE_TIMEOUT && *lock(&state) == before {
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(recording_status())
}

fn record_finish(command: RecordingCommand) -> Reply {
    let recording = lock(&RECORDING).take().ok_or_else(no_recording)?;
    recording
        .commands
        .try_send(command)
        .map_err(|_| no_recording())?;
    match recording.finished.recv_timeout(RECORD_FINISH_TIMEOUT) {
        Ok(Ok(Some(path))) => Ok(json!({ "path": absolute(&path) })),
        Ok(Ok(None)) => Ok(json!({ "cancelled": true })),
        Ok(Err(error)) => Err(internal(error)),
        Err(_) => Err(Error::new(
            ErrorCode::Timeout,
            "the recording did not finish in time",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_lists_the_available_commands() {
        let status = dispatch("status", &Value::Null).unwrap();
        assert_eq!(status["app"], "jiffshot");
        assert!(status["commands"]["record.start"].is_string());
        assert_eq!(status["recording"], json!({ "active": false }));
    }

    #[test]
    fn unknown_commands_and_arguments_are_rejected() {
        assert!(dispatch("nope", &Value::Null).is_err());
        assert!(dispatch("status", &json!({ "x": 1 })).is_err());
        assert!(dispatch("screenshot", &json!({ "cursor": "maybe" })).is_err());
        assert!(dispatch("screenshot", &json!({ "cursor": true, "no_cursor": true })).is_err());
        assert!(dispatch("record.start", &json!({ "audio": "loud" })).is_err());
        assert!(dispatch("record.start", &json!({ "fps": "5" })).is_err());
    }

    #[test]
    fn recording_commands_need_a_recording() {
        for command in ["record.pause", "record.stop", "record.cancel"] {
            let error = dispatch(command, &Value::Null).unwrap_err();
            assert_eq!(error.code, ErrorCode::Conflict, "{command}");
        }
    }

    #[test]
    fn ocr_validates_its_image() {
        assert_eq!(
            dispatch("ocr", &Value::Null).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        let missing = dispatch("ocr", &json!({ "path": "/nonexistent/a.png" })).unwrap_err();
        assert_eq!(missing.code, ErrorCode::NotFound);
    }

    #[test]
    fn library_list_respects_the_limit() {
        assert!(dispatch("library.list", &json!({ "limit": "x" })).is_err());
        let listed = dispatch("library.list", &json!({ "limit": "2" })).unwrap();
        assert!(listed["captures"].as_array().unwrap().len() <= 2);
    }
}
