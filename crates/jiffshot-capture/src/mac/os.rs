//! Small macOS services the portable modules need.

use std::path::PathBuf;
pub use crate::macos::picker::{choose_application_path, choose_open_paths, choose_save_path};

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightPostEventAccess() -> bool;
    fn CGRequestPostEventAccess() -> bool;
}

/// Whether Jiffshot may post input events to other applications.
pub fn event_posting_allowed() -> bool {
    // SAFETY: this is a plain permission query.
    unsafe { CGPreflightPostEventAccess() }
}

/// Requests the permission needed to send input events to another app.
pub fn require_event_posting() -> std::io::Result<()> {
    // SAFETY: both functions are plain permission queries; the request may
    // show the macOS permission prompt.
    if unsafe { CGPreflightPostEventAccess() || CGRequestPostEventAccess() } {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Input automation and automatic scrolling need Accessibility permission. Allow Jiffshot in System Settings > Privacy & Security > Accessibility, then retry.",
        ))
    }
}

/// Local wall-clock time as `(year, month, day, hour, minute, second)`.
pub fn local_time() -> (u16, u16, u16, u16, u16, u16) {
    // SAFETY: `time` and `localtime_r` only write the out-parameters given.
    let tm = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm
    };
    (
        (tm.tm_year + 1900) as u16,
        (tm.tm_mon + 1) as u16,
        tm.tm_mday as u16,
        tm.tm_hour as u16,
        tm.tm_min as u16,
        tm.tm_sec.min(59) as u16,
    )
}

/// The user's home folder.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// `~/Pictures`.
pub fn pictures_dir() -> Option<PathBuf> {
    home_dir().map(|home| home.join("Pictures"))
}

/// How text lines end on this system (clipboard text, recognized text).
pub const LINE_END: &str = "\n";

/// Opens a file or folder the way Finder would.
pub fn open_path(path: &std::path::Path) -> std::io::Result<()> {
    std::process::Command::new("/usr/bin/open").arg(path).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().map(|_| ())
}
