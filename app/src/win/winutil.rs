//! Small Win32 helpers: the editor window's handle, bringing it forward,
//! opening folders and asking the capture side for a capture.

use jiffshot_capture::Frame;
#[cfg(windows)]
use jiffshot_capture::wide;
use jiffshot_doc::Pixmap;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::atomic::{AtomicIsize, Ordering};
#[cfg(windows)]
use windows_sys::Win32::Foundation::HWND;
#[cfg(windows)]
use windows_sys::Win32::UI::Shell::ShellExecuteW;
#[cfg(windows)]
use windows_sys::Win32::UI::WindowsAndMessaging::{IsIconic, SetForegroundWindow, ShowWindow, SW_MINIMIZE, SW_RESTORE};
#[cfg(windows)]
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// The editor window's handle, set once it is mounted.
#[cfg(windows)]
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);

#[cfg(windows)]
pub fn set_main_hwnd(hwnd: isize) {
    MAIN_HWND.store(hwnd, Ordering::Release);
}

/// The editor's window (the capture side's windows share the process, so
/// this is the handle the host reported, not "the first visible window").
#[cfg(windows)]
pub fn main_hwnd() -> isize {
    MAIN_HWND.load(Ordering::Acquire)
}

/// macOS sharing and clipboard APIs do not need an owner window.
#[cfg(target_os = "macos")]
pub fn main_hwnd() -> isize {
    0
}

#[cfg(windows)]
pub fn bring_to_front() {
    bring_forward(main_hwnd());
}

#[cfg(target_os = "macos")]
pub fn bring_to_front() {
    if let Some(handle) = crate::ui::main_window_handle() {
        bring_forward(handle);
    }
}

/// Restores `hwnd` when minimized and makes it the foreground window.
#[cfg(windows)]
pub fn bring_forward(hwnd: isize) {
    let hwnd = hwnd as HWND;
    if hwnd.is_null() {
        return;
    }
    // SAFETY: plain Win32 calls on our own window.
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd);
    }
}

#[cfg(target_os = "macos")]
pub fn bring_forward(handle: isize) {
    if handle != 0 {
        if let Err(error) = crate::mac_window::activate(handle) {
            eprintln!("Could not activate the window: {error}");
        }
    }
}

#[cfg(windows)]
pub fn minimize() {
    let hwnd = main_hwnd() as HWND;
    if !hwnd.is_null() {
        // SAFETY: plain Win32 call on our own window.
        unsafe { ShowWindow(hwnd, SW_MINIMIZE) };
    }
}

#[cfg(target_os = "macos")]
pub fn minimize() {
    if let Some(window) = crate::ui::main_window_handle().and_then(crate::mac_window::window) {
        window.miniaturize(None);
    }
}

#[cfg(windows)]
fn shell(verb: &str, file: &Path, args: Option<&str>) -> bool {
    let verb = wide(verb);
    let file = wide(&file.to_string_lossy());
    let args = args.map(wide);
    // SAFETY: NUL-terminated strings that outlive the call.
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            args.as_ref().map_or(std::ptr::null(), |a| a.as_ptr()),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    r as isize > 32
}

/// Opens Explorer with `path` selected.
#[cfg(windows)]
pub fn reveal(path: &Path) {
    let _ = std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn();
}

#[cfg(target_os = "macos")]
pub(crate) fn file_url(path: &Path) -> Result<objc2::rc::Retained<objc2_foundation::NSURL>, String> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| "The file path contains a NUL byte.".to_owned())?;
    // SAFETY: CString supplies a non-null NUL-terminated filesystem path.
    Ok(unsafe {
        objc2_foundation::NSURL::fileURLWithFileSystemRepresentation_isDirectory_relativeToURL(
            std::ptr::NonNull::new(path.as_ptr() as *mut _).expect("CString pointers are non-null"),
            false,
            None,
        )
    })
}

/// Opens Finder with `path` selected.
#[cfg(target_os = "macos")]
pub fn reveal(path: &Path) {
    match file_url(path) {
        Ok(url) => objc2_app_kit::NSWorkspace::sharedWorkspace()
            .activateFileViewerSelectingURLs(&objc2_foundation::NSArray::from_retained_slice(&[url])),
        Err(error) => eprintln!("Could not reveal {}: {error}", path.display()),
    }
}

#[cfg(windows)]
pub fn open_folder(path: &Path) {
    shell("explore", path, None);
}

#[cfg(target_os = "macos")]
pub fn open_folder(path: &Path) {
    match file_url(path) {
        Ok(url) if objc2_app_kit::NSWorkspace::sharedWorkspace().openURL(&url) => {}
        Ok(_) => eprintln!("Could not open folder {}", path.display()),
        Err(error) => eprintln!("Could not open folder {}: {error}", path.display()),
    }
}

/// Opens the system print dialog for a file.
#[cfg(windows)]
pub fn print_file(path: &Path) -> bool {
    shell("print", path, None)
}

/// Moves files to the Recycle Bin (so a mistaken delete can be undone).
#[cfg(windows)]
pub fn recycle(files: &[PathBuf]) -> bool {
    use windows_sys::Win32::UI::Shell::{SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_SILENT, FO_DELETE, SHFILEOPSTRUCTW};
    if files.is_empty() {
        return true;
    }
    // A double-NUL-terminated list of paths.
    let mut list: Vec<u16> = Vec::new();
    for f in files {
        list.extend(f.to_string_lossy().encode_utf16());
        list.push(0);
    }
    list.push(0);
    let mut op = SHFILEOPSTRUCTW {
        hwnd: main_hwnd() as HWND,
        wFunc: FO_DELETE,
        pFrom: list.as_ptr(),
        pTo: std::ptr::null(),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT) as u16,
        fAnyOperationsAborted: 0,
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: std::ptr::null(),
    };
    // SAFETY: the list and struct outlive the call.
    unsafe { SHFileOperationW(&mut op) == 0 && op.fAnyOperationsAborted == 0 }
}

/// Moves files to the Trash (so a mistaken delete can be undone).
#[cfg(target_os = "macos")]
pub fn recycle(files: &[PathBuf]) -> bool {
    let manager = objc2_foundation::NSFileManager::defaultManager();
    for path in files {
        let url = match file_url(path) {
            Ok(url) => url,
            Err(error) => {
                eprintln!("Could not move {} to the Trash: {error}", path.display());
                return false;
            }
        };
        if let Err(error) = manager.trashItemAtURL_resultingItemURL_error(&url, None) {
            eprintln!("Could not move {} to the Trash: {}", path.display(), error.localizedDescription());
            return false;
        }
    }
    true
}

/// Starts a capture: the editor gets out of the way, then the capture side
/// opens its overlay and hands the result back.
pub fn start_capture() -> Result<(), String> {
    minimize();
    jiffshot_workflow::request_capture(jiffshot_workflow::MODE_IMAGE, 250);
    Ok(())
}

/// Starts a screen recording.
pub fn start_recording() -> Result<(), String> {
    minimize();
    jiffshot_workflow::request_capture(jiffshot_workflow::MODE_VIDEO, 250);
    Ok(())
}

/// Opens the Capture window over the editor.
pub fn open_capture_window() -> Result<(), String> {
    jiffshot_workflow::request_capture(jiffshot_workflow::OPEN_CAPTURE_WINDOW, 1);
    Ok(())
}

/// The flattened document as an opaque frame (composited over white) for
/// the Windows clipboard.
#[cfg(windows)]
pub fn frame_from_pixmap(pm: &Pixmap) -> Frame {
    let mut frame = Frame::new(pm.width(), pm.height());
    for (dst, src) in frame.pixels.chunks_exact_mut(4).zip(pm.pixels()) {
        let inv = 255 - src.alpha() as u32;
        let over = |c: u8| (c as u32 + inv).min(255) as u8;
        dst.copy_from_slice(&[over(src.blue()), over(src.green()), over(src.red()), 255]);
    }
    frame
}

/// Preserves transparency when copying an editor image on macOS.
#[cfg(target_os = "macos")]
pub fn frame_from_pixmap(pm: &Pixmap) -> Frame {
    jiffshot_workflow::frame_from_pixmap(pm)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn editor_clipboard_frame_preserves_straight_alpha() {
        let rgba = image::RgbaImage::from_raw(2, 2, vec![
            255, 0, 0, 128, 0, 0, 255, 255,
            0, 255, 0, 0, 0, 255, 0, 64,
        ]).unwrap();
        let pixmap = jiffshot_doc::io::from_rgba_image(&rgba).unwrap();
        let frame = frame_from_pixmap(&pixmap);
        assert!(frame.alpha);
        assert_eq!(frame.pixels, [
            0, 0, 255, 128, 255, 0, 0, 255,
            0, 0, 0, 0, 0, 255, 0, 64,
        ]);
    }
}
