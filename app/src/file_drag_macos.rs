//! Outgoing library file drags through Rustolonia's additive macOS ABI.

use crate::library_transfer_macos::Prepared;
use rustolonia::{AppScope, AsControl, Border, EventSubscription};
use rustolonia_sys as sys;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{fence, AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

const APPLICATION_IID: sys::Guid = sys::Guid {
    data1: 0x6B2E8F10,
    data2: 0x4C91,
    data3: 0x4E3A,
    data4: [0x9A, 0x77, 0x1F, 0x0C, 0x2B, 0x3A, 0x4D, 0xA6],
};
const HANDLER_IID: sys::Guid = sys::Guid {
    data1: 0x6B2E8F10,
    data2: 0x4C91,
    data3: 0x4E3A,
    data4: [0x9A, 0x77, 0x1F, 0x0C, 0x2B, 0x3A, 0x4D, 0xA7],
};

#[repr(C)]
struct ApplicationVtbl {
    query_interface: unsafe extern "system" fn(*mut sys::IUnknown, *const sys::Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut sys::IUnknown) -> u32,
    release: unsafe extern "system" fn(*mut sys::IUnknown) -> u32,
    set_handler: unsafe extern "system" fn(*mut Application, *mut sys::IAvnControl, *mut Handler) -> i32,
    complete: unsafe extern "system" fn(*mut Application, i64, *const u16) -> i32,
    cancel: unsafe extern "system" fn(*mut Application, i64) -> i32,
}

#[repr(C)]
struct Application {
    vtable: *const ApplicationVtbl,
}

// SAFETY: IAvnApplication12 has an IUnknown prefix.
unsafe impl sys::ComInterface for Application {
    const IID: sys::Guid = APPLICATION_IID;
}

#[repr(C)]
struct HandlerVtbl {
    query_interface: unsafe extern "system" fn(*mut sys::IUnknown, *const sys::Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut sys::IUnknown) -> u32,
    release: unsafe extern "system" fn(*mut sys::IUnknown) -> u32,
    preparing: unsafe extern "system" fn(*mut Handler, i64) -> i32,
    completed: unsafe extern "system" fn(*mut Handler, i64, i32, i32) -> i32,
}

#[repr(C)]
struct Handler {
    vtable: *const HandlerVtbl,
    references: AtomicU32,
    coordinator: Weak<Coordinator>,
    tile: u64,
    path: PathBuf,
    suppress_tap: Arc<AtomicI64>,
}

// SAFETY: the handler implements IUnknown followed by the two file-drag callbacks.
unsafe impl sys::ComInterface for Handler {
    const IID: sys::Guid = HANDLER_IID;
}

static HANDLER_VTABLE: HandlerVtbl = HandlerVtbl { query_interface, add_ref, release, preparing, completed };
static NEXT_TILE: AtomicU64 = AtomicU64::new(1);

fn handler(coordinator: Weak<Coordinator>, tile: u64, path: PathBuf, suppress_tap: Arc<AtomicI64>) -> sys::ComPtr<Handler> {
    let value = Box::new(Handler {
        vtable: &HANDLER_VTABLE,
        references: AtomicU32::new(1),
        coordinator,
        tile,
        path,
        suppress_tap,
    });
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

unsafe extern "system" fn preparing(this: *mut Handler, token: i64) -> i32 {
    if this.is_null() {
        return sys::E_POINTER;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the host retains the handler throughout its callback.
        let handler = unsafe { &*this };
        let coordinator = handler.coordinator.upgrade().ok_or("the file-drag coordinator is unavailable")?;
        coordinator.prepare(token, handler.tile, handler.path.clone(), handler.suppress_tap.clone())
    }));
    match result {
        Ok(Ok(())) => sys::S_OK,
        Ok(Err(error)) => {
            crate::shutdown_ui_macos::report(&format!("macOS file-drag preparation failed: {error}"));
            sys::E_FAIL
        }
        Err(_) => {
            crate::shutdown_ui_macos::report("the macOS file-drag preparation callback panicked");
            sys::E_FAIL
        }
    }
}

unsafe extern "system" fn completed(this: *mut Handler, token: i64, effect: i32, status: i32) -> i32 {
    if this.is_null() {
        return sys::E_POINTER;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the host retains the handler throughout its callback.
        let handler = unsafe { &*this };
        let coordinator = handler.coordinator.upgrade().ok_or("the file-drag coordinator is unavailable")?;
        coordinator.completed(token, handler.tile, handler.suppress_tap.clone(), effect, status)
    }));
    match result {
        Ok(Ok(())) => sys::S_OK,
        Ok(Err(error)) => {
            crate::shutdown_ui_macos::report(&format!("macOS file-drag completion failed: {error}"));
            sys::E_FAIL
        }
        Err(_) => {
            crate::shutdown_ui_macos::report("the macOS file-drag completion callback panicked");
            sys::E_FAIL
        }
    }
}

struct Pending {
    tile: u64,
    prepared: Option<Prepared>,
    completed: bool,
}

struct Coordinator {
    application: sys::ComPtr<Application>,
    scope: AppScope,
    pending: Mutex<HashMap<i64, Pending>>,
    suspended: AtomicBool,
    reported_attachment_error: AtomicBool,
    _host: sys::Host,
}

fn lock_pending(coordinator: &Coordinator) -> MutexGuard<'_, HashMap<i64, Pending>> {
    match coordinator.pending.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            coordinator.pending.clear_poison();
            crate::shutdown_ui_macos::report("the macOS file-drag state was poisoned; recovering its retained transfers");
            poisoned.into_inner()
        }
    }
}

struct Tile {
    control: sys::ComPtr<sys::IAvnControl>,
    path: PathBuf,
    suppress_tap: Arc<AtomicI64>,
    loaded: bool,
    registered: bool,
    retired: bool,
    _loaded: EventSubscription,
    _unloaded: EventSubscription,
    _tapped: EventSubscription,
}

thread_local! {
    static COORDINATOR: RefCell<Option<Arc<Coordinator>>> = const { RefCell::new(None) };
    static TILES: RefCell<HashMap<u64, Tile>> = RefCell::new(HashMap::new());
    static TEST_EVENTS: RefCell<Vec<TestEvent>> = const { RefCell::new(Vec::new()) };
}

#[derive(Clone, Debug)]
pub(crate) enum TestEvent {
    Preparing { token: i64, path: PathBuf },
    Submitted { token: i64, files: Vec<PathBuf> },
    Discarded { token: i64, files: Vec<PathBuf> },
    Completed { token: i64, effect: i32, status: i32 },
}

static TEST_PREPARATION_GATE: Mutex<Option<std::sync::mpsc::Receiver<()>>> = Mutex::new(None);
static TEST_PREPARATION_WAITING: AtomicBool = AtomicBool::new(false);

fn record_test_event(event: TestEvent) {
    if is_selftest() {
        TEST_EVENTS.with(|events| events.borrow_mut().push(event));
    }
}

fn is_selftest() -> bool {
    std::env::var_os("JIFFSHOT_UITEST").is_some() && std::env::var_os("JIFFSHOT_UITEST_LIBRARY_DRAG").is_some()
}

pub(crate) fn events_for_selftest() -> Vec<TestEvent> {
    TEST_EVENTS.with(|events| events.borrow().clone())
}

pub(crate) fn registered_for_selftest(path: &std::path::Path) -> usize {
    TILES.with(|tiles| tiles.borrow().values().filter(|tile| tile.path == path && tile.registered).count())
}

pub(crate) fn hold_preparation_for_selftest() -> std::sync::mpsc::Sender<()> {
    assert!(is_selftest());
    let (sender, receiver) = std::sync::mpsc::channel();
    assert!(TEST_PREPARATION_GATE.lock().expect("drag fixture gate").replace(receiver).is_none());
    sender
}

pub(crate) fn preparation_waiting_for_selftest() -> bool {
    TEST_PREPARATION_WAITING.load(Ordering::Acquire)
}

pub(crate) fn wait_for_selftest_preparation() -> Result<(), String> {
    if !is_selftest() {
        return Ok(());
    }
    let receiver = TEST_PREPARATION_GATE.lock().map_err(|_| "the drag fixture gate was poisoned")?.take();
    if let Some(receiver) = receiver {
        TEST_PREPARATION_WAITING.store(true, Ordering::Release);
        let result = receiver.recv_timeout(Duration::from_secs(15));
        TEST_PREPARATION_WAITING.store(false, Ordering::Release);
        result.map_err(|error| format!("the drag fixture preparation gate failed: {error}"))?;
    }
    Ok(())
}

fn coordinator() -> io::Result<Arc<Coordinator>> {
    COORDINATOR.with(|slot| {
        slot.try_borrow()
            .map(|slot| slot.clone())
            .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "macOS file-drag coordination is busy"))
    })?.ok_or_else(|| io::Error::other("macOS file-drag coordination is not installed"))
}

fn open_application() -> io::Result<(sys::Host, sys::ComPtr<Application>)> {
    let path = rustolonia::discover_host_path().map_err(io::Error::other)?;
    let host = sys::Host::load(path).map_err(io::Error::other)?;
    let application = host.activation_factory().and_then(|factory| factory.create_application()).map_err(io::Error::other)?;
    let application = application.query_interface::<Application>().map_err(|error| {
        if error.0 == sys::E_NOINTERFACE {
            io::Error::other("the Rustolonia host does not support macOS file dragging")
        } else {
            io::Error::other(format!("could not initialize macOS file dragging: {error}"))
        }
    })?;
    Ok((host, application))
}

/// Run only in a dedicated package-probe process.
pub(crate) fn check_native_companion() -> io::Result<()> {
    let host = rustolonia::discover_host_path().map_err(io::Error::other)?;
    let directory = host.parent().ok_or_else(|| io::Error::other("the host has no parent directory"))?;
    crate::termination_macos::check_native_capability(
        &directory.join("libAvaloniaNative.dylib"),
        b"avn_supports_retained_file_drag_event\0",
        "retained file-drag event",
        "file-drag",
    )
}

/// Run in a separate package-probe process from the native companion probe.
pub(crate) fn check_host_abi() -> io::Result<()> {
    let (_host, _application) = open_application()?;
    Ok(())
}

pub(crate) fn install(scope: &AppScope) -> io::Result<()> {
    if !scope.check_access().map_err(io::Error::other)? {
        return Err(io::Error::other("macOS file dragging must be installed on the UI thread"));
    }
    if COORDINATOR.with(|slot| slot.borrow().is_some()) {
        return Err(io::Error::other("macOS file dragging is already installed"));
    }
    let (host, application) = open_application()?;
    COORDINATOR.with(|slot| *slot.borrow_mut() = Some(Arc::new(Coordinator {
        application,
        scope: scope.clone(),
        pending: Mutex::new(HashMap::new()),
        suspended: AtomicBool::new(false),
        reported_attachment_error: AtomicBool::new(false),
        _host: host,
    })));
    Ok(())
}

impl Coordinator {
    fn set_handler(&self, control: &sys::ComPtr<sys::IAvnControl>, handler: Option<&sys::ComPtr<Handler>>) -> io::Result<()> {
        // SAFETY: both owned interfaces are live, and this method is called on the UI thread.
        let result = unsafe {
            let application = self.application.as_raw();
            ((*(*application).vtable).set_handler)(
                application,
                control.as_raw(),
                handler.map_or(ptr::null_mut(), sys::ComPtr::as_raw),
            )
        };
        if result < 0 {
            Err(io::Error::other(format!("could not change a macOS file-drag handler: {}", sys::Error(result))))
        } else {
            Ok(())
        }
    }

    fn has_tile(&self, tile: u64) -> bool {
        lock_pending(self).values().any(|pending| pending.tile == tile)
    }

    fn prepare(&self, token: i64, tile: u64, path: PathBuf, suppress_tap: Arc<AtomicI64>) -> Result<(), String> {
        if token <= 0 {
            return Err("the file-drag token is invalid".into());
        }
        {
            let mut pending = lock_pending(self);
            if pending.contains_key(&token) {
                return Err("the file-drag token is already pending".into());
            }
            pending.insert(token, Pending { tile, prepared: None, completed: false });
        }
        suppress_tap.store(token, Ordering::Release);
        record_test_event(TestEvent::Preparing { token, path: path.clone() });
        crate::shutdown_ui_macos::post("a library drag preparation", move || crate::app::prepare_library_drag(token, path))
    }

    fn completed(self: &Arc<Self>, token: i64, tile: u64, suppress_tap: Arc<AtomicI64>, effect: i32, status: i32) -> Result<(), String> {
        let mut prepared = {
            let mut pending = lock_pending(self);
            let pending = pending.get_mut(&token).ok_or("the completed file-drag token was not pending")?;
            if pending.tile != tile {
                return Err("the completed file-drag token belongs to another tile".into());
            }
            if pending.completed {
                return Err("the file-drag token has already completed".into());
            }
            pending.completed = true;
            pending.prepared.take()
        };
        record_test_event(TestEvent::Completed { token, effect, status });
        // Keep exports if the dispatcher cannot run: the host disposes its data
        // transfer only after this callback returns.
        if let Some(prepared) = &mut prepared {
            prepared.retain_for_destination();
        }
        let message = if status < 0 {
            Some(format!("File drag failed: {}", sys::Error(status)))
        } else if effect != 0 && effect != 1 {
            Some(format!("File drag returned an unexpected effect: {effect}"))
        } else if effect == 1 && prepared.is_none() {
            Some("File drag completed without a prepared transfer".into())
        } else {
            None
        };
        let substitutions = if message.is_none() && effect == 1 {
            prepared.as_ref().map(|prepared| prepared.font_substitutions().to_vec()).unwrap_or_default()
        } else {
            Vec::new()
        };
        let coordinator = self.clone();
        crate::shutdown_ui_macos::post("a library drag completion", move || {
            if effect == 0 {
                if let Some(prepared) = &mut prepared {
                    prepared.discard_after_completion();
                }
            }
            drop(prepared);
            lock_pending(&coordinator).remove(&token);
            let _ = suppress_tap.compare_exchange(token, 0, Ordering::AcqRel, Ordering::Acquire);
            finish_tile(tile);
            if let Some(message) = message {
                crate::app::report_library_drag_error(message);
            } else if !substitutions.is_empty() {
                crate::app::report_library_drag_fonts(substitutions);
            }
        })
    }
}

pub(crate) fn register_tile(
    control: &Border,
    path: PathBuf,
    suppress_tap: Arc<AtomicI64>,
    tapped: EventSubscription,
) -> rustolonia::Result<()> {
    coordinator().map_err(|error| rustolonia::Error::Load(error.to_string()))?;
    let key = NEXT_TILE.fetch_update(Ordering::AcqRel, Ordering::Acquire, |key| key.checked_add(1))
        .map_err(|_| rustolonia::Error::Load("too many library drag tiles".into()))?;
    let raw = control.as_control()?;
    let loaded = control.subscribe_loaded(move |_| tile_loaded(key))?;
    let unloaded = control.subscribe_unloaded(move |_| tile_unloaded(key))?;
    let tile = Tile {
        control: raw,
        path,
        suppress_tap,
        loaded: false,
        registered: false,
        retired: false,
        _loaded: loaded,
        _unloaded: unloaded,
        _tapped: tapped,
    };
    TILES.with(|tiles| { tiles.borrow_mut().insert(key, tile); });
    Ok(())
}

fn tile_loaded(key: u64) {
    TILES.with(|tiles| {
        if let Some(tile) = tiles.borrow_mut().get_mut(&key) {
            tile.loaded = true;
        }
    });
    if let Err(error) = attach_tile(key) {
        report_attachment_error(error);
    }
}

fn tile_unloaded(key: u64) {
    TILES.with(|tiles| {
        if let Some(tile) = tiles.borrow_mut().get_mut(&key) {
            tile.loaded = false;
            tile.registered = false;
        }
    });
    if let Err(error) = crate::shutdown_ui_macos::post("a detached library drag tile", move || finish_tile(key)) {
        crate::shutdown_ui_macos::report(&format!("could not retire a detached library drag tile: {error}"));
    }
}

fn attach_tile(key: u64) -> io::Result<()> {
    let coordinator = coordinator()?;
    if coordinator.suspended.load(Ordering::Acquire) || coordinator.has_tile(key) {
        return Ok(());
    }
    let details = TILES.with(|tiles| {
        tiles.borrow().get(&key).filter(|tile| tile.loaded && !tile.registered && !tile.retired)
            .map(|tile| (tile.control.clone(), tile.path.clone(), tile.suppress_tap.clone()))
    });
    let Some((control, path, suppress_tap)) = details else { return Ok(()) };
    let callback = handler(Arc::downgrade(&coordinator), key, path, suppress_tap);
    coordinator.set_handler(&control, Some(&callback))?;
    TILES.with(|tiles| {
        if let Some(tile) = tiles.borrow_mut().get_mut(&key) {
            tile.registered = true;
        }
    });
    Ok(())
}

fn report_attachment_error(error: io::Error) {
    let message = format!("Could not enable library file dragging: {error}");
    crate::shutdown_ui_macos::report(&message);
    let first = coordinator().is_ok_and(|coordinator| !coordinator.reported_attachment_error.swap(true, Ordering::AcqRel));
    if first {
        if let Err(error) = crate::shutdown_ui_macos::post("a library drag registration error", move || {
            crate::app::report_library_drag_error(message);
        }) {
            crate::shutdown_ui_macos::report(&format!("could not report the library drag error: {error}"));
        }
    }
}

fn finish_tile(key: u64) {
    let coordinator = match coordinator() {
        Ok(coordinator) => coordinator,
        Err(error) => {
            crate::shutdown_ui_macos::report(&error.to_string());
            return;
        }
    };
    let retired = TILES.with(|tiles| {
        tiles.borrow().get(&key).filter(|tile| tile.retired)
            .map(|tile| tile.registered.then(|| tile.control.clone()))
    });
    if let Some(control) = retired.filter(|_| !coordinator.has_tile(key)) {
        if let Some(control) = control {
            if let Err(error) = coordinator.set_handler(&control, None) {
                crate::shutdown_ui_macos::report(&format!("could not finish retiring a library drag tile: {error}"));
                return;
            }
        }
        let tile = TILES.with(|tiles| tiles.borrow_mut().remove(&key));
        drop(tile);
    } else if let Err(error) = attach_tile(key) {
        report_attachment_error(error);
    }
}

/// Retire the existing list before its controls are removed or rebuilt.
pub(crate) fn retire_tiles() {
    let coordinator = match coordinator() {
        Ok(coordinator) => coordinator,
        Err(error) => {
            crate::shutdown_ui_macos::report(&error.to_string());
            return;
        }
    };
    let keys = TILES.with(|tiles| tiles.borrow().keys().copied().collect::<Vec<_>>());
    for key in keys {
        let control = TILES.with(|tiles| {
            let mut tiles = tiles.borrow_mut();
            let tile = tiles.get_mut(&key)?;
            tile.retired = true;
            tile.registered.then(|| tile.control.clone())
        });
        if let Some(control) = control {
            match coordinator.set_handler(&control, None) {
                Ok(()) => TILES.with(|tiles| {
                    if let Some(tile) = tiles.borrow_mut().get_mut(&key) {
                        tile.registered = false;
                    }
                }),
                Err(error) if coordinator.has_tile(key) => {
                    // A started AppKit drag remains registered until completion.
                    crate::shutdown_ui_macos::report(&format!("deferring active library drag retirement: {error}"));
                }
                Err(error) => crate::shutdown_ui_macos::report(&format!("could not retire a library drag tile: {error}")),
            }
        }
        finish_tile(key);
    }
}

pub(crate) fn has_pending() -> io::Result<bool> {
    let coordinator = coordinator()?;
    let pending = !lock_pending(&coordinator).is_empty();
    Ok(pending)
}

pub(crate) fn is_pending(token: i64) -> bool {
    match coordinator() {
        Ok(coordinator) => lock_pending(&coordinator).get(&token).is_some_and(|pending| !pending.completed),
        Err(error) => {
            crate::shutdown_ui_macos::report(&error.to_string());
            false
        }
    }
}

pub(crate) fn preparation_finished(token: i64, result: Result<Prepared, String>) {
    let coordinator = match coordinator() {
        Ok(coordinator) => coordinator,
        Err(error) => {
            crate::shutdown_ui_macos::report(&format!("could not deliver a library drag: {error}"));
            return;
        }
    };
    if !lock_pending(&coordinator).get(&token).is_some_and(|pending| !pending.completed) {
        if is_selftest() {
            if let Ok(prepared) = &result {
                record_test_event(TestEvent::Discarded { token, files: prepared.files_for_selftest() });
            }
        }
        return;
    }
    let prepared = match result {
        Ok(prepared) => prepared,
        Err(error) => return fail_preparation(&coordinator, token, error),
    };
    let urls = match prepared.file_urls_json() {
        Ok(urls) => urls,
        Err(error) => return fail_preparation(&coordinator, token, error),
    };
    if is_selftest() {
        record_test_event(TestEvent::Submitted { token, files: prepared.files_for_selftest() });
    }
    {
        let mut pending = lock_pending(&coordinator);
        let Some(transfer) = pending.get_mut(&token).filter(|transfer| !transfer.completed) else { return };
        transfer.prepared = Some(prepared);
    }
    // SAFETY: the application is live, the token is pending, and the host
    // copies the NUL-terminated UTF-16 payload before returning.
    let result = unsafe {
        let application = coordinator.application.as_raw();
        ((*(*application).vtable).complete)(application, token, urls.as_ptr())
    };
    if result < 0 {
        fail_preparation(&coordinator, token, format!("could not start the file drag: {}", sys::Error(result)));
    }
}

fn fail_preparation(coordinator: &Coordinator, token: i64, error: String) {
    if !lock_pending(coordinator).get(&token).is_some_and(|pending| !pending.completed) {
        return;
    }
    crate::app::report_library_drag_error(format!("File drag failed: {error}"));
    // SAFETY: this is outside the host callback on the UI thread.
    let result = unsafe {
        let application = coordinator.application.as_raw();
        ((*(*application).vtable).cancel)(application, token)
    };
    if result < 0 && lock_pending(coordinator).contains_key(&token) {
        crate::shutdown_ui_macos::report(&format!("could not cancel the failed library drag: {}", sys::Error(result)));
    }
}

/// Prevent new gestures and clear every installed registration before host shutdown.
pub(crate) fn suspend_for_shutdown() -> io::Result<()> {
    let coordinator = coordinator()?;
    if !lock_pending(&coordinator).is_empty() {
        return Err(io::Error::other("finish or cancel the library drag before quitting"));
    }
    coordinator.suspended.store(true, Ordering::Release);
    let registered = TILES.with(|tiles| {
        tiles.borrow().iter().filter(|(_, tile)| tile.registered)
            .map(|(key, tile)| (*key, tile.control.clone())).collect::<Vec<_>>()
    });
    for (key, control) in registered {
        if let Err(error) = coordinator.set_handler(&control, None) {
            let restore = resume_after_failed_shutdown();
            return Err(match restore {
                Ok(()) => error,
                Err(restore) => io::Error::other(format!("{error}; could not restore file dragging: {restore}")),
            });
        }
        TILES.with(|tiles| {
            if let Some(tile) = tiles.borrow_mut().get_mut(&key) {
                tile.registered = false;
            }
        });
    }
    Ok(())
}

pub(crate) fn resume_after_failed_shutdown() -> io::Result<()> {
    let coordinator = coordinator()?;
    if !coordinator.suspended.swap(false, Ordering::AcqRel) {
        return Ok(());
    }
    let keys = TILES.with(|tiles| tiles.borrow().keys().copied().collect::<Vec<_>>());
    let mut errors = Vec::new();
    for key in keys {
        if let Err(error) = attach_tile(key) {
            errors.push(error.to_string());
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(io::Error::other(errors.join("; "))) }
}

pub(crate) fn uninstall() -> io::Result<()> {
    let Some(coordinator) = COORDINATOR.with(|slot| slot.borrow().clone()) else { return Ok(()) };
    if !coordinator.scope.check_access().map_err(io::Error::other)? {
        return Err(io::Error::other("macOS file dragging must be uninstalled on the UI thread"));
    }
    suspend_for_shutdown()?;
    let tiles = TILES.with(|tiles| std::mem::take(&mut *tiles.borrow_mut()));
    drop(tiles);
    COORDINATOR.with(|slot| { slot.borrow_mut().take(); });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_supports_both_interfaces_and_balances_references() {
        let callback = handler(Weak::new(), 7, PathBuf::from("/capture.png"), Arc::new(AtomicI64::new(0)));
        let unknown = callback.query_interface::<sys::IUnknown>().unwrap();
        let same = unknown.query_interface::<Handler>().unwrap();
        assert_eq!(callback.as_raw(), same.as_raw());
        assert_eq!(unsafe { (*callback.as_raw()).references.load(Ordering::Relaxed) }, 3);
        drop(same);
        drop(unknown);
        assert_eq!(unsafe { (*callback.as_raw()).references.load(Ordering::Relaxed) }, 1);
        // SAFETY: this callback is still owned and has no live coordinator.
        assert_eq!(unsafe { preparing(callback.as_raw(), 1) }, sys::E_FAIL);
        assert_eq!(unsafe { completed(callback.as_raw(), 1, 0, sys::S_OK) }, sys::E_FAIL);
    }
}
