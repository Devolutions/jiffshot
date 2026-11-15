//! The editor's small modal dialogs: keyboard shortcuts, About, resize image
//! and trim (Preferences is its own window).

use super::capture_view::parts;
use super::icons::vector as vicons;
use super::theme_kit::{apply_role_to_control, bind_window_theme, solid, with_role, ButtonRole, Surface};
use rustolonia::{
    AnyEl, AppScope, AsControl, Border, Button, CheckBox, Dock, DockPanel, Effect, FontWeight, Grid, HorizontalAlignment, NumericUpDown, Orientation, Path,
    Result, ScrollBarVisibility, ScrollViewer, Signal, StackPanel, TextBlock, TextWrapping, Thickness, VerticalAlignment, Window, WindowStartupLocation,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// `SizeToContent.WidthAndHeight`.
const SIZE_TO_CONTENT: rustolonia::SizeToContent = rustolonia::SizeToContent::WIDTH_AND_HEIGHT;

/// What the OK button closes a dialog with.
const ACCEPTED: &str = "ok";

/// A dialog window: `body` above a distinct footer with primary/secondary
/// buttons (`ok` + Cancel, or Close alone when `ok` is `None`).
fn frame(scope: &AppScope, title: &str, dark: Signal<bool>, body: &impl AsControl, ok: Option<&str>) -> Result<Window> {
    let window = {
        let control = Window::new()?;
        control.set_title(title)?;
        control.set_size_to_content(SIZE_TO_CONTENT)?;
        control.set_min_width(380.0)?;
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
        control
    };
    let make_button = |label: &str, role: ButtonRole| -> Result<Button> {
        let control = Button::new()?;
        control.set_content_text(label)?;
        control.set_horizontal_content_alignment(HorizontalAlignment::Center)?;
        let target = control.clone();
        Effect::new(move || apply_role_to_control(&target, dark.get(), role))?;
        Ok(control)
    };
    if let Some(ok) = ok {
        let closes = window.clone();
        let accept = make_button(ok, ButtonRole::Primary)?;
        accept.set_default(true)?;
        accept.on_click(scope, move |_| {
            let _ = closes.close_with_object(ACCEPTED);
        })?;
        buttons.children()?.add(accept)?;
    }
    let closes = window.clone();
    let cancel = make_button(if ok.is_some() { "Cancel" } else { "Close" }, ButtonRole::Secondary)?;
    if ok.is_none() {
        cancel.set_default(true)?;
    }
    cancel.set_cancel(true)?;
    cancel.on_click(scope, move |_| {
        let _ = closes.close();
    })?;
    buttons.children()?.add(cancel)?;

    let footer = Border::el()
        .dock_panel_dock(Dock::Bottom)
        .background(move || solid(Surface::for_theme(dark.get()).panel))
        .border_brush(move || solid(Surface::for_theme(dark.get()).border))
        .border_thickness((0.0, 1.0, 0.0, 0.0))
        .padding((22.0, 12.0))
        .child(AnyEl::from_control(buttons))
        .build(scope)?;

    let content_area = {
        let control = Border::new()?;
        control.set_padding(Thickness::new(22.0, 20.0, 22.0, 18.0))?;
        control.set_child(body)?;
        control
    };

    let root = DockPanel::new()?;
    root.children()?.add(footer)?;
    root.children()?.add(content_area)?;
    window.set_content(&root)?;
    bind_window_theme(scope, &window, dark)?;
    Ok(window)
}

fn card(scope: &AppScope, dark: Signal<bool>, inner: &impl AsControl, padding: (f64, f64)) -> Result<Border> {
    let border = Border::el()
        .corner_radius(8.0)
        .border_thickness(1.0)
        .padding(padding)
        .background(move || solid(Surface::for_theme(dark.get()).card))
        .border_brush(move || solid(Surface::for_theme(dark.get()).border))
        .build(scope)?;
    border.set_child(inner)?;
    Ok(border)
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

fn label(scope: &AppScope, dark: Signal<bool>, text: &str) -> Result<TextBlock> {
    TextBlock::el()
        .text(text.to_owned())
        .font_size(13.0)
        .vertical_alignment(VerticalAlignment::Center)
        .margin((0.0, 0.0, 18.0, 0.0))
        .foreground(move || solid(Surface::for_theme(dark.get()).text))
        .build(scope)
}

fn note(scope: &AppScope, dark: Signal<bool>, text: &str) -> Result<TextBlock> {
    TextBlock::el()
        .text(text.to_owned())
        .font_size(12.0)
        .text_wrapping(TextWrapping::Wrap)
        .max_width(400.0)
        .margin((2.0, 8.0, 2.0, 0.0))
        .foreground(move || solid(Surface::for_theme(dark.get()).muted_text))
        .build(scope)
}

fn number(value: u32, min: u32, max: u32) -> Result<NumericUpDown> {
    let control = NumericUpDown::new()?;
    control.set_minimum(rustolonia::Decimal::from(i64::from(min)))?;
    control.set_maximum(rustolonia::Decimal::from(i64::from(max)))?;
    control.set_value(rustolonia::Decimal::from(i64::from(value)))?;
    control.set_increment(rustolonia::Decimal::ONE)?;
    control.set_format_string("0")?;
    control.set_width(144.0)?;
    control.set_horizontal_alignment(HorizontalAlignment::Right)?;
    Ok(control)
}

/// A number box's value, rounded; `fallback` when it is empty or not a
/// number.
fn value_of(number: &NumericUpDown, fallback: u32) -> u32 {
    number.value().ok().flatten().map(|v| v.to_f64()).filter(|v| v.is_finite() && *v >= 0.0).map(|v| v.round() as u32).unwrap_or(fallback)
}

/// A two-column grid of labels and right-aligned controls.
fn rows(scope: &AppScope, dark: Signal<bool>, rows: &[(&str, &NumericUpDown)]) -> Result<Grid> {
    let grid = {
        let control = Grid::new()?;
        control.set_column_definitions("*,Auto")?;
        control.set_row_definitions(vec!["Auto"; rows.len()].join(","))?;
        control.set_row_spacing(10.0)?;
        control
    };
    for (i, (text, control)) in rows.iter().enumerate() {
        let row = i32::try_from(i).unwrap_or(i32::MAX);
        let label = label(scope, dark, text)?;
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
    // A taller scrolling capture must keep its size: a box clamped to the
    // limit would resize it to 16384 unasked (the model refuses larger sizes).
    let width = number(w, 1, w.max(16384))?;
    let height = number(h, 1, h.max(16384))?;
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
    let dark = scope.signal(parts::dark_theme(scope));
    let form = StackPanel::new()?;
    form.set_spacing(12.0)?;
    form.children()?.add(rows(scope, dark, &[("Width (px)", &width), ("Height (px)", &height), ("Scale (%)", &percent)])?)?;
    form.children()?.add(
        Border::el()
            .height(1.0)
            .background(move || solid(Surface::for_theme(dark.get()).divider))
            .build(scope)?,
    )?;
    form.children()?.add(keep)?;

    let body = StackPanel::new()?;
    body.set_min_width(360.0)?;
    let children = body.children()?;
    children.add(
        TextBlock::el()
            .text(format!("Current size: {w} × {h} pixels"))
            .font_size(12.5)
            .font_weight(FontWeight::DemiBold)
            .margin((2.0, 0.0, 0.0, 10.0))
            .foreground(move || solid(Surface::for_theme(dark.get()).secondary_text))
            .build(scope)?,
    )?;
    children.add(card(scope, dark, &form, (16.0, 14.0))?)?;
    children.add(note(scope, dark, "Objects on the image are scaled proportionally with it.")?)?;
    let dialog = frame(scope, "Resize image", dark, &body, Some("Resize"))?;
    ask(scope, owner, &dialog, move |accepted| {
        if accepted {
            done(value_of(&width, w).max(1), value_of(&height, h).max(1));
        }
    })
}

/// Image ▸ Trim: pixels to remove from each side. `done` gets the left,
/// top, right and bottom amounts when the user trims.
pub fn trim(scope: &AppScope, owner: &Window, w: u32, h: u32, done: impl FnOnce([u32; 4]) + Send + 'static) -> Result<()> {
    let dark = scope.signal(parts::dark_theme(scope));
    let (max_x, max_y) = (w.saturating_sub(1), h.saturating_sub(1));
    let left = number(0, 0, max_x)?;
    let top = number(0, 0, max_y)?;
    let right = number(0, 0, max_x)?;
    let bottom = number(0, 0, max_y)?;
    let body = StackPanel::new()?;
    body.set_min_width(360.0)?;
    let children = body.children()?;
    children.add(
        TextBlock::el()
            .text("Pixels to remove from each side")
            .font_size(12.5)
            .font_weight(FontWeight::DemiBold)
            .margin((2.0, 0.0, 0.0, 10.0))
            .foreground(move || solid(Surface::for_theme(dark.get()).secondary_text))
            .build(scope)?,
    )?;
    let grid = rows(scope, dark, &[("Left", &left), ("Top", &top), ("Right", &right), ("Bottom", &bottom)])?;
    children.add(card(scope, dark, &grid, (16.0, 14.0))?)?;
    children.add(note(scope, dark, "Image ▸ Auto trim removes uniform solid-color borders automatically.")?)?;
    let dialog = frame(scope, "Trim", dark, &body, Some("Trim"))?;
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
    let dark = scope.signal(parts::dark_theme(scope));
    let list = StackPanel::new()?;
    let list_children = list.children()?;
    for (i, (keys, what)) in SHORTCUTS.iter().enumerate() {
        if i > 0 {
            list_children.add(
                Border::el()
                    .height(1.0)
                    .background(move || solid(Surface::for_theme(dark.get()).divider))
                    .build(scope)?,
            )?;
        }
        let keycap = Border::el()
            .grid_column(1)
            .corner_radius(4.0)
            .border_thickness(1.0)
            .padding((8.0, 3.0))
            .margin((18.0, 0.0, 0.0, 0.0))
            .horizontal_alignment(HorizontalAlignment::Right)
            .vertical_alignment(VerticalAlignment::Center)
            .background(move || solid(Surface::for_theme(dark.get()).window))
            .border_brush(move || solid(Surface::for_theme(dark.get()).border))
            .child(
                TextBlock::el()
                    .text(super::shortcut(keys))
                    .font_size(11.5)
                    .font_family("Consolas, Cascadia Mono, monospace")
                    .foreground(move || solid(Surface::for_theme(dark.get()).secondary_text)),
            );
        let desc = TextBlock::el()
            .text((*what).to_owned())
            .font_size(12.5)
            .vertical_alignment(VerticalAlignment::Center)
            .foreground(move || solid(Surface::for_theme(dark.get()).text));
        let row = Border::el()
            .padding((14.0, 7.0, 18.0, 7.0))
            .child(Grid::el().column_definitions("*,Auto").child(desc).child(keycap))
            .build(scope)?;
        list_children.add(row)?;
    }
    let framed = card(scope, dark, &list, (0.0, 2.0))?;
    let scroller = ScrollViewer::el()
        .max_height(460.0)
        .min_width(480.0)
        .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
        .content(AnyEl::from_control(framed))
        .build(scope)?;
    let dialog = frame(scope, "Keyboard shortcuts", dark, &scroller, None)?;
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
    let dark = scope.signal(parts::dark_theme(scope));
    let window = {
        let control = Window::new()?;
        control.set_title("Quit Jiffshot")?;
        control.set_size_to_content(SIZE_TO_CONTENT)?;
        control.set_min_width(400.0)?;
        control.set_can_resize(false)?;
        control.set_show_in_taskbar(false)?;
        control.set_window_startup_location(WindowStartupLocation::CenterOwner)?;
        control.set_icon(super::icon_file())?;
        control
    };
    let body = {
        let control = StackPanel::new()?;
        control.set_spacing(6.0)?;
        control.set_max_width(400.0)?;
        control.set_margin(Thickness::new(22.0, 20.0, 22.0, 18.0))?;
        control
    };
    body.children()?.add(
        TextBlock::el()
            .text(format!("Do you want to save the changes made to “{name}”?"))
            .font_size(14.0)
            .font_weight(FontWeight::DemiBold)
            .text_wrapping(TextWrapping::Wrap)
            .foreground(move || solid(Surface::for_theme(dark.get()).text))
            .build(scope)?,
    )?;
    body.children()?.add(note(scope, dark, "Your changes will be lost if you don't save them.")?)?;
    let button = |label: &str, role: ButtonRole, result: Option<&'static str>| -> Result<Button> {
        let control = Button::new()?;
        control.set_content_text(label)?;
        control.set_horizontal_content_alignment(HorizontalAlignment::Center)?;
        let target = control.clone();
        Effect::new(move || apply_role_to_control(&target, dark.get(), role))?;
        let closes = window.clone();
        control.on_click(scope, move |_| {
            let _ = match result {
                Some(result) => closes.close_with_object(result),
                None => closes.close(),
            };
        })?;
        Ok(control)
    };
    let discard = button("Don't Save", ButtonRole::Secondary, Some(UNSAVED_DISCARD))?;
    discard.set_dock_panel_dock(Dock::Left)?;
    let cancel = button("Cancel", ButtonRole::Secondary, None)?;
    cancel.set_cancel(true)?;
    let save = button("Save…", ButtonRole::Primary, Some(UNSAVED_SAVE))?;
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
    let buttons = DockPanel::new()?;
    buttons.children()?.add(discard)?;
    buttons.children()?.add(right)?;
    let footer = Border::el()
        .dock_panel_dock(Dock::Bottom)
        .background(move || solid(Surface::for_theme(dark.get()).panel))
        .border_brush(move || solid(Surface::for_theme(dark.get()).border))
        .border_thickness((0.0, 1.0, 0.0, 0.0))
        .padding((22.0, 12.0))
        .child(AnyEl::from_control(buttons))
        .build(scope)?;
    let root = DockPanel::new()?;
    root.children()?.add(footer)?;
    root.children()?.add(body)?;
    window.set_content(&root)?;
    bind_window_theme(scope, &window, dark)?;
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
    let dark = scope.signal(parts::dark_theme(scope));
    let inner = {
        let control = StackPanel::new()?;
        control.set_spacing(6.0)?;
        control.set_min_width(340.0)?;
        control
    };
    let children = inner.children()?;
    children.add(
        TextBlock::el()
            .text("Jiffshot Editor")
            .font_size(18.0)
            .font_weight(FontWeight::DemiBold)
            .foreground(move || solid(Surface::for_theme(dark.get()).text))
            .build(scope)?,
    )?;
    children.add(
        TextBlock::el()
            .text(format!("Version {} · By Devolutions", env!("CARGO_PKG_VERSION")))
            .font_size(12.5)
            .foreground(move || solid(Surface::for_theme(dark.get()).secondary_text))
            .build(scope)?,
    )?;
    children.add(
        TextBlock::el()
            .text("Screen capture, video recording, and image annotation.")
            .font_size(12.5)
            .margin((0.0, 4.0, 0.0, 8.0))
            .foreground(move || solid(Surface::for_theme(dark.get()).muted_text))
            .build(scope)?,
    )?;
    let links = StackPanel::new()?;
    links.set_orientation(Orientation::Horizontal)?;
    links.set_spacing(8.0)?;
    for (label, url) in [("Devolutions/jiffshot", "https://github.com/Devolutions/jiffshot"), ("devolutions.net", "https://devolutions.net")] {
        let url_owned = url.to_owned();
        let icon = Path::el()
            .width(13.0)
            .height(13.0)
            .data(vicons::EXTERNAL_LINK)
            .stroke(move || Some(solid(Surface::for_theme(dark.get()).accent_text)))
            .stroke_thickness(1.4)
            .vertical_alignment(VerticalAlignment::Center);
        let content = StackPanel::el()
            .orientation(Orientation::Horizontal)
            .spacing(6.0)
            .child(TextBlock::el().text(label.to_owned()).vertical_alignment(VerticalAlignment::Center))
            .child(icon);
        let link = with_role(
            Button::el()
                .content(content)
                .tool_tip_tip(url.to_owned()),
            dark,
            ButtonRole::Link,
        )
        .build(scope)?;
        link.on_click(scope, move |_| {
            if let Err(error) = crate::winutil::open_url(&url_owned) {
                eprintln!("Could not open {url_owned}: {error}");
            }
        })?;
        links.children()?.add(link)?;
    }
    children.add(links)?;
    let framed = card(scope, dark, &inner, (18.0, 16.0))?;
    let dialog = frame(scope, "About Jiffshot", dark, &framed, None)?;
    ask(scope, owner, &dialog, |_| ())
}
