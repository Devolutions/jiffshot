//! The editor application: view-model state, commands, files, the library
//! tray and quick styles. Runs on the UI thread; background work posts its
//! results back with [`post`].
//!
//! The state lives in a thread-local `RefCell`. Model calls change state
//! and queue one [`App::sync`] on the dispatcher instead of publishing
//! directly: the window's model is busy during a call, and a published
//! value can make a control write a coerced value straight back (a slider
//! clamping to a new range), which would re-enter it.
//! Writes that arrive while a sync is publishing find the state borrowed
//! and are dropped, which is right for echoes.

use crate::canvas::{with_editor, EDITOR, PENDING_EVENTS};
use crate::vm::*;
use crate::{library, settings, winutil};
use jiffshot_capture::library::{edits_of, Library, EDITS_EXT};
use jiffshot_doc::editor::{self as ed, Tool, EVENT_COLOR_PICKED};
use jiffshot_doc::object::{Cap, Dash, Kind, Object, Style};
use jiffshot_doc::raster::trim_bounds;
use jiffshot_doc::text::Align;
use jiffshot_doc::{combine, effects, io, Document, Rgba};
use rustolonia::storage::{
    FileTypeFilter, OpenFilePickerOptions, PickerOutcome, SaveFilePickerOptions, WellKnownFolder,
};
use rustolonia::{AppScope, Result};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Runs `f` on the app state, unless it is already in use further up the
/// stack (a re-entrant echo from a window).
pub fn with<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| {
        let mut guard = cell.try_borrow_mut().ok()?;
        guard.as_mut().map(f)
    })
}

/// Like [`with`], then queues publishing whatever changed.
pub fn act(f: impl FnOnce(&mut App)) -> Result<()> {
    let _t = crate::trace::span("act");
    with(f);
    schedule_sync();
    Ok(())
}

static SYNC_QUEUED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(target_os = "macos")]
struct CaptureRefreshLatch(std::sync::atomic::AtomicU8);

#[cfg(target_os = "macos")]
impl CaptureRefreshLatch {
    const DIRTY: u8 = 1;
    const QUEUED: u8 = 2;

    const fn new() -> Self {
        Self(std::sync::atomic::AtomicU8::new(0))
    }

    fn request(&self) -> bool {
        self.0
            .fetch_or(Self::DIRTY | Self::QUEUED, Ordering::AcqRel)
            & Self::QUEUED
            == 0
    }

    fn begin(&self) {
        self.0.fetch_and(!Self::DIRTY, Ordering::AcqRel);
    }

    fn retry(&self) {
        self.0.fetch_or(Self::DIRTY, Ordering::AcqRel);
    }

    fn queue_if_dirty(&self) -> bool {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state & (Self::DIRTY | Self::QUEUED) == Self::DIRTY)
                    .then_some(state | Self::QUEUED)
            })
            .is_ok()
    }

    fn finish(&self) -> bool {
        self.0.fetch_and(!Self::QUEUED, Ordering::AcqRel);
        self.queue_if_dirty()
    }

    fn abandon(&self) {
        self.0.fetch_and(!Self::QUEUED, Ordering::AcqRel);
    }
}

#[cfg(target_os = "macos")]
static CAPTURE_REFRESH: CaptureRefreshLatch = CaptureRefreshLatch::new();
#[cfg(target_os = "macos")]
static CAPTURE_REFRESH_HELD_FOR_SELFTEST: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(target_os = "macos")]
static CAPTURE_REFRESH_BUSY_FOR_SELFTEST: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(target_os = "macos")]
pub(crate) fn request_capture_state_refresh() {
    if CAPTURE_REFRESH.request() {
        queue_capture_state_refresh();
    }
}

#[cfg(target_os = "macos")]
fn queue_capture_state_refresh() {
    if let Err(error) =
        crate::shutdown_ui_macos::post("a capture state update", refresh_capture_state_when_ready)
    {
        CAPTURE_REFRESH.abandon();
        jiffshot_workflow::log(&format!("Could not deliver the capture state: {error}"));
    }
}

#[cfg(target_os = "macos")]
fn refresh_capture_state_when_ready() {
    CAPTURE_REFRESH.begin();
    let Some((settings, state)) = jiffshot_workflow::capture_state_snapshot() else {
        if CAPTURE_REFRESH.finish() {
            queue_capture_state_refresh();
        }
        return;
    };
    enum Access {
        Busy,
        Missing,
        Updated,
    }
    let access = APP.with(|cell| {
        let _held = CAPTURE_REFRESH_HELD_FOR_SELFTEST
            .load(Ordering::Acquire)
            .then(|| cell.try_borrow_mut().ok())
            .flatten();
        match cell.try_borrow_mut() {
            Err(_) => Access::Busy,
            Ok(mut guard) => match guard.as_mut() {
                None => Access::Missing,
                Some(app) if app.exiting => Access::Busy,
                Some(app) => {
                    app.set_widget(state.show_widget);
                    crate::capture_window::refresh(&state, settings);
                    crate::tray::publish(state);
                    Access::Updated
                }
            },
        }
    });
    match access {
        Access::Busy => {
            CAPTURE_REFRESH_BUSY_FOR_SELFTEST.fetch_add(1, Ordering::Relaxed);
            CAPTURE_REFRESH.retry();
            if let Err(error) = crate::shutdown_ui_macos::retry_named(
                "a capture state update",
                refresh_capture_state_when_ready,
            ) {
                CAPTURE_REFRESH.abandon();
                jiffshot_workflow::log(&format!(
                    "Could not retry the capture state update: {error}"
                ));
            }
        }
        Access::Missing => {
            CAPTURE_REFRESH.abandon();
            jiffshot_workflow::log(
                "Could not display the capture state: the editor is unavailable",
            );
        }
        Access::Updated => {
            schedule_sync();
            if CAPTURE_REFRESH.finish() {
                queue_capture_state_refresh();
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn hold_capture_state_for_selftest(held: bool) {
    assert!(
        std::env::var_os("JIFFSHOT_UITEST").is_some()
            && (std::env::var_os("JIFFSHOT_UITEST_SETTINGS_HANDOFF").is_some()
                || std::env::var_os("JIFFSHOT_UITEST_STARTUP_WATCH").is_some()),
        "not a capture-state self-test"
    );
    CAPTURE_REFRESH_HELD_FOR_SELFTEST.store(held, Ordering::Release);
    if !held && CAPTURE_REFRESH.queue_if_dirty() {
        queue_capture_state_refresh();
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn capture_state_busy_attempts_for_selftest() -> usize {
    CAPTURE_REFRESH_BUSY_FOR_SELFTEST.load(Ordering::Relaxed)
}

#[cfg(target_os = "macos")]
pub(crate) fn report_capture_settings_notice(title: &'static str, text: String) {
    enum Access {
        Busy,
        Missing,
        Updated,
    }
    let access = APP.with(|cell| match cell.try_borrow_mut() {
        Err(_) => Access::Busy,
        Ok(mut guard) => match guard.as_mut() {
            None => Access::Missing,
            Some(app) if app.exiting => Access::Busy,
            Some(app) => {
                app.toast(title, &text, true);
                Access::Updated
            }
        },
    });
    match access {
        Access::Updated => schedule_sync(),
        Access::Missing => {
            jiffshot_workflow::log(&format!("{title}: {text} (the editor is unavailable)"))
        }
        Access::Busy => {
            let retained = text.clone();
            if let Err(error) =
                crate::shutdown_ui_macos::retry_named("a Capture settings notice", move || {
                    report_capture_settings_notice(title, retained);
                })
            {
                jiffshot_workflow::log(&format!("Could not show {title}: {text} ({error})"));
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn reserved_share_name(name: &str) -> bool {
    crate::share::BUILT_IN.contains(&name) || matches!(name, "Paint" | "Add program…")
}

#[cfg(target_os = "macos")]
fn font_notice_for_style(tool: Tool, style: &Style) -> Option<String> {
    let (bold, italic) = match tool {
        Tool::Text | Tool::Callout => (style.bold, style.italic),
        Tool::Step => (true, false),
        _ => return None,
    };
    let substitution = jiffshot_doc::text::font_substitution(&style.font, bold, italic)?;
    jiffshot_doc::text::describe_font_substitutions(std::slice::from_ref(&substitution))
}

/// Publishes changes once the current dispatcher job (and the model lock
/// it may hold) is done. Several requests before then share one sync.
pub fn schedule_sync() {
    if SYNC_QUEUED.swap(true, Ordering::AcqRel) {
        return;
    }
    let scope = SCOPE.lock().unwrap().clone();
    if let Some(scope) = scope {
        let _ = scope.post(|| {
            SYNC_QUEUED.store(false, Ordering::Release);
            with(|app| app.sync());
        });
    }
}

/// Runs `f` on the UI thread with the app state, from any thread.
pub fn post(f: impl FnOnce(&mut App) + Send + 'static) {
    let scope = SCOPE.lock().unwrap().clone();
    if let Some(scope) = scope {
        let _ = scope.post(move || {
            let _ = act(f);
        });
    }
}

#[cfg(target_os = "macos")]
pub fn post_incoming(path: PathBuf) -> std::io::Result<()> {
    let scope = SCOPE
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| std::io::Error::other("the editor is not running"))?;
    scope
        .post(move || {
            if std::env::var_os("JIFFSHOT_UITEST").is_some()
                && std::env::var_os("JIFFSHOT_UITEST_CONTROL_REENTRANT").is_some()
            {
                APP.with(|cell| {
                    let _busy = cell.try_borrow_mut().ok();
                    finish_incoming_when_ready(path);
                });
            } else {
                finish_incoming_when_ready(path);
            }
        })
        .map_err(|error| {
            std::io::Error::other(format!("could not queue a forwarded file: {error}"))
        })
}

#[cfg(target_os = "macos")]
fn finish_incoming_when_ready(path: PathBuf) {
    enum Access {
        Busy,
        Finished,
        Missing,
        Exiting,
    }
    let mut pending = Some(path);
    let state = APP.with(|cell| match cell.try_borrow_mut() {
        Err(_) => Access::Busy,
        Ok(mut guard) => match guard.as_mut() {
            Some(app) if app.exiting => Access::Exiting,
            Some(app) => {
                let path = pending.take().expect("forwarded file is present");
                app.incoming(path.clone());
                if std::env::var_os("JIFFSHOT_UITEST").is_some()
                    && std::env::var_os("JIFFSHOT_UITEST_CONTROL_REENTRANT").is_some()
                    && (app.source.as_deref() == Some(path.as_path())
                        || app.path.as_deref() == Some(path.as_path()))
                {
                    jiffshot_workflow::log(
                        "control fixture: forwarded file opened after an editor-busy retry",
                    );
                }
                Access::Finished
            }
            None => Access::Missing,
        },
    });
    match state {
        Access::Finished => schedule_sync(),
        Access::Missing => {
            jiffshot_workflow::log("Could not open a forwarded file: the editor is unavailable.")
        }
        Access::Exiting => jiffshot_workflow::log("Cancelled a forwarded file while quitting."),
        Access::Busy => {
            let path = pending.expect("forwarded file is present while the editor is busy");
            if let Err(error) =
                crate::shutdown_ui_macos::retry_named("a forwarded file retry", move || {
                    finish_incoming_when_ready(path)
                })
            {
                jiffshot_workflow::log(&format!("Could not retry a forwarded file: {error}"));
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn post_exit() -> std::result::Result<(), String> {
    crate::shutdown_ui_macos::post("an exit request", exit_when_ready)
}

#[cfg(target_os = "macos")]
fn exit_when_ready() {
    enum Access {
        Busy,
        Finished,
        Missing,
    }
    let state = APP.with(|cell| match cell.try_borrow_mut() {
        Err(_) => Access::Busy,
        Ok(mut guard) => match guard.as_mut() {
            Some(app) => {
                app.exit();
                Access::Finished
            }
            None => Access::Missing,
        },
    });
    match state {
        Access::Finished => schedule_sync(),
        Access::Missing => jiffshot_workflow::log("Could not quit: the editor is unavailable."),
        Access::Busy => {
            if let Err(error) =
                crate::shutdown_ui_macos::retry_named("an exit request retry", exit_when_ready)
            {
                jiffshot_workflow::log(&format!("Could not retry the exit request: {error}"));
            }
        }
    }
}

/// The capture side's latest HUD, not yet shown.
static PENDING_HUD: Mutex<Option<String>> = Mutex::new(None);

/// Shows the capture side's HUD (`json`), from any thread. While recording
/// it changes every tick: only the latest is shown, so a UI thread that
/// falls behind skips states instead of queueing them, and showing one does
/// not sync the editor.
pub fn post_hud(json: String) {
    let queued = PENDING_HUD.lock().unwrap().replace(json).is_some();
    if queued {
        return;
    }
    let scope = SCOPE.lock().unwrap().clone();
    if let Some(scope) = scope {
        let _ = scope.post(|| {
            let Some(json) = PENDING_HUD.lock().unwrap().take() else {
                return;
            };
            if with(|app| app.set_hud(&json)).is_none() {
                // The state is in use further up the stack: next turn. The
                // last HUD (all panels gone) must not be lost.
                post_hud(json);
            }
        });
    }
}

/// Runs `f` on the UI thread without syncing the editor afterwards, for
/// changes that only reach other windows (the overlay, Screen Draw).
pub fn post_quiet(f: impl FnOnce(&mut App) + Send + 'static) {
    let scope = SCOPE.lock().unwrap().clone();
    if let Some(scope) = scope {
        let _ = scope.post(move || {
            with(f);
        });
    }
}

static SCOPE: Mutex<Option<AppScope>> = Mutex::new(None);

#[cfg(target_os = "macos")]
fn spawn_shutdown_worker<T: Send + 'static>(
    work: T,
    spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<()>,
    run: impl FnOnce(T) + Send + 'static,
) -> std::result::Result<(), (std::io::Error, T)> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    if let Err(error) = spawn(Box::new(move || match receiver.recv() {
        Ok(work) => run(work),
        Err(_) => jiffshot_workflow::log("the shutdown worker did not receive its work"),
    })) {
        return Err((error, work));
    }
    sender.send(work).map_err(|error| {
        (
            std::io::Error::other("the shutdown worker stopped before receiving its work"),
            error.0,
        )
    })
}

#[cfg(target_os = "macos")]
fn retry_on_ui(callback: impl FnOnce() + Send + 'static) {
    if let Err(error) = crate::shutdown_ui_macos::retry(callback) {
        jiffshot_workflow::log(&format!("could not schedule a shutdown retry: {error}"));
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn native_exit_requested(token: i64) {
    native_exit_requested_after(token, 0);
}

#[cfg(target_os = "macos")]
fn native_exit_requested_after(token: i64, attempts: usize) {
    match crate::termination_macos::pending_token() {
        Ok(Some(pending)) if pending == token => {}
        Ok(_) => {
            crate::shutdown_ui_macos::report("ignored a stale macOS termination callback");
            return;
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            if attempts == 0 || attempts % 1200 == 0 {
                crate::shutdown_ui_macos::report(
                    "macOS termination coordination is busy; retrying the pending request",
                );
            }
            retry_on_ui(move || native_exit_requested_after(token, attempts.saturating_add(1)));
            return;
        }
        Err(error) => {
            crate::shutdown_ui_macos::report(&format!(
                "could not inspect the macOS termination request: {error}"
            ));
            return;
        }
    }
    enum Access {
        Busy,
        Finished,
        Missing,
    }
    let state = APP.with(|cell| match cell.try_borrow_mut() {
        Err(_) => Access::Busy,
        Ok(mut guard) => match guard.as_mut() {
            Some(app) => match app.native_termination {
                None => {
                    app.native_termination = Some(token);
                    if !app.exiting {
                        app.exit();
                    }
                    Access::Finished
                }
                Some(current) if current == token => Access::Finished,
                Some(_) => Access::Busy,
            },
            None => Access::Missing,
        },
    });
    match state {
        Access::Finished => schedule_sync(),
        Access::Busy => {
            if attempts == 0 || attempts % 1200 == 0 {
                crate::shutdown_ui_macos::report(
                    "macOS termination state is busy; retrying the pending request",
                );
            }
            retry_on_ui(move || native_exit_requested_after(token, attempts.saturating_add(1)));
        }
        Access::Missing => {
            jiffshot_workflow::log("could not quit: the editor is unavailable");
            if let Err(error) = crate::termination_macos::complete(token, false) {
                jiffshot_workflow::log(&format!("could not cancel macOS termination: {error}"));
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn hold_cancelled_token_for_selftest(token: i64) -> std::result::Result<(), String> {
    if std::env::var_os("JIFFSHOT_UITEST").is_none() {
        return Err("the delayed cancellation fixture requires UI self-test mode".into());
    }
    if crate::termination_macos::pending_token()
        .map_err(|error| error.to_string())?
        .is_some()
    {
        return Err("a macOS termination request is already pending".into());
    }
    APP.with(|cell| {
        let mut guard = cell.try_borrow_mut().map_err(|_| "the editor is busy")?;
        let app = guard.as_mut().ok_or("the editor is unavailable")?;
        if app.exiting || app.native_termination.is_some() {
            return Err("the editor is already quitting".into());
        }
        app.native_termination = Some(token);
        app.exiting = true;
        Ok(())
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn finish_cancelled_token_for_selftest(token: i64) {
    native_cancelled(token);
}

#[cfg(target_os = "macos")]
fn adopt_pending_termination(
    current: &mut Option<i64>,
    pending: Option<i64>,
) -> std::result::Result<(), String> {
    match (*current, pending) {
        (None, None) => Ok(()),
        (None, Some(token)) => {
            *current = Some(token);
            Ok(())
        }
        (Some(current), Some(pending)) if current == pending => Ok(()),
        (Some(_), None) => Err("the macOS termination request is no longer pending".into()),
        (Some(_), Some(_)) => Err("the macOS termination request changed while quitting".into()),
    }
}

#[cfg(target_os = "macos")]
fn native_cancelled(token: i64) {
    let updated = APP.with(|cell| match cell.try_borrow_mut() {
        Err(_) => false,
        Ok(mut guard) => {
            if let Some(app) = guard.as_mut() {
                if app.native_termination == Some(token) {
                    app.native_termination = None;
                    app.exiting = false;
                }
            }
            true
        }
    });
    if updated {
        schedule_sync();
    } else {
        retry_on_ui(move || native_cancelled(token));
    }
}

#[cfg(target_os = "macos")]
fn native_cancellation_failed(token: i64, message: String) {
    jiffshot_workflow::log(&message);
    let mut message = Some(message);
    let updated = APP.with(|cell| match cell.try_borrow_mut() {
        Err(_) => false,
        Ok(mut guard) => {
            if let Some(app) = guard.as_mut() {
                if app.native_termination == Some(token) {
                    app.exiting = false;
                    app.toast(
                        "Could not cancel macOS termination",
                        message.as_deref().expect("cancellation error is present"),
                        true,
                    );
                }
            }
            true
        }
    });
    if updated {
        schedule_sync();
    } else {
        let message = message
            .take()
            .expect("cancellation error is present while the editor is busy");
        retry_on_ui(move || native_cancellation_failed(token, message));
    }
}

#[cfg(target_os = "macos")]
fn native_completion_failed(token: i64, error: String) {
    let mut error = Some(error);
    let updated = APP.with(|cell| match cell.try_borrow_mut() {
        Err(_) => false,
        Ok(mut guard) => {
            if let Some(app) = guard.as_mut() {
                if app.native_termination == Some(token) {
                    app.exit_failed(error.take().expect("completion error is present"), true);
                }
            }
            true
        }
    });
    if updated {
        schedule_sync();
    } else {
        let error = error.expect("completion error is present while the editor is busy");
        retry_on_ui(move || native_completion_failed(token, error));
    }
}

#[cfg(target_os = "macos")]
fn finish_exit_when_ready(result: std::result::Result<(), String>) {
    enum Access {
        Busy,
        Finished,
        Missing,
    }
    let mut pending = Some(result);
    let state = APP.with(|cell| match cell.try_borrow_mut() {
        Err(_) => Access::Busy,
        Ok(mut guard) => match guard.as_mut() {
            Some(app) => {
                app.finish_exit(pending.take().expect("shutdown result is present"));
                Access::Finished
            }
            None => Access::Missing,
        },
    });
    match state {
        Access::Finished => schedule_sync(),
        Access::Missing => {
            jiffshot_workflow::log("could not finish quitting: the editor is unavailable")
        }
        Access::Busy => {
            let result = pending.expect("shutdown result is present while the editor is busy");
            retry_on_ui(move || finish_exit_when_ready(result));
        }
    }
}

#[cfg(target_os = "macos")]
fn finish_library_copy_when_ready(
    generation: u64,
    change_count: isize,
    result: std::result::Result<crate::library_transfer_macos::Prepared, String>,
) {
    enum Access {
        Busy,
        Finished,
        Missing,
        Exiting,
    }
    let mut pending = Some(result);
    let state = APP.with(|cell| match cell.try_borrow_mut() {
        Err(_) => Access::Busy,
        Ok(mut guard) => match guard.as_mut() {
            Some(app) if app.exiting => Access::Exiting,
            Some(app) => {
                app.finish_library_copy(
                    generation,
                    change_count,
                    pending.take().expect("copy result is present"),
                );
                Access::Finished
            }
            None => Access::Missing,
        },
    });
    match state {
        Access::Finished => schedule_sync(),
        Access::Missing => jiffshot_workflow::log(
            "Could not complete the library copy: the editor is unavailable.",
        ),
        Access::Exiting => jiffshot_workflow::log("Cancelled the library copy while quitting."),
        Access::Busy => {
            let result = pending.expect("copy result is present while the editor is busy");
            if let Err(error) =
                crate::shutdown_ui_macos::retry_named("a library copy retry", move || {
                    finish_library_copy_when_ready(generation, change_count, result);
                })
            {
                jiffshot_workflow::log(&format!("Could not retry the library copy: {error}"));
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn prepare_library_drag(token: i64, path: PathBuf) {
    if !crate::file_drag_macos::is_pending(token) {
        return;
    }
    enum Access {
        Busy,
        Missing,
        Failed(String),
        Ready(Vec<crate::library_transfer_macos::Source>),
    }
    let state = APP.with(|cell| match cell.try_borrow_mut() {
        Err(_) => Access::Busy,
        Ok(mut guard) => match guard.as_mut() {
            None => Access::Missing,
            Some(app) if app.exiting => Access::Failed("Jiffshot is quitting".into()),
            Some(app) if !app.library_items.iter().any(|item| item.path == path) => {
                Access::Failed("the capture is no longer in the library view".into())
            }
            Some(app) => match app.library_sources(app.library_targets(&path)) {
                Ok(sources) => {
                    app.set_status("Preparing captures for dragging…");
                    Access::Ready(sources)
                }
                Err(error) => Access::Failed(error),
            },
        },
    });
    let sources = match state {
        Access::Busy => {
            retry_on_ui(move || prepare_library_drag(token, path));
            return;
        }
        Access::Missing => {
            return crate::file_drag_macos::preparation_finished(
                token,
                Err("the editor is unavailable".into()),
            )
        }
        Access::Failed(error) => {
            return crate::file_drag_macos::preparation_finished(token, Err(error))
        }
        Access::Ready(sources) => sources,
    };
    let worker = std::thread::Builder::new()
        .name("jiffshot-library-drag".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::file_drag_macos::wait_for_selftest_preparation()
                    .and_then(|()| crate::library_transfer_macos::prepare_drag(sources))
            }))
            .unwrap_or_else(|_| Err("the file-drag preparation worker panicked".into()));
            if let Err(error) =
                crate::shutdown_ui_macos::post("the library drag preparation result", move || {
                    crate::file_drag_macos::preparation_finished(token, result);
                })
            {
                jiffshot_workflow::log(&format!("Could not deliver the library drag: {error}"));
            }
        });
    if let Err(error) = worker {
        crate::file_drag_macos::preparation_finished(
            token,
            Err(format!("could not start the file-drag worker: {error}")),
        );
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn report_library_drag_error(message: String) {
    report_library_drag_status(message, false);
}

#[cfg(target_os = "macos")]
pub(crate) fn report_library_drag_fonts(substitutions: Vec<jiffshot_doc::text::FontSubstitution>) {
    if let Some(notice) = jiffshot_doc::text::describe_font_substitutions(&substitutions) {
        report_library_drag_status(format!("{notice}; Dragged captures"), true);
    }
}

#[cfg(target_os = "macos")]
fn report_library_drag_status(message: String, warn: bool) {
    let mut message = Some(message);
    let updated = APP.with(|cell| match cell.try_borrow_mut() {
        Err(_) => false,
        Ok(mut guard) => {
            if let Some(app) = guard.as_mut() {
                let message = message.take().expect("the library drag status is present");
                app.set_status(&message);
                if warn {
                    app.toast(
                        "Font substitution",
                        message
                            .strip_prefix("Font substitution: ")
                            .unwrap_or(&message),
                        true,
                    );
                }
            } else {
                jiffshot_workflow::log(
                    message
                        .as_deref()
                        .expect("the library drag status is present"),
                );
            }
            true
        }
    });
    if updated {
        schedule_sync();
    } else {
        let message = message.expect("the library drag status is present while the editor is busy");
        retry_on_ui(move || report_library_drag_status(message, warn));
    }
}

pub fn install(scope: AppScope, opening: Option<PathBuf>) {
    *SCOPE.lock().unwrap() = Some(scope.clone());
    let settings = settings::Settings::load();
    let root = jiffshot_capture::config::library_root();
    let library = Library::new(root);
    APP.with(|cell| {
        *cell.borrow_mut() = Some(App {
            scope,
            sink: None,
            shown: Default::default(),
            path: None,
            source: None,
            library,
            exiting: false,
            #[cfg(target_os = "macos")]
            native_termination: None,
            #[cfg(target_os = "macos")]
            pending_playback_shutdown: Vec::new(),
            toasts: 0,
            settings,
            frame: 0,
            focus: 0,
            zoom: 0,
            status: String::new(),
            object_clipboard: None,
            library_items: Vec::new(),
            library_selected: Default::default(),
            #[cfg(target_os = "macos")]
            library_copy_generation: 0,
            library_entries: Vec::new(),
            shown_filters: Vec::new(),
            canvas_drop: None,
            opening,
            last_tool: None,
            last_kind: None,
            video: None,
            narration: None,
            narration_request: None,
        })
    });
}

/// Values last sent to the windows, so unchanged ones are not resent.
#[derive(Default)]
pub struct Shown {
    strings: std::collections::HashMap<&'static str, String>,
    numbers: std::collections::HashMap<&'static str, f64>,
    flags: std::collections::HashMap<&'static str, bool>,
}

pub struct App {
    scope: AppScope,
    /// Set once Exit started, so the capture side stopping does not start it again.
    exiting: bool,
    #[cfg(target_os = "macos")]
    native_termination: Option<i64>,
    #[cfg(target_os = "macos")]
    pending_playback_shutdown: Vec<crate::video::PlaybackShutdown>,
    /// Toasts shown so far, so each one is a new value of `Toast`.
    toasts: u64,
    pub(crate) sink: Option<MainViewModelSink>,
    shown: Shown,
    /// Where Save writes: a `.jsd` file, or `None` for a new document.
    path: Option<PathBuf>,
    /// The image or capture the document was opened from.
    source: Option<PathBuf>,
    pub library: Library,
    pub settings: settings::Settings,
    frame: i64,
    focus: i64,
    zoom: i64,
    status: String,
    object_clipboard: Option<Object>,
    library_items: Vec<library::Item>,
    library_selected: std::collections::HashSet<PathBuf>,
    #[cfg(target_os = "macos")]
    library_copy_generation: u64,
    library_entries: Vec<library::Entry>,
    shown_filters: Vec<String>,
    /// When images were last dropped onto the canvas.
    canvas_drop: Option<std::time::Instant>,
    opening: Option<PathBuf>,
    last_tool: Option<Tool>,
    last_kind: Option<String>,
    pub video: Option<crate::video::Session>,
    pub narration: Option<crate::video::Narration>,
    pub(crate) narration_request: Option<u64>,
}

fn opacity_percent(style: &Style) -> f64 {
    (style.opacity * 100.0).round() as f64
}

fn align_name(a: Align) -> &'static str {
    match a {
        Align::Left => "Left",
        Align::Center => "Center",
        Align::Right => "Right",
    }
}

fn cap_name(c: Cap) -> &'static str {
    c.name()
}

fn dash_name(d: Dash) -> &'static str {
    match d {
        Dash::Solid => "Solid",
        Dash::Dash => "Dash",
        Dash::Dot => "Dot",
    }
}

/// Sections of the properties panel for a tool or selected object kind.
fn panel_for(kind: &str) -> &'static str {
    match kind {
        "Arrow" | "Line" => "stroke;width;caps;dash;shadow;opacity",
        "Shape" => "variant;stroke;fill;gradient;width;dash;shadow;opacity;rotation",
        "Text" => "text;font;textopts;textcolor;fill;gradient;stroke;width;shadow;opacity;rotation",
        "Callout" => "variant;tails;text;font;textopts;textcolor;fill;gradient;stroke;width;shadow;opacity;rotation",
        "Step" => "variant;text;font;textcolor;fill;gradient;stroke;width;shadow",
        "Pen" | "Marker" => "variant;amount;stroke;width;opacity;shadow",
        "Highlight" => "variant;fill;width",
        "Blur" => "variant;amount;fill",
        "Magnify" => "variant;amount;stroke;width;shadow",
        "Spotlight" => "variant;amount",
        "Stamp" => "variant;shadow;opacity;rotation",
        "Image" => "stroke;width;shadow;opacity;rotation",
        "Drop zone" => "zone;stroke;width;shadow",
        "Fill" => "fill;amount",
        "Cut Out" => "variant",
        "Selection" => "variant;backfill",
        "Magic Wand" => "amount;backfill",
        "Smart Move" => "backfill",
        "Eraser" => "variant;width",
        "Group" => "shadow;opacity",
        _ => "",
    }
}

fn amount_label(kind: &str) -> &'static str {
    match kind {
        "Blur" => "Intensity",
        "Magnify" => "Zoom",
        "Spotlight" => "Dim",
        "Magic Wand" | "Fill" => "Tolerance",
        "Pen" | "Marker" => "Smoothing",
        _ => "",
    }
}

/// What the properties panel shows, read from the editor in one go.
struct Snapshot {
    tool: Tool,
    kind: Option<String>,
    style: Style,
    text: String,
    variant: String,
    variants: Vec<String>,
    amount: f64,
    rotation: f64,
    size: (u32, u32),
    objects: usize,
    undo: Option<String>,
    redo: Option<String>,
    modified: bool,
    damage: bool,
    picked: Option<Rgba>,
    selected_count: usize,
    background_fill: &'static str,
}

impl App {
    pub fn attach(&mut self, sink: MainViewModelSink) -> Result<()> {
        for f in jiffshot_doc::text::families() {
            sink.add_fonts(f)?;
        }
        for (name, _) in effects::catalog(Rgba::BLACK) {
            sink.add_effects(name)?;
        }
        for name in effects::INTERACTIVE {
            sink.add_effects(*name)?;
        }
        for name in crate::share::BUILT_IN {
            sink.add_shares(*name)?;
        }
        for (name, _) in &self.settings.programs {
            sink.add_shares(name)?;
        }
        sink.add_shares("Add program…")?;
        for path in self.settings.recent.iter().filter(|p| p.exists()) {
            sink.add_recent_files(file_uri(path))?;
        }
        for t in ["Combine side by side", "Combine stacked", "Combine grid"]
            .iter()
            .chain(combine::TEMPLATES)
        {
            sink.add_templates(*t)?;
        }
        for t in combine::THEMES {
            sink.add_template_themes(t.name)?;
        }
        self.sink = Some(sink);
        if let Some(window) = self.scope.main_window() {
            use rustolonia::storage::{DragDropEffects, FileDropEvent};
            let _ =
                self.scope
                    .on_file_drop(&window, DragDropEffects::COPY, |event| {
                        if let FileDropEvent::Drop { items, .. } = event {
                            let paths: Vec<PathBuf> = items
                                .iter()
                                .filter_map(|i| i.local_path().map(Path::to_path_buf))
                                .collect();
                            post(move |app| {
                                // Files dropped on the canvas were placed there.
                                if app.canvas_drop.is_some_and(|t| {
                                    t.elapsed() < std::time::Duration::from_secs(2)
                                }) {
                                    return;
                                }
                                if let Some(first) = paths.first() {
                                    app.open_path(first);
                                }
                            });
                        }
                    });
        }
        if let Some(path) = self.opening.take() {
            self.open_path(&path);
        } else {
            self.set_status(
                "Capture with Print Screen or the tray icon, open an image, or paste one.",
            );
        }
        self.refresh_library();
        self.load_stamps();
        schedule_sync();
        Ok(())
    }

    pub fn detach(&mut self) {
        #[cfg(target_os = "macos")]
        {
            // The reactive runtime has already stopped; autosave must not publish to it.
            self.sink = None;
            if let Err(error) = self.autosave_checked() {
                jiffshot_workflow::log(&format!("Final editor autosave failed: {error}"));
            }
        }
        #[cfg(not(target_os = "macos"))]
        self.autosave();
        self.stop_video();
        self.sink = None;
    }

    pub fn set_status(&mut self, text: impl Into<String>) {
        self.status = text.into();
    }

    #[cfg(target_os = "macos")]
    fn status_with_font_substitutions(
        &mut self,
        text: impl Into<String>,
        substitutions: &[jiffshot_doc::text::FontSubstitution],
    ) {
        let text = text.into();
        if let Some(notice) = jiffshot_doc::text::describe_font_substitutions(substitutions) {
            self.set_status(format!("{notice}; {text}"));
            self.toast(
                "Font substitution",
                notice
                    .strip_prefix("Font substitution: ")
                    .unwrap_or(&notice),
                true,
            );
        } else {
            self.set_status(text);
        }
    }

    // ---- publishing ----------------------------------------------------

    fn put_str(
        &mut self,
        name: &'static str,
        value: &str,
        set: impl Fn(&MainViewModelSink, &str) -> Result<()>,
    ) {
        if self.shown.strings.get(name).map(String::as_str) != Some(value) {
            if let Some(sink) = &self.sink {
                let _ = set(sink, value);
            }
            self.shown.strings.insert(name, value.to_owned());
        }
    }

    fn put_num(
        &mut self,
        name: &'static str,
        value: f64,
        set: impl Fn(&MainViewModelSink, f64) -> Result<()>,
    ) {
        if self.shown.numbers.get(name) != Some(&value) {
            if let Some(sink) = &self.sink {
                let _ = set(sink, value);
            }
            self.shown.numbers.insert(name, value);
        }
    }

    fn put_flag(
        &mut self,
        name: &'static str,
        value: bool,
        set: impl Fn(&MainViewModelSink, bool) -> Result<()>,
    ) {
        if self.shown.flags.get(name) != Some(&value) {
            if let Some(sink) = &self.sink {
                let _ = set(sink, value);
            }
            self.shown.flags.insert(name, value);
        }
    }

    fn snapshot(&self) -> Option<Snapshot> {
        let _t = crate::trace::span("snapshot");
        let mut guard = EDITOR.lock();
        let e = guard.as_mut()?;
        let selected = e.selected().cloned();
        let tool = e.tool();
        let kind = selected.as_ref().map(|o| {
            if o.zone.is_some() {
                "Drop zone".to_owned()
            } else {
                o.kind_name().to_owned()
            }
        });
        let variant_tool = selected.as_ref().map(ed::tool_for).unwrap_or(tool);
        let variant = selected
            .as_ref()
            .and_then(ed::variant_of)
            .unwrap_or_else(|| e.variant(tool));
        Some(Snapshot {
            tool,
            kind,
            style: e.current_style(),
            text: selected
                .as_ref()
                .and_then(|o| o.text().map(str::to_owned))
                .unwrap_or_default(),
            variant,
            variants: e.variants_for(variant_tool),
            amount: e.selected_amount().unwrap_or_else(|| e.tool_amount(tool)) as f64,
            rotation: selected.as_ref().map_or(0.0, |o| o.rotation as f64),
            size: e.document().size(),
            objects: e.document().objects.len(),
            undo: e.undo_label().map(str::to_owned),
            redo: e.redo_label().map(str::to_owned),
            modified: e.is_modified(),
            damage: e.has_damage(),
            picked: e.picked_color(),
            selected_count: e.selected_ids().len(),
            background_fill: e.background_fill.name(),
        })
    }

    /// Publishes everything that changed since the last call.
    pub fn sync(&mut self) {
        let _t = crate::trace::span("sync");
        let events = PENDING_EVENTS.swap(0, Ordering::Relaxed)
            | with_editor(|e| e.take_events()).unwrap_or(0);
        let Some(s) = self.snapshot() else { return };
        if let Some(color) = s.picked {
            let hex = color.to_hex();
            self.set_status(format!("Picked {hex} — it is now the stroke color."));
            with_editor(|e| {
                let prev = e.tool();
                e.set_tool(Tool::Arrow);
                e.update_style("stroke", |st| st.stroke = color);
                e.set_tool(prev);
            });
        }
        let _ = events & EVENT_COLOR_PICKED;
        let kind_name = s.kind.clone().unwrap_or_else(|| s.tool.name().to_owned());
        self.put_str("Tool", s.tool.name(), |k, v| k.set_tool(v));
        self.put_str("Selection", s.kind.as_deref().unwrap_or(""), |k, v| {
            k.set_selection(v)
        });
        self.put_str("Panel", panel_for(&kind_name), |k, v| k.set_panel(v));
        self.put_str("AmountLabel", amount_label(&kind_name), |k, v| {
            k.set_amount_label(v)
        });
        if self.last_tool != Some(s.tool) || self.last_kind != s.kind {
            self.last_tool = Some(s.tool);
            self.last_kind = s.kind.clone();
            if let Some(sink) = &self.sink {
                let _ = sink.clear_variants();
                for v in &s.variants {
                    let _ = sink.add_variants(v);
                }
            }
            self.shown.strings.remove("Variant");
            self.publish_quick_styles(&kind_name);
        }
        let st = &s.style;
        self.put_str("StrokeColor", &st.stroke.to_hex(), |k, v| {
            k.set_stroke_color(v)
        });
        self.put_str("FillColor", &st.fill.to_hex(), |k, v| k.set_fill_color(v));
        self.put_str("TextColor", &st.text_color.to_hex(), |k, v| {
            k.set_text_color(v)
        });
        self.put_num("StrokeWidth", st.width as f64, |k, v| k.set_stroke_width(v));
        self.put_num("FontSize", st.font_size.round() as f64, |k, v| {
            k.set_font_size(v)
        });
        self.put_str("FontFamily", &st.font, |k, v| k.set_font_family(v));
        self.put_flag("Bold", st.bold, |k, v| k.set_bold(v));
        self.put_flag("Italic", st.italic, |k, v| k.set_italic(v));
        self.put_flag("Shadow", st.shadow, |k, v| k.set_shadow(v));
        self.put_num("Opacity", opacity_percent(st), |k, v| k.set_opacity(v));
        self.put_str("Align", align_name(st.align), |k, v| k.set_align(v));
        self.put_str("Dash", dash_name(st.dash), |k, v| k.set_dash(v));
        self.put_str("StartCap", cap_name(st.start_cap), |k, v| {
            k.set_start_cap(v)
        });
        self.put_str("EndCap", cap_name(st.end_cap), |k, v| k.set_end_cap(v));
        self.put_str("Text", &s.text, |k, v| k.set_text(v));
        self.put_str("Variant", &s.variant, |k, v| k.set_variant(v));
        self.put_num("Amount", (s.amount * 100.0).round() / 100.0, |k, v| {
            k.set_amount(v)
        });
        self.put_num("Rotation", s.rotation.round(), |k, v| k.set_rotation(v));
        let info = format!("{} × {} px · {} objects", s.size.0, s.size.1, s.objects);
        self.put_str("DocumentInfo", &info, |k, v| k.set_document_info(v));
        let name = self
            .path
            .as_ref()
            .or(self.source.as_ref())
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".to_owned());
        let title = format!(
            "{}{name} — Jiffshot Editor",
            if s.modified { "• " } else { "" }
        );
        self.put_str("Title", &title, |k, v| k.set_title(v));
        self.mark_unsaved();
        let status = self.status.clone();
        self.put_str("Status", &status, |k, v| k.set_status(v));
        let has_sel = s.kind.is_some();
        self.put_flag("UndoEnabled", s.undo.is_some(), |k, v| {
            k.set_undo_enabled(v)
        });
        self.put_flag("RedoEnabled", s.redo.is_some(), |k, v| {
            k.set_redo_enabled(v)
        });
        for (name, set) in [
            (
                "DeleteEnabled",
                MainViewModelSink::set_delete_enabled as fn(&MainViewModelSink, bool) -> Result<()>,
            ),
            ("DuplicateEnabled", MainViewModelSink::set_duplicate_enabled),
            ("CutEnabled", MainViewModelSink::set_cut_enabled),
            (
                "BringToFrontEnabled",
                MainViewModelSink::set_bring_to_front_enabled,
            ),
            (
                "BringForwardEnabled",
                MainViewModelSink::set_bring_forward_enabled,
            ),
            (
                "SendBackwardEnabled",
                MainViewModelSink::set_send_backward_enabled,
            ),
            (
                "SendToBackEnabled",
                MainViewModelSink::set_send_to_back_enabled,
            ),
        ] {
            self.put_flag(name, has_sel, set);
        }
        self.put_str("BackgroundFill", s.background_fill, |k, v| {
            k.set_background_fill(v)
        });
        let theme = self
            .settings
            .theme
            .clone()
            .unwrap_or_else(|| "Dark".to_owned());
        self.put_str("Theme", &theme, |k, v| k.set_theme(v));
        let tools = self.settings.toolbar.join(",");
        self.put_str("ToolOrder", &tools, |k, v| k.set_tool_order(v));
        self.publish_layout();
        let tp = self.settings.template.clone();
        self.put_str("TemplateTheme", combine::theme(&tp.theme).name, |k, v| {
            k.set_template_theme(v)
        });
        self.put_flag("TemplateCaptions", tp.captions, |k, v| {
            k.set_template_captions(v)
        });
        self.put_flag("TemplateNumbers", tp.numbers, |k, v| {
            k.set_template_numbers(v)
        });
        self.put_str("TemplateFooter", &tp.footer, |k, v| {
            k.set_template_footer(v)
        });
        let fit = with_editor(|e| e.selected().and_then(|o| o.zone).is_some_and(|z| z.fit))
            .unwrap_or(false);
        self.put_flag("ZoneFit", fit, |k, v| k.set_zone_fit(v));
        let vp = self.settings.video.clone();
        self.put_str("GifPreset", &vp.gif_preset, |k, v| k.set_gif_preset(v));
        self.put_flag("GifLoop", vp.gif_loop, |k, v| k.set_gif_loop(v));
        self.put_flag("GifFade", vp.gif_fade, |k, v| k.set_gif_fade(v));
        self.put_flag("GifDither", vp.gif_dither, |k, v| k.set_gif_dither(v));
        self.put_num("VideoVolume", vp.volume, |k, v| k.set_video_volume(v));
        self.put_flag("Muted", vp.muted, |k, v| k.set_muted(v));
        self.put_num("SelectionCount", s.selected_count as f64, |k, v| {
            k.set_selection_count(v as i64)
        });
        if s.selected_count > 1 {
            let note = format!("{} objects selected", s.selected_count);
            self.put_str("Selection", &note, |k, v| k.set_selection(v));
        }
        self.put_num("HeadSize", st.head_size as f64, |k, v| k.set_head_size(v));
        self.put_num("ShadowAngle", st.shadow_angle as f64, |k, v| {
            k.set_shadow_angle(v)
        });
        self.put_num("ShadowDistance", st.shadow_distance as f64, |k, v| {
            k.set_shadow_distance(v)
        });
        self.put_num("ShadowBlur", st.shadow_blur as f64, |k, v| {
            k.set_shadow_blur(v)
        });
        self.put_str("ShadowColor", &st.shadow_color.to_hex(), |k, v| {
            k.set_shadow_color(v)
        });
        self.put_str("OutlineColor", &st.outline.to_hex(), |k, v| {
            k.set_outline_color(v)
        });
        self.put_num("OutlineWidth", st.outline_width as f64, |k, v| {
            k.set_outline_width(v)
        });
        self.put_flag("Underline", st.underline, |k, v| k.set_underline(v));
        self.put_flag("Strike", st.strike, |k, v| k.set_strike(v));
        let valign = match st.valign {
            jiffshot_doc::object::VAlign::Top => "Top",
            jiffshot_doc::object::VAlign::Middle => "Middle",
            jiffshot_doc::object::VAlign::Bottom => "Bottom",
        };
        self.put_str("VAlign", valign, |k, v| k.set_v_align(v));
        self.put_num("Padding", st.padding as f64, |k, v| k.set_padding(v));
        self.put_str("Fill2", &st.fill2.to_hex(), |k, v| k.set_fill2(v));
        self.put_flag("GradientRadial", st.gradient_radial, |k, v| {
            k.set_gradient_radial(v)
        });
        self.put_num("GradientAngle", st.gradient_angle as f64, |k, v| {
            k.set_gradient_angle(v)
        });
        let video = self.video.is_some();
        self.put_str("Mode", if video { "Video" } else { "Image" }, |k, v| {
            k.set_mode(v)
        });
        if events & ed::EVENT_FOCUS_TEXT != 0 {
            self.focus += 1;
            let focus = self.focus;
            if let Some(sink) = &self.sink {
                let _ = sink.set_focus_text(focus);
            }
        }
        if s.damage {
            self.bump_frame();
        }
    }

    /// Tells the canvas to fetch pending damage.
    pub fn bump_frame(&mut self) {
        self.frame += 1;
        if let Some(sink) = &self.sink {
            let _ = sink.set_frame_id(self.frame);
        }
    }

    pub(crate) fn zoom(&mut self, what: &str) {
        self.zoom += 1;
        let request = format!("{what}:{}", self.zoom);
        if let Some(sink) = &self.sink {
            let _ = sink.set_zoom_request(request);
        }
    }

    // ---- documents --------------------------------------------------------

    /// Saves edits of library captures automatically, next to the capture.
    pub fn autosave(&mut self) {
        let _ = self.autosave_checked();
    }

    fn autosave_checked(&mut self) -> std::result::Result<(), String> {
        let modified = with_editor(|e| e.is_modified())
            .ok_or("The editor is unavailable; unsaved edits cannot be checked.")?;
        if !modified {
            return Ok(());
        }
        let target = match (&self.path, &self.source) {
            (Some(p), _) => Some(p.clone()),
            (None, Some(src)) if src.starts_with(&self.library.root) => {
                Some(src.with_extension(EDITS_EXT))
            }
            _ => None,
        };
        let Some(path) = target else {
            return Err("Save the current document before quitting.".into());
        };
        if !self.save_to(&path) {
            return Err(format!(
                "Could not save edits to {}. {}",
                path.display(),
                self.status
            ));
        }
        Ok(())
    }

    fn save_to(&mut self, path: &Path) -> bool {
        let doc = with_editor(|e| e.document().clone());
        let Some(doc) = doc else { return false };
        let is_doc = io::is_document(path);
        let result = if is_doc {
            io::save_document(&doc, path)
        } else {
            io::export(&doc, path)
        };
        match result {
            Ok(()) => {
                if is_doc {
                    self.path = Some(path.to_path_buf());
                    with_editor(|e| e.mark_saved());
                    library::write_thumbnail(&doc, self.source.as_deref().unwrap_or(path));
                    #[cfg(target_os = "macos")]
                    if self.sink.is_some() {
                        self.refresh_library();
                    }
                    #[cfg(not(target_os = "macos"))]
                    self.refresh_library();
                }
                #[cfg(target_os = "macos")]
                if !is_doc {
                    self.status_with_font_substitutions(
                        format!("Saved {}", path.display()),
                        &doc.font_substitutions(),
                    );
                } else {
                    self.set_status(format!("Saved {}", path.display()));
                }
                #[cfg(not(target_os = "macos"))]
                self.set_status(format!("Saved {}", path.display()));
                true
            }
            Err(error) => {
                self.set_status(format!("Could not save {}: {error}", path.display()));
                false
            }
        }
    }

    pub(crate) fn load(&mut self, doc: Document, path: Option<PathBuf>, source: Option<PathBuf>) {
        self.autosave();
        self.stop_video();
        let size = doc.size();
        #[cfg(target_os = "macos")]
        let substitutions = doc.font_substitutions();
        with_editor(|e| e.load(doc));
        self.path = path;
        self.source = source;
        self.zoom("fit");
        self.bump_frame();
        self.mark_current();
        #[cfg(target_os = "macos")]
        self.status_with_font_substitutions(format!("{} × {} px", size.0, size.1), &substitutions);
        #[cfg(not(target_os = "macos"))]
        self.set_status(format!("{} × {} px", size.0, size.1));
    }

    pub(crate) fn load_narration_slide(&mut self, doc: Document) {
        self.autosave();
        with_editor(|e| e.load(doc));
        self.path = None;
        self.source = None;
        self.zoom("fit");
        self.bump_frame();
        self.mark_current();
    }

    /// The Share list: the built-in destinations, the user's programs, and
    /// Add program.
    pub(crate) fn publish_shares(&mut self) {
        let Some(sink) = &self.sink else { return };
        let _ = sink.clear_shares();
        for n in crate::share::BUILT_IN {
            let _ = sink.add_shares(*n);
        }
        for (n, _) in &self.settings.programs {
            let _ = sink.add_shares(n);
        }
        let _ = sink.add_shares("Add program…");
    }

    /// Puts `path` first in File ▸ Open recent.
    fn remember_recent(&mut self, path: &Path) {
        let recent = &mut self.settings.recent;
        recent.retain(|p| p != path);
        recent.insert(0, path.to_path_buf());
        recent.truncate(10);
        self.settings.save();
        self.publish_recent();
    }

    pub(crate) fn publish_recent(&mut self) {
        let Some(sink) = &self.sink else { return };
        let _ = sink.clear_recent_files();
        for path in self.settings.recent.iter().filter(|p| p.exists()) {
            let _ = sink.add_recent_files(file_uri(path));
        }
    }

    pub fn open_recent(&mut self, uri: &str) {
        let path = match path_of_uri(uri) {
            Ok(path) => path,
            Err(error) => return self.set_status(format!("Could not open recent file: {error}")),
        };
        if path.exists() {
            self.open_path(&path);
        } else {
            self.settings.recent.retain(|p| *p != path);
            self.settings.save();
            self.publish_recent();
            self.set_status(format!("{} is gone", path.display()));
        }
    }

    /// Image ▸ Auto trim: removes edges of one solid color.
    pub fn auto_trim(&mut self) {
        let Some(r) = with_editor(|e| trim_bounds(e.document().background())) else {
            return;
        };
        match r {
            Some(r) => self.document_op("Auto trim", |d| d.crop(r)),
            None => self.set_status("No solid-colored edges to trim"),
        }
    }

    /// Resize Image: to `w` x `h` pixels.
    pub fn resize_to(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 || w > 16384 || h > 16384 {
            return self.set_status("Sizes go from 1 to 16384 pixels");
        }
        self.document_op("Resize", |d| {
            if d.size() == (w, h) {
                return false;
            }
            d.scale(w, h);
            true
        });
    }

    /// Trim: removes this many pixels from each side.
    pub fn trim(&mut self, left: u32, top: u32, right: u32, bottom: u32) {
        let (w, h) = with_editor(|e| e.document().size()).unwrap_or_default();
        if left + right >= w || top + bottom >= h {
            return self.set_status("That would trim away the whole image");
        }
        let r = jiffshot_doc::IRect::new(
            left as i32,
            top as i32,
            (w - left - right) as i32,
            (h - top - bottom) as i32,
        );
        self.document_op("Trim", |d| d.crop(r));
    }

    pub fn open_path(&mut self, path: &Path) {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        if ext == "mp4" {
            return self.open_video(path);
        }
        // A library capture with saved edits opens the edits.
        let is_doc = io::is_document(path);
        let (file, doc_path) = match (!is_doc).then(|| edits_of(path)).flatten() {
            Some(edits) => (edits.clone(), Some(edits)),
            None => (path.to_path_buf(), is_doc.then(|| path.to_path_buf())),
        };
        let started = std::time::Instant::now();
        match io::open(&file) {
            Ok(mut doc) => {
                if doc_path.is_none() {
                    add_cursor_object(&mut doc, path);
                }
                #[cfg(target_os = "macos")]
                let font_notice =
                    jiffshot_doc::text::describe_font_substitutions(&doc.font_substitutions());
                let source = if is_doc {
                    None
                } else {
                    Some(path.to_path_buf())
                };
                self.load(doc, doc_path, source);
                self.remember_recent(path);
                let ms = started.elapsed().as_secs_f64() * 1000.0;
                let (w, h) = with_editor(|e| e.document().size()).unwrap_or_default();
                let status = format!("Opened {} ({w} × {h}) in {ms:.0} ms", file.display());
                #[cfg(target_os = "macos")]
                let status =
                    font_notice.map_or(status.clone(), |notice| format!("{notice}; {status}"));
                self.set_status(status);
            }
            Err(error) => self.set_status(format!("Could not open {}: {error}", file.display())),
        }
    }

    fn window(&self) -> Option<rustolonia::Window> {
        self.scope.main_window()
    }

    /// Runs a file picker and hands the chosen paths to `then` on the UI
    /// thread.
    fn pick_open(
        &mut self,
        title: &str,
        multiple: bool,
        images_only: bool,
        then: impl FnOnce(&mut App, Vec<PathBuf>) + Send + 'static,
    ) {
        let Some(window) = self.window() else { return };
        let mut filter = FileTypeFilter::new(if images_only {
            "Images"
        } else {
            "Jiffshot and image files"
        });
        let exts: &[&str] = if images_only {
            &["png", "jpg", "jpeg", "bmp", "gif", "webp"]
        } else {
            io::OPEN_EXTENSIONS
        };
        for e in exts.iter().chain((!images_only).then_some(&"mp4")) {
            filter = filter.with_extension(e);
        }
        let options = OpenFilePickerOptions::new()
            .title(title)
            .allow_multiple(multiple)
            .start_in(WellKnownFolder::Pictures)
            .file_type(filter);
        let op = match self.scope.open_file_picker(&window, &options) {
            Ok(op) => op,
            Err(error) => return self.set_status(format!("File picker failed: {error}")),
        };
        let _ = self.scope.spawn(async move {
            if let Ok(PickerOutcome::Selected(items)) = op.await {
                let paths: Vec<PathBuf> = items
                    .iter()
                    .filter_map(|i| i.local_path().map(Path::to_path_buf))
                    .collect();
                post(move |app| then(app, paths));
            }
        });
    }

    pub(crate) fn pick_save(
        &mut self,
        title: &str,
        name: &str,
        kinds: &[(&str, &str)],
        then: impl FnOnce(&mut App, PathBuf) + Send + 'static,
    ) {
        let Some(window) = self.window() else { return };
        let mut options = SaveFilePickerOptions::new()
            .title(title)
            .suggested_file_name(name)
            .show_overwrite_prompt(true)
            .start_in(WellKnownFolder::Pictures);
        for (label, ext) in kinds {
            options = options.file_type(FileTypeFilter::new(*label).with_extension(ext));
        }
        if let Some((_, ext)) = kinds.first() {
            options = options.default_extension(*ext);
        }
        let op = match self.scope.save_file_picker(&window, &options) {
            Ok(op) => op,
            Err(error) => return self.set_status(format!("File picker failed: {error}")),
        };
        let _ = self.scope.spawn(async move {
            if let Ok(PickerOutcome::Selected(items)) = op.await {
                if let Some(path) = items
                    .first()
                    .and_then(|i| i.local_path().map(Path::to_path_buf))
                {
                    post(move |app| then(app, path));
                }
            }
        });
    }

    fn stem(&self) -> String {
        self.path
            .as_ref()
            .or(self.source.as_ref())
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into())
    }

    // ---- commands ----------------------------------------------------------

    pub fn new_image(&mut self) {
        self.load(Document::blank(1280, 720, Rgba::WHITE), None, None);
    }

    pub fn open(&mut self) {
        self.pick_open("Open", false, false, |app, paths| {
            if let Some(p) = paths.first() {
                app.open_path(p);
            }
        });
    }

    pub fn save(&mut self) {
        let target = match (&self.path, &self.source) {
            (Some(p), _) => Some(p.clone()),
            (None, Some(src)) if src.starts_with(&self.library.root) => {
                Some(src.with_extension(EDITS_EXT))
            }
            _ => None,
        };
        match target {
            Some(path) => {
                self.save_to(&path);
            }
            None => self.save_as(),
        }
    }

    pub fn save_as(&mut self) {
        let name = format!("{}.{EDITS_EXT}", self.stem());
        let kinds = [
            ("Jiffshot document", EDITS_EXT),
            ("PNG image", "png"),
            ("JPEG image", "jpg"),
            ("BMP image", "bmp"),
            ("GIF image", "gif"),
            ("WebP image", "webp"),
        ];
        self.pick_save("Save as", &name, &kinds, |app, path| {
            app.save_to(&path);
        });
    }

    pub fn export(&mut self) {
        let mut kinds = vec![
            ("PNG image", "png"),
            ("JPEG image", "jpg"),
            ("PDF document", "pdf"),
            ("BMP image", "bmp"),
            ("GIF image", "gif"),
            ("WebP image", "webp"),
        ];
        // The format used last comes first.
        if let Some(i) = self
            .settings
            .export_format
            .as_deref()
            .and_then(|f| kinds.iter().position(|k| k.1 == f))
        {
            let k = kinds.remove(i);
            kinds.insert(0, k);
        }
        let name = format!("{}.{}", self.stem(), kinds[0].1);
        self.pick_save("Export image", &name, &kinds, |app, path| {
            let doc = with_editor(|e| e.document().clone());
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .unwrap_or_default();
            if let Some(doc) = doc {
                match io::export(&doc, &path) {
                    Ok(()) => {
                        app.settings.export_format = Some(ext);
                        app.settings.save();
                        #[cfg(target_os = "macos")]
                        app.status_with_font_substitutions(
                            format!("Exported {}", path.display()),
                            &doc.font_substitutions(),
                        );
                        #[cfg(not(target_os = "macos"))]
                        app.set_status(format!("Exported {}", path.display()));
                    }
                    Err(error) => app.set_status(format!("Export failed: {error}")),
                }
            }
        });
    }

    pub fn copy_image(&mut self) {
        self.copy_image_with(jiffshot_capture::clipboard::copy);
    }

    pub(crate) fn copy_image_with(
        &mut self,
        copy: impl FnOnce(&jiffshot_capture::Frame, isize) -> std::io::Result<()>,
    ) {
        #[cfg(target_os = "macos")]
        let Some((flat, substitutions)) =
            with_editor(|e| (e.document().flatten(), e.document().font_substitutions()))
        else {
            return;
        };
        #[cfg(not(target_os = "macos"))]
        let Some(flat) = with_editor(|e| e.document().flatten()) else {
            return;
        };
        let frame = winutil::frame_from_pixmap(&flat);
        match copy(&frame, winutil::main_hwnd()) {
            #[cfg(target_os = "macos")]
            Ok(()) => self.status_with_font_substitutions(
                format!(
                    "Copied the image ({} × {}) to the clipboard",
                    flat.width(),
                    flat.height()
                ),
                &substitutions,
            ),
            #[cfg(not(target_os = "macos"))]
            Ok(()) => self.set_status(format!(
                "Copied the image ({} × {}) to the clipboard",
                flat.width(),
                flat.height()
            )),
            Err(error) => self.set_status(format!("Copy failed: {error}")),
        }
    }

    pub fn copy(&mut self) {
        match with_editor(|e| e.selected().cloned()).flatten() {
            Some(o) => {
                self.set_status(format!("Copied the {}", o.kind_name().to_lowercase()));
                self.object_clipboard = Some(o);
            }
            None => self.copy_image(),
        }
    }

    pub fn cut(&mut self) {
        self.copy();
        with_editor(|e| e.delete_selected());
    }

    pub fn paste(&mut self) {
        self.paste_from_clipboard(jiffshot_capture::clipboard::read(winutil::main_hwnd()));
    }

    pub(crate) fn paste_from_clipboard(
        &mut self,
        contents: std::io::Result<Option<jiffshot_capture::clipboard::Pasted>>,
    ) {
        use jiffshot_capture::clipboard::Pasted;
        match contents {
            Ok(Some(Pasted::Image {
                width,
                height,
                rgba,
            })) => {
                let img = image::RgbaImage::from_raw(width, height, rgba)
                    .and_then(|i| io::from_rgba_image(&i));
                if let Some(pm) = img {
                    with_editor(|e| e.paste_image(pm));
                    self.object_clipboard = None;
                    self.set_status(format!("Pasted a {width} × {height} image"));
                } else {
                    #[cfg(target_os = "macos")]
                    self.set_status(
                        "Paste failed: the clipboard image has invalid dimensions or pixels",
                    );
                }
            }
            Ok(Some(Pasted::Files(files))) => {
                let mut n = 0;
                #[cfg(target_os = "macos")]
                let mut errors = Vec::new();
                for f in files {
                    match io::load_image(&f) {
                        Ok(pm) => {
                            with_editor(|e| e.paste_image(pm));
                            n += 1;
                        }
                        Err(_error) => {
                            #[cfg(target_os = "macos")]
                            errors.push(format!("{}: {_error}", f.display()));
                        }
                    }
                }
                #[cfg(target_os = "macos")]
                if !errors.is_empty() {
                    self.set_status(format!(
                        "Pasted {n} image file(s); could not open {}",
                        errors.join("; ")
                    ));
                } else {
                    self.set_status(format!("Pasted {n} image file(s)"));
                }
                #[cfg(windows)]
                self.set_status(format!("Pasted {n} image file(s)"));
            }
            Ok(Some(Pasted::Text(text))) if self.object_clipboard.is_none() => {
                with_editor(|e| {
                    let (w, h) = e.document().size();
                    let style = Style {
                        ..e.tool_style(Tool::Text)
                    };
                    let rect = jiffshot_doc::Box2::from_xywh(
                        w as f32 * 0.1,
                        h as f32 * 0.1,
                        w as f32 * 0.5,
                        style.font_size * 2.0,
                    );
                    let id = e.add_object(
                        "Paste text",
                        Kind::Text {
                            rect,
                            text: String::new(),
                        },
                        style,
                    );
                    let _ = id;
                    e.set_selected_text(&text);
                });
            }
            #[cfg(target_os = "macos")]
            Err(error) => self.set_status(format!("Paste failed: {error}")),
            _ => match self.object_clipboard.clone() {
                Some(o) => {
                    with_editor(|e| e.paste_object(o));
                }
                None => self.set_status("Nothing to paste"),
            },
        }
    }

    pub fn new_capture(&mut self) {
        match winutil::start_capture() {
            Ok(()) => self.set_status("Select what to capture…"),
            Err(error) => self.set_status(error),
        }
    }

    /// Opens the Capture window; the editor stays open.
    pub fn open_capture_window(&mut self) {
        if let Err(error) = winutil::open_capture_window() {
            self.set_status(error);
        }
    }

    pub fn combine_images(&mut self) {
        self.template("Combine grid");
    }

    /// Images (or a capture to open) dropped at a canvas point:
    /// "x,y|path|path…". Several images fill the next empty drop zones.
    fn drop_files(&mut self, arg: &str) {
        self.canvas_drop = Some(std::time::Instant::now());
        let mut parts = arg.split('|');
        let at = parts
            .next()
            .and_then(|xy| xy.split_once(','))
            .and_then(|(x, y)| Some(jiffshot_doc::Pt::new(x.parse().ok()?, y.parse().ok()?)));
        let paths: Vec<PathBuf> = parts.filter(|p| !p.is_empty()).map(PathBuf::from).collect();
        let Some(at) = at else { return };
        let images: Vec<_> = paths
            .iter()
            .filter_map(|p| io::load_image(p).ok())
            .collect();
        if images.is_empty() {
            if let Some(first) = paths.first() {
                self.open_path(first);
            }
            return;
        }
        let n = images.len();
        with_editor(|e| {
            let mut images = images.into_iter();
            if let Some(first) = images.next() {
                e.drop_image(at, first);
            }
            for img in images {
                let empty = e.document().objects.iter().find(|o| o.zone.is_some() && matches!(&o.kind, jiffshot_doc::object::Kind::Image { image, .. } if image.pixmap.is_none())).map(|o| o.id);
                match empty {
                    Some(id) => e.fill_zone(id, img),
                    None => {
                        e.drop_image(jiffshot_doc::Pt::new(at.x + 24.0, at.y + 24.0), img);
                    }
                }
            }
        });
        self.set_status(format!("Placed {n} image{}", if n == 1 { "" } else { "s" }));
    }

    /// Chooses a picture for the selected drop zone.
    fn pick_zone_image(&mut self) {
        let zone =
            with_editor(|e| e.selected().filter(|o| o.zone.is_some()).map(|o| o.id)).flatten();
        let Some(id) = zone else { return };
        self.pick_open(
            "Choose an image for the drop zone",
            false,
            true,
            move |app, paths| {
                if let Some(img) = paths.first().and_then(|p| io::load_image(p).ok()) {
                    with_editor(|e| e.fill_zone(id, img));
                    app.bump_frame();
                }
            },
        );
    }

    fn template_options(&self) -> combine::Options {
        let t = &self.settings.template;
        combine::Options {
            theme: combine::theme(&t.theme),
            captions: t.captions,
            numbers: t.numbers,
            footer: Some(t.footer.clone()).filter(|f| !f.trim().is_empty()),
        }
    }

    /// Changes a template choice; the next template uses it.
    pub fn template_prefs(&mut self, change: impl FnOnce(&mut crate::settings::TemplatePrefs)) {
        change(&mut self.settings.template);
        self.settings.save();
        schedule_sync();
    }

    fn template(&mut self, name: &str) {
        let name = name.to_owned();
        // Captures picked in the library go in; otherwise templates start
        // with empty drop zones and combining asks for images.
        let picked: Vec<_> = self
            .library_items
            .iter()
            .filter(|i| self.library_selected.contains(&i.path))
            .filter_map(|i| io::load_image(&i.path).ok())
            .collect();
        if !name.starts_with("Combine") && (!picked.is_empty() || self.library_selected.is_empty())
        {
            if let Some(doc) = combine::template_with(&name, &picked, &self.template_options()) {
                self.load(doc, None, None);
                with_editor(|e| e.checkpoint("Template", None));
                let browser = if cfg!(target_os = "macos") {
                    "Finder"
                } else {
                    "Explorer"
                };
                self.set_status(format!(
                    "{name}: drop images from the tray or {browser} onto the drop zones"
                ));
            }
            return;
        }
        if name.starts_with("Combine") && picked.len() >= 2 {
            let arrangement = match name.as_str() {
                "Combine side by side" => combine::Arrangement::Horizontal,
                "Combine stacked" => combine::Arrangement::Vertical,
                _ => combine::Arrangement::Grid,
            };
            if let Some(doc) = combine::combine(&picked, arrangement, 24.0, Rgba::WHITE) {
                self.load(doc, None, None);
                with_editor(|e| e.checkpoint("Combine", None));
            }
            return;
        }
        let options = self.template_options();
        self.pick_open("Choose images", true, true, move |app, paths| {
            let images: Vec<_> = paths
                .iter()
                .filter_map(|p| io::load_image(p).ok())
                .collect();
            if images.is_empty() {
                return;
            }
            let doc = match name.as_str() {
                "Combine side by side" => {
                    combine::combine(&images, combine::Arrangement::Horizontal, 24.0, Rgba::WHITE)
                }
                "Combine stacked" => {
                    combine::combine(&images, combine::Arrangement::Vertical, 24.0, Rgba::WHITE)
                }
                "Combine grid" => {
                    combine::combine(&images, combine::Arrangement::Grid, 24.0, Rgba::WHITE)
                }
                other => combine::template_with(other, &images, &options),
            };
            if let Some(doc) = doc {
                app.load(doc, None, None);
                with_editor(|e| e.checkpoint("Template", None));
                app.set_status(format!("{name}: {} image(s)", images.len()));
            }
        });
    }

    pub fn open_library_folder(&mut self) {
        let _ = std::fs::create_dir_all(&self.library.root);
        winutil::open_folder(&self.library.root);
    }

    /// Ends Jiffshot: the editor's edits are saved, the capture side stops,
    /// then the application. The capture side's own Exit comes back here.
    pub fn exit(&mut self) {
        if std::mem::replace(&mut self.exiting, true) {
            return;
        }
        #[cfg(target_os = "macos")]
        {
            if let Err(error) = self.quit_ready() {
                self.exit_failed(error, false);
                return;
            }
            if let Err(error) = self.autosave_checked() {
                self.exit_failed(error, false);
                return;
            }
            if let Err(error) = jiffshot_capture::macos::suspend_file_panels_for_shutdown() {
                self.exit_failed(
                    format!("Could not dismiss open file panels: {error}"),
                    false,
                );
                return;
            }
            let mut playback = std::mem::take(&mut self.pending_playback_shutdown);
            if let Some(current) = self
                .video
                .take()
                .and_then(crate::video::Session::stop_for_shutdown)
            {
                playback.push(current);
            }
            self.set_status("Finishing capture and video before quitting…");
            let scope = self.scope.clone();
            let spawned = spawn_shutdown_worker(
                playback,
                |work| {
                    std::thread::Builder::new()
                        .name("jiffshot-shutdown".into())
                        .spawn(work)
                        .map(|_| ())
                },
                move |playback| {
                    let mut errors = Vec::new();
                    if let Err(error) = crate::capture_engine_macos::stop() {
                        errors.push(error);
                    }
                    for player in playback {
                        if let Err(error) = player.join() {
                            errors.push(error);
                        }
                    }
                    let result = if errors.is_empty() {
                        Ok(())
                    } else {
                        Err(errors.join("; "))
                    };
                    crate::shutdown_ui_macos::post_from_worker(&scope, move || {
                        finish_exit_when_ready(result)
                    });
                },
            );
            if let Err((error, playback)) = spawned {
                self.pending_playback_shutdown = playback;
                self.exit_failed(format!("Could not start shutdown: {error}"), false);
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.autosave();
            self.stop_video();
            jiffshot_workflow::stop_engine();
            if let Err(error) = crate::ui::shutdown(&self.scope) {
                self.exiting = false;
                eprintln!("could not shut down Jiffshot: {error}");
                self.toast("Could not quit Jiffshot", &error, true);
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn quit_ready(&self) -> std::result::Result<(), String> {
        if self.narration.is_some() || self.narration_request.is_some() {
            Err("Finish or cancel narration before quitting.".into())
        } else if crate::video::pending_work() {
            Err(
                "Finish or cancel the narration save and wait for video exports before quitting."
                    .into(),
            )
        } else if crate::file_drag_macos::has_pending()
            .map_err(|error| format!("Could not inspect library dragging: {error}"))?
        {
            Err("Finish or cancel the library drag before quitting.".into())
        } else {
            Ok(())
        }
    }

    #[cfg(target_os = "macos")]
    fn adopt_pending_termination(&mut self) -> std::result::Result<(), String> {
        let pending = crate::termination_macos::pending_token()
            .map_err(|error| format!("Could not inspect macOS termination: {error}"))?;
        adopt_pending_termination(&mut self.native_termination, pending)
    }

    #[cfg(target_os = "macos")]
    fn finish_exit(&mut self, result: std::result::Result<(), String>) {
        let result = result
            .and_then(|()| self.adopt_pending_termination())
            .and_then(|()| self.quit_ready())
            .and_then(|()| self.autosave_checked());
        if let Err(error) = result {
            self.exit_failed(error, true);
            return;
        }
        if self.native_termination.is_some() {
            if let Err(error) = crate::ui::selftest_before_native_termination() {
                self.exit_failed(error, true);
                return;
            }
        }
        if let Err(error) = crate::file_drag_macos::suspend_for_shutdown() {
            self.exit_failed(format!("Could not stop library dragging: {error}"), true);
            return;
        }
        if let Some(token) = self.native_termination {
            if let Err(error) =
                crate::shutdown_ui_macos::post("native termination acceptance", move || {
                    if let Err(error) = crate::termination_macos::complete(token, true) {
                        native_completion_failed(token, error.to_string());
                    }
                })
            {
                self.exit_failed(
                    format!("Could not complete macOS termination: {error}"),
                    true,
                );
            }
        } else if let Err(error) = crate::ui::shutdown(&self.scope) {
            self.exit_failed(error, true);
        }
    }

    #[cfg(target_os = "macos")]
    fn exit_failed(&mut self, error: String, engine_stopped: bool) {
        let error = match crate::file_drag_macos::resume_after_failed_shutdown() {
            Ok(()) => error,
            Err(resume) => format!("{error}; could not restore library dragging: {resume}"),
        };
        let error = match jiffshot_capture::macos::resume_file_panels_after_failed_shutdown() {
            Ok(()) => error,
            Err(resume) => format!("{error}; could not restore file panels: {resume}"),
        };
        let (error, can_cancel) = match self.adopt_pending_termination() {
            Ok(()) => (error, true),
            Err(coordination) if coordination == error => (error, false),
            Err(coordination) => (format!("{error}; {coordination}"), false),
        };
        let message = if engine_stopped {
            format!("{error}. If the capture side has stopped, restart Jiffshot before capturing again.")
        } else {
            error
        };
        jiffshot_workflow::log(&format!("could not shut down Jiffshot: {message}"));
        self.toast("Could not quit Jiffshot", &message, true);
        if let Err(error) = crate::capture_engine_macos::acknowledge_failure() {
            jiffshot_workflow::log(&format!(
                "could not acknowledge the capture-engine failure: {error}"
            ));
        }
        if let Some(token) = self.native_termination.filter(|_| can_cancel) {
            let original = message.clone();
            if let Err(error) = crate::shutdown_ui_macos::post(
                "native termination cancellation",
                move || {
                    match crate::termination_macos::complete(token, false) {
                    Ok(()) => native_cancelled(token),
                    Err(error) => native_cancellation_failed(token, format!("{original} Could not cancel macOS termination: {error}. Retry Quit after addressing the problem.")),
                }
                },
            ) {
                let message = format!("{message} Could not queue cancellation of macOS termination: {error}. Retry Quit after addressing the problem.");
                jiffshot_workflow::log(&message);
                self.exiting = false;
                self.toast("Could not cancel macOS termination", &message, true);
            }
        } else {
            self.exiting = false;
        }
    }

    /// The editor window was closed: Jiffshot keeps running in the tray.
    pub fn hidden(&mut self) {
        self.autosave();
        self.stop_video();
    }

    /// Shows a notice from the capture side as a toast (the `toast` signal).
    pub fn toast(&mut self, title: &str, text: &str, problem: bool) {
        self.toasts += 1;
        let value = format!(
            "{}\u{1f}{title}\u{1f}{text}\u{1f}{}",
            self.toasts, problem as u8
        );
        self.put_str("Toast", &value, |k, v| k.set_toast(v));
    }

    pub fn show_capture_window(&mut self) {
        crate::capture_window::show(&self.scope);
    }

    /// Shows Grab Text's result in its window (the `grab_text` signal).
    pub fn show_grab_text(&mut self, text: &str) {
        self.toasts += 1;
        let value = format!("{}\u{1f}{text}", self.toasts);
        self.put_str("GrabText", &value, |k, v| k.set_grab_text(v));
    }

    /// The selection overlay to show, or empty (the `overlay` signal).
    pub fn set_overlay(&mut self, json: &str) {
        self.put_str("Overlay", json, |k, v| k.set_overlay(v));
    }

    /// Screen Draw to show, or empty (the `screen_draw` signal).
    pub fn set_screen_draw(&mut self, json: &str) {
        self.put_str("ScreenDraw", json, |k, v| k.set_screen_draw(v));
    }

    /// Opens the Preferences dialog on `tab` (the `prefs` signal).
    pub fn open_prefs(&mut self, tab: &str) {
        self.toasts += 1;
        let value = format!("{}\u{1f}{}", self.toasts, crate::prefs::dialog(self, tab));
        self.put_str("Prefs", &value, |k, v| k.set_prefs(v));
    }

    /// The capture side's panels and outlines (the `hud` signal).
    pub fn set_hud(&mut self, json: &str) {
        self.put_str("Hud", json, |k, v| k.set_hud(v));
    }

    /// The Capture Widget follows the capture side's setting.
    pub fn set_widget(&mut self, on: bool) {
        self.put_flag("Widget", on, |k, v| k.set_widget(v));
    }

    /// Shows the editor window and brings it forward.
    pub fn show_editor(&mut self) {
        if let Some(window) = self.scope.main_window() {
            let _ = window.show();
            #[cfg(windows)]
            if let Ok(handle) = self.scope.platform_handle(&window) {
                winutil::set_main_hwnd(handle);
            }
        }
        winutil::bring_to_front();
        self.refresh_library();
    }

    pub fn save_quick_style(&mut self) {
        let Some((tool, style)) = with_editor(|e| {
            (
                e.selected().map(ed::tool_for).unwrap_or(e.tool()),
                e.current_style(),
            )
        }) else {
            return;
        };
        self.settings.add_quick_style(tool.name(), style);
        self.settings.save();
        self.last_kind = Some("refresh".into());
        self.set_status(format!("Saved a quick style for {}", tool.name()));
    }

    fn publish_quick_styles(&mut self, kind: &str) {
        let _t = crate::trace::span("publish_quick_styles");
        let tool = Tool::from_name(kind).unwrap_or(Tool::Select);
        let styles = self.settings.quick_styles(tool);
        // Favorites first, each group in the list's order.
        let mut order: Vec<(usize, Style, bool)> = styles
            .into_iter()
            .enumerate()
            .map(|(i, s)| {
                let f = self.settings.is_favorite(tool, &s);
                (i, s, f)
            })
            .collect();
        order.sort_by_key(|(_, _, favorite)| !favorite);
        let Some(sink) = &self.sink else { return };
        let _ = sink.clear_quick_styles();
        for (index, style, favorite) in order {
            let _ = sink.add_quick_styles(QuickStyle {
                index,
                tool,
                style,
                favorite,
                sink: None,
            });
        }
    }

    /// "up:Tool", "down:Tool", "hide:Tool", "show:Tool" or "reset"; the
    /// toolbar keeps at least one tool. `all` is every tool in the usual
    /// order, as the window lists them.
    fn toolbar(&mut self, arg: &str) {
        const ALL: &[&str] = &[
            "Select",
            "Arrow",
            "Line",
            "Shape",
            "Text",
            "Callout",
            "Step",
            "Pen",
            "Highlight",
            "Blur",
            "Magnify",
            "Spotlight",
            "Stamp",
            "Fill",
            "Crop",
            "Cut Out",
            "Selection",
            "Magic Wand",
            "Eraser",
            "Eyedropper",
            "Smart Move",
            "Text Replace",
        ];
        let mut order: Vec<String> = if self.settings.toolbar.is_empty() {
            ALL.iter().map(|s| s.to_string()).collect()
        } else {
            self.settings.toolbar.clone()
        };
        let (verb, name) = arg.split_once(':').unwrap_or((arg, ""));
        let at = order.iter().position(|t| t == name);
        match (verb, at) {
            ("reset", _) => order.clear(),
            ("up", Some(i)) if i > 0 => order.swap(i - 1, i),
            ("down", Some(i)) if i + 1 < order.len() => order.swap(i, i + 1),
            ("hide", Some(i)) if order.len() > 1 => {
                order.remove(i);
                self.set_status(format!(
                    "{name} hidden; View ▸ Show all tools brings it back"
                ));
            }
            ("show", None) if ALL.contains(&name) => order.push(name.to_owned()),
            _ => return,
        }
        self.settings.toolbar = order;
        self.settings.save();
        let joined = self.settings.toolbar.join(",");
        self.put_str("ToolOrder", &joined, |k, v| k.set_tool_order(v));
    }

    /// Deletes, moves or (un)favorites the quick style at `index`.
    pub fn edit_quick_style(&mut self, tool: Tool, index: usize, style: &Style, what: &str) {
        match what {
            "favorite" => self.settings.toggle_favorite(tool, style),
            "delete" => self.settings.edit_styles(tool, |l| {
                if index < l.len() {
                    l.remove(index);
                }
            }),
            "earlier" if index > 0 => self.settings.edit_styles(tool, |l| {
                if index < l.len() {
                    l.swap(index - 1, index);
                }
            }),
            "later" => self.settings.edit_styles(tool, |l| {
                if index + 1 < l.len() {
                    l.swap(index, index + 1);
                }
            }),
            _ => return,
        }
        self.settings.save();
        self.publish_quick_styles(tool.name());
    }

    /// The current tool's (or selection's) quick styles from a theme, or
    /// back to the built-in ones.
    fn style_theme(&mut self, name: &str) {
        let Some(tool) = with_editor(|e| e.selected().map(ed::tool_for).unwrap_or(e.tool())) else {
            return;
        };
        if name == "reset" {
            self.settings.reset_styles(tool);
            self.set_status(format!("{} quick styles reset", tool.name()));
        } else {
            let t = combine::theme(name);
            let styles = crate::settings::Settings::themed_styles(
                tool,
                &[t.accent, t.ink, t.muted, t.frame],
            );
            self.settings.edit_styles(tool, |l| *l = styles);
            self.set_status(format!(
                "{} quick styles in the {} theme",
                tool.name(),
                t.name
            ));
        }
        self.settings.save();
        self.publish_quick_styles(tool.name());
    }

    pub fn apply_quick_style(&mut self, tool: Tool, style: Style) {
        with_editor(|e| {
            let font = e.current_style().font;
            if e.selected().is_none() && e.tool() != tool {
                e.set_tool(tool);
            }
            let apply = style.clone();
            e.update_style("quickstyle", move |s| {
                *s = Style {
                    font: font.clone(),
                    ..apply.clone()
                };
            });
            e.seal();
        });
    }

    pub fn document_op(&mut self, label: &str, op: impl FnOnce(&mut Document) -> bool) {
        let done = with_editor(|e| e.edit_document(label, op)).unwrap_or(false);
        if done {
            self.zoom("fit");
        }
    }

    pub fn flatten_selected(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some((true, substitutions)) = with_editor(|e| {
            let selected = e.selected_ids();
            let substitutions = jiffshot_doc::text::object_font_substitutions(
                e.document()
                    .objects
                    .iter()
                    .filter(|object| selected.contains(&object.id)),
            );
            (e.flatten_selected(), substitutions)
        }) {
            if !substitutions.is_empty() {
                self.status_with_font_substitutions(
                    "Flattened the selected objects",
                    &substitutions,
                );
            }
        }
        #[cfg(not(target_os = "macos"))]
        with_editor(|e| e.flatten_selected());
    }

    pub fn flatten_document(&mut self) {
        #[cfg(target_os = "macos")]
        let substitutions = with_editor(|e| e.document().font_substitutions()).unwrap_or_default();
        self.document_op("Flatten", |d| {
            let any = !d.objects.is_empty();
            d.flatten_objects();
            any
        });
        #[cfg(target_os = "macos")]
        if !substitutions.is_empty() {
            self.status_with_font_substitutions("Flattened the document", &substitutions);
        }
    }

    pub fn effect(&mut self, name: &str) {
        let style = with_editor(|e| e.current_style()).unwrap_or_default();
        if name == "Watermark" {
            return self.pick_open("Choose a watermark image", false, true, |app, paths| {
                if let Some(image) = paths.first().and_then(|p| io::load_image(p).ok()) {
                    let effect = effects::Effect::Watermark {
                        image: Arc::new(image),
                        opacity: 0.35,
                    };
                    app.document_op("Watermark", |d| effects::apply(d, effect));
                    app.set_status("Watermark added");
                }
            });
        }
        if name == "Spotlight & magnify" {
            with_editor(|e| e.spotlight_magnify(1.6));
            self.bump_frame();
            return self.set_status("Spotlight & magnify: the selected area (or the middle) is enlarged; draw a rectangle first to choose it.");
        }
        if name == "Capture info" {
            let text = self.capture_info();
            let effect = effects::Effect::CaptureInfo { text };
            self.document_op(name, |d| effects::apply(d, effect));
            return;
        }
        match effects::by_name_for(name, style.stroke, style.fill) {
            Some(effect) => {
                let started = std::time::Instant::now();
                self.document_op(name, |d| effects::apply(d, effect));
                self.set_status(format!(
                    "{name} applied in {:.0} ms",
                    started.elapsed().as_secs_f64() * 1000.0
                ));
            }
            None => self.set_status(format!("Unknown effect {name}")),
        }
    }

    /// When and where the capture was taken, from its sidecar file.
    fn capture_info(&self) -> String {
        let meta = self
            .source
            .as_ref()
            .and_then(|s| std::fs::read(s.with_extension("json")).ok())
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
        let mut parts = Vec::new();
        if let Some(m) = &meta {
            if let Some(t) = m["captured_at"].as_str() {
                parts.push(t.replace('T', " "));
            }
            if let Some(t) = m["window_title"].as_str() {
                parts.push(t.to_owned());
            }
            if let Some(p) = m["process"].as_str() {
                parts.push(p.to_owned());
            }
        }
        if parts.is_empty() {
            parts.push(jiffshot_capture::library::timestamp().replace('T', " "));
        }
        parts.push(
            if cfg!(target_os = "macos") {
                "macOS"
            } else {
                "Windows"
            }
            .to_owned(),
        );
        parts.join("  ·  ")
    }

    pub fn resize(&mut self, factor: f32) {
        self.document_op("Resize", |d| {
            let (w, h) = d.size();
            let (nw, nh) = (
                ((w as f32 * factor).round() as u32).max(1),
                ((h as f32 * factor).round() as u32).max(1),
            );
            if nw > 16384 || nh > 16384 {
                return false;
            }
            d.scale(nw, nh);
            true
        });
    }

    pub fn set_tool(&mut self, tool: Tool) {
        with_editor(|e| {
            e.select(None);
            e.set_tool(tool);
        });
        let hint = match tool {
            Tool::Crop => "Drag to crop the image.",
            Tool::CutOut => "Drag across the part to remove; the rest closes the gap.",
            Tool::Selection => "Drag to select part of the image, then move it.",
            Tool::SmartMove => "Drag around an object to lift it off the background.",
            Tool::TextReplace => "Drag around text to make it editable.",
            Tool::Fill => "Click an area to fill it.",
            Tool::Eyedropper => "Click to pick a color.",
            _ => "",
        };
        if !hint.is_empty() {
            self.set_status(hint);
        }
        #[cfg(target_os = "macos")]
        if let Some(notice) =
            with_editor(|e| font_notice_for_style(tool, &e.tool_style(tool))).flatten()
        {
            self.set_status(notice);
        }
    }

    /// `Run` command: actions from toolbar buttons and panel lists.
    pub fn run(&mut self, action: &str) {
        let (verb, arg) = action.split_once(':').unwrap_or((action, ""));
        match verb {
            "tool" => {
                if let Some(t) = Tool::from_name(arg) {
                    self.set_tool(t);
                }
            }
            "effect" => self.effect(arg),
            "template" => self.template(arg),
            "zoom" => self.zoom(arg),
            "open" => self.open_path(Path::new(arg)),
            "library" => self.refresh_library(),
            "trim" => self.video_action(arg),
            "finish" => {
                with_editor(|e| e.finish_polygon());
            }
            "add-stamp" => self.add_stamp(),
            "drop" => self.drop_files(arg),
            "style-theme" => self.style_theme(arg),
            "toolbar" => self.toolbar(arg),
            "zone-fit" => {
                with_editor(|e| e.set_zone_fit(arg == "1"));
            }
            "zone-swap" => {
                if !with_editor(|e| e.swap_zones()).unwrap_or(false) {
                    self.set_status("Shift+click two drop zones, then Swap.");
                }
            }
            "tail" => {
                if !with_editor(|e| e.callout_tail(arg != "remove")).unwrap_or(false) {
                    self.set_status("Select a callout first.");
                }
            }
            "zone-empty" => {
                with_editor(|e| e.empty_zone());
            }
            "zone-pick" => self.pick_zone_image(),
            "share" => self.share(arg),
            "hidden" => self.hidden(),
            // The Preferences dialog: open it on a tab; apply what it sends.
            "prefs" => self.open_prefs(if arg.is_empty() { "capture" } else { arg }),
            "prefs-apply" => crate::prefs::apply(self, arg),
            // A capture panel's button ("ID:INDEX"; no index when closed), or
            // a toolbar's action ("ID:act:NAME").
            "hud" => {
                let mut parts = arg.splitn(3, ':');
                if let Some(id) = parts.next().and_then(|id| id.parse().ok()) {
                    match (parts.next(), parts.next()) {
                        (Some("act"), Some(action)) => jiffshot_workflow::panel_action(id, action),
                        (index, _) => {
                            jiffshot_workflow::answer_panel(id, index.and_then(|i| i.parse().ok()))
                        }
                    }
                }
            }
            // The Capture Widget's buttons.
            "widget" => {
                let mode = match arg {
                    "video" => jiffshot_workflow::MODE_VIDEO,
                    "scrolling" => jiffshot_workflow::MODE_SCROLLING,
                    "text" => jiffshot_workflow::MODE_TEXT,
                    _ => jiffshot_workflow::MODE_IMAGE,
                };
                jiffshot_workflow::request_capture(mode, 1);
            }
            "resize" | "trim-image" => {
                let n: Vec<u32> = arg
                    .split(',')
                    .filter_map(|v| v.trim().parse().ok())
                    .collect();
                match (verb, n.as_slice()) {
                    ("resize", &[w, h]) => self.resize_to(w, h),
                    ("trim-image", &[l, t, r, b]) => self.trim(l, t, r, b),
                    _ => {}
                }
            }
            // Editor Preferences.
            "theme" if matches!(arg, "Dark" | "Light" | "System") => {
                self.settings.set_theme(arg);
            }
            "export-format" => {
                self.settings.export_format =
                    Some(arg.to_ascii_lowercase()).filter(|f| !f.is_empty());
                self.settings.save();
            }
            "ftp" => {
                #[cfg(target_os = "macos")]
                self.save_share_preferences(Some(arg), &[]);
                #[cfg(windows)]
                {
                    self.settings.ftp = Some(arg.trim().to_owned()).filter(|u| !u.is_empty());
                    self.settings.save();
                }
            }
            "program-remove" => {
                #[cfg(target_os = "macos")]
                self.save_share_preferences(None, &[arg.to_owned()]);
                #[cfg(windows)]
                {
                    self.settings.programs.retain(|(n, _)| n != arg);
                    self.settings.save();
                    self.publish_shares();
                }
            }
            "library-folder" if !arg.is_empty() => {
                let root = PathBuf::from(arg);
                self.settings.set_library(root.clone());
                self.library = Library::new(root);
                self.show_library();
                self.set_status(format!("Library: {arg}"));
            }
            "align" => {
                with_editor(|e| e.align_selected(arg));
            }
            "deselect" => {
                with_editor(|e| e.select(None));
            }
            "export-to" => {
                let doc = with_editor(|e| e.document().clone());
                if let Some(doc) = doc {
                    match io::export(&doc, Path::new(arg)) {
                        #[cfg(target_os = "macos")]
                        Ok(()) => self.status_with_font_substitutions(
                            format!("Exported {arg}"),
                            &doc.font_substitutions(),
                        ),
                        #[cfg(not(target_os = "macos"))]
                        Ok(()) => self.set_status(format!("Exported {arg}")),
                        Err(error) => self.set_status(format!("Export failed: {error}")),
                    }
                }
            }
            _ => self.set_status(format!("Unknown action {action}")),
        }
    }

    // ---- property edits from the panel ---------------------------------

    pub fn style_edit(&mut self, property: &'static str, change: impl Fn(&mut Style)) {
        with_editor(|e| e.update_style(property, change));
        #[cfg(target_os = "macos")]
        if matches!(property, "font" | "bold" | "italic") {
            if let Some((tool, style)) = with_editor(|e| {
                (
                    e.selected().map(ed::tool_for).unwrap_or_else(|| e.tool()),
                    e.current_style(),
                )
            }) {
                if let Some(notice) = font_notice_for_style(tool, &style) {
                    self.set_status(notice);
                } else if matches!(tool, Tool::Text | Tool::Callout | Tool::Step)
                    && self.status.starts_with("Font substitution: ")
                {
                    self.set_status("Font available");
                }
            }
        }
    }

    pub fn set_text(&mut self, value: &str) {
        with_editor(|e| e.set_selected_text(value));
    }

    pub fn set_variant(&mut self, value: &str) {
        if value.is_empty() {
            return;
        }
        with_editor(|e| e.set_variant(value));
    }

    pub fn set_amount(&mut self, value: f64) {
        with_editor(|e| {
            if e.selection().is_some() {
                e.set_selected_amount(value as f32);
            } else {
                let t = e.tool();
                e.set_tool_amount(t, value as f32);
            }
        });
    }

    pub fn set_rotation(&mut self, value: f64) {
        with_editor(|e| e.set_selected_rotation(value as f32));
    }

    pub fn document_expand(&mut self) {
        if with_editor(|e| e.expand_to_fit()).unwrap_or(false) {
            self.zoom("fit");
        } else {
            self.set_status("Every object already fits on the canvas");
        }
    }

    // ---- sharing -----------------------------------------------------------

    /// The flattened image as a temporary file named after the capture.
    fn temp_export(&mut self, ext: &str) -> std::result::Result<PathBuf, String> {
        let doc = with_editor(|e| e.document().clone()).ok_or("no document")?;
        #[cfg(target_os = "macos")]
        let path = crate::share::temporary_export_path(&self.stem(), ext)?;
        #[cfg(windows)]
        let path = {
            let dir = std::env::temp_dir().join("Jiffshot share");
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            dir.join(format!("{}.{ext}", self.stem()))
        };
        io::export(&doc, &path)?;
        #[cfg(target_os = "macos")]
        {
            let substitutions = doc.font_substitutions();
            if !substitutions.is_empty() {
                self.status_with_font_substitutions("Prepared the image to share", &substitutions);
            }
        }
        Ok(path)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn share_work(
        &mut self,
        pending: &str,
        work: impl FnOnce() -> std::result::Result<String, String> + Send + 'static,
    ) {
        self.set_status(pending);
        if let Err(error) = std::thread::Builder::new()
            .name("jiffshot-share".into())
            .spawn(move || {
                let result = work();
                post(move |app| match result {
                    Ok(note) => app.set_status(note),
                    Err(error) => app.set_status(format!("Share failed: {error}")),
                });
            })
        {
            self.set_status(format!("Share failed: {error}"));
        }
    }

    #[cfg(target_os = "macos")]
    fn share_dialog(
        &mut self,
        destination: &str,
        file: PathBuf,
        subject: String,
        folder: Option<PathBuf>,
    ) {
        let destination = destination.to_owned();
        self.share_work(&format!("Opening {destination}…"), move || {
            let result = match destination.as_str() {
                "Open with…" => crate::share::open_with_in(&file, folder.as_deref()),
                "Email" => crate::share::email(&file, &subject),
                "Print" => crate::share::print(&file),
                _ => unreachable!(),
            };
            result.map(|completed| {
                if completed {
                    match destination.as_str() {
                        "Open with…" => "Opened in the selected program".to_owned(),
                        "Email" => "Email sharing completed".to_owned(),
                        _ => "Print dialog completed".to_owned(),
                    }
                } else {
                    format!("{destination} cancelled")
                }
            })
        });
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn share_dialog_destination(&mut self, destination: &str, folder: Option<PathBuf>) {
        let file = match self.temp_export("png") {
            Ok(file) => file,
            Err(error) => {
                self.set_status(format!("Share failed: {error}"));
                return;
            }
        };
        self.share_dialog(destination, file, self.stem(), folder);
    }

    #[cfg(target_os = "macos")]
    fn share_application(&mut self, name: String, program: PathBuf, file: PathBuf) {
        let Some(program) = program.to_str().map(str::to_owned) else {
            return self.set_status("Share failed: the application path cannot be opened");
        };
        self.share_work(&format!("Opening {name}…"), move || {
            crate::share::open_in(&program, &file).map(|()| format!("Opened in {name}"))
        });
    }

    #[cfg(target_os = "macos")]
    fn share_folder(&mut self, file: PathBuf) {
        let Some(window) = self.window() else {
            return self.set_status("Share failed: no editor window");
        };
        let options = rustolonia::storage::FolderPickerOptions::new().title("Share to folder");
        let operation = match self.scope.open_folder_picker(&window, &options) {
            Ok(operation) => operation,
            Err(error) => {
                return self.set_status(format!("Share failed: could not choose a folder: {error}"))
            }
        };
        self.set_status("Choose a folder");
        if let Err(error) = self.scope.spawn(async move {
            let result = match operation.await {
                Ok(PickerOutcome::Selected(items)) => items
                    .first()
                    .and_then(|item| item.local_path().map(Path::to_path_buf))
                    .map(Some)
                    .ok_or_else(|| "the folder picker returned no local folder".to_owned()),
                Ok(PickerOutcome::Cancelled) => Ok(None),
                Err(error) => Err(format!("could not choose a folder: {error}")),
            };
            post(move |app| match result {
                Ok(Some(folder)) => app.share_work("Copying to folder…", move || {
                    crate::share::to_folder(&file, &folder)
                        .map(|path| format!("Copied to {}", path.display()))
                }),
                Ok(None) => app.set_status("Folder sharing cancelled"),
                Err(error) => app.set_status(format!("Share failed: {error}")),
            });
        }) {
            self.set_status(format!(
                "Share failed: could not wait for the folder picker: {error}"
            ));
        }
    }

    pub fn share(&mut self, to: &str) {
        use crate::share;
        #[cfg(target_os = "macos")]
        if to == "Add program…" {
            self.pick_program();
            return;
        }
        #[cfg(target_os = "macos")]
        if matches!(to, "Open with…" | "Email" | "Print") {
            self.share_dialog_destination(to, None);
            return;
        }
        #[cfg(target_os = "macos")]
        if to == "FTP" {
            let Some(url) = self.settings.ftp.clone() else {
                self.set_status("Share failed: Set an FTP destination in Preferences and add a \"Jiffshot FTP\" password for the username in Keychain Access.");
                return;
            };
            let file = match self.temp_export("png") {
                Ok(file) => file,
                Err(error) => return self.set_status(format!("Share failed: {error}")),
            };
            self.share_work("Uploading to FTP…", move || {
                crate::share::ftp(&url, &file).map(|at| format!("Uploaded to {at}"))
            });
            return;
        }
        #[cfg(target_os = "macos")]
        if matches!(to, "Preview" | "Paint" | "Folder…") {
            let file = match self.temp_export("png") {
                Ok(file) => file,
                Err(error) => return self.set_status(format!("Share failed: {error}")),
            };
            match to {
                "Preview" | "Paint" => {
                    self.share_application("Preview".into(), PathBuf::from("mspaint.exe"), file)
                }
                "Folder…" => self.share_folder(file),
                _ => unreachable!(),
            }
            return;
        }
        #[cfg(target_os = "macos")]
        if !reserved_share_name(to) {
            if let Some(program) = self
                .settings
                .programs
                .iter()
                .find(|(name, _)| name == to)
                .map(|(_, path)| path.clone())
            {
                let file = match self.temp_export("png") {
                    Ok(file) => file,
                    Err(error) => return self.set_status(format!("Share failed: {error}")),
                };
                self.share_application(to.to_owned(), program, file);
                return;
            }
        }
        let office = |exe: &'static str| move |p: &Path| share::open_in(exe, p);
        let result: std::result::Result<String, String> = (|| {
            match to {
            "Open with…" => share::open_with(&self.temp_export("png")?).map(|opened| if opened { "Choose a program" } else { "Open With cancelled" }.into()),
            "Email" => share::email(&self.temp_export("png")?, &self.stem()).map(|opened| if opened { "Opened a new message" } else { "Email cancelled" }.into()),
            "Word" => office("winword.exe")(&self.temp_export("png")?).map(|_| "Opened in Word".into()),
            "PowerPoint" => office("powerpnt.exe")(&self.temp_export("png")?).map(|_| "Opened in PowerPoint".into()),
            "Excel" => office("excel.exe")(&self.temp_export("png")?).map(|_| "Opened in Excel".into()),
            "OneNote" => office("onenote.exe")(&self.temp_export("png")?).map(|_| "Opened in OneNote".into()),
            "Paint" => office("mspaint.exe")(&self.temp_export("png")?).map(|_| "Opened in Paint".into()),
            "Preview" => office("mspaint.exe")(&self.temp_export("png")?).map(|_| "Opened in Preview".into()),
            "Print" => share::print(&self.temp_export("png")?).map(|opened| if opened { "Sent to the printer dialog" } else { "Print cancelled" }.into()),
            "FTP" => match self.settings.ftp.clone() {
                Some(url) => share::ftp(&url, &self.temp_export("png")?).map(|at| format!("Uploaded to {at}")),
                None if cfg!(target_os = "macos") => Err("Set an FTP destination in Preferences and add a \"Jiffshot FTP\" password for the username in Keychain Access.".into()),
                None => Err("Set \"ftp\": \"ftp://user@host/folder\" in %APPDATA%\\Jiffshot\\editor.json, and the password as the \"Jiffshot FTP\" credential (cmdkey /generic:\"Jiffshot FTP\" /user:USER /pass)".into()),
            },
            "Folder…" => {
                let file = self.temp_export("png")?;
                let Some(window) = self.window() else { return Err("no window".into()) };
                let options = rustolonia::storage::FolderPickerOptions::new().title("Share to folder");
                let op = self.scope.open_folder_picker(&window, &options).map_err(|e| e.to_string())?;
                let _ = self.scope.spawn(async move {
                    if let Ok(PickerOutcome::Selected(items)) = op.await {
                        if let Some(dir) = items.first().and_then(|i| i.local_path().map(Path::to_path_buf)) {
                            let result = share::to_folder(&file, &dir);
                            post(move |app| match result {
                                Ok(p) => app.set_status(format!("Copied to {}", p.display())),
                                Err(e) => app.set_status(format!("Copy failed: {e}")),
                            });
                        }
                    }
                });
                Ok("Choose a folder".into())
            }
            "Add program…" => {
                self.pick_program();
                Ok("Choose a program".into())
            }
            name => match self.settings.programs.iter().find(|(n, _)| n == name).map(|(_, p)| p.clone()) {
                Some(exe) => share::open_in(&exe.to_string_lossy(), &self.temp_export("png")?).map(|_| format!("Opened in {name}")),
                None => Err(format!("Unknown destination {name}")),
            },
        }
        })();
        match result {
            Ok(note) => self.set_status(note),
            Err(error) => self.set_status(format!("Share failed: {error}")),
        }
    }

    /// Asks for one or more files of the given extensions.
    pub(crate) fn pick_files(
        &mut self,
        title: &str,
        multiple: bool,
        label: &str,
        exts: &[&str],
        then: impl FnOnce(&mut App, Vec<PathBuf>) + Send + 'static,
    ) {
        self.pick_files_with_completion(title, multiple, label, exts, move |app, outcome| {
            match outcome {
                Ok(Some(paths)) => then(app, paths),
                Ok(None) => {}
                Err(error) => app.set_status(format!("File picker failed: {error}")),
            }
        });
    }

    pub(crate) fn pick_files_with_completion(
        &mut self,
        title: &str,
        multiple: bool,
        label: &str,
        exts: &[&str],
        then: impl FnOnce(&mut App, std::result::Result<Option<Vec<PathBuf>>, String>) + Send + 'static,
    ) -> bool {
        let Some(window) = self.window() else {
            return false;
        };
        let mut filter = FileTypeFilter::new(label);
        for e in exts {
            filter = filter.with_extension(e);
        }
        let options = OpenFilePickerOptions::new()
            .title(title)
            .allow_multiple(multiple)
            .start_in(WellKnownFolder::Videos)
            .file_type(filter);
        let op = match self.scope.open_file_picker(&window, &options) {
            Ok(op) => op,
            Err(error) => {
                self.set_status(format!("File picker failed: {error}"));
                return false;
            }
        };
        let spawned = self.scope.spawn(async move {
            let outcome = match op.await {
                Ok(PickerOutcome::Selected(items)) => Ok(Some(
                    items
                        .iter()
                        .filter_map(|i| i.local_path().map(Path::to_path_buf))
                        .collect(),
                )),
                Ok(_) => Ok(None),
                Err(error) => Err(error.to_string()),
            };
            post(move |app| then(app, outcome));
        });
        match spawned {
            Ok(_) => true,
            Err(error) => {
                self.set_status(format!("File picker failed: {error}"));
                false
            }
        }
    }

    #[cfg(windows)]
    fn pick_program(&mut self) {
        let Some(window) = self.window() else { return };
        let options = OpenFilePickerOptions::new()
            .title("Add a program to share to")
            .file_type(FileTypeFilter::new("Programs").with_extension("exe"));
        let Ok(op) = self.scope.open_file_picker(&window, &options) else {
            return;
        };
        let _ = self.scope.spawn(async move {
            if let Ok(PickerOutcome::Selected(items)) = op.await {
                if let Some(exe) = items
                    .first()
                    .and_then(|i| i.local_path().map(Path::to_path_buf))
                {
                    post(move |app| {
                        let name = exe
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        app.settings.programs.retain(|(n, _)| *n != name);
                        app.settings.programs.push((name.clone(), exe));
                        app.settings.save();
                        app.publish_shares();
                        app.set_status(format!("{name} added to Share"));
                    });
                }
            }
        });
    }

    #[cfg(target_os = "macos")]
    fn pick_program(&mut self) {
        self.pick_program_in(None);
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn pick_program_in(&mut self, folder: Option<&Path>) {
        if self.window().is_none() {
            return self.set_status("Could not choose a program: no editor window");
        }
        let folder = folder.map(Path::to_path_buf);
        self.set_status("Choose an application");
        // Avalonia's file picker drops .app bundles because they are directories.
        let spawned = std::thread::Builder::new()
            .name("jiffshot-application-picker".into())
            .spawn(move || {
                let result = jiffshot_capture::os::choose_application_path(
                    "Add a program to share to",
                    folder.as_deref(),
                );
                post(move |app| match result {
                    Ok(Some(path)) => app.add_share_program(path),
                    Ok(None) => app.set_status("Application selection cancelled"),
                    Err(error) => app.set_status(format!("Could not choose a program: {error}")),
                });
            });
        if let Err(error) = spawned {
            self.set_status(format!("Could not choose a program: {error}"));
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn add_share_program(&mut self, program: PathBuf) {
        let Some(name) = program
            .file_stem()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
        else {
            return self.set_status("Could not add the application: its name cannot be saved");
        };
        if reserved_share_name(&name) {
            return self.set_status(format!(
                "Could not add {name}: that name is already used by a built-in share destination"
            ));
        }
        let previous = self.settings.programs.clone();
        self.settings
            .programs
            .retain(|(existing, _)| *existing != name);
        self.settings.programs.push((name.clone(), program));
        if let Err(error) = self.settings.try_save() {
            self.settings.programs = previous;
            return self.set_status(format!("Could not add {name}: {error}"));
        }
        self.publish_shares();
        self.set_status(format!("{name} added to Share"));
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn save_share_preferences(&mut self, ftp: Option<&str>, names: &[String]) -> bool {
        let previous_ftp = self.settings.ftp.clone();
        let previous_programs = self.settings.programs.clone();
        if let Some(ftp) = ftp {
            self.settings.ftp = Some(ftp.trim().to_owned()).filter(|value| !value.is_empty());
        }
        self.settings
            .programs
            .retain(|(name, _)| !names.contains(name));
        let removed = previous_programs.len() - self.settings.programs.len();
        if removed == 0 && self.settings.ftp == previous_ftp {
            return true;
        }
        if let Err(error) = self.settings.try_save() {
            self.settings.ftp = previous_ftp;
            self.settings.programs = previous_programs;
            self.set_status(format!("Could not save Share preferences: {error}"));
            return false;
        }
        if removed > 0 {
            self.publish_shares();
        }
        self.set_status("Share preferences saved");
        true
    }

    // ---- custom stamps ---------------------------------------------------

    fn stamps_dir() -> Option<PathBuf> {
        jiffshot_capture::paths::config_dir().map(|d| d.join("Stamps"))
    }

    /// Loads the stamps in `%APPDATA%\Jiffshot\Stamps` into the Stamp tool.
    pub fn load_stamps(&mut self) {
        let Some(dir) = Self::stamps_dir() else {
            return;
        };
        let mut stamps = Vec::new();
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .unwrap_or_default();
            if !matches!(
                ext.as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp"
            ) {
                continue;
            }
            if let (Ok(pm), Some(stem)) = (io::load_image(&path), path.file_stem()) {
                stamps.push((stem.to_string_lossy().into_owned(), Arc::new(pm)));
            }
        }
        stamps.sort_by(|a, b| a.0.cmp(&b.0));
        with_editor(|e| e.custom_stamps = stamps);
        self.last_kind = Some("refresh".into());
    }

    /// Copies image files into the stamps folder.
    pub fn add_stamp(&mut self) {
        self.pick_open("Add stamps", true, true, |app, paths| {
            let Some(dir) = App::stamps_dir() else { return };
            let _ = std::fs::create_dir_all(&dir);
            let mut n = 0;
            for p in &paths {
                if let Some(name) = p.file_name() {
                    if std::fs::copy(p, dir.join(name)).is_ok() {
                        n += 1;
                    }
                }
            }
            app.load_stamps();
            app.set_status(format!(
                "Added {n} stamp(s); pick them in the Stamp tool's type list"
            ));
        });
    }

    // ---- selection and image commands -------------------------------------

    pub fn publish_narration(&mut self) {
        let on = self.narration.is_some();
        self.put_flag("Narrating", on, |k, v| k.set_narrating(v));
    }

    /// Changes a saved video choice and applies it.
    pub fn video_prefs(&mut self, change: impl FnOnce(&mut crate::settings::VideoPrefs)) {
        change(&mut self.settings.video);
        self.settings.save();
        crate::video::apply_volume(self);
    }

    /// Dark → Light → System (follows Windows) → Dark.
    pub fn cycle_theme(&mut self) {
        let next = match self.settings.theme.as_deref().unwrap_or("Dark") {
            "Dark" => "Light",
            "Light" => "System",
            _ => "Dark",
        };
        self.settings.set_theme(next);
        self.set_status(format!("Theme: {next}"));
    }

    pub fn set_background_fill(&mut self, value: &str) {
        with_editor(|e| e.background_fill = jiffshot_doc::editor::BackgroundFill::from_name(value));
    }

    pub fn smart_redact(&mut self) {
        let Some(flat) = with_editor(|e| e.document().flatten()) else {
            return;
        };
        let lines = match crate::ocr::recognize(&flat) {
            Ok(lines) => lines,
            Err(error) => return self.set_status(format!("Text recognition failed: {error}")),
        };
        let found = jiffshot_doc::redact::areas(&lines);
        if found.is_empty() {
            return self.set_status("Smart Redact found nothing sensitive");
        }
        let mut kinds: Vec<&str> = found.iter().map(|(_, k)| k.name()).collect();
        kinds.sort();
        kinds.dedup();
        let count = found.len();
        with_editor(|e| {
            let style = Style {
                fill: e.tool_style(Tool::Blur).fill.with_alpha(255),
                width: 0.0,
                ..Style::default()
            };
            let mut ids = Vec::new();
            e.edit_document("Smart Redact", |d| {
                for (area, _) in &found {
                    let rect = area.to_box().inflate(3.0);
                    ids.push(d.add(
                        Kind::Blur {
                            rect,
                            mode: jiffshot_doc::object::BlurMode::Redact,
                            amount: 10.0,
                        },
                        style.clone(),
                    ));
                }
                true
            });
            e.set_selection(ids);
        });
        self.set_status(format!(
            "Smart Redact covered {count} item(s): {}. Delete a box to reveal what is under it.",
            kinds.join(", ")
        ));
    }

    pub fn new_from_clipboard(&mut self) {
        self.new_from_clipboard_data(jiffshot_capture::clipboard::read(winutil::main_hwnd()));
    }

    pub(crate) fn new_from_clipboard_data(
        &mut self,
        contents: std::io::Result<Option<jiffshot_capture::clipboard::Pasted>>,
    ) {
        use jiffshot_capture::clipboard::Pasted;
        match contents {
            Ok(Some(Pasted::Image {
                width,
                height,
                rgba,
            })) => {
                if let Some(pm) = image::RgbaImage::from_raw(width, height, rgba)
                    .and_then(|i| io::from_rgba_image(&i))
                {
                    self.load(Document::new(pm), None, None);
                    self.set_status(format!("New image from the clipboard ({width} × {height})"));
                } else {
                    #[cfg(target_os = "macos")]
                    self.set_status(
                        "New from clipboard failed: the image has invalid dimensions or pixels",
                    );
                }
            }
            Ok(Some(Pasted::Files(files))) => match files.first() {
                Some(f) => self.open_path(f),
                None => self.set_status("The clipboard has no image"),
            },
            #[cfg(target_os = "macos")]
            Err(error) => self.set_status(format!("New from clipboard failed: {error}")),
            _ => self.set_status("The clipboard has no image"),
        }
    }

    /// Prints the flattened image through the system's photo printing.
    pub fn print(&mut self) {
        #[cfg(target_os = "macos")]
        {
            match self.temp_export("png") {
                Ok(path) => self.share_dialog("Print", path, String::new(), None),
                Err(error) => self.set_status(format!("Print failed: {error}")),
            }
        }
        #[cfg(windows)]
        {
            let Some(doc) = with_editor(|e| e.document().clone()) else {
                return;
            };
            let path = std::env::temp_dir().join(format!("{} (print).png", self.stem()));
            match io::export(&doc, &path) {
                Ok(()) => {
                    if winutil::print_file(&path) {
                        self.set_status("Sent to the printer dialog");
                    } else {
                        self.set_status("Could not open the system print dialog");
                    }
                }
                Err(error) => self.set_status(format!("Print failed: {error}")),
            }
        }
    }

    /// Closes the document and shows the next capture in the library.
    pub fn close_file(&mut self) {
        self.autosave();
        let current = self.source.clone();
        let next = self
            .library_items
            .iter()
            .map(|i| i.path.clone())
            .find(|p| Some(p) != current.as_ref());
        match next {
            Some(p) => self.open_path(&p),
            None => self.new_image(),
        }
    }

    /// Copies the current capture (with its edits) as a new library item.
    pub fn duplicate_file(&mut self) {
        let Some(doc) = with_editor(|e| e.document().clone()) else {
            return;
        };
        let folder = self
            .source
            .as_ref()
            .and_then(|s| s.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| self.library.root.join("Copies"));
        let mut path = folder.join(format!("{} copy.png", self.stem()));
        let mut n = 2;
        while path.exists() {
            path = folder.join(format!("{} copy {n}.png", self.stem()));
            n += 1;
        }
        let result = std::fs::create_dir_all(&folder)
            .map_err(|e| e.to_string())
            .and_then(|_| {
                io::write_atomic(&path, &io::encode(doc.background(), io::Format::Png, 0)?)
            })
            .and_then(|_| io::save_document(&doc, &path.with_extension(EDITS_EXT)));
        match result {
            Ok(()) => {
                self.refresh_library();
                self.open_path(&path);
                self.set_status(format!("Duplicated as {}", path.display()));
            }
            Err(error) => self.set_status(format!("Duplicate failed: {error}")),
        }
    }

    /// Moves the current capture and its edits to the Recycle Bin.
    pub fn delete_file(&mut self) {
        let Some(src) = self
            .source
            .clone()
            .filter(|s| s.starts_with(&self.library.root))
        else {
            return self.set_status("Only library captures can be deleted here");
        };
        with_editor(|e| e.mark_saved());
        let files: Vec<PathBuf> = ["png", "mp4", EDITS_EXT, "json", "cursor.png", "cursor.json"]
            .iter()
            .map(|x| src.with_extension(x))
            .filter(|p| p.exists())
            .collect();
        if winutil::recycle(&files) {
            self.source = None;
            self.path = None;
            self.refresh_library();
            self.close_file();
            let trash = if cfg!(target_os = "macos") {
                "Trash"
            } else {
                "Recycle Bin"
            };
            self.set_status(format!(
                "Moved {} to the {trash}",
                src.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ));
        } else {
            let trash = if cfg!(target_os = "macos") {
                "Trash"
            } else {
                "Recycle Bin"
            };
            self.set_status(format!("Could not move the capture to the {trash}"));
        }
    }

    // ---- library ---------------------------------------------------------

    /// Reads the library from disk and shows it.
    pub fn refresh_library(&mut self) {
        self.library_entries = self
            .library
            .recent(2000)
            .into_iter()
            .map(library::Entry::read)
            .collect();
        crate::tray::set_recent(
            self.library_entries
                .iter()
                .take(10)
                .map(|e| file_uri(&e.path))
                .collect(),
        );
        self.show_library();
    }

    /// Shows the library entries already read, searched, filtered and
    /// sorted.
    fn show_library(&mut self) {
        let entries = std::mem::take(&mut self.library_entries);
        self.show_entries(&entries);
        self.library_entries = entries;
    }

    fn show_entries(&mut self, entries: &[library::Entry]) {
        let view = self.settings.library_view.clone();
        // The tray leaves out what was closed from it; the library view
        // shows everything.
        let closed: std::collections::HashSet<&PathBuf> = view.closed.iter().collect();
        let visible: Vec<library::Entry> = entries
            .iter()
            .filter(|e| view.expanded || !closed.contains(&e.path))
            .cloned()
            .collect();
        let shown = library::select(
            &visible,
            &view.search,
            &view.filter,
            &view.sort,
            std::time::SystemTime::now(),
        );
        let limit = if view.expanded { 600 } else { 120 };
        self.library_selected
            .retain(|p| shown.iter().any(|e| &e.path == p));
        self.library_items = shown
            .into_iter()
            .take(limit)
            .map(|e| {
                let (current, selected) = (
                    self.source.as_deref() == Some(e.path.as_path()),
                    self.library_selected.contains(&e.path),
                );
                library::Item::new(e, current, selected, view.thumb * 88.0 / 120.0)
            })
            .collect();
        let filters = library::filters(entries);
        if let Some(sink) = &self.sink {
            #[cfg(target_os = "macos")]
            crate::file_drag_macos::retire_tiles();
            let _ = sink.clear_library();
            for item in &self.library_items {
                let _ = sink.add_library(item.clone());
            }
            if self.shown_filters != filters {
                // A new list clears the box's choice: show it again.
                self.shown.strings.remove("LibraryFilter");
                self.shown.strings.remove("LibrarySort");
                let _ = sink.clear_library_filters();
                for f in &filters {
                    let _ = sink.add_library_filters(f);
                }
                if self.shown_filters.is_empty() {
                    let _ = sink.clear_library_sorts();
                    for s in library::SORTS {
                        let _ = sink.add_library_sorts(*s);
                    }
                }
            }
        }
        self.shown_filters = filters;
        let hidden = if view.expanded { 0 } else { closed.len() };
        let mut info = format!(
            "{} of {} captures in {}",
            self.library_items.len(),
            entries.len(),
            self.library.root.display()
        );
        if hidden > 0 {
            info += &format!(" · {hidden} closed from the tray");
        }
        self.put_str("LibraryInfo", &info, |k, v| k.set_library_info(v));
        self.put_str("LibrarySearch", &view.search, |k, v| {
            k.set_library_search(v)
        });
        self.put_str("LibraryFilter", &view.filter, |k, v| {
            k.set_library_filter(v)
        });
        self.put_str("LibrarySort", &view.sort, |k, v| k.set_library_sort(v));
        self.put_num("ThumbSize", view.thumb, |k, v| k.set_thumb_size(v));
        self.put_flag("LibraryExpanded", view.expanded, |k, v| {
            k.set_library_expanded(v)
        });
        let count = self.library_selected.len() as f64;
        self.put_num("LibrarySelected", count, |k, v| {
            k.set_library_selected(v as i64)
        });
        self.mark_unsaved();
        library::make_thumbnails(self.library_items.clone());
    }

    /// Changes how the library is shown and refreshes it.
    /// Changes which parts of the window show, and remembers it.
    pub fn layout(&mut self, change: impl FnOnce(&mut crate::settings::Layout)) {
        change(&mut self.settings.layout);
        self.settings.save();
        self.publish_layout();
    }

    fn publish_layout(&mut self) {
        let l = self.settings.layout.clone();
        self.put_flag("ShowSideBar", l.side_bar, |k, v| k.set_show_side_bar(v));
        self.put_flag("ShowEffects", l.effects, |k, v| k.set_show_effects(v));
        self.put_flag("ShowTray", l.tray, |k, v| k.set_show_tray(v));
        self.put_flag("ToolLabels", l.labels, |k, v| k.set_tool_labels(v));
    }

    pub fn library_view(&mut self, change: impl FnOnce(&mut crate::settings::LibraryView)) {
        change(&mut self.settings.library_view);
        self.settings.save();
        self.show_library();
    }

    /// The captures an item action applies to: the selection when the item
    /// is part of it, else the item alone.
    fn library_targets(&self, path: &Path) -> Vec<PathBuf> {
        if self.library_selected.contains(path) {
            self.library_items
                .iter()
                .filter(|i| self.library_selected.contains(&i.path))
                .map(|i| i.path.clone())
                .collect()
        } else {
            vec![path.to_path_buf()]
        }
    }

    #[cfg(target_os = "macos")]
    fn library_sources(
        &self,
        targets: Vec<PathBuf>,
    ) -> std::result::Result<Vec<crate::library_transfer_macos::Source>, String> {
        use crate::library_transfer_macos::Source;

        let mut sources = Vec::with_capacity(targets.len());
        for path in targets {
            let is_video = path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("mp4"));
            let edits = path.with_extension(EDITS_EXT);
            let open = !is_video
                && (self.source.as_deref() == Some(path.as_path())
                    || self.path.as_deref() == Some(edits.as_path()));
            let open_document = if open {
                match with_editor(|editor| editor.document().clone()) {
                    Some(doc) => Some(doc),
                    None => return Err("the open editor is unavailable".into()),
                }
            } else {
                None
            };
            sources.push(Source {
                path,
                open_document,
            });
        }
        Ok(sources)
    }

    #[cfg(target_os = "macos")]
    fn copy_captures(&mut self, targets: Vec<PathBuf>) {
        let sources = match self.library_sources(targets) {
            Ok(sources) => sources,
            Err(error) => return self.set_status(format!("Copy failed: {error}")),
        };
        let Some(generation) = self.library_copy_generation.checked_add(1) else {
            return self.set_status("Copy failed: too many copy requests");
        };
        self.library_copy_generation = generation;
        let change_count = jiffshot_capture::clipboard::change_count();
        self.set_status("Preparing captures for the clipboard…");
        let worker = std::thread::Builder::new()
            .name("jiffshot-library-copy".into())
            .spawn(move || {
                let result = crate::library_transfer_macos::prepare(sources);
                if let Err(error) =
                    crate::shutdown_ui_macos::post("the library copy result", move || {
                        finish_library_copy_when_ready(generation, change_count, result);
                    })
                {
                    jiffshot_workflow::log(&format!(
                        "Could not deliver the library copy to the editor: {error}"
                    ));
                }
            });
        if let Err(error) = worker {
            self.set_status(format!(
                "Copy failed: could not start the copy worker: {error}"
            ));
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn retry_library_copy_for_selftest(
        &mut self,
        path: PathBuf,
    ) -> std::result::Result<(), String> {
        if std::env::var_os("JIFFSHOT_UITEST").is_none()
            || std::env::var_os("JIFFSHOT_UITEST_LIBRARY_COPY").is_none()
        {
            return Err("the library copy retry fixture requires UI self-test mode".into());
        }
        let generation = self
            .library_copy_generation
            .checked_add(1)
            .ok_or("too many copy requests")?;
        self.library_copy_generation = generation;
        let change_count = jiffshot_capture::clipboard::change_count();
        let result =
            crate::library_transfer_macos::prepare(vec![crate::library_transfer_macos::Source {
                path,
                open_document: None,
            }]);
        self.set_status("Preparing captures for the clipboard…");
        finish_library_copy_when_ready(generation, change_count, result);
        if self.status != "Preparing captures for the clipboard…" {
            return Err("the busy copy was delivered reentrantly".into());
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn finish_library_copy(
        &mut self,
        generation: u64,
        change_count: isize,
        result: std::result::Result<crate::library_transfer_macos::Prepared, String>,
    ) {
        if generation != self.library_copy_generation {
            return;
        }
        if jiffshot_capture::clipboard::change_count() != change_count {
            return self.set_status(
                "Copy cancelled because the clipboard changed while preparing the captures",
            );
        }
        match result.and_then(|prepared| prepared.copy(winutil::main_hwnd())) {
            Ok((message, substitutions)) => {
                self.status_with_font_substitutions(message, &substitutions)
            }
            Err(error) => self.set_status(format!("Copy failed: {error}")),
        }
    }

    #[cfg(windows)]
    /// An image capture as it looks now, including unsaved edits when it is
    /// open in the editor.
    fn capture_pixmap(&self, path: &Path) -> Option<jiffshot_doc::Pixmap> {
        if library::is_video(path) {
            return None;
        }
        if self.source.as_deref() == Some(path) {
            if let Some(flat) = with_editor(|e| e.document().flatten()) {
                return Some(flat);
            }
        }
        library::render_image(path)
    }

    #[cfg(windows)]
    /// Files that stand for captures outside Jiffshot: edited images are
    /// flattened to PNGs in a scratch folder, the rest are the captures
    /// themselves.
    pub fn shareable_files(&self, targets: &[PathBuf]) -> Vec<PathBuf> {
        let scratch = jiffshot_capture::paths::local_dir().join("shared");
        let mut used = std::collections::HashSet::new();
        let mut files = Vec::new();
        for t in targets.iter().filter(|t| t.exists()) {
            let open_modified = self.source.as_ref() == Some(t)
                && with_editor(|e| e.is_modified()).unwrap_or(false);
            if library::is_video(t) || !(open_modified || library::has_edits(t)) {
                files.push(t.clone());
                continue;
            }
            let stem = t
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "capture".to_owned());
            let mut name = format!("{stem}.png");
            let mut n = 2;
            while !used.insert(name.to_lowercase()) {
                name = format!("{stem} ({n}).png");
                n += 1;
            }
            let out = scratch.join(name);
            let written = self
                .capture_pixmap(t)
                .and_then(|pm| jiffshot_doc::io::encode(&pm, jiffshot_doc::io::Format::Png, 0).ok())
                .is_some_and(|bytes| {
                    std::fs::create_dir_all(&scratch)
                        .and_then(|()| std::fs::write(&out, bytes))
                        .is_ok()
                });
            files.push(if written { out } else { t.clone() });
        }
        files
    }

    #[cfg(windows)]
    /// Copies one image as pixels, or any other captures as files.
    fn copy_captures(&mut self, targets: &[PathBuf]) {
        let hwnd = winutil::main_hwnd();
        if let [one] = targets {
            if let Some(pm) = self.capture_pixmap(one) {
                let frame = winutil::frame_from_pixmap(&pm);
                match jiffshot_capture::clipboard::copy(&frame, hwnd) {
                    Ok(()) => self.set_status(format!(
                        "Copied the image ({} × {}) to the clipboard",
                        pm.width(),
                        pm.height()
                    )),
                    Err(error) => self.set_status(format!("Copy failed: {error}")),
                }
                return;
            }
        }
        let files = self.shareable_files(targets);
        if files.is_empty() {
            return self.set_status("Nothing to copy");
        }
        match jiffshot_capture::clipboard::copy_files(&files, hwnd) {
            Ok(()) => self.set_status(format!("Copied {} to the clipboard", count_of(files.len()))),
            Err(error) => self.set_status(format!("Copy failed: {error}")),
        }
    }

    #[cfg(windows)]
    /// The files to drag out for a pressed library item, if it is still listed.
    pub fn drag_files(&self, path: &Path) -> Vec<PathBuf> {
        if !self.library_items.iter().any(|i| i.path == path) {
            return Vec::new();
        }
        self.shareable_files(&self.library_targets(path))
    }

    /// Pin, close, delete, rename, tag, select, copy or reveal library items.
    pub fn library_action(&mut self, path: &Path, what: &str, value: Option<String>) {
        let targets = self.library_targets(path);
        match what {
            "select" => {
                if !self.library_selected.remove(path) {
                    self.library_selected.insert(path.to_path_buf());
                }
                let on = self.library_selected.contains(path);
                if let Some(item) = self.library_items.iter().find(|i| i.path == path) {
                    item.set_selected(on);
                }
                let count = self.library_selected.len() as f64;
                self.put_num("LibrarySelected", count, |k, v| {
                    k.set_library_selected(v as i64)
                });
                return;
            }
            "reveal" => return winutil::reveal(path),
            #[cfg(target_os = "macos")]
            "copy" => {
                if !self.library_items.iter().any(|item| item.path == path) {
                    return self
                        .set_status("Copy failed: the capture is no longer in the library view");
                }
                return self.copy_captures(targets);
            }
            #[cfg(windows)]
            "copy" => return self.copy_captures(&targets),
            "pin" => {
                let pin = match jiffshot_capture::library::read_meta_checked(path) {
                    Ok(meta) => !meta.is_some_and(|meta| meta.pinned),
                    Err(error) => {
                        self.set_status(format!(
                            "Could not read the capture's details for {}: {error}",
                            path.display()
                        ));
                        return;
                    }
                };
                if self.edit_metas(&targets, |m| m.pinned = pin) {
                    self.set_status(format!(
                        "{} {}",
                        if pin { "Pinned" } else { "Unpinned" },
                        count_of(targets.len())
                    ));
                }
            }
            "tags" => {
                let tags = library::parse_tags(value.as_deref().unwrap_or(""));
                if self.edit_metas(&targets, |m| m.tags = tags.clone()) {
                    self.set_status(if tags.is_empty() {
                        "Tags removed".to_owned()
                    } else {
                        format!("Tagged {}: {}", count_of(targets.len()), tags.join(", "))
                    });
                }
            }
            "close" => {
                for t in &targets {
                    if !self.settings.library_view.closed.contains(t) {
                        self.settings.library_view.closed.push(t.clone());
                    }
                }
                self.settings.save();
                self.set_status(format!(
                    "Closed {} from the tray; they stay in the library",
                    count_of(targets.len())
                ));
            }
            "delete" => {
                let mut files = Vec::new();
                for t in &targets {
                    if Some(t) == self.source.as_ref() {
                        with_editor(|e| e.mark_saved());
                        self.source = None;
                        self.path = None;
                        self.close_file();
                    }
                    files.push(t.clone());
                    files.extend(
                        jiffshot_capture::library::companions(t)
                            .into_iter()
                            .filter(|c| c.exists()),
                    );
                }
                if winutil::recycle(&files) {
                    let trash = if cfg!(target_os = "macos") {
                        "Trash"
                    } else {
                        "Recycle Bin"
                    };
                    self.set_status(format!("Moved {} to the {trash}", count_of(targets.len())));
                } else {
                    let trash = if cfg!(target_os = "macos") {
                        "Trash"
                    } else {
                        "Recycle Bin"
                    };
                    self.set_status(format!("Could not move the capture to the {trash}"));
                }
            }
            "rename" => {
                let name = value.unwrap_or_default();
                match jiffshot_capture::library::rename(path, &name) {
                    Ok(new) => {
                        let thumb = library::thumbnail_path(path);
                        let _ = std::fs::rename(&thumb, library::thumbnail_path(&new));
                        if self.source.as_deref() == Some(path) {
                            self.source = Some(new.clone());
                        }
                        let old_edits = path.with_extension(EDITS_EXT);
                        let new_edits = new.with_extension(EDITS_EXT);
                        if self.path.as_deref() == Some(old_edits.as_path()) {
                            self.path = Some(new_edits.clone());
                        }
                        if self.library_selected.remove(path) {
                            self.library_selected.insert(new.clone());
                        }
                        let mut settings_changed = false;
                        for recent in &mut self.settings.recent {
                            if recent == path {
                                *recent = new.clone();
                                settings_changed = true;
                            } else if *recent == old_edits {
                                *recent = new_edits.clone();
                                settings_changed = true;
                            }
                        }
                        for closed in &mut self.settings.library_view.closed {
                            if closed == path {
                                *closed = new.clone();
                                settings_changed = true;
                            }
                        }
                        if settings_changed {
                            let mut seen = std::collections::HashSet::new();
                            self.settings
                                .recent
                                .retain(|recent| seen.insert(recent.clone()));
                            let mut seen = std::collections::HashSet::new();
                            self.settings
                                .library_view
                                .closed
                                .retain(|closed| seen.insert(closed.clone()));
                            self.settings.save();
                            self.publish_recent();
                        }
                        self.set_status(format!(
                            "Renamed to {}",
                            new.file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default()
                        ));
                    }
                    Err(error) => self.set_status(format!("Could not rename: {error}")),
                }
            }
            _ => return,
        }
        self.refresh_library();
    }

    fn edit_metas(
        &mut self,
        paths: &[PathBuf],
        change: impl Fn(&mut jiffshot_capture::library::CaptureMeta),
    ) -> bool {
        let failures: Vec<String> = paths
            .iter()
            .filter_map(|path| {
                Self::edit_meta(path, &change)
                    .err()
                    .map(|error| format!("{}: {error}", path.display()))
            })
            .collect();
        if failures.is_empty() {
            true
        } else {
            self.set_status(format!(
                "Could not save the capture's details: {}; {} of {} updated",
                failures.join("; "),
                paths.len() - failures.len(),
                paths.len()
            ));
            false
        }
    }

    fn edit_meta(
        path: &Path,
        change: impl FnOnce(&mut jiffshot_capture::library::CaptureMeta),
    ) -> std::io::Result<()> {
        use jiffshot_capture::library::{read_meta_checked, write_meta, CaptureKind, CaptureMeta};
        std::fs::metadata(path)?;
        // Recordings and imported files get a sidecar when first tagged.
        let mut meta = read_meta_checked(path)?.unwrap_or_else(|| {
            let (w, h) = io::load_image(path)
                .map(|p| (p.width(), p.height()))
                .unwrap_or((0, 0));
            CaptureMeta {
                kind: if path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("mp4"))
                {
                    CaptureKind::Video
                } else {
                    CaptureKind::Region
                },
                rect: jiffshot_capture::Rect::new(0, 0, w as i32, h as i32),
                width: w,
                height: h,
                captured_at: String::new(),
                window_title: None,
                process: None,
                cursor_included: false,
                url: None,
                tags: Vec::new(),
                pinned: false,
            }
        });
        change(&mut meta);
        write_meta(path, &meta)
    }

    /// Images or videos picked in the library, combined into one.
    pub fn combine_selected(&mut self) {
        let picked: Vec<PathBuf> = self
            .library_items
            .iter()
            .filter(|i| self.library_selected.contains(&i.path))
            .map(|i| i.path.clone())
            .collect();
        if picked.len() < 2 {
            return self.set_status(crate::ui::shortcut(
                "Ctrl+click two or more captures in the library first.",
            ));
        }
        let videos = picked
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4")))
            .count();
        if videos == picked.len() {
            return crate::video::combine_files(self, picked);
        }
        let images: Vec<_> = picked
            .iter()
            .filter_map(|p| io::load_image(p).ok())
            .collect();
        if let Some(doc) = combine::combine(&images, combine::Arrangement::Grid, 24.0, Rgba::WHITE)
        {
            self.load(doc, None, None);
            with_editor(|e| e.checkpoint("Combine", None));
            self.set_status(format!("Combined {} images", images.len()));
        }
    }

    pub fn clear_library_selection(&mut self) {
        self.library_selected.clear();
        for i in &self.library_items {
            i.set_selected(false);
        }
        self.put_num("LibrarySelected", 0.0, |k, v| {
            k.set_library_selected(v as i64)
        });
    }

    /// Brings back everything closed from the tray.
    pub fn show_closed(&mut self) {
        let n = self.settings.library_view.closed.len();
        self.library_view(|v| v.closed.clear());
        self.set_status(format!("{} back in the tray", count_of(n)));
    }

    /// Marks the open capture's tray item while it has unsaved edits.
    fn mark_unsaved(&self) {
        let dirty = with_editor(|e| e.is_modified()).unwrap_or(false);
        for item in &self.library_items {
            item.set_unsaved(dirty && self.source.as_deref() == Some(item.path.as_path()));
        }
    }

    fn mark_current(&mut self) {
        let current = self.source.clone();
        for item in &self.library_items {
            item.set_current(current.as_deref() == Some(item.path.as_path()));
        }
    }

    /// A new capture arrived from the capture side (or another launch).
    pub fn incoming(&mut self, path: PathBuf) {
        self.show_editor();
        self.open_path(&path);
        self.refresh_library();
    }

    // ---- text recognition -------------------------------------------------

    pub fn grab_text(&mut self) {
        let Some(flat) = with_editor(|e| e.document().flatten()) else {
            return;
        };
        match crate::ocr::recognize(&flat) {
            Ok(lines) if !lines.is_empty() => {
                let text = lines
                    .iter()
                    .map(|l| l.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let preview: String = text.chars().take(80).collect();
                let copied = jiffshot_capture::clipboard::copy_text(&text, winutil::main_hwnd());
                #[cfg(target_os = "macos")]
                if let Err(error) = copied {
                    self.set_status(format!("Text found but copy failed: {error}. {preview}"));
                    return;
                }
                #[cfg(windows)]
                let _ = copied;
                self.set_status(format!("Copied {} line(s) of text: {preview}", lines.len()));
            }
            Ok(_) => self.set_status("No text found"),
            Err(error) => self.set_status(format!("Text recognition failed: {error}")),
        }
    }

    pub fn simplify(&mut self) {
        #[cfg(target_os = "macos")]
        let mut substitutions =
            with_editor(|e| e.document().font_substitutions()).unwrap_or_default();
        let Some(flat) = with_editor(|e| e.document().flatten()) else {
            return;
        };
        let lines = crate::ocr::recognize(&flat).unwrap_or_default();
        let mut bars = 0;
        self.document_op("Simplify", |d| {
            d.flatten_objects();
            bars = jiffshot_doc::smart::simplify(d, &lines, 12);
            true
        });
        #[cfg(target_os = "macos")]
        {
            for substitution in
                with_editor(|e| e.document().font_substitutions()).unwrap_or_default()
            {
                if !substitutions.contains(&substitution) {
                    substitutions.push(substitution);
                }
            }
            self.status_with_font_substitutions(
                format!("Simplified: {bars} text line(s) replaced by editable bars"),
                &substitutions,
            );
        }
        #[cfg(not(target_os = "macos"))]
        self.set_status(format!(
            "Simplified: {bars} text line(s) replaced by editable bars"
        ));
    }

    // ---- video (see video.rs) --------------------------------------------

    pub(crate) fn stop_video(&mut self) {
        if let Some(v) = self.video.take() {
            v.stop();
        }
        self.cancel_narration();
    }

    pub(crate) fn cancel_narration(&mut self) {
        self.narration_request = None;
        if let Some(mut narration) = self.narration.take() {
            narration.cancel();
            self.publish_narration();
        }
    }

    fn open_video(&mut self, path: &Path) {
        self.autosave();
        self.stop_video();
        match crate::video::Session::open(path) {
            Ok(mut session) => {
                let frame = session.frame_at(0.0);
                let duration = session.duration();
                let info = session.info();
                self.video = Some(session);
                crate::video::apply_volume(self);
                if let Some(pm) = frame {
                    with_editor(|e| e.load(Document::new(pm)));
                }
                self.path = None;
                self.source = Some(path.to_path_buf());
                if let Some(sink) = &self.sink {
                    let _ = sink.set_video_duration(duration);
                    let _ = sink.set_video_position(0.0);
                    let _ = sink.set_trim_start(0.0);
                    let _ = sink.set_trim_end(duration);
                    let _ = sink.set_video_info(&info);
                }
                self.zoom("fit");
                self.bump_frame();
                self.mark_current();
                self.set_status(format!("Opened video {}", path.display()));
            }
            Err(error) => {
                self.set_status(format!("Could not open video {}: {error}", path.display()))
            }
        }
    }

    pub fn video_action(&mut self, what: &str) {
        crate::video::action(self, what);
    }
}

/// A quick style shown in the panel; Apply puts it on the selection (or the
/// tool).
pub struct QuickStyle {
    /// Position in the tool's list.
    index: usize,
    tool: Tool,
    style: Style,
    favorite: bool,
    sink: Option<QuickStyleViewModelSink>,
}

impl QuickStyle {
    fn edit(&self, what: &'static str) {
        let (tool, index, style) = (self.tool, self.index, self.style.clone());
        post(move |app| app.edit_quick_style(tool, index, &style, what));
    }
}

impl QuickStyleViewModel for QuickStyle {
    fn attach(&mut self, sink: QuickStyleViewModelSink) -> Result<()> {
        let s = &self.style;
        sink.set_name(format!("{} {}", self.tool.name(), self.index + 1))?;
        sink.set_stroke(s.stroke.to_hex())?;
        sink.set_fill(if s.multiply {
            s.fill.with_alpha(255).to_hex()
        } else {
            s.fill.to_hex()
        })?;
        sink.set_text_color(s.text_color.to_hex())?;
        sink.set_width(s.width as f64)?;
        sink.set_is_favorite(self.favorite)?;
        self.sink = Some(sink);
        Ok(())
    }

    fn detach(&mut self) -> Result<()> {
        self.sink = None;
        Ok(())
    }

    fn apply(&mut self) -> Result<()> {
        let (tool, style) = (self.tool, self.style.clone());
        // Runs after this call returns, outside the nested model's lock.
        post(move |app| app.apply_quick_style(tool, style));
        Ok(())
    }

    fn delete(&mut self) -> Result<()> {
        self.edit("delete");
        Ok(())
    }

    fn move_earlier(&mut self) -> Result<()> {
        self.edit("earlier");
        Ok(())
    }

    fn move_later(&mut self) -> Result<()> {
        self.edit("later");
        Ok(())
    }

    fn toggle_favorite(&mut self) -> Result<()> {
        self.edit("favorite");
        Ok(())
    }
}

/// Shared handle for the library item sinks, updated from the thumbnail
/// thread.
pub type ItemSink = Arc<Mutex<Option<LibraryItemViewModelSink>>>;

/// A capture taken with "pointer as an object" has the pointer saved next to
/// it (`<stem>.cursor.png` and `.cursor.json`); it opens as a movable image.
fn add_cursor_object(doc: &mut jiffshot_doc::Document, path: &Path) {
    if let Err(error) = try_add_cursor_object(doc, path) {
        jiffshot_workflow::log(&error);
    }
}

pub(crate) fn try_add_cursor_object(
    doc: &mut jiffshot_doc::Document,
    path: &Path,
) -> std::result::Result<bool, String> {
    let (png, metadata) = jiffshot_capture::library::cursor_paths(path);
    let json = match std::fs::read_to_string(&metadata) {
        Ok(json) => json,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !metadata.is_symlink() => {
            return Ok(false)
        }
        Err(error) => {
            return Err(format!(
                "could not read editable pointer {}: {error}",
                metadata.display()
            ))
        }
    };
    match jiffshot_capture::library::read_meta_checked(path) {
        Ok(Some(meta)) if !meta.cursor_included => return Ok(false),
        Ok(_) => {}
        Err(error) => {
            return Err(format!(
                "could not read capture metadata for its editable pointer: {error}"
            ))
        }
    }
    let at = serde_json::from_str::<jiffshot_capture::library::CursorPlacement>(&json).map_err(
        |error| {
            format!(
                "invalid editable pointer metadata {}: {error}",
                metadata.display()
            )
        },
    )?;
    let image = jiffshot_doc::Pixmap::load_png(&png)
        .map_err(|error| format!("could not load editable pointer {}: {error}", png.display()))?;
    let [x, y, width, height] = at
        .geometry((image.width(), image.height()))
        .map_err(|error| {
            format!(
                "invalid editable pointer metadata {}: {error}",
                metadata.display()
            )
        })?;
    let rect = jiffshot_doc::Box2::from_xywh(x, y, width, height);
    let kind = jiffshot_doc::object::Kind::Image {
        rect,
        image: jiffshot_doc::object::ImageRef::new("cursor".to_owned(), image),
    };
    doc.add(
        kind,
        jiffshot_doc::object::Style {
            width: 0.0,
            ..Default::default()
        },
    );
    Ok(true)
}

/// "1 capture", "3 captures".
fn count_of(n: usize) -> String {
    format!("{n} capture{}", if n == 1 { "" } else { "s" })
}

/// A path as the storage URI the Open recent menu shows.
#[cfg(windows)]
fn file_uri(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file:///");
    for c in text.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '%' => out.push_str("%25"),
            '#' => out.push_str("%23"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let mut out = if path.is_absolute() {
        String::from("file://")
    } else {
        String::from("file:./")
    };
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    out
}

pub(crate) use jiffshot_capture::paths::decode_uri_path;

/// Back from a file URI (or a plain path).
#[cfg(windows)]
fn path_of_uri(uri: &str) -> std::result::Result<PathBuf, String> {
    let Some(rest) = uri.strip_prefix("file:///") else {
        return Ok(PathBuf::from(uri));
    };
    let bytes = decode_uri_path(rest)?;
    let text = String::from_utf8(bytes).map_err(|_| "the file URI is not valid UTF-8")?;
    Ok(PathBuf::from(text.replace('/', "\\")))
}

#[cfg(target_os = "macos")]
fn path_of_uri(uri: &str) -> std::result::Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;

    let path = if uri.starts_with("file:///") {
        &uri["file://".len()..]
    } else if let Some(path) = uri.strip_prefix("file:./") {
        path
    } else if uri.starts_with("file://") {
        return Err("the file URI does not refer to a local path".into());
    } else {
        return Ok(PathBuf::from(uri));
    };
    Ok(PathBuf::from(std::ffi::OsString::from_vec(
        decode_uri_path(path)?,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn capture_refresh_coalesces_updates_and_retries_busy_delivery() {
        let latch = CaptureRefreshLatch::new();
        assert!(latch.request());
        assert!(!latch.request());
        latch.begin();
        assert!(!latch.request());
        assert!(latch.finish());
        latch.begin();
        assert!(!latch.finish());

        assert!(latch.request());
        latch.begin();
        latch.retry();
        assert!(!latch.request());
        latch.begin();
        assert!(!latch.finish());

        assert!(latch.request());
        latch.abandon();
        assert!(latch.queue_if_dirty());
        latch.begin();
        assert!(!latch.finish());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn capture_refresh_keeps_a_concurrent_final_update() {
        for _ in 0..200 {
            let latch = CaptureRefreshLatch::new();
            assert!(latch.request());
            latch.begin();
            let barrier = std::sync::Barrier::new(2);
            let (producer_queued, consumer_queued) = std::thread::scope(|scope| {
                let producer = scope.spawn(|| {
                    barrier.wait();
                    latch.request()
                });
                barrier.wait();
                let consumer_queued = latch.finish();
                (producer.join().unwrap(), consumer_queued)
            });
            assert_ne!(
                producer_queued, consumer_queued,
                "exactly one callback must be queued for the new update"
            );
            latch.begin();
            assert!(!latch.finish());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn text_tool_font_notices_use_the_rendered_style() {
        let style = Style {
            font: "Jiffshot deliberately unavailable UI face e70346".into(),
            bold: false,
            italic: true,
            ..Style::default()
        };
        assert!(font_notice_for_style(Tool::Text, &style)
            .unwrap()
            .contains("(italic): using Helvetica Neue"));
        assert!(font_notice_for_style(Tool::Callout, &style)
            .unwrap()
            .contains("(italic): using Helvetica Neue"));
        assert!(font_notice_for_style(Tool::Step, &style)
            .unwrap()
            .contains("(bold): using Helvetica Neue"));
        assert_eq!(font_notice_for_style(Tool::Shape, &style), None);
        assert_eq!(
            font_notice_for_style(
                Tool::Text,
                &Style {
                    font: "Helvetica Neue".into(),
                    ..style
                }
            ),
            None
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn built_in_share_names_cannot_hide_custom_programs() {
        for name in crate::share::BUILT_IN
            .iter()
            .copied()
            .chain(["Paint", "Add program…"])
        {
            assert!(reserved_share_name(name), "{name}");
        }
        assert!(!reserved_share_name("Fixture Application"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cursor_object_loads_from_capture_companions() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "jiffshot-cursor-path-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("capture-caf\u{e9}.png");
        let mut pointer = jiffshot_capture::Frame::new(3, 2);
        pointer.alpha = true;
        for pixel in pointer.pixels.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[0, 0, 255, 255]);
        }
        jiffshot_capture::library::write_cursor(&path, &pointer, (4, 5)).unwrap();
        let mut doc = jiffshot_doc::Document::new(jiffshot_doc::Pixmap::new(20, 20).unwrap());
        add_cursor_object(&mut doc, &path);
        assert_eq!(doc.objects.len(), 1);
        let jiffshot_doc::object::Kind::Image { rect, .. } = &doc.objects[0].kind else {
            panic!("the cursor was not loaded as an image object");
        };
        assert_eq!(*rect, jiffshot_doc::Box2::from_xywh(4.0, 5.0, 3.0, 2.0));

        let mut meta = jiffshot_capture::library::CaptureMeta {
            kind: jiffshot_capture::library::CaptureKind::Region,
            rect: jiffshot_capture::Rect::new(0, 0, 20, 20),
            width: 20,
            height: 20,
            captured_at: jiffshot_capture::library::timestamp(),
            window_title: None,
            process: None,
            cursor_included: false,
            url: None,
            tags: Vec::new(),
            pinned: false,
        };
        jiffshot_capture::library::write_meta(&path, &meta).unwrap();
        let mut without_cursor =
            jiffshot_doc::Document::new(jiffshot_doc::Pixmap::new(20, 20).unwrap());
        add_cursor_object(&mut without_cursor, &path);
        assert!(without_cursor.objects.is_empty());
        meta.cursor_included = true;
        jiffshot_capture::library::write_meta(&path, &meta).unwrap();
        add_cursor_object(&mut without_cursor, &path);
        assert_eq!(without_cursor.objects.len(), 1);

        std::fs::write(
            path.with_extension("cursor.json"),
            "{\"x\":1.5,\"y\":2.25,\"width\":1.5,\"height\":0.75}",
        )
        .unwrap();
        let mut scaled = jiffshot_doc::Document::new(jiffshot_doc::Pixmap::new(20, 20).unwrap());
        add_cursor_object(&mut scaled, &path);
        assert_eq!(
            scaled.objects[0].rect().unwrap(),
            jiffshot_doc::Box2::from_xywh(1.5, 2.25, 1.5, 0.75)
        );
        std::fs::write(
            path.with_extension("cursor.json"),
            "{\"x\":1,\"y\":2,\"width\":1}",
        )
        .unwrap();
        let mut invalid = jiffshot_doc::Document::new(jiffshot_doc::Pixmap::new(20, 20).unwrap());
        add_cursor_object(&mut invalid, &path);
        assert!(invalid.objects.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pending_native_termination_joins_an_inflight_exit() {
        let mut current = None;
        adopt_pending_termination(&mut current, None).unwrap();
        adopt_pending_termination(&mut current, Some(41)).unwrap();
        assert_eq!(current, Some(41));
        adopt_pending_termination(&mut current, Some(41)).unwrap();
        assert!(adopt_pending_termination(&mut current, Some(42))
            .unwrap_err()
            .contains("changed"));
        assert!(adopt_pending_termination(&mut current, None)
            .unwrap_err()
            .contains("no longer pending"));
        assert_eq!(current, Some(41));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn shutdown_worker_start_failure_retains_the_work_for_retry() {
        let (release, blocked) = std::sync::mpsc::channel();
        let playback = std::thread::spawn(move || blocked.recv().unwrap());
        let error = spawn_shutdown_worker(
            vec![playback],
            |_| Err(std::io::Error::other("no worker")),
            |_| panic!("a failed worker must not run"),
        )
        .unwrap_err();
        assert_eq!(error.1.len(), 1);
        assert!(!error.1[0].is_finished());

        let error = spawn_shutdown_worker(
            error.1,
            |worker| {
                drop(worker);
                Ok(())
            },
            |_| panic!("a dropped worker must not run"),
        )
        .unwrap_err();
        assert_eq!(error.1.len(), 1);
        assert!(!error.1[0].is_finished());

        let (sender, receiver) = std::sync::mpsc::channel();
        spawn_shutdown_worker(
            error.1,
            |worker| std::thread::Builder::new().spawn(worker).map(|_| ()),
            move |workers| {
                for worker in workers {
                    worker.join().unwrap();
                }
                sender.send(()).unwrap();
            },
        )
        .unwrap();
        assert!(receiver
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err());
        release.send(()).unwrap();
        receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
    }

    #[test]
    fn auto_trim_finds_the_picture_inside_a_solid_border() {
        let mut pm = jiffshot_doc::Pixmap::new(40, 30).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        let mut paint = tiny_skia::Paint::default();
        paint.set_color_rgba8(200, 30, 30, 255);
        pm.fill_rect(
            tiny_skia::Rect::from_xywh(5.0, 7.0, 20.0, 10.0).unwrap(),
            &paint,
            tiny_skia::Transform::identity(),
            None,
        );
        assert_eq!(
            trim_bounds(&pm),
            Some(jiffshot_doc::IRect::new(5, 7, 20, 10))
        );
        pm.fill(tiny_skia::Color::WHITE);
        assert_eq!(trim_bounds(&pm), None, "one color all over");
    }

    #[cfg(windows)]
    #[test]
    fn recent_files_round_trip_through_uris() {
        let path = Path::new(r"C:\Users\me\Pictures\Jiffshot\2026-10\a capture #1 100%.png");
        let uri = file_uri(path);
        assert_eq!(
            uri,
            "file:///C:/Users/me/Pictures/Jiffshot/2026-10/a%20capture%20%231%20100%25.png"
        );
        assert_eq!(path_of_uri(&uri).unwrap(), path);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn recent_files_round_trip_mac_paths() {
        let path = Path::new("/Users/me/Pictures/Jiffshot/a capture #1 100% ? \\ café.png");
        let uri = file_uri(path);
        assert_eq!(uri, "file:///Users/me/Pictures/Jiffshot/a%20capture%20%231%20100%25%20%3F%20%5C%20caf%C3%A9.png");
        assert_eq!(path_of_uri(&uri).unwrap(), path);

        let relative = Path::new("relative capture/space %.png");
        assert_eq!(path_of_uri(&file_uri(relative)).unwrap(), relative);
        assert_eq!(
            path_of_uri("/Users/me/a plain path.png").unwrap(),
            Path::new("/Users/me/a plain path.png")
        );
        for invalid in [
            "file:///tmp/%",
            "file:///tmp/%é",
            "file:///tmp/%00",
            "file://remote/tmp/a.png",
        ] {
            assert!(path_of_uri(invalid).is_err(), "{invalid}");
        }
    }
}
