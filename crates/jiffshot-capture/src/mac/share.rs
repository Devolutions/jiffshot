//! Local share destinations used by the capture side and the editor on
//! macOS: other apps (Office apps by bundle ID), the default mail app, the
//! printer and folders. `owner` (a window on Windows) is unused here.

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSSharingService, NSSharingServiceDelegate, NSSharingServiceNameComposeEmail};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol, NSString, NSUserCancelledError};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Creates a private per-operation folder for a temporary share export.
pub fn temporary_export_path(stem: &str, ext: &str) -> Result<PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;

    if stem.is_empty() || matches!(stem, "." | "..") || stem.contains(['/', '\0'])
        || ext.is_empty() || ext.contains(['/', '\0'])
    {
        return Err("invalid temporary sharing filename".into());
    }
    static NEXT_EXPORT: AtomicU64 = AtomicU64::new(0);
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)
        .map_err(|error| format!("could not prepare a temporary share: {error}"))?.as_nanos();
    for _ in 0..32 {
        let sequence = NEXT_EXPORT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("jiffshot-share-{}-{timestamp}-{sequence}", std::process::id()));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir.join(format!("{stem}.{ext}"))),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("could not prepare a temporary sharing folder: {error}")),
        }
    }
    Err("could not create a unique temporary sharing folder".into())
}

/// The macOS app for a Windows program name used by the share menu, as a
/// bundle identifier; other names are app paths or app names.
pub fn mac_app(program: &str) -> Option<&'static str> {
    let exe = program.rsplit(['\\', '/']).next().unwrap_or(program).to_ascii_lowercase();
    Some(match exe.as_str() {
        "winword.exe" => "com.microsoft.Word",
        "excel.exe" => "com.microsoft.Excel",
        "powerpnt.exe" => "com.microsoft.Powerpoint",
        "onenote.exe" => "com.microsoft.onenote.mac",
        "outlook.exe" => "com.microsoft.Outlook",
        "mspaint.exe" => "com.apple.Preview",
        _ => return None,
    })
}

fn open(args: &[&OsStr]) -> Result<(), String> {
    let out = Command::new("/usr/bin/open").args(args).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let error = String::from_utf8_lossy(&out.stderr);
        let error = error.trim();
        Err(if error.is_empty() { format!("open exited with {}", out.status) } else { error.to_owned() })
    }
}

/// Opens `file` in an app: a share-menu program (`winword.exe`…), an app
/// bundle path, or an app name.
pub fn open_in(_owner: isize, program: &str, file: &Path) -> Result<(), String> {
    let office = match mac_app(program) {
        Some("com.microsoft.Word") => Some("Word"),
        Some("com.microsoft.Excel") => Some("Excel"),
        Some("com.microsoft.Powerpoint") => Some("PowerPoint"),
        Some("com.microsoft.onenote.mac") => Some("OneNote"),
        _ => None,
    };
    if let Some(name) = office {
        return Err(format!("Direct {name} insertion is not yet available on macOS; copy and paste the capture instead."));
    }
    let result = match mac_app(program) {
        Some(bundle) => open(&["-b".as_ref(), bundle.as_ref(), file.as_os_str()]),
        None => open(&["-a".as_ref(), program.as_ref(), file.as_os_str()]),
    };
    let name = mac_app(program).map_or(program, |b| b.rsplit('.').next().unwrap_or(b));
    result.map_err(|error| format!("{name} is not installed or could not start: {error}"))
}

fn script_result(success: bool, stdout: &[u8], stderr: &[u8], status: &str, action: &str) -> Result<Option<String>, String> {
    if success {
        return Ok(Some(String::from_utf8_lossy(stdout).trim_end_matches(['\r', '\n']).to_owned()));
    }
    let error = String::from_utf8_lossy(stderr);
    let error = error.trim();
    if error.ends_with("(-128)") {
        return Ok(None);
    }
    if error.ends_with("(-1743)") {
        return Err(format!("{action}: macOS denied automation. Check System Settings > Privacy & Security > Automation, then retry. {error}"));
    }
    let detail = if error.is_empty() { status } else { error };
    Err(format!("{action}: {detail}"))
}

fn script(source: &str, args: &[&OsStr], action: &str) -> Result<Option<String>, String> {
    let output = Command::new("/usr/bin/osascript")
        .args(["-e", source])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{action}: {e}"))?;
    script_result(output.status.success(), &output.stdout, &output.stderr, &output.status.to_string(), action)
}

/// Asks which app to open the file with. `false` means the user cancelled.
pub fn open_with(_owner: isize, file: &Path) -> Result<bool, String> {
    open_with_in(file, None)
}

/// Opens the application picker in an optional starting folder before sharing.
pub fn open_with_in(file: &Path, folder: Option<&Path>) -> Result<bool, String> {
    let Some(app) = crate::macos::picker::choose_application_path("Open the capture with", folder)
        .map_err(|error| format!("could not choose an application: {error}"))? else {
        return Ok(false);
    };
    open(&["-a".as_ref(), app.as_os_str(), file.as_os_str()]).map_err(|e| format!("could not open the capture: {e}"))?;
    Ok(true)
}

/// Opens the system print dialog through Preview. `false` means cancellation.
pub fn print(_owner: isize, file: &Path) -> Result<bool, String> {
    let source = "on run argv\n\
        tell application id \"com.apple.Preview\"\n\
        activate\n\
        print (POSIX file (item 1 of argv)) with print dialog\n\
        end tell\n\
        end run";
    script(source, &[file.as_os_str()], "could not print the capture").map(|result| result.is_some())
}

/// Copies the file into `folder` (an iCloud Drive, OneDrive, Dropbox or
/// network folder), never overwriting. A failed copy may leave an incomplete
/// file at the destination named in the error.
pub fn to_folder(file: &Path, folder: &Path) -> Result<PathBuf, String> {
    let mut source = std::fs::File::open(file)
        .map_err(|error| format!("could not open {}: {error}", file.display()))?;
    let metadata = source.metadata()
        .map_err(|error| format!("could not inspect {}: {error}", file.display()))?;
    if !metadata.is_file() {
        return Err(format!("the share source is not a regular file: {}", file.display()));
    }

    let name = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Capture".into());
    let ext = file.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_else(|| "png".into());
    let mut n = 1u64;
    loop {
        let target = if n == 1 {
            folder.join(format!("{name}.{ext}"))
        } else {
            folder.join(format!("{name} ({n}).{ext}"))
        };
        let mut output = match std::fs::OpenOptions::new().write(true).create_new(true).open(&target) {
            Ok(output) => output,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                n = n.checked_add(1).ok_or("no available filename for the shared capture")?;
                continue;
            }
            Err(error) => return Err(format!("could not create {}: {error}", target.display())),
        };
        write_shared_file(&mut source, &mut output, metadata.permissions(), &target)?;
        return Ok(target);
    }
}

fn write_shared_file(
    source: &mut impl Read,
    output: &mut std::fs::File,
    permissions: std::fs::Permissions,
    target: &Path,
) -> Result<(), String> {
    io::copy(source, &mut *output)
        .and_then(|_| output.set_permissions(permissions))
        .map_err(|error| format!("could not finish sharing to {}: {error}; an incomplete file may remain there", target.display()))?;
    Ok(())
}

type EmailResult = Result<bool, String>;

struct EmailIvars {
    id: u64,
    result: mpsc::Sender<EmailResult>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; the delegate is
    // confined to AppKit's main thread.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "JiffshotEmailDelegate"]
    #[ivars = EmailIvars]
    struct EmailDelegate;

    unsafe impl NSObjectProtocol for EmailDelegate {}

    unsafe impl NSSharingServiceDelegate for EmailDelegate {
        #[allow(non_snake_case)]
        #[unsafe(method(sharingService:didShareItems:))]
        unsafe fn sharingService_didShareItems(&self, _service: &NSSharingService, _items: &NSArray) {
            self.finish(Ok(true));
        }

        #[allow(non_snake_case)]
        #[unsafe(method(sharingService:didFailToShareItems:error:))]
        unsafe fn sharingService_didFailToShareItems_error(&self, _service: &NSSharingService, _items: &NSArray, error: &NSError) {
            self.finish(email_error(&error.domain().to_string(), error.code(), &error.localizedDescription().to_string()));
        }
    }
);

struct EmailJob {
    _service: Retained<NSSharingService>,
    _delegate: Retained<EmailDelegate>,
}

thread_local! {
    static EMAIL_JOBS: RefCell<HashMap<u64, EmailJob>> = RefCell::new(HashMap::new());
}

static NEXT_EMAIL_ID: AtomicU64 = AtomicU64::new(1);

fn email_error(domain: &str, code: isize, message: &str) -> EmailResult {
    if (domain == "NSCocoaErrorDomain" && code == NSUserCancelledError) || (domain == "NSOSStatusErrorDomain" && code == -128) {
        Ok(false)
    } else {
        Err(format!("mail sharing failed: {message} ({domain} {code})"))
    }
}

impl EmailDelegate {
    fn new(mtm: MainThreadMarker, id: u64, result: mpsc::Sender<EmailResult>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(EmailIvars { id, result });
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }

    fn finish(&self, result: EmailResult) {
        if self.ivars().result.send(result.clone()).is_err() {
            if let Err(error) = result {
                eprintln!("{error}");
            }
        }
        let id = self.ivars().id;
        // The delegate must remain alive until its callback returns.
        dispatch2::DispatchQueue::main().exec_async(move || {
            EMAIL_JOBS.with(|jobs| {
                jobs.borrow_mut().remove(&id);
            });
        });
    }
}

fn compose(file: &Path, subject: &str, result: mpsc::Sender<EmailResult>) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("mail sharing must start on the main thread")?;
    if !file.is_file() {
        return Err(format!("the attachment is unavailable: {}", file.display()));
    }
    // SAFETY: reading an AppKit constant.
    let service = NSSharingService::sharingServiceNamed(unsafe { NSSharingServiceNameComposeEmail })
        .ok_or("no mail app is set up")?;
    let url = crate::macos::picker::file_url(file, false).map_err(|error| error.to_string())?;
    let item: Retained<AnyObject> = Retained::into_super(Retained::into_super(url));
    let items = NSArray::from_retained_slice(&[item]);
    service.setSubject(Some(&NSString::from_str(subject)));
    // SAFETY: file URLs are pasteboard-writable items.
    unsafe {
        if !service.canPerformWithItems(Some(&items)) {
            return Err("the mail app cannot take attachments".into());
        }
    }
    let id = NEXT_EMAIL_ID.fetch_add(1, Ordering::Relaxed);
    let delegate = EmailDelegate::new(mtm, id, result);
    service.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    EMAIL_JOBS.with(|jobs| {
        jobs.borrow_mut().insert(id, EmailJob { _service: service.clone(), _delegate: delegate });
    });
    // SAFETY: file URLs are pasteboard-writable items; the retained service
    // and its delegate remain alive until a completion callback.
    unsafe {
        service.performWithItems(&items);
    }
    Ok(())
}

/// Shares `file` by email, waiting on a worker thread for the service result.
/// `false` means the user cancelled.
pub fn email(_owner: isize, file: &Path, subject: &str) -> EmailResult {
    if objc2::MainThreadMarker::new().is_some() {
        return Err("email sharing must be started from a worker thread".into());
    }
    let (file, subject) = (file.to_path_buf(), subject.to_owned());
    let (started_tx, started_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let state = Arc::new(AtomicU8::new(0));
    let main_state = state.clone();
    dispatch2::DispatchQueue::main().exec_async(move || {
        if main_state.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            let _ = started_tx.send(compose(&file, &subject, result_tx));
        }
    });
    let started = match started_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Disconnected) => return Err("mail sharing could not start on the main thread".into()),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            if state.compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return Err("mail sharing could not start on the main thread".into());
            }
            started_rx.recv_timeout(Duration::from_secs(10))
                .map_err(|_| "mail sharing did not finish starting; the mail app may still open".to_owned())?
        }
    };
    started?;
    result_rx.recv_timeout(Duration::from_secs(60 * 60)).map_err(|_| "the mail app did not report a sharing result".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            loop {
                let id = NEXT.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!("jiffshot-share-{name}-{}-{id}", std::process::id()));
                match std::fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => panic!("could not create {}: {error}", path.display()),
                }
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if let Err(error) = std::fs::remove_dir_all(&self.0) {
                eprintln!("could not remove {}: {error}", self.0.display());
            }
        }
    }

    #[test]
    fn office_programs_map_to_mac_apps() {
        assert_eq!(mac_app("winword.exe"), Some("com.microsoft.Word"));
        assert_eq!(mac_app(r"C:\Program Files\Microsoft Office\EXCEL.EXE"), Some("com.microsoft.Excel"));
        assert_eq!(mac_app("/Applications/Pixelmator Pro.app"), None);
    }

    #[test]
    fn office_sharing_does_not_report_insertion_after_only_launching_an_app() {
        for (program, name) in [
            ("winword.exe", "Word"),
            ("excel.exe", "Excel"),
            ("powerpnt.exe", "PowerPoint"),
            ("onenote.exe", "OneNote"),
        ] {
            let error = open_in(0, program, Path::new("capture.png")).unwrap_err();
            assert!(error.contains(name));
            assert!(error.contains("not yet available"));
        }
    }

    #[test]
    fn script_errors_and_cancellation_are_reported() {
        assert_eq!(script_result(true, b"printed\n", b"", "exit status: 0", "print").unwrap(), Some("printed".into()));
        assert_eq!(script_result(false, b"", b"User canceled. (-128)\n", "exit status: 1", "print").unwrap(), None);
        let denied = script_result(false, b"", b"automation denied (-1743)", "exit status: 1", "print").unwrap_err();
        assert!(denied.contains("System Settings > Privacy & Security > Automation"));
        assert!(denied.contains("automation denied (-1743)"));
        assert!(script_result(false, b"", b"", "exit status: 1", "print").unwrap_err().contains("exit status: 1"));
    }

    #[test]
    fn email_errors_distinguish_cancellation() {
        assert_eq!(email_error("NSCocoaErrorDomain", NSUserCancelledError, "cancelled"), Ok(false));
        assert_eq!(email_error("NSOSStatusErrorDomain", -128, "cancelled"), Ok(false));
        assert!(email_error("NSCocoaErrorDomain", 12, "attachment failed").unwrap_err().contains("attachment failed"));
    }

    #[test]
    fn to_folder_never_overwrites() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = Scratch::new("basic");
        let dir = &scratch.0;
        let src = dir.join("src.png");
        std::fs::write(&src, b"x").unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o640)).unwrap();
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        assert_eq!(to_folder(&src, &out).unwrap(), out.join("src.png"));
        assert_eq!(to_folder(&src, &out).unwrap(), out.join("src (2).png"));
        assert_eq!(std::fs::metadata(out.join("src.png")).unwrap().permissions().mode() & 0o777, 0o640);
    }

    #[test]
    fn to_folder_does_not_follow_dangling_destination_links() {
        use std::os::unix::fs::symlink;

        let scratch = Scratch::new("link");
        let dir = &scratch.0;
        let src = dir.join("src.png");
        std::fs::write(&src, b"capture").unwrap();
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let linked = dir.join("linked.png");
        let link = out.join("src.png");
        symlink(&linked, &link).unwrap();
        std::fs::write(out.join("src (2).png"), b"existing").unwrap();

        let copied = to_folder(&src, &out).unwrap();
        assert_eq!(copied, out.join("src (3).png"));
        assert_eq!(std::fs::read(&copied).unwrap(), b"capture");
        assert!(!linked.exists());
        assert!(std::fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(out.join("src (2).png")).unwrap(), b"existing");
    }

    #[test]
    fn concurrent_folder_shares_use_distinct_destinations() {
        use std::collections::HashSet;
        use std::sync::Barrier;

        const WORKERS: usize = 12;
        let scratch = Scratch::new("concurrent");
        let dir = &scratch.0;
        let src = dir.join("src.png");
        std::fs::write(&src, vec![0x5a; 512 * 1024]).unwrap();
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("src.png"), b"existing").unwrap();
        let start = Arc::new(Barrier::new(WORKERS));
        let workers: Vec<_> = (0..WORKERS).map(|_| {
            let (src, out, start) = (src.clone(), out.clone(), start.clone());
            std::thread::spawn(move || {
                start.wait();
                to_folder(&src, &out).unwrap()
            })
        }).collect();
        let paths: HashSet<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
        assert_eq!(paths.len(), WORKERS);
        assert_eq!(std::fs::read(out.join("src.png")).unwrap(), b"existing");
        for path in paths {
            assert_eq!(std::fs::read(path).unwrap(), vec![0x5a; 512 * 1024]);
        }
    }

    #[test]
    fn folder_share_reports_incomplete_output_after_a_copy_error() {
        struct Interrupted(bool);
        impl Read for Interrupted {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if buf.is_empty() {
                    return Ok(0);
                }
                if self.0 {
                    return Err(io::Error::other("injected read error"));
                }
                self.0 = true;
                let count = buf.len().min(4);
                buf[..count].copy_from_slice(&b"part"[..count]);
                Ok(count)
            }
        }

        let scratch = Scratch::new("interrupted");
        let dir = &scratch.0;
        let target = dir.join("capture.png");
        let mut output = std::fs::OpenOptions::new().write(true).create_new(true).open(&target).unwrap();
        let permissions = output.metadata().unwrap().permissions();
        let error = write_shared_file(&mut Interrupted(false), &mut output, permissions, &target).unwrap_err();
        assert!(error.contains("injected read error"));
        assert!(error.contains("incomplete file"));
        assert!(error.contains(target.to_str().unwrap()));
        drop(output);
        assert_eq!(std::fs::read(&target).unwrap(), b"part");
    }

    #[test]
    fn folder_share_does_not_create_output_when_source_is_invalid() {
        let scratch = Scratch::new("invalid");
        let dir = &scratch.0;
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        assert!(to_folder(&dir.join("missing.png"), &out).is_err());
        assert!(to_folder(&dir, &out).unwrap_err().contains("not a regular file"));
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0);
    }
}
