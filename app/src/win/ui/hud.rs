//! The capture side's on-screen furniture (the `hud` signal): control panels
//! next to the area being captured (countdown, recording, scrolling, Step
//! Capture…), red outlines just outside it, and the recording toolbar.
//!
//! The capture side owns the state and sends all of it as JSON each time
//! (`jiffshot_workflow`'s `hud.rs`); [`apply`] makes the windows match. None
//! of them takes the focus or shows in screen captures. A panel's button
//! answers `hud:ID:INDEX`, a toolbar's `hud:ID:act:NAME`, through the main
//! view model's `run`.

mod hud_toolbar;
#[cfg(target_os = "macos")]
pub(super) mod native_popup_macos;
#[cfg_attr(target_os = "macos", path = "hud/native_macos.rs")]
pub(in crate::ui) mod native;

use hud_toolbar::RecordingToolbar;
use jiffshot_capture::Rect;
use rustolonia::{
    size_to_content, AppScope, Border, Brush, Button, Color, Effect, EventSubscription, FontWeight, HorizontalAlignment, Orientation, Result, StackPanel,
    TextBlock, TextWrapping, ThemeVariantScope, VerticalAlignment, Window, WindowDecorations,
};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

thread_local! {
    static HUD: RefCell<Hud> = RefCell::new(Hud::default());
    #[cfg(target_os = "macos")]
    static TEST_MENU: RefCell<Option<rustolonia::MenuFlyout>> = const { RefCell::new(None) };
}

#[cfg(target_os = "macos")]
pub(super) fn toolbar_tooltip_for_selftest(id: u64, open: bool) -> std::result::Result<(), String> {
    let target = HUD.with(|hud| hud.borrow().toolbars.get(&id).map(RecordingToolbar::tooltip_for_selftest))
        .ok_or("the test toolbar is unavailable")?;
    if open {
        target.set_tool_tip_placement(rustolonia::PlacementMode::Top).map_err(|error| error.to_string())?;
    }
    target.set_tool_tip_is_open(open).map_err(|error| error.to_string())?;
    if !open {
        target.set_tool_tip_placement(rustolonia::PlacementMode::Pointer).map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(super) fn toolbar_menu_for_selftest(id: u64, open: bool) -> std::result::Result<(), String> {
    if open {
        let show = HUD.with(|hud| hud.borrow().toolbars.get(&id).map(|toolbar| toolbar.menu_for_selftest(id)))
            .ok_or("the test toolbar is unavailable")?;
        let menu = show().map_err(|error| error.to_string())?;
        TEST_MENU.with(|slot| *slot.borrow_mut() = Some(menu));
    } else {
        let menu = TEST_MENU.with(|slot| slot.borrow_mut().take()).ok_or("the test toolbar menu is unavailable")?;
        menu.hide().map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(super) fn close_toolbar_for_selftest(id: u64) -> std::result::Result<(), String> {
    let toolbar = HUD.with(|hud| hud.borrow_mut().toolbars.remove(&id)).ok_or("the test toolbar is unavailable")?;
    toolbar.close();
    Ok(())
}

/// Sends a command to the main view model (`hud:…`), on the UI thread.
fn run(command: String) {
    crate::ui::main().model.run(|m| m.run(command));
}

#[cfg(target_os = "macos")]
fn report_failure(message: String) {
    eprintln!("hud: {message}");
    crate::app::post(move |app| app.toast("Capture controls failed", &message, true));
}

#[cfg(target_os = "macos")]
fn show_protected(
    scope: &AppScope,
    window: &Window,
    click_through: bool,
    position: impl FnOnce() -> std::io::Result<()> + Send + 'static,
    failed: impl FnOnce(String) + Send + 'static,
) -> std::io::Result<()> {
    let number = native::show_protected(scope, window, click_through)?;
    let result = native::reveal_when_excluded(
        scope, window, number, click_through,
        move || position().map_err(|error| error.to_string()),
        || Ok(()),
        move |message| {
            report_failure(message.clone());
            failed(message);
        },
    );
    if result.is_err() {
        if let Err(error) = window.close() {
            eprintln!("hud: could not close a failed window: {error}");
        }
        if let Err(error) = native_popup_macos::discard_failed_owner(window) {
            eprintln!("hud: could not clear a failed window's popup gate: {error}");
        }
    }
    result
}

type HudResult<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The windows showing, by the capture side's ids.
#[derive(Default)]
struct Hud {
    panels: HashMap<u64, HudPanel>,
    borders: HashMap<u64, HudBorder>,
    toolbars: HashMap<u64, RecordingToolbar>,
}

/// A rectangle `[x, y, w, h]` (desktop pixels) of the JSON.
fn area_of(e: &Value) -> Rect {
    let a = e.get("area").and_then(Value::as_array);
    let v = |i: usize| a.and_then(|a| a.get(i)).and_then(Value::as_i64).unwrap_or(0) as i32;
    Rect::new(v(0), v(1), v(2), v(3))
}

/// `{"panels":[{"id","area":[x,y,w,h],"status","buttons":[…]} or {"id","area","toolbar":{…}}],"borders":[{"id","area"}]}`.
fn apply(scope: &AppScope, json: &str) {
    let _t = crate::trace::span("hud");
    if json.is_empty() {
        return;
    }
    let Ok(root) = serde_json::from_str::<Value>(json) else {
        eprintln!("hud: bad state");
        return;
    };
    let list = |name: &str| root.get(name).and_then(Value::as_array).cloned().unwrap_or_default();
    // Windows are made and closed outside the borrow: showing one runs its
    // event handlers.
    let mut closing: Vec<Box<dyn FnOnce()>> = Vec::new();
    let mut live = Vec::new();
    for p in list("panels") {
        let Some(id) = p.get("id").and_then(Value::as_u64) else { continue };
        live.push(id);
        if let Some(state) = p.get("toolbar") {
            let existing = HUD.with(|h| h.borrow().toolbars.get(&id).map(|t| t.update(state)).is_some());
            if !existing {
                match RecordingToolbar::new(scope, id, area_of(&p)) {
                    Ok(toolbar) => {
                        toolbar.update(state);
                        HUD.with(|h| h.borrow_mut().toolbars.insert(id, toolbar));
                    }
                    Err(error) => {
                        eprintln!("hud: no toolbar: {error:?}");
                        #[cfg(target_os = "macos")]
                        {
                            report_failure(format!("Could not show the recording toolbar: {error}"));
                            run(format!("hud:{id}:act:cancel"));
                        }
                    }
                }
            }
            continue;
        }
        let status = p.get("status").and_then(Value::as_str).unwrap_or_default();
        let existing = HUD.with(|h| h.borrow().panels.get(&id).map(|panel| panel.set_status(status)).is_some());
        if !existing {
            let buttons: Vec<String> = p
                .get("buttons")
                .and_then(Value::as_array)
                .map(|b| b.iter().map(|b| b.as_str().unwrap_or_default().to_owned()).collect())
                .unwrap_or_default();
            match HudPanel::new(scope, id, area_of(&p), &buttons, status) {
                Ok(panel) => {
                    HUD.with(|h| h.borrow_mut().panels.insert(id, panel));
                }
                Err(error) => {
                    eprintln!("hud: no panel: {error:?}");
                    #[cfg(target_os = "macos")]
                    {
                        report_failure(format!("Could not show the capture panel: {error}"));
                        run(format!("hud:{id}"));
                    }
                }
            }
        }
    }
    HUD.with(|h| {
        let mut hud = h.borrow_mut();
        let gone: Vec<u64> = hud.toolbars.keys().filter(|id| !live.contains(id)).copied().collect();
        for id in gone {
            if let Some(toolbar) = hud.toolbars.remove(&id) {
                closing.push(Box::new(move || toolbar.close()));
            }
        }
        let gone: Vec<u64> = hud.panels.keys().filter(|id| !live.contains(id)).copied().collect();
        for id in gone {
            if let Some(panel) = hud.panels.remove(&id) {
                closing.push(Box::new(move || panel.dismiss()));
            }
        }
    });

    let mut live = Vec::new();
    for b in list("borders") {
        let Some(id) = b.get("id").and_then(Value::as_u64) else { continue };
        live.push(id);
        if HUD.with(|h| h.borrow().borders.contains_key(&id)) {
            continue;
        }
        match HudBorder::new(scope, area_of(&b)) {
            Ok(border) => {
                HUD.with(|h| h.borrow_mut().borders.insert(id, border));
            }
            Err(error) => {
                eprintln!("hud: no outline: {error:?}");
                #[cfg(target_os = "macos")]
                report_failure(format!("Could not show the capture outline: {error}"));
            }
        }
    }
    HUD.with(|h| {
        let mut hud = h.borrow_mut();
        let gone: Vec<u64> = hud.borders.keys().filter(|id| !live.contains(id)).copied().collect();
        for id in gone {
            if let Some(border) = hud.borders.remove(&id) {
                closing.push(Box::new(move || border.close()));
            }
        }
    });
    for close in closing {
        close();
    }
}

/// Moves `window`, `size` DIPs, next to `area` (desktop pixels): below it,
/// else above it, else inside its bottom edge, `gap` DIPs away; centred
/// under the area or aligned with its right edge.
fn place_beside(_scope: &AppScope, window: &Window, area: Rect, size: (f64, f64), gap: f64, centred: bool) -> std::result::Result<(), String> {
    let (work, scale) = native::screen_of(area);
    let (w, h) = ((size.0 * scale).ceil() as i32, (size.1 * scale).ceil() as i32);
    let gap = (gap * scale).round() as i32;
    let x = if centred { area.x + (area.width - w) / 2 } else { area.right() - w };
    let (x, y) = native::beside(area, work, x, w, h, gap);
    #[cfg(target_os = "macos")]
    { native::position(_scope, window, x, y).map_err(|error| error.to_string()) }
    #[cfg(windows)]
    { window.set_position((x, y)).map_err(|error| error.to_string()) }
}

/// A borderless, topmost window that does not take the focus.
fn hud_window() -> Result<Window> {
    Window::new()?
        .title("Jiffshot")?
        .window_decorations(WindowDecorations::None)?
        .show_activated(false)?
        .show_in_taskbar(false)?
        .can_resize(false)
}

/// A capture panel: the status (live) and its buttons, the first one
/// the main action. It sits below the area, or above it, or inside its
/// bottom edge when neither fits on the screen.
struct HudPanel {
    window: Window,
    status: TextBlock,
    /// Set when the capture side closes the panel (any other close is Esc).
    dismissed: Arc<AtomicBool>,
    _subscriptions: Vec<EventSubscription>,
}

impl HudPanel {
    /// Gap between the area and the panel, past the outline (DIPs).
    const GAP: f64 = 11.0;

    fn new(scope: &AppScope, id: u64, area: Rect, buttons: &[String], status: &str) -> HudResult<Self> {
        let text = TextBlock::new()?
            .text(status)?
            .font_size(13.5)?
            .font_weight(FontWeight::DemiBold)?
            .text_wrapping(TextWrapping::Wrap)?
            .foreground(Brush::solid(Color::rgb(0xE8, 0xEA, 0xED)))?;
        let mut subscriptions = Vec::new();
        let rows = StackPanel::new()?.spacing(8.0)?;
        let columns = if buttons.len() > 4 { 2 } else { buttons.len().max(1) };
        for (row_index, labels) in buttons.chunks(columns).enumerate() {
            let row = StackPanel::new()?
                .orientation(Orientation::Horizontal)?
                .spacing(8.0)?
                .horizontal_alignment(HorizontalAlignment::Right)?;
            for (column, label) in labels.iter().enumerate() {
                let index = row_index * columns + column;
                let mut button = Button::new()?
                    .content_text(label)?
                    .min_width(76.0)?
                    .height(30.0)?
                    .padding((12.0, 0.0))?
                    .corner_radius(6.0)?
                    .horizontal_content_alignment(HorizontalAlignment::Center)?
                    .vertical_content_alignment(VerticalAlignment::Center)?
                    .foreground(Brush::solid(Color::rgb(255, 255, 255)))?
                    .background(Brush::solid(if index == 0 {
                        Color::rgb(0x2F, 0x5D, 0xA8)
                    } else {
                        Color::rgb(0x3A, 0x3C, 0x42)
                    }))?;
                if buttons.len() > 4 && labels.len() > 1 {
                    button = button.width(156.0)?;
                }
                subscriptions.push(button.subscribe_click(move |_| run(format!("hud:{id}:{index}")))?);
                row.children()?.add(&button)?;
            }
            rows.children()?.add(&row)?;
        }
        let content = Border::new()?
            .border_brush(Brush::solid(Color::rgb(0x3A, 0x3C, 0x42)))?
            .border_thickness(1.0)?
            .padding((12.0, 9.0, 12.0, 10.0))?
            .child(Some(&StackPanel::new()?.spacing(10.0)?.child(&text)?.child(&rows)?))?;
        let themed = ThemeVariantScope::new()?.requested_theme_variant("Dark")?.child(Some(&content))?;
        let window = hud_window()?
            .width(360.0)?
            .size_to_content(size_to_content::HEIGHT)?
            .background(Brush::solid(Color::rgb(0x23, 0x24, 0x28)))?
            .content(Some(&themed))?;
        scope.set_topmost(&window, true)?;
        let placed = window.clone();
        let placement_scope = scope.clone();
        #[cfg(target_os = "macos")]
        let placement_ready = Arc::new(AtomicBool::new(false));
        #[cfg(target_os = "macos")]
        let event_ready = placement_ready.clone();
        subscriptions.push(window.subscribe_size_changed(move |e| {
            #[cfg(target_os = "macos")]
            if !event_ready.load(Ordering::Acquire) {
                return;
            }
            if let Err(error) = place_beside(&placement_scope, &placed, area, (e.new_size.width, e.new_size.height), Self::GAP, false) {
                eprintln!("hud: placing a window failed: {error}");
            }
        })?);
        let dismissed = Arc::new(AtomicBool::new(false));
        let closed = dismissed.clone();
        // Closed by anything but the capture side (which never happens in
        // normal use): the same as Esc.
        subscriptions.push(window.subscribe_closing(move |_| {
            if !closed.swap(true, Ordering::AcqRel) {
                run(format!("hud:{id}"));
            }
        })?);
        #[cfg(windows)]
        {
            window.show()?;
            native::prepare(scope, &window, false);
        }
        #[cfg(target_os = "macos")]
        let positioned = window.clone();
        #[cfg(target_os = "macos")]
        let position_scope = scope.clone();
        #[cfg(target_os = "macos")]
        show_protected(
            scope,
            &window,
            false,
            move || {
                placement_ready.store(true, Ordering::Release);
                if let Some(size) = native::size(&position_scope, &positioned)? {
                    place_beside(&position_scope, &positioned, area, size, Self::GAP, false).map_err(std::io::Error::other)?;
                }
                Ok(())
            },
            |_| {},
        )?;
        Ok(HudPanel {
            window,
            status: text,
            dismissed,
            _subscriptions: subscriptions,
        })
    }

    fn set_status(&self, status: &str) {
        let _ = self.status.set_text(status);
    }

    /// Closes the panel because the capture side is done with it.
    fn dismiss(&self) {
        self.dismissed.store(true, Ordering::Release);
        let _ = self.window.close();
    }
}

/// A capture outline: a see-through, click-through window just larger than
/// the area, drawing a thin red frame outside it. (One window rather than
/// four strips: Windows keeps top-level windows taller than a few pixels.)
struct HudBorder {
    window: Window,
}

impl HudBorder {
    /// The outline's thickness, in desktop pixels.
    const WEIGHT: i32 = 3;

    fn new(scope: &AppScope, area: Rect) -> HudResult<Self> {
        let outer = area.inflate(Self::WEIGHT);
        let (_, scale) = native::screen_of(outer);
        let frame = Border::new()?
            .border_brush(Brush::new(Color::rgb(0xE5, 0x39, 0x35), 220.0 / 255.0))?
            .border_thickness(Self::WEIGHT as f64 / scale)?;
        let window = hud_window()?
            .width(outer.width as f64 / scale)?
            .height(outer.height as f64 / scale)?
            .position((outer.x, outer.y))?
            .background(Brush::solid(Color::new(0, 0, 0, 0)))?
            .content(Some(&frame))?;
        scope.set_transparent(&window, true)?;
        scope.set_topmost(&window, true)?;
        #[cfg(windows)]
        {
            window.show()?;
            native::prepare(scope, &window, true);
            native::place(native::handle(scope, &window), outer, false);
        }
        #[cfg(target_os = "macos")]
        let positioned = window.clone();
        #[cfg(target_os = "macos")]
        let position_scope = scope.clone();
        #[cfg(target_os = "macos")]
        show_protected(
            scope,
            &window,
            true,
            move || {
                native::place_checked(native::handle(&position_scope, &positioned), outer, false)
            },
            |_| {},
        )?;
        Ok(HudBorder { window })
    }

    fn close(&self) {
        let _ = self.window.close();
    }
}

/// Starts following the main view model's signals. Called once, on the UI
/// thread, after the main window is mounted.
pub fn install(scope: &AppScope) -> Result<()> {
    let sink = crate::ui::main().sink;
    let scope = scope.clone();
    // The capture side's panels and outlines, all of them each time.
    Effect::new(move || {
        let json = sink.hud.get();
        let target = scope.clone();
        scope.post(move || apply(&target, &json))
    })?;
    Ok(())
}
