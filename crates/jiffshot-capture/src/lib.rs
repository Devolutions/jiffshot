//! Screen capture primitives for Jiffshot's capture side and editor.
//!
//! Everything here works in physical screen pixels. Windows callers must make
//! the process per-monitor DPI aware before capturing.

#[cfg(windows)]
pub mod clipboard;
#[cfg(windows)]
pub mod config;
#[cfg(windows)]
pub mod drag;
pub mod frame;
#[cfg(windows)]
pub mod gdi;
pub mod geom;
#[cfg(windows)]
pub mod ipc;
#[cfg(windows)]
pub mod library;
#[cfg(windows)]
pub mod media;
#[cfg(windows)]
pub mod ocr;
#[cfg(windows)]
pub mod paths;
#[cfg(windows)]
pub mod screen;
#[cfg(windows)]
pub mod share;
pub mod stitch;
#[cfg(windows)]
pub mod targets;

pub use frame::Frame;
pub use geom::Rect;

/// Encodes `s` as a NUL-terminated UTF-16 string for Win32 calls.
#[cfg(windows)]
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Decodes a UTF-16 buffer up to its first NUL.
#[cfg(windows)]
pub fn from_wide(buffer: &[u16]) -> String {
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}
