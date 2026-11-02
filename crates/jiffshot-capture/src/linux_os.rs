//! Linux counterparts of the small OS services the shared library code uses.

use std::path::PathBuf;

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
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// `~/Pictures`.
pub fn pictures_dir() -> Option<PathBuf> {
    home_dir().map(|home| home.join("Pictures"))
}

/// How text lines end on this system (clipboard text, recognized text).
pub const LINE_END: &str = "\n";

/// Opens a file or folder with the desktop's default application.
pub fn open_path(path: &std::path::Path) -> std::io::Result<()> {
    std::process::Command::new("xdg-open")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}
