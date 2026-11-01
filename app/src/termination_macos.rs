//! Application-wide deferred termination for the pinned Rustolonia ABI.

use rustolonia::AppScope;
use rustolonia_sys as sys;
use std::cell::RefCell;
use std::ffi::{c_void, CStr, CString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{fence, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

const APPLICATION_IID: sys::Guid = sys::Guid {
    data1: 0x6B2E8F10,
    data2: 0x4C91,
    data3: 0x4E3A,
    data4: [0x9A, 0x77, 0x1F, 0x0C, 0x2B, 0x3A, 0x4D, 0xA4],
};
const HANDLER_IID: sys::Guid = sys::Guid {
    data1: 0x6B2E8F10,
    data2: 0x4C91,
    data3: 0x4E3A,
    data4: [0x9A, 0x77, 0x1F, 0x0C, 0x2B, 0x3A, 0x4D, 0xA5],
};

#[repr(C)]
struct ApplicationVtbl {
    query_interface: unsafe extern "system" fn(*mut sys::IUnknown, *const sys::Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut sys::IUnknown) -> u32,
    release: unsafe extern "system" fn(*mut sys::IUnknown) -> u32,
    set_handler: unsafe extern "system" fn(*mut Application, *mut Handler) -> i32,
    complete: unsafe extern "system" fn(*mut Application, i64, i32) -> i32,
}

#[repr(C)]
struct Application {
    vtable: *const ApplicationVtbl,
}

// SAFETY: IAvnApplication11 has an IUnknown prefix.
unsafe impl sys::ComInterface for Application {
    const IID: sys::Guid = APPLICATION_IID;
}

#[repr(C)]
struct HandlerVtbl {
    query_interface: unsafe extern "system" fn(*mut sys::IUnknown, *const sys::Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut sys::IUnknown) -> u32,
    release: unsafe extern "system" fn(*mut sys::IUnknown) -> u32,
    requested: unsafe extern "system" fn(*mut Handler, i64, i32) -> i32,
}

#[repr(C)]
struct Handler {
    vtable: *const HandlerVtbl,
    references: AtomicU32,
    coordinator: Weak<Coordinator>,
}

// SAFETY: the handler implements IUnknown followed by OnRequested.
unsafe impl sys::ComInterface for Handler {
    const IID: sys::Guid = HANDLER_IID;
}

static HANDLER_VTABLE: HandlerVtbl = HandlerVtbl { query_interface, add_ref, release, requested };

fn handler(coordinator: Weak<Coordinator>) -> sys::ComPtr<Handler> {
    let value = Box::new(Handler { vtable: &HANDLER_VTABLE, references: AtomicU32::new(1), coordinator });
    // SAFETY: the new handler owns its initial COM reference.
    unsafe { sys::ComPtr::from_raw(Box::into_raw(value)).expect("a boxed handler is not null") }
}

unsafe extern "system" fn query_interface(
    this: *mut sys::IUnknown,
    iid: *const sys::Guid,
    result: *mut *mut c_void,
) -> i32 {
    if this.is_null() || iid.is_null() || result.is_null() {
        return sys::E_POINTER;
    }
    // SAFETY: COM supplies valid pointers for this call.
    unsafe {
        *result = ptr::null_mut();
        if *iid != sys::Guid::IUNKNOWN && *iid != HANDLER_IID {
            return sys::E_NOINTERFACE;
        }
        add_ref(this);
        *result = this.cast();
    }
    sys::S_OK
}

unsafe extern "system" fn add_ref(this: *mut sys::IUnknown) -> u32 {
    // SAFETY: COM calls this on a live Handler whose IUnknown is its prefix.
    unsafe { (*this.cast::<Handler>()).references.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn release(this: *mut sys::IUnknown) -> u32 {
    // SAFETY: the last COM release owns the allocation.
    unsafe {
        let value = this.cast::<Handler>();
        let remaining = (*value).references.fetch_sub(1, Ordering::Release) - 1;
        if remaining == 0 {
            fence(Ordering::Acquire);
            drop(Box::from_raw(value));
        }
        remaining
    }
}

unsafe extern "system" fn requested(this: *mut Handler, token: i64, flags: i32) -> i32 {
    if this.is_null() {
        return sys::E_POINTER;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the host retains the handler throughout its callback.
        let coordinator = unsafe { (*this).coordinator.upgrade() }.ok_or(sys::Error(sys::E_FAIL))?;
        coordinator.request(token, flags)
    }));
    match result {
        Ok(Ok(())) => sys::S_OK,
        Ok(Err(error)) => {
            eprintln!("macOS termination callback failed: {error}");
            error.0
        }
        Err(_) => {
            eprintln!("macOS termination callback panicked");
            sys::E_FAIL
        }
    }
}

#[derive(Default)]
struct Pending {
    token: Option<i64>,
}

impl Pending {
    fn request(&mut self, token: i64) -> Result<bool, &'static str> {
        if token <= 0 {
            return Err("the termination token is invalid");
        }
        match self.token {
            None => {
                self.token = Some(token);
                Ok(true)
            }
            Some(current) if current == token => Ok(false),
            Some(_) => Err("another termination request is already pending"),
        }
    }

    fn require(&self, token: i64) -> io::Result<()> {
        if self.token == Some(token) {
            Ok(())
        } else {
            Err(io::Error::other("the termination request is no longer pending"))
        }
    }

    fn unschedule(&mut self, token: i64) {
        if self.token == Some(token) {
            self.token = None;
        }
    }
}

struct Coordinator {
    application: sys::ComPtr<Application>,
    scope: AppScope,
    pending: Mutex<Pending>,
    _host: sys::Host,
}

fn lock_pending(pending: &Mutex<Pending>) -> MutexGuard<'_, Pending> {
    match pending.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            pending.clear_poison();
            crate::shutdown_ui_macos::report("the macOS termination state was poisoned; recovering its pending token");
            poisoned.into_inner()
        }
    }
}

thread_local! {
    static COORDINATOR: RefCell<Option<Arc<Coordinator>>> = const { RefCell::new(None) };
    static TEST_REQUESTS: RefCell<Vec<(i64, i32)>> = const { RefCell::new(Vec::new()) };
}

fn open_application() -> io::Result<(sys::Host, sys::ComPtr<Application>)> {
    let path = rustolonia::discover_host_path().map_err(io::Error::other)?;
    let host = sys::Host::load(path).map_err(io::Error::other)?;
    let application = host.activation_factory().and_then(|factory| factory.create_application()).map_err(io::Error::other)?;
    let application = application.query_interface::<Application>().map_err(|error| {
        if error.0 == sys::E_NOINTERFACE {
            io::Error::other("the Rustolonia host does not support deferred macOS termination")
        } else {
            io::Error::other(format!("could not initialize macOS termination coordination: {error}"))
        }
    })?;
    Ok((host, application))
}

fn dl_error() -> String {
    // SAFETY: dlerror returns either null or a thread-local NUL-terminated string.
    unsafe {
        let error = libc::dlerror();
        if error.is_null() { "unknown dynamic loader error".into() } else { CStr::from_ptr(error).to_string_lossy().into_owned() }
    }
}

pub(crate) fn check_native_capability(
    path: &Path,
    symbol: &'static [u8],
    capability_name: &str,
    result_name: &str,
) -> io::Result<()> {
    let path = path.canonicalize()?;
    let name = CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::other("the native companion path contains a NUL"))?;
    let symbol = CStr::from_bytes_with_nul(symbol).map_err(io::Error::other)?;
    // SAFETY: the absolute path and symbol are NUL-terminated; the library
    // remains open until the function has returned.
    unsafe {
        let library = libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if library.is_null() {
            return Err(io::Error::other(format!("could not load {}: {}", path.display(), dl_error())));
        }
        libc::dlerror();
        let symbol = libc::dlsym(library, symbol.as_ptr());
        let outcome = if symbol.is_null() {
            Err(io::Error::other(format!("the native companion lacks {capability_name} support: {}", dl_error())))
        } else {
            let capability: unsafe extern "C" fn() -> libc::c_int = std::mem::transmute(symbol);
            let value = capability();
            if value == 1 {
                Ok(())
            } else {
                Err(io::Error::other(format!("the native companion returned unsupported {result_name} capability {value}")))
            }
        };
        if libc::dlclose(library) != 0 {
            return Err(io::Error::other(format!("could not close {}: {}", path.display(), dl_error())));
        }
        outcome
    }
}

fn check_native_companion(path: &Path) -> io::Result<()> {
    check_native_capability(path, b"avn_supports_deferred_termination\0", "deferred termination", "termination")
}

/// Run only in the dedicated package-probe process, not in the live app.
pub(crate) fn check_host() -> io::Result<()> {
    let host = rustolonia::discover_host_path().map_err(io::Error::other)?;
    let directory = host.parent().ok_or_else(|| io::Error::other("the host has no parent directory"))?;
    check_native_companion(&directory.join("libAvaloniaNative.dylib"))?;
    let (_host, _application) = open_application()?;
    Ok(())
}

impl Coordinator {
    fn request(&self, token: i64, flags: i32) -> sys::Result<()> {
        if std::env::var_os("JIFFSHOT_UITEST").is_some() {
            TEST_REQUESTS.with(|requests| requests.borrow_mut().push((token, flags)));
        }
        let request = lock_pending(&self.pending).request(token);
        let first = request.map_err(|message| {
                eprintln!("macOS termination: {message}");
                sys::Error(sys::E_FAIL)
            })?;
        if first {
            if let Err(error) = crate::shutdown_ui_macos::post("the native termination request", move || crate::app::native_exit_requested(token)) {
                lock_pending(&self.pending).unschedule(token);
                eprintln!("could not queue macOS termination cleanup: {error}");
                return Err(sys::Error(sys::E_FAIL));
            }
        }
        Ok(())
    }
}

pub(crate) fn requests_for_selftest() -> Vec<(i64, i32)> {
    TEST_REQUESTS.with(|requests| requests.borrow().clone())
}

pub(crate) fn pending_token() -> io::Result<Option<i64>> {
    let coordinator = COORDINATOR.with(|slot| {
        slot.try_borrow()
            .map(|slot| slot.clone())
            .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "macOS termination coordination is busy"))
    })?
        .ok_or_else(|| io::Error::other("macOS termination coordination is not installed"))?;
    let pending = lock_pending(&coordinator.pending).token;
    Ok(pending)
}

pub(crate) fn pending_for_selftest() -> io::Result<Option<i64>> {
    pending_token()
}

pub(crate) fn install(scope: &AppScope) -> io::Result<()> {
    if !scope.check_access().map_err(io::Error::other)? {
        return Err(io::Error::other("macOS termination must be installed on the UI thread"));
    }
    if COORDINATOR.with(|slot| slot.borrow().is_some()) {
        return Err(io::Error::other("macOS termination is already installed"));
    }
    let (host, application) = open_application()?;
    let coordinator = Arc::new(Coordinator { application, scope: scope.clone(), pending: Mutex::new(Pending::default()), _host: host });
    let handler = handler(Arc::downgrade(&coordinator));
    // SAFETY: both interfaces are live and the host retains the handler on success.
    let result = unsafe {
        let application = coordinator.application.as_raw();
        ((*(*application).vtable).set_handler)(application, handler.as_raw())
    };
    if result < 0 {
        return Err(io::Error::other(format!("could not install macOS termination coordination: {}", sys::Error(result))));
    }
    COORDINATOR.with(|slot| *slot.borrow_mut() = Some(coordinator));
    Ok(())
}

pub(crate) fn uninstall() -> io::Result<()> {
    let coordinator = COORDINATOR.with(|slot| slot.borrow().clone())
        .ok_or_else(|| io::Error::other("macOS termination coordination is not installed"))?;
    // SAFETY: the host interface is live and a null handler unregisters it.
    let result = unsafe {
        let application = coordinator.application.as_raw();
        ((*(*application).vtable).set_handler)(application, ptr::null_mut())
    };
    if result < 0 {
        return Err(io::Error::other(format!("could not remove macOS termination coordination: {}", sys::Error(result))));
    }
    COORDINATOR.with(|slot| { slot.borrow_mut().take(); });
    Ok(())
}

pub(crate) fn complete(token: i64, accept: bool) -> io::Result<()> {
    let coordinator = COORDINATOR.with(|slot| slot.borrow().clone())
        .ok_or_else(|| io::Error::other("macOS termination coordination is not installed"))?;
    if !coordinator.scope.check_access().map_err(io::Error::other)? {
        return Err(io::Error::other("macOS termination must complete on the UI thread"));
    }
    lock_pending(&coordinator.pending).require(token)?;
    // SAFETY: the host interface is live, the token is pending, and this is
    // posted after OnRequested returned to AppKit.
    let result = unsafe {
        let application = coordinator.application.as_raw();
        ((*(*application).vtable).complete)(application, token, i32::from(accept))
    };
    if result < 0 {
        return Err(io::Error::other(format!("could not complete macOS termination: {}", sys::Error(result))));
    }
    lock_pending(&coordinator.pending).token = None;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    #[test]
    fn repeated_requests_share_one_pending_token() {
        let mut pending = Pending::default();
        assert!(pending.request(4).unwrap());
        assert!(!pending.request(4).unwrap());
        assert!(pending.request(5).is_err());
        assert!(pending.require(4).is_ok());
        assert!(pending.require(5).is_err());
        pending.unschedule(5);
        assert!(!pending.request(4).unwrap());
        pending.unschedule(4);
        assert!(pending.request(4).unwrap());
        pending.token = None;
        assert!(pending.request(5).unwrap());
        assert!(pending.request(0).is_err());
    }

    #[test]
    fn poisoned_pending_state_keeps_its_termination_token() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        lock_pending(&pending).request(9).unwrap();
        let poisoned = pending.clone();
        assert!(std::thread::spawn(move || {
            let _guard = poisoned.lock().unwrap();
            panic!("inject termination state poisoning");
        }).join().is_err());
        lock_pending(&pending).require(9).unwrap();
        assert!(!pending.is_poisoned());
        assert!(!lock_pending(&pending).request(9).unwrap());
    }

    #[test]
    fn native_companion_probe_requires_the_callable_capability() {
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                for name in ["missing-symbol.dylib", "unsupported.dylib", "supported.dylib"] {
                    let _ = std::fs::remove_file(self.0.join(name));
                }
                let _ = std::fs::remove_dir(&self.0);
            }
        }

        let name = format!(
            "jiffshot-termination-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        );
        let scratch = Scratch(std::env::temp_dir().join(name));
        std::fs::create_dir(&scratch.0).unwrap();
        let arch = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "x86_64",
            arch => panic!("unsupported macOS architecture: {arch}"),
        };
        for (name, source) in [
            ("missing-symbol.dylib", "int another_export(void) { return 1; }"),
            ("unsupported.dylib", "int avn_supports_deferred_termination(void) { return 2; }"),
            ("supported.dylib", "int avn_supports_deferred_termination(void) { return 1; }"),
        ] {
            let path = scratch.0.join(name);
            let mut compiler = Command::new("xcrun")
                .args(["clang", "-arch", arch, "-dynamiclib", "-x", "c", "-o"])
                .arg(&path).arg("-")
                .stdin(Stdio::piped()).stderr(Stdio::piped())
                .spawn().expect("start the fixture compiler");
            compiler.stdin.take().unwrap().write_all(source.as_bytes()).unwrap();
            let output = compiler.wait_with_output().unwrap();
            assert!(output.status.success(), "fixture compilation failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        assert!(check_native_companion(&scratch.0.join("absent.dylib")).is_err());
        assert!(check_native_companion(&scratch.0.join("missing-symbol.dylib")).unwrap_err()
            .to_string().contains("lacks deferred termination support"));
        assert!(check_native_companion(&scratch.0.join("unsupported.dylib")).unwrap_err()
            .to_string().contains("unsupported termination capability 2"));
        check_native_companion(&scratch.0.join("supported.dylib")).unwrap();
    }
}
