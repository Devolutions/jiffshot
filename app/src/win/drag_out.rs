//! Dragging captures from the tray and the library into other apps.
//!
//! Rustolonia has no outgoing drag and drop, nor press and move events, so
//! tiles report which capture is under the pointer and a subclass of the
//! main window starts a shell drag when the left button is dragged from it.

use crate::{app, winutil};
use std::path::PathBuf;
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXDRAG, SM_CYDRAG, WM_CAPTURECHANGED, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE};

const MK_LBUTTON: WPARAM = 0x0001;

/// The capture whose tile is under the pointer.
static HOVERED: Mutex<Option<PathBuf>> = Mutex::new(None);
/// Where the left button went down on a tile, and its capture.
static PRESSED: Mutex<Option<(i32, i32, PathBuf)>> = Mutex::new(None);

pub fn hover(path: &str) {
    *HOVERED.lock().unwrap() = (!path.is_empty()).then(|| PathBuf::from(path));
}

pub fn leave(path: &str) {
    let mut hovered = HOVERED.lock().unwrap();
    if hovered.as_deref().is_some_and(|p| p.as_os_str() == path) {
        *hovered = None;
    }
}

pub fn install(hwnd: isize) {
    if hwnd != 0 {
        unsafe { SetWindowSubclass(hwnd as HWND, Some(subclass), 0x4A53_4447, 0) };
    }
}

fn point(lparam: LPARAM) -> (i32, i32) {
    ((lparam & 0xFFFF) as i16 as i32, ((lparam >> 16) & 0xFFFF) as i16 as i32)
}

/// Whether the pointer moved far enough from `from` to start a drag.
fn past_threshold(from: (i32, i32), to: (i32, i32), slop: (i32, i32)) -> bool {
    (to.0 - from.0).abs() > slop.0 || (to.1 - from.1).abs() > slop.1
}

unsafe extern "system" fn subclass(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, _data: usize) -> LRESULT {
    let result = DefSubclassProc(hwnd, msg, wparam, lparam);
    match msg {
        WM_LBUTTONDOWN => {
            let hovered = HOVERED.lock().unwrap().clone();
            *PRESSED.lock().unwrap() = hovered.map(|p| {
                let (x, y) = point(lparam);
                (x, y, p)
            });
        }
        WM_LBUTTONUP | WM_CAPTURECHANGED => {
            // Avalonia releases capture itself on button up; only a capture
            // taken by someone else cancels the press.
            if msg == WM_LBUTTONUP || lparam != hwnd as LPARAM {
                *PRESSED.lock().unwrap() = None;
            }
        }
        WM_MOUSEMOVE if wparam & MK_LBUTTON != 0 => {
            let slop = (GetSystemMetrics(SM_CXDRAG), GetSystemMetrics(SM_CYDRAG));
            let start = {
                let mut pressed = PRESSED.lock().unwrap();
                match pressed.as_ref() {
                    Some((x, y, _)) if past_threshold((*x, *y), point(lparam), slop) => pressed.take().map(|(_, _, p)| p),
                    _ => None,
                }
            };
            if let Some(path) = start {
                drag(path);
            }
        }
        _ => {}
    }
    result
}

fn drag(path: PathBuf) {
    // The app state is released before the modal drag loop starts.
    let files = app::with(|app| app.drag_files(&path)).unwrap_or_default();
    if files.is_empty() {
        return;
    }
    if let Err(error) = jiffshot_capture::drag::drag_files(winutil::main_hwnd(), &files) {
        let _ = app::act(move |app| app.set_status(format!("Could not drag the capture: {error}")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_moves_do_not_start_a_drag() {
        assert!(!past_threshold((10, 10), (14, 6), (4, 4)));
        assert!(past_threshold((10, 10), (15, 10), (4, 4)));
        assert!(past_threshold((10, 10), (10, 5), (4, 4)));
    }

    #[test]
    fn points_are_signed() {
        let lparam = ((-3i16 as u16 as isize) << 16) | (-7i16 as u16 as isize);
        assert_eq!(point(lparam), (-7, -3));
    }

    #[test]
    fn leaving_another_tile_keeps_the_hover() {
        hover("a.png");
        leave("b.png");
        assert_eq!(HOVERED.lock().unwrap().as_deref(), Some(std::path::Path::new("a.png")));
        leave("a.png");
        assert!(HOVERED.lock().unwrap().is_none());
    }
}
