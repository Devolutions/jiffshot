//! The Capture window: image and video settings, compact share/options
//! actions, the Capture button and its details, and the window menus.
//!
//! Rust decides what it shows (`crate::capture_window`, in
//! `jiffshot_workflow::panel`'s JSON): the `view` signal lays out the tab,
//! and `menu`, `panel` and `problem` ("serial\x1fJSON") open a menu, an
//! option panel or a note under the control that asked for it. This module
//! only lays them out and sends back actions.

#[cfg(windows)]
mod caption;
pub(super) mod parts;

use self::parts::{on_click, payload, post, Form, Input, Subscriptions, GLYPHS};
use super::dark_blue;
use crate::vm::CaptureBinding;
#[cfg(target_os = "macos")]
use crate::vm::CaptureViewModelSink;
use jiffshot_workflow::panel::CaptureTiming;
use rustolonia::{
    AppScope, AsControl, Border, Button, Color, Control, Dock, DockPanel, Effect, El, Flyout, FontWeight, Grid, HorizontalAlignment, Image, Key, MenuFlyout,
    MenuItem, MenuItemToggleType, NodeRef, Orientation, PlacementMode, Result, ScrollViewer, Separator, Signal, StackPanel, TextAlignment, TextBlock, TextWrapping, Upcast,
    VerticalAlignment, Window, WindowStartupLocation,
};
#[cfg(windows)]
use rustolonia::{Brush, Path, WindowDecorations, WindowState};
#[cfg(target_os = "macos")]
use rustolonia::ScrollBarVisibility;
use serde::Deserialize;
use std::cell::RefCell;
use std::rc::Rc;

/// The window's colors.
struct Palette {
    back: Color,
    card: Color,
    track: Color,
    edge: Color,
    frame: Color,
    text: Color,
    muted: Color,
    accent: Color,
}

const LIGHT: Palette = Palette {
    back: parts::rgb(0xF3F3F3),
    card: parts::rgb(0xFFFFFF),
    track: parts::rgb(0xE0E0E0),
    edge: parts::rgb(0xE0E0E0),
    frame: parts::rgb(0xC8C8C8),
    text: parts::rgb(0x1F1F1F),
    muted: parts::rgb(0x606060),
    accent: parts::rgb(0x1971C7),
};

const DARK: Palette = Palette {
    back: parts::rgb(dark_blue::windows_or(dark_blue::WINDOW, 0x1F2023)),
    card: parts::rgb(dark_blue::windows_or(dark_blue::PANEL, 0x2B2D31)),
    track: parts::rgb(dark_blue::windows_or(dark_blue::HOVER, 0x3A3C40)),
    edge: parts::rgb(dark_blue::windows_or(dark_blue::BORDER, 0x3A3C40)),
    frame: parts::rgb(dark_blue::windows_or(dark_blue::STRONG_BORDER, 0x4A4D52)),
    text: parts::rgb(dark_blue::windows_or(dark_blue::TEXT, 0xEDEEF0)),
    muted: parts::rgb(dark_blue::windows_or(dark_blue::SECONDARY_TEXT, 0xA9ADB4)),
    accent: parts::rgb(dark_blue::windows_or(dark_blue::ACCENT, 0x2F7FD6)),
};

const TITLE: &str = "Jiffshot Capture";

const MODES: [(&str, &str); 3] = [
    ("Select", "Select an area, then choose what to create (Ctrl+1)"),
    ("Image", "Capture an image (Ctrl+2)"),
    ("Video", "Record video (Ctrl+3)"),
];

const MODE_GLYPHS: [&str; 3] = ["\u{E8B3}", "\u{E722}", "\u{E714}"];

const WHITE: Color = parts::rgb(0xFFFFFF);
const SELECTED_MODE_DARK: Color = parts::rgb(dark_blue::windows_or(dark_blue::SELECTION, 0x284662));
const SELECTED_MODE_ICON_DARK: Color = parts::rgb(dark_blue::windows_or(dark_blue::ACCENT_TEXT, 0x73BAFF));
const CAPTURE_RED: Color = parts::rgb(0xD13438);
const CAPTURE_RED_HOVER: Color = parts::rgb(0xB82B2F);
const CAPTURE_RING: Color = parts::rgb(0xF5B0A8);

/// The palette for the application's current theme.
fn palette(dark: Signal<bool>) -> &'static Palette {
    if dark.get() { &DARK } else { &LIGHT }
}

// ---- the JSON (jiffshot_workflow::panel) -----------------------------------

/// What the window shows for the current tab (`panel::View`).
#[derive(Deserialize)]
struct View {
    tab: usize,
    #[serde(default)]
    timing: CaptureTiming,
    #[serde(default)]
    interval_running: bool,
    #[serde(default)]
    active_schedule: Option<String>,
    #[serde(default)]
    fields: Vec<FieldRow>,
    #[serde(default)]
    capture: Action,
    #[serde(default)]
    toggles: Vec<Toggle>,
}

/// An icon toggle (`panel::Toggle`).
#[derive(Deserialize)]
struct Toggle {
    id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    on: bool,
    #[serde(default = "enabled")]
    enabled: bool,
}

fn enabled() -> bool {
    true
}

/// The Capture button (`panel::Action`).
#[derive(Deserialize, Default)]
struct Action {
    #[serde(default)]
    verb: String,
    #[serde(default)]
    detail: String,
    #[serde(default)]
    hint: String,
}

#[derive(Deserialize)]
struct FieldRow {
    id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    text: String,
    /// The gear button's panel.
    gear: Option<String>,
}

/// One menu entry (`panel::MenuItem`).
#[derive(Deserialize)]
struct MenuEntry {
    #[serde(default)]
    id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    checked: bool,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    keep_open: bool,
    #[serde(default)]
    separator: bool,
    #[serde(default)]
    items: Vec<MenuEntry>,
}

/// An option panel (`panel::Panel`).
#[derive(Deserialize)]
struct Panel {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    rows: Vec<parts::Row>,
    /// The confirm button's text; none for a read-only panel.
    ok: Option<String>,
    #[serde(default)]
    action: Option<PanelAction>,
}

#[derive(Deserialize)]
struct PanelAction {
    id: String,
    label: String,
}

// ---- state ------------------------------------------------------------------

/// The window and what its callbacks need. UI thread only.
struct Ui {
    scope: AppScope,
    window: NodeRef<Window>,
    dark: Signal<bool>,
    tab: Signal<usize>,
    timing: Signal<CaptureTiming>,
    interval_running: Signal<bool>,
    active_schedule: Signal<Option<String>>,
    action: Signal<(String, String, String)>,
    fields: NodeRef<StackPanel>,
    /// The static controls' subscriptions, for the window's life.
    _controls: Subscriptions,
    /// The rows' subscriptions, ended when the rows are rebuilt.
    rows: Subscriptions,
    /// The last menu's or panel's.
    popup: Subscriptions,
    /// The control the next menu, panel or note opens under.
    anchor: Option<Control>,
    topmost: bool,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn with_ui<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    UI.with(|ui| ui.borrow_mut().as_mut().map(f))
}

/// Sends `action` to the model, as the window's controls do.
fn run(action: String) {
    super::capture().model.run(|m| m.run(action));
}

/// Asks the model for a menu or panel (`verb` is "menu" or "panel"), to open
/// under `anchor`.
fn request(verb: &str, which: &str, anchor: Control) {
    with_ui(|ui| ui.anchor = Some(anchor));
    run(format!("{verb}:{which}"));
}

/// The window, and the control the next popup opens under.
fn popup_target() -> Option<(AppScope, Window, Control, Subscriptions)> {
    with_ui(|ui| {
        let window = ui.window.get()?;
        let anchor = ui.anchor.clone().unwrap_or_else(|| window.upcast());
        Some((ui.scope.clone(), window, anchor, ui.popup.clone()))
    })
    .flatten()
}

// ---- building ----------------------------------------------------------------

/// Builds the Capture window for `binding` (not yet shown).
pub fn build(scope: &AppScope, binding: &CaptureBinding) -> Result<Window> {
    let dark = scope.signal(parts::dark_theme(scope));
    let tab = scope.signal(0_usize);
    let timing = scope.signal(CaptureTiming::Manual);
    let interval_running = scope.signal(false);
    let active_schedule = scope.signal(None::<String>);
    let action = scope.signal((String::from("Capture"), String::new(), String::new()));
    #[cfg(windows)]
    let custom_caption = scope.signal(true);
    let (window_ref, fields) = (NodeRef::new(), NodeRef::new());
    // The static controls' subscriptions last as long as the window.
    let subs = Subscriptions::default();

    let menu_button = |label: &str, which: &'static str| {
        on_click(flat(dark).content(label.to_owned()), &subs, move |button: &Button| request("menu", which, button.upcast()))
    };
    #[cfg(windows)]
    let title_bar = Grid::el()
        .column_definitions(format!("{},*,{},{}", caption::MENU_WIDTH, caption::BUTTON_WIDTH, caption::BUTTON_WIDTH))
        .background(move || if dark.get() { parts::rgb(dark_blue::CHROME) } else { palette(dark).back })
        .child(
            Grid::el()
                .column_definitions("Auto,Auto")
                .child(Image::el().source(super::icon_file()).width(20.0).height(20.0).margin((14.0, 0.0, 10.0, 0.0)).vertical_alignment(VerticalAlignment::Center))
                .child(
                    StackPanel::el()
                        .grid_column(1)
                        .orientation(Orientation::Horizontal)
                        .spacing(2.0)
                        .vertical_alignment(VerticalAlignment::Center)
                        .child(menu_button("File", "file"))
                        .child(menu_button("Edit", "edit"))
                        .child(menu_button("View", "view"))
                        .child(menu_button("Help", "help")),
                ),
        )
        .child(caption_button(scope, dark, custom_caption, &subs, 2, "Minimize", false))
        .child(caption_button(scope, dark, custom_caption, &subs, 3, "Close", true));
    #[cfg(not(windows))]
    let title_bar = Grid::el()
        .column_definitions("Auto,Auto,*")
        .child(Image::el().source(super::icon_file()).width(20.0).height(20.0).margin((14.0, 0.0, 10.0, 0.0)).vertical_alignment(VerticalAlignment::Center))
        .child(
            StackPanel::el()
                .grid_column(1)
                .orientation(Orientation::Horizontal)
                .spacing(2.0)
                .vertical_alignment(VerticalAlignment::Center)
                .child(menu_button("File", "file"))
                .child(menu_button("Edit", "edit"))
                .child(menu_button("View", "view"))
                .child(menu_button("Help", "help")),
        );

    let capture_button = {
        let hover = scope.signal(false);
        let verb = TextBlock::el()
            .text(move || match (tab.get(), action.get().0.as_str()) {
                (1, "Start") | (0, "Start") => "Start images".to_owned(),
                (1, "Stop") | (0, "Stop") => "Stop images".to_owned(),
                (_, verb) => verb.to_owned(),
            })
            .text_alignment(TextAlignment::Center)
            .horizontal_alignment(HorizontalAlignment::Center)
            .text_wrapping(TextWrapping::Wrap)
            .width(92.0)
            .font_size(17.0)
            .font_weight(FontWeight::DemiBold)
            .foreground(WHITE);
        let content = StackPanel::el()
            .spacing(5.0)
            .horizontal_alignment(HorizontalAlignment::Center)
            .child(glyph(MODE_GLYPHS[0], 28.0).text(move || MODE_GLYPHS[tab.get().min(2)].to_owned()).horizontal_alignment(HorizontalAlignment::Center).foreground(WHITE))
            .child(verb);
        let button = Button::el()
            .width(112.0)
            .height(112.0)
            .corner_radius(56.0)
            .padding(0.0)
            .border_thickness(3.0)
            .border_brush(CAPTURE_RING)
            .horizontal_alignment(HorizontalAlignment::Center)
            .horizontal_content_alignment(HorizontalAlignment::Center)
            .vertical_content_alignment(VerticalAlignment::Center)
            .background(move || if hover.get() { CAPTURE_RED_HOVER } else { CAPTURE_RED })
            .on_pointer_entered(move |_| hover.set(true))
            .on_pointer_exited(move |_| hover.set(false))
            .tool_tip_tip(move || {
                let (verb, detail, _) = action.get();
                if detail.is_empty() { verb } else { format!("{verb}: {}", detail.replace('\n', " · ")) }
            })
            .content(content);
        on_click(button, &subs, |_| run("capture".into()))
    };
    let detail = TextBlock::el()
        .visible(move || !action.get().1.is_empty())
        .text(move || action.get().1)
        .width(144.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .text_alignment(TextAlignment::Center)
        .text_wrapping(TextWrapping::Wrap)
        .font_size(12.0)
        .foreground(move || palette(dark).text);
    let mode_capsule = Border::el()
        .width(168.0)
        .height(52.0)
        .padding(3.0)
        .corner_radius(12.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .background(move || palette(dark).track)
        .child(
            Grid::el()
                .column_definitions("*,*,*")
                .children_iter(MODES.iter().enumerate().map(|(i, &(label, tip))| mode_button(dark, tab, &subs, i, label, tip))),
        );
    let mode_cluster = StackPanel::el()
        .width(172.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .child(mode_capsule)
        .child(capture_button.margin((0.0, 12.0, 0.0, 0.0)))
        .child(detail.margin((0.0, 6.0, 0.0, 0.0)));
    let stop_interval = status_button(dark, &subs, "Stop", "Stop interval capture", "stop-interval");
    let cancel_schedule = status_button(dark, &subs, "Cancel", "Cancel daily scheduled capture", "cancel-schedule");
    let card = Border::el()
        .background(move || palette(dark).card)
        .border_brush(move || palette(dark).edge)
        .border_thickness(1.0)
        .corner_radius(8.0)
        .child(
            Grid::el()
                .column_definitions("*,176")
                .child(
                    StackPanel::el()
                        .spacing(8.0)
                        .margin((14.0, 14.0, 0.0, 0.0))
                        .child(StackPanel::el().spacing(8.0).node_ref(fields))
                        .child(
                            StackPanel::el()
                                .spacing(2.0)
                                .visible(move || {
                                    (interval_running.get() && timing.get() != CaptureTiming::Interval)
                                            || (active_schedule.get().is_some() && timing.get() != CaptureTiming::Schedule)
                                })
                                .child(
                                    StackPanel::el()
                                        .orientation(Orientation::Horizontal)
                                        .spacing(8.0)
                                        .visible(move || interval_running.get() && timing.get() != CaptureTiming::Interval)
                                        .child(
                                            TextBlock::el()
                                                .text("Interval capture is running.")
                                                .font_size(12.0)
                                                .vertical_alignment(VerticalAlignment::Center)
                                                .foreground(move || palette(dark).muted),
                                        )
                                        .child(stop_interval),
                                )
                                .child(
                                    StackPanel::el()
                                        .orientation(Orientation::Horizontal)
                                        .spacing(8.0)
                                        .visible(move || active_schedule.get().is_some() && timing.get() != CaptureTiming::Schedule)
                                        .child(
                                            TextBlock::el()
                                                .text(move || active_schedule.get().map_or_else(String::new, |at| format!("Daily capture at {at}.")))
                                                .font_size(12.0)
                                                .vertical_alignment(VerticalAlignment::Center)
                                                .foreground(move || palette(dark).muted),
                                        )
                                        .child(cancel_schedule),
                                ),
                        ),
                )
                .child(
                    Border::el()
                        .grid_column(1)
                        .vertical_alignment(VerticalAlignment::Center)
                        .horizontal_alignment(HorizontalAlignment::Center)
                        .child(mode_cluster),
                ),
        );

    let editor = on_click(
        flat(dark).dock_panel_dock(Dock::Right).vertical_alignment(VerticalAlignment::Center).content(
            StackPanel::el()
                .orientation(Orientation::Horizontal)
                .spacing(8.0)
                .child(glyph("\u{E8A7}", 14.0).foreground(move || palette(dark).muted))
                .child(TextBlock::el().text("Editor").vertical_alignment(VerticalAlignment::Center)),
        ),
        &subs,
        |_| run("editor".into()),
    );
    let presets = on_click(
        flat(dark).tool_tip_tip("Run, edit, import or export presets").content(
            StackPanel::el()
                .orientation(Orientation::Horizontal)
                .spacing(8.0)
                .child(glyph("\u{E713}", 15.0))
                .child(TextBlock::el().text("Presets").vertical_alignment(VerticalAlignment::Center)),
        ),
        &subs,
        |button: &Button| request("menu", "presets", button.upcast()),
    );
    let new_preset = on_click(
        flat(dark)
            .tool_tip_tip("New preset from these settings")
            .content(StackPanel::el().orientation(Orientation::Horizontal).spacing(4.0).child(glyph("\u{E710}", 14.0)).child(glyph("\u{E70D}", 9.0))),
        &subs,
        |button: &Button| request("menu", "new-preset", button.upcast()),
    );
    let bottom_bar = DockPanel::el()
        .grid_row(2)
        .margin((8.0, 0.0, 8.0, 0.0))
        .child(editor)
        .child(StackPanel::el().orientation(Orientation::Horizontal).spacing(2.0).vertical_alignment(VerticalAlignment::Center).child(presets).child(new_preset));

    let theme_scope = scope.clone();
    let closing_scope = scope.clone();
    let loaded_scope = scope.clone();
    let window = Window::el()
        // The drawn title bar holds the icon and the menus, so the title is
        // only the native window's (the taskbar's and Alt+Tab's).
        .title("")
        .on_loaded(move |_| {
            #[cfg(windows)]
            {
                if !custom_caption.get() {
                    return;
                }
                let Some(window) = window_ref.get() else {
                    eprintln!("Capture window title bar: window unavailable");
                    return;
                };
                let result = if let Some(hwnd) = parts::hwnd(&loaded_scope, &window) {
                    parts::set_native_title(hwnd, TITLE);
                    caption::install(hwnd as windows_sys::Win32::Foundation::HWND)
                } else {
                    Err(std::io::Error::other("native window handle unavailable"))
                };
                if let Err(error) = result {
                    eprintln!("Capture window title bar: {error}");
                    custom_caption.set(false);
                    let fallback = window
                        .set_extend_client_area_to_decorations_hint(false)
                        .and_then(|_| window.set_window_decorations(WindowDecorations::Full))
                        .and_then(|_| window.set_title(TITLE))
                        .and_then(|_| window.set_height(350.0));
                    if let Err(error) = fallback {
                        eprintln!("Capture window native title bar: {error:?}");
                    }
                }
            }
            #[cfg(not(windows))]
            if let Some(hwnd) = window_ref.get().and_then(|window| parts::hwnd(&loaded_scope, &window)) {
                parts::set_native_title(hwnd, TITLE);
            }
        })
        .icon(super::icon_file())
        .width(560.0)
        .height(320.0)
        .can_resize(false)
        .can_maximize(false);
    #[cfg(windows)]
    let window = window.window_decorations(WindowDecorations::BorderOnly);
    let window = window
        .window_startup_location(WindowStartupLocation::CenterScreen)
        .extend_client_area_to_decorations_hint(true)
        .extend_client_area_title_bar_height_hint(40.0)
        .background(move || palette(dark).back)
        .foreground(move || palette(dark).text)
        .on_actual_theme_variant_changed(move |_| dark.set_if_changed(parts::dark_theme(&theme_scope)))
        .on_key_down(on_key)
        .on_closing(move |e| {
            // Closing keeps it for next time; only Jiffshot's Exit ends it.
            if parts::closing_for_good(e.close_reason) || std::env::var_os("JIFFSHOT_UITEST").is_some() {
                return;
            }
            e.cancel = true;
            post(&closing_scope, "Capture window", || {
                close_to_tray();
                Ok(())
            });
        })
        .content(
            Grid::el()
                .row_definitions("40,*,44")
                .child(title_bar)
                .child(Grid::el().grid_row(1).margin((8.0, 0.0, 8.0, 0.0)).child(card))
                .child(bottom_bar),
        )
        .node_ref(window_ref)
        .build(scope)?;

    UI.with(|ui| {
        *ui.borrow_mut() = Some(Ui {
            scope: scope.clone(),
            window: window_ref,
            dark,
            tab,
            timing,
            interval_running,
            active_schedule,
            action,
            fields,
            _controls: subs,
            rows: Subscriptions::default(),
            popup: Subscriptions::default(),
            anchor: None,
            topmost: false,
        })
    });
    follow(scope, binding)?;
    Ok(window)
}

/// Follows the model's signals. Each is handled after
/// the action that changed it has returned: rebuilding the rows from inside
/// a row's own click would end the subscription that is running.
fn follow(scope: &AppScope, binding: &CaptureBinding) -> Result<()> {
    let sink = binding.sink;
    let s = scope.clone();
    Effect::new(move || {
        #[cfg(windows)]
        let json = sink.view.get();
        #[cfg(windows)]
        post(&s, "Capture window view", move || apply_view(&json));
        #[cfg(target_os = "macos")]
        {
            let _ = sink.view.get();
            post(&s, "Capture window view", latest_view_update(sink));
        }
        Ok(())
    })?;
    let s = scope.clone();
    Effect::new(move || {
        if let Some(json) = payload(&sink.menu.get()).map(str::to_owned) {
            post(&s, "Capture window menu", move || show_menu(&json));
        }
        Ok(())
    })?;
    let s = scope.clone();
    Effect::new(move || {
        if let Some(json) = payload(&sink.panel.get()).map(str::to_owned) {
            post(&s, "Capture window panel", move || show_panel(&json));
        }
        Ok(())
    })?;
    let s = scope.clone();
    Effect::new(move || {
        if let Some(text) = payload(&sink.problem.get()).map(str::to_owned) {
            post(&s, "Capture window note", move || show_problem(&text));
        }
        Ok(())
    })?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn latest_view_update(sink: CaptureViewModelSink) -> impl FnOnce() -> Result<()> + Send + 'static {
    move || apply_view(&sink.view.get_untracked())
}

#[cfg(target_os = "macos")]
pub(super) fn delayed_view_for_selftest() -> impl FnOnce() -> Result<()> + Send + 'static {
    latest_view_update(super::capture().sink)
}

#[cfg(target_os = "macos")]
pub(super) fn view_tab_for_selftest() -> Option<usize> {
    with_ui(|ui| ui.tab.get_untracked())
}

/// A borderless button (`Button.flat`).
fn flat(dark: Signal<bool>) -> El<Button> {
    Button::el().background(parts::TRANSPARENT).padding((9.0, 4.0)).corner_radius(5.0).foreground(move || palette(dark).text)
}

#[cfg(windows)]
fn caption_button(scope: &AppScope, dark: Signal<bool>, visible: Signal<bool>, subs: &Subscriptions, column: i32, label: &'static str, close: bool) -> El<Grid> {
    let hover = scope.signal(false);
    let button = on_click(
        Button::el()
            .width(caption::BUTTON_WIDTH)
            .height(caption::HEIGHT)
            .padding(0.0)
            .corner_radius(0.0)
            .border_thickness(0.0)
            .foreground(parts::TRANSPARENT)
            .background(parts::TRANSPARENT)
            .on_pointer_entered(move |_| hover.set(true))
            .on_pointer_exited(move |_| hover.set(false))
            .tool_tip_tip(label)
            .content(label),
        subs,
        move |_| {
            if let Some(window) = with_ui(|ui| ui.window.get()).flatten() {
                let result = if close { window.close() } else { window.set_window_state(WindowState::Minimized) };
                if let Err(error) = result {
                    eprintln!("Capture window {label}: {error:?}");
                }
            }
        },
    );
    Grid::el()
        .grid_column(column)
        .visible(visible)
        .child(Border::el().background(move || {
            if !hover.get() {
                parts::TRANSPARENT
            } else if close {
                parts::rgb(0xC42B1C)
            } else if dark.get() {
                parts::rgb(dark_blue::HOVER)
            } else {
                parts::rgb(0xE2E2E2)
            }
        }))
        .child(
            Path::el()
                .data(if close { "M 0,0 L 9,9 M 9,0 L 0,9" } else { "M 0,0 L 10,0" })
                .stroke(move || Some(Brush::solid(if close && hover.get() { WHITE } else { palette(dark).text })))
                .stroke_thickness(1.0)
                .horizontal_alignment(HorizontalAlignment::Center)
                .vertical_alignment(VerticalAlignment::Center),
        )
        .child(button)
}

fn status_button(dark: Signal<bool>, subs: &Subscriptions, label: &str, tip: &str, action: &'static str) -> El<Button> {
    on_click(
        flat(dark)
            .padding((8.0, 2.0))
            .border_brush(move || palette(dark).frame)
            .border_thickness(1.0)
            .tool_tip_tip(super::shortcut(tip))
            .content(label.to_owned()),
        subs,
        move |_| run(action.into()),
    )
}

/// An icon-font glyph.
fn glyph(text: &str, size: f64) -> El<TextBlock> {
    TextBlock::el()
        .text(super::icons::glyph(text).to_owned())
        .font_family(GLYPHS)
        .font_size(size)
        .horizontal_alignment(HorizontalAlignment::Center)
        .vertical_alignment(VerticalAlignment::Center)
}

// ---- the view ----------------------------------------------------------------

fn apply_view(json: &str) -> Result<()> {
    let Ok(view) = serde_json::from_str::<View>(json) else { return Ok(()) };
    let Some((scope, dark, fields, rows, (tab, timing, interval_running, active_schedule, action))) =
        with_ui(|ui| Some((ui.scope.clone(), ui.dark, ui.fields.get()?, ui.rows.clone(), (ui.tab, ui.timing, ui.interval_running, ui.active_schedule, ui.action))))
            .flatten()
    else {
        return Ok(());
    };
    tab.set_if_changed(view.tab);
    timing.set_if_changed(view.timing);
    interval_running.set_if_changed(view.interval_running);
    active_schedule.set_if_changed(view.active_schedule);
    let Action { verb, detail, hint } = view.capture;
    action.set_if_changed((verb, detail, hint));
    rows.clear();

    let list = fields.children()?;
    list.clear()?;
    for field in &view.fields {
        list.add(field_row(dark, &rows, field).build(&scope)?)?;
    }
    if !view.toggles.is_empty() {
        list.add(toggle_row(dark, &rows, &view.toggles).build(&scope)?)?;
    }
    Ok(())
}

/// The quick switches (cursor and copy for images; microphone, system audio
/// and webcam for video) and the more-options menu.
fn toggle_row(dark: Signal<bool>, subs: &Subscriptions, toggles: &[Toggle]) -> El<Grid> {
    let chips = StackPanel::el().orientation(Orientation::Horizontal).spacing(4.0).children_iter(toggles.iter().enumerate().map(|(i, toggle)| {
        // A gap between the image switches and the video ones in Select mode.
        let gap = if i == 2 && toggles.len() == 5 { 10.0 } else { 0.0 };
        toggle_chip(dark, subs, toggle).margin((gap, 0.0, 0.0, 0.0))
    }));
    Grid::el()
        .column_definitions("*,Auto")
        .height(34.0)
        .margin((64.0, 2.0, 0.0, 0.0))
        .child(chips)
        .child(
            on_click(
                flat(dark)
                    .grid_column(1)
                    .width(34.0)
                    .height(32.0)
                    .padding(0.0)
                    .font_size(20.0)
                    .horizontal_content_alignment(HorizontalAlignment::Center)
                    .vertical_content_alignment(VerticalAlignment::Center)
                    .tool_tip_tip("More options: preview, timing, Screen Draw and recording settings")
                    .content("\u{22EF}"),
                subs,
                move |button: &Button| request("menu", "more", button.upcast()),
            ),
        )
}

fn toggle_chip(dark: Signal<bool>, subs: &Subscriptions, toggle: &Toggle) -> El<Button> {
    let icon = match toggle.id.as_str() {
        "cursor-both" | "image:switch:cursor" | "video:switch:record-cursor" => "\u{E962}",
        "image:switch:clipboard" => "\u{E8C8}",
        "video:switch:microphone" => "\u{E720}",
        "video:switch:system-audio" => "\u{E767}",
        "video:switch:webcam" => "\u{E8B8}",
        _ => "\u{E713}",
    };
    let on = toggle.on;
    let id = toggle.id.clone();
    let state = if on { "on" } else { "off" };
    on_click(
        Button::el()
            .width(34.0)
            .height(32.0)
            .padding(0.0)
            .corner_radius(6.0)
            .enabled(toggle.enabled)
            .opacity(if toggle.enabled { 1.0 } else { 0.45 })
            .horizontal_content_alignment(HorizontalAlignment::Center)
            .vertical_content_alignment(VerticalAlignment::Center)
            .background(move || if on { palette(dark).accent } else { parts::TRANSPARENT })
            .border_brush(move || if on { palette(dark).accent } else { palette(dark).frame })
            .border_thickness(1.0)
            .tool_tip_tip(format!("{}: {state}", toggle.label))
            .content(glyph(icon, 16.0).foreground(move || if on { WHITE } else { palette(dark).text })),
        subs,
        move |_| run(format!("pick:{id}")),
    )
}

/// One segment of the icon capsule mode switch.
fn mode_button(dark: Signal<bool>, tab: Signal<usize>, subs: &Subscriptions, i: usize, label: &str, tip: &str) -> El<Button> {
    let on = move || tab.get() == i;
    let selected_background = move || if dark.get() { SELECTED_MODE_DARK } else { palette(dark).accent };
    let selected_foreground = move || if dark.get() { SELECTED_MODE_ICON_DARK } else { WHITE };
    let underline = move || if dark.get() { SELECTED_MODE_ICON_DARK } else { WHITE };
    on_click(
        Button::el()
            .grid_column(i as i32)
            .padding(0.0)
            .corner_radius(9.0)
            .border_thickness(0.0)
            .horizontal_alignment(HorizontalAlignment::Stretch)
            .vertical_alignment(VerticalAlignment::Stretch)
            .horizontal_content_alignment(HorizontalAlignment::Stretch)
            .vertical_content_alignment(VerticalAlignment::Stretch)
            .background(move || if on() { selected_background() } else { parts::TRANSPARENT })
            .tool_tip_tip(super::shortcut(tip))
            .content(
                Grid::el()
                    .child(
                        Border::el()
                            .visible(on)
                            .height(2.0)
                            .corner_radius(1.0)
                            .margin((12.0, 0.0, 12.0, 1.0))
                            .vertical_alignment(VerticalAlignment::Bottom)
                            .background(underline),
                    )
                    .child(
                    StackPanel::el()
                        .spacing(2.0)
                        .horizontal_alignment(HorizontalAlignment::Center)
                        .vertical_alignment(VerticalAlignment::Center)
                        .child(
                            glyph(MODE_GLYPHS[i], 17.0)
                                .horizontal_alignment(HorizontalAlignment::Center)
                                .foreground(move || if on() { selected_foreground() } else { palette(dark).muted }),
                        )
                        .child(
                            TextBlock::el()
                                .text(label.to_owned())
                                .horizontal_alignment(HorizontalAlignment::Center)
                                .font_size(11.5)
                                .font_weight(move || if on() { FontWeight::DemiBold } else { FontWeight::Normal })
                                .foreground(move || if on() { WHITE } else { palette(dark).muted }),
                        ),
                    ),
            ),
        subs,
        move |_| run(format!("tab:{i}")),
    )
}

/// A dropdown row: label, the dropdown (a menu) and maybe a gear (a panel).
fn field_row(dark: Signal<bool>, subs: &Subscriptions, field: &FieldRow) -> El<Grid> {
    let id = field.id.clone();
    let drop = Button::el()
        .grid_column(1)
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .height(30.0)
        .background(move || palette(dark).card)
        .border_brush(move || palette(dark).frame)
        .border_thickness(1.0)
        .corner_radius(4.0)
        .padding((9.0, 0.0, 7.0, 0.0))
        .horizontal_content_alignment(HorizontalAlignment::Stretch)
        .vertical_content_alignment(VerticalAlignment::Center)
        .tool_tip_tip(field.text.clone())
        .content(
            DockPanel::el()
                .child(glyph("\u{E70D}", 10.0).dock_panel_dock(Dock::Right).margin((6.0, 0.0, 0.0, 0.0)))
                .child(TextBlock::el().text(field.text.clone()).text_trimming("CharacterEllipsis").vertical_alignment(VerticalAlignment::Center).font_size(13.0)),
        );
    let grid = Grid::el()
        .column_definitions("64,*,34")
        .height(30.0)
        .child(TextBlock::el().text(field.label.clone()).vertical_alignment(VerticalAlignment::Center).font_size(13.5))
        .child(on_click(drop, subs, move |button: &Button| request("menu", &id, button.upcast())));
    match field.gear.clone() {
        Some(panel) => grid.child(on_click(
            flat(dark).grid_column(2).padding((6.0, 4.0)).horizontal_alignment(HorizontalAlignment::Right).tool_tip_tip("Region size and position").content(glyph("\u{E713}", 14.0)),
            subs,
            move |button: &Button| request("panel", &panel, button.upcast()),
        )),
        None => grid,
    }
}

// ---- keys and closing ------------------------------------------------------

/// Enter runs the selected mode, the command key plus 1-3 switches modes, and Escape closes.
fn on_key(e: &mut rustolonia::ControlKeyDownEventArgs) {
    if e.handled {
        return;
    }
    let command = e.key_modifiers & super::COMMAND_MODIFIER != 0;
    match Key::try_from(e.key) {
        Ok(Key::Return) => run("capture".into()),
        Ok(Key::Escape) => close_to_tray(),
        Ok(key @ (Key::D1 | Key::D2 | Key::D3)) if command => run(format!("tab:{}", key as i32 - Key::D1 as i32)),
        _ => return,
    }
    e.handled = true;
}

/// Hides the window (it stays mounted for next time) and tells the model.
fn close_to_tray() {
    if let Some(window) = with_ui(|ui| ui.window.get()).flatten() {
        let _ = window.hide();
    }
    run("closed".into());
}

// ---- menus -------------------------------------------------------------------

fn show_menu(json: &str) -> Result<()> {
    let Ok(entries) = serde_json::from_str::<Vec<MenuEntry>>(json) else { return Ok(()) };
    let Some((_, _, anchor, popup)) = popup_target() else { return Ok(()) };
    popup.clear();
    let flyout = MenuFlyout::new()?.placement(PlacementMode::BottomEdgeAlignedLeft)?;
    let items = flyout.items()?;
    for entry in &entries {
        items.add(menu_entry(entry, &popup)?)?;
    }
    flyout.show_at_with_control(&anchor)
}

fn menu_entry(entry: &MenuEntry, subs: &Subscriptions) -> Result<Control> {
    if entry.separator {
        return Ok(Separator::new()?.upcast());
    }
    let (label, keys) = match entry.label.split_once('\t') {
        Some((label, keys)) => (label, Some(keys)),
        None => (entry.label.as_str(), None),
    };
    let item = MenuItem::new()?.enabled(entry.enabled)?.stays_open_on_click(entry.keep_open)?;
    match keys {
        // A shortcut or preset hotkey, as text: not every one parses as a gesture.
        Some(keys) => {
            let header = DockPanel::new()?
                .min_width(220.0)?
                .child(TextBlock::new()?.text(keys)?.opacity(0.65)?.margin((24.0, 0.0, 0.0, 0.0))?.dock_panel_dock(Dock::Right)?)?
                .child(TextBlock::new()?.text(label)?)?;
            item.set_header(Some(&header as &dyn AsControl))?;
        }
        None => item.set_header_text(label)?,
    }
    if !entry.items.is_empty() {
        let children = item.items()?;
        for child in &entry.items {
            children.add(menu_entry(child, subs)?)?;
        }
        if entry.checked {
            item.set_header_text(format!("{label}  \u{2713}"))?;
        }
        return Ok(item.upcast());
    }
    if entry.checked || entry.keep_open {
        item.set_toggle_type(MenuItemToggleType::CheckBox)?;
        item.set_checked(entry.checked)?;
    }
    let (id, keep_open, this) = (entry.id.clone(), entry.keep_open, item.clone());
    subs.keep(item.subscribe_click(move |_| {
        if keep_open {
            let checked = this.get_is_checked().unwrap_or(false);
            let _ = this.set_checked(!checked);
        }
        if let Some(command) = id.strip_prefix("window:") {
            window_command(command);
        } else if !id.is_empty() {
            run(format!("pick:{id}"));
        }
    })?);
    Ok(item.upcast())
}

/// Menu commands about this window itself.
fn window_command(command: &str) {
    let Some((scope, window, topmost)) = with_ui(|ui| {
        if command == "topmost" {
            ui.topmost = !ui.topmost;
        }
        Some((ui.scope.clone(), ui.window.get()?, ui.topmost))
    })
    .flatten() else {
        return;
    };
    let done = match command {
        "topmost" => scope.set_topmost(&window, topmost),
        "about" => super::dialogs::about(&scope, &window),
        _ => Ok(()),
    };
    if let Err(error) = done {
        eprintln!("Capture window: {error:?}");
    }
}

// ---- option panels and notes -----------------------------------------------

#[cfg(target_os = "macos")]
fn delay_rows_height(scope: &AppScope, window: &Window) -> f64 {
    let height = scope.platform_handle(window).ok()
        .and_then(crate::mac_window::window)
        .and_then(|window| window.screen())
        .map(|screen| screen.visibleFrame().size.height)
        .filter(|height| height.is_finite() && *height > 0.0);
    match height {
        Some(height) => (height - 180.0).clamp(120.0, 640.0),
        None => {
            eprintln!("Capture options: could not determine the window's screen; using a conservative panel height");
            300.0
        }
    }
}

fn show_panel(json: &str) -> Result<()> {
    show_panel_with_height(json, None).map(|_| ())
}

type ShownPanel = (ScrollViewer, Flyout, Option<Rc<dyn Fn()>>);

#[cfg(any(test, target_os = "macos"))]
fn is_timing_panel(id: &str) -> bool {
    jiffshot_workflow::panel::capture_context(id).1 == "delay"
}

fn activate_panel_action(id: &str, flyout: &Flyout, values: &[Input]) {
    if let Err(error) = flyout.hide() {
        eprintln!("Capture options: could not close the panel: {error:?}");
        return;
    }
    run(format!("{id}:{}", parts::values(values)));
}

fn show_panel_with_height(json: &str, max_height: Option<f64>) -> Result<Option<ShownPanel>> {
    let Ok(panel) = serde_json::from_str::<Panel>(json) else { return Ok(None) };
    let Some((scope, window, anchor, popup)) = popup_target() else { return Ok(None) };
    let Some(owner) = with_ui(|ui| ui.window) else { return Ok(None) };
    popup.clear();

    let body = StackPanel::new()?.spacing(8.0)?.margin(4.0)?.min_width(280.0)?;
    let children = body.children()?;
    children.add(TextBlock::new()?.text(&panel.title)?.font_weight(FontWeight::DemiBold)?.font_size(14.0)?.margin((0.0, 0.0, 0.0, 4.0))?)?;
    #[cfg(target_os = "macos")]
    let (rows, scroll) = if is_timing_panel(&panel.id) {
        let content = StackPanel::new()?.spacing(8.0)?;
        let scroll = ScrollViewer::new()?
            .max_height(max_height.unwrap_or_else(|| delay_rows_height(&scope, &window)))?
            .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)?
            .content(Some(&content))?;
        children.add(&scroll)?;
        (content.children()?, Some(scroll))
    } else {
        (body.children()?, None)
    };
    #[cfg(windows)]
    let (rows, scroll) = {
        let _ = (max_height, window);
        (body.children()?, None::<ScrollViewer>)
    };
    let mut inputs: Vec<Input> = Vec::new();
    for row in &panel.rows {
        rows.add(parts::row(&scope, Form::Panel, row, owner, &mut inputs, &popup)?)?;
    }
    let flyout = Flyout::new()?.placement(PlacementMode::BottomEdgeAlignedLeft)?.content(Some(&body as &dyn AsControl))?;

    let mut panel_action: Option<Rc<dyn Fn()>> = None;
    if let Some(action) = &panel.action {
        let button = Button::new()?.content_text(&action.label)?.horizontal_alignment(HorizontalAlignment::Stretch)?;
        let (id, open, values) = (action.id.clone(), flyout.clone(), inputs.clone());
        popup.keep(button.subscribe_click(move |_| activate_panel_action(&id, &open, &values))?);
        let (id, open, values) = (action.id.clone(), flyout.clone(), inputs.clone());
        panel_action = Some(Rc::new(move || activate_panel_action(&id, &open, &values)));
        children.add(&button)?;
    }

    let buttons = StackPanel::new()?.orientation(Orientation::Horizontal)?.spacing(8.0)?.horizontal_alignment(HorizontalAlignment::Right)?.margin((0.0, 8.0, 0.0, 0.0))?;
    if let Some(ok) = &panel.ok {
        let apply = Button::new()?.content_text(ok)?.default(true)?.min_width(72.0)?.horizontal_content_alignment(HorizontalAlignment::Center)?;
        let (id, open) = (panel.id.clone(), flyout.clone());
        popup.keep(apply.subscribe_click(move |_| {
            let _ = open.hide();
            run(format!("apply:{id}:{}", parts::values(&inputs)));
        })?);
        buttons.children()?.add(&apply)?;
    }
    let close = Button::new()?
        .content_text(if panel.ok.is_some() { "Cancel" } else { "Close" })?
        .min_width(72.0)?
        .horizontal_content_alignment(HorizontalAlignment::Center)?;
    let open = flyout.clone();
    popup.keep(close.subscribe_click(move |_| {
        let _ = open.hide();
    })?);
    buttons.children()?.add(&close)?;
    children.add(&buttons)?;
    flyout.show_at_with_control(&anchor)?;
    Ok(scroll.map(|scroll| (scroll, flyout, panel_action)))
}

#[cfg(target_os = "macos")]
pub(super) fn show_delay_for_selftest(json: &str, height: f64) -> Result<Option<ShownPanel>> {
    show_panel_with_height(json, Some(height))
}

fn show_problem(text: &str) -> Result<()> {
    let Some((_, _, anchor, _)) = popup_target() else { return Ok(()) };
    let note = TextBlock::new()?.text(text)?.text_wrapping(TextWrapping::Wrap)?.max_width(320.0)?.margin(4.0)?;
    Flyout::new()?.placement(PlacementMode::BottomEdgeAlignedLeft)?.content(Some(&note as &dyn AsControl))?.show_at_with_control(&anchor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_and_unscoped_timing_panels_are_identified() {
        assert!(is_timing_panel("delay"));
        assert!(is_timing_panel("image:delay"));
        assert!(is_timing_panel("video:delay"));
        assert!(!is_timing_panel("image:selection"));
        assert!(!is_timing_panel("image:delay:extra"));
    }

    fn contrast(foreground: Color, background: Color) -> f64 {
        fn luminance(color: Color) -> f64 {
            let channel = |value: u8| {
                let value = f64::from(value) / 255.0;
                if value <= 0.04045 { value / 12.92 } else { ((value + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
        }
        let (a, b) = (luminance(foreground), luminance(background));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn capture_colors_keep_readable_text_in_both_themes() {
        for (theme, palette) in [("light", LIGHT), ("dark", DARK)] {
            for (name, foreground, background) in [
                ("body", palette.text, palette.back),
                ("card", palette.text, palette.card),
                ("muted background", palette.muted, palette.back),
                ("muted", palette.muted, palette.card),
            ] {
                let ratio = contrast(foreground, background);
                assert!(ratio >= 4.5, "{theme} {name} contrast {ratio:.2} is below 4.5:1");
            }
            let ratio = contrast(WHITE, palette.accent);
            assert!(ratio >= 3.0, "{theme} toggle icon contrast {ratio:.2} is below 3:1");
        }
        for (name, background) in [("capture", CAPTURE_RED), ("capture hover", CAPTURE_RED_HOVER)] {
            let ratio = contrast(WHITE, background);
            assert!(ratio >= 4.5, "{name} contrast {ratio:.2} is below 4.5:1");
        }
        let ratio = contrast(SELECTED_MODE_ICON_DARK, SELECTED_MODE_DARK);
        assert!(ratio >= 4.5, "dark selected mode contrast {ratio:.2} is below 4.5:1");
    }

    #[test]
    fn workflow_view_exposes_compact_controls_and_option_menus_for_each_tab() {
        let mut settings = jiffshot_workflow::settings::Settings::default();
        let cases: &[(usize, &[&str], &[&str], &str)] = &[
            (0, &["share"], &["switch:preview", "switch:clipboard", "switch:cursor", "switch:delay", "", "panel:delay"], "Capture"),
            (1, &["selection", "effects", "share"], &["switch:preview", "switch:clipboard", "switch:cursor", "switch:delay", "", "panel:delay"], "Capture"),
            (
                2,
                &["selection", "share"],
                &["switch:webcam", "switch:microphone", "switch:system-audio", "switch:record-cursor", "switch:draw", "switch:preview"],
                "Record",
            ),
        ];
        for &(tab, expected_fields, expected_options, verb) in cases {
            settings.capture_window.tab = tab;
            let json = serde_json::to_string(&jiffshot_workflow::panel::view(&settings, "PrtScn", false)).expect("serialize workflow view");
            let view: View = serde_json::from_str(&json).expect("deserialize capture view");
            let options = jiffshot_workflow::panel::menu(&settings, "options");
            assert_eq!(view.tab, tab);
            assert_eq!(view.fields.iter().map(|field| field.id.as_str()).collect::<Vec<_>>(), expected_fields);
            assert_eq!(options.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), expected_options);
            assert_eq!(view.fields.iter().find(|field| field.id == "selection").and_then(|field| field.gear.as_deref()), (tab != 0).then_some("selection"));
            assert_eq!(view.capture.verb, verb);
            assert!(!view.capture.detail.is_empty(), "tab {tab} should describe the action");
        }
    }
}
