//! Local share destinations used by the capture side and the editor:
//! other programs, the default mail program, the printer and folders.

use crate::wide;
use std::path::{Path, PathBuf};
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::Foundation::{FreeLibrary, HWND};

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

fn shell(owner: isize, verb: &str, file: &OsStr, args: Option<&[u16]>) -> bool {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let verb = wide(verb);
    let file = file.encode_wide().chain(Some(0)).collect::<Vec<_>>();
    // SAFETY: NUL-terminated strings that outlive the call.
    let r = unsafe {
        ShellExecuteW(
            owner as HWND,
            verb.as_ptr(),
            file.as_ptr(),
            args.map_or(std::ptr::null(), |a| a.as_ptr()),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    r as isize > 32
}

fn quoted(p: &Path) -> Vec<u16> {
    std::iter::once(b'"' as u16)
        .chain(p.as_os_str().encode_wide())
        .chain([b'"' as u16, 0])
        .collect()
}

/// Opens `file` in an absolute executable path or an executable registered in App Paths.
pub fn open_in(owner: isize, program: &str, file: &Path) -> Result<(), String> {
    let executable = executable_path(program)?;
    if shell(
        owner,
        "open",
        executable.as_os_str(),
        Some(&quoted(file)),
    ) {
        Ok(())
    } else {
        Err(format!("{program} is not installed or could not start"))
    }
}

/// Lets Windows ask which program to open the file with.
pub fn open_with(owner: isize, file: &Path) -> Result<bool, String> {
    if shell(owner, "openas", file.as_os_str(), None) {
        Ok(true)
    } else {
        Err("Windows could not show the Open with dialog".into())
    }
}

pub fn print(owner: isize, file: &Path) -> Result<bool, String> {
    if shell(owner, "print", file.as_os_str(), None) {
        Ok(true)
    } else {
        Err("Windows has no print handler for this file type".into())
    }
}

/// Copies the file into `folder` (a OneDrive, Dropbox or network folder),
/// never overwriting.
pub fn to_folder(file: &Path, folder: &Path) -> Result<PathBuf, String> {
    let name = file.file_stem().unwrap_or_else(|| OsStr::new("Capture"));
    let ext = file.extension().unwrap_or_else(|| OsStr::new("png"));
    for n in 1_u64.. {
        let mut target_name = OsString::from(name);
        if n != 1 {
            target_name.push(format!(" ({n})"));
        }
        target_name.push(".");
        target_name.push(ext);
        let target = folder.join(target_name);
        let mut destination = match std::fs::File::options()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(destination) => destination,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        };
        let result = (|| {
            let mut source = std::fs::File::open(file)?;
            let permissions = source.metadata()?.permissions();
            std::io::copy(&mut source, &mut destination)?;
            destination.set_permissions(permissions)?;
            Ok::<_, std::io::Error>(())
        })();
        if let Err(error) = result {
            drop(destination);
            if let Err(cleanup) = std::fs::remove_file(&target) {
                if cleanup.kind() != std::io::ErrorKind::NotFound {
                    return Err(format!("{error}; could not remove {}: {cleanup}", target.display()));
                }
            }
            return Err(error.to_string());
        }
        return Ok(target);
    }
    Err("No available destination filename.".into())
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
    let path = crate::wide_path(file);
    let name = file
        .file_name()
        .map(|name| name.encode_wide().chain(Some(0)).collect())
        .unwrap_or_else(|| vec![0]);
    let subject = wide(subject);
    // SAFETY: mapi32.dll's MAPISendMailW has this signature; every pointer
    // handed to it outlives the call.
    unsafe {
        let module = LoadLibraryW(wide("mapi32.dll").as_ptr());
        if module.is_null() {
            return Err("no mail program is set up (Simple MAPI is missing)".into());
        }
        let Some(proc_) = GetProcAddress(module, c"MAPISendMailW".as_ptr() as *const u8) else {
            FreeLibrary(module);
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
        let result = send(0, owner as usize, &message, MAPI_DIALOG | MAPI_LOGON_UI, 0);
        FreeLibrary(module);
        match result {
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
    use std::collections::HashSet;

    #[test]
    fn app_paths_reject_bare_names_and_relative_paths() {
        assert!(app_path_value("mspaint.exe").is_none());
        assert!(app_path_value(r".\mspaint.exe").is_none());
        assert_eq!(
            app_path_value("\"C:\\Windows\\System32\\mspaint.exe\"\0"),
            Some(PathBuf::from(r"C:\Windows\System32\mspaint.exe"))
        );
    }

    #[test]
    fn concurrent_folder_copies_never_overwrite_each_other() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("jiffshot-share-{nonce}"));
        std::fs::create_dir(&root).unwrap();
        let source = root.join("source.bin");
        let folder = root.join("destination");
        std::fs::create_dir(&folder).unwrap();
        let contents = vec![0xA5; 1 << 20];
        std::fs::write(&source, &contents).unwrap();
        std::fs::write(folder.join("source.bin"), b"existing").unwrap();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let source = source.clone();
                let folder = folder.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    to_folder(&source, &folder).unwrap()
                })
            })
            .collect();
        let paths: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();

        assert_eq!(paths.iter().collect::<HashSet<_>>().len(), paths.len());
        assert_eq!(std::fs::read(folder.join("source.bin")).unwrap(), b"existing");
        for path in paths {
            assert_eq!(std::fs::read(path).unwrap(), contents);
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
