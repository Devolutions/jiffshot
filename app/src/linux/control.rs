//! The Linux control server that `jiffshot-cli` talks to.

use super::capture::{
    self, CaptureCursor, PauseRequest, RecordingAnnotation, RecordingAudio, RecordingCommand,
    RecordingFrameRate, RecordingState, RecordingWebcam,
};
use jiffshot_control::{Error, ErrorCode, Request, Response};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

mod guide;

const COMMANDS: &[(&str, &str)] = &[
    ("status", "server, protocol, library folder and recording"),
    (
        "screenshot",
        "capture a monitor or window through the desktop portal",
    ),
    ("ocr", "recognize text in an image file"),
    ("library.list", "the newest saved captures"),
    (
        "library.search",
        "search saved captures by text, tag or filter",
    ),
    ("library.tag", "replace the tags of a saved capture"),
    ("library.pin", "pin or unpin a saved capture"),
    ("record.status", "the current recording"),
    ("record.start", "start a portal screen recording"),
    ("record.pause", "pause or resume the recording"),
    ("record.resume", "resume the recording"),
    ("record.mark", "mark a kept or discarded recording range"),
    ("record.annotate", "render an annotation into the recording"),
    ("record.clear", "remove recording annotations"),
    ("record.stop", "stop and save the recording"),
    ("record.cancel", "discard the recording"),
    ("steps.start", "start a manual step-by-step guide"),
    ("steps.add", "capture and describe a manual guide step"),
    ("steps.status", "the current guide and its steps"),
    (
        "steps.finish",
        "save the guide as a PNG and editable document",
    ),
    ("steps.cancel", "discard the current guide"),
    (
        "video.info",
        "inspect metadata; optionally decode frames for an exact count and CFR",
    ),
    ("video.verify", "decode and verify a saved video"),
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
                "steps": guide::status()?,
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
        "record.start" => record_start(&Args::new(
            args,
            &["audio", "fps", "cursor", "no_cursor", "webcam"],
        )?),
        "record.pause" => {
            Args::new(args, &[])?;
            record_pause()
        }
        "record.resume" => {
            Args::new(args, &[])?;
            record_resume()
        }
        "record.annotate" => record_annotate(&Args::new(args, &["kind", "style", "duration_ms"])?),
        "record.clear" => {
            Args::new(args, &[])?;
            record_clear()
        }
        "record.mark" => record_mark(&Args::new(args, &["label", "discard"])?),
        "record.stop" => {
            Args::new(args, &[])?;
            record_finish(RecordingCommand::Stop)
        }
        "record.cancel" => {
            Args::new(args, &[])?;
            record_finish(RecordingCommand::Cancel)
        }
        "steps.start" => guide::start(&Args::new(args, &["title", "restart"])?),
        "steps.add" => guide::add(&Args::new(args, &["at", "right", "description"])?),
        "steps.status" => {
            Args::new(args, &[])?;
            guide::status()
        }
        "steps.finish" => guide::finish(&Args::new(args, &["out", "title", "open"])?),
        "steps.cancel" => {
            Args::new(args, &[])?;
            guide::cancel()
        }
        command if command.starts_with("video.") => {
            jiffshot_video::dispatch(command, args.clone(), &|| false)
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
    let missing = || Error::new(ErrorCode::NotFound, format!("no saved capture at {path}"));
    let path = std::fs::canonicalize(absolute(Path::new(path))).map_err(|_| missing())?;
    let library =
        std::fs::canonicalize(capture::capture_directory().map_err(internal)?).map_err(internal)?;
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

fn recording_command_error(error: async_channel::TrySendError<RecordingCommand>) -> Error {
    match error {
        async_channel::TrySendError::Full(_) => {
            Error::new(ErrorCode::Conflict, "the recording command queue is full")
        }
        async_channel::TrySendError::Closed(_) => no_recording(),
    }
}

fn record_mark(args: &Args) -> Reply {
    let label = args.text("label")?.map(str::to_owned);
    let discard = args.flag("discard")?;
    if label.is_some() == discard {
        return Err(invalid("provide exactly one of label or discard=true"));
    }
    if label
        .as_ref()
        .is_some_and(|label| label.trim().is_empty() || label.chars().count() > 128)
    {
        return Err(invalid("marker label must contain 1..128 characters"));
    }
    let (commands, state) = {
        let slot = lock(&RECORDING);
        let recording = slot.as_ref().ok_or_else(no_recording)?;
        (recording.commands.clone(), Arc::clone(&recording.state))
    };
    if !matches!(
        *lock(&state),
        RecordingState::Recording | RecordingState::Paused
    ) {
        return Err(Error::new(
            ErrorCode::Conflict,
            "the recording is not running",
        ));
    }
    let (result, receiver) = mpsc::channel();
    commands
        .try_send(RecordingCommand::Mark {
            label: label.clone(),
            discard,
            result,
        })
        .map_err(recording_command_error)?;
    let at_ms = receiver
        .recv_timeout(STATE_TIMEOUT)
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => Error::new(
                ErrorCode::Timeout,
                "the recording marker did not complete in time",
            ),
            mpsc::RecvTimeoutError::Disconnected => no_recording(),
        })?
        .map_err(invalid)?;
    Ok(json!({ "at_ms": at_ms, "label": label, "discard": discard }))
}

fn annotation_commands() -> Result<async_channel::Sender<RecordingCommand>, Error> {
    let slot = lock(&RECORDING);
    let recording = slot.as_ref().ok_or_else(no_recording)?;
    if !matches!(
        *lock(&recording.state),
        RecordingState::Recording | RecordingState::Paused
    ) {
        return Err(Error::new(
            ErrorCode::Conflict,
            "the recording is not running",
        ));
    }
    Ok(recording.commands.clone())
}

fn record_annotate(args: &Args) -> Reply {
    let kind = jiffshot_video::annotation::kind(
        args.0
            .get("kind")
            .ok_or_else(|| invalid("kind is required; use the same schema as doc.add"))?,
    )?;
    let style = jiffshot_video::annotation::style(
        args.0.get("style"),
        jiffshot_doc::object::Style::default(),
    )?;
    let duration_ms = match args.0.get("duration_ms") {
        None | Some(Value::Null) => Some(0),
        Some(Value::Number(value)) => value.as_u64(),
        Some(Value::String(value)) => value.parse::<u64>().ok(),
        Some(_) => None,
    }
    .filter(|duration| *duration <= 3_600_000)
    .ok_or_else(|| invalid("duration_ms must be a whole number from 0 to 3600000"))?;
    let commands = annotation_commands()?;
    let (result, response) = mpsc::channel();
    commands
        .try_send(RecordingCommand::Annotate {
            annotation: Box::new(RecordingAnnotation {
                kind,
                style,
                duration_ms,
            }),
            result,
        })
        .map_err(recording_command_error)?;
    let (annotation_id, at_ms) = response
        .recv_timeout(STATE_TIMEOUT)
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => Error::new(
                ErrorCode::Timeout,
                "the recording annotation did not complete in time",
            ),
            mpsc::RecvTimeoutError::Disconnected => no_recording(),
        })?
        .map_err(invalid)?;
    Ok(json!({
        "annotation_id": annotation_id,
        "at_ms": at_ms,
        "coordinates": "recording pixels",
        "duration_ms": duration_ms
    }))
}

fn record_clear() -> Reply {
    let commands = annotation_commands()?;
    let (result, response) = mpsc::channel();
    commands
        .try_send(RecordingCommand::ClearAnnotations { result })
        .map_err(recording_command_error)?;
    let at_ms = response
        .recv_timeout(STATE_TIMEOUT)
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => Error::new(
                ErrorCode::Timeout,
                "the recording annotation clear did not complete in time",
            ),
            mpsc::RecvTimeoutError::Disconnected => no_recording(),
        })?
        .map_err(internal)?;
    Ok(json!({ "cleared": true, "at_ms": at_ms }))
}

fn record_guide_caption(text: &str) -> Result<(), Error> {
    let commands = {
        let slot = lock(&RECORDING);
        let Some(recording) = slot.as_ref() else {
            return Ok(());
        };
        if !matches!(
            *lock(&recording.state),
            RecordingState::Recording | RecordingState::Paused
        ) {
            return Err(Error::new(
                ErrorCode::Conflict,
                "the recording is not running",
            ));
        }
        recording.commands.clone()
    };
    let (result, response) = mpsc::channel();
    commands
        .try_send(RecordingCommand::GuideCaption {
            text: text.to_owned(),
            result,
        })
        .map_err(recording_command_error)?;
    response
        .recv_timeout(STATE_TIMEOUT)
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => Error::new(
                ErrorCode::Timeout,
                "the recording guide caption did not complete in time; it may still be recorded",
            ),
            mpsc::RecvTimeoutError::Disconnected => no_recording(),
        })?
        .map_err(invalid)?;
    Ok(())
}

pub(super) fn record_pause() -> Reply {
    change_recording_pause(PauseRequest::Toggle)
}

fn record_resume() -> Reply {
    change_recording_pause(PauseRequest::Resume)
}

fn change_recording_pause(request: PauseRequest) -> Reply {
    let (commands, state) = {
        let slot = lock(&RECORDING);
        let recording = slot.as_ref().ok_or_else(no_recording)?;
        (recording.commands.clone(), Arc::clone(&recording.state))
    };
    let state = set_recording_pause(&commands, &state, request, STATE_TIMEOUT)?;
    Ok(json!({ "active": true, "state": state_name(state) }))
}

fn set_recording_pause(
    commands: &async_channel::Sender<RecordingCommand>,
    state: &Mutex<RecordingState>,
    request: PauseRequest,
    timeout: Duration,
) -> Result<RecordingState, Error> {
    let before = *lock(state);
    if !matches!(before, RecordingState::Recording | RecordingState::Paused) {
        return Err(Error::new(
            ErrorCode::Conflict,
            format!("cannot change pause state while {}", state_name(before)),
        ));
    }
    let (result, response) = mpsc::channel();
    commands
        .try_send(RecordingCommand::RequestPause { request, result })
        .map_err(recording_command_error)?;
    response
        .recv_timeout(timeout)
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => Error::new(
                ErrorCode::Timeout,
                "the recording pause request did not complete in time",
            ),
            mpsc::RecvTimeoutError::Disconnected => no_recording(),
        })?
        .map_err(internal)
}

fn record_finish(command: RecordingCommand) -> Reply {
    let recording = enqueue_recording_finish(&mut lock(&RECORDING), command)?;
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

fn enqueue_recording_finish(
    slot: &mut Option<ActiveRecording>,
    command: RecordingCommand,
) -> Result<ActiveRecording, Error> {
    let result = slot
        .as_ref()
        .ok_or_else(no_recording)?
        .commands
        .try_send(command);
    match result {
        Ok(()) => slot.take().ok_or_else(no_recording),
        Err(error @ async_channel::TrySendError::Full(_)) => Err(recording_command_error(error)),
        Err(async_channel::TrySendError::Closed(_)) => {
            drop(slot.take());
            Err(no_recording())
        }
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
        assert!(status["commands"]["record.resume"].is_string());
        assert!(status["commands"]["video.edit"].is_string());
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
        for command in [
            "record.pause",
            "record.resume",
            "record.stop",
            "record.cancel",
        ] {
            let error = dispatch(command, &Value::Null).unwrap_err();
            assert_eq!(error.code, ErrorCode::Conflict, "{command}");
        }
    }

    #[test]
    fn pause_toggles_and_resume_is_idempotent() {
        let (commands, receiver) = async_channel::bounded(2);
        let state = Arc::new(Mutex::new(RecordingState::Recording));
        let worker_state = Arc::clone(&state);
        let worker = std::thread::spawn(move || {
            for (expected_request, expected_state) in [
                (PauseRequest::Toggle, RecordingState::Paused),
                (PauseRequest::Resume, RecordingState::Recording),
                (PauseRequest::Resume, RecordingState::Recording),
            ] {
                let RecordingCommand::RequestPause { request, result } =
                    receiver.recv_blocking().unwrap()
                else {
                    panic!("expected an acknowledged pause request");
                };
                assert_eq!(request, expected_request);
                *lock(&worker_state) = expected_state;
                result.send(Ok(expected_state)).unwrap();
            }
        });
        set_recording_pause(
            &commands,
            &state,
            PauseRequest::Toggle,
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(*lock(&state), RecordingState::Paused);
        set_recording_pause(
            &commands,
            &state,
            PauseRequest::Resume,
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(*lock(&state), RecordingState::Recording);
        set_recording_pause(
            &commands,
            &state,
            PauseRequest::Resume,
            Duration::from_secs(1),
        )
        .unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn pause_reports_a_timeout_instead_of_a_stale_success() {
        let (commands, _receiver) = async_channel::bounded(1);
        let state = Mutex::new(RecordingState::Recording);
        let error = set_recording_pause(
            &commands,
            &state,
            PauseRequest::Toggle,
            Duration::from_millis(1),
        )
        .expect_err("an unprocessed pause must time out");
        assert_eq!(error.code, ErrorCode::Timeout);
        assert_eq!(*lock(&state), RecordingState::Recording);
    }

    #[test]
    fn pause_reports_a_full_command_queue() {
        let (commands, _receiver) = async_channel::bounded(1);
        commands.try_send(RecordingCommand::TogglePause).unwrap();
        let state = Mutex::new(RecordingState::Recording);
        let error = set_recording_pause(
            &commands,
            &state,
            PauseRequest::Toggle,
            Duration::from_secs(1),
        )
        .expect_err("a full queue must not be reported as a successful pause");
        assert_eq!(error.code, ErrorCode::Conflict);
        assert!(error.message.contains("queue is full"));
    }

    #[test]
    fn pause_propagates_recorder_errors() {
        let (commands, receiver) = async_channel::bounded(1);
        let state = Mutex::new(RecordingState::Recording);
        let worker = std::thread::spawn(move || {
            let RecordingCommand::RequestPause { result, .. } = receiver.recv_blocking().unwrap()
            else {
                panic!("expected an acknowledged pause request");
            };
            result
                .send(Err("recording clock unavailable".into()))
                .unwrap();
        });
        let error = set_recording_pause(
            &commands,
            &state,
            PauseRequest::Toggle,
            Duration::from_secs(1),
        )
        .expect_err("the recorder error must reach the caller");
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(error.message.contains("recording clock unavailable"));
        worker.join().unwrap();
    }

    #[test]
    fn stop_preserves_the_active_recording_when_the_command_queue_is_full() {
        let (commands, receiver) = async_channel::bounded(1);
        commands.try_send(RecordingCommand::TogglePause).unwrap();
        let (_done, finished) = mpsc::channel();
        let mut slot = Some(ActiveRecording {
            commands,
            state: Arc::new(Mutex::new(RecordingState::Recording)),
            finished,
        });
        let error = match enqueue_recording_finish(&mut slot, RecordingCommand::Stop) {
            Err(error) => error,
            Ok(_) => panic!("a full queue must not orphan the active recording"),
        };
        assert_eq!(error.code, ErrorCode::Conflict);
        assert!(slot.is_some());
        assert!(matches!(
            receiver.try_recv().unwrap(),
            RecordingCommand::TogglePause
        ));
        enqueue_recording_finish(&mut slot, RecordingCommand::Stop)
            .expect("stop can be retried after the queue drains");
        assert!(slot.is_none());
        assert!(matches!(
            receiver.try_recv().unwrap(),
            RecordingCommand::Stop
        ));
    }

    #[test]
    fn stop_clears_a_recording_with_a_closed_command_queue() {
        let (commands, receiver) = async_channel::bounded(1);
        drop(receiver);
        let (_done, finished) = mpsc::channel();
        let mut slot = Some(ActiveRecording {
            commands,
            state: Arc::new(Mutex::new(RecordingState::Recording)),
            finished,
        });
        assert!(enqueue_recording_finish(&mut slot, RecordingCommand::Stop).is_err());
        assert!(slot.is_none());
    }

    #[test]
    fn recording_mark_validates_arguments_before_looking_for_a_recording() {
        for args in [
            json!({}),
            json!({ "label": "intro", "discard": true }),
            json!({ "label": " " }),
            json!({ "label": "x".repeat(129) }),
            json!({ "discard": "perhaps" }),
        ] {
            let error = dispatch("record.mark", &args).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidRequest, "{args}");
        }
        for args in [json!({ "label": "intro" }), json!({ "discard": true })] {
            let error = dispatch("record.mark", &args).unwrap_err();
            assert_eq!(error.code, ErrorCode::Conflict, "{args}");
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

    #[test]
    #[ignore = "requires an isolated control socket and a built jiffshot-cli"]
    fn linux_video_cli_edits_and_verifies_over_real_control_socket() {
        use jiffshot_capture::media::{VideoSpec, Writer, HNS_PER_SECOND};
        use jiffshot_video::recording_marks::{self, Marker, RecordingMarks};

        assert!(std::env::var_os("JIFFSHOT_UITEST").is_some());
        let root = PathBuf::from(std::env::var_os("JIFFSHOT_TEST_ROOT").expect("test root"));
        let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").expect("private runtime"));
        assert_eq!(
            runtime.canonicalize().unwrap(),
            root.join("runtime").canonicalize().unwrap(),
            "the control socket must use the private test runtime"
        );
        let cli = std::env::var_os("JIFFSHOT_CLI_E2E").expect("built CLI path");
        let fixtures = tempfile::tempdir_in(&root).expect("fixture directory");
        let source = fixtures.path().join("source.mp4");
        let mut writer =
            Writer::create(&source, VideoSpec::for_size(64, 48, 12), None).expect("encoder");
        let pixels = [0_u8, 0, 240, 0].repeat(64 * 48);
        for n in 0..12 {
            let at = n * HNS_PER_SECOND / 12;
            let duration = (n + 1) * HNS_PER_SECOND / 12 - at;
            writer.write_frame(&pixels, at, duration).expect("frame");
        }
        writer.finish().expect("finalize source");
        let marks = RecordingMarks::new(
            1_000,
            12,
            64,
            48,
            vec![
                Marker::Label {
                    at_ms: 250,
                    label: "demo".into(),
                },
                Marker::Discard { at_ms: 400 },
                Marker::Discard { at_ms: 500 },
                Marker::Label {
                    at_ms: 750,
                    label: "demo".into(),
                },
            ],
        )
        .with_activity_tracking_complete(true);
        recording_marks::write(&source, &marks).expect("write fixture sidecar");
        let server = jiffshot_control::serve_owned(handle).expect("isolated control server");
        let invoke = |args: &[&str], success: bool| {
            let output = std::process::Command::new(&cli)
                .arg("--no-launch")
                .args(args)
                .output()
                .expect("run CLI");
            let response: Value = serde_json::from_slice(&output.stdout)
                .unwrap_or_else(|error| panic!("invalid CLI response: {error}: {output:?}"));
            assert_eq!(output.status.success(), success, "{response}");
            assert_eq!(response["ok"], success, "{response}");
            response
        };
        let path = source.to_str().unwrap();
        let status = invoke(&["status"], true);
        assert!(status["result"]["commands"]["video.edit"].is_string());
        assert!(status["result"]["commands"]["record.mark"].is_string());
        assert!(status["result"]["commands"]["record.resume"].is_string());
        assert!(status["result"]["commands"]["record.annotate"].is_string());
        assert!(status["result"]["commands"]["record.clear"].is_string());
        for command in [
            "steps.start",
            "steps.add",
            "steps.status",
            "steps.finish",
            "steps.cancel",
        ] {
            assert!(
                status["result"]["commands"][command].is_string(),
                "{command}"
            );
        }
        assert_eq!(status["result"]["steps"]["active"], false);
        let guide = invoke(&["steps", "start", "Manual guide"], true);
        assert_eq!(guide["result"]["title"], "Manual guide");
        assert_eq!(invoke(&["steps", "status"], true)["result"]["active"], true);
        assert_eq!(
            invoke(&["status"], true)["result"]["steps"]["title"],
            "Manual guide"
        );
        assert_eq!(
            invoke(&["steps", "start"], false)["error"]["code"],
            "conflict"
        );
        assert_eq!(
            invoke(&["steps", "add", "--description", "Open"], false)["error"]["code"],
            "invalid_request"
        );
        assert_eq!(
            invoke(&["steps", "add", "4,5"], false)["error"]["code"],
            "invalid_request"
        );
        assert_eq!(
            invoke(&["steps", "finish"], false)["error"]["code"],
            "conflict"
        );
        assert_eq!(invoke(&["steps", "cancel"], true)["result"]["steps"], 0);
        assert_eq!(
            invoke(&["steps", "status"], true)["result"]["active"],
            false
        );
        let info = invoke(&["video", "info", path], true);
        assert_eq!(info["result"]["width"], 64);
        assert!(info["result"]["frames"].is_null());
        let exact_info = invoke(&["video", "info", path, "--exact"], true);
        assert_eq!(exact_info["result"]["frames"], 12);
        assert_eq!(exact_info["result"]["constant_frame_rate"], true);
        let selected = fixtures.path().join("selected.mp4");
        let selected_path = selected.to_str().unwrap();
        let export = invoke(
            &[
                "video",
                "export",
                path,
                "--out",
                selected_path,
                "--keep-marked",
                "--drop-discarded",
                "--fps",
                "20",
            ],
            true,
        );
        assert_eq!(export["result"]["frames"], 8);
        let verified = invoke(&["video", "verify", selected_path], true);
        assert_eq!(verified["result"]["frames"], 8);

        let idle = fixtures.path().join("idle.mp4");
        let idle_result = invoke(
            &[
                "video",
                "export",
                path,
                "--out",
                idle.to_str().unwrap(),
                "--idle-after",
                "0.2",
                "--idle-mode",
                "cut",
                "--min-pause",
                "0.1",
                "--fps",
                "12",
            ],
            true,
        );
        assert!(idle_result["result"]["duration"].as_f64().unwrap() < 1.0);
        let incomplete = fixtures.path().join("incomplete.mp4");
        std::fs::copy(&source, &incomplete).expect("copy source");
        recording_marks::write(
            &incomplete,
            &RecordingMarks::new(1_000, 12, 64, 48, Vec::new()),
        )
        .expect("incomplete sidecar");
        let rejected_path = fixtures.path().join("rejected.mp4");
        let rejected = invoke(
            &[
                "video",
                "export",
                incomplete.to_str().unwrap(),
                "--out",
                rejected_path.to_str().unwrap(),
                "--idle-after",
                "0.2",
            ],
            false,
        );
        assert_eq!(rejected["error"]["code"], "invalid_request");
        assert!(!rejected_path.exists());

        let plan = fixtures.path().join("edit.json");
        std::fs::write(
            &plan,
            serde_json::to_vec(&json!({
                "fps": 12,
                "segments": [{"path":"source.mp4", "from":0.0, "to":1.0, "speed":2.0}],
                "outputs": [
                    {"format":"mp4", "path":"edited.mp4"},
                    {"format":"gif", "path":"preview.gif", "fps":6, "max_width":32}
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        let edited = invoke(&["video", "edit", plan.to_str().unwrap()], true);
        assert_eq!(edited["result"][0]["frames"], 6);
        assert_eq!(edited["result"][1]["format"], "gif");
        let verified = invoke(
            &[
                "video",
                "verify",
                fixtures.path().join("edited.mp4").to_str().unwrap(),
            ],
            true,
        );
        assert_eq!(verified["result"]["frames"], 6);
        let no_recording = invoke(&["record", "mark", "--label", "demo"], false);
        assert_eq!(no_recording["error"]["code"], "conflict");
        let no_recording = invoke(&["record", "resume"], false);
        assert_eq!(no_recording["error"]["code"], "conflict");
        let no_recording = invoke(&["record", "clear"], false);
        assert_eq!(no_recording["error"]["code"], "conflict");

        let (commands, receiver) = async_channel::bounded(4);
        let state = Arc::new(Mutex::new(RecordingState::Recording));
        let worker_state = Arc::clone(&state);
        let (_done, finished) = mpsc::channel();
        *lock(&RECORDING) = Some(ActiveRecording {
            commands,
            state,
            finished,
        });
        let worker = std::thread::spawn(move || {
            for _ in 0..3 {
                let RecordingCommand::RequestPause { request, result } =
                    receiver.recv_blocking().unwrap()
                else {
                    panic!("expected an acknowledged pause request");
                };
                let mut state = lock(&worker_state);
                *state = match request {
                    PauseRequest::Toggle if *state == RecordingState::Recording => {
                        RecordingState::Paused
                    }
                    PauseRequest::Toggle | PauseRequest::Resume => RecordingState::Recording,
                };
                result.send(Ok(*state)).unwrap();
            }
            let RecordingCommand::Annotate { annotation, result } =
                receiver.recv_blocking().unwrap()
            else {
                panic!("expected an annotation request");
            };
            let RecordingAnnotation {
                kind,
                style,
                duration_ms,
            } = *annotation;
            assert!(matches!(
                kind,
                jiffshot_doc::object::Kind::Text { text, .. } if text == "Open settings"
            ));
            assert_eq!(style.font_size, 24.0);
            assert_eq!(duration_ms, 1200);
            result.send(Ok((1, 200))).unwrap();
            let RecordingCommand::ClearAnnotations { result } = receiver.recv_blocking().unwrap()
            else {
                panic!("expected an annotation clear request");
            };
            result.send(Ok(700)).unwrap();
        });
        assert_eq!(
            invoke(&["record", "pause"], true)["result"]["state"],
            "paused"
        );
        assert_eq!(
            invoke(&["record", "resume"], true)["result"]["state"],
            "recording"
        );
        assert_eq!(
            invoke(&["record", "resume"], true)["result"]["state"],
            "recording"
        );
        let kind =
            r#"{"type":"Text","rect":{"x0":10,"y0":20,"x1":200,"y1":80},"text":"Open settings"}"#;
        let annotation = invoke(
            &[
                "record",
                "annotate",
                "--kind",
                kind,
                "--style",
                r#"{"font_size":24}"#,
                "--duration-ms",
                "1200",
            ],
            true,
        );
        assert_eq!(annotation["result"]["annotation_id"], 1);
        assert_eq!(annotation["result"]["at_ms"], 200);
        let rejected = invoke(
            &[
                "record",
                "annotate",
                "--kind",
                kind,
                "--duration-ms",
                "3600001",
            ],
            false,
        );
        assert_eq!(rejected["error"]["code"], "invalid_request");
        let cleared = invoke(&["record", "clear"], true);
        assert_eq!(cleared["result"]["cleared"], true);
        assert_eq!(cleared["result"]["at_ms"], 700);
        worker.join().expect("pause controller should finish");
        drop(lock(&RECORDING).take());
        server.shutdown().expect("shut down test control server");
    }

    #[test]
    #[ignore = "requires a private Wayland portal chooser that rejects three requests then accepts one"]
    fn linux_portal_chooser_cancellation_preserves_control_state() {
        assert!(std::env::var_os("JIFFSHOT_UITEST").is_some());
        let root = PathBuf::from(std::env::var_os("JIFFSHOT_TEST_ROOT").expect("test root"))
            .canonicalize()
            .expect("canonical test root");
        let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").expect("private runtime"))
            .canonicalize()
            .expect("canonical runtime");
        let library = PathBuf::from(std::env::var_os("JIFFSHOT_LIBRARY").expect("test library"))
            .canonicalize()
            .expect("canonical test library");
        assert_eq!(runtime, root.join("runtime"));
        assert!(library.starts_with(&root), "library must be isolated");
        let directory = tempfile::tempdir_in(&library).expect("create isolated capture directory");
        let library = directory.path();
        super::super::settings::set_capture_directory(library).expect("isolate capture library");
        let cli = std::env::var_os("JIFFSHOT_CLI_E2E").expect("built CLI path");
        let server = jiffshot_control::serve_owned(handle).expect("isolated control server");
        let invoke = |args: &[&str], success: bool| {
            let output = std::process::Command::new(&cli)
                .arg("--no-launch")
                .args(args)
                .output()
                .expect("run CLI");
            let response: Value = serde_json::from_slice(&output.stdout)
                .unwrap_or_else(|error| panic!("invalid CLI response: {error}: {output:?}"));
            assert_eq!(output.status.success(), success, "{args:?}: {response}");
            assert_eq!(response["ok"], success, "{args:?}: {response}");
            response
        };

        let screenshot = library.join("cancelled.png");
        let denied = invoke(
            &["screenshot", "--out", screenshot.to_str().unwrap()],
            false,
        );
        assert_eq!(denied["error"]["code"], "unavailable");
        assert!(!screenshot.exists());

        let denied = invoke(&["record", "start", "--audio", "none"], false);
        assert_eq!(denied["error"]["code"], "unavailable");
        assert_eq!(
            invoke(&["record", "status"], true)["result"]["active"],
            false
        );

        invoke(&["steps", "start", "Cancelled guide"], true);
        let denied = invoke(
            &["steps", "add", "120,80", "--description", "Denied step"],
            false,
        );
        assert_eq!(denied["error"]["code"], "unavailable");
        let guide = invoke(&["steps", "status"], true);
        assert_eq!(guide["result"]["active"], true);
        assert_eq!(guide["result"]["steps"], json!([]));
        invoke(&["steps", "cancel"], true);

        let started = invoke(&["record", "start", "--audio", "none"], true);
        assert_eq!(started["result"]["state"], "recording");
        assert_eq!(
            invoke(&["record", "cancel"], true)["result"]["cancelled"],
            true
        );
        assert_eq!(
            invoke(&["record", "status"], true)["result"]["active"],
            false
        );
        assert!(
            std::fs::read_dir(library)
                .expect("read capture directory")
                .next()
                .is_none(),
            "cancelled captures must not leave output files"
        );
        server.shutdown().expect("shut down test control server");
    }

    #[test]
    #[ignore = "requires a private Wayland desktop with animation, ScreenCast portal and built jiffshot-cli"]
    fn linux_portal_recording_and_manual_guides_over_control_socket() {
        use jiffshot_video::recording_marks::{self, Marker};

        assert!(std::env::var_os("JIFFSHOT_UITEST").is_some());
        let root = PathBuf::from(std::env::var_os("JIFFSHOT_TEST_ROOT").expect("test root"))
            .canonicalize()
            .expect("canonical test root");
        let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").expect("private runtime"))
            .canonicalize()
            .expect("canonical runtime");
        let library = PathBuf::from(std::env::var_os("JIFFSHOT_LIBRARY").expect("test library"))
            .canonicalize()
            .expect("canonical test library");
        assert_eq!(runtime, root.join("runtime"));
        assert!(library.starts_with(&root), "library must be isolated");
        let directory = tempfile::tempdir_in(&library).expect("create isolated capture directory");
        let library = directory.path();
        super::super::settings::set_capture_directory(library).expect("isolate capture library");
        let cli = std::env::var_os("JIFFSHOT_CLI_E2E").expect("built CLI path");
        let swaymsg = std::env::var_os("JIFFSHOT_TEST_SWAYMSG").expect("private compositor tool");
        let changed = std::process::Command::new(&swaymsg)
            .args(["output", "HEADLESS-1", "bg", "#882222", "solid_color"])
            .output()
            .expect("set the private desktop background");
        assert!(changed.status.success(), "{changed:?}");
        let server = jiffshot_control::serve_owned(handle).expect("isolated control server");
        let invoke = |args: &[&str]| {
            let output = std::process::Command::new(&cli)
                .arg("--no-launch")
                .args(args)
                .output()
                .expect("run CLI");
            let response: Value = serde_json::from_slice(&output.stdout)
                .unwrap_or_else(|error| panic!("invalid CLI response: {error}: {output:?}"));
            assert!(output.status.success(), "{args:?}: {response}");
            assert_eq!(response["ok"], true, "{response}");
            response["result"].clone()
        };
        let screenshot = library.join("portal-screen.png");
        let shot = invoke(&[
            "screenshot",
            "--out",
            screenshot.to_str().unwrap(),
            "--no-cursor",
        ]);
        assert_eq!(
            (shot["width"].as_u64(), shot["height"].as_u64()),
            (Some(960), Some(540))
        );
        assert!(screenshot.is_file());

        let recording = invoke(&[
            "record",
            "start",
            "--audio",
            "none",
            "--fps",
            "15",
            "--no-cursor",
        ]);
        assert_eq!(recording["state"], "recording");
        std::thread::sleep(Duration::from_millis(450));
        invoke(&["steps", "start", "Portal guide"]);
        let start = invoke(&["record", "mark", "--label", "demo"])["at_ms"]
            .as_u64()
            .unwrap();
        let start_wall = Instant::now();
        let first = invoke(&["steps", "add", "120,80", "--description", "First step"]);
        assert_eq!(first["index"], 1);
        assert_eq!(invoke(&["record", "pause"])["state"], "paused");
        let pause_wall = Instant::now();
        let paused_start = invoke(&["record", "mark", "--discard"])["at_ms"]
            .as_u64()
            .unwrap();
        let second = invoke(&[
            "steps",
            "add",
            "240,160",
            "--description",
            "Paused step",
            "--right",
        ]);
        assert_eq!(second["index"], 2);
        let changed = std::process::Command::new(swaymsg)
            .args(["output", "HEADLESS-1", "bg", "#2222cc", "solid_color"])
            .output()
            .expect("change the private desktop background");
        assert!(changed.status.success(), "{changed:?}");
        std::thread::sleep(Duration::from_millis(900));
        let paused_end = invoke(&["record", "mark", "--discard"])["at_ms"]
            .as_u64()
            .unwrap();
        assert_eq!(
            paused_start, paused_end,
            "recording time must freeze while paused"
        );
        assert_eq!(invoke(&["record", "resume"])["state"], "recording");
        let paused_duration = pause_wall.elapsed();
        std::thread::sleep(Duration::from_millis(1000));
        let end = invoke(&["record", "mark", "--label", "demo"])["at_ms"]
            .as_u64()
            .unwrap();
        let elapsed_without_pause = start_wall.elapsed().saturating_sub(paused_duration);
        assert!(
            ((end - start) as f64 / 1000.0 - elapsed_without_pause.as_secs_f64()).abs() < 0.7,
            "marker interval should exclude the portal pause"
        );
        let guide_path = library.join("portal-guide.png");
        let guide = invoke(&["steps", "finish", "--out", guide_path.to_str().unwrap()]);
        assert_eq!(guide["steps"].as_array().unwrap().len(), 2);
        assert!(guide_path.is_file());
        assert!(guide_path.with_extension("jsd").is_file());
        let editable = jiffshot_doc::io::open(&guide_path.with_extension("jsd"))
            .expect("reopen editable portal guide");
        assert_eq!(editable.size(), (1400, 1420));
        let saved = invoke(&["record", "stop"]);
        let path = PathBuf::from(saved["path"].as_str().expect("recorded path"));
        assert!(path.starts_with(library));
        let marks = recording_marks::read(&path)
            .unwrap()
            .expect("recording sidecar");
        assert_eq!(marks.captions().len(), 2);
        assert_eq!(marks.captions()[0].text, "First step");
        assert_eq!(marks.captions()[1].text, "Paused step");
        assert_eq!(
            marks.captions()[1].at_ms,
            paused_start,
            "saved marks: {marks:?}; requested marker interval: {start}..{end}"
        );
        assert!(marks.captions().iter().all(|caption| !caption.burned_in));
        assert_eq!(marks.markers().len(), 4);
        assert!(matches!(marks.markers()[0], Marker::Label { at_ms, .. } if at_ms == start));
        assert!(
            !marks.activities_complete(),
            "Wayland cannot prove complete input activity"
        );
        let video = path.to_str().unwrap();
        let verified = invoke(&["video", "verify", video]);
        let frames = verified["frames"].as_u64().unwrap();
        let fps = verified["fps"].as_f64().unwrap();
        assert!(frames > 10);
        assert!((fps - 15.0).abs() < 0.01, "recorded frame rate: {fps}");
        assert_eq!(
            verified["constant_frame_rate"], true,
            "a 15 fps recording must have evenly spaced decoded frames"
        );
        let duration = verified["duration"].as_f64().unwrap();
        assert!(
            (duration - frames as f64 / fps).abs() <= 1.0 / fps,
            "recording duration {duration} must match {frames} frames at {fps} fps"
        );
        let (before, _) = jiffshot_capture::media::post::frame(&path, 0.2).unwrap();
        let (after, _) = jiffshot_capture::media::post::frame(&path, duration - 0.2).unwrap();
        let before = before.pixel(400, 250).unwrap();
        let after = after.pixel(400, 250).unwrap();
        assert!(
            i16::from(before.red()) - i16::from(before.blue()) > 50,
            "before pause: {before:?}"
        );
        assert!(
            i16::from(after.blue()) - i16::from(after.red()) > 50,
            "after resume: {after:?}"
        );
        let resume_at = paused_start as f64 / 1000.0;
        assert!(
            resume_at + 0.7 < duration,
            "recording must contain the resumed samples: {resume_at}..{duration}"
        );
        let (early, _) = jiffshot_capture::media::post::frame(&path, resume_at + 0.25).unwrap();
        let (late, _) = jiffshot_capture::media::post::frame(&path, resume_at + 0.7).unwrap();
        let early_background = early.pixel(400, 250).unwrap();
        assert!(
            i16::from(early_background.blue()) - i16::from(early_background.red()) > 50,
            "the recording must promptly show the post-pause desktop: {early_background:?}"
        );
        let changed_pixels = (10..130)
            .flat_map(|y| (10..190).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let first = early.pixel(x, y).unwrap();
                let second = late.pixel(x, y).unwrap();
                first.red().abs_diff(second.red()) > 25
                    || first.green().abs_diff(second.green()) > 25
                    || first.blue().abs_diff(second.blue()) > 25
            })
            .count();
        assert!(
            changed_pixels > 25,
            "the animated surface must continue moving after the pause: {changed_pixels} pixels"
        );
        let output = library.join("portal-captioned.mp4");
        let exported = invoke(&[
            "video",
            "export",
            video,
            "--out",
            output.to_str().unwrap(),
            "--captions-from-recording",
            "--fps",
            "15",
        ]);
        assert!(exported["frames"].as_u64().unwrap() > 10);
        invoke(&["video", "verify", output.to_str().unwrap()]);
        let (captioned, _) =
            jiffshot_capture::media::post::frame(&output, resume_at + 0.7).unwrap();
        let source_pixel = late.pixel(30, 458).unwrap();
        let caption_pixel = captioned.pixel(30, 458).unwrap();
        assert!(
            i16::from(source_pixel.blue()) - i16::from(caption_pixel.blue()) > 60,
            "the recorded guide caption must be visible after export: {source_pixel:?} vs {caption_pixel:?}"
        );
        server.shutdown().expect("shut down private control server");
    }
}
