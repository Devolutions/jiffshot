//! Notices from the capture side ("Capture saved", "Recording failed"…):
//! a small borderless window in the bottom-right corner of the primary
//! screen that does not take the focus and goes away after a few seconds or
//! a click. One shows at a time; a new notice replaces the last.
//!
//! The model asks with the `toast` signal: "serial\x1ftitle\x1ftext\x1f1"
//! (the last field is 1 for a problem). The window is built once and shown
//! again for each notice.

use super::capture_view::parts::{self, post, rgb, SIZE_TO_CONTENT};
use rustolonia::{AppScope, Border, Color, Dock, DockPanel, Effect, FontWeight, Result, Signal, StackPanel, TextBlock, TextWrapping, Window, WindowDecorations};
use std::cell::RefCell;
use std::time::Duration;

/// How long a notice stays up.
const SHOWN_FOR: Duration = Duration::from_secs(5);

const ACCENT: Color = rgb(0x3D8BFF);
const PROBLEM: Color = rgb(0xE5534B);

/// Card, border, title and text colors: light, dark.
const CARD: [Color; 2] = [rgb(0xFFFFFF), rgb(0x2B2D31)];
const EDGE: [Color; 2] = [rgb(0xD5D8DD), rgb(0x3A3C40)];
const TITLE: [Color; 2] = [rgb(0x1F2328), rgb(0xF2F3F5)];
const TEXT: [Color; 2] = [rgb(0x4B5563), rgb(0xC8CACE)];

/// The toast window and what it shows. UI thread only.
struct Toast {
    window: Window,
    title: Signal<String>,
    text: Signal<String>,
    problem: Signal<bool>,
    dark: Signal<bool>,
    /// Bumped for each notice, so that an older one's timer leaves it up.
    shown: u64,
    #[cfg(windows)]
    prepared: bool,
    #[cfg(target_os = "macos")]
    pending: bool,
}

thread_local! {
    static TOAST: RefCell<Option<Toast>> = const { RefCell::new(None) };
}

/// Starts following the main view model's signals. Called once, on the UI
/// thread, after the main window is mounted.
pub fn install(scope: &AppScope) -> Result<()> {
    let toast = super::main().sink.toast;
    let scope = scope.clone();
    Effect::new(move || {
        let value = toast.get();
        // "serial\x1ftitle\x1ftext\x1fproblem"
        if let [_, title, text, problem] = value.split('\u{1f}').collect::<Vec<_>>()[..] {
            let (title, text, problem) = (title.to_owned(), text.to_owned(), problem == "1");
            let s = scope.clone();
            post(&scope, "toast", move || show(&s, title, text, problem));
        }
        Ok(())
    })?;
    Ok(())
}

/// Shows a notice, replacing the one on screen.
fn show(scope: &AppScope, title: String, text: String, problem: bool) -> Result<()> {
    if TOAST.with(|t| t.borrow().is_none()) {
        let toast = build(scope)?;
        TOAST.with(|t| *t.borrow_mut() = Some(toast));
    }
    let Some((window, signals, shown)) = TOAST.with(|t| {
        t.borrow_mut().as_mut().map(|toast| {
            toast.shown += 1;
            let signals = (toast.title, toast.text, toast.problem, toast.dark);
            (toast.window.clone(), signals, toast.shown)
        })
    }) else {
        return Ok(());
    };
    // Outside the borrow: the window reacts to them.
    let (title_signal, text_signal, problem_signal, dark) = signals;
    window.set_title(format!("Jiffshot: {title}"))?;
    dark.set_if_changed(parts::dark_theme(scope));
    title_signal.set_if_changed(title);
    text_signal.set_if_changed(text);
    problem_signal.set_if_changed(problem);
    if !window.get_is_visible()? {
        #[cfg(windows)]
        {
            // Shown off screen first: its size is only known once it is shown.
            window.set_position((-32000, -32000))?;
            window.show()?;
            let prepare = TOAST.with(|t| t.borrow_mut().as_mut().is_some_and(|toast| !std::mem::replace(&mut toast.prepared, true)));
            if prepare {
                parts::make_tool_window(scope, &window);
            }
        }
        #[cfg(target_os = "macos")]
        {
            TOAST.with(|t| {
                if let Some(toast) = t.borrow_mut().as_mut() {
                    toast.pending = true;
                }
            });
            let protected = match super::hud::native::show_protected(scope, &window, false) {
                Ok(protected) => protected,
                Err(error) => {
                    toast_failed(format!("could not protect the notification: {error}"));
                    return Ok(());
                }
            };
            let position_scope = scope.clone();
            let positioned = window.clone();
            let ready_scope = scope.clone();
            if let Err(error) = super::hud::native::reveal_when_excluded(
                scope, &window, protected, false,
                move || place(&position_scope, &positioned),
                move || {
                    let shown = TOAST.with(|t| t.borrow_mut().as_mut().map(|toast| {
                        toast.pending = false;
                        toast.shown
                    }));
                    if let Some(shown) = shown {
                        dismiss_after(&ready_scope, shown);
                    }
                    Ok(())
                },
                toast_failed,
            ) {
                if let Err(hide_error) = window.hide() {
                    eprintln!("toast: could not hide a failed notification: {hide_error:?}");
                }
                toast_failed(format!("could not schedule the notification: {error}"));
            }
            return Ok(());
        }
    }
    #[cfg(target_os = "macos")]
    if TOAST.with(|t| t.borrow().as_ref().is_some_and(|toast| toast.pending)) {
        return Ok(());
    }
    if let Err(error) = place(scope, &window) {
        eprintln!("toast: could not place the notification: {error}");
    }
    dismiss_after(scope, shown);
    Ok(())
}

fn dismiss_after(scope: &AppScope, shown: u64) {
    parts::after(scope, SHOWN_FOR, move || {
        if TOAST.with(|t| t.borrow().as_ref().is_some_and(|toast| toast.shown == shown)) {
            hide();
        }
    });
}

#[cfg(target_os = "macos")]
fn toast_failed(message: String) {
    TOAST.with(|t| {
        if let Some(toast) = t.borrow_mut().as_mut() {
            toast.pending = false;
        }
    });
    eprintln!("toast: {message}");
}

fn hide() {
    if let Some(window) = TOAST.with(|t| t.borrow_mut().as_mut().map(|toast| {
        #[cfg(target_os = "macos")]
        { toast.pending = false; }
        toast.window.clone()
    })) {
        let _ = window.hide();
    }
}

/// The bottom-right corner of the primary screen's work area, 16 px in.
fn place(scope: &AppScope, window: &Window) -> std::result::Result<(), String> {
    let hwnd = parts::hwnd(scope, window).ok_or("the native notification window is unavailable")?;
    let (width, height) = parts::outer_size(hwnd).ok_or("the notification window has no size")?;
    let area = parts::primary_work_area();
    if width <= 0 || height <= 0 || area.is_empty() {
        return Err("the notification window or primary display has invalid bounds".into());
    }
    let margin = (16.0 * parts::scale(hwnd)).round() as i32;
    #[cfg(windows)]
    {
        parts::move_topmost(hwnd, area.right() - width - margin, area.bottom() - height - margin);
        Ok(())
    }
    #[cfg(target_os = "macos")]
    super::hud::native::position(scope, window, area.right() - width - margin, area.bottom() - height - margin)
        .map_err(|error| error.to_string())
}

fn build(scope: &AppScope) -> Result<Toast> {
    let title = scope.signal(String::new());
    let text = scope.signal(String::new());
    let problem = scope.signal(false);
    let dark = scope.signal(parts::dark_theme(scope));
    let tone = move |colors: [Color; 2]| colors[usize::from(dark.get())];
    let resize_scope = scope.clone();
    let closing_scope = scope.clone();
    let window = Window::el()
        .title("Jiffshot")
        .window_decorations(WindowDecorations::None)
        .show_activated(false)
        .show_in_taskbar(false)
        .can_resize(false)
        .size_to_content(SIZE_TO_CONTENT)
        // See-through outside the rounded card.
        .background(parts::TRANSPARENT)
        .on_tapped(|_| hide())
        // A longer notice replacing a shorter one keeps the corner.
        .on_size_changed(move |_| {
            if let Some(window) = TOAST.with(|t| t.borrow().as_ref().and_then(|toast| {
                #[cfg(target_os = "macos")]
                if toast.pending { return None; }
                Some(toast.window.clone())
            })) {
                if window.get_is_visible().unwrap_or(false) {
                    if let Err(error) = place(&resize_scope, &window) {
                        eprintln!("toast: could not place the notification: {error}");
                    }
                }
            }
        })
        .on_closing(move |e| {
            // Kept for the next notice.
            if !parts::closing_for_good(e.close_reason) {
                e.cancel = true;
                post(&closing_scope, "toast", || {
                    hide();
                    Ok(())
                });
            }
        })
        .content(
            Border::el()
                .background(move || tone(CARD))
                .border_brush(move || tone(EDGE))
                .border_thickness(1.0)
                .corner_radius(8.0)
                .child(
                    DockPanel::el()
                        .child(
                            Border::el()
                                .width(4.0)
                                .background(move || if problem.get() { PROBLEM } else { ACCENT })
                                .corner_radius((8.0, 0.0, 0.0, 8.0))
                                .dock_panel_dock(Dock::Left),
                        )
                        .child(
                            StackPanel::el()
                                .margin((14.0, 10.0, 16.0, 12.0))
                                .spacing(3.0)
                                .child(TextBlock::el().text(title).font_weight(FontWeight::DemiBold).font_size(13.5).foreground(move || tone(TITLE)))
                                .child(TextBlock::el().text(text).font_size(12.0).text_wrapping(TextWrapping::Wrap).max_width(340.0).foreground(move || tone(TEXT))),
                        ),
                ),
        )
        .build(scope)?;
    scope.set_transparent(&window, true)?;
    scope.set_topmost(&window, true)?;
    Ok(Toast {
        window, title, text, problem, dark, shown: 0,
        #[cfg(windows)]
        prepared: false,
        #[cfg(target_os = "macos")]
        pending: false,
    })
}
