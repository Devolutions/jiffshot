//! Capture-safe native popups for the recording toolbar and Capture Widget.
//!
//! The pinned Rustolonia crate predates the safe popup-gate wrapper. This
//! narrow optional ABI bridge can be replaced by that wrapper once a matching
//! Rustolonia crate and host release is available.

use jiffshot_capture::screen::{self, ExclusionTicket};
use rustolonia::{AppScope, AsControl, EventSubscription, Window};
use rustolonia_sys as sys;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::atomic::{fence, AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

const APPLICATION_IID: sys::Guid = sys::Guid {
    data1: 0x6B2E8F10,
    data2: 0x4C91,
    data3: 0x4E3A,
    data4: [0x9A, 0x77, 0x1F, 0x0C, 0x2B, 0x3A, 0x4D, 0xA2],
};
const HANDLER_IID: sys::Guid = sys::Guid {
    data1: 0x6B2E8F10,
    data2: 0x4C91,
    data3: 0x4E3A,
    data4: [0x9A, 0x77, 0x1F, 0x0C, 0x2B, 0x3A, 0x4D, 0xA3],
};

#[repr(C)]
struct ApplicationVtbl {
    query_interface: unsafe extern "system" fn(*mut sys::IUnknown, *const sys::Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut sys::IUnknown) -> u32,
    release: unsafe extern "system" fn(*mut sys::IUnknown) -> u32,
    set_gate: unsafe extern "system" fn(*mut Application, *mut sys::IAvnWindow, *mut Handler) -> i32,
    acknowledge: unsafe extern "system" fn(*mut Application, i64) -> i32,
}

#[repr(C)]
struct Application {
    vtable: *const ApplicationVtbl,
}

// SAFETY: IAvnApplication10 is a COM interface with an IUnknown prefix.
unsafe impl sys::ComInterface for Application {
    const IID: sys::Guid = APPLICATION_IID;
}

#[repr(C)]
struct HandlerVtbl {
    query_interface: unsafe extern "system" fn(*mut sys::IUnknown, *const sys::Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut sys::IUnknown) -> u32,
    release: unsafe extern "system" fn(*mut sys::IUnknown) -> u32,
    preparing: unsafe extern "system" fn(*mut Handler, i64, i32) -> i32,
    closed: unsafe extern "system" fn(*mut Handler, i64) -> i32,
}

#[repr(C)]
struct Handler {
    vtable: *const HandlerVtbl,
    references: AtomicU32,
    gate: Weak<Gate>,
}

// SAFETY: the handler implements IUnknown followed by the two host callbacks.
unsafe impl sys::ComInterface for Handler {
    const IID: sys::Guid = HANDLER_IID;
}

static HANDLER_VTABLE: HandlerVtbl = HandlerVtbl {
    query_interface,
    add_ref,
    release,
    preparing,
    closed,
};

fn handler(gate: Weak<Gate>) -> sys::ComPtr<Handler> {
    let value = Box::new(Handler { vtable: &HANDLER_VTABLE, references: AtomicU32::new(1), gate });
    // SAFETY: the newly allocated handler owns its initial COM reference.
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
    // SAFETY: every COM reference is released once; the last release owns
    // the allocation.
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

fn callback(this: *mut Handler, f: impl FnOnce(Arc<Gate>) -> sys::Result<()>) -> i32 {
    if this.is_null() {
        return sys::E_POINTER;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the host retains the handler throughout its callback.
        let gate = unsafe { (*this).gate.upgrade() }.ok_or(sys::Error(sys::E_FAIL))?;
        f(gate)
    }));
    match result {
        Ok(Ok(())) => sys::S_OK,
        Ok(Err(error)) => {
            eprintln!("capture popup callback failed: {error}");
            error.0
        }
        Err(_) => {
            eprintln!("capture popup callback panicked");
            sys::E_FAIL
        }
    }
}

unsafe extern "system" fn preparing(this: *mut Handler, token: i64, number: i32) -> i32 {
    callback(this, |gate| gate.prepare(token, number))
}

unsafe extern "system" fn closed(this: *mut Handler, token: i64) -> i32 {
    callback(this, |gate| gate.closed(token))
}

#[derive(Clone, Copy)]
struct Pending {
    number: u32,
    ticket: ExclusionTicket,
}

struct Retirement {
    ticket: ExclusionTicket,
    done: AtomicBool,
}

impl Retirement {
    fn retire(&self) {
        if !self.done.swap(true, Ordering::AcqRel) && !screen::retire_exclusion(self.ticket) {
            eprintln!("capture popup: its exclusion was already retired");
        }
    }
}

struct Owner {
    window: Window,
    interface: sys::ComPtr<sys::IAvnWindow>,
    _identity: sys::ComPtr<sys::IUnknown>,
    subscription: EventSubscription,
    unloaded: bool,
}

struct Gate {
    application: sys::ComPtr<Application>,
    scope: AppScope,
    pending: Mutex<HashMap<i64, Pending>>,
    retiring: Mutex<Vec<Arc<Retirement>>>,
    stopping: AtomicBool,
    _host: sys::Host,
}

thread_local! {
    static GATE: RefCell<Option<Arc<Gate>>> = const { RefCell::new(None) };
    static OWNERS: RefCell<HashMap<usize, Owner>> = RefCell::new(HashMap::new());
    static RETIRED_SUBSCRIPTIONS: RefCell<Vec<EventSubscription>> = const { RefCell::new(Vec::new()) };
    static TEST_GATE: RefCell<Option<Weak<Gate>>> = const { RefCell::new(None) };
    static STOPPING: Cell<bool> = const { Cell::new(false) };
}

fn window_interfaces(window: &Window) -> io::Result<(sys::ComPtr<sys::IAvnWindow>, sys::ComPtr<sys::IUnknown>)> {
    let interface = window.as_control().map_err(io::Error::other)?
        .query_interface::<sys::IAvnWindow>().map_err(io::Error::other)?;
    let identity = interface.query_interface::<sys::IUnknown>().map_err(io::Error::other)?;
    Ok((interface, identity))
}

fn open_application_gate() -> io::Result<(sys::Host, sys::ComPtr<Application>)> {
    let path = rustolonia::discover_host_path().map_err(io::Error::other)?;
    let host = sys::Host::load(path).map_err(io::Error::other)?;
    let application = host.activation_factory().and_then(|factory| factory.create_application()).map_err(io::Error::other)?;
    let application = application.query_interface::<Application>().map_err(|error| {
        if error.0 == sys::E_NOINTERFACE {
            io::Error::other("the Rustolonia host does not support capture-safe native popups; a newer matching host is required")
        } else {
            io::Error::other(format!("could not initialize the native popup gate: {error}"))
        }
    })?;
    Ok((host, application))
}

pub(in crate::ui) fn check_host_gate() -> io::Result<()> {
    let (_host, _application) = open_application_gate()?;
    Ok(())
}

impl Gate {
    fn new(scope: &AppScope) -> io::Result<Arc<Self>> {
        let (host, application) = open_application_gate()?;
        Ok(Arc::new(Self {
            application,
            scope: scope.clone(),
            pending: Mutex::new(HashMap::new()),
            retiring: Mutex::new(Vec::new()),
            stopping: AtomicBool::new(false),
            _host: host,
        }))
    }

    fn attach(self: &Arc<Self>, window: &Window) -> io::Result<()> {
        let (interface, identity) = window_interfaces(window)?;
        let key = identity.as_raw() as usize;
        if OWNERS.with(|owners| owners.borrow().contains_key(&key)) {
            return Ok(());
        }
        let subscription = window.subscribe_unloaded(move |_| {
            if let Err(error) = owner_unloaded(key) {
                report(error.to_string());
            }
        }).map_err(io::Error::other)?;
        let handler = handler(Arc::downgrade(self));
        // SAFETY: both owned COM references remain live throughout this call.
        let result = unsafe {
            let application = self.application.as_raw();
            ((*(*application).vtable).set_gate)(application, interface.as_raw(), handler.as_raw())
        };
        if result < 0 {
            return Err(io::Error::other(format!("could not protect native popups: {}", sys::Error(result))));
        }
        OWNERS.with(|owners| {
            owners.borrow_mut().insert(key, Owner {
                window: window.clone(),
                interface,
                _identity: identity,
                subscription,
                unloaded: false,
            });
        });
        Ok(())
    }

    fn clear(&self, window: &sys::ComPtr<sys::IAvnWindow>) -> io::Result<()> {
        // SAFETY: the owned window and auxiliary application are live on the UI thread.
        let result = unsafe {
            let application = self.application.as_raw();
            ((*(*application).vtable).set_gate)(application, window.as_raw(), ptr::null_mut())
        };
        if result < 0 {
            return Err(io::Error::other(format!("could not clear native popup protection: {}", sys::Error(result))));
        }
        Ok(())
    }

    fn prepare(self: &Arc<Self>, token: i64, number: i32) -> sys::Result<()> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(sys::Error(sys::E_FAIL));
        }
        let number = u32::try_from(number).ok().filter(|number| *number != 0).ok_or(sys::Error(sys::E_INVALIDARG))?;
        if token <= 0 {
            return Err(sys::Error(sys::E_INVALIDARG));
        }
        let mut pending = self.pending.lock().map_err(|_| sys::Error(sys::E_FAIL))?;
        if pending.contains_key(&token) {
            return Err(sys::Error(sys::E_INVALIDARG));
        }
        let ticket = screen::register_excluded_window(number);
        pending.insert(token, Pending { number, ticket });
        drop(pending);

        let gate = Arc::downgrade(self);
        if let Err(error) = self.scope.spawn(async move {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let Some(gate) = gate.upgrade() else { return };
                if gate.stopping.load(Ordering::Acquire) {
                    return;
                }
                let current = match gate.pending.lock() {
                    Ok(pending) => pending.get(&token).copied(),
                    Err(_) => {
                        report("could not inspect the popup exclusion".into());
                        return;
                    }
                };
                let Some(current) = current else { return };
                if current.number != number || current.ticket != ticket || !screen::is_exclusion_registered(ticket) {
                    report("the popup exclusion was replaced before it could be shown".into());
                    return;
                }
                if screen::exclusion_ready(number) {
                    // SAFETY: the gate is live and this task runs on the UI
                    // dispatcher; the token belongs to this application.
                    let result = unsafe {
                        let application = gate.application.as_raw();
                        ((*(*application).vtable).acknowledge)(application, token)
                    };
                    if result < 0 {
                        report(format!("could not reveal the protected popup: {}", sys::Error(result)));
                    }
                    return;
                }
                if Instant::now() >= deadline {
                    report("recording did not acknowledge the popup exclusion".into());
                    return;
                }
                let delay = match gate.scope.delay(Duration::from_millis(25)) {
                    Ok(delay) => delay,
                    Err(error) => {
                        report(format!("could not schedule the popup exclusion: {error}"));
                        return;
                    }
                };
                drop(gate);
                if let Err(error) = delay.await {
                    report(format!("could not wait for the popup exclusion: {error}"));
                    return;
                }
            }
        }) {
            if let Ok(mut pending) = self.pending.lock() {
                pending.remove(&token);
            }
            screen::retire_exclusion(ticket);
            eprintln!("could not schedule the native popup gate: {error}");
            return Err(sys::Error(sys::E_FAIL));
        }
        Ok(())
    }

    fn closed(&self, token: i64) -> sys::Result<()> {
        let pending = self.pending.lock().map_err(|_| sys::Error(sys::E_FAIL))?
            .remove(&token).ok_or(sys::Error(sys::E_INVALIDARG))?;
        let retirement = Arc::new(Retirement { ticket: pending.ticket, done: AtomicBool::new(false) });
        let mut retiring = self.retiring.lock().map_err(|_| sys::Error(sys::E_FAIL))?;
        retiring.retain(|entry| !entry.done.load(Ordering::Acquire));
        retiring.push(retirement.clone());
        drop(retiring);
        // A queued retirement can also be completed synchronously after an
        // owner closes, before shutdown stops the dispatcher.
        dispatch2::DispatchQueue::main().exec_async(move || retirement.retire());
        Ok(())
    }

    fn retire_closed(&self) -> io::Result<()> {
        let retired = std::mem::take(&mut *self.retiring.lock()
            .map_err(|_| io::Error::other("the popup retirement state is poisoned"))?);
        for retirement in retired {
            retirement.retire();
        }
        Ok(())
    }
}

fn report(message: String) {
    eprintln!("capture popup: {message}");
    crate::app::post(move |app| app.toast("Capture popup unavailable", &message, true));
}

/// Installs the gate before the owner's first popup can be opened.
pub(in crate::ui) fn install(scope: &AppScope, window: &Window) -> io::Result<()> {
    if super::native::capturable() {
        return Ok(());
    }
    if !scope.check_access().map_err(io::Error::other)? {
        return Err(io::Error::other("native popup protection must be installed on the UI thread"));
    }
    if STOPPING.with(Cell::get) {
        return Err(io::Error::other("native popup protection is shutting down"));
    }
    let (_, identity) = window_interfaces(window)?;
    let key = identity.as_raw() as usize;
    clear_unloaded_owner(key)?;
    let gate = match GATE.with(|slot| slot.borrow().clone()) {
        Some(gate) => gate,
        None => {
            let gate = Gate::new(scope)?;
            GATE.with(|slot| *slot.borrow_mut() = Some(gate.clone()));
            gate
        }
    };
    let result = gate.attach(window);
    if result.is_err() && OWNERS.with(|owners| owners.borrow().is_empty()) {
        GATE.with(|slot| { slot.borrow_mut().take(); });
    }
    result
}

fn drain_subscriptions() {
    let subscriptions = RETIRED_SUBSCRIPTIONS.with(|subscriptions| std::mem::take(&mut *subscriptions.borrow_mut()));
    for mut subscription in subscriptions {
        if let Err(error) = subscription.unsubscribe() {
            eprintln!("capture popup: could not detach its owner observer: {error}");
        }
    }
}

fn clear_owner(key: usize) -> io::Result<()> {
    let window = OWNERS.with(|owners| owners.borrow().get(&key).map(|owner| owner.interface.clone()));
    let Some(window) = window else { return Ok(()) };
    let gate = GATE.with(|slot| slot.borrow().clone())
        .ok_or_else(|| io::Error::other("the native popup gate is unavailable"))?;
    gate.clear(&window)?;
    let retirement = gate.retire_closed();
    if let Some(owner) = OWNERS.with(|owners| owners.borrow_mut().remove(&key)) {
        RETIRED_SUBSCRIPTIONS.with(|subscriptions| subscriptions.borrow_mut().push(owner.subscription));
        dispatch2::DispatchQueue::main().exec_async(drain_subscriptions);
    }
    if OWNERS.with(|owners| owners.borrow().is_empty()) {
        GATE.with(|slot| { slot.borrow_mut().take(); });
    }
    retirement
}

fn clear_unloaded_owner(key: usize) -> io::Result<()> {
    if OWNERS.with(|owners| owners.borrow().get(&key).is_some_and(|owner| owner.unloaded)) {
        clear_owner(key)?;
    }
    Ok(())
}

/// Discards a failed owner only after it is hidden and the host confirms
/// that no popup is still open. Reused windows install the gate before show.
pub(super) fn discard_failed_owner(window: &Window) -> io::Result<()> {
    if OWNERS.with(|owners| owners.borrow().is_empty()) {
        return Ok(());
    }
    let (_, identity) = window_interfaces(window)?;
    let key = identity.as_raw() as usize;
    if !OWNERS.with(|owners| owners.borrow().contains_key(&key)) {
        return Ok(());
    }
    if window.get_is_visible().map_err(io::Error::other)? {
        return Err(io::Error::other("a failed popup owner is still visible"));
    }
    clear_owner(key)
}

fn owner_unloaded(key: usize) -> io::Result<()> {
    OWNERS.with(|owners| {
        if let Some(owner) = owners.borrow_mut().get_mut(&key) {
            owner.unloaded = true;
        }
    });
    // The host closes this owner's native popups before raising Unloaded.
    // Clearing here also covers automatic application shutdown, whose
    // dispatcher may not run another posted callback.
    clear_unloaded_owner(key)
}

pub(in crate::ui) fn stopping() -> bool {
    STOPPING.with(Cell::get)
}

/// Closes protected owners and releases the auxiliary application while the
/// native platform and dispatcher are still alive.
pub(in crate::ui) fn prepare_shutdown(scope: &AppScope) -> io::Result<()> {
    if !scope.check_access().map_err(io::Error::other)? {
        return Err(io::Error::other("native popup protection must shut down on the UI thread"));
    }
    STOPPING.with(|stopping| stopping.set(true));
    let gate = GATE.with(|slot| slot.borrow().clone());
    if let Some(gate) = &gate {
        gate.stopping.store(true, Ordering::Release);
    }
    let owners = OWNERS.with(|owners| {
        owners.borrow().iter().map(|(&key, owner)| (key, owner.window.clone(), owner.unloaded)).collect::<Vec<_>>()
    });
    let outcome = (|| {
        for (key, window, unloaded) in owners {
            if !unloaded || window.get_is_visible().map_err(io::Error::other)? {
                window.close().map_err(io::Error::other)?;
            }
            let still_owned = OWNERS.with(|owners| owners.borrow().contains_key(&key));
            if still_owned {
                if window.get_is_visible().map_err(io::Error::other)? {
                    return Err(io::Error::other("a protected popup owner did not close; its gate remains installed"));
                }
                clear_unloaded_owner(key)?;
            }
            if OWNERS.with(|owners| owners.borrow().contains_key(&key)) {
                return Err(io::Error::other("a protected popup owner did not close; its gate remains installed"));
            }
        }
        if let Some(gate) = &gate {
            if !gate.pending.lock().map_err(|_| io::Error::other("the native popup gate is poisoned"))?.is_empty() {
                return Err(io::Error::other("native popups remained open during shutdown"));
            }
            gate.retire_closed()?;
        }
        GATE.with(|slot| { slot.borrow_mut().take(); });
        drain_subscriptions();
        Ok(())
    })();
    if outcome.is_err() {
        if let Some(gate) = &gate {
            gate.stopping.store(false, Ordering::Release);
        }
        STOPPING.with(|stopping| stopping.set(false));
    }
    outcome
}

pub(in crate::ui) fn cancel_shutdown() {
    STOPPING.with(|stopping| stopping.set(false));
}

pub(in crate::ui) fn after_run() {
    drain_subscriptions();
}

pub(in crate::ui) fn state_for_selftest() -> (usize, bool) {
    (OWNERS.with(|owners| owners.borrow().len()), GATE.with(|slot| slot.borrow().is_some()))
}

pub(in crate::ui) fn retry_unloaded_owner_for_selftest(scope: &AppScope) -> io::Result<()> {
    let owners = OWNERS.with(|owners| {
        owners.borrow().iter().map(|(&key, owner)| (key, owner.window.clone())).collect::<Vec<_>>()
    });
    let [(key, window)] = owners.as_slice() else {
        return Err(io::Error::other("the self-test requires exactly one protected owner"));
    };
    let first = owner_unloaded(*key);
    let retry = install(scope, window);
    let still_unloaded = OWNERS.with(|owners| owners.borrow().get(key).is_some_and(|owner| owner.unloaded));
    if first.is_ok() || retry.is_ok() || !still_unloaded {
        return Err(io::Error::other("a failed unload was not preserved on retry"));
    }
    Ok(())
}

pub(in crate::ui) fn reinstall_unloaded_owner_for_selftest(scope: &AppScope) -> io::Result<()> {
    let owners = OWNERS.with(|owners| {
        owners.borrow().values().map(|owner| (owner.window.clone(), owner.unloaded)).collect::<Vec<_>>()
    });
    let [(window, true)] = owners.as_slice() else {
        return Err(io::Error::other("the self-test requires one unloaded owner"));
    };
    install(scope, window)?;
    if OWNERS.with(|owners| owners.borrow().values().any(|owner| owner.unloaded)) {
        return Err(io::Error::other("the reused owner is still marked unloaded"));
    }
    Ok(())
}

pub(in crate::ui) fn cancel_shutdown_for_selftest(scope: &AppScope) -> io::Result<()> {
    let windows = OWNERS.with(|owners| owners.borrow().values().map(|owner| owner.window.clone()).collect::<Vec<_>>());
    let [window] = windows.as_slice() else {
        return Err(io::Error::other("the self-test requires exactly one protected owner"));
    };
    let attempted = Arc::new(AtomicBool::new(false));
    let observed = attempted.clone();
    let mut subscription = window.subscribe_closing(move |event| {
        observed.store(true, Ordering::Release);
        event.cancel = true;
    }).map_err(io::Error::other)?;
    let outcome = prepare_shutdown(scope);
    subscription.unsubscribe().map_err(io::Error::other)?;
    if outcome.is_ok() || !attempted.load(Ordering::Acquire) {
        return Err(io::Error::other("shutdown did not attempt and respect the protected owner close"));
    }
    Ok(())
}

pub(in crate::ui) struct GateDropProbe(Weak<Gate>);

impl GateDropProbe {
    pub(in crate::ui) fn dropped(&self) -> bool {
        self.0.upgrade().is_none()
    }
}

pub(in crate::ui) fn drop_probe_for_selftest() -> io::Result<GateDropProbe> {
    let gate = GATE.with(|slot| slot.borrow().as_ref().map(Arc::downgrade))
        .ok_or_else(|| io::Error::other("the native popup gate is unavailable"))?;
    TEST_GATE.with(|slot| *slot.borrow_mut() = Some(gate.clone()));
    Ok(GateDropProbe(gate))
}

pub(in crate::ui) fn test_gate_dropped() -> bool {
    TEST_GATE.with(|slot| slot.borrow().as_ref().is_some_and(|gate| gate.upgrade().is_none()))
}

pub(in crate::ui) fn pending_for_selftest() -> io::Result<Vec<(i64, u32)>> {
    let Some(gate) = GATE.with(|slot| slot.borrow().clone()) else { return Ok(Vec::new()) };
    let pending = gate.pending.lock().map_err(|_| io::Error::other("the native popup gate is poisoned"))?;
    Ok(pending.iter().map(|(&token, pending)| (token, pending.number)).collect())
}

pub(in crate::ui) fn acknowledge_for_selftest(token: i64) -> io::Result<i32> {
    let gate = GATE.with(|slot| slot.borrow().clone()).ok_or_else(|| io::Error::other("the native popup gate is unavailable"))?;
    // SAFETY: self-tests call this on the UI dispatcher while the gate is live.
    Ok(unsafe {
        let application = gate.application.as_raw();
        ((*(*application).vtable).acknowledge)(application, token)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_exposes_only_its_declared_interfaces_and_owns_references() {
        let handler = handler(Weak::new());
        // SAFETY: the test owns this live handler and observes only its count.
        assert_eq!(unsafe { (*handler.as_raw()).references.load(Ordering::Relaxed) }, 1);
        let unknown = handler.query_interface::<sys::IUnknown>().expect("IUnknown");
        let same = handler.query_interface::<Handler>().expect("popup handler");
        // SAFETY: all three references are live until they are dropped below.
        assert_eq!(unsafe { (*handler.as_raw()).references.load(Ordering::Relaxed) }, 3);
        drop((unknown, same));
        // SAFETY: the original reference remains live.
        assert_eq!(unsafe { (*handler.as_raw()).references.load(Ordering::Relaxed) }, 1);
        assert_eq!(handler.query_interface::<Application>().unwrap_err().0, sys::E_NOINTERFACE);
        // SAFETY: the host ABI accepts this live handler; the missing gate
        // must return a failure rather than unwind across the ABI.
        assert_eq!(unsafe { preparing(handler.as_raw(), 1, 1) }, sys::E_FAIL);
    }
}
