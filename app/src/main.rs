//! Jiffshot application entry points for the Windows, macOS and Linux builds.
//!
//! Windows and macOS share the Avalonia desktop front end in `win/` (with
//! macOS-only integration modules here); Linux has its own in `linux/`.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(any(windows, target_os = "macos"))]
mod win;

#[cfg(target_os = "linux")]
mod linux;

mod canvas;
mod library_tags;
#[cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "win/settings.rs"]
mod settings;
#[cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "win/winutil.rs"]
mod winutil;
#[cfg(target_os = "linux")]
#[allow(dead_code, clippy::wrong_self_convention, clippy::type_complexity, clippy::items_after_test_module)]
#[path = "win/vm.rs"]
mod vm;
#[cfg(target_os = "linux")]
#[allow(dead_code, clippy::wrong_self_convention, clippy::type_complexity, clippy::items_after_test_module)]
#[path = "win/vm_support.rs"]
mod vm_support;
#[cfg(target_os = "linux")]
#[allow(dead_code, clippy::all)]
#[path = "win/library.rs"]
mod library;
#[cfg(target_os = "linux")]
#[allow(dead_code, clippy::all)]
#[path = "win/video.rs"]
mod video;
#[cfg(target_os = "linux")]
#[allow(dead_code, clippy::all)]
#[path = "win/app.rs"]
mod app;
#[cfg(target_os = "linux")]
#[allow(dead_code, clippy::all)]
#[path = "win/ocr.rs"]
mod ocr;
#[cfg(target_os = "linux")]
#[allow(dead_code, clippy::all)]
#[path = "win/share.rs"]
mod share;
#[cfg(target_os = "linux")]
#[allow(dead_code, clippy::all)]
#[path = "win/main.rs"]
mod shared_main;
mod trace;
mod ui_only;

#[cfg(target_os = "macos")]
mod capture_engine_macos;
#[cfg(target_os = "macos")]
mod file_drag_macos;
#[cfg(target_os = "macos")]
mod library_transfer_macos;
#[cfg(target_os = "macos")]
mod mac_host;
#[cfg(target_os = "macos")]
mod mac_window;
#[cfg(target_os = "macos")]
mod shutdown_ui_macos;
#[cfg(target_os = "macos")]
mod termination_macos;

#[cfg(any(windows, target_os = "macos"))]
#[path = "win/ui/mod.rs"]
mod ui;

#[cfg(target_os = "linux")]
#[allow(dead_code, clippy::wrong_self_convention, clippy::type_complexity, clippy::items_after_test_module)]
#[path = "win/ui/mod.rs"]
mod ui;

#[cfg(windows)]
pub(crate) use win::drag_out;
#[cfg(any(windows, target_os = "macos"))]
pub(crate) use win::{
    app, capture_window, library, ocr, prefs, settings, share, tray, video, vm, vm_support,
    winutil,
};

#[cfg(any(windows, target_os = "macos"))]
fn main() -> rustolonia::Result<()> {
    win::run()
}

#[cfg(target_os = "linux")]
fn main() -> rustolonia::Result<()> {
    linux::run()
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn main() {
    eprintln!("Jiffshot currently supports Windows, macOS and Linux.");
}
