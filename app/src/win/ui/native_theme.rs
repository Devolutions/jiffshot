//! Native Windows title-bar colors for the editor.

use super::{capture_view::parts, dark_blue};
use rustolonia::{AppScope, Window};
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_COLOR_DEFAULT, DWMWA_TEXT_COLOR,
};

static REPORTED_FAILURE: AtomicBool = AtomicBool::new(false);

fn caption_colors(dark: bool) -> (u32, u32) {
    if dark {
        let rgb = dark_blue::CHROME;
        (
            ((rgb & 0xFF) << 16) | (rgb & 0xFF00) | ((rgb >> 16) & 0xFF),
            dark_blue::TEXT & 0x00FF_FFFF,
        )
    } else {
        (DWMWA_COLOR_DEFAULT, DWMWA_COLOR_DEFAULT)
    }
}

fn set_color(hwnd: HWND, attribute: u32, color: u32) -> i32 {
    // SAFETY: hwnd belongs to this window, and DWM reads the color during the call.
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            attribute,
            (&color as *const u32).cast(),
            std::mem::size_of_val(&color) as u32,
        )
    }
}

/// Applies the current theme once the window has a native handle.
pub fn apply(scope: &AppScope, window: &Window) {
    let Some(handle) = parts::hwnd(scope, window) else {
        return;
    };
    let hwnd = handle as HWND;
    let (caption, text) = caption_colors(parts::dark_theme(scope));
    let status = set_color(hwnd, DWMWA_CAPTION_COLOR as u32, caption);
    if status < 0 && !REPORTED_FAILURE.swap(true, Ordering::Relaxed) {
        eprintln!(
            "Windows title-bar color unavailable (HRESULT {:#010x}); using the system title bar",
            status as u32
        );
    }
    if status < 0 {
        return;
    }
    let status = set_color(hwnd, DWMWA_TEXT_COLOR as u32, text);
    if status < 0 {
        // Avoid leaving an unreadable system text color over a custom background.
        let reset_caption = set_color(hwnd, DWMWA_CAPTION_COLOR as u32, DWMWA_COLOR_DEFAULT);
        let reset_text = set_color(hwnd, DWMWA_TEXT_COLOR as u32, DWMWA_COLOR_DEFAULT);
        if !REPORTED_FAILURE.swap(true, Ordering::Relaxed) {
            eprintln!(
                "Windows title-bar text color unavailable (HRESULT {:#010x}); system-color restore HRESULTs: {:#010x}, {:#010x}",
                status as u32, reset_caption as u32, reset_text as u32
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caption_uses_readable_dark_colors_and_system_defaults_for_light() {
        assert_eq!(caption_colors(true), (0x001D110C, 0x00FFFFFF));
        assert_eq!(
            caption_colors(false),
            (DWMWA_COLOR_DEFAULT, DWMWA_COLOR_DEFAULT)
        );
    }
}
