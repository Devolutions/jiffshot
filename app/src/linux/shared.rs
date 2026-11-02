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

/// Binds the portal global shortcuts chosen in the settings; keeps running until the process exits.
pub fn start_shortcuts() {
    use super::shortcut::{listen, ShortcutAction, ShortcutOptions};
    if !super::settings::global_shortcuts_enabled().unwrap_or(false) {
        return;
    }
    let (Ok(trigger), Ok(controls), Ok(start)) = (
        super::settings::global_capture_shortcut(),
        super::settings::global_recording_shortcuts(),
        super::settings::global_recording_start(),
    ) else {
        return;
    };
    let options = ShortcutOptions { recording_controls: controls, recording_start: start };
    std::thread::spawn(move || {
        let (_stop, receiver) = async_channel::bounded(1);
        let result = futures_lite::future::block_on(listen(
            trigger,
            options,
            receiver,
            |_| Ok(()),
            move |action| {
                match action {
                    ShortcutAction::Capture => app::post(|_| super::capture_window::show()),
                    ShortcutAction::ToggleRecording => match start.audio() {
                        Some(audio) => toggle_recording_with(audio_name(audio), true),
                        None => report("The recording start shortcut is disabled.".into()),
                    },
                    ShortcutAction::PauseRecording => {
                        let _ = control::record_pause();
                    }
                    ShortcutAction::StopRecording => {
                        if control::recording_active() {
                            toggle_recording_with("none", true);
                        }
                    }
                }
                Ok(())
            },
        ));
        if let Err(error) = result {
            eprintln!("jiffshot: global shortcuts unavailable: {error}");
        }
    });
}

fn audio_name(audio: super::capture::RecordingAudio) -> &'static str {
    use super::capture::RecordingAudio::*;
    match audio {
        Disabled => "none",
        System => "system",
        Microphone => "microphone",
        SystemAndMicrophone => "both",
    }
}

/// Runs the capture or recording requested on the command line.
pub fn run_launch_action(flag: &str) {
    match flag {
        "--capture" => capture_image(),
        "--record" => toggle_recording(),
        "--capture-interactive" => capture_desktop(false),
        "--grab-text" => capture_text(),
        "--scrolling" => capture_scrolling(super::scroll::Mode::Scrolling(
            super::scroll::Direction::Vertical,
        )),
        "--scrolling-horizontal" => capture_scrolling(super::scroll::Mode::Scrolling(
            super::scroll::Direction::Horizontal,
        )),
        "--panoramic" => capture_scrolling(super::scroll::Mode::Panoramic),
        "--interval" => capture_interval(5, true),
        _ => {}
    }
}

enum Active {
    Scroll(async_channel::Sender<super::scroll::Action>),
    Interval(async_channel::Sender<()>),
}

static ACTIVE: std::sync::Mutex<Option<Active>> = std::sync::Mutex::new(None);

fn active() -> std::sync::MutexGuard<'static, Option<Active>> {
    ACTIVE.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Whether a scrolling, panoramic or interval capture is running.
pub(crate) fn busy() -> bool {
    active().is_some()
}

/// Finishes (or with `cancel`, discards) the running scrolling capture, or
/// stops the interval capture.
pub(crate) fn finish_active(cancel: bool) {
    match active().as_ref() {
        Some(Active::Scroll(stop)) => {
            let action = if cancel {
                super::scroll::Action::Cancel
            } else {
                super::scroll::Action::Finish
            };
            let _ = stop.try_send(action);
        }
        Some(Active::Interval(stop)) => {
            let _ = stop.try_send(());
        }
        None => {}
    }
}

/// The desktop's own screenshot dialog, which offers region and window choices.
pub(crate) fn capture_desktop(then_text: bool) {
    report("Choose what to capture in the desktop dialog…".into());
    std::thread::spawn(move || {
        let result = futures_lite::future::block_on(super::screenshot::capture_interactive())
            .and_then(|document| match document {
                Some(document) => super::capture::save_capture(&document).map(Some),
                None => Ok(None),
            });
        finish_single(result, then_text);
    });
}

fn finish_single(result: Result<Option<PathBuf>, String>, then_text: bool) {
    match result {
        Ok(Some(path)) => {
            app::post(move |app| {
                app.incoming(path);
                if then_text {
                    app.grab_text();
                }
            });
        }
        Ok(None) => report("Capture cancelled".into()),
        Err(error) => report(format!("Capture failed: {error}")),
    }
}

/// Captures a monitor or window and copies the text found in it.
pub(crate) fn capture_text() {
    report("Choose a screen or window to read text from…".into());
    std::thread::spawn(|| {
        let cursor = super::capture::CaptureCursor::Hidden;
        let result = futures_lite::future::block_on(super::capture::capture_monitor(cursor))
            .and_then(|document| super::capture::save_capture(&document).map(Some));
        finish_single(result, true);
    });
}

/// Starts a scrolling or panoramic capture; call [`finish_active`] to end it.
pub(super) fn capture_scrolling(mode: super::scroll::Mode) {
    use super::scroll::{Direction, Mode, Progress};
    let (stop, receiver) = async_channel::bounded(2);
    {
        let mut slot = active();
        if slot.is_some() {
            drop(slot);
            return finish_active(false);
        }
        *slot = Some(Active::Scroll(stop));
    }
    report("Choose a window or monitor to capture…".into());
    std::thread::spawn(move || {
        let result = futures_lite::future::block_on(super::scroll::capture(
            mode,
            receiver,
            |progress| {
                report(match progress {
                    Progress::Started => match mode {
                        Mode::Scrolling(Direction::Vertical) => {
                            "Scroll the content down slowly, then press Finish.".to_owned()
                        }
                        Mode::Scrolling(Direction::Horizontal) => {
                            "Scroll the content right slowly, then press Finish.".to_owned()
                        }
                        Mode::Panoramic => "Pan the content slowly, then press Finish.".to_owned(),
                    },
                    Progress::Advanced(extent) => format!("Captured {extent} pixels so far…"),
                    Progress::PanoramicAdvanced { width, height } => {
                        format!("Captured {width} × {height} pixels so far…")
                    }
                    Progress::LostOverlap => {
                        "Lost overlap; move back a little and scroll more slowly.".to_owned()
                    }
                });
            },
        ));
        *active() = None;
        super::capture_window::refresh();
        match result {
            Ok(Some(captured)) => {
                if captured.limit_reached {
                    report("Capture limit reached; saved what was captured".into());
                }
                let saved = super::capture::save_capture(&captured.document);
                finish_single(saved.map(Some), false);
            }
            Ok(None) => report("Capture cancelled".into()),
            Err(error) => report(format!("Capture failed: {error}")),
        }
    });
}

/// Starts an interval capture; a second call stops it.
pub(crate) fn capture_interval(seconds: u32, cursor: bool) {
    use super::interval::Progress;
    let (stop, receiver) = async_channel::bounded(1);
    {
        let mut slot = active();
        if slot.is_some() {
            drop(slot);
            return finish_active(false);
        }
        *slot = Some(Active::Interval(stop));
    }
    let preferences = match super::settings::IntervalPreferences::new(seconds, true) {
        Ok(preferences) => preferences,
        Err(error) => {
            *active() = None;
            return report(error);
        }
    };
    report("Choose a screen or window for interval capture…".into());
    std::thread::spawn(move || {
        let cursor = super::capture::CaptureCursor::from_included(cursor);
        let result = futures_lite::future::block_on(super::interval::capture(
            preferences,
            cursor,
            receiver,
            |progress| match progress {
                Progress::Saved { count, path } => {
                    report(format!("Interval capture saved {count} image(s)"));
                    open_in_editor(path);
                }
                Progress::Skipped { count } => {
                    report(format!("Interval capture skipped {count} unchanged frame(s)"));
                }
            },
        ));
        *active() = None;
        super::capture_window::refresh();
        match result {
            Ok(done) => report(format!(
                "Interval capture stopped: {} saved, {} unchanged skipped",
                done.saved, done.skipped
            )),
            Err(error) => report(format!("Interval capture failed: {error}")),
        }
    });
}
