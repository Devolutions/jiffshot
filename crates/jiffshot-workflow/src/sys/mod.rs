//! The window-message plumbing the capture side runs on: per-thread message
//! queues, timers, global hotkeys and a few input calls, with Win32's names
//! and semantics. On Windows these are the real calls; on macOS `mac`
//! emulates them (queues and timers in-process, hotkeys through Carbon's
//! `RegisterEventHotKey`, input through Quartz events), so the tray, overlay
//! and panel loops are the same code on both.

#![allow(non_snake_case, non_camel_case_types, clippy::missing_safety_doc)]

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::*;

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
pub use mac::*;

/// Whether `vk` is held down now.
pub fn key_down(vk: u16) -> bool {
    // SAFETY: a plain key-state query.
    unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
}

/// The mouse position in desktop pixels.
pub fn cursor_pos() -> (i32, i32) {
    let mut p = POINT { x: 0, y: 0 };
    // SAFETY: out-parameter only.
    unsafe { GetCursorPos(&mut p) };
    (p.x, p.y)
}

/// Moves the mouse pointer to a desktop-pixel point.
pub fn set_cursor_pos(x: i32, y: i32) {
    // SAFETY: a plain call.
    unsafe { SetCursorPos(x, y) };
}

/// Posts `msg` to the hidden window of `class` (in this process); false when
/// there is none or the post failed.
pub fn post_to_class(class: &str, msg: u32, wparam: WPARAM, lparam: LPARAM) -> bool {
    let hwnd = find_window(class);
    // SAFETY: a post to our own window, found by class.
    !hwnd.is_null() && unsafe { PostMessageW(hwnd, msg, wparam, lparam) } != 0
}
