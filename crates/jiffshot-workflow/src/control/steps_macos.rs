use super::{internal, invalid, not_found, point_json, Args, Reply};
use crate::geometry::CaptureLayout;
use crate::{stepcap, workflow};
use jiffshot_capture::library::{CaptureKind, Library, EDITS_EXT};
use jiffshot_capture::{screen, Rect};
use jiffshot_control::{Error, ErrorCode};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

struct Guide {
    id: String,
    title: String,
    steps: Vec<stepcap::Step>,
}

static GUIDE: Mutex<Option<Guide>> = Mutex::new(None);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn summary(guide: &Option<Guide>) -> Value {
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

fn steps_json(guide: &Guide) -> Vec<Value> {
    guide
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            json!({
                "index": index + 1,
                "description": step.description,
                "at": point_json(step.click),
            })
        })
        .collect()
}

fn start(a: &Args) -> Reply {
    a.only(&["title", "restart"])?;
    let restart = a.bool("restart")?;
    let title = a.string("title")?.unwrap_or_else(|| "How to …".into());
    let mut slot = GUIDE.lock().unwrap();
    if let Some(guide) = slot.as_ref() {
        if !restart {
            return Err(Error::new(
                ErrorCode::Conflict,
                format!(
                    "guide {} is in progress ({} steps); finish, cancel or restart it",
                    guide.id,
                    guide.steps.len()
                ),
            ));
        }
    }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    *slot = Some(Guide {
        id: format!("guide-{}-{id}", std::process::id()),
        title,
        steps: Vec::new(),
    });
    Ok(summary(&slot))
}

fn add(a: &Args) -> Reply {
    add_with(a, capture_step_at)
}

fn add_with(
    a: &Args,
    capture: impl FnOnce(Option<(i32, i32)>, bool, Option<String>) -> Result<stepcap::Step, Error>,
) -> Reply {
    a.only(&["at", "right", "description"])?;
    let point = a.point("at")?;
    let right = a.bool("right")?;
    let description = a
        .string("description")?
        .filter(|text| !text.trim().is_empty());
    let mut slot = GUIDE.lock().unwrap();
    let guide = slot
        .as_mut()
        .ok_or_else(|| not_found("no guide is in progress; run steps.start first"))?;
    let step = capture(point, right, description)?;
    if crate::shutdown_macos::requested() {
        return Err(Error::new(ErrorCode::Conflict, "Jiffshot is shutting down"));
    }
    let description = step.description.clone();
    let point = step.click;
    guide.steps.push(step);
    Ok(json!({ "index": guide.steps.len(), "description": description, "at": point_json(point) }))
}

fn capture_step(
    point: (i32, i32),
    right: bool,
    description: Option<String>,
) -> Result<stepcap::Step, Error> {
    capture_step_at(Some(point), right, description)
}

fn capture_step_at(
    point: Option<(i32, i32)>,
    right: bool,
    description: Option<String>,
) -> Result<stepcap::Step, Error> {
    let layout = CaptureLayout::current().map_err(internal)?;
    let point = match point {
        Some(point) => stepcap::ClickPosition::from_grid(point),
        None => {
            let logical = screen::try_cursor_pos_points().map_err(internal)?;
            stepcap::ClickPosition::from_points(logical, layout.desktop_scale()).map_err(internal)?
        }
    };
    let (x, y) = point.reference();
    let monitor = layout.monitor_at_precise(x, y).map_err(internal)?;
    let area = monitor.rect;
    let layout = layout.with_monitor(&monitor).map_err(internal)?;
    let mapping = layout.mapping(area).map_err(internal)?;
    let frame = layout.grab(area).map_err(internal)?;
    let description = match description {
        Some(description) => description,
        None => stepcap::describe_at(&layout, point, right).map_err(internal)?,
    };
    layout.validate().map_err(internal)?;
    Ok(stepcap::Step {
        frame,
        area,
        mapping: Some(mapping),
        click: point.grid,
        click_precise: point.precise,
        right,
        description,
    })
}

pub(super) fn click_with_step(
    requested: Option<bool>,
    point: (i32, i32),
    right: bool,
    description: Option<String>,
    perform: impl FnOnce() -> Result<(), Error>,
) -> Result<Option<Value>, Error> {
    click_with_step_and_capture(requested, point, right, description, capture_step, perform)
}

pub(super) fn require_for_click(requested: Option<bool>) -> Result<(), Error> {
    if requested == Some(true) && GUIDE.lock().map_err(|_| internal("the guide state is unavailable"))?.is_none() {
        return Err(not_found("step needs a guide in progress; run steps.start first"));
    }
    Ok(())
}

fn click_with_step_and_capture(
    requested: Option<bool>,
    point: (i32, i32),
    right: bool,
    description: Option<String>,
    capture: impl FnOnce((i32, i32), bool, Option<String>) -> Result<stepcap::Step, Error>,
    perform: impl FnOnce() -> Result<(), Error>,
) -> Result<Option<Value>, Error> {
    if requested == Some(false) {
        perform()?;
        return Ok(None);
    }
    let mut slot = GUIDE.lock().map_err(|_| internal("the guide state is unavailable"))?;
    let Some(guide) = slot.as_mut() else {
        if requested == Some(true) {
            return Err(not_found("step needs a guide in progress; run steps.start first"));
        }
        drop(slot);
        perform()?;
        return Ok(None);
    };
    let description = description.filter(|text| !text.trim().is_empty());
    let step = capture(point, right, description)?;
    if crate::shutdown_macos::requested() {
        return Err(Error::new(ErrorCode::Conflict, "Jiffshot is shutting down"));
    }
    perform()?;
    let description = step.description.clone();
    guide.steps.push(step);
    Ok(Some(json!({ "index": guide.steps.len(), "description": description, "at": point_json(point) })))
}

fn finish(a: &Args) -> Reply {
    a.only(&["out", "title", "open"])?;
    let out = a.out("out", &["png"])?;
    let title = a.string("title")?;
    let open = a.bool("open")?;
    let mut slot = GUIDE.lock().unwrap();
    let guide = slot
        .as_ref()
        .ok_or_else(|| not_found("no guide is in progress"))?;
    if guide.steps.is_empty() {
        return Err(Error::new(
            ErrorCode::Conflict,
            "the guide has no steps yet",
        ));
    }
    let title = title.unwrap_or_else(|| guide.title.clone());
    let doc = stepcap::guide_titled(&guide.steps, &title).map_err(internal)?;
    let frame = workflow::frame_from_pixmap(&doc.flatten());
    let rect = Rect::new(0, 0, frame.width as i32, frame.height as i32);
    let capture = workflow::from_frame(CaptureKind::Steps, rect, frame);
    let document_bytes = jiffshot_doc::io::document_bytes(&doc).map_err(internal)?;
    if crate::shutdown_macos::requested() {
        return Err(Error::new(ErrorCode::Conflict, "Jiffshot is shutting down"));
    }
    let path = match out {
        Some(path) => {
            let document = path.with_extension(EDITS_EXT);
            if document.is_dir() {
                return Err(internal(format!(
                    "the editable guide path is a directory: {}",
                    document.display()
                )));
            }
            let bytes = capture.frame.encode_png_vec().map_err(internal)?;
            jiffshot_doc::io::write_atomic(&path, &bytes).map_err(internal)?;
            path
        }
        None => Library::new(jiffshot_capture::config::library_root())
            .save(&capture.frame, &capture.meta)
            .map_err(internal)?,
    };
    let document = path.with_extension(EDITS_EXT);
    jiffshot_doc::io::write_atomic(&document, &document_bytes)
        .map_err(|error| internal(format!("the guide image was saved to {}, but its editable document could not be saved: {error}", path.display())))?;
    let opened = open && crate::hooks().is_some_and(|hooks| (hooks.open_in_editor)(&path));
    let result = json!({
        "id": guide.id,
        "title": title,
        "path": path,
        "document": document,
        "width": capture.frame.width,
        "height": capture.frame.height,
        "opened": opened,
        "steps": steps_json(guide),
        "font_substitutions": doc.font_substitutions(),
    });
    *slot = None;
    Ok(result)
}

fn cancel(a: &Args) -> Reply {
    a.only(&[])?;
    let guide = GUIDE
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| not_found("no guide is in progress"))?;
    Ok(json!({ "id": guide.id, "cancelled": true, "steps": guide.steps.len() }))
}

pub(super) fn status() -> Reply {
    let guide = GUIDE.lock().map_err(|_| internal("the guide state is unavailable"))?;
    Ok(summary(&guide))
}

pub(super) fn dispatch(command: &str, a: &Args) -> Reply {
    match command {
        "steps.status" => {
            a.only(&[])?;
            status()
        }
        "steps.start" => start(a),
        "steps.add" => add(a),
        "steps.finish" => finish(a),
        "steps.cancel" => cancel(a),
        _ => Err(invalid(format!("unknown step-guide command {command:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiffshot_capture::Frame;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn call(command: &str, args: Value) -> Reply {
        dispatch(command, &Args::new(args).unwrap())
    }

    #[test]
    fn start_conflicts_restart_and_cancel_preserve_guide_identity() {
        let _test = TEST_LOCK.lock().unwrap();
        *GUIDE.lock().unwrap() = None;
        assert_eq!(
            call("steps.status", json!({})).unwrap(),
            json!({"active":false})
        );
        let first = call("steps.start", json!({"title":"First"})).unwrap();
        assert_eq!(first["title"], "First");
        assert_eq!(
            call("steps.start", json!({})).unwrap_err().code,
            ErrorCode::Conflict
        );
        assert_eq!(
            call("steps.finish", json!({})).unwrap_err().code,
            ErrorCode::Conflict
        );
        assert_eq!(call("steps.status", json!({})).unwrap()["id"], first["id"]);
        let second = call("steps.start", json!({"title":"Second", "restart":true})).unwrap();
        assert_ne!(first["id"], second["id"]);
        let cancelled = call("steps.cancel", json!({})).unwrap();
        assert_eq!(cancelled["id"], second["id"]);
        assert_eq!(cancelled["steps"], 0);
        assert_eq!(
            call("steps.status", json!({})).unwrap(),
            json!({"active":false})
        );
    }

    #[test]
    fn failed_finish_keeps_guide_for_retry() {
        let _test = TEST_LOCK.lock().unwrap();
        *GUIDE.lock().unwrap() = None;
        let started = call("steps.start", json!({"title":"Test"})).unwrap();
        let added = add_with(
            &Args::new(json!({"at":[3,4], "description":"Test step"})).unwrap(),
            |point, right, description| {
                Ok(stepcap::Step {
                    frame: Frame::new(16, 12),
                    area: Rect::new(0, 0, 16, 12),
                    mapping: None,
                    click: point.expect("the test specified a point"),
                    click_precise: None,
                    right,
                    description: description.unwrap(),
                })
            },
        )
        .unwrap();
        assert_eq!(
            added,
            json!({"index":1, "at":{"x":3, "y":4}, "description":"Test step"})
        );
        let root = std::env::temp_dir().join(format!(
            "jiffshot-step-finish-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("guide.jsd")).unwrap();
        let out = root.join("guide.png");
        std::fs::write(&out, b"previous guide").unwrap();
        assert!(call("steps.finish", json!({"out":out})).is_err());
        assert_eq!(std::fs::read(&out).unwrap(), b"previous guide");
        assert_eq!(
            call("steps.status", json!({})).unwrap()["id"],
            started["id"]
        );
        std::fs::remove_dir(root.join("guide.jsd")).unwrap();
        let result = call("steps.finish", json!({"out":out, "title":"Retried"})).unwrap();
        assert_eq!(result["title"], "Retried");
        assert_eq!(result["steps"], json!([{"index":1, "at":{"x":3, "y":4}, "description":"Test step"}]));
        assert!(out.is_file() && root.join("guide.jsd").is_file());
        let document = jiffshot_doc::io::load_document(&root.join("guide.jsd")).unwrap();
        assert!(document.objects.iter().any(|object| matches!(&object.kind, jiffshot_doc::object::Kind::Text { text, .. } if text == "Test step")));
        assert_eq!(result["font_substitutions"], json!(document.font_substitutions()));
        assert_eq!(
            call("steps.status", json!({})).unwrap(),
            json!({"active":false})
        );
        std::fs::remove_file(out).unwrap();
        std::fs::remove_file(root.join("guide.jsd")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn adding_a_step_without_coordinates_returns_the_captured_pointer_position() {
        let _test = TEST_LOCK.lock().unwrap();
        *GUIDE.lock().unwrap() = None;
        call("steps.start", json!({})).unwrap();
        let added = add_with(&Args::new(json!({"description": "Pointer step"})).unwrap(), |point, right, description| {
            assert_eq!(point, None);
            Ok(stepcap::Step {
                frame: Frame::new(4, 4),
                area: Rect::new(0, 0, 4, 4),
                mapping: None,
                click: (2, 3),
                click_precise: Some((2.25, 3.25)),
                right,
                description: description.unwrap(),
            })
        }).unwrap();
        assert_eq!(added, json!({"index":1, "at":{"x":2, "y":3}, "description":"Pointer step"}));
        assert_eq!(call("steps.cancel", json!({})).unwrap()["steps"], 1);
    }

    #[test]
    fn click_steps_are_added_only_after_successful_input() {
        let _test = TEST_LOCK.lock().unwrap();
        *GUIDE.lock().unwrap() = None;
        let capture = |point, right, description: Option<String>| {
            Ok(stepcap::Step {
                frame: Frame::new(4, 4),
                area: Rect::new(0, 0, 4, 4),
                mapping: None,
                click: point,
                click_precise: None,
                right,
                description: description.unwrap_or_default(),
            })
        };
        assert_eq!(
            click_with_step_and_capture(Some(true), (2, 3), false, None, capture, || Ok(()))
                .unwrap_err().code,
            ErrorCode::NotFound
        );
        assert!(click_with_step_and_capture(None, (2, 3), false, None, capture, || Ok(())).unwrap().is_none());
        call("steps.start", json!({})).unwrap();
        assert!(click_with_step_and_capture(None, (2, 3), false, None, capture, || Err(internal("input failed"))).is_err());
        assert_eq!(call("steps.status", json!({})).unwrap()["steps"], json!([]));
        let step = click_with_step_and_capture(None, (2, 3), true, Some("Button".into()), capture, || Ok(())).unwrap();
        assert_eq!(step, Some(json!({"index":1, "at":{"x":2, "y":3}, "description":"Button"})));
        assert!(click_with_step_and_capture(Some(false), (2, 3), false, None, capture, || Ok(())).unwrap().is_none());
        assert_eq!(call("steps.cancel", json!({})).unwrap()["steps"], 1);
    }
}
