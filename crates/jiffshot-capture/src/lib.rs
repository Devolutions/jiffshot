//! Screen capture primitives for Jiffshot's capture side and editor.
//!
//! The cross-platform desktop geometry uses physical screen pixels on Windows
//! and a reference grid on macOS, where one point is multiplied by the largest
//! attached backing scale. Windows callers must enable per-monitor DPI
//! awareness (see [`screen::enable_per_monitor_dpi`]). macOS also exposes
//! precise logical capture bounds independent of that compatibility grid.
//!
//! The native modules have one backend per platform with the same public
//! API: `win/` for Windows, `mac/` for macOS. Linux builds use only the
//! portable [`frame`], [`geom`] and [`stitch`] modules; the Linux application
//! has its own capture backend.

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
#[cfg_attr(windows, path = "win/clipboard.rs")]
#[cfg_attr(target_os = "macos", path = "mac/clipboard.rs")]
#[cfg_attr(target_os = "linux", path = "linux_clipboard.rs")]
pub mod clipboard;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
pub mod config;
#[cfg(windows)]
#[path = "win/drag.rs"]
pub mod drag;
pub mod frame;
#[cfg(windows)]
#[path = "win/gdi.rs"]
pub mod gdi;
pub mod geom;
#[cfg(any(windows, target_os = "macos"))]
#[cfg_attr(windows, path = "win/ipc.rs")]
#[cfg_attr(target_os = "macos", path = "mac/ipc.rs")]
pub mod ipc;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
pub mod library;
#[cfg(target_os = "macos")]
#[path = "mac/mod.rs"]
pub mod macos;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
pub mod media;
#[cfg(any(windows, target_os = "macos"))]
pub mod ocr;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
#[cfg_attr(windows, path = "win/os.rs")]
#[cfg_attr(target_os = "macos", path = "mac/os.rs")]
#[cfg_attr(target_os = "linux", path = "linux_os.rs")]
pub mod os;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
pub mod paths;
#[cfg(any(windows, target_os = "macos"))]
#[cfg_attr(windows, path = "win/screen.rs")]
#[cfg_attr(target_os = "macos", path = "mac/screen.rs")]
pub mod screen;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
#[cfg_attr(windows, path = "win/share.rs")]
#[cfg_attr(target_os = "macos", path = "mac/share.rs")]
#[cfg_attr(target_os = "linux", path = "linux_share.rs")]
pub mod share;
pub mod stitch;
pub mod visibility;
#[cfg(any(windows, target_os = "macos"))]
#[cfg_attr(windows, path = "win/targets.rs")]
#[cfg_attr(target_os = "macos", path = "mac/targets.rs")]
pub mod targets;

pub use frame::Frame;
pub use geom::{CaptureMapping, Rect};

/// Encodes `s` as a NUL-terminated UTF-16 string for Win32 calls.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Encodes an OS path as NUL-terminated UTF-16 without lossy conversion.
#[cfg(windows)]
pub fn wide_path(path: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Decodes a NUL-terminated UTF-16 path without replacing unpaired surrogates.
#[cfg(windows)]
pub fn path_from_wide(buffer: &[u16]) -> std::path::PathBuf {
    use std::os::windows::ffi::OsStringExt;
    let len = buffer.iter().position(|&unit| unit == 0).unwrap_or(buffer.len());
    std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer[..len]))
}

/// Decodes a UTF-16 buffer up to its first NUL.
pub fn from_wide(buffer: &[u16]) -> String {
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStringExt;

    #[test]
    fn windows_paths_round_trip_unpaired_utf16() {
        let path = std::path::PathBuf::from(std::ffi::OsString::from_wide(&[
            b'C' as u16,
            b':' as u16,
            b'\\' as u16,
            0xD800,
            b'.' as u16,
            b'p' as u16,
            b'n' as u16,
            b'g' as u16,
        ]));
        assert_eq!(path_from_wide(&wide_path(&path)), path);
    }
}
