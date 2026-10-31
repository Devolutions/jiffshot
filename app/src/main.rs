//! Jiffshot application entry points for the Windows and Linux builds.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
include!("main_windows.rs");

#[cfg(target_os = "linux")]
include!("main_linux.rs");

#[cfg(not(any(windows, target_os = "linux")))]
fn main() {
    eprintln!("Jiffshot currently supports Windows and Linux.");
}
