//! Small Windows services the portable modules need.

use std::path::PathBuf;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::UI::Shell::{FOLDERID_Pictures, SHGetKnownFolderPath};

/// Local wall-clock time as `(year, month, day, hour, minute, second)`.
pub fn local_time() -> (u16, u16, u16, u16, u16, u16) {
    // SAFETY: out-parameter only.
    let t = unsafe {
        let mut t = std::mem::zeroed();
        GetLocalTime(&mut t);
        t
    };
    (t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

/// The user's Pictures folder, following OneDrive or other redirection.
pub fn pictures_dir() -> Option<PathBuf> {
    // SAFETY: the returned string is freed with CoTaskMemFree as
    // documented, after being copied.
    let pictures = unsafe {
        let mut path = std::ptr::null_mut();
        let hr = SHGetKnownFolderPath(&FOLDERID_Pictures, 0, std::ptr::null_mut(), &mut path);
        let result = if hr == 0 && !path.is_null() {
            let len = (0..).take_while(|&i| *path.add(i) != 0).count();
            Some(PathBuf::from(String::from_utf16_lossy(std::slice::from_raw_parts(path, len))))
        } else {
            None
        };
        CoTaskMemFree(path as *const _);
        result
    };
    pictures.or_else(|| std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join("Pictures")))
}

/// How text lines end on this system (clipboard text, recognized text).
pub const LINE_END: &str = "\r\n";
