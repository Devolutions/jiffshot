//! Notices from the capture side ("Capture saved", "Recording failed"…):
//! a small borderless window in the bottom-right corner of the primary
//! screen that does not take the focus and goes away after a few seconds or
//! a click. One shows at a time; a new notice replaces the last.
//!
//! The model asks with the `toast` signal: "serial\x1ftitle\x1ftext\x1f1"
//! (the last field is 1 for a problem). The window is built once and shown
//! again for each notice.

use super::capture_view::parts::{self, post, rgb, SIZE_TO_CONTENT};
use super::dark_blue;
use super::icons::vector as vicons;
use super::theme_kit::{with_role, ButtonRole};
use rustolonia::{
    AppScope, Border, Brush, Button, Color, Dock, DockPanel, Effect, FontWeight, HorizontalAlignment, Orientation, Path, Result, Signal, StackPanel,
    TextBlock, TextWrapping, VerticalAlignment, Window, WindowDecorations,
};
use std::cell::RefCell;
use std::time::Duration;

/// How long a notice stays up.
const SHOWN_FOR: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToastSeverity {
    Success,
    Info,
    Warning,
    Error,
}

impl ToastSeverity {
    const fn icon_path(self) -> &'static str {
        match self {
            ToastSeverity::Success => vicons::STATUS_SUCCESS,
            ToastSeverity::Info => vicons::STATUS_INFO,
            ToastSeverity::Warning => vicons::STATUS_WARNING,
            ToastSeverity::Error => vicons::STATUS_ERROR,
        }
    }

    const fn color(self, dark: bool) -> Color {
        match (self, dark) {
            (ToastSeverity::Success, true) => rgb(0x3FB950),
            (ToastSeverity::Success, false) => rgb(0x1A7F37),
            (ToastSeverity::Info, true) => rgb(dark_blue::windows_or(dark_blue::ACCENT_TEXT, 0x3D8BFF)),
            (ToastSeverity::Info, false) => rgb(dark_blue::windows_or(0x0068C3, 0x2F5DA8)),
            (ToastSeverity::Warning, true) => rgb(0xF5A623),
            (ToastSeverity::Warning, false) => rgb(0xB54708),
            (ToastSeverity::Error, true) => rgb(0xF04438),
            (ToastSeverity::Error, false) => rgb(0xD92D20),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ToastAction {
    OpenFile(String),
    OpenEditor,
    OpenLibraryFolder,
    OpenPrefs(&'static str),
    RetryCapture(&'static str),
}

impl ToastAction {
    const fn label(&self) -> &'static str {
        match self {
            ToastAction::OpenFile(_) => "Open",
            ToastAction::OpenEditor => "Open in Editor",
            ToastAction::OpenLibraryFolder => "Open folder",
            ToastAction::OpenPrefs(_) => "Preferences",
            ToastAction::RetryCapture(_) => "Retry",
        }
    }

    fn execute(&self) {
        match self {
            ToastAction::OpenFile(path) => {
                let path = path.clone();
                crate::app::post(move |app| {
                    app.show_editor();
                    app.open_path(std::path::Path::new(&path));
                });
            }
            ToastAction::OpenEditor => {
                crate::app::post(|app| app.show_editor());
            }
            ToastAction::OpenLibraryFolder => {
                super::main().model.run(|m| m.open_library_folder());
            }
            ToastAction::OpenPrefs(tab) => {
                let action = format!("prefs:{tab}");
                super::main().model.run(move |m| m.run(action));
            }
            ToastAction::RetryCapture(mode) => {
                let action = format!("widget:{mode}");
                super::main().model.run(move |m| m.run(action));
            }
        }
    }
}

fn looks_like_media_path(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    let has_ext = [".mp4", ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".pdf", ".jsd"]
        .iter()
        .any(|ext| lower.ends_with(ext));
    has_ext && (trimmed.contains('\\') || trimmed.contains('/'))
}

fn classify_toast(title: &str, text: &str, problem: bool) -> (ToastSeverity, Option<ToastAction>) {
    let title_lower = title.to_ascii_lowercase();
    let text_lower = text.to_ascii_lowercase();

    let severity = if problem {
        if title_lower.contains("failed")
            || title_lower.contains("could not")
            || title_lower.contains("unavailable")
            || text_lower.contains("failed")
            || text_lower.starts_with("could not")
        {
            ToastSeverity::Error
        } else {
            ToastSeverity::Warning
        }
    } else if title_lower.contains("saved")
        || title_lower.contains("exported")
        || title_lower.contains("copied")
    {
        ToastSeverity::Success
    } else {
        ToastSeverity::Info
    };

    let action = if !problem {
        if looks_like_media_path(text) {
            Some(ToastAction::OpenFile(text.trim().to_owned()))
        } else if title_lower.contains("saved") {
            if text_lower.contains("opened in the editor") {
                Some(ToastAction::OpenLibraryFolder)
            } else {
                Some(ToastAction::OpenEditor)
            }
        } else {
            None
        }
    } else if title_lower.contains("hotkey") || text_lower.contains("hotkey") {
        Some(ToastAction::OpenPrefs("hotkeys"))
    } else if title_lower.contains("preferences") {
        let tab = if text_lower.contains("share") { "share" } else { "capture" };
        Some(ToastAction::OpenPrefs(tab))
    } else if title_lower.contains("login") || title_lower.contains("startup") || title_lower.contains("permission") {
        Some(ToastAction::OpenPrefs("capture"))
    } else if title_lower.contains("recording failed") {
        Some(ToastAction::RetryCapture("video"))
    } else if title_lower.contains("scrolling capture failed") {
        Some(ToastAction::RetryCapture("scrolling"))
    } else if title_lower.starts_with("grab text") {
        Some(ToastAction::RetryCapture("text"))
    } else if title_lower.contains("capture failed") || title_lower == "region not selected" {
        Some(ToastAction::RetryCapture("image"))
    } else {
        None
    };

    (severity, action)
}

/// Card, border, title and text colors: light, dark.
const CARD: [Color; 2] = [rgb(0xFFFFFF), rgb(dark_blue::windows_or(dark_blue::TOAST, 0x2B2D31))];
const EDGE: [Color; 2] = [rgb(0xD5D8DD), rgb(dark_blue::windows_or(dark_blue::STRONG_BORDER, 0x3A3C40))];
const TITLE: [Color; 2] = [rgb(0x1F2328), rgb(dark_blue::windows_or(dark_blue::TEXT, 0xF2F3F5))];
const TEXT: [Color; 2] = [rgb(0x4B5563), rgb(dark_blue::windows_or(dark_blue::SECONDARY_TEXT, 0xC8CACE))];

/// The toast window and what it shows. UI thread only.
struct Toast {
    window: Window,
    title: Signal<String>,
    text: Signal<String>,
    severity: Signal<ToastSeverity>,
    action: Signal<Option<ToastAction>>,
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
            let signals = (toast.title, toast.text, toast.severity, toast.action, toast.dark);
            (toast.window.clone(), signals, toast.shown)
        })
    }) else {
        return Ok(());
    };
    let (severity, next_action) = classify_toast(&title, &text, problem);
    // Outside the borrow: the window reacts to them.
    let (title_signal, text_signal, severity_signal, action_signal, dark) = signals;
    window.set_title(format!("Jiffshot: {title}"))?;
    dark.set_if_changed(parts::dark_theme(scope));
    title_signal.set_if_changed(title);
    text_signal.set_if_changed(text);
    severity_signal.set_if_changed(severity);
    action_signal.set_if_changed(next_action);
    if !window.is_visible()? {
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
    let severity = scope.signal(ToastSeverity::Info);
    let action: Signal<Option<ToastAction>> = scope.signal(None);
    let dark = scope.signal(parts::dark_theme(scope));
    let tone = move |colors: [Color; 2]| colors[usize::from(dark.get())];
    let severity_color = move || severity.get().color(dark.get());
    let severity_brush = move || Some(Brush::solid(severity.get().color(dark.get())));
    let resize_scope = scope.clone();
    let closing_scope = scope.clone();

    let action_btn = with_role(
        Button::el()
            .name("ToastActionButton")
            .content(TextBlock::el().text(move || action.with(|a| a.as_ref().map_or("", ToastAction::label).to_owned())))
            .visible(move || action.with(Option::is_some))
            .vertical_alignment(VerticalAlignment::Center)
            .margin((12.0, 0.0, 0.0, 0.0))
            .on_click(move |_| {
                if let Some(act) = action.get_untracked() {
                    hide();
                    act.execute();
                }
            }),
        dark,
        ButtonRole::Compact,
    );

    let close_btn = Button::el()
        .name("ToastDismissButton")
        .width(26.0)
        .height(26.0)
        .min_width(26.0)
        .min_height(26.0)
        .padding(0.0)
        .corner_radius(4.0)
        .margin((6.0, 0.0, 0.0, 0.0))
        .background(parts::TRANSPARENT)
        .vertical_alignment(VerticalAlignment::Center)
        .horizontal_content_alignment(HorizontalAlignment::Center)
        .vertical_content_alignment(VerticalAlignment::Center)
        .tool_tip_tip("Dismiss notification")
        .content(
            Path::el()
                .width(10.0)
                .height(10.0)
                .data(vicons::CLOSE)
                .stroke(move || Some(Brush::solid(tone(TEXT))))
                .stroke_thickness(1.5),
        )
        .on_click(|_| hide());

    let severity_icon = Border::el()
        .width(22.0)
        .height(22.0)
        .margin((12.0, 11.0, 0.0, 0.0))
        .vertical_alignment(VerticalAlignment::Top)
        .dock_panel_dock(Dock::Left)
        .child(
            Path::el()
                .width(16.0)
                .height(16.0)
                .data(move || severity.get().icon_path().to_owned())
                .stroke(severity_brush)
                .stroke_thickness(1.6)
                .horizontal_alignment(HorizontalAlignment::Center)
                .vertical_alignment(VerticalAlignment::Center),
        );

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
                if window.is_visible().unwrap_or(false) {
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
                                .background(severity_color)
                                .corner_radius((8.0, 0.0, 0.0, 8.0))
                                .dock_panel_dock(Dock::Left),
                        )
                        .child(severity_icon)
                        .child(
                            StackPanel::el()
                                .orientation(Orientation::Horizontal)
                                .dock_panel_dock(Dock::Right)
                                .margin((0.0, 10.0, 10.0, 10.0))
                                .child(action_btn)
                                .child(close_btn),
                        )
                        .child(
                            StackPanel::el()
                                .margin((10.0, 10.0, 8.0, 12.0))
                                .spacing(3.0)
                                .vertical_alignment(VerticalAlignment::Center)
                                .child(TextBlock::el().text(title).font_weight(FontWeight::DemiBold).font_size(13.0).foreground(move || tone(TITLE)))
                                .child(
                                    TextBlock::el()
                                        .text(text)
                                        .font_size(12.0)
                                        .text_wrapping(TextWrapping::Wrap)
                                        .max_width(320.0)
                                        .visible(move || text.with(|t| !t.is_empty()))
                                        .foreground(move || tone(TEXT)),
                                ),
                        ),
                ),
        )
        .build(scope)?;
    scope.set_transparent(&window, true)?;
    scope.set_topmost(&window, true)?;
    Ok(Toast {
        window, title, text, severity, action, dark, shown: 0,
        #[cfg(windows)]
        prepared: false,
        #[cfg(target_os = "macos")]
        pending: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_toast_maps_completions_warnings_and_errors_to_distinct_severities_and_actions() {
        assert_eq!(
            classify_toast("Capture saved", "Copied to the clipboard; opened in the editor", false),
            (ToastSeverity::Success, Some(ToastAction::OpenLibraryFolder))
        );
        assert_eq!(
            classify_toast("Capture saved", "Saved in library", false),
            (ToastSeverity::Success, Some(ToastAction::OpenEditor))
        );
        assert_eq!(
            classify_toast("Video exported", r"C:\Captures\demo.mp4", false),
            (ToastSeverity::Success, Some(ToastAction::OpenFile(r"C:\Captures\demo.mp4".into())))
        );
        assert_eq!(
            classify_toast("Recording notice", "Synthetic notification", false),
            (ToastSeverity::Info, None)
        );
        assert_eq!(
            classify_toast("Font substitution", "Replaced missing family", true),
            (ToastSeverity::Warning, None)
        );
        assert_eq!(
            classify_toast("Hotkeys not changed", "Ctrl+Alt+1 is already in use", true),
            (ToastSeverity::Warning, Some(ToastAction::OpenPrefs("hotkeys")))
        );
        assert_eq!(
            classify_toast("Recording failed", "The microphone is in use by another app", true),
            (ToastSeverity::Error, Some(ToastAction::RetryCapture("video")))
        );
        assert_eq!(
            classify_toast("Grab text", "No text found in the selection", true),
            (ToastSeverity::Warning, Some(ToastAction::RetryCapture("text")))
        );
        assert_eq!(
            classify_toast("Capture failed", "Could not capture the window", true),
            (ToastSeverity::Error, Some(ToastAction::RetryCapture("image")))
        );
    }
}
