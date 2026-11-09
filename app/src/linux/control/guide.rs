//! Manual step guides using portal-selected screenshots.

use super::{internal, invalid, lock, record_guide_caption, Args, Reply, OPEN_HANDLER};
use crate::linux::{capture, shared};
use jiffshot_capture::{Frame, Rect};
use jiffshot_control::{Error, ErrorCode};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

struct Step {
    frame: Frame,
    at: (i32, i32),
    right: bool,
    description: String,
}

struct Guide {
    id: String,
    title: String,
    steps: Vec<Step>,
}

static GUIDE: Mutex<Option<Guide>> = Mutex::new(None);
static ADDING: Mutex<()> = Mutex::new(());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn not_found() -> Error {
    Error::new(ErrorCode::NotFound, "no guide is in progress")
}

fn steps_json(guide: &Guide) -> Vec<Value> {
    guide
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            json!({
                "index": index + 1,
                "description": step.description,
                "at": { "x": step.at.0, "y": step.at.1 },
            })
        })
        .collect()
}

fn summary(guide: Option<&Guide>) -> Value {
    match guide {
        None => json!({ "active": false }),
        Some(guide) => json!({
            "active": true,
            "id": guide.id,
            "title": guide.title,
            "steps": steps_json(guide),
        }),
    }
}

fn title(args: &Args, default: &str) -> Result<String, Error> {
    let title = args.text("title")?.unwrap_or(default);
    if title.chars().count() > 4096 {
        return Err(invalid("guide title must contain at most 4096 characters"));
    }
    Ok(title.to_owned())
}

pub(super) fn start(args: &Args) -> Reply {
    let title = title(args, "How to …")?;
    let restart = args.flag("restart")?;
    let mut slot = lock(&GUIDE);
    if let Some(guide) = slot.as_ref().filter(|_| !restart) {
        return Err(Error::new(
            ErrorCode::Conflict,
            format!(
                "guide {} is in progress ({} steps); finish, cancel or restart it",
                guide.id,
                guide.steps.len()
            ),
        ));
    }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    *slot = Some(Guide {
        id: format!("guide-{}-{id}", std::process::id()),
        title,
        steps: Vec::new(),
    });
    Ok(summary(slot.as_ref()))
}

fn point(args: &Args) -> Result<(i32, i32), Error> {
    let invalid_point = || invalid("at must be a point X,Y in captured-image pixels");
    let pair = match args.0.get("at") {
        Some(Value::String(text)) => {
            let values: Vec<_> = text
                .split(',')
                .map(|value| value.trim().parse::<f64>().ok())
                .collect();
            match values.as_slice() {
                [x, y] => (*x, *y),
                _ => return Err(invalid_point()),
            }
        }
        Some(Value::Array(values)) if values.len() == 2 => (values[0].as_f64(), values[1].as_f64()),
        Some(Value::Object(values)) => (
            values.get("x").and_then(Value::as_f64),
            values.get("y").and_then(Value::as_f64),
        ),
        _ => return Err(invalid_point()),
    };
    match pair {
        (Some(x), Some(y))
            if x.is_finite()
                && y.is_finite()
                && (0.0..=i32::MAX as f64).contains(&x)
                && (0.0..=i32::MAX as f64).contains(&y) =>
        {
            Ok((x.round() as i32, y.round() as i32))
        }
        _ => Err(invalid_point()),
    }
}

fn accepts(guide: &Guide, frame: &Frame) -> bool {
    let images = guide
        .steps
        .iter()
        .map(|step| (step.frame.width, step.frame.height))
        .chain(std::iter::once((frame.width, frame.height)));
    let pixels = images.clone().try_fold(0_u64, |total, (width, height)| {
        total.checked_add(u64::from(width) * u64::from(height))
    });
    pixels.is_some_and(|pixels| pixels <= jiffshot_doc::io::MAX_DOCUMENT_PIXELS)
        && jiffshot_video::guide::fits(images)
}

pub(super) fn add(args: &Args) -> Reply {
    add_with(
        args,
        || {
            let document = futures_lite::future::block_on(capture::capture_monitor(
                capture::CaptureCursor::Hidden,
            ))
            .map_err(|error| Error::new(ErrorCode::Unavailable, error))?;
            Ok(shared::frame_from_pixmap(&document.flatten()))
        },
        record_guide_caption,
    )
}

fn add_with(
    args: &Args,
    capture_frame: impl FnOnce() -> Result<Frame, Error>,
    record_caption: impl FnOnce(&str) -> Result<(), Error>,
) -> Reply {
    let at = point(args)?;
    let right = args.flag("right")?;
    let description = args
        .text("description")?
        .ok_or_else(|| invalid("a guide step needs --description on Linux"))?;
    if description.trim().is_empty() || description.chars().count() > 4096 {
        return Err(invalid("step description must contain 1..4096 characters"));
    }
    let _adding = lock(&ADDING);
    let guide_id = lock(&GUIDE).as_ref().ok_or_else(not_found)?.id.clone();
    let frame = capture_frame()?;
    frame.validate().map_err(internal)?;
    if !frame.bounds().contains(at.0, at.1) {
        return Err(invalid("at is outside the captured image"));
    }
    let mut slot = lock(&GUIDE);
    let guide = slot
        .as_mut()
        .filter(|guide| guide.id == guide_id)
        .ok_or_else(|| Error::new(ErrorCode::Conflict, "the guide changed during capture"))?;
    if !accepts(guide, &frame) {
        return Err(Error::new(
            ErrorCode::Conflict,
            "the guide is too large to add another screenshot",
        ));
    }
    guide.steps.push(Step {
        frame,
        at,
        right,
        description: description.to_owned(),
    });
    let index = guide.steps.len();
    drop(slot);
    record_caption(description).map_err(|error| {
        Error::new(
            error.code,
            format!(
                "the guide step was added, but its recording caption could not be confirmed: {}",
                error.message
            ),
        )
    })?;
    Ok(json!({
        "index": index,
        "description": description,
        "at": { "x": at.0, "y": at.1 },
    }))
}

pub(super) fn status() -> Reply {
    Ok(summary(lock(&GUIDE).as_ref()))
}

pub(super) fn finish(args: &Args) -> Reply {
    let out = args.text("out")?.map(PathBuf::from);
    if let Some(path) = &out {
        if !path.is_absolute()
            || !path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
        {
            return Err(invalid("out must be an absolute PNG path"));
        }
        if path
            .with_extension(jiffshot_capture::library::EDITS_EXT)
            .is_dir()
        {
            return Err(internal("the editable guide path is a directory"));
        }
    }
    let open = args.flag("open")?;
    let mut slot = lock(&GUIDE);
    let guide = slot.as_ref().ok_or_else(not_found)?;
    if guide.steps.is_empty() {
        return Err(Error::new(
            ErrorCode::Conflict,
            "the guide has no steps yet",
        ));
    }
    let title = title(args, &guide.title)?;
    let steps: Vec<_> = guide
        .steps
        .iter()
        .map(|step| jiffshot_video::guide::Step {
            frame: &step.frame,
            area: Rect::new(0, 0, step.frame.width as i32, step.frame.height as i32),
            mapping: None,
            click: step.at,
            click_precise: None,
            right: step.right,
            description: &step.description,
        })
        .collect();
    let document = jiffshot_video::guide::render(&steps, &title).map_err(internal)?;
    let document_bytes = jiffshot_doc::io::document_bytes(&document).map_err(internal)?;
    let path = match out {
        Some(path) => {
            let png = document.flatten().encode_png().map_err(internal)?;
            jiffshot_doc::io::write_atomic(&path, &png).map_err(internal)?;
            path
        }
        None => capture::save_capture(&document).map_err(internal)?,
    };
    let editable = path.with_extension(jiffshot_capture::library::EDITS_EXT);
    jiffshot_doc::io::write_atomic(&editable, &document_bytes).map_err(|error| {
        internal(format!(
            "the guide image was saved to {}, but its editable document could not be saved: {error}",
            path.display()
        ))
    })?;
    let mut result = json!({
        "id": guide.id,
        "title": title,
        "path": path,
        "document": editable,
        "width": document.width(),
        "height": document.height(),
        "opened": false,
        "steps": steps_json(guide),
    });
    *slot = None;
    drop(slot);
    if open {
        if let Some(handler) = lock(&OPEN_HANDLER).as_ref() {
            handler(path);
            result["opened"] = json!(true);
        }
    }
    Ok(result)
}

pub(super) fn cancel() -> Reply {
    let guide = lock(&GUIDE).take().ok_or_else(not_found)?;
    Ok(json!({ "id": guide.id, "cancelled": true, "steps": guide.steps.len() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiffshot_doc::object::{Kind, ShapeKind};

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn arguments(value: &Value) -> Args<'_> {
        Args::new(
            value,
            &[
                "title",
                "restart",
                "at",
                "right",
                "description",
                "out",
                "open",
            ],
        )
        .unwrap()
    }

    fn start_guide(title: &str) -> Value {
        let value = json!({ "title": title });
        start(&arguments(&value)).unwrap()
    }

    #[test]
    fn manual_guides_validate_lifecycle_and_input_before_capture() {
        let _test = lock(&TEST_LOCK);
        *lock(&GUIDE) = None;
        assert_eq!(status().unwrap(), json!({ "active": false }));
        let first = start_guide("Setup");
        assert_eq!(first["title"], "Setup");
        assert_eq!(
            start(&arguments(&json!({}))).unwrap_err().code,
            ErrorCode::Conflict
        );
        assert_eq!(
            finish(&arguments(&json!({}))).unwrap_err().code,
            ErrorCode::Conflict
        );

        for value in [
            json!({ "description": "Open" }),
            json!({ "at": "3,4" }),
            json!({ "at": "-1,4", "description": "Open" }),
            json!({ "at": [3, 4], "description": " " }),
            json!({ "at": [3, 4], "description": "Open", "right": "perhaps" }),
        ] {
            let error = add_with(
                &arguments(&value),
                || panic!("invalid arguments must not request a screenshot"),
                |_| panic!("invalid arguments must not record a caption"),
            )
            .unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidRequest, "{value}");
        }
        let outside = json!({ "at": [32, 23], "description": "Outside" });
        let error = add_with(
            &arguments(&outside),
            || Ok(Frame::new(32, 24)),
            |_| panic!("an out-of-bounds point must not record a caption"),
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(status().unwrap()["steps"], json!([]));

        let valid = json!({ "at": { "x": 31, "y": 23 }, "description": "Open" });
        let added = add_with(
            &arguments(&valid),
            || Ok(Frame::new(32, 24)),
            |text| {
                assert!(
                    GUIDE.try_lock().is_ok(),
                    "caption recording must not hold the guide lock"
                );
                assert_eq!(text, "Open");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(added["index"], 1);
        assert_eq!(added["at"], json!({ "x": 31, "y": 23 }));

        let restarted = start(&arguments(&json!({ "title": "New", "restart": true }))).unwrap();
        assert_ne!(restarted["id"], first["id"]);
        assert_eq!(restarted["steps"], json!([]));
        assert_eq!(cancel().unwrap()["steps"], 0);
        assert_eq!(status().unwrap(), json!({ "active": false }));
    }

    #[test]
    fn finish_releases_the_guide_before_opening_the_editor() {
        let _test = lock(&TEST_LOCK);
        *lock(&GUIDE) = None;
        start_guide("Setup");
        let step = json!({ "at": [1, 1], "description": "Open" });
        add_with(&arguments(&step), || Ok(Frame::new(4, 4)), |_| Ok(())).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("guide.png");
        let (sender, receiver) = std::sync::mpsc::channel();
        *lock(&OPEN_HANDLER) = Some(Box::new(move |path| {
            sender.send((path, GUIDE.try_lock().is_ok())).unwrap();
        }));
        let result = finish(&arguments(&json!({ "out": output, "open": true }))).unwrap();
        *lock(&OPEN_HANDLER) = None;
        assert_eq!(receiver.recv().unwrap(), (output, true));
        assert_eq!(result["opened"], true);
        assert_eq!(status().unwrap(), json!({ "active": false }));
    }

    #[test]
    fn added_step_queues_its_description_to_the_active_recorder() {
        use super::super::{ActiveRecording, RecordingCommand, RecordingState, RECORDING};
        use std::sync::{mpsc, Arc};

        let _test = lock(&TEST_LOCK);
        *lock(&GUIDE) = None;
        start_guide("Setup");
        let (commands, receiver) = async_channel::bounded(1);
        let (_done, finished) = mpsc::channel();
        *lock(&RECORDING) = Some(ActiveRecording {
            commands,
            state: Arc::new(Mutex::new(RecordingState::Recording)),
            finished,
        });
        let worker = std::thread::spawn(move || {
            let RecordingCommand::GuideCaption { text, result } = receiver.recv_blocking().unwrap()
            else {
                panic!("expected a guide caption command");
            };
            assert_eq!(text, "Open settings");
            result.send(Ok(123)).unwrap();
        });
        let request = json!({ "at": [1, 1], "description": "Open settings" });
        let added = add_with(
            &arguments(&request),
            || Ok(Frame::new(4, 4)),
            record_guide_caption,
        )
        .unwrap();
        assert_eq!(added["index"], 1);
        worker.join().unwrap();
        drop(lock(&RECORDING).take());
        cancel().unwrap();
    }

    #[test]
    fn manual_steps_save_an_editable_guide_and_keep_steps_on_caption_failure() {
        let _test = lock(&TEST_LOCK);
        *lock(&GUIDE) = None;
        start_guide("Setup");
        let first = json!({ "at": "8,9", "description": "Open settings" });
        add_with(
            &arguments(&first),
            || Ok(Frame::new(32, 24)),
            |text| {
                assert_eq!(text, "Open settings");
                Ok(())
            },
        )
        .unwrap();
        let second = json!({ "at": [20, 10], "right": true, "description": "Choose option" });
        let error = add_with(
            &arguments(&second),
            || Ok(Frame::new(32, 24)),
            |_| Err(Error::new(ErrorCode::Internal, "caption queue closed")),
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(error.message.contains("guide step was added"));
        assert!(error.message.contains("caption queue closed"));
        assert_eq!(status().unwrap()["steps"].as_array().unwrap().len(), 2);

        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("guide.png");
        let editable = output.with_extension(jiffshot_capture::library::EDITS_EXT);
        std::fs::create_dir(&editable).unwrap();
        let request = json!({ "out": output, "title": "Finished" });
        assert_eq!(
            finish(&arguments(&request)).unwrap_err().code,
            ErrorCode::Internal
        );
        assert!(!output.exists());
        assert!(status().unwrap()["active"].as_bool().unwrap());
        std::fs::remove_dir(&editable).unwrap();

        let finished = finish(&arguments(&request)).unwrap();
        assert_eq!(finished["title"], "Finished");
        assert_eq!(finished["path"], json!(output));
        assert_eq!(finished["document"], json!(editable));
        assert_eq!(
            (finished["width"].as_u64(), finished["height"].as_u64()),
            (Some(1400), Some(1420))
        );
        assert_eq!(finished["steps"][1]["description"], "Choose option");
        assert!(!finished["opened"].as_bool().unwrap());
        let png = jiffshot_doc::io::open(&output).expect("open guide PNG");
        assert_eq!(png.size(), (1400, 1420));
        let document = jiffshot_doc::io::open(&editable).expect("open editable guide");
        let badges: Vec<_> = document
            .objects
            .iter()
            .filter_map(|object| match &object.kind {
                Kind::Step { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(badges, ["1", "2"]);
        let rings: Vec<_> = document
            .objects
            .iter()
            .filter(|object| {
                matches!(
                    object.kind,
                    Kind::Shape {
                        shape: ShapeKind::Ellipse,
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(rings.len(), 2);
        assert_eq!(rings[0].style.stroke, jiffshot_doc::Rgba::RED);
        assert_eq!(
            rings[1].style.stroke,
            jiffshot_doc::Rgba::rgb(0xF5, 0x9E, 0x0B)
        );
        assert_eq!(status().unwrap(), json!({ "active": false }));
    }

    #[test]
    fn caption_timeout_reports_that_the_step_remains_and_may_still_be_recorded() {
        let _test = lock(&TEST_LOCK);
        *lock(&GUIDE) = None;
        start_guide("Setup");
        let request = json!({ "at": [1, 1], "description": "Open" });
        let error = add_with(
            &arguments(&request),
            || Ok(Frame::new(4, 4)),
            |_| {
                Err(Error::new(
                    ErrorCode::Timeout,
                    "the queued caption may still be recorded",
                ))
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Timeout);
        assert!(error.message.contains("the guide step was added"));
        assert!(error.message.contains("may still be recorded"));
        assert_eq!(status().unwrap()["steps"].as_array().unwrap().len(), 1);
        cancel().unwrap();
    }

    #[test]
    fn a_restarted_guide_cannot_receive_an_old_capture() {
        let _test = lock(&TEST_LOCK);
        *lock(&GUIDE) = None;
        start_guide("Old");
        let request = json!({ "at": [1, 1], "description": "Old step" });
        let error = add_with(
            &arguments(&request),
            || {
                start(&arguments(&json!({ "title": "New", "restart": true }))).unwrap();
                Ok(Frame::new(4, 4))
            },
            |_| panic!("a replaced guide must not record a caption"),
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Conflict);
        assert_eq!(status().unwrap()["title"], "New");
        assert_eq!(status().unwrap()["steps"], json!([]));
        cancel().unwrap();
    }

    #[test]
    fn guide_capacity_checks_the_editable_canvas_before_adding_a_step() {
        let step = || Step {
            frame: Frame::new(1, 1),
            at: (0, 0),
            right: false,
            description: "Step".into(),
        };
        let mut guide = Guide {
            id: "guide".into(),
            title: "Test".into(),
            steps: (0..36).map(|_| step()).collect(),
        };
        assert!(accepts(&guide, &Frame::new(1, 1)));
        guide.steps.push(step());
        assert!(!accepts(&guide, &Frame::new(1, 1)));
    }
}
