//! AppKit file panels. The caller waits on a worker; AppKit stays on the UI thread.

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSModalResponse, NSModalResponseAbort, NSModalResponseCancel, NSModalResponseOK, NSOpenPanel, NSSavePanel};
use objc2_foundation::{NSArray, NSString, NSURLComponents, NSURL};
use objc2_uniform_type_identifiers::UTType;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CString, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

thread_local! {
    static PANELS: RefCell<HashMap<u64, Retained<NSSavePanel>>> = RefCell::new(HashMap::new());
    static PANELS_SUSPENDED: Cell<bool> = const { Cell::new(false) };
}

static NEXT_PANEL: AtomicU64 = AtomicU64::new(1);

pub(super) fn active_panels_for_selftest() -> io::Result<usize> {
    if std::env::var_os("JIFFSHOT_UITEST").is_none()
        || std::env::var_os("JIFFSHOT_UITEST_PANEL_SHUTDOWN").is_none()
    {
        return Err(io::Error::other("the file-panel shutdown self-test is not enabled"));
    }
    MainThreadMarker::new().ok_or_else(|| io::Error::other("file panels must be inspected on the main thread"))?;
    Ok(PANELS.with(|panels| panels.borrow().len()))
}

pub(super) fn suspend_for_shutdown() -> io::Result<()> {
    MainThreadMarker::new().ok_or_else(|| io::Error::other("file panels must be cancelled on the main thread"))?;
    PANELS_SUSPENDED.with(|suspended| suspended.set(true));
    let panels = PANELS.with(|panels| panels.borrow().values().cloned().collect::<Vec<_>>());
    for panel in panels {
        // SAFETY: the retained panel belongs to this process and is used on the main thread.
        unsafe { panel.cancel(None) };
    }
    Ok(())
}

pub(super) fn resume_after_failed_shutdown() -> io::Result<()> {
    MainThreadMarker::new().ok_or_else(|| io::Error::other("file panels must be resumed on the main thread"))?;
    PANELS_SUSPENDED.with(|suspended| suspended.set(false));
    Ok(())
}

pub(crate) fn file_path(url: &NSURL) -> io::Result<PathBuf> {
    if !url.isFileURL() {
        return Err(io::Error::other("the URL is not a file URL"));
    }
    if url.host().is_some_and(|host| {
        let host = host.to_string();
        !host.is_empty() && !host.eq_ignore_ascii_case("localhost")
    }) {
        return Err(io::Error::other("the file URL names a remote host"));
    }
    let components = NSURLComponents::componentsWithURL_resolvingAgainstBaseURL(url, false)
        .ok_or_else(|| io::Error::other("could not parse the file URL"))?;
    let encoded = components.percentEncodedPath()
        .ok_or_else(|| io::Error::other("the file URL has no path"))?;
    let bytes = crate::paths::decode_uri_path(&encoded.to_string())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if bytes.is_empty() {
        return Err(io::Error::other("the file URL has an empty path"));
    }
    if bytes[0] != b'/' {
        return Err(io::Error::other("the file URL does not contain an absolute path"));
    }
    if url.isFileReferenceURL() {
        let resolved = url.filePathURL().ok_or_else(|| io::Error::other("the file reference could not be resolved"))?;
        if resolved.isFileReferenceURL() {
            return Err(io::Error::other("the file reference did not resolve to a path"));
        }
        return file_path(&resolved);
    }
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

pub(crate) fn file_url(path: &Path, is_directory: bool) -> io::Result<Retained<NSURL>> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the path contains a NUL byte"))?;
    let ptr = NonNull::new(path.as_ptr().cast_mut()).ok_or_else(|| io::Error::other("the path is unavailable"))?;
    // SAFETY: `path` is NUL-terminated and lives through this call.
    Ok(unsafe { NSURL::fileURLWithFileSystemRepresentation_isDirectory_relativeToURL(ptr, is_directory, None) })
}

fn selected_files(urls: &NSArray<NSURL>) -> io::Result<Vec<PathBuf>> {
    let files = urls.iter().map(|url| file_path(&url)).collect::<io::Result<Vec<_>>>()?;
    if files.is_empty() {
        return Err(io::Error::other("the file panel accepted without selecting a file"));
    }
    Ok(files)
}

fn panel_response<T>(response: NSModalResponse, selected: impl FnOnce() -> io::Result<T>) -> io::Result<Option<T>> {
    if response == NSModalResponseOK {
        selected().map(Some)
    } else if response == NSModalResponseCancel {
        Ok(None)
    } else if response == NSModalResponseAbort {
        Err(io::Error::other("the file panel could not be displayed"))
    } else {
        Err(io::Error::other(format!("the file panel returned an unexpected response: {response}")))
    }
}

fn send_result<T>(sender: &mpsc::Sender<io::Result<Option<T>>>, result: io::Result<Option<T>>) {
    if sender.send(result).is_err() {
        eprintln!("the file panel finished after its caller stopped waiting");
    }
}

fn start_if_allowed<T>(
    id: u64,
    result: mpsc::Sender<io::Result<Option<T>>>,
    start: impl FnOnce(u64, mpsc::Sender<io::Result<Option<T>>>) -> io::Result<()>,
) -> io::Result<()> {
    if PANELS_SUSPENDED.with(Cell::get) {
        send_result(&result, Ok(None));
        Ok(())
    } else {
        start(id, result)
    }
}

fn cancel_panel(id: u64) {
    let panel = PANELS.with(|panels| panels.borrow().get(&id).cloned());
    if let Some(panel) = panel {
        // SAFETY: this is a live panel owned by this process, on the main thread.
        unsafe { panel.cancel(None) };
    }
}

fn close_timed_out_panel(id: u64) {
    dispatch2::DispatchQueue::main().exec_async(move || cancel_panel(id));
}

fn activate(mtm: MainThreadMarker) {
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
}

fn open_panel(id: u64, prompt: &str, extensions: &[String], folder: Option<&Path>, multiple: bool, result: mpsc::Sender<io::Result<Option<Vec<PathBuf>>>>) -> io::Result<()> {
    let mtm = MainThreadMarker::new().ok_or_else(|| io::Error::other("the file panel must open on the main thread"))?;
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setTitle(Some(&NSString::from_str(prompt)));
    panel.setCanChooseFiles(true);
    panel.setCanChooseDirectories(false);
    panel.setTreatsFilePackagesAsDirectories(false);
    panel.setAllowsMultipleSelection(multiple);
    if let Some(folder) = folder {
        let url = file_url(folder, true)?;
        panel.setDirectoryURL(Some(&url));
    }
    if !extensions.is_empty() {
        let types = extensions.iter().map(|ext| {
            UTType::typeWithFilenameExtension(&NSString::from_str(ext))
                .ok_or_else(|| io::Error::other(format!("macOS does not recognize the file type .{ext}")))
        }).collect::<io::Result<Vec<_>>>()?;
        panel.setAllowedContentTypes(&NSArray::from_retained_slice(&types));
    }
    let selected = panel.clone();
    let handler = RcBlock::new(move |response: NSModalResponse| {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            PANELS.with(|panels| { panels.borrow_mut().remove(&id); });
            panel_response(response, || selected_files(&selected.URLs()))
        })).unwrap_or_else(|_| Err(io::Error::other("the file panel completion panicked")));
        send_result(&result, outcome);
    });
    activate(mtm);
    PANELS.with(|panels| { panels.borrow_mut().insert(id, Retained::into_super(panel.clone())); });
    panel.beginWithCompletionHandler(&handler);
    Ok(())
}

fn save_panel(id: u64, prompt: &str, name: &str, folder: Option<&Path>, result: mpsc::Sender<io::Result<Option<PathBuf>>>) -> io::Result<()> {
    let mtm = MainThreadMarker::new().ok_or_else(|| io::Error::other("the file panel must open on the main thread"))?;
    let panel = NSSavePanel::savePanel(mtm);
    panel.setTitle(Some(&NSString::from_str(prompt)));
    panel.setNameFieldStringValue(&NSString::from_str(name));
    if let Some(folder) = folder {
        let url = file_url(folder, true)?;
        panel.setDirectoryURL(Some(&url));
    }
    let selected = panel.clone();
    let handler = RcBlock::new(move |response: NSModalResponse| {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            PANELS.with(|panels| { panels.borrow_mut().remove(&id); });
            panel_response(response, || {
                let url = selected.URL().ok_or_else(|| io::Error::other("the save panel accepted without a file"))?;
                file_path(&url)
            })
        })).unwrap_or_else(|_| Err(io::Error::other("the save panel completion panicked")));
        send_result(&result, outcome);
    });
    activate(mtm);
    PANELS.with(|panels| { panels.borrow_mut().insert(id, panel.clone()); });
    panel.beginWithCompletionHandler(&handler);
    Ok(())
}

fn run_panel<T: Send + 'static>(
    start: impl FnOnce(u64, mpsc::Sender<io::Result<Option<T>>>) -> io::Result<()> + Send + 'static,
) -> io::Result<Option<T>> {
    if MainThreadMarker::new().is_some() {
        return Err(io::Error::other("file panels must be requested from a worker thread"));
    }
    let (started_tx, started_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let id = NEXT_PANEL.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .map_err(|_| io::Error::other("too many file panels have been opened"))?;
    let state = Arc::new(AtomicU8::new(0));
    let on_main = state.clone();
    dispatch2::DispatchQueue::main().exec_async(move || {
        if on_main.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            let started = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| start_if_allowed(id, result_tx, start)))
                .unwrap_or_else(|_| Err(io::Error::other("the file panel could not start")));
            if started.is_err() {
                cancel_panel(id);
            }
            if started_tx.send(started).is_err() {
                eprintln!("the file panel started after its caller stopped waiting");
            }
        }
    });
    let started = match started_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Disconnected) => return Err(io::Error::other("the file panel could not start on the main thread")),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            if state.compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return Err(io::Error::other("the main thread did not start the file panel"));
            }
            match started_rx.recv_timeout(Duration::from_secs(10)) {
                Ok(result) => result,
                Err(_) => {
                    close_timed_out_panel(id);
                    return Err(io::Error::other("the file panel did not finish starting"));
                }
            }
        }
    };
    started?;
    match result_rx.recv_timeout(Duration::from_secs(60 * 60)) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::other("the file panel closed without reporting a result")),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            close_timed_out_panel(id);
            Err(io::Error::other("the file panel did not report a result"))
        }
    }
}

/// Opens a native multiple-file panel. An empty result means the user cancelled.
pub fn choose_open_paths(prompt: &str, extensions: &[&str], folder: Option<&Path>) -> io::Result<Vec<PathBuf>> {
    let prompt = prompt.to_owned();
    let extensions = extensions.iter().map(|extension| (*extension).to_owned()).collect::<Vec<_>>();
    let folder = folder.map(Path::to_path_buf);
    run_panel(move |id, result| open_panel(id, &prompt, &extensions, folder.as_deref(), true, result)).map(|files| files.unwrap_or_default())
}

fn application_selection(paths: Option<Vec<PathBuf>>) -> io::Result<Option<PathBuf>> {
    let Some(mut paths) = paths else { return Ok(None) };
    if paths.len() != 1 {
        return Err(io::Error::other("the file panel did not return one application"));
    }
    let path = paths.pop().expect("checked the selection length");
    if !path.is_dir() || !path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("app")) {
        return Err(io::Error::other("the selected item is not a local application bundle"));
    }
    Ok(Some(path))
}

/// Opens a native application-bundle panel. `None` means the user cancelled.
pub fn choose_application_path(prompt: &str, folder: Option<&Path>) -> io::Result<Option<PathBuf>> {
    let prompt = prompt.to_owned();
    let folder = folder.map(Path::to_path_buf);
    let extensions = vec!["app".to_owned()];
    run_panel(move |id, result| open_panel(id, &prompt, &extensions, folder.as_deref(), false, result))
        .and_then(application_selection)
}

/// Opens a native save panel. `None` means the user cancelled.
pub fn choose_save_path(prompt: &str, name: &str, folder: Option<&Path>) -> io::Result<Option<PathBuf>> {
    let (prompt, name, folder) = (prompt.to_owned(), name.to_owned(), folder.map(Path::to_path_buf));
    run_panel(move |id, result| save_panel(id, &prompt, &name, folder.as_deref(), result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suspended_panels_cancel_queued_requests_and_resume_afterward() {
        PANELS_SUSPENDED.with(|suspended| suspended.set(true));
        let (sender, receiver) = mpsc::channel::<io::Result<Option<u8>>>();
        start_if_allowed(1, sender, |_, _| panic!("a suspended panel must not start")).unwrap();
        assert_eq!(receiver.recv().unwrap().unwrap(), None);

        PANELS_SUSPENDED.with(|suspended| suspended.set(false));
        let (sender, receiver) = mpsc::channel();
        start_if_allowed(2, sender, |id, sender| {
            assert_eq!(id, 2);
            sender.send(Ok(Some(7))).unwrap();
            Ok(())
        }).unwrap();
        assert_eq!(receiver.recv().unwrap().unwrap(), Some(7));
    }

    #[test]
    fn panel_shutdown_requires_the_main_thread() {
        std::thread::spawn(|| {
            assert!(suspend_for_shutdown().is_err());
            assert!(resume_after_failed_shutdown().is_err());
        }).join().unwrap();
    }

    #[test]
    fn application_selection_requires_one_local_bundle() {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("jiffshot-application-picker-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let bundle = root.join("Example.APP");
        let other_dir = root.join("Folder");
        let file = root.join("Not an application.app");
        std::fs::create_dir(&bundle).unwrap();
        std::fs::create_dir(&other_dir).unwrap();
        std::fs::write(&file, b"not a bundle").unwrap();

        assert_eq!(application_selection(None).unwrap(), None);
        assert!(application_selection(Some(vec![])).is_err());
        assert!(application_selection(Some(vec![bundle.clone(), bundle.clone()])).is_err());
        assert!(application_selection(Some(vec![other_dir.clone()])).is_err());
        assert!(application_selection(Some(vec![file.clone()])).is_err());
        assert_eq!(application_selection(Some(vec![bundle.clone()])).unwrap(), Some(bundle.clone()));

        std::fs::remove_file(file).unwrap();
        std::fs::remove_dir(bundle).unwrap();
        std::fs::remove_dir(other_dir).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn panel_results_distinguish_cancel_abort_and_invalid_responses() {
        assert_eq!(panel_response(NSModalResponseOK, || Ok(42)).unwrap(), Some(42));
        assert_eq!(panel_response(NSModalResponseCancel, || panic!("cancel must not read a selection")).unwrap(), None::<i32>);
        assert!(panel_response::<i32>(NSModalResponseAbort, || Ok(42)).is_err());
        assert!(panel_response::<i32>(12345, || Ok(42)).is_err());
        assert!(panel_response(NSModalResponseOK, || Err::<i32, _>(io::Error::other("invalid selection"))).is_err());
    }

    #[test]
    fn file_urls_keep_unusual_names_and_multiple_paths() {
        let paths = [
            PathBuf::from("/tmp/space \"quote\"\nline.png"),
            PathBuf::from(OsString::from_vec(b"/tmp/non-utf8-\xff.webp".to_vec())),
        ];
        for is_directory in [false, true] {
            let urls = paths.iter().map(|path| file_url(path, is_directory).unwrap()).collect::<Vec<_>>();
            assert_eq!(selected_files(&NSArray::from_retained_slice(&urls)).unwrap(), paths);
        }
        assert!(selected_files(&NSArray::from_slice(&[])).is_err());
        assert!(file_url(Path::new(OsString::from_vec(b"bad\0folder".to_vec()).as_os_str()), true).is_err());
        for invalid in ["file://remote.invalid/tmp/a.png", "file:relative/a.png", "file:///tmp/a%00.png", "file:///.file/id=0.0"] {
            let url = NSURL::URLWithString(&NSString::from_str(invalid)).unwrap();
            assert!(file_path(&url).is_err(), "{invalid}");
        }
    }
}
