//! Local share destinations used by the capture side and the editor:
//! other programs, the default mail program, the printer and folders.

use crate::wide;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::HWND;

fn app_path_value(value: &str) -> Option<PathBuf> {
    let value = value.trim_end_matches('\0').trim().trim_matches('"');
    let path = PathBuf::from(value);
    path.is_absolute().then_some(path)
}

fn registered_program(program: &str) -> Option<PathBuf> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW,
    };

    let key = wide(&format!(
        r"Software\Microsoft\Windows\CurrentVersion\App Paths\{program}"
    ));
    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        let mut size = 0;
        // SAFETY: the key is NUL-terminated; a null output buffer requests its size.
        let status = unsafe {
            RegGetValueW(
                root,
                key.as_ptr(),
                std::ptr::null(),
                RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        if status != ERROR_SUCCESS || size < 2 || size % 2 != 0 {
            continue;
        }

        let mut value = vec![0u16; size as usize / 2];
        // SAFETY: `value` has the queried byte capacity and all strings live through the call.
        let status = unsafe {
            RegGetValueW(
                root,
                key.as_ptr(),
                std::ptr::null(),
                RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
                std::ptr::null_mut(),
                value.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if status != ERROR_SUCCESS {
            continue;
        }
        let value = String::from_utf16_lossy(&value[..size as usize / 2]);
        let Some(path) = app_path_value(&value) else {
            continue;
        };
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn executable_path(program: &str) -> Result<PathBuf, String> {
    let path = Path::new(program);
    if path.is_absolute() && path.is_file() {
        return Ok(path.to_owned());
    }
    if path.components().count() == 1 {
        if let Some(path) = registered_program(program) {
            return Ok(path);
        }
    }
    Err(format!("{program} is not installed or could not start"))
}

fn shell(owner: isize, verb: &str, file: &str, args: Option<&str>) -> bool {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let (verb, file) = (wide(verb), wide(file));
    let args = args.map(wide);
    // SAFETY: NUL-terminated strings that outlive the call.
    let r = unsafe {
        ShellExecuteW(
            owner as HWND,
            verb.as_ptr(),
            file.as_ptr(),
            args.as_ref().map_or(std::ptr::null(), |a| a.as_ptr()),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    r as isize > 32
}

fn quoted(p: &Path) -> String {
    format!("\"{}\"", p.display())
}

/// Opens `file` in an absolute executable path or an executable registered in App Paths.
pub fn open_in(owner: isize, program: &str, file: &Path) -> Result<(), String> {
    let executable = executable_path(program)?;
    if shell(
        owner,
        "open",
        &executable.to_string_lossy(),
        Some(&quoted(file)),
    ) {
        Ok(())
    } else {
        Err(format!("{program} is not installed or could not start"))
    }
}

/// Lets Windows ask which program to open the file with.
pub fn open_with(owner: isize, file: &Path) -> Result<bool, String> {
    if shell(owner, "openas", &file.to_string_lossy(), None) {
        Ok(true)
    } else {
        Err("Windows could not show the Open with dialog".into())
    }
}

pub fn print(owner: isize, file: &Path) -> Result<bool, String> {
    if shell(owner, "print", &file.to_string_lossy(), None) {
        Ok(true)
    } else {
        Err("Windows has no print handler for this file type".into())
    }
}

/// Copies the file into `folder` (a OneDrive, Dropbox or network folder),
/// never overwriting.
pub fn to_folder(file: &Path, folder: &Path) -> Result<PathBuf, String> {
    let name = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Capture".into());
    let ext = file
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_else(|| "png".into());
    let mut target = folder.join(format!("{name}.{ext}"));
    let mut n = 2;
    while target.exists() {
        target = folder.join(format!("{name} ({n}).{ext}"));
        n += 1;
    }
    std::fs::copy(file, &target).map_err(|e| e.to_string())?;
    Ok(target)
}

#[repr(C)]
struct MapiFileDescW {
    reserved: u32,
    flags: u32,
    position: u32,
    path: *const u16,
    name: *const u16,
    file_type: *mut std::ffi::c_void,
}

#[repr(C)]
struct MapiMessageW {
    reserved: u32,
    subject: *const u16,
    note: *const u16,
    message_type: *const u16,
    date: *const u16,
    conversation: *const u16,
    flags: u32,
    originator: *mut std::ffi::c_void,
    recip_count: u32,
    recips: *mut std::ffi::c_void,
    file_count: u32,
    files: *mut MapiFileDescW,
}

type MapiSendMailW = unsafe extern "system" fn(
    session: usize,
    ui: usize,
    message: *const MapiMessageW,
    flags: u32,
    reserved: u32,
) -> u32;

/// Opens a new message in the default mail program with `file` attached
/// (Simple MAPI, which Outlook and most desktop mail programs provide).
pub fn email(owner: isize, file: &Path, subject: &str) -> Result<bool, String> {
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    const MAPI_LOGON_UI: u32 = 1;
    const MAPI_DIALOG: u32 = 8;
    let path = wide(&file.to_string_lossy());
    let name = wide(
        &file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    let subject = wide(subject);
    // SAFETY: mapi32.dll's MAPISendMailW has this signature; every pointer
    // handed to it outlives the call.
    unsafe {
        let module = LoadLibraryW(wide("mapi32.dll").as_ptr());
        if module.is_null() {
            return Err("no mail program is set up (Simple MAPI is missing)".into());
        }
        let Some(proc_) = GetProcAddress(module, c"MAPISendMailW".as_ptr() as *const u8) else {
            return Err("the mail program does not support attachments from other apps".into());
        };
        let send: MapiSendMailW = std::mem::transmute(proc_);
        let mut attachment = MapiFileDescW {
            reserved: 0,
            flags: 0,
            position: u32::MAX,
            path: path.as_ptr(),
            name: name.as_ptr(),
            file_type: std::ptr::null_mut(),
        };
        let message = MapiMessageW {
            reserved: 0,
            subject: subject.as_ptr(),
            note: std::ptr::null(),
            message_type: std::ptr::null(),
            date: std::ptr::null(),
            conversation: std::ptr::null(),
            flags: 0,
            originator: std::ptr::null_mut(),
            recip_count: 0,
            recips: std::ptr::null_mut(),
            file_count: 1,
            files: &mut attachment,
        };
        match send(0, owner as usize, &message, MAPI_DIALOG | MAPI_LOGON_UI, 0) {
            0 => Ok(true),
            1 => Ok(false),
            code => Err(format!(
                "the mail program refused the message (MAPI error {code})"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_paths_reject_bare_names_and_relative_paths() {
        assert!(app_path_value("mspaint.exe").is_none());
        assert!(app_path_value(r".\mspaint.exe").is_none());
        assert_eq!(
            app_path_value("\"C:\\Windows\\System32\\mspaint.exe\"\0"),
            Some(PathBuf::from(r"C:\Windows\System32\mspaint.exe"))
        );
    }
}
