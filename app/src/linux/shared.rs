//! Hooks that let the shared editor window use the Linux portal capture and
//! recording engines.

use super::control;
use crate::app;
use crate::winutil::{self, CaptureRequest};
use std::path::PathBuf;

fn open_in_editor(path: PathBuf) {
    app::post(move |app| app.incoming(path));
}

fn report(message: String) {
    app::post(move |app| app.set_status(message));
}

/// Takes a portal screenshot, saves it to the library and opens it.
pub(crate) fn capture_image() {
    capture_image_with(true, 0);
}

/// Like [`capture_image`], waiting `delay` seconds after the desktop dialog.
pub(crate) fn capture_image_with(cursor: bool, delay: u64) {
    report("Choose a screen or window in the desktop dialog…".into());
    std::thread::spawn(move || {
        let cursor = super::capture::CaptureCursor::from_included(cursor);
        let captured = if delay == 0 {
            futures_lite::future::block_on(super::capture::capture_monitor(cursor)).map(Some)
        } else {
            let (_keep, cancel) = std::sync::mpsc::channel();
            let note = move || report(format!("Capturing in {delay} s…"));
            futures_lite::future::block_on(super::capture::capture_monitor_after_delay(
                cursor,
                std::time::Duration::from_secs(delay),
                &cancel,
                note,
                || {},
            ))
        };
        match captured.and_then(|document| match document {
            Some(document) => super::capture::save_capture(&document).map(Some),
            None => Ok(None),
        }) {
            Ok(Some(path)) => open_in_editor(path),
            Ok(None) => report("Capture cancelled".into()),
            Err(error) => report(format!("Capture failed: {error}")),
        }
    });
}

/// Starts a recording, or stops the running one and opens the video.
pub(crate) fn toggle_recording() {
    toggle_recording_with("none", true);
}

pub(crate) fn toggle_recording_with(audio: &'static str, cursor: bool) {
    std::thread::spawn(move || {
        if control::recording_active() {
            match control::stop_recording() {
                Ok(Some(path)) => open_in_editor(path),
                Ok(None) => report("Recording cancelled".into()),
                Err(error) => report(format!("Could not stop the recording: {error}")),
            }
        } else {
            report("Choose a screen in the desktop dialog…".into());
            match control::start_recording_with(audio, cursor) {
                Ok(()) => report("Recording… press Record again to stop".into()),
                Err(error) => report(format!("Could not start recording: {error}")),
            }
        }
    });
}

pub fn install_capture_handler() {
    winutil::set_capture_handler(|request| match request {
        CaptureRequest::Image => capture_image(),
        CaptureRequest::Video => toggle_recording(),
        CaptureRequest::CaptureWindow => app::post(|_| super::capture_window::show()),
    });
    control::set_open_handler(open_in_editor);
}

/// Starts the status-notifier tray when it is enabled. Capture opens the
/// Capture window and Quit exits; the returned session must stay alive.
pub fn start_tray() -> Option<super::tray::Session> {
    if !super::settings::tray_enabled().unwrap_or(false) {
        return None;
    }
    use super::tray::Event;
    let session = super::tray::Session::start(|event| match event {
        Event::Ready => {}
        Event::Open => {}
        Event::Capture => app::post(|_| super::capture_window::show()),
        Event::Quit => std::process::exit(0),
        Event::Unavailable(error) => eprintln!("jiffshot: tray unavailable: {error}"),
    })
    .map_err(|error| eprintln!("jiffshot: {error}"))
    .ok()?;
    session.activate();
    Some(session)
}

/// Runs the capture or recording requested on the command line.
pub fn run_launch_action(flag: &str) {
    match flag {
        "--capture" => capture_image(),
        "--record" => toggle_recording(),
        _ => {}
    }
}
