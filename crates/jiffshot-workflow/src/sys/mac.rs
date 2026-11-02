//! macOS: Win32's message calls emulated in-process.
//!
//! Every thread has a message queue (created on first use). A "window" is an
//! id with a class name, a window procedure and the queue of the thread that
//! created it; posting to it queues on that thread, and `DispatchMessageW`
//! calls its procedure. Timers are deadlines checked when the queue is read,
//! as Win32 generates `WM_TIMER` lazily. Global hotkeys go through Carbon's
//! `RegisterEventHotKey` (no Accessibility or Input Monitoring permission
//! needed); its handler, run by the main thread's run loop, posts
//! `WM_HOTKEY` to the registering window or thread. Keys are named with
//! Windows virtual-key codes everywhere and translated here.

use jiffshot_capture::macos::ffi::{
    CFRelease, CFTypeRef, CGEventCreateScrollWheelEvent2, CGEventPost, CGEventSourceButtonState, CGEventSourceKeyState,
    CGWarpMouseCursorPosition, kCGEventSourceStateCombinedSessionState, kCGHIDEventTap, kCGScrollEventUnitLine,
};
use jiffshot_capture::screen;
use objc2_core_foundation::CGPoint;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

pub type HWND = *mut c_void;
pub type WPARAM = usize;
pub type LPARAM = isize;
pub type LRESULT = isize;
pub type BOOL = i32;
pub type HOT_KEY_MODIFIERS = u32;
pub type WndProc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;
pub type TIMERPROC = Option<unsafe extern "system" fn(HWND, u32, usize, u32)>;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct POINT {
    pub x: i32,
    pub y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MSG {
    pub hwnd: HWND,
    pub message: u32,
    pub wParam: WPARAM,
    pub lParam: LPARAM,
    pub time: u32,
    pub pt: POINT,
}

// SAFETY: `hwnd` is an id, never dereferenced.
unsafe impl Send for MSG {}

impl MSG {
    fn new(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> MSG {
        MSG { hwnd, message, wParam: wparam, lParam: lparam, time: 0, pt: POINT::default() }
    }
}

pub const WM_DESTROY: u32 = 0x0002;
pub const WM_CLOSE: u32 = 0x0010;
pub const WM_QUIT: u32 = 0x0012;
pub const WM_TIMER: u32 = 0x0113;
pub const WM_HOTKEY: u32 = 0x0312;
pub const WM_APP: u32 = 0x8000;
pub const PM_REMOVE: u32 = 1;
pub const QS_ALLINPUT: u32 = 0x04FF;

pub const MOD_ALT: HOT_KEY_MODIFIERS = 0x0001;
pub const MOD_CONTROL: HOT_KEY_MODIFIERS = 0x0002;
pub const MOD_SHIFT: HOT_KEY_MODIFIERS = 0x0004;
/// Command on a Mac keyboard.
pub const MOD_WIN: HOT_KEY_MODIFIERS = 0x0008;
pub const MOD_NOREPEAT: HOT_KEY_MODIFIERS = 0x4000;

pub const VK_LBUTTON: u16 = 0x01;
pub const VK_RBUTTON: u16 = 0x02;
pub const VK_BACK: u16 = 0x08;
pub const VK_TAB: u16 = 0x09;
pub const VK_RETURN: u16 = 0x0D;
pub const VK_ESCAPE: u16 = 0x1B;
pub const VK_SPACE: u16 = 0x20;
pub const VK_LEFT: u16 = 0x25;
pub const VK_UP: u16 = 0x26;
pub const VK_RIGHT: u16 = 0x27;
pub const VK_DOWN: u16 = 0x28;
/// Print Screen; F13 on a Mac keyboard (the key in its place).
pub const VK_SNAPSHOT: u16 = 0x2C;
pub const VK_DELETE: u16 = 0x2E;
pub const VK_F1: u16 = 0x70;
pub const VK_F9: u16 = 0x78;
pub const VK_F10: u16 = 0x79;
pub const VK_F11: u16 = 0x7A;

/// The Mac key code (`kVK_*`) for a Windows virtual key.
pub fn mac_key_code(vk: u16) -> Option<u16> {
    const LETTERS: [u16; 26] = [
        0x00, 0x0B, 0x08, 0x02, 0x0E, 0x03, 0x05, 0x04, 0x22, 0x26, 0x28, 0x25, 0x2E, 0x2D, 0x1F, 0x23, 0x0C, 0x0F, 0x01,
        0x11, 0x20, 0x09, 0x0D, 0x07, 0x10, 0x06,
    ];
    const DIGITS: [u16; 10] = [0x1D, 0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1A, 0x1C, 0x19];
    const FKEYS: [u16; 20] = [
        0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F, 0x69, 0x6B, 0x71, 0x6A, 0x40, 0x4F, 0x50, 0x5A,
    ];
    Some(match vk {
        0x41..=0x5A => LETTERS[(vk - 0x41) as usize],
        0x30..=0x39 => DIGITS[(vk - 0x30) as usize],
        0x70..=0x83 => FKEYS[(vk - 0x70) as usize],
        VK_BACK => 0x33,
        VK_TAB => 0x30,
        VK_RETURN => 0x24,
        VK_ESCAPE => 0x35,
        VK_SPACE => 0x31,
        VK_LEFT => 0x7B,
        VK_RIGHT => 0x7C,
        VK_DOWN => 0x7D,
        VK_UP => 0x7E,
        VK_DELETE => 0x75,
        VK_SNAPSHOT => 0x69,
        _ => return None,
    })
}

struct Timer {
    hwnd: usize,
    id: usize,
    every: Duration,
    due: Instant,
}

#[derive(Default)]
struct Pending {
    messages: VecDeque<MSG>,
    quit: Option<i32>,
    timers: Vec<Timer>,
}

#[derive(Default)]
struct Queue {
    pending: Mutex<Pending>,
    wake: Condvar,
}

impl Queue {
    fn lock(&self) -> MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn post(&self, msg: MSG) {
        self.lock().messages.push_back(msg);
        self.wake.notify_all();
    }
}

thread_local! {
    static QUEUE: Arc<Queue> = Arc::default();
}

fn this_queue() -> Arc<Queue> {
    QUEUE.with(Arc::clone)
}

struct Window {
    id: usize,
    class: String,
    proc: WndProc,
    queue: Arc<Queue>,
}

static WINDOWS: Mutex<Vec<Window>> = Mutex::new(Vec::new());
static NEXT_WINDOW: AtomicUsize = AtomicUsize::new(1);

fn windows() -> MutexGuard<'static, Vec<Window>> {
    WINDOWS.lock().unwrap_or_else(|e| e.into_inner())
}

fn window_queue(hwnd: HWND) -> Option<Arc<Queue>> {
    windows().iter().find(|w| w.id == hwnd as usize).map(|w| w.queue.clone())
}

/// The hidden window of `class` in this process, or null.
pub fn find_window(class: &str) -> HWND {
    windows().iter().find(|w| w.class == class).map_or(std::ptr::null_mut(), |w| w.id as HWND)
}

/// Creates a hidden window of `class` on this thread whose messages go to
/// `proc`.
pub fn create_message_window(class: &str, _title: &str, proc: WndProc) -> HWND {
    let id = NEXT_WINDOW.fetch_add(1, Ordering::Relaxed);
    windows().push(Window { id, class: class.to_owned(), proc, queue: this_queue() });
    id as HWND
}

pub unsafe fn PostMessageW(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> BOOL {
    let queue = if hwnd.is_null() { Some(this_queue()) } else { window_queue(hwnd) };
    match queue {
        Some(q) => {
            q.post(MSG::new(hwnd, msg, wparam, lparam));
            1
        }
        None => 0,
    }
}

pub unsafe fn PostQuitMessage(code: i32) {
    let q = this_queue();
    q.lock().quit = Some(code);
    q.wake.notify_all();
}

pub unsafe fn SetTimer(hwnd: HWND, id: usize, ms: u32, _proc: TIMERPROC) -> usize {
    let Some(q) = window_queue(hwnd) else { return 0 };
    let every = Duration::from_millis(ms.max(10) as u64);
    let mut p = q.lock();
    p.timers.retain(|t| !(t.hwnd == hwnd as usize && t.id == id));
    p.timers.push(Timer { hwnd: hwnd as usize, id, every, due: Instant::now() + every });
    drop(p);
    q.wake.notify_all();
    id
}

pub unsafe fn KillTimer(hwnd: HWND, id: usize) -> BOOL {
    let Some(q) = window_queue(hwnd) else { return 0 };
    let mut p = q.lock();
    let before = p.timers.len();
    p.timers.retain(|t| !(t.hwnd == hwnd as usize && t.id == id));
    (p.timers.len() != before) as BOOL
}

/// The next message for this thread, without waiting: posted messages
/// first, then quit, then a due timer.
fn take(p: &mut Pending) -> Option<MSG> {
    if let Some(m) = p.messages.pop_front() {
        return Some(m);
    }
    if let Some(code) = p.quit.take() {
        return Some(MSG::new(std::ptr::null_mut(), WM_QUIT, code as usize, 0));
    }
    let now = Instant::now();
    let t = p.timers.iter_mut().filter(|t| t.due <= now).min_by_key(|t| t.due)?;
    t.due = now + t.every;
    Some(MSG::new(t.hwnd as HWND, WM_TIMER, t.id, 0))
}

fn next_due(p: &Pending) -> Option<Instant> {
    p.timers.iter().map(|t| t.due).min()
}

pub unsafe fn GetMessageW(msg: *mut MSG, _hwnd: HWND, _min: u32, _max: u32) -> BOOL {
    let q = this_queue();
    let mut p = q.lock();
    loop {
        if let Some(m) = take(&mut p) {
            *msg = m;
            return (m.message != WM_QUIT) as BOOL;
        }
        p = match next_due(&p) {
            Some(due) => q.wake.wait_timeout(p, due.saturating_duration_since(Instant::now())).unwrap_or_else(|e| e.into_inner()).0,
            None => q.wake.wait(p).unwrap_or_else(|e| e.into_inner()),
        };
    }
}

pub unsafe fn PeekMessageW(msg: *mut MSG, _hwnd: HWND, _min: u32, _max: u32, _remove: u32) -> BOOL {
    let q = this_queue();
    let mut p = q.lock();
    match take(&mut p) {
        Some(m) => {
            *msg = m;
            1
        }
        None => 0,
    }
}

/// Waits up to `ms` for a message or a due timer on this thread.
pub unsafe fn MsgWaitForMultipleObjects(_count: u32, _handles: *const c_void, _all: BOOL, ms: u32, _mask: u32) -> u32 {
    let q = this_queue();
    let p = q.lock();
    let ready = |p: &Pending| !p.messages.is_empty() || p.quit.is_some() || next_due(p).is_some_and(|d| d <= Instant::now());
    if ready(&p) {
        return 0;
    }
    let mut until = Instant::now() + Duration::from_millis(ms as u64);
    if let Some(due) = next_due(&p) {
        until = until.min(due);
    }
    let _ = q.wake.wait_timeout_while(p, until.saturating_duration_since(Instant::now()), |p| !ready(p));
    0
}

pub unsafe fn TranslateMessage(_msg: *const MSG) -> BOOL {
    0
}

pub unsafe fn DispatchMessageW(msg: *const MSG) -> LRESULT {
    let m = *msg;
    if m.hwnd.is_null() {
        return 0;
    }
    let proc = windows().iter().find(|w| w.id == m.hwnd as usize).map(|w| w.proc);
    match proc {
        Some(proc) => proc(m.hwnd, m.message, m.wParam, m.lParam),
        None => 0,
    }
}

/// `WM_CLOSE` destroys the window (its procedure sees `WM_DESTROY`); the
/// rest is ignored.
pub unsafe fn DefWindowProcW(hwnd: HWND, msg: u32, _wparam: WPARAM, _lparam: LPARAM) -> LRESULT {
    if msg == WM_CLOSE {
        let proc = windows().iter().find(|w| w.id == hwnd as usize).map(|w| w.proc);
        if let Some(proc) = proc {
            proc(hwnd, WM_DESTROY, 0, 0);
            let queue = window_queue(hwnd);
            windows().retain(|w| w.id != hwnd as usize);
            if let Some(q) = queue {
                q.lock().timers.retain(|t| t.hwnd != hwnd as usize);
            }
            let mut keys = hotkeys();
            let gone: Vec<_> = keys.iter().filter(|k| k.hwnd == hwnd as usize).map(|k| k.carbon).collect();
            keys.retain(|k| k.hwnd != hwnd as usize);
            drop(keys);
            gone.into_iter().for_each(unregister_carbon);
        }
    }
    0
}

pub unsafe fn GetCursorPos(point: *mut POINT) -> BOOL {
    let (x, y) = screen::cursor_pos();
    *point = POINT { x, y };
    1
}

pub unsafe fn SetCursorPos(x: i32, y: i32) -> BOOL {
    let (px, py) = screen::to_points(x as f64, y as f64);
    (CGWarpMouseCursorPosition(CGPoint { x: px, y: py }) == 0) as BOOL
}

/// Bit 15 set while `vk` (a key or mouse button) is down.
pub unsafe fn GetAsyncKeyState(vk: i32) -> i16 {
    let state = kCGEventSourceStateCombinedSessionState;
    let down = match vk as u16 {
        VK_LBUTTON => CGEventSourceButtonState(state, 0),
        VK_RBUTTON => CGEventSourceButtonState(state, 1),
        vk => mac_key_code(vk).is_some_and(|code| CGEventSourceKeyState(state, code)),
    };
    if down {
        i16::MIN
    } else {
        0
    }
}

fn post_scroll(vertical: i32, horizontal: i32) {
    // SAFETY: creates, posts and releases one scroll event.
    unsafe {
        let event = CGEventCreateScrollWheelEvent2(std::ptr::null_mut(), kCGScrollEventUnitLine, 2, vertical, horizontal, 0);
        if !event.is_null() {
            CGEventPost(kCGHIDEventTap, event);
            CFRelease(event as CFTypeRef);
        }
    }
}

/// Turns the mouse wheel `notches` (positive: up), three lines a notch as
/// on Windows.
pub fn wheel(notches: i32) {
    post_scroll(notches * 3, 0);
}

/// Tilts the wheel `notches` (positive: right).
pub fn hwheel(notches: i32) {
    post_scroll(0, -notches * 3);
}

/// Sleeps are already fine-grained on macOS.
pub struct FineTimer;

impl FineTimer {
    pub fn start() -> FineTimer {
        FineTimer
    }
}

// --- One instance per user -------------------------------------------------

/// Claims the one running Jiffshot for this user (an exclusive lock on a
/// file, released by the system when the process ends).
pub fn claim_instance(name: &str) -> std::io::Result<bool> {
    use std::os::fd::IntoRawFd;
    match jiffshot_capture::ipc::try_lock(name)? {
        Some(file) => {
            // The descriptor intentionally remains open for the process.
            let _ = file.into_raw_fd();
            Ok(true)
        }
        None => Ok(false),
    }
}

/// The socket other processes reach the instance through.
fn socket_path(class: &str) -> std::io::Result<std::path::PathBuf> {
    Ok(jiffshot_capture::ipc::private_dir()?.join(format!("{class}.sock")))
}

/// Lets other processes post to the window of `class` through
/// [`post_to_instance`]: a private socket served on a thread of its own.
/// Only for the process holding [`claim_instance`].
pub fn serve_instance(class: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = socket_path(class)?;
    jiffshot_capture::ipc::remove_stale_socket(&path)?;
    let listener = std::os::unix::net::UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let class = class.to_owned();
    std::thread::Builder::new().name("jiffshot-instance".into()).spawn(move || {
        for incoming in listener.incoming() {
            match incoming {
                Ok(stream) => {
                    if let Err(error) = receive_instance(stream, &class) {
                        eprintln!("Jiffshot instance socket: {error}");
                    }
                }
                Err(error) => eprintln!("Jiffshot instance socket: {error}"),
            }
        }
    })?;
    Ok(())
}

fn receive_instance(mut stream: std::os::unix::net::UnixStream, class: &str) -> std::io::Result<()> {
    use std::io::{Read, Write};
    jiffshot_capture::ipc::verify_peer(&stream)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut buf = [0u8; 24];
    stream.read_exact(&mut buf)?;
    let word = |i: usize| u64::from_le_bytes(buf[i * 8..i * 8 + 8].try_into().expect("fixed-length message"));
    let (msg, wparam, lparam) = (word(0), word(1), word(2));
    let accepted = valid_instance_message(msg, wparam, lparam)
        && super::post_to_class(class, msg as u32, wparam as WPARAM, lparam as LPARAM);
    stream.write_all(&[u8::from(accepted)])?;
    if accepted {
        Ok(())
    } else {
        Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "instance request was rejected"))
    }
}

fn valid_instance_message(msg: u64, wparam: u64, lparam: u64) -> bool {
    if lparam != 0 {
        return false;
    }
    match msg {
        msg if msg == crate::tray::WM_APP_REQUEST_EXIT as u64 || msg == crate::tray::WM_APP_MENU as u64 => wparam == 0,
        msg if msg == crate::tray::WM_APP_CAPTURE as u64 => {
            wparam <= crate::tray::MODE_VIDEO_FULL as u64 || wparam == crate::tray::OPEN_CAPTURE_WINDOW as u64
        }
        _ => false,
    }
}

/// Waits for the claimed instance to begin listening, then sends one
/// request and waits for acknowledgement.
pub fn post_to_instance_when_ready(class: &str, msg: u32, wparam: WPARAM, lparam: LPARAM, wait: Duration) -> std::io::Result<()> {
    send_instance_at(&socket_path(class)?, msg, wparam, lparam, wait)
}

fn send_instance_at(path: &std::path::Path, msg: u32, wparam: WPARAM, lparam: LPARAM, wait: Duration) -> std::io::Result<()> {
    use std::io::{Read, Write};
    if !valid_instance_message(msg as u64, wparam as u64, lparam as u64) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid instance request"));
    }
    let mut stream = jiffshot_capture::ipc::connect_when_ready(path, wait)?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut buf = [0u8; 24];
    buf[..8].copy_from_slice(&(msg as u64).to_le_bytes());
    buf[8..16].copy_from_slice(&(wparam as u64).to_le_bytes());
    buf[16..].copy_from_slice(&(lparam as u64).to_le_bytes());
    stream.write_all(&buf).map_err(|error| {
        std::io::Error::new(error.kind(), format!("the request may have been delivered, but delivery could not be confirmed: {error}"))
    })?;
    let mut ack = [0];
    stream.read_exact(&mut ack).map_err(|error| {
        std::io::Error::new(error.kind(), format!("the request may have been delivered, but the running instance did not acknowledge it: {error}"))
    })?;
    if ack == [1] {
        Ok(())
    } else {
        Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "the running instance rejected the request"))
    }
}

// --- Global hotkeys (Carbon) -------------------------------------------------

#[repr(C)]
struct EventTypeSpec {
    class: u32,
    kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

type EventHandler = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn GetApplicationEventTarget() -> *mut c_void;
    fn InstallEventHandler(
        target: *mut c_void,
        handler: EventHandler,
        count: u32,
        types: *const EventTypeSpec,
        user: *mut c_void,
        out: *mut *mut c_void,
    ) -> i32;
    fn RegisterEventHotKey(code: u32, modifiers: u32, id: EventHotKeyID, target: *mut c_void, options: u32, out: *mut *mut c_void) -> i32;
    fn UnregisterEventHotKey(hotkey: *mut c_void) -> i32;
    fn CreateEvent(
        allocator: *const c_void,
        class: u32,
        kind: u32,
        time: f64,
        attributes: u32,
        event: *mut *mut c_void,
    ) -> i32;
    fn SetEventParameter(event: *mut c_void, name: u32, kind: u32, size: u32, data: *const c_void) -> i32;
    fn SendEventToEventTarget(event: *mut c_void, target: *mut c_void) -> i32;
    fn ReleaseEvent(event: *mut c_void);
    fn GetEventParameter(
        event: *mut c_void,
        name: u32,
        kind: u32,
        actual_kind: *mut u32,
        size: u32,
        actual_size: *mut u32,
        data: *mut c_void,
    ) -> i32;
}

const fn four(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

const SIGNATURE: u32 = four(b"JFSH");

struct HotKey {
    hwnd: usize,
    id: i32,
    queue: Arc<Queue>,
    modifiers: u32,
    vk: u16,
    carbon: u32,
}

static HOTKEYS: Mutex<Vec<HotKey>> = Mutex::new(Vec::new());
static NEXT_CARBON: AtomicU32 = AtomicU32::new(1);
/// Carbon id → `EventHotKeyRef`, touched only on the main thread.
static REFS: Mutex<Vec<(u32, usize)>> = Mutex::new(Vec::new());

fn hotkeys() -> MutexGuard<'static, Vec<HotKey>> {
    HOTKEYS.lock().unwrap_or_else(|e| e.into_inner())
}

unsafe extern "C" fn on_hotkey(_next: *mut c_void, event: *mut c_void, _user: *mut c_void) -> i32 {
    let mut id = EventHotKeyID { signature: 0, id: 0 };
    let status = GetEventParameter(
        event,
        four(b"----"),
        four(b"hkid"),
        std::ptr::null_mut(),
        std::mem::size_of::<EventHotKeyID>() as u32,
        std::ptr::null_mut(),
        &mut id as *mut EventHotKeyID as *mut c_void,
    );
    if status != 0 || id.signature != SIGNATURE {
        return -9874; // eventNotHandledErr
    }
    let target = hotkeys().iter().find(|k| k.carbon == id.id).map(|k| (k.queue.clone(), k.hwnd, k.id, k.modifiers, k.vk));
    if let Some((queue, hwnd, hotkey, modifiers, vk)) = target {
        let lparam = ((vk as isize) << 16) | (modifiers & 0xF) as isize;
        queue.post(MSG::new(hwnd as HWND, WM_HOTKEY, hotkey as usize, lparam));
    }
    0
}

fn is_main_thread() -> bool {
    // SAFETY: a plain query.
    unsafe { libc::pthread_main_np() != 0 }
}

fn carbon_modifiers(modifiers: u32) -> u32 {
    const CMD: u32 = 0x0100;
    const SHIFT: u32 = 0x0200;
    const OPTION: u32 = 0x0800;
    const CONTROL: u32 = 0x1000;
    let mut m = 0;
    if modifiers & MOD_WIN != 0 {
        m |= CMD;
    }
    if modifiers & MOD_SHIFT != 0 {
        m |= SHIFT;
    }
    if modifiers & MOD_ALT != 0 {
        m |= OPTION;
    }
    if modifiers & MOD_CONTROL != 0 {
        m |= CONTROL;
    }
    m
}

fn install_handler() -> bool {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(|| {
        let spec = EventTypeSpec { class: four(b"keyb"), kind: 5 }; // kEventHotKeyPressed
        // SAFETY: installs a static handler on the application target.
        let status = unsafe { InstallEventHandler(GetApplicationEventTarget(), on_hotkey, 1, &spec, std::ptr::null_mut(), std::ptr::null_mut()) };
        if status != 0 {
            eprintln!("could not install the macOS hotkey handler: {status}");
        }
        status == 0
    })
}

fn register_carbon_now(carbon: u32, code: u16, modifiers: u32) -> bool {
    if !install_handler() {
        return false;
    }
    let mut reference = std::ptr::null_mut();
    // SAFETY: registers one hotkey for this application.
    let status = unsafe {
        RegisterEventHotKey(code as u32, carbon_modifiers(modifiers), EventHotKeyID { signature: SIGNATURE, id: carbon }, GetApplicationEventTarget(), 0, &mut reference)
    };
    if status == 0 {
        REFS.lock().unwrap_or_else(|e| e.into_inner()).push((carbon, reference as usize));
    }
    status == 0
}

/// Registers on the main thread. A timed-out request is removed if it
/// eventually runs, rather than reporting a key that was never registered.
fn register_carbon(carbon: u32, code: u16, modifiers: u32) -> bool {
    if is_main_thread() {
        return register_carbon_now(carbon, code, modifiers);
    }
    enum State {
        Pending,
        Finished(bool),
        TimedOut,
    }
    let state = Arc::new((Mutex::new(State::Pending), Condvar::new()));
    let main_state = Arc::clone(&state);
    dispatch2::DispatchQueue::main().exec_async(move || {
        let success = register_carbon_now(carbon, code, modifiers);
        let (lock, wake) = &*main_state;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        match *state {
            State::Pending => {
                *state = State::Finished(success);
                wake.notify_one();
            }
            State::TimedOut if success => {
                drop(state);
                unregister_carbon(carbon);
            }
            _ => {}
        }
    });
    let (lock, wake) = &*state;
    let state = lock.lock().unwrap_or_else(|e| e.into_inner());
    let (mut state, _) = wake.wait_timeout_while(state, Duration::from_millis(500), |state| matches!(state, State::Pending))
        .unwrap_or_else(|e| e.into_inner());
    match *state {
        State::Finished(success) => success,
        _ => {
            *state = State::TimedOut;
            false
        }
    }
}

fn unregister_carbon(carbon: u32) {
    let run = move || {
        let mut refs = REFS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = refs.iter().position(|(c, _)| *c == carbon) {
            let (_, reference) = refs.remove(i);
            // SAFETY: a reference RegisterEventHotKey returned.
            unsafe { UnregisterEventHotKey(reference as *mut c_void) };
        }
    };
    if is_main_thread() {
        run();
    } else {
        dispatch2::DispatchQueue::main().exec_async(run);
    }
}

/// Registers a global hotkey that posts `WM_HOTKEY` (wparam `id`) to
/// `hwnd`, or to this thread when null. Fails when this process already
/// has the key, or the system refuses it.
pub unsafe fn RegisterHotKey(hwnd: HWND, id: i32, modifiers: HOT_KEY_MODIFIERS, vk: u32) -> BOOL {
    let modifiers = modifiers & 0xF;
    let vk = vk as u16;
    let Some(code) = mac_key_code(vk) else { return 0 };
    let queue = if hwnd.is_null() { this_queue() } else {
        match window_queue(hwnd) {
            Some(q) => q,
            None => return 0,
        }
    };
    let carbon = NEXT_CARBON.fetch_add(1, Ordering::Relaxed);
    {
        let mut keys = hotkeys();
        if keys.iter().any(|k| k.modifiers == modifiers && k.vk == vk) {
            return 0;
        }
        if keys.iter().any(|k| k.hwnd == hwnd as usize && k.id == id && Arc::ptr_eq(&k.queue, &queue)) {
            return 0;
        }
        keys.push(HotKey { hwnd: hwnd as usize, id, queue, modifiers, vk, carbon });
    }
    if register_carbon(carbon, code, modifiers) {
        1
    } else {
        hotkeys().retain(|k| k.carbon != carbon);
        0
    }
}

pub unsafe fn UnregisterHotKey(hwnd: HWND, id: i32) -> BOOL {
    let queue = this_queue();
    let mut keys = hotkeys();
    let Some(i) = keys.iter().position(|k| k.hwnd == hwnd as usize && k.id == id && (!hwnd.is_null() || Arc::ptr_eq(&k.queue, &queue))) else {
        return 0;
    };
    let carbon = keys.remove(i).carbon;
    drop(keys);
    unregister_carbon(carbon);
    1
}

/// Exercises the Carbon event handler without synthesizing a hardware keypress.
#[doc(hidden)]
pub fn deliver_selftest_hotkey(id: i32) -> std::io::Result<()> {
    let queue = this_queue();
    let carbon = hotkeys()
        .iter()
        .find(|key| key.hwnd == 0 && key.id == id && Arc::ptr_eq(&key.queue, &queue))
        .map(|key| key.carbon)
        .ok_or_else(|| std::io::Error::other("the test hotkey is not registered"))?;
    let (tx, rx) = std::sync::mpsc::channel();
    dispatch2::DispatchQueue::main().exec_sync(move || {
        let result = (|| {
            let id = EventHotKeyID { signature: SIGNATURE, id: carbon };
            let mut event = std::ptr::null_mut();
            // SAFETY: the event is created and delivered on the main thread,
            // and its parameter remains valid for the duration of delivery.
            unsafe {
                let status = CreateEvent(std::ptr::null(), four(b"keyb"), 5, 0.0, 0, &mut event);
                if status != 0 || event.is_null() {
                    return Err(std::io::Error::other(format!("could not create the Carbon test event: {status}")));
                }
                let status = SetEventParameter(event, four(b"----"), four(b"hkid"), std::mem::size_of::<EventHotKeyID>() as u32, (&id as *const EventHotKeyID).cast());
                let status = if status == 0 { SendEventToEventTarget(event, GetApplicationEventTarget()) } else { status };
                ReleaseEvent(event);
                if status == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::other(format!("could not deliver the Carbon test event: {status}")))
                }
            }
        })();
        if tx.send(result).is_err() {
            eprintln!("the hotkey self-test stopped before event delivery completed");
        }
    });
    rx.recv().map_err(|_| std::io::Error::other("the Carbon test event did not finish"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_socket() -> (std::path::PathBuf, std::path::PathBuf) {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::path::PathBuf::from(format!("/tmp/jiffshot-instance-test-{}-{id}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        (dir.join("instance.sock"), dir)
    }

    unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if msg == WM_APP + 1 {
            PostMessageW(hwnd, WM_APP + 2, wparam + 1, lparam);
        }
        if msg == WM_DESTROY {
            PostQuitMessage(7);
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    #[test]
    fn queues_timers_and_quit() {
        let hwnd = create_message_window("JiffshotTestQueue", "", proc);
        assert_eq!(find_window("JiffshotTestQueue"), hwnd);
        unsafe {
            assert_eq!(PostMessageW(hwnd, WM_APP + 1, 41, 0), 1);
            let mut msg: MSG = std::mem::zeroed();
            assert_eq!(GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0), 1);
            DispatchMessageW(&msg);
            assert_eq!(GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0), 1);
            assert_eq!((msg.message, msg.wParam), (WM_APP + 2, 42));
            SetTimer(hwnd, 9, 20, None);
            let started = Instant::now();
            assert_eq!(GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0), 1);
            assert_eq!((msg.message, msg.wParam), (WM_TIMER, 9));
            assert!(started.elapsed() >= Duration::from_millis(15));
            assert_eq!(KillTimer(hwnd, 9), 1);
            assert_eq!(PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE), 0);
            // From another thread, as the app's tray icon posts.
            std::thread::spawn(move || {
                let h = find_window("JiffshotTestQueue");
                PostMessageW(h, WM_CLOSE, 0, 0);
            })
            .join()
            .unwrap();
            assert_eq!(GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0), 1);
            DispatchMessageW(&msg);
            DefWindowProcW(msg.hwnd, msg.message, 0, 0);
            assert!(find_window("JiffshotTestQueue").is_null());
            assert_eq!(GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0), 0);
            assert_eq!(msg.wParam, 7);
        }
    }

    #[test]
    fn key_codes() {
        assert_eq!(mac_key_code(b'A' as u16), Some(0x00));
        assert_eq!(mac_key_code(b'2' as u16), Some(0x13));
        assert_eq!(mac_key_code(VK_F9), Some(0x65));
        assert_eq!(mac_key_code(VK_ESCAPE), Some(0x35));
        assert_eq!(mac_key_code(0xFF), None);
    }

    #[test]
    fn instance_messages_cannot_invoke_internal_callbacks() {
        assert!(valid_instance_message(crate::tray::WM_APP_CAPTURE as u64, crate::tray::MODE_IMAGE as u64, 0));
        assert!(valid_instance_message(crate::tray::WM_APP_REQUEST_EXIT as u64, 0, 0));
        assert!(!valid_instance_message(WM_CLOSE as u64, 0, 0));
        assert!(!valid_instance_message(crate::tray::WM_APP_RUN as u64, 0, 123));
        assert!(!valid_instance_message(crate::tray::WM_APP_CAPTURE as u64, u64::MAX, 0));
        assert!(!valid_instance_message(WM_CLOSE as u64, 1, 0));
    }

    #[test]
    fn instance_listener_acknowledges_only_queued_requests() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;
        let class = "JiffshotTestInstance";
        let hwnd = create_message_window(class, "", proc);
        let (mut client, server) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || receive_instance(server, class));
        let mut bytes = [0u8; 24];
        bytes[..8].copy_from_slice(&(crate::tray::WM_APP_CAPTURE as u64).to_le_bytes());
        bytes[8..16].copy_from_slice(&(crate::tray::MODE_IMAGE as u64).to_le_bytes());
        client.write_all(&bytes).unwrap();
        let mut ack = [0];
        client.read_exact(&mut ack).unwrap();
        assert_eq!(ack, [1]);
        worker.join().unwrap().unwrap();
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) }, 1);
        assert_eq!((msg.hwnd, msg.message, msg.wParam), (hwnd, crate::tray::WM_APP_CAPTURE, crate::tray::MODE_IMAGE));

        let (mut client, server) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || receive_instance(server, class));
        bytes[..8].copy_from_slice(&(crate::tray::WM_APP_RUN as u64).to_le_bytes());
        client.write_all(&bytes).unwrap();
        client.read_exact(&mut ack).unwrap();
        assert_eq!(ack, [0]);
        assert_eq!(worker.join().unwrap().unwrap_err().kind(), std::io::ErrorKind::InvalidData);
        unsafe { DefWindowProcW(hwnd, WM_CLOSE, 0, 0) };

        let (mut client, server) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || receive_instance(server, class));
        bytes[..8].copy_from_slice(&(crate::tray::WM_APP_CAPTURE as u64).to_le_bytes());
        client.write_all(&bytes).unwrap();
        client.read_exact(&mut ack).unwrap();
        assert_eq!(ack, [0]);
        assert_eq!(worker.join().unwrap().unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn instance_request_waits_for_a_delayed_listener() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;
        let (socket, dir) = test_socket();
        let server_path = socket.clone();
        let server = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            let listener = UnixListener::bind(server_path).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let mut message = [0; 24];
            stream.read_exact(&mut message).unwrap();
            stream.write_all(&[1]).unwrap();
            message
        });
        send_instance_at(&socket, crate::tray::WM_APP_REQUEST_EXIT, 0, 0, Duration::from_secs(2)).unwrap();
        let message = server.join().unwrap();
        assert_eq!(u64::from_le_bytes(message[..8].try_into().unwrap()), crate::tray::WM_APP_REQUEST_EXIT as u64);
        std::fs::remove_file(socket).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn instance_request_with_missing_acknowledgement_is_not_replayed() {
        use std::io::Read;
        use std::os::unix::net::UnixListener;
        let (socket, dir) = test_socket();
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut message = [0; 24];
            stream.read_exact(&mut message).unwrap();
            drop(stream);
            listener.set_nonblocking(true).unwrap();
            (listener, message)
        });
        let error = send_instance_at(&socket, crate::tray::WM_APP_REQUEST_EXIT, 0, 0, Duration::from_secs(2)).unwrap_err();
        assert!(error.to_string().contains("may have been delivered"));
        let (listener, message) = server.join().unwrap();
        assert_eq!(u64::from_le_bytes(message[..8].try_into().unwrap()), crate::tray::WM_APP_REQUEST_EXIT as u64);
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        drop(listener);
        std::fs::remove_file(socket).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
