//! Every window, built from Rust with Rustolonia's code-first API: no AXAML,
//! no managed code. Windows bind to the signals of the view models in
//! [`crate::vm`] and send what the user does to their models.
//!
//! Event callbacks must be `Send`, but view models live on the UI thread:
//! callbacks reach the main and Capture windows' bindings through [`main`]
//! and [`capture`], and carry anything else in a [`UiOnly`].

#[path = "../../ui/canvas_view.rs"]
pub mod canvas_view;
mod capture_view;
mod dark_blue;
pub mod dialogs;
mod extras;
mod hud;
mod icons;
mod main_window;
#[cfg(windows)]
mod native_theme;
mod overlay;
mod prefs_window;
mod selftest;
mod toast;
mod tray;

pub use crate::ui_only::UiOnly;
use crate::vm::{
    CaptureBinding, CaptureViewModel, MainBinding, MainViewModel, TrayBinding, TrayViewModel,
};
use rustolonia::{AppScope, Result, Window};
use std::cell::RefCell;
use std::path::PathBuf;

pub(crate) fn selftest_failed() -> bool {
    selftest::failed()
}

#[cfg(target_os = "macos")]
pub(crate) fn check_native_popup_gate() -> std::io::Result<()> {
    hud::native_popup_macos::check_host_gate()
}

/// Translates the editor's shortcut vocabulary to the platform's command key.
pub(crate) fn shortcut(text: &str) -> String {
    #[cfg(target_os = "macos")]
    {
        text.replace("Ctrl+Y", "Cmd+Shift+Z").replace("Ctrl+", "Cmd+")
    }
    #[cfg(not(target_os = "macos"))]
    {
        text.to_owned()
    }
}

#[cfg(target_os = "macos")]
pub(crate) const COMMAND_MODIFIER: i32 = rustolonia::key_modifiers::META;
#[cfg(not(target_os = "macos"))]
pub(crate) const COMMAND_MODIFIER: i32 = rustolonia::key_modifiers::CONTROL;

thread_local! {
    static MAIN: RefCell<Option<MainBinding>> = const { RefCell::new(None) };
    static CAPTURE: RefCell<Option<CaptureBinding>> = const { RefCell::new(None) };
    static TRAY: RefCell<Option<TrayBinding>> = const { RefCell::new(None) };
    static MAIN_WINDOW: RefCell<Option<(AppScope, Window)>> = const { RefCell::new(None) };
    static CANVAS: RefCell<Option<canvas_view::CanvasView>> = const { RefCell::new(None) };
}

/// The editor window and its scope, once mounted. On the UI thread.
fn main_window() -> Option<Window> {
    MAIN_WINDOW.with(|m| m.borrow().as_ref().map(|(_, w)| w.clone()))
}

fn scope() -> Option<AppScope> {
    MAIN_WINDOW.with(|m| m.borrow().as_ref().map(|(s, _)| s.clone()))
}

/// The editor window's native handle (0 until first shown).
pub(crate) fn main_window_handle() -> Option<isize> {
    let (scope, window) = MAIN_WINDOW.with(|m| m.borrow().clone())?;
    scope.platform_handle(&window).ok().filter(|h| *h != 0)
}

#[cfg(target_os = "macos")]
pub(crate) fn forget_native_window(handle: isize) {
    hud::native::forget_cloak(handle);
}

/// The editor canvas, once the editor window is built. On the UI thread.
pub fn canvas() -> Option<canvas_view::CanvasView> {
    CANVAS.with(|c| c.borrow().clone())
}

fn set_canvas(canvas: canvas_view::CanvasView) {
    CANVAS.with(|c| *c.borrow_mut() = Some(canvas));
}

/// The editor window's binding. On the UI thread, once mounted.
pub fn main() -> MainBinding {
    MAIN.with(|m| m.borrow().clone())
        .expect("the main window is mounted")
}

/// The Capture window's binding. On the UI thread, once mounted.
pub fn capture() -> CaptureBinding {
    CAPTURE
        .with(|m| m.borrow().clone())
        .expect("the Capture window is mounted")
}

/// The program icon as a file, for window and tray icons (written once next
/// to the settings; the executable embeds the same image).
pub fn icon_file() -> String {
    static PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let dir = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("Jiffshot");
        let path = dir.join("jiffshot.ico");
        let bytes = include_bytes!("../../../../assets/jiffshot.ico");
        if std::fs::read(&path).ok().as_deref() != Some(&bytes[..]) {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(&path, bytes);
        }
        path.to_string_lossy().into_owned()
    })
    .clone()
}

/// The display name for a recent file URI or plain path.
pub(crate) fn recent_file_name(uri: &str) -> String {
    if uri.starts_with("file:") {
        let name = uri.rsplit('/').next().unwrap_or(uri);
        match crate::app::decode_uri_path(name) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(_) => name.to_owned(),
        }
    } else {
        std::path::Path::new(uri).file_name().unwrap_or_else(|| std::ffi::OsStr::new(uri)).to_string_lossy().into_owned()
    }
}

/// Mounts the editor window, shown or hidden. It is the application's main
/// window, so it must be mounted first.
pub fn mount_main_window(scope: &AppScope, model: impl MainViewModel, show: bool) -> Result<()> {
    let binding = MainBinding::new(model)?;
    MAIN.with(|m| *m.borrow_mut() = Some(binding.clone()));
    let window = main_window::build(scope, &binding)?;
    #[cfg(windows)]
    if std::env::var_os("JIFFSHOT_UITEST").is_some() {
        window.set_show_activated(false)?;
    }
    MAIN_WINDOW.with(|m| *m.borrow_mut() = Some((scope.clone(), window.clone())));
    if show {
        scope.mount(window)?;
    } else {
        scope.mount_hidden(window)?;
    }
    // The windows the main view model asks for.
    toast::install(scope)?;
    extras::install(scope)?;
    prefs_window::install(scope)?;
    overlay::install(scope)?;
    hud::install(scope)?;
    // JIFFSHOT_UITEST=DIR: the editor drives itself (see `selftest`).
    if let Some(dir) = std::env::var_os("JIFFSHOT_UITEST").filter(|d| !d.is_empty()) {
        let test_scope = scope.clone();
        scope.post(move || selftest::start(&test_scope, std::path::Path::new(&dir)))?;
    }
    Ok(())
}

/// Mounts the Capture window, hidden.
pub fn mount_capture_window(scope: &AppScope, model: impl CaptureViewModel) -> Result<Window> {
    let binding = CaptureBinding::new(model)?;
    CAPTURE.with(|m| *m.borrow_mut() = Some(binding.clone()));
    let window = capture_view::build(scope, &binding)?;
    scope.mount_hidden(window.clone())?;
    Ok(window)
}

/// Shows the tray icon.
pub fn mount_tray(scope: &AppScope, model: impl TrayViewModel) -> Result<()> {
    let binding = tray::mount(scope, model)?;
    TRAY.with(|t| *t.borrow_mut() = Some(binding));
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn selftest_before_native_termination() -> std::result::Result<(), String> {
    selftest::before_native_termination()
}

/// Detaches every view model (the editor saves its work), once the
/// application has stopped running, however it stopped.
pub fn detach_all() {
    #[cfg(target_os = "macos")]
    // The editor's final autosave can refresh the tray after the runtime stops.
    if let Some(tray) = TRAY.with(|t| t.borrow_mut().take()) {
        tray.model.run(|m| m.detach());
    }
    if let Some(main) = MAIN.with(|m| m.borrow_mut().take()) {
        main.model.run(|m| m.detach());
    }
    if let Some(capture) = CAPTURE.with(|c| c.borrow_mut().take()) {
        capture.model.run(|m| m.detach());
    }
    #[cfg(not(target_os = "macos"))]
    if let Some(tray) = TRAY.with(|t| t.borrow_mut().take()) {
        tray.model.run(|m| m.detach());
    }
    #[cfg(target_os = "macos")]
    {
        hud::native_popup_macos::after_run();
        selftest::after_run();
    }
}

/// Stops native popup protection before the application's platform is disposed.
pub(crate) fn shutdown(scope: &AppScope) -> std::result::Result<(), String> {
    #[cfg(target_os = "macos")]
    hud::native_popup_macos::prepare_shutdown(scope).map_err(|error| error.to_string())?;
    #[cfg(target_os = "macos")]
    if let Err(error) = crate::termination_macos::uninstall() {
        hud::native_popup_macos::cancel_shutdown();
        return Err(error.to_string());
    }
    if let Err(error) = scope.shutdown() {
        #[cfg(target_os = "macos")]
        {
            hud::native_popup_macos::cancel_shutdown();
            if let Err(restore) = crate::termination_macos::install(scope) {
                return Err(format!("{error}; could not restore macOS termination coordination: {restore}"));
            }
        }
        return Err(error.to_string());
    }
    Ok(())
}
