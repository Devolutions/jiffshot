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
    let file = jiffshot_capture::wide_path(file);
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
pub fn reveal(path: &Path) -> Result<(), String> {
    use windows_sys::Win32::Foundation::RPC_E_CHANGED_MODE;
    use windows_sys::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows_sys::Win32::UI::Shell::SHOpenFolderAndSelectItems;

    let item = ShellItem::parse(path)?;
    // SAFETY: initializes COM on this thread; an existing apartment is also usable.
    let initialized =
        unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
    if initialized < 0 && initialized != RPC_E_CHANGED_MODE {
        return Err(format!(
            "Could not initialize the Windows shell (HRESULT {initialized:#010x})"
        ));
    }
    // With no child items, the shell selects this absolute item in its parent folder.
    // SAFETY: item owns a valid absolute PIDL, which outlives the synchronous call.
    let result = unsafe { SHOpenFolderAndSelectItems(item.0, 0, std::ptr::null(), 0) };
    if initialized >= 0 {
        // SAFETY: balances this call's successful COM initialization.
        unsafe { CoUninitialize() };
    }
    if result < 0 {
        return Err(format!(
            "Could not select the capture in Explorer (HRESULT {result:#010x})"
        ));
    }
    Ok(())
}

#[cfg(windows)]
struct ShellItem(*mut windows_sys::Win32::UI::Shell::Common::ITEMIDLIST);

#[cfg(windows)]
impl ShellItem {
    fn parse(path: &Path) -> Result<Self, String> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::UI::Shell::{ILCreateFromPathW, PathCchStripPrefix};

        path.metadata().map_err(|error| error.to_string())?;
        let absolute = std::path::absolute(path).map_err(|error| error.to_string())?;
        let mut path: Vec<u16> = absolute.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: path is a writable, NUL-terminated buffer with the supplied capacity.
        // Explorer needs normal DOS/UNC paths, not canonicalize's extended-length prefix.
        let result = unsafe { PathCchStripPrefix(path.as_mut_ptr(), path.len()) };
        if result < 0 {
            return Err(format!(
                "Could not prepare the capture's shell path (HRESULT {result:#010x})"
            ));
        }
        // SAFETY: path is a NUL-terminated absolute filesystem path.
        let item = unsafe { ILCreateFromPathW(path.as_ptr()) };
        if item.is_null() {
            return Err("Could not resolve the capture's Windows shell location".to_owned());
        }
        Ok(Self(item))
    }
}

#[cfg(windows)]
impl Drop for ShellItem {
    fn drop(&mut self) {
        // SAFETY: this PIDL was allocated by ILCreateFromPathW and is freed exactly once.
        unsafe { windows_sys::Win32::UI::Shell::ILFree(self.0) };
    }
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
pub fn reveal(path: &Path) -> Result<(), String> {
    let url = file_url(path)?;
    objc2_app_kit::NSWorkspace::sharedWorkspace()
        .activateFileViewerSelectingURLs(&objc2_foundation::NSArray::from_retained_slice(&[url]));
    Ok(())
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

#[cfg(windows)]
pub fn open_url(url: &str) -> Result<(), String> {
    let verb = wide("open");
    let url = wide(url);
    // SAFETY: NUL-terminated strings outlive the call.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            url.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result as isize > 32 {
        Ok(())
    } else {
        Err(format!("Windows shell returned {}", result as isize))
    }
}

#[cfg(target_os = "macos")]
pub fn open_url(url: &str) -> Result<(), String> {
    use objc2_foundation::{NSString, NSURL};

    let url = NSURL::URLWithString(&NSString::from_str(url)).ok_or_else(|| "The URL is invalid.".to_owned())?;
    if objc2_app_kit::NSWorkspace::sharedWorkspace().openURL(&url) {
        Ok(())
    } else {
        Err("The system could not open the URL.".to_owned())
    }
}

#[cfg(target_os = "linux")]
pub fn open_url(url: &str) -> Result<(), String> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Could not start xdg-open: {error}"))
}

/// Opens the system print dialog for a file.
#[cfg(windows)]
pub fn print_file(path: &Path) -> bool {
    shell("print", path, None)
}

#[cfg(windows)]
fn recycle_path_list(files: &[PathBuf]) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    let mut list = Vec::new();
    for file in files {
        list.extend(file.as_os_str().encode_wide());
        list.push(0);
    }
    list.push(0);
    list
}

/// Moves files to the Recycle Bin (so a mistaken delete can be undone).
#[cfg(windows)]
pub fn recycle(files: &[PathBuf]) -> bool {
    use windows_sys::Win32::UI::Shell::{SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_SILENT, FO_DELETE, SHFILEOPSTRUCTW};

    if files.is_empty() {
        return true;
    }
    // A double-NUL-terminated list of paths.
    let list = recycle_path_list(files);
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

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStringExt;

    #[test]
    fn recycle_paths_preserve_unpaired_utf16() {
        let path = PathBuf::from(std::ffi::OsString::from_wide(&[
            b'C' as u16,
            b':' as u16,
            b'\\' as u16,
            0xD800,
            b'.' as u16,
            b'p' as u16,
            b'n' as u16,
            b'g' as u16,
        ]));
        assert_eq!(
            recycle_path_list(&[path]),
            [
                b'C' as u16,
                b':' as u16,
                b'\\' as u16,
                0xD800,
                b'.' as u16,
                b'p' as u16,
                b'n' as u16,
                b'g' as u16,
                0,
                0,
            ]
        );
    }

    #[test]
    fn recycle_paths_use_a_double_terminated_path_list() {
        let paths = [PathBuf::from("first"), PathBuf::from("second")];
        let expected: Vec<u16> = "first\0second\0\0".encode_utf16().collect();
        assert_eq!(recycle_path_list(&paths), expected);
    }
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

/// What the editor asks the capture side for.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureRequest {
    Image,
    Video,
    CaptureWindow,
}

#[cfg(target_os = "linux")]
type CaptureHandler = Box<dyn Fn(CaptureRequest) + Send + Sync>;

#[cfg(target_os = "linux")]
static CAPTURE_HANDLER: std::sync::Mutex<Option<CaptureHandler>> = std::sync::Mutex::new(None);

/// Registers the Linux capture flows that serve the editor's requests.
#[cfg(target_os = "linux")]
pub fn set_capture_handler(handler: impl Fn(CaptureRequest) + Send + Sync + 'static) {
    *CAPTURE_HANDLER.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(Box::new(handler));
}

#[cfg(target_os = "linux")]
fn request(request: CaptureRequest) -> Result<(), String> {
    match CAPTURE_HANDLER.lock().unwrap_or_else(|poison| poison.into_inner()).as_ref() {
        Some(handler) => {
            handler(request);
            Ok(())
        }
        None => Err("capture is not ready yet".to_owned()),
    }
}

/// Starts a capture: the editor gets out of the way, then the capture side
/// opens its overlay and hands the result back.
pub fn start_capture() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    return request(CaptureRequest::Image);
    #[cfg(not(target_os = "linux"))]
    {
        minimize();
        jiffshot_workflow::request_capture(jiffshot_workflow::MODE_IMAGE, 250);
        Ok(())
    }
}

/// Starts a screen recording.
pub fn start_recording() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    return request(CaptureRequest::Video);
    #[cfg(not(target_os = "linux"))]
    {
        minimize();
        jiffshot_workflow::request_capture(jiffshot_workflow::MODE_VIDEO, 250);
        Ok(())
    }
}

/// Opens the Capture window over the editor.
pub fn open_capture_window() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    return request(CaptureRequest::CaptureWindow);
    #[cfg(not(target_os = "linux"))]
    {
        jiffshot_workflow::request_capture(jiffshot_workflow::OPEN_CAPTURE_WINDOW, 1);
        Ok(())
    }
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

#[cfg(target_os = "linux")]
pub fn main_hwnd() -> isize {
    0
}

#[cfg(target_os = "linux")]
pub fn bring_to_front() {}

#[cfg(target_os = "linux")]
pub fn bring_forward(_handle: isize) {}

#[cfg(target_os = "linux")]
pub fn minimize() {}

#[cfg(target_os = "linux")]
pub fn reveal(path: &Path) -> Result<(), String> {
    jiffshot_capture::os::open_path(path.parent().unwrap_or(path))
        .map_err(|error| error.to_string())
}

#[cfg(target_os = "linux")]
pub fn open_folder(path: &Path) {
    if let Err(error) = jiffshot_capture::os::open_path(path) {
        eprintln!("Could not open {}: {error}", path.display());
    }
}

/// Moves files to the Trash with `gio`, so a mistaken delete can be undone.
#[cfg(target_os = "linux")]
pub fn recycle(files: &[PathBuf]) -> bool {
    files.is_empty()
        || std::process::Command::new("gio")
            .arg("trash")
            .args(files)
            .status()
            .is_ok_and(|status| status.success())
}

/// Preserves transparency when copying an editor image.
#[cfg(target_os = "linux")]
pub fn frame_from_pixmap(pm: &Pixmap) -> Frame {
    let mut frame = Frame::new(pm.width(), pm.height());
    for (dst, src) in frame.pixels.as_chunks_mut::<4>().0.iter_mut().zip(pm.pixels()) {
        *dst = [src.blue(), src.green(), src.red(), src.alpha()];
    }
    frame.alpha = true;
    frame
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

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::UI::Shell::SHGetPathFromIDListW;

    #[test]
    fn shell_item_preserves_capture_folder_and_filename() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the system clock is after the Unix epoch")
            .as_nanos();
        let folder = std::env::temp_dir().join(format!(
            "jiffshot reveal test {} {unique}",
            std::process::id()
        ));
        std::fs::create_dir(&folder).expect("create the isolated capture folder");
        for name in [
            "Capture 2026-11-03 093441.png",
            "Capture, selected.mp4",
            "Capture \u{00e9}\u{4e2d}.png",
        ] {
            let path = folder.join(name);
            std::fs::write(&path, []).expect("create the capture fixture");
            let canonical = std::fs::canonicalize(&path).expect("resolve the expected capture");
            for input in [&path, &canonical] {
                let item = ShellItem::parse(input).expect("resolve the capture's shell item");
                let mut buffer = [0_u16; 260];
                // SAFETY: item is a valid PIDL and buffer has the required MAX_PATH capacity.
                assert_ne!(
                    unsafe { SHGetPathFromIDListW(item.0, buffer.as_mut_ptr()) },
                    0,
                    "read the shell path for {input:?}"
                );
                let length = buffer
                    .iter()
                    .position(|&c| c == 0)
                    .expect("terminated shell path");
                let selected = PathBuf::from(OsString::from_wide(&buffer[..length]));
                assert_eq!(
                    std::fs::canonicalize(selected).expect("resolve the shell's selected file"),
                    canonical,
                    "the shell must select the capture in its actual folder for {input:?}"
                );
            }
            std::fs::remove_file(path).expect("remove the capture fixture");
        }
        std::fs::remove_dir(folder).expect("remove the isolated capture folder");
    }

    #[test]
    fn reveal_missing_capture_reports_an_error() {
        let missing = std::env::temp_dir()
            .join(format!("jiffshot-missing-capture-{}", std::process::id()))
            .join("Capture.png");
        let error = reveal(&missing).expect_err("a missing capture must not open a default folder");
        assert_eq!(
            error,
            missing
                .metadata()
                .expect_err("the fixture must not exist")
                .to_string(),
            "the reveal error must report the missing capture"
        );
    }
}
