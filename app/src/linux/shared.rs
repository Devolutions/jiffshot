//! Hooks that let the shared editor window use the Linux portal capture and
//! recording engines.

use super::control;
use crate::app;
use crate::winutil::{self, CaptureRequest};
use std::path::PathBuf;

#[derive(Clone)]
pub(super) struct Delivery {
    open_in_editor: bool,
    copy_to_clipboard: bool,
    settings_error: Option<String>,
    background: bool,
    request_id: Option<u64>,
}

impl Delivery {
    pub(super) fn for_capture_window(open_in_editor: bool, request_id: Option<u64>) -> Self {
        let (copy_to_clipboard, settings_error) = match super::settings::copy_capture_to_clipboard() {
            Ok(copy) => (copy, None),
            Err(error) => (false, Some(format!("could not read the clipboard preference: {error}"))),
        };
        Self {
            open_in_editor,
            copy_to_clipboard,
            settings_error,
            background: false,
            request_id,
        }
    }

    fn from_settings(background: bool) -> Self {
        let mut errors = Vec::new();
        let open_in_editor = if background {
            false
        } else {
            match super::settings::open_in_editor() {
                Ok(open) => open,
                Err(error) => {
                    errors.push(format!("editor preference: {error}"));
                    true
                }
            }
        };
        let copy_to_clipboard = if background {
            false
        } else {
            match super::settings::copy_capture_to_clipboard() {
                Ok(copy) => copy,
                Err(error) => {
                    errors.push(format!("clipboard preference: {error}"));
                    false
                }
            }
        };
        let settings_error = (!errors.is_empty())
            .then(|| format!("could not read delivery settings ({})", errors.join("; ")));
        Self {
            open_in_editor,
            copy_to_clipboard,
            settings_error,
            background,
            request_id: None,
        }
    }
}

static RECORDING_OPEN_IN_EDITOR: std::sync::Mutex<Option<bool>> = std::sync::Mutex::new(None);

fn deliver_image(path: PathBuf, delivery: Delivery) {
    let copy_error = if delivery.copy_to_clipboard {
        jiffshot_doc::io::load_image(&path)
            .map_err(|error| error.to_string())
            .and_then(super::clipboard::copy_pixmap)
            .err()
            .map(|error| format!("clipboard delivery failed: {error}"))
    } else {
        None
    };
    let problem_note = match (delivery.settings_error, copy_error) {
        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    let open = delivery.open_in_editor;
    let request_id = delivery.request_id;
    app::post(move |app| {
        let reopened = if open {
            app.incoming(path);
            false
        } else {
            app.refresh_library();
            app.set_status(format!("Capture saved to {}", path.display()));
            request_id.is_some_and(|id| {
                let reopen = crate::ui::capture_view::image_finished(
                    id,
                    Some(path),
                    problem_note.is_some(),
                    true,
                );
                if reopen {
                    super::capture_window::show();
                }
                reopen
            })
        };
        if let Some(problem) = problem_note {
            let message = format!("Capture saved, but {problem}");
            app.set_status(message.clone());
            if reopened {
                super::capture_window::problem(message);
            } else {
                super::notification::send(
                    "Capture saved with problems",
                    message,
                    super::notification::Urgency::Critical,
                );
            }
        }
    });
}

fn cancel_capture(delivery: &Delivery) {
    let request_id = delivery.request_id;
    app::post(move |app| {
        if let Some(id) = request_id {
            if crate::ui::capture_view::image_finished(id, None, false, true) {
                super::capture_window::show();
            }
        }
        app.set_status("Capture cancelled");
    });
}

fn fail_capture(delivery: &Delivery, error: impl std::fmt::Display) {
    let request_id = delivery.request_id;
    let message = if delivery.background {
        format!("Scheduled capture failed: {error}")
    } else {
        format!("Capture failed: {error}")
    };
    app::post(move |app| {
        app.set_status(message.clone());
        let reopened = request_id.is_some_and(|id| {
            let reopen = crate::ui::capture_view::image_finished(id, None, true, true);
            if reopen {
                super::capture_window::show();
                super::capture_window::problem(message.clone());
            }
            reopen
        });
        if !reopened {
            super::notification::send(
                "Capture failed",
                message,
                super::notification::Urgency::Critical,
            );
        }
    });
}

pub(super) fn report(message: String) {
    app::post(move |app| app.set_status(message));
}

/// Takes a portal screenshot, saves it to the library and delivers it, waiting `delay` seconds
/// after the desktop dialog when non-zero.
fn capture_image_with(cursor: bool, delay: u64) {
    capture_image_with_delivery(cursor, delay, Delivery::from_settings(false));
}

pub(super) fn capture_image_with_delivery(cursor: bool, delay: u64, delivery: Delivery) {
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
            Ok(Some(path)) => deliver_image(path, delivery),
            Ok(None) => cancel_capture(&delivery),
            Err(error) => fail_capture(&delivery, error),
        }
    });
}

/// Starts a recording, or stops the running one and delivers the video.
pub(crate) fn toggle_recording() {
    toggle_recording_with("none", true, false, None);
}

pub(crate) fn toggle_recording_with(
    audio: &'static str,
    cursor: bool,
    webcam: bool,
    open_in_editor: Option<bool>,
) {
    std::thread::spawn(move || {
        if control::recording_active() {
            let open = RECORDING_OPEN_IN_EDITOR
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .take()
                .unwrap_or_else(|| super::settings::open_in_editor().unwrap_or(true));
            match control::stop_recording() {
                Ok(Some(path)) if open => app::post(move |app| app.incoming(path)),
                Ok(Some(path)) => app::post(move |app| {
                    app.refresh_library();
                    let message = format!("Recording saved to {}", path.display());
                    app.set_status(message.clone());
                    super::notification::send(
                        "Recording saved",
                        message,
                        super::notification::Urgency::Normal,
                    );
                }),
                Ok(None) => report("Recording cancelled".into()),
                Err(error) => {
                    let message = format!("Could not stop the recording: {error}");
                    report(message.clone());
                    super::notification::send(
                        "Recording failed",
                        message,
                        super::notification::Urgency::Critical,
                    );
                }
            }
        } else {
            let open = open_in_editor
                .unwrap_or_else(|| super::settings::open_in_editor().unwrap_or(true));
            report("Choose a screen in the desktop dialog…".into());
            match control::start_recording_with(audio, cursor, webcam) {
                Ok(()) => {
                    *RECORDING_OPEN_IN_EDITOR
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner()) = Some(open);
                    report("Recording… press Record again to stop".into());
                }
                Err(error) => {
                    let message = format!("Could not start recording: {error}");
                    report(message.clone());
                    super::notification::send(
                        "Recording failed",
                        message,
                        super::notification::Urgency::Critical,
                    );
                }
            }
        }
    });
}

pub fn install_capture_handler() {
    winutil::set_capture_handler(|request| match request {
        CaptureRequest::Image => capture_overlay(super::overlay::Mode::Normal, false, 0),
        CaptureRequest::Video => toggle_recording(),
        CaptureRequest::CaptureWindow => app::post(|_| super::capture_window::show()),
    });
    control::set_open_handler(|path| app::post(move |app| app.incoming(path)));
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

/// Fires the daily capture while the application runs.
pub fn start_schedule() {
    use super::schedule::{DailySchedule, LocalMinute};
    use super::settings::ScheduleClaim;
    std::thread::spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_secs(15));
        let Ok(preferences) = super::settings::schedule_preferences() else { continue };
        if !preferences.enabled {
            continue;
        }
        let Ok(now) = LocalMinute::now() else { continue };
        let mut schedule = DailySchedule::new(preferences.time, preferences.last_fired);
        if schedule.should_fire(now)
            && matches!(super::settings::mark_schedule_fired(preferences.time, now.day()), Ok(ScheduleClaim::Claimed))
        {
            capture_image_with_delivery(true, 0, Delivery::from_settings(true));
        }
    });
}

/// Binds the portal global shortcuts chosen in the settings; keeps running until the process exits.
pub fn start_shortcuts() {
    use super::shortcut::{listen, ShortcutAction, ShortcutOptions};
    if !super::settings::global_shortcuts_enabled().unwrap_or(false) {
        return;
    }
    let preset_hotkeys = super::settings::global_preset_shortcuts().unwrap_or(false);
    let (Ok(trigger), Ok(controls), Ok(start)) = (
        super::settings::global_capture_shortcut(),
        super::settings::global_recording_shortcuts(),
        super::settings::global_recording_start(),
    ) else {
        return;
    };
    let options = ShortcutOptions { recording_controls: controls, recording_start: start, preset_hotkeys };
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
                        Some(audio) => toggle_recording_with(audio_name(audio), true, false, None),
                        None => report("The recording start shortcut is disabled.".into()),
                    },
                    ShortcutAction::PauseRecording => {
                        if let Err(error) = control::record_pause() {
                            report(format!("Could not change recording pause state: {error}"));
                        }
                    }
                    ShortcutAction::StopRecording => {
                        if control::recording_active() {
                            toggle_recording_with("none", true, false, None);
                        }
                    }
                    ShortcutAction::Preset(index) => {
                        let preset = super::settings::screen_capture_presets()
                            .ok()
                            .and_then(|presets| presets.into_iter().nth(index));
                        match preset {
                            Some(preset) => match preset.mode() {
                                super::settings::PresetMode::Screen => capture_image_with(
                                    matches!(preset.cursor(), super::capture::CaptureCursor::Included),
                                    preset.delay_seconds(),
                                ),
                                super::settings::PresetMode::Desktop => capture_desktop(),
                                super::settings::PresetMode::Text => capture_text(),
                            },
                            None => report(format!("There is no capture preset number {}.", index + 1)),
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
        "--capture" => capture_overlay(super::overlay::Mode::Normal, false, 0),
        "--record" => toggle_recording(),
        "--capture-interactive" => capture_desktop(),
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

/// The last area the selection overlay captured, for "Repeat last area".
static LAST_AREA: std::sync::Mutex<Option<jiffshot_capture::Rect>> = std::sync::Mutex::new(None);

#[allow(clippy::chunks_exact_to_as_chunks)]
pub(super) fn frame_from_pixmap(pixmap: &tiny_skia::Pixmap) -> jiffshot_capture::Frame {
    let mut frame = jiffshot_capture::Frame::new(pixmap.width(), pixmap.height());
    for (dst, src) in frame.pixels.chunks_exact_mut(4).zip(pixmap.pixels()) {
        let inv = 255 - src.alpha() as u32;
        let over = |c: u8| (c as u32 + inv).min(255) as u8;
        dst.copy_from_slice(&[over(src.blue()), over(src.green()), over(src.red()), 255]);
    }
    frame
}

#[allow(clippy::chunks_exact_to_as_chunks)]
fn document_from_frame(frame: &jiffshot_capture::Frame) -> Option<jiffshot_doc::Document> {
    let mut pixmap = tiny_skia::Pixmap::new(frame.width, frame.height)?;
    for (dst, src) in pixmap.pixels_mut().iter_mut().zip(frame.pixels.chunks_exact(4)) {
        let (b, g, r, a) = (src[0] as u32, src[1] as u32, src[2] as u32, src[3] as u32);
        let premultiply = |c: u32| ((c * a + 127) / 255) as u8;
        *dst = tiny_skia::PremultipliedColorU8::from_rgba(premultiply(r), premultiply(g), premultiply(b), a as u8)?;
    }
    Some(jiffshot_doc::Document::new(pixmap))
}

fn save_frame(frame: &jiffshot_capture::Frame) -> Result<PathBuf, String> {
    let document = document_from_frame(frame).ok_or("the selection is empty")?;
    super::capture::save_capture(&document)
}

/// Grabs the desktop, lets the user select on it with the overlay (like the Windows and macOS
/// one), then saves and delivers the selected part. `repeat` skips the overlay and takes the last
/// area again. Falls back to the desktop's own dialog when the grab or the overlay fails.
pub(crate) fn capture_overlay(mode: super::overlay::Mode, repeat: bool, delay: u64) {
    capture_overlay_with(mode, repeat, delay, Delivery::from_settings(false));
}

pub(super) fn capture_overlay_with(
    mode: super::overlay::Mode,
    repeat: bool,
    delay: u64,
    delivery: Delivery,
) {
    report(if delay == 0 { "Preparing the selection…".into() } else { format!("Capturing in {delay} s…") });
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400 + delay * 1000));
        let frame = match futures_lite::future::block_on(super::screenshot::capture_full_screen()) {
            Ok(Some(document)) => frame_from_pixmap(&document.flatten()),
            Ok(None) => return cancel_capture(&delivery),
            Err(error) => {
                eprintln!("jiffshot: {error}; using the desktop dialog");
                return capture_desktop_with(delivery);
            }
        };
        let last = *LAST_AREA.lock().unwrap();
        if let (true, Some(area)) = (repeat, last) {
            return finish_selection(Some(super::overlay::Selection::Region(area)), frame, delivery);
        }
        let Some(scope) = app::scope() else {
            return fail_capture(&delivery, "The editor is not running");
        };
        let fallback_delivery = delivery.clone();
        let posted = scope.clone().post(move || {
            let select_delivery = delivery.clone();
            let shown = crate::ui::overlay::select(&scope, frame, mode, move |selection, frame| {
                let finish_delivery = select_delivery.clone();
                std::thread::spawn(move || finish_selection(selection, frame, finish_delivery));
            });
            if let Err(error) = shown {
                eprintln!("jiffshot: the selection overlay failed: {error}; using the desktop dialog");
                capture_desktop_with(delivery);
            }
        });
        if posted.is_err() {
            fail_capture(&fallback_delivery, "The editor is not running");
        }
    });
}

fn finish_selection(
    selection: Option<super::overlay::Selection>,
    frame: jiffshot_capture::Frame,
    delivery: Delivery,
) {
    let Some(selection) = selection else { return cancel_capture(&delivery) };
    *LAST_AREA.lock().unwrap() = Some(selection.rect());
    let result = super::overlay::crop(&frame, &selection)
        .ok_or_else(|| "the selection is empty".to_owned())
        .and_then(|cropped| save_frame(&cropped));
    match result {
        Ok(path) => deliver_image(path, delivery),
        Err(error) => fail_capture(&delivery, error),
    }
}

/// The desktop's own screenshot dialog, which offers region and window choices.
/// Captures the whole desktop after `delay` seconds without a chooser; falls back to the
/// screen-sharing chooser when the desktop refuses.
pub(super) fn capture_full_screen_with(cursor: bool, delay: u64, delivery: Delivery) {
    report(if delay == 0 { "Capturing the screen…".into() } else { format!("Capturing in {delay} s…") });
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400 + delay * 1000));
        let result = futures_lite::future::block_on(super::screenshot::capture_full_screen())
            .and_then(|document| match document {
                Some(document) => super::capture::save_capture(&document).map(Some),
                None => Ok(None),
            });
        match result {
            Err(error) => {
                eprintln!("jiffshot: {error}; using the screen-sharing chooser");
                capture_image_with_delivery(cursor, 0, delivery);
            }
            other => finish_single(other, delivery),
        }
    });
}

pub(crate) fn capture_desktop() {
    capture_desktop_with(Delivery::from_settings(false));
}

fn capture_desktop_with(delivery: Delivery) {
    report("Choose what to capture in the desktop dialog…".into());
    std::thread::spawn(move || {
        let result = futures_lite::future::block_on(super::screenshot::capture_interactive())
            .and_then(|document| match document {
                Some(document) => super::capture::save_capture(&document).map(Some),
                None => Ok(None),
            });
        finish_single(result, delivery);
    });
}

fn finish_single(result: Result<Option<PathBuf>, String>, delivery: Delivery) {
    match result {
        Ok(Some(path)) => deliver_image(path, delivery),
        Ok(None) => cancel_capture(&delivery),
        Err(error) => fail_capture(&delivery, error),
    }
}

/// Copies text from a desktop-selected area without opening an image.
pub(crate) fn capture_text() {
    report("Choose an area containing text in the desktop dialog…".into());
    if let Err(error) = std::thread::Builder::new()
        .name("jiffshot-grab-screen-text".to_owned())
        .spawn(|| report(super::screen_text::message(super::screen_text::grab())))
    {
        report(format!("Could not start screen text capture: {error}"));
    }
}

/// Starts a scrolling or panoramic capture; call [`finish_active`] to end it.
pub(super) fn capture_scrolling(mode: super::scroll::Mode) {
    capture_scrolling_with(mode, Delivery::from_settings(false));
}

pub(super) fn capture_scrolling_with(mode: super::scroll::Mode, delivery: Delivery) {
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
                finish_single(saved.map(Some), delivery);
            }
            Ok(None) => cancel_capture(&delivery),
            Err(error) => fail_capture(&delivery, error),
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
    let delivery = Delivery::from_settings(true);
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
                    deliver_image(path, delivery.clone());
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
