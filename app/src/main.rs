//! Jiffshot application entry points for the Windows and Linux builds.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod win;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(any(windows, target_os = "linux"))]
mod canvas;
#[cfg(any(windows, target_os = "linux"))]
mod library_tags;
#[cfg(any(windows, target_os = "linux"))]
mod trace;
#[cfg(any(windows, target_os = "linux"))]
mod ui_only;

#[cfg(windows)]
#[path = "win/ui/mod.rs"]
mod ui;

#[cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "linux/ui.rs"]
mod ui;

#[cfg(windows)]
pub(crate) use win::{
    app, capture_window, drag_out, library, ocr, prefs, settings, share, tray, video, vm,
    vm_support, winutil,
};

#[cfg(windows)]
fn main() -> rustolonia::Result<()> {
    win::run()
}

#[cfg(target_os = "linux")]
fn main() -> rustolonia::Result<()> {
    linux::run()
}

#[cfg(not(any(windows, target_os = "linux")))]
fn main() {
    eprintln!("Jiffshot currently supports Windows and Linux.");
}
