use super::{capture, document, internal, invalid, not_found, Args, Reply};
use crate::recorder::{self, Control, RecordingLease};
use jiffshot_capture::library::{Library, StagedRecording};
use jiffshot_capture::media::VideoSpec;
use jiffshot_control::{Error, ErrorCode};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

struct Recording {
    id: String,
    key: Option<String>,
    path: PathBuf,
    staging: StagedRecording,
    target: Value,
    fps: u32,
    started_at: String,
    control: Arc<Control>,
    worker: JoinHandle<Result<VideoSpec, String>>,
}

static RECORDING: Mutex<Option<Recording>> = Mutex::new(None);
static LAST_RECORDING: Mutex<Option<Value>> = Mutex::new(None);
static LIFECYCLE: Mutex<()> = Mutex::new(());
static FINALIZING: AtomicBool = AtomicBool::new(false);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub(super) fn dispatch(command: &str, a: &Args) -> Reply {
    match command {
        "record.status" => {
            a.only(&[])?;
            status()
        }
        "record.start" => start(a),
        "record.pause" => pause(a, true),
        "record.resume" => pause(a, false),
        "record.options" => options(a),
        "record.annotate" => annotate(a),
        "record.clear" => clear(a),
        "record.stop" => stop(a, false),
        "record.cancel" => stop(a, true),
        _ => Err(invalid(format!("unknown recording command {command:?}"))),
    }
}

fn conflict(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::Conflict, message)
}

fn recording_json(recording: &Recording) -> Reply {
    let c = &recording.control;
    let error = c.error.lock().map_err(|_| internal("the recording error state is unavailable"))?.clone();
    let audio_note = c.audio.lock().map_err(|_| internal("the recording audio state is unavailable"))?.clone();
    let webcam_corner = c.webcam_corner.lock().map_err(|_| internal("the recording webcam state is unavailable"))?.clone();
    Ok(json!({
        "active": true,
        "id": recording.id,
        "key": recording.key,
        "path": recording.path,
        "target": recording.target,
        "fps": recording.fps,
        "started_at": recording.started_at,
        "frames": c.frames.load(Ordering::Relaxed),
        "duration_ms": c.millis.load(Ordering::Relaxed),
        "paused": c.paused.load(Ordering::Relaxed),
        "ended": recording.worker.is_finished(),
        "error": error,
        "audio_note": audio_note,
        "system_silent": c.system_silent.load(Ordering::Relaxed),
        "microphone_silent": c.microphone_silent.load(Ordering::Relaxed),
        "options": {
            "cursor": c.cursor.load(Ordering::Relaxed),
            "highlight": c.highlight.load(Ordering::Relaxed),
            "clicks": c.clicks.load(Ordering::Relaxed),
            "webcam": c.webcam.load(Ordering::Relaxed),
            "webcam_circle": c.webcam_circle.load(Ordering::Relaxed),
            "webcam_corner": webcam_corner,
            "system": c.system.load(Ordering::Relaxed),
            "microphone": c.microphone.load(Ordering::Relaxed),
        },
    }))
}

pub(super) fn status() -> Reply {
    status_from(&RECORDING)
}

fn status_from(slot: &Mutex<Option<Recording>>) -> Reply {
    let recording = slot.lock().map_err(|_| internal("the recording state is unavailable"))?;
    match &*recording {
        Some(recording) => recording_json(recording),
        None => {
            let finalizing = FINALIZING.load(Ordering::Acquire);
            let last = LAST_RECORDING.lock().map_err(|_| internal("the last recording state is unavailable"))?;
            Ok(json!({
                "active": false,
                "finalizing": finalizing,
                "tray_recording": recorder::is_recording() && !finalizing,
                "last": *last,
            }))
        }
    }
}

fn validate_corner(corner: &str) -> Result<(), Error> {
    if matches!(corner, "top-left" | "top-right" | "bottom-left" | "bottom-right") {
        Ok(())
    } else {
        Err(invalid("webcam_corner must be top-left, top-right, bottom-left or bottom-right"))
    }
}

fn start(a: &Args) -> Reply {
    let mut allowed = capture::TARGET.to_vec();
    allowed.extend_from_slice(&[
        "out", "fps", "audio", "no_cursor", "clicks", "highlight", "max_seconds", "key",
        "webcam", "webcam_corner", "webcam_circle", "webcam_size", "noise_gate",
    ]);
    a.only(&allowed)?;
    let _lifecycle = LIFECYCLE.lock().map_err(|_| internal("the recording lifecycle state is unavailable"))?;
    let key = a.string("key")?;
    {
        let current = RECORDING.lock().map_err(|_| internal("the recording state is unavailable"))?;
        if let Some(recording) = current.as_ref() {
            if key.is_some() && recording.key == key {
                let mut result = recording_json(recording)?;
                result["already_started"] = json!(true);
                return Ok(result);
            }
            return Err(conflict(format!("recording {} is already running; stop it first", recording.id)));
        }
    }
    let audio = a.string("audio")?.unwrap_or_else(|| "none".into());
    let audio = if audio == "mic" { "microphone" } else { audio.as_str() };
    if !matches!(audio, "none" | "system" | "microphone" | "both") {
        return Err(invalid("audio must be none, system, microphone or both"));
    }
    let fps = a.int("fps", 5, 60)?.unwrap_or(30) as u32;
    let max = a.number("max_seconds")?.map(|seconds| {
        if !seconds.is_finite() || !(0.001..=86_400.0).contains(&seconds) {
            Err(invalid("max_seconds must be finite and between 0.001 and 86400"))
        } else {
            Ok(Duration::from_secs_f64(seconds))
        }
    }).transpose()?;
    let mut options = recorder::Options::audio_only(fps, audio);
    options.cursor = !a.bool("no_cursor")?;
    options.click_rings = a.bool("clicks")?;
    options.cursor_highlight = a.bool("highlight")?;
    let corner = a.string("webcam_corner")?.unwrap_or_else(|| "bottom-right".into());
    validate_corner(&corner)?;
    options.webcam_corner = corner.clone();
    options.webcam = a.bool("webcam")?.then_some(corner);
    options.webcam_circle = a.bool("webcam_circle")?;
    let size = a.number("webcam_size")?.unwrap_or(0.25);
    if !size.is_finite() || !(0.05..=0.75).contains(&size) {
        return Err(invalid("webcam_size must be 0.05..0.75 of the recording width"));
    }
    options.webcam_size = size as f32;
    options.noise_gate = a.bool("noise_gate")?;
    let path = match a.out("out", &["mp4"])? {
        Some(path) => path,
        None => Library::new(jiffshot_capture::config::library_root()).recording_path().map_err(internal)?,
    };
    match std::fs::symlink_metadata(&path) {
        Ok(_) => return Err(conflict("recording output already exists; choose a new path")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(internal(format!("cannot inspect {}: {error}", path.display()))),
    }
    if recorder::is_recording() {
        return Err(conflict("another recording is already running (tray or CLI)"));
    }
    let target = capture::target(a, false)?;
    if crate::shutdown_macos::requested() {
        return Err(conflict("Jiffshot is shutting down"));
    }
    let lease = RecordingLease::acquire().map_err(conflict)?;
    let staging = StagedRecording::for_destination(&path).map_err(internal)?;
    let id = NEXT_ID.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .map_err(|_| internal("too many recording sessions"))?;
    let mut state = Control::new(&options);
    state.max_duration = max;
    let control = Arc::new(state);
    let (c, staged_path, rect, layout) = (control.clone(), staging.path().to_path_buf(), target.rect, target.layout.clone());
    let worker = std::thread::Builder::new().name("jiffshot-control-record".into()).spawn(move || {
        let result = recorder::record_with_lease(&staged_path, rect, &options, &c, &layout, lease);
        if let Err(error) = &result {
            match c.error.lock() {
                Ok(mut state) => *state = Some(error.clone()),
                Err(_) => crate::log(&format!("could not store the recording error: {error}; the error state is unavailable")),
            }
        }
        c.stop.store(true, Ordering::Release);
        result
    }).map_err(internal)?;

    let began = Instant::now();
    while control.frames.load(Ordering::Acquire) == 0 && !worker.is_finished() {
        if crate::shutdown_macos::requested() || began.elapsed() >= Duration::from_secs(10) {
            control.stop.store(true, Ordering::Release);
            let result = worker.join().unwrap_or_else(|_| Err("the recorder crashed".into()));
            return Err(interrupted_start(staging, result, crate::shutdown_macos::requested));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if control.frames.load(Ordering::Acquire) == 0 {
        let result = worker.join().unwrap_or_else(|_| Err("the recorder crashed".into()));
        return Err(failed_start(staging, result));
    }
    let (finished_failure, state_unavailable) = if worker.is_finished() {
        match control.error.lock() {
            Ok(error) => (error.is_some(), false),
            Err(_) => (true, true),
        }
    } else {
        (false, false)
    };
    if finished_failure {
        let outcome = worker.join().unwrap_or_else(|_| Err("the recorder crashed".into()));
        let error = match (state_unavailable, outcome) {
            (true, Err(error)) => format!("the recording error state is unavailable; {error}"),
            (true, Ok(_)) => "the recording error state is unavailable".into(),
            (false, Err(error)) => error,
            (false, Ok(_)) => "the recorder stopped unexpectedly".into(),
        };
        return Err(discard_failed_start(staging, internal(format!("could not record: {error}"))));
    }
    let recording = Recording {
        id: format!("rec-{}-{id}", std::process::id()),
        key,
        path,
        staging,
        target: target.json(),
        fps,
        started_at: jiffshot_capture::library::timestamp(),
        control,
        worker,
    };
    let (recording, reply) = started_status(recording)?;
    let mut slot = match RECORDING.lock() {
        Ok(slot) => slot,
        Err(_) => return Err(cancel_started(recording, internal("the recording state is unavailable"))),
    };
    *slot = Some(recording);
    drop(slot);
    if crate::shutdown_macos::requested() {
        return Err(conflict("Jiffshot is shutting down; the recording will be finalized"));
    }
    Ok(reply)
}

fn with_start_cleanup(reason: Error, cleanup: std::io::Result<()>) -> Error {
    match cleanup {
        Ok(()) => reason,
        Err(error) => internal(format!("{}; {error}", reason.message)),
    }
}

fn discard_failed_start(staging: StagedRecording, reason: Error) -> Error {
    with_start_cleanup(reason, staging.discard())
}

fn failed_start(staging: StagedRecording, outcome: Result<VideoSpec, String>) -> Error {
    let error = match outcome {
        Err(error) => error,
        Ok(_) => "the recorder stopped before writing a frame".into(),
    };
    discard_failed_start(staging, internal(format!("could not record: {error}")))
}

fn interrupted_start(
    staging: StagedRecording,
    outcome: Result<VideoSpec, String>,
    shutting_down: impl FnOnce() -> bool,
) -> Error {
    let cleanup = staging.discard();
    let mut reason = if shutting_down() {
        conflict("Jiffshot is shutting down; the recording did not start")
    } else {
        Error::new(ErrorCode::Timeout, "the recording did not start within 10 s")
    };
    if let Err(error) = outcome {
        reason.message.push_str(&format!("; recorder error: {error}"));
    }
    with_start_cleanup(reason, cleanup)
}

fn started_status(recording: Recording) -> Result<(Recording, Value), Error> {
    match recording_json(&recording) {
        Ok(reply) => Ok((recording, reply)),
        Err(error) => Err(cancel_started(recording, error)),
    }
}

fn cancel_started(recording: Recording, reason: Error) -> Error {
    match finish(recording, true) {
        Ok(result) => {
            let worker_error = result.get("recording_error").and_then(Value::as_str)
                .map(|error| format!("; recorder error: {error}")).unwrap_or_default();
            internal(format!("could not report the started recording: {reason}; it was cancelled{worker_error}"))
        }
        Err(error) => internal(format!("could not report the started recording: {reason}; cancellation failed: {error}")),
    }
}

fn current<'a>(slot: &'a mut Option<Recording>, a: &Args) -> Result<&'a mut Recording, Error> {
    let id = a.string("id")?;
    match slot {
        None => Err(not_found("no recording is running")),
        Some(recording) if id.as_ref().is_some_and(|id| *id != recording.id) => {
            Err(not_found(format!("recording {} is the one running", recording.id)))
        }
        Some(recording) => Ok(recording),
    }
}

fn require_live(recording: &Recording) -> Result<(), Error> {
    if recording.worker.is_finished() || recording.control.stop.load(Ordering::Acquire) {
        Err(conflict("recording has ended; use record.stop to collect the finalized result"))
    } else {
        Ok(())
    }
}

fn pause(a: &Args, paused: bool) -> Reply {
    a.only(&["id"])?;
    let mut slot = RECORDING.lock().map_err(|_| internal("the recording state is unavailable"))?;
    let recording = current(&mut slot, a)?;
    require_live(recording)?;
    recording_json(recording)?;
    recording.control.paused.store(paused, Ordering::Release);
    recording_json(recording).map_err(|error| internal(format!(
        "the recording pause state changed, but its status could not be reported: {error}"
    )))
}

fn options(a: &Args) -> Reply {
    a.only(&["id", "cursor", "highlight", "clicks", "webcam", "webcam_circle", "webcam_corner", "system", "microphone"])?;
    let switches = ["cursor", "highlight", "clicks", "webcam", "webcam_circle", "system", "microphone"]
        .iter().map(|name| a.flag(name)).collect::<Result<Vec<_>, _>>()?;
    let corner = a.string("webcam_corner")?;
    if let Some(corner) = &corner {
        validate_corner(corner)?;
    }
    let mut slot = RECORDING.lock().map_err(|_| internal("the recording state is unavailable"))?;
    let recording = current(&mut slot, a)?;
    require_live(recording)?;
    recording_json(recording)?;
    let c = &recording.control;
    let corner_update = match corner {
        Some(corner) => Some((
            c.webcam_corner.lock().map_err(|_| internal("the recording webcam state is unavailable"))?,
            corner,
        )),
        None => None,
    };
    for (flag, value) in [&c.cursor, &c.highlight, &c.clicks, &c.webcam, &c.webcam_circle, &c.system, &c.microphone].into_iter().zip(switches) {
        if let Some(value) = value {
            flag.store(value, Ordering::Release);
        }
    }
    if let Some((mut current_corner, corner)) = corner_update {
        *current_corner = corner;
    }
    recording_json(recording).map_err(|error| internal(format!(
        "recording options may have changed, but their status could not be reported: {error}"
    )))
}

fn annotate(a: &Args) -> Reply {
    annotate_in(a, &RECORDING)
}

fn annotate_in(a: &Args, slot: &Mutex<Option<Recording>>) -> Reply {
    a.only(&["id", "kind", "style", "duration_ms"])?;
    let kind = document::kind(a.0.get("kind").ok_or_else(|| invalid("kind is required; use the same schema as doc.add"))?)?;
    let style = document::style(a.0.get("style"), jiffshot_doc::object::Style::default())?;
    let object = jiffshot_doc::object::Object::new(0, kind, style);
    let duration = Duration::from_millis(a.int("duration_ms", 0, 3_600_000)?.unwrap_or(0) as u64);
    let mut slot = slot.lock().map_err(|_| internal("the recording state is unavailable"))?;
    let recording = current(&mut slot, a)?;
    require_live(recording)?;
    let substitutions = jiffshot_doc::text::object_font_substitutions(std::iter::once(&object));
    let mark = recorder::add_annotation(&recording.control, object.kind, object.style, duration).map_err(invalid)?;
    Ok(json!({ "id": recording.id, "annotation_id": mark, "coordinates": "recording pixels",
        "duration_ms": duration.as_millis(), "font_substitutions": substitutions }))
}

fn clear(a: &Args) -> Reply {
    a.only(&["id"])?;
    let mut slot = RECORDING.lock().map_err(|_| internal("the recording state is unavailable"))?;
    let recording = current(&mut slot, a)?;
    require_live(recording)?;
    recorder::clear_annotations(&recording.control).map_err(internal)?;
    Ok(json!({ "id": recording.id, "cleared": true }))
}

struct Finalizing;

impl Drop for Finalizing {
    fn drop(&mut self) {
        FINALIZING.store(false, Ordering::Release);
    }
}

fn finish(recording: Recording, cancel: bool) -> Reply {
    let _finalizing = Finalizing;
    let Recording { id, key, path, staging, target, fps, started_at, control, worker } = recording;
    control.paused.store(false, Ordering::Release);
    control.stop.store(true, Ordering::Release);
    let outcome = worker.join().unwrap_or_else(|_| Err("the recorder crashed".into()));
    let (frames, duration) = (control.frames.load(Ordering::Relaxed), control.millis.load(Ordering::Relaxed));
    if cancel {
        if let Err(cleanup) = staging.discard() {
            return Err(match &outcome {
                Err(error) => internal(format!("{cleanup}; recorder error: {error}")),
                Ok(_) => internal(cleanup),
            });
        }
        let mut result = json!({ "id": id, "cancelled": true, "frames": frames, "duration_ms": duration });
        if let Err(error) = outcome {
            result["recording_error"] = json!(error);
        }
        return Ok(result);
    }
    let spec = match outcome {
        Ok(spec) => spec,
        Err(error) => {
            staging.discard().map_err(|cleanup| internal(format!("the recording failed: {error}; {cleanup}")))?;
            return Err(internal(format!("the recording failed: {error}")));
        }
    };
    let path = staging.publish_to(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            conflict(error.to_string())
        } else {
            internal(error)
        }
    })?;
    let size = std::fs::metadata(&path)
        .map_err(|error| internal(format!("recording saved to {}, but its size could not be read: {error}", path.display())))?.len();
    let audio_note = control.audio.lock().map_err(|_| internal(format!(
        "recording saved to {}, but its audio status is unavailable", path.display()
    )))?.clone();
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
        "size_bytes": size,
        "started_at": started_at,
        "audio_note": audio_note,
        "system_silent": control.system_silent.load(Ordering::Relaxed),
        "microphone_silent": control.microphone_silent.load(Ordering::Relaxed),
    });
    *LAST_RECORDING.lock().map_err(|_| internal(format!(
        "recording saved to {}, but its final status could not be stored", path.display()
    )))? = Some(result.clone());
    Ok(result)
}

fn stop(a: &Args, cancel: bool) -> Reply {
    a.only(&["id"])?;
    let _lifecycle = LIFECYCLE.lock().map_err(|_| internal("the recording lifecycle state is unavailable"))?;
    let recording = {
        let mut slot = RECORDING.lock().map_err(|_| internal("the recording state is unavailable"))?;
        current(&mut slot, a)?;
        FINALIZING.store(true, Ordering::Release);
        slot.take().expect("the current recording was checked")
    };
    finish(recording, cancel)
}

/// Called after the control server has drained requests, before the engine exits.
pub(super) fn shutdown() -> Result<(), String> {
    let _lifecycle = LIFECYCLE.lock().map_err(|_| "the recording lifecycle state is poisoned")?;
    let recording = {
        let mut slot = RECORDING.lock().map_err(|_| "the recording state is poisoned")?;
        let recording = slot.take();
        if recording.is_some() {
            FINALIZING.store(true, Ordering::Release);
        }
        recording
    };
    if let Some(recording) = recording {
        let result = finish(recording, false).map_err(|error| format!("could not finalize the CLI recording during shutdown: {error}"))?;
        crate::log(&format!("control: finalized recording during shutdown: {}", result["path"]));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn staging_fixture() -> (StagedRecording, PathBuf) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "jiffshot-record-status-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&directory).expect("create the recording fixture directory");
        let path = directory.join("fixture.mp4");
        let staging = StagedRecording::for_destination(&path).expect("create the recording fixture stage");
        std::fs::write(staging.path(), b"synthetic recording").expect("write the recording fixture");
        (staging, directory)
    }

    fn fixture() -> (Recording, PathBuf) {
        let (staging, directory) = staging_fixture();
        let path = directory.join("fixture.mp4");
        let control = Arc::new(Control::new(&recorder::Options::audio_only(10, "none")));
        let worker_control = control.clone();
        let worker = std::thread::spawn(move || {
            let started = Instant::now();
            while !worker_control.stop.load(Ordering::Acquire) {
                if started.elapsed() > Duration::from_secs(5) {
                    return Err("the synthetic recorder was not stopped".into());
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(VideoSpec::for_size(64, 48, 10))
        });
        (
            Recording {
                id: "rec-fixture".into(),
                key: None,
                path,
                staging,
                target: json!({}),
                fps: 10,
                started_at: String::new(),
                control,
                worker,
            },
            directory,
        )
    }

    fn poison(control: &Arc<Control>, field: &'static str) {
        let control = control.clone();
        let result = std::thread::spawn(move || match field {
            "error" => {
                let _guard = control.error.lock().unwrap();
                panic!("poison recording error state");
            }
            "audio" => {
                let _guard = control.audio.lock().unwrap();
                panic!("poison recording audio state");
            }
            "webcam" => {
                let _guard = control.webcam_corner.lock().unwrap();
                panic!("poison recording webcam state");
            }
            _ => panic!("unknown fixture field"),
        }).join();
        assert!(result.is_err());
    }

    fn discard_fixture(recording: Recording, directory: &Path) {
        let Recording { control, staging, worker, .. } = recording;
        control.stop.store(true, Ordering::Release);
        worker.join().expect("join the synthetic recorder").expect("stop the synthetic recorder");
        staging.discard().expect("discard the recording fixture");
        std::fs::remove_dir(directory).expect("remove the recording fixture directory");
    }

    fn obstruct_stage(staged: &Path, obstructed: bool) -> Option<PathBuf> {
        obstructed.then(|| {
            let obstruction = staged.parent().unwrap().join("fixture-obstruction");
            std::fs::write(&obstruction, b"preserve").expect("create the fixture obstruction");
            obstruction
        })
    }

    fn remove_stage_fixture(directory: &Path, staged: &Path, obstruction: Option<PathBuf>) {
        assert!(!staged.exists());
        if let Some(obstruction) = obstruction {
            assert_eq!(std::fs::read(&obstruction).unwrap(), b"preserve");
            std::fs::remove_file(obstruction).expect("remove the fixture obstruction");
            std::fs::remove_dir(staged.parent().unwrap()).expect("remove the recording fixture stage");
        } else {
            assert!(!staged.parent().unwrap().exists());
        }
        std::fs::remove_dir(directory).expect("remove the recording fixture directory");
    }

    #[test]
    fn stopped_before_first_frame_reports_worker_and_cleanup_failures() {
        for worker_failed in [false, true] {
            for cleanup_failed in [false, true] {
                let (staging, directory) = staging_fixture();
                let staged = staging.path().to_path_buf();
                let obstruction = obstruct_stage(&staged, cleanup_failed);
                let outcome = if worker_failed {
                    Err("synthetic worker failure".into())
                } else {
                    Ok(VideoSpec::for_size(64, 48, 10))
                };
                let error = failed_start(staging, outcome);
                assert_eq!(error.code, ErrorCode::Internal);
                let expected = if worker_failed {
                    "synthetic worker failure"
                } else {
                    "the recorder stopped before writing a frame"
                };
                assert!(error.message.contains(expected), "{}", error.message);
                assert_eq!(error.message.contains("could not discard temporary recording"), cleanup_failed);
                if cleanup_failed {
                    assert!(error.message.contains(&staged.display().to_string()));
                }
                remove_stage_fixture(&directory, &staged, obstruction);
            }
        }
    }

    #[test]
    fn interrupted_start_reports_cause_worker_and_cleanup_failures() {
        for shutdown in [false, true] {
            for worker_failed in [false, true] {
                for cleanup_failed in [false, true] {
                    let (staging, directory) = staging_fixture();
                    let staged = staging.path().to_path_buf();
                    let obstruction = obstruct_stage(&staged, cleanup_failed);
                    let outcome = if worker_failed {
                        Err("synthetic worker failure".into())
                    } else {
                        Ok(VideoSpec::for_size(64, 48, 10))
                    };
                    let error = interrupted_start(staging, outcome, || {
                        assert!(!staged.exists());
                        shutdown
                    });
                    let code = if cleanup_failed {
                        ErrorCode::Internal
                    } else if shutdown {
                        ErrorCode::Conflict
                    } else {
                        ErrorCode::Timeout
                    };
                    assert_eq!(error.code, code);
                    assert!(error.message.contains(if shutdown { "shutting down" } else { "within 10 s" }));
                    assert_eq!(error.message.contains("synthetic worker failure"), worker_failed);
                    assert_eq!(error.message.contains("could not discard temporary recording"), cleanup_failed);
                    if cleanup_failed {
                        assert!(error.message.contains(&staged.display().to_string()));
                    }
                    remove_stage_fixture(&directory, &staged, obstruction);
                }
            }
        }
    }

    #[test]
    fn unavailable_recording_metadata_does_not_poison_the_recording_slot() {
        for field in ["error", "audio", "webcam"] {
            let (recording, directory) = fixture();
            poison(&recording.control, field);
            let slot = Mutex::new(Some(recording));
            let error = status_from(&slot).unwrap_err();
            assert_eq!(error.code, ErrorCode::Internal);
            assert!(error.message.contains(field), "{}", error.message);
            let recording = slot.into_inner().expect("the recording slot was poisoned").unwrap();
            discard_fixture(recording, &directory);
        }
    }

    #[test]
    fn started_status_preserves_recording_metadata() {
        let (recording, directory) = fixture();
        recording.control.frames.store(42, Ordering::Relaxed);
        let (recording, status) = started_status(recording).expect("report the started recording");
        assert_eq!(status["active"], true);
        assert_eq!(status["id"], "rec-fixture");
        assert_eq!(status["frames"], 42);
        assert_eq!(status["options"]["webcam_corner"], "bottom-right");
        discard_fixture(recording, &directory);
    }

    #[test]
    fn recording_annotations_report_the_actual_font_substitutions() {
        let (recording, directory) = fixture();
        recording.control.set_annotation_size(64, 48).unwrap();
        let slot = Mutex::new(Some(recording));
        let family = "Jiffshot unavailable recording face 71c647";
        let text = annotate_in(&Args::new(json!({
            "id":"rec-fixture",
            "kind":{"type":"Text","rect":{"x0":1,"y0":1,"x1":60,"y1":20},"text":"Text"},
            "style":{"font":family,"italic":true},
            "duration_ms":1000
        })).unwrap(), &slot).unwrap();
        let italic = jiffshot_doc::text::font_substitution(family, false, true).unwrap();
        assert_eq!(text["annotation_id"], 1);
        assert_eq!(text["duration_ms"], 1000);
        assert_eq!(text["font_substitutions"], json!([italic]));
        let step = annotate_in(&Args::new(json!({
            "id":"rec-fixture",
            "kind":{"type":"Step","center":{"x":20,"y":20},"radius":10,"label":"1"},
            "style":{"font":family,"bold":false,"italic":true}
        })).unwrap(), &slot).unwrap();
        let bold = jiffshot_doc::text::font_substitution(family, true, false).unwrap();
        assert_eq!(step["annotation_id"], 2);
        assert_eq!(step["font_substitutions"], json!([bold]));
        let shape = annotate_in(&Args::new(json!({
            "id":"rec-fixture",
            "kind":{"type":"Shape","rect":{"x0":1,"y0":1,"x1":20,"y1":20},"shape":"Rectangle"},
            "style":{"font":family}
        })).unwrap(), &slot).unwrap();
        assert_eq!(shape["font_substitutions"], json!([]));
        let recording = slot.into_inner().unwrap().unwrap();
        discard_fixture(recording, &directory);
    }

    #[test]
    fn failed_started_status_cancels_the_worker_and_discards_the_stage() {
        let (recording, directory) = fixture();
        let control = recording.control.clone();
        let staged = recording.staging.path().to_path_buf();
        poison(&control, "audio");
        let error = started_status(recording).err().expect("the status should fail");
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(error.message.contains("audio state"));
        assert!(error.message.contains("cancelled"));
        assert!(control.stop.load(Ordering::Acquire));
        assert!(!staged.exists());
        assert!(!staged.parent().unwrap().exists());
        std::fs::remove_dir(directory).expect("remove the recording fixture directory");
    }

    #[test]
    fn failed_started_status_reports_a_worker_failure() {
        let (mut recording, directory) = fixture();
        recording.control.stop.store(true, Ordering::Release);
        let worker = std::mem::replace(&mut recording.worker, std::thread::spawn(|| Err("synthetic worker failure".into())));
        worker.join().expect("join the synthetic recorder").expect("stop the synthetic recorder");
        poison(&recording.control, "webcam");
        let error = started_status(recording).err().expect("the status should fail");
        assert!(error.message.contains("cancelled"));
        assert!(error.message.contains("synthetic worker failure"));
        std::fs::remove_dir(directory).expect("remove the recording fixture directory");
    }

    #[test]
    fn failed_started_status_reports_a_cleanup_failure() {
        let (mut recording, directory) = fixture();
        recording.control.stop.store(true, Ordering::Release);
        let worker = std::mem::replace(&mut recording.worker, std::thread::spawn(|| Err("synthetic worker failure".into())));
        worker.join().expect("join the synthetic recorder").expect("stop the synthetic recorder");
        let staged = recording.staging.path().to_path_buf();
        let stage_directory = staged.parent().unwrap().to_path_buf();
        let obstruction = stage_directory.join("fixture-obstruction");
        std::fs::write(&obstruction, b"preserve").expect("create the fixture obstruction");
        poison(&recording.control, "error");
        let error = started_status(recording).err().expect("the status should fail");
        assert!(error.message.contains("cancellation failed"));
        assert!(error.message.contains("synthetic worker failure"));
        assert!(error.message.contains(&staged.display().to_string()));
        assert_eq!(std::fs::read(&obstruction).unwrap(), b"preserve");
        std::fs::remove_file(obstruction).expect("remove the fixture obstruction");
        std::fs::remove_dir(stage_directory).expect("remove the recording fixture stage");
        std::fs::remove_dir(directory).expect("remove the recording fixture directory");
    }

    #[test]
    fn finished_recording_reports_its_path_when_audio_status_is_unavailable() {
        let (recording, directory) = fixture();
        let path = recording.path.clone();
        poison(&recording.control, "audio");
        let error = finish(recording, false).unwrap_err();
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(error.message.contains(&path.display().to_string()));
        assert!(error.message.contains("audio status"));
        assert_eq!(std::fs::read(&path).unwrap(), b"synthetic recording");
        std::fs::remove_file(path).expect("remove the published fixture");
        std::fs::remove_dir(directory).expect("remove the recording fixture directory");
    }
}
