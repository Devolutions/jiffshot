//! Hide ordinary application windows, not the capture UI created afterward.

use rustolonia::{AppScope, Window, WindowState};
use std::cell::RefCell;
use std::io;
use std::thread::ThreadId;
use std::time::{Duration, Instant};

#[derive(Default)]
struct State {
    depth: usize,
    windows: Vec<(Window, bool)>,
    ready_at: Option<Instant>,
}

// Linux window managers may animate hides; native Windows/macOS hides below
// suppress transitions, but still need a compositor turn before capture.
const SETTLE: Duration = if cfg!(target_os = "linux") {
    Duration::from_millis(500)
} else {
    Duration::from_millis(150)
};

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

#[cfg(target_os = "macos")]
fn hide_without_animation(scope: &AppScope, window: &Window) -> io::Result<()> {
    use objc2_app_kit::NSWindowAnimationBehavior;

    let handle = scope.platform_handle(window).map_err(io::Error::other)?;
    let native = crate::mac_window::window(handle)
        .ok_or_else(|| io::Error::other("the window to hide is no longer available"))?;
    let behavior = native.animationBehavior();
    native.setAnimationBehavior(NSWindowAnimationBehavior::None);
    let result = window.hide().map_err(io::Error::other);
    native.setAnimationBehavior(behavior);
    result
}

#[cfg(windows)]
fn hide_without_animation(scope: &AppScope, window: &Window) -> io::Result<()> {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_TRANSITIONS_FORCEDISABLED,
    };

    let hwnd = scope.platform_handle(window).map_err(io::Error::other)? as HWND;
    let disabled = 1i32;
    // SAFETY: the live window belongs to this UI thread; DWM reads a BOOL.
    unsafe {
        let status = DwmSetWindowAttribute(
            hwnd,
            DWMWA_TRANSITIONS_FORCEDISABLED as u32,
            (&disabled as *const i32).cast(),
            std::mem::size_of_val(&disabled) as u32,
        );
        if status < 0 {
            return Err(io::Error::other(format!(
                "Could not disable window transitions: HRESULT {status:#010x}"
            )));
        }
    }
    let result = window.hide().map_err(io::Error::other);
    // This write-only attribute is owned by this hook. Return to normal DWM policy.
    let enabled = 0i32;
    // SAFETY: the same live window remains valid after hiding; DWM reads a BOOL.
    let status = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_TRANSITIONS_FORCEDISABLED as u32,
            (&enabled as *const i32).cast(),
            std::mem::size_of_val(&enabled) as u32,
        )
    };
    if status < 0 {
        eprintln!("Could not restore window transitions after capture: HRESULT {status:#010x}");
    }
    result
}

#[cfg(target_os = "linux")]
fn hide_without_animation(_scope: &AppScope, window: &Window) -> io::Result<()> {
    window.hide().map_err(io::Error::other)
}

/// Also tracks windows dismissed by their Capture button before the hook runs.
pub(crate) fn hide_window(window: &Window) -> io::Result<()> {
    let scope =
        super::scope().ok_or_else(|| io::Error::other("the application scope is unavailable"))?;
    hide_without_animation(&scope, window)?;
    STATE.with(|slot| slot.borrow_mut().ready_at = Some(Instant::now() + SETTLE));
    Ok(())
}

fn restore(state: &mut State) {
    for (window, activated) in state.windows.drain(..) {
        let result = window.set_show_activated(false).and_then(|_| window.show());
        if let Err(error) = result {
            eprintln!("Could not restore a Jiffshot window after capture: {error}");
        }
        if let Err(error) = window.set_show_activated(activated) {
            eprintln!("Could not restore a Jiffshot window's activation setting: {error}");
        }
    }
}

fn change(scope: &AppScope, hide: bool) -> io::Result<(Duration, Option<Instant>)> {
    STATE.with(|slot| {
        let mut state = slot.borrow_mut();
        if !hide {
            state.depth = state
                .depth
                .checked_sub(1)
                .ok_or_else(|| io::Error::other("unbalanced screenshot visibility"))?;
            if state.depth == 0 {
                restore(&mut state);
                state.ready_at = None;
            }
            return Ok((Duration::ZERO, None));
        }
        if state.depth == 0 {
            let result = (|| {
                for window in scope.windows() {
                    if window.is_visible().map_err(io::Error::other)?
                        && window.window_state().map_err(io::Error::other)?
                            != WindowState::Minimized
                    {
                        let activated = window.show_activated().map_err(io::Error::other)?;
                        hide_without_animation(scope, &window)?;
                        state.windows.push((window, activated));
                    }
                }
                Ok::<_, io::Error>(())
            })();
            if let Err(error) = result {
                restore(&mut state);
                return Err(error);
            }
            if !state.windows.is_empty() {
                state.ready_at = Some(Instant::now() + SETTLE);
            }
        }
        state.depth += 1;
        let settle = state
            .ready_at
            .map(|ready| ready.saturating_duration_since(Instant::now()))
            .unwrap_or_default();
        Ok((settle, state.ready_at))
    })
}

fn dispatch(
    scope: &AppScope,
    ui_thread: ThreadId,
    hide: bool,
) -> io::Result<(Duration, Option<Instant>)> {
    if std::thread::current().id() == ui_thread {
        return change(scope, hide);
    }
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    let target = scope.clone();
    scope
        .post(move || {
            let result = change(&target, hide);
            if let Err(error) = send.send(result) {
                eprintln!("The screenshot visibility caller stopped waiting");
                if hide && error.0.is_ok() {
                    if let Err(error) = change(&target, false) {
                        eprintln!("Could not undo screenshot window hiding: {error}");
                    }
                }
            }
        })
        .map_err(io::Error::other)?;
    receive.recv().map_err(io::Error::other)?
}

pub(crate) fn install(scope: &AppScope) -> rustolonia::Result<()> {
    let scope = scope.clone();
    let ui_thread = std::thread::current().id();
    jiffshot_capture::visibility::install(move || {
        let (settle, hidden) = dispatch(&scope, ui_thread, true)?;
        // Let the compositor replace the hidden windows with the desktop underneath.
        if !settle.is_zero() {
            std::thread::sleep(settle);
        }
        let scope = scope.clone();
        let restore: Box<dyn FnOnce() + Send> = Box::new(move || {
            if let Err(error) = dispatch(&scope, ui_thread, false) {
                eprintln!("Could not restore Jiffshot windows after capture: {error}");
            }
        });
        Ok((restore, hidden))
    })
    .map_err(|error| rustolonia::Error::Load(error.to_string()))
}
