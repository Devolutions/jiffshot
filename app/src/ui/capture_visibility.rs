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
    hidden_since: Option<Instant>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
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
                state.hidden_since = None;
            }
            return Ok((Duration::ZERO, None));
        }
        if state.depth == 0 {
            let result = (|| {
                for window in scope.windows() {
                    if window.get_is_visible().map_err(io::Error::other)?
                        && window.get_window_state().map_err(io::Error::other)?
                            != WindowState::Minimized
                    {
                        let activated = window.get_show_activated().map_err(io::Error::other)?;
                        window.hide().map_err(io::Error::other)?;
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
                state.hidden_since = Some(Instant::now());
            }
        }
        state.depth += 1;
        let settle = state
            .hidden_since
            .map(|hidden| {
                (hidden + Duration::from_millis(150)).saturating_duration_since(Instant::now())
            })
            .unwrap_or_default();
        Ok((settle, state.hidden_since))
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
