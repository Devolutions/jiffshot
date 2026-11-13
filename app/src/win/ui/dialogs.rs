//! The editor's small modal dialogs: keyboard shortcuts, About, resize image
//! and trim (Preferences is its own window).

use rustolonia::{
    AppScope, AsControl, Button, CheckBox, Dock, DockPanel, FontWeight, Grid, HorizontalAlignment, NumericUpDown, Orientation, Result, StackPanel, TextBlock, TextWrapping, Thickness, VerticalAlignment, Window,
    WindowStartupLocation,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// `SizeToContent.WidthAndHeight`.
const SIZE_TO_CONTENT: rustolonia::SizeToContent = rustolonia::SizeToContent::WIDTH_AND_HEIGHT;

/// What the OK button closes a dialog with.
const ACCEPTED: &str = "ok";

/// A dialog window: `body` above OK and Cancel, or Close alone when `ok` is
/// `None`.
fn frame(scope: &AppScope, title: &str, body: &impl AsControl, ok: Option<&str>) -> Result<Window> {
    let window = {
        let control = Window::new()?;
        control.set_title(title)?;
        control.set_size_to_content(SIZE_TO_CONTENT)?;
        control.set_can_resize(false)?;
        control.set_show_in_taskbar(false)?;
        control.set_window_startup_location(WindowStartupLocation::CenterOwner)?;
        control.set_icon(super::icon_file())?;
        control
    };
    let buttons = {
        let control = StackPanel::new()?;
        control.set_orientation(Orientation::Horizontal)?;
        control.set_spacing(8.0)?;
        control.set_horizontal_alignment(HorizontalAlignment::Right)?;
        control.set_margin(Thickness::new(0.0, 16.0, 0.0, 0.0))?;
        control
    };
    let button = |label: &str| -> Result<Button> { {
        let control = Button::new()?;
        control.set_content_text(label)?;
        control.set_min_width(80.0)?;
        control.set_horizontal_content_alignment(HorizontalAlignment::Center)?;
        Ok(control)
    } };
    if let Some(ok) = ok {
        let closes = window.clone();
        let accept = button(ok)?;
        accept.set_default(true)?;
        accept.on_click(scope, move |_| {
            let _ = closes.close_with_object(ACCEPTED);
        })?;
        buttons.children()?.add(accept)?;
    }
    let closes = window.clone();
    let cancel = button(if ok.is_some() { "Cancel" } else { "Close" })?;
    cancel.set_cancel(true)?;
    cancel.on_click(scope, move |_| {
        let _ = closes.close();
    })?;
    buttons.children()?.add(cancel)?;
    let root = {
        let control = DockPanel::new()?;
        control.set_margin(Thickness::uniform(20.0))?;
        control
    };
    buttons.set_dock_panel_dock(Dock::Bottom)?;
    root.children()?.add(buttons)?;
    root.children()?.add(body)?;
    window.set_content(&root)?;
    Ok(window)
}

/// Shows `dialog` over `owner`; `done` runs on the UI thread with whether
/// OK closed it.
fn ask(scope: &AppScope, owner: &Window, dialog: &Window, done: impl FnOnce(bool) + Send + 'static) -> Result<()> {
    let operation = scope.show_dialog(owner, dialog)?;
    scope.spawn(async move {
        let accepted = matches!(operation.await, Ok(Some(result)) if result == ACCEPTED);
        done(accepted);
    })
}

fn label(text: &str) -> Result<TextBlock> {
    {
        let control = TextBlock::new()?;
        control.set_text(text)?;
        control.set_vertical_alignment(VerticalAlignment::Center)?;
        control.set_margin(Thickness::new(0.0, 0.0, 12.0, 0.0))?;
        Ok(control)
    }
}

fn note(text: &str) -> Result<TextBlock> {
    {
        let control = TextBlock::new()?;
        control.set_text(text)?;
        control.set_font_size(11.5)?;
        control.set_opacity(0.7)?;
        control.set_text_wrapping(TextWrapping::Wrap)?;
        control.set_max_width(380.0)?;
        control.set_margin(Thickness::new(0.0, 6.0, 0.0, 0.0))?;
        Ok(control)
    }
}

fn number(value: u32, min: u32, max: u32) -> Result<NumericUpDown> {
    {
        let control = NumericUpDown::new()?;
        control.set_minimum(rustolonia::Decimal::from(i64::from(min)))?;
        control.set_maximum(rustolonia::Decimal::from(i64::from(max)))?;
        control.set_value(rustolonia::Decimal::from(i64::from(value)))?;
        control.set_increment(rustolonia::Decimal::ONE)?;
        control.set_format_string("0")?;
        control.set_width(150.0)?;
        Ok(control)
    }
}

/// A number box's value, rounded; `fallback` when it is empty or not a
/// number.
fn value_of(number: &NumericUpDown, fallback: u32) -> u32 {
    number.value().ok().flatten().map(|v| v.to_f64()).filter(|v| v.is_finite() && *v >= 0.0).map(|v| v.round() as u32).unwrap_or(fallback)
}

/// A two-column grid of labels and controls.
fn rows(rows: &[(&str, &NumericUpDown)]) -> Result<Grid> {
    let grid = {
        let control = Grid::new()?;
        control.set_column_definitions("Auto,*")?;
        control.set_row_definitions(vec!["Auto"; rows.len()].join(","))?;
        control.set_row_spacing(8.0)?;
        control
    };
    for (i, (text, control)) in rows.iter().enumerate() {
        let row = i32::try_from(i).unwrap_or(i32::MAX);
        let label = label(text)?;
        label.set_grid_row(row)?;
        control.set_grid_row(row)?;
        control.set_grid_column(1)?;
        grid.children()?.add(label)?;
        grid.children()?.add(*control)?;
    }
    Ok(grid)
}

/// Image ▸ Resize image: pixels or percent, keeping proportions or not.
/// `done` gets the new size when the user resizes.
pub fn resize_image(scope: &AppScope, owner: &Window, w: u32, h: u32, done: impl FnOnce(u32, u32) + Send + 'static) -> Result<()> {
    let (w, h) = (w.max(1), h.max(1));
    let width = number(w, 1, 16384)?;
    let height = number(h, 1, 16384)?;
    let percent = number(100, 1, 1000)?;
    let keep = {
        let control = CheckBox::new()?;
        control.set_content_text("Keep proportions")?;
        control.set_checked(Some(true))?;
        control
    };
    // Set while one box updates the others.
    let syncing = Arc::new(AtomicBool::new(false));
    let linked = |source: &NumericUpDown, update: Box<dyn Fn(f64) + Send>| -> Result<()> {
        let syncing = syncing.clone();
        source.clone().on_value_changed(scope, move |args| {
            let Some(v) = args.new_value.map(|v| v.to_f64()) else { return };
            if !syncing.swap(true, Ordering::SeqCst) {
                update(v);
                syncing.store(false, Ordering::SeqCst);
            }
        })?;
        Ok(())
    };
    let (fw, fh) = (f64::from(w), f64::from(h));
    let set = |number: &NumericUpDown, value: f64| {
        let result = rustolonia::Decimal::try_from(value.round().max(1.0)).and_then(|value| number.set_value(value));
        if let Err(error) = result {
            eprintln!("resize value: {error:?}");
        }
    };
    {
        let (height, percent, keep) = (height.clone(), percent.clone(), keep.clone());
        linked(
            &width,
            Box::new(move |v| {
                if keep.is_checked().ok().flatten() == Some(true) {
                    set(&height, v * fh / fw);
                    set(&percent, v * 100.0 / fw);
                }
            }),
        )?;
    }
    {
        let (width, percent, keep) = (width.clone(), percent.clone(), keep.clone());
        linked(
            &height,
            Box::new(move |v| {
                if keep.is_checked().ok().flatten() == Some(true) {
                    set(&width, v * fw / fh);
                    set(&percent, v * 100.0 / fh);
                }
            }),
        )?;
    }
    {
        let (width, height) = (width.clone(), height.clone());
        linked(
            &percent,
            Box::new(move |p| {
                set(&width, fw * p / 100.0);
                set(&height, fh * p / 100.0);
            }),
        )?;
    }
    let body = StackPanel::new()?;
    let children = body.children()?;
    children.add({
        let control = TextBlock::new()?;
        control.set_text(format!("Now {w} × {h} pixels."))?;
        control.set_margin(Thickness::new(0.0, 0.0, 0.0, 12.0))?;
        control
    })?;
    children.add(rows(&[("Width (px)", &width), ("Height (px)", &height), ("Percent", &percent)])?)?;
    children.add({
        let control = StackPanel::new()?;
        control.set_height(8.0)?;
        control
    })?;
    children.add(keep)?;
    children.add(note("Objects on the image are scaled with it.")?)?;
    let dialog = frame(scope, "Resize image", &body, Some("Resize"))?;
    ask(scope, owner, &dialog, move |accepted| {
        if accepted {
            done(value_of(&width, w).max(1), value_of(&height, h).max(1));
        }
    })
}

/// Image ▸ Trim: pixels to remove from each side. `done` gets the left,
/// top, right and bottom amounts when the user trims.
pub fn trim(scope: &AppScope, owner: &Window, w: u32, h: u32, done: impl FnOnce([u32; 4]) + Send + 'static) -> Result<()> {
    let (max_x, max_y) = (w.saturating_sub(1), h.saturating_sub(1));
    let left = number(0, 0, max_x)?;
    let top = number(0, 0, max_y)?;
    let right = number(0, 0, max_x)?;
    let bottom = number(0, 0, max_y)?;
    let body = StackPanel::new()?;
    let children = body.children()?;
    children.add({
        let control = TextBlock::new()?;
        control.set_text("Pixels to remove from each side:")?;
        control.set_margin(Thickness::new(0.0, 0.0, 0.0, 12.0))?;
        control
    })?;
    children.add(rows(&[("Left", &left), ("Top", &top), ("Right", &right), ("Bottom", &bottom)])?)?;
    children.add(note("Image ▸ Auto trim removes edges of one solid color by itself.")?)?;
    let dialog = frame(scope, "Trim", &body, Some("Trim"))?;
    ask(scope, owner, &dialog, move |accepted| {
        if accepted {
            done([value_of(&left, 0), value_of(&top, 0), value_of(&right, 0), value_of(&bottom, 0)]);
        }
    })
}

/// Help ▸ Keyboard shortcuts' list.
const SHORTCUTS: [(&str, &str); 28] = [
    ("Ctrl+Shift+N", "New capture"),
    ("Ctrl+N", "New image"),
    ("Ctrl+O", "Open"),
    ("Ctrl+S", "Save"),
    ("Ctrl+Shift+S", "Save as"),
    ("Ctrl+E", "Export"),
    ("Ctrl+P", "Print"),
    ("Ctrl+W", "Close"),
    ("Ctrl+Shift+V", "New from clipboard"),
    ("Ctrl+Z / Ctrl+Y", "Undo / redo"),
    ("Ctrl+C / Ctrl+X / Ctrl+V", "Copy / cut / paste"),
    (if cfg!(target_os = "macos") { "Backspace / Cmd+Backspace / Delete" } else { "Delete" }, "Delete selection"),
    ("Ctrl+Shift+C", "Copy all"),
    ("Ctrl+A / Ctrl+D", "Select all / deselect"),
    ("Ctrl+Shift+I", "Invert selection"),
    ("Ctrl+Shift+G / U", "Group / ungroup"),
    ("Ctrl+Shift+F / B", "Bring forward / send backward"),
    ("Ctrl+T / Ctrl+Shift+T", "Flatten selected / all"),
    ("Ctrl+1 … Ctrl+9", "The first nine tools"),
    ("Ctrl++ / Ctrl+−", "Zoom in / out"),
    ("Ctrl+0", "Actual size"),
    ("Ctrl+Alt+0", "Fit"),
    ("Ctrl+G", "Pixel grid"),
    ("Ctrl+L", "Library view"),
    ("Ctrl+,", "Editor preferences"),
    ("Shift+drag a corner", "Stretch a picture freely / keep a shape's proportions"),
    ("Esc", "Deselect"),
    ("F1", "These shortcuts"),
];

/// Shows the keyboard shortcuts over `owner`.
pub fn keyboard_shortcuts(scope: &AppScope, owner: &Window) -> Result<()> {
    let grid = {
        let control = Grid::new()?;
        control.set_column_definitions("Auto,*")?;
        control.set_row_definitions(vec!["Auto"; SHORTCUTS.len()].join(","))?;
        control.set_row_spacing(4.0)?;
        control
    };
    for (i, (keys, what)) in SHORTCUTS.iter().enumerate() {
        let row = i32::try_from(i).unwrap_or(i32::MAX);
        grid.children()?.add({
            let control = TextBlock::new()?;
            control.set_text(super::shortcut(keys))?;
            control.set_font_family("Consolas, Cascadia Mono, monospace")?;
            control.set_margin(Thickness::new(0.0, 0.0, 18.0, 0.0))?;
            control.set_grid_row(row)?;
            control
        })?;
        grid.children()?.add({
            let control = TextBlock::new()?;
            control.set_text(what)?;
            control.set_grid_row(row)?;
            control.set_grid_column(1)?;
            control
        })?;
    }
    let dialog = frame(scope, "Keyboard shortcuts", &grid, None)?;
    ask(scope, owner, &dialog, |_| ())
}

/// The answer to the unsaved-edits question asked before quitting.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsavedChoice {
    Save,
    Discard,
    Cancel,
}

/// What [`unsaved_changes`]'s dialog closes with for Save and Don't Save.
#[cfg(target_os = "macos")]
pub(crate) const UNSAVED_SAVE: &str = "save";
#[cfg(target_os = "macos")]
pub(crate) const UNSAVED_DISCARD: &str = "discard";

/// Asks over `owner` whether to save `name`'s edits before quitting and
/// returns the dialog; `done` runs on the UI thread with the answer (Cancel
/// when the dialog is closed).
#[cfg(target_os = "macos")]
pub fn unsaved_changes(scope: &AppScope, owner: &Window, name: &str, done: impl FnOnce(UnsavedChoice) + Send + 'static) -> Result<Window> {
    let window = {
        let control = Window::new()?;
        control.set_title("Quit Jiffshot")?;
        control.set_size_to_content(SIZE_TO_CONTENT)?;
        control.set_can_resize(false)?;
        control.set_show_in_taskbar(false)?;
        control.set_window_startup_location(WindowStartupLocation::CenterOwner)?;
        control.set_icon(super::icon_file())?;
        control
    };
    let body = {
        let control = StackPanel::new()?;
        control.set_spacing(6.0)?;
        control.set_max_width(380.0)?;
        control
    };
    body.children()?.add({
        let control = TextBlock::new()?;
        control.set_text(format!("Do you want to save the changes made to “{name}”?"))?;
        control.set_font_weight(FontWeight::DemiBold)?;
        control.set_text_wrapping(TextWrapping::Wrap)?;
        control
    })?;
    body.children()?.add(note("Your changes will be lost if you don't save them.")?)?;
    let button = |label: &str, result: Option<&'static str>| -> Result<Button> {
        let control = Button::new()?;
        control.set_content_text(label)?;
        control.set_min_width(80.0)?;
        control.set_horizontal_content_alignment(HorizontalAlignment::Center)?;
        let closes = window.clone();
        control.on_click(scope, move |_| {
            let _ = match result {
                Some(result) => closes.close_with_object(result),
                None => closes.close(),
            };
        })?;
        Ok(control)
    };
    let discard = button("Don't Save", Some(UNSAVED_DISCARD))?;
    discard.set_dock_panel_dock(Dock::Left)?;
    let cancel = button("Cancel", None)?;
    cancel.set_cancel(true)?;
    let save = button("Save…", Some(UNSAVED_SAVE))?;
    save.set_default(true)?;
    let right = {
        let control = StackPanel::new()?;
        control.set_orientation(Orientation::Horizontal)?;
        control.set_spacing(8.0)?;
        control.set_horizontal_alignment(HorizontalAlignment::Right)?;
        control.set_margin(Thickness::new(24.0, 0.0, 0.0, 0.0))?;
        control
    };
    right.children()?.add(cancel)?;
    right.children()?.add(save)?;
    let buttons = {
        let control = DockPanel::new()?;
        control.set_margin(Thickness::new(0.0, 16.0, 0.0, 0.0))?;
        control
    };
    buttons.children()?.add(discard)?;
    buttons.children()?.add(right)?;
    let root = {
        let control = DockPanel::new()?;
        control.set_margin(Thickness::uniform(20.0))?;
        control
    };
    buttons.set_dock_panel_dock(Dock::Bottom)?;
    root.children()?.add(buttons)?;
    root.children()?.add(body)?;
    window.set_content(&root)?;
    let operation = scope.show_dialog(owner, &window)?;
    scope.spawn(async move {
        let choice = match operation.await {
            Ok(Some(result)) if result == UNSAVED_SAVE => UnsavedChoice::Save,
            Ok(Some(result)) if result == UNSAVED_DISCARD => UnsavedChoice::Discard,
            _ => UnsavedChoice::Cancel,
        };
        done(choice);
    })?;
    Ok(window)
}

/// Shows the About dialog over `owner`.
pub fn about(scope: &AppScope, owner: &Window) -> Result<()> {
    let body = {
        let control = StackPanel::new()?;
        control.set_spacing(6.0)?;
        control
    };
    let children = body.children()?;
    children.add({
        let control = TextBlock::new()?;
        control.set_text("Jiffshot Editor")?;
        control.set_font_size(18.0)?;
        control.set_font_weight(FontWeight::DemiBold)?;
        control
    })?;
    children.add({
        let control = TextBlock::new()?;
        control.set_text(format!("Version {}", env!("CARGO_PKG_VERSION")))?;
        control
    })?;
    children.add({
        let control = TextBlock::new()?;
        control.set_text("Screen capture and image editing.")?;
        control.set_margin(Thickness::new(0.0, 6.0, 0.0, 0.0))?;
        control
    })?;
    children.add({
        let control = TextBlock::new()?;
        control.set_text("By Devolutions")?;
        control
    })?;
    for (label, url) in [("Devolutions/jiffshot", "https://github.com/Devolutions/jiffshot"), ("devolutions.net", "https://devolutions.net")] {
        let url = url.to_owned();
        let link = Button::new()?;
        link.set_content_text(label)?;
        link.on_click(scope, move |_| {
            if let Err(error) = crate::winutil::open_url(&url) {
                eprintln!("Could not open {url}: {error}");
            }
        })?;
        children.add(link)?;
    }
    let dialog = frame(scope, "About Jiffshot", &body, None)?;
    ask(scope, owner, &dialog, |_| ())
}
