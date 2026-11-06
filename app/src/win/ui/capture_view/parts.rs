//! Parts the capture-side windows (the Capture window, the Capture Widget,
//! toasts, Grab Text and Preferences) share: option rows, event
//! subscriptions that end with the controls they belong to, small helpers
//! for the model's "serial\x1fpayload" values, and the Win32 calls that place
//! windows in physical pixels without activating them.

use rustolonia::{
    AnyEl, AppScope, Button, CheckBox, Color, ComboBox, ComboBoxItem, Control, Dock, DockPanel, El, EventSubscription, FolderPickerOptions, Grid,
    HorizontalAlignment, Key, NodeRef, NumericUpDown, PickerOutcome, Result, TextBlock, TextBox, TextWrapping, ThemeVariant, Upcast, VerticalAlignment,
    Window, WindowCloseReason,
};
use serde::Deserialize;
use serde_json::Value;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[cfg(windows)]
use windows_sys::Win32::Foundation::{HWND, RECT};
#[cfg(windows)]
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
#[cfg(windows)]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, GetWindowLongPtrW, GetWindowRect, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, SystemParametersInfoW, GWL_EXSTYLE, HWND_TOPMOST, SM_CXSCREEN,
    GWL_STYLE, SPI_GETWORKAREA, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WS_CAPTION, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_POPUP, WS_THICKFRAME,
};

/// The icon font of the glyphs on buttons.
pub const GLYPHS: &str = super::super::icons::FONT;

/// Avalonia's `SizeToContent.WidthAndHeight`.
pub const SIZE_TO_CONTENT: i32 = 3;

pub const TRANSPARENT: Color = Color::new(0, 0, 0, 0);

/// A color from `0xRRGGBB`.
pub const fn rgb(hex: u32) -> Color {
    Color::rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// Whether the application shows its dark theme (the editor sets the
/// application's theme from the Color theme preference).
pub fn dark_theme(scope: &AppScope) -> bool {
    !matches!(scope.actual_theme_variant(), Ok(ThemeVariant::Light))
}

/// The payload of a "serial\x1fpayload" value: the model bumps the serial
/// so that the same payload twice is two requests.
pub fn payload(value: &str) -> Option<&str> {
    value.split_once('\u{1f}').map(|(_, payload)| payload)
}

/// Whether a window's Closing is the application or Windows ending, which
/// must not be cancelled (other closes just hide the window for next time).
pub fn closing_for_good(close_reason: i32) -> bool {
    matches!(WindowCloseReason::try_from(close_reason), Ok(WindowCloseReason::ApplicationShutdown | WindowCloseReason::OSShutdown))
}

/// Runs `f` on the UI thread after what is running now, logging its error.
pub fn post(scope: &AppScope, what: &'static str, f: impl FnOnce() -> Result<()> + Send + 'static) {
    let posted = scope.post(move || {
        if let Err(error) = f() {
            eprintln!("{what}: {error:?}");
        }
    });
    if let Err(error) = posted {
        eprintln!("{what}: {error:?}");
    }
}

/// Runs `f` on the UI thread once `delay` has passed.
pub fn after(scope: &AppScope, delay: Duration, f: impl FnOnce() + Send + 'static) {
    let started = scope.delay(delay).and_then(|wait| {
        scope.spawn(async move {
            if wait.await.is_ok() {
                f();
            }
        })
    });
    if let Err(error) = started {
        eprintln!("timer: {error:?}");
    }
}

// ---- subscriptions ----------------------------------------------------------

/// Event subscriptions that end together, when the controls they belong to
/// are rebuilt or their window replaced (the `El::on_*` builders keep theirs
/// for the application's life). UI thread only.
#[derive(Clone, Default)]
pub struct Subscriptions(Rc<RefCell<Vec<EventSubscription>>>);

impl Subscriptions {
    pub fn keep(&self, subscription: EventSubscription) {
        self.0.borrow_mut().push(subscription);
    }

    /// Ends them all. Never from inside one of their own callbacks: post it.
    pub fn clear(&self) {
        let ended = std::mem::take(&mut *self.0.borrow_mut());
        drop(ended);
    }
}

/// Like `on_click`, with the subscription kept in `subs`. `f` gets the
/// button, for a menu or panel to open under it.
pub fn on_click<T>(el: El<T>, subs: &Subscriptions, mut f: impl FnMut(&T) + Send + 'static) -> El<T>
where
    T: Upcast<Button> + Clone + Send + 'static,
{
    let subs = subs.clone();
    el.with_op(move |_, control| {
        let this = control.clone();
        subs.keep(control.upcast_ref().subscribe_click(move |_| f(&this))?);
        Ok(())
    })
}

// ---- option rows ------------------------------------------------------------

/// One row of an option panel or a Preferences tab
/// (`jiffshot_workflow::panel::Row`).
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Row {
    Number {
        #[serde(default)]
        label: String,
        #[serde(default)]
        value: i64,
        #[serde(default)]
        min: i64,
        #[serde(default = "i64_max")]
        max: i64,
    },
    Check {
        #[serde(default)]
        label: String,
        #[serde(default)]
        value: bool,
    },
    Choice {
        #[serde(default)]
        label: String,
        #[serde(default)]
        choices: Vec<String>,
        #[serde(default)]
        value: usize,
    },
    Text {
        #[serde(default)]
        label: String,
        #[serde(default)]
        value: String,
    },
    Folder {
        #[serde(default)]
        label: String,
        #[serde(default)]
        value: String,
    },
    Hotkey {
        #[serde(default)]
        label: String,
        #[serde(default)]
        value: String,
        #[serde(default)]
        placeholder: String,
    },
    Note {
        #[serde(default)]
        text: String,
    },
    #[serde(other)]
    Unknown,
}

fn i64_max() -> i64 {
    i64::MAX
}

/// Where rows go: the Capture window's small option panels, or the
/// Preferences dialog (wider labels; notes keep a place in the values).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Form {
    Panel,
    Prefs,
}

/// A row's input, read when the panel or dialog is confirmed.
#[derive(Clone)]
pub enum Input {
    Number(NumericUpDown),
    Check(CheckBox),
    Choice(ComboBox),
    Text(TextBox),
    /// A recorded hotkey ("Ctrl+PrtScn"), or empty.
    Hotkey(Arc<Mutex<String>>),
    /// A note in Preferences: `null`, so that values line up with rows.
    Nothing,
}

impl Input {
    /// The value to send back: a number, a boolean, a choice's index or text.
    pub fn value(&self) -> Value {
        match self {
            Input::Number(control) => {
                let number = control.get_value().ok().flatten().and_then(|text| text.trim().parse::<f64>().ok()).unwrap_or(0.0);
                Value::from(number.round() as i64)
            }
            Input::Check(control) => Value::from(control.get_is_checked().ok().flatten() == Some(true)),
            Input::Choice(control) => Value::from(control.get_selected_index().unwrap_or(0).max(0)),
            Input::Text(control) => Value::from(control.get_text().ok().flatten().unwrap_or_default()),
            Input::Hotkey(value) => Value::from(value.lock().map(|v| v.clone()).unwrap_or_default()),
            Input::Nothing => Value::Null,
        }
    }
}

/// The values of `inputs`, as the JSON array the model reads.
pub fn values(inputs: &[Input]) -> Value {
    Value::Array(inputs.iter().map(Input::value).collect())
}

/// Builds `row`, adding its input (if it has one) to `inputs`. A folder
/// row's Choose… button opens the folder picker over `owner`.
pub fn row(scope: &AppScope, form: Form, row: &Row, owner: NodeRef<Window>, inputs: &mut Vec<Input>, subs: &Subscriptions) -> Result<Control> {
    let wide = form == Form::Prefs;
    let control = match row {
        Row::Number { label, value, min, max } => {
            let el = NumericUpDown::el()
                .value(value.to_string())
                .minimum(min.to_string())
                .maximum(max.to_string())
                .increment("1")
                .format_string("0")
                .grid_column(1);
            let el = if wide { el.width(140.0).horizontal_alignment(HorizontalAlignment::Left) } else { el.min_width(130.0) };
            let control = el.build(scope)?;
            inputs.push(Input::Number(control.clone()));
            labeled(scope, form, label, control.upcast())?
        }
        Row::Check { label, value } => {
            let control = CheckBox::el().content(label.clone()).checked(Some(*value)).build(scope)?;
            inputs.push(Input::Check(control.clone()));
            control.upcast()
        }
        Row::Choice { label, choices, value } => {
            let items: Vec<El<ComboBoxItem>> = choices.iter().map(|choice| ComboBoxItem::el().content(choice.clone())).collect();
            let control = ComboBox::el()
                .items_iter(items)
                .selected_index(i32::try_from(*value).unwrap_or(0))
                .min_width(if wide { 220.0 } else { 130.0 })
                .grid_column(1)
                .build(scope)?;
            inputs.push(Input::Choice(control.clone()));
            labeled(scope, form, label, control.upcast())?
        }
        Row::Text { label, value } => {
            let el = TextBox::el().text(value.clone()).grid_column(1);
            let control = if wide { el } else { el.min_width(130.0) }.build(scope)?;
            inputs.push(Input::Text(control.clone()));
            labeled(scope, form, label, control.upcast())?
        }
        Row::Folder { label, value } => {
            let path = TextBox::el().text(value.clone()).read_only(true).build(scope)?;
            let title = label.clone();
            let target = path.clone();
            let picker = scope.clone();
            let choose = on_click(Button::el().content("Choose\u{2026}").margin((8.0, 0.0, 0.0, 0.0)).dock_panel_dock(Dock::Right), subs, move |_| {
                pick_folder(&picker, owner, &title, &target);
            });
            let line = DockPanel::el().grid_column(1).child(choose).child(AnyEl::from_control(path.clone())).build(scope)?;
            inputs.push(Input::Text(path));
            labeled(scope, form, label, line.upcast())?
        }
        Row::Hotkey { label, value, placeholder } => {
            let line = hotkey_field(scope, value, placeholder, inputs, subs)?;
            labeled(scope, form, label, line.upcast())?
        }
        Row::Note { text } => {
            if wide {
                inputs.push(Input::Nothing);
                TextBlock::el().text(text.clone()).text_wrapping(TextWrapping::Wrap).opacity(0.75).build(scope)?.upcast()
            } else {
                TextBlock::el().text(text.clone()).text_wrapping(TextWrapping::Wrap).max_width(380.0).font_size(12.5).build(scope)?.upcast()
            }
        }
        Row::Unknown => {
            if wide {
                inputs.push(Input::Nothing);
            }
            TextBlock::el().build(scope)?.upcast()
        }
    };
    Ok(control)
}

/// A box that records the hotkey pressed in it, with a Clear button.
/// Jiffshot's own global hotkeys are off while it has the focus, so that
/// pressing one records it instead of starting a capture.
fn hotkey_field(scope: &AppScope, value: &str, placeholder: &str, inputs: &mut Vec<Input>, subs: &Subscriptions) -> Result<DockPanel> {
    let stored = Arc::new(Mutex::new(value.to_owned()));
    let placeholder = if placeholder.is_empty() { "None".to_owned() } else { placeholder.to_owned() };
    let shown = TextBlock::el().text(hotkey_caption(value, &placeholder, false)).build(scope)?;
    let field = Button::el()
        .content(AnyEl::from_control(shown.clone()))
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .horizontal_content_alignment(HorizontalAlignment::Left)
        .tool_tip_tip("Click, then press the keys; Backspace clears")
        .build(scope)?;
    let control: &Control = field.upcast_ref();
    let set = {
        let (stored, shown, placeholder) = (stored.clone(), shown.clone(), placeholder.clone());
        move |text: String, focused: bool| {
            let _ = shown.set_text(hotkey_caption(&text, &placeholder, focused));
            if let Ok(mut stored) = stored.lock() {
                *stored = text;
            }
        }
    };
    let current = {
        let stored = stored.clone();
        move || stored.lock().map(|v| v.clone()).unwrap_or_default()
    };
    {
        let (set, current) = (set.clone(), current.clone());
        subs.keep(control.subscribe_key_down(move |e| {
            if e.handled {
                return;
            }
            let Ok(key) = Key::try_from(e.key) else { return };
            match key {
                Key::Tab | Key::Escape | Key::Return => return,
                Key::Back | Key::Delete if e.key_modifiers == 0 => set(String::new(), true),
                _ => match hotkey_text(e.key, e.key_modifiers) {
                    Some(text) => set(text, true),
                    None => set(current(), true),
                },
            }
            e.handled = true;
        })?);
    }
    {
        // Windows reports Print Screen only when it is released.
        let set = set.clone();
        subs.keep(control.subscribe_key_up(move |e| {
            if !e.handled && e.key == Key::Snapshot as i32 {
                if let Some(text) = hotkey_text(e.key, e.key_modifiers) {
                    set(text, true);
                }
                e.handled = true;
            }
        })?);
    }
    {
        let (set, current) = (set.clone(), current.clone());
        subs.keep(control.subscribe_got_focus(move |_| {
            set(current(), true);
            pause_hotkeys(true);
        })?);
    }
    {
        let (set, current) = (set.clone(), current.clone());
        subs.keep(control.subscribe_lost_focus(move |_| {
            set(current(), false);
            pause_hotkeys(false);
        })?);
    }
    let clear = on_click(Button::el().content("Clear").margin((8.0, 0.0, 0.0, 0.0)).dock_panel_dock(Dock::Right), subs, move |_| set(String::new(), false));
    inputs.push(Input::Hotkey(stored));
    DockPanel::el().grid_column(1).child(clear).child(AnyEl::from_control(field)).build(scope)
}

/// What a hotkey box shows: the hotkey, what an empty one does, or a
/// prompt while it records.
fn hotkey_caption(value: &str, placeholder: &str, recording: bool) -> String {
    match (value.is_empty(), recording) {
        (false, _) if cfg!(windows) => value.replace("PrtScn", "Print Screen"),
        (false, _) => value.to_owned(),
        (true, true) => "Press the keys\u{2026}".to_owned(),
        (true, false) => placeholder.to_owned(),
    }
}

/// The hotkey for an Avalonia key and its modifiers, written the way
/// Jiffshot stores hotkeys ("Ctrl+Shift+1", "Ctrl+PrtScn"). Letters and
/// digits need Ctrl, Alt or the Windows (Command) key, so typing is not
/// taken over; function keys and Print Screen may stand alone.
pub fn hotkey_text(key: i32, modifiers: i32) -> Option<String> {
    use rustolonia::key_modifiers::{ALT, CONTROL, META, SHIFT};
    let d0 = Key::D0 as i32;
    let a = Key::A as i32;
    let f1 = Key::F1 as i32;
    let (name, bare) = match key {
        k if k == Key::Snapshot as i32 => ("PrtScn".to_owned(), true),
        k if (d0..=d0 + 9).contains(&k) => (char::from(b'0' + (k - d0) as u8).to_string(), false),
        k if (a..=a + 25).contains(&k) => (char::from(b'A' + (k - a) as u8).to_string(), false),
        k if (f1..=f1 + 23).contains(&k) => (format!("F{}", k - f1 + 1), true),
        _ => return None,
    };
    if !bare && modifiers & (CONTROL | ALT | META) == 0 {
        return None;
    }
    let mut text = String::new();
    for (bit, label) in [(CONTROL, "Ctrl"), (ALT, "Alt"), (SHIFT, "Shift"), (META, if cfg!(target_os = "macos") { "Cmd" } else { "Win" })] {
        if modifiers & bit != 0 {
            text.push_str(label);
            text.push('+');
        }
    }
    text.push_str(&name);
    Some(text)
}

/// Turns Jiffshot's global hotkeys off while a hotkey box records, and on.
pub fn pause_hotkeys(paused: bool) {
    #[cfg(any(windows, target_os = "macos"))]
    jiffshot_workflow::capture::pause_hotkeys(paused);
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = paused;
}
/// `control` (already in column 1) after its label.
fn labeled(scope: &AppScope, form: Form, label: &str, control: Control) -> Result<Control> {
    let text = TextBlock::el().text(label.to_owned()).vertical_alignment(VerticalAlignment::Center);
    let grid = match form {
        Form::Panel => Grid::el().column_definitions("150,*").child(text),
        Form::Prefs => Grid::el().column_definitions("220,*").child(text.text_wrapping(TextWrapping::Wrap).margin((0.0, 0.0, 12.0, 0.0))),
    };
    Ok(grid.child(AnyEl::from_control(control)).build(scope)?.upcast())
}

/// Lets the user choose a folder for `target`.
fn pick_folder(scope: &AppScope, owner: NodeRef<Window>, title: &str, target: &TextBox) {
    let Some(window) = owner.get() else { return };
    let operation = match scope.open_folder_picker(&window, &FolderPickerOptions::new().title(title)) {
        Ok(operation) => operation,
        Err(error) => return eprintln!("folder picker: {error:?}"),
    };
    let (scope, target) = (scope.clone(), target.clone());
    let spawned = scope.clone().spawn(async move {
        if let Ok(PickerOutcome::Selected(items)) = operation.await {
            if let Some(path) = items.first().and_then(|item| item.local_path()).map(|path| path.display().to_string()) {
                post(&scope, "folder picker", move || target.set_text(path));
            }
        }
    });
    if let Err(error) = spawned {
        eprintln!("folder picker: {error:?}");
    }
}

// ---- Win32 ------------------------------------------------------------------

/// The window's handle, once it has been shown.
pub fn hwnd(scope: &AppScope, window: &Window) -> Option<isize> {
    let handle = scope.platform_handle(window).ok().filter(|&handle| handle != 0)?;
    #[cfg(target_os = "macos")]
    crate::mac_window::window(handle)?;
    Some(handle)
}

/// Never activated by a click, not in Alt+Tab, and left out of screen
/// captures (unless `JIFFSHOT_HUD_CAPTURABLE` is set, to check it on
/// screen). Call once the window has been shown.
#[cfg(windows)]
pub fn make_tool_window(scope: &AppScope, window: &Window) {
    let Some(hwnd) = hwnd(scope, window) else { return };
    let hwnd = hwnd as HWND;
    // SAFETY: plain Win32 calls on our own window.
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) | (WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW) as isize;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style);
    }
    if std::env::var_os("JIFFSHOT_HUD_CAPTURABLE").is_none() {
        let _ = scope.set_excluded_from_capture(window, true);
    }
}

/// Makes a borderless window a popup, which Windows lets be smaller than
/// the minimum size of a top-level window (about 38 px high at 100%).
#[cfg(windows)]
pub fn make_popup(hwnd: isize) {
    let hwnd = hwnd as HWND;
    // SAFETY: plain Win32 calls on our own window.
    unsafe {
        let style = (GetWindowLongPtrW(hwnd, GWL_STYLE) & !((WS_CAPTION | WS_THICKFRAME) as isize)) | WS_POPUP as isize;
        SetWindowLongPtrW(hwnd, GWL_STYLE, style);
        SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

#[cfg(target_os = "macos")]
pub fn make_popup(handle: isize) {
    if let Some(window) = crate::mac_window::window(handle) {
        window.setMinSize(objc2_foundation::NSSize::new(0.0, 0.0));
    }
}

/// The window's outer size in physical pixels.
#[cfg(windows)]
pub fn outer_size(hwnd: isize) -> Option<(i32, i32)> {
    let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: `rect` is a valid out pointer.
    let ok = unsafe { GetWindowRect(hwnd as HWND, &mut rect) } != 0;
    ok.then(|| (rect.right - rect.left, rect.bottom - rect.top))
}

#[cfg(target_os = "macos")]
pub fn outer_size(handle: isize) -> Option<(i32, i32)> {
    let frame = crate::mac_window::window(handle)?.frame();
    let scale = jiffshot_capture::screen::scale_factor();
    Some(((frame.size.width * scale).round() as i32, (frame.size.height * scale).round() as i32))
}

/// The primary screen's work area (without the taskbar), in physical pixels.
#[cfg(windows)]
pub fn primary_work_area() -> jiffshot_capture::Rect {
    let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: SPI_GETWORKAREA writes one RECT.
    unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut rect as *mut RECT).cast(), 0) };
    jiffshot_capture::Rect::from_ltrb(rect.left, rect.top, rect.right, rect.bottom)
}

#[cfg(target_os = "macos")]
pub fn primary_work_area() -> jiffshot_capture::Rect {
    jiffshot_capture::screen::monitors().into_iter().find(|monitor| monitor.primary).map(|monitor| monitor.work).unwrap_or_default()
}

/// The primary screen's width in physical pixels (it starts at x = 0).
#[cfg(windows)]
pub fn primary_screen_width() -> i32 {
    // SAFETY: a plain Win32 query.
    unsafe { GetSystemMetrics(SM_CXSCREEN) }
}

/// The window's display scale (1 at 96 DPI).
#[cfg(windows)]
pub fn scale(hwnd: isize) -> f64 {
    // SAFETY: a plain Win32 query on our own window.
    let dpi = unsafe { GetDpiForWindow(hwnd as HWND) };
    if dpi == 0 { 1.0 } else { f64::from(dpi) / 96.0 }
}

#[cfg(target_os = "macos")]
pub fn scale(_handle: isize) -> f64 {
    jiffshot_capture::screen::scale_factor()
}

/// Moves the window's top-left corner to (`x`, `y`), topmost, without
/// activating it.
#[cfg(windows)]
pub fn move_topmost(hwnd: isize, x: i32, y: i32) {
    // SAFETY: a plain Win32 call on our own window.
    unsafe { SetWindowPos(hwnd as HWND, HWND_TOPMOST, x, y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE) };
}

/// Sets the native window's title (the taskbar's and Alt+Tab's) without
/// changing the one Avalonia draws.
#[cfg(windows)]
pub fn set_native_title(hwnd: isize, title: &str) {
    let title = jiffshot_capture::wide(title);
    // SAFETY: a NUL-terminated string that outlives the call.
    unsafe { SetWindowTextW(hwnd as HWND, title.as_ptr()) };
}

#[cfg(target_os = "macos")]
pub fn set_native_title(handle: isize, title: &str) {
    if let Some(window) = crate::mac_window::window(handle) {
        window.setTitle(&objc2_foundation::NSString::from_str(title));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustolonia::key_modifiers::{ALT, CONTROL, META, SHIFT};

    #[test]
    fn hotkey_text_records_supported_keys() {
        let win = if cfg!(target_os = "macos") { "Cmd" } else { "Win" };
        assert_eq!(hotkey_text(Key::Snapshot as i32, CONTROL).as_deref(), Some("Ctrl+PrtScn"));
        assert_eq!(hotkey_text(Key::Snapshot as i32, 0).as_deref(), Some("PrtScn"));
        assert_eq!(hotkey_text(Key::D1 as i32, CONTROL | SHIFT).as_deref(), Some("Ctrl+Shift+1"));
        assert_eq!(hotkey_text(Key::S as i32, SHIFT | ALT | META).as_deref(), Some(format!("Alt+Shift+{win}+S").as_str()));
        assert_eq!(hotkey_text(Key::F12 as i32, 0).as_deref(), Some("F12"));
        assert_eq!(hotkey_text(Key::A as i32, 0), None);
        assert_eq!(hotkey_text(Key::A as i32, SHIFT), None);
        assert_eq!(hotkey_text(Key::LeftCtrl as i32, CONTROL), None);
    }

    #[test]
    fn hotkey_caption_shows_value_placeholder_or_prompt() {
        assert_eq!(hotkey_caption("", "Default: F1", false), "Default: F1");
        assert_eq!(hotkey_caption("", "Default: F1", true), "Press the keys\u{2026}");
        let shown = hotkey_caption("Ctrl+PrtScn", "x", false);
        assert_eq!(shown, if cfg!(windows) { "Ctrl+Print Screen" } else { "Ctrl+PrtScn" });
    }
}