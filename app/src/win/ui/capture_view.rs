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
#[cfg(not(target_os = "linux"))]
use jiffshot_workflow::panel::CaptureTiming;
/// The timing workflow of a tab (`panel::CaptureTiming`); Linux offers manual only.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CaptureTiming {
    #[default]
    Manual,
    Interval,
    Schedule,
}
use rustolonia::{
    AppScope, AsControl, Border, Button, Color, Control, Dock, DockPanel, Effect, El, Flyout, FontWeight, Grid, HorizontalAlignment, Key, MenuFlyout,
    MenuItem, MenuItemToggleType, NodeRef, Orientation, PlacementMode, Result, ScrollViewer, Separator, Signal, StackPanel, TextAlignment, TextBlock, TextWrapping, Upcast,
    VerticalAlignment, Window, WindowStartupLocation,
};
#[cfg(not(target_os = "macos"))]
use rustolonia::Image;
#[cfg(windows)]
use rustolonia::{Brush, Path, TemplatedControl, WindowDecorations, WindowState};
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
    recording: bool,
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
    /// The title menus, for the macOS menu bar.
    #[cfg(target_os = "macos")]
    #[serde(default)]
    menus: Vec<MenuEntry>,
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

struct MenuButton {
    name: &'static str,
    button: Button,
    #[cfg(windows)]
    width: Signal<f64>,
}

/// The window and what its callbacks need. UI thread only.
struct Ui {
    scope: AppScope,
    window: NodeRef<Window>,
    dark: Signal<bool>,
    tab: Signal<usize>,
    recording: Signal<bool>,
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
    /// The in-window menu bar, or a zero-height popup anchor on macOS.
    menu_bar: NodeRef<StackPanel>,
    menu_buttons: Vec<MenuButton>,
    open_menu: Option<MenuFlyout>,
    active_menu: Signal<Option<&'static str>>,
    /// Invalidates posted payloads after a newer request or dismissal.
    menu_revision: u64,
    topmost: bool,
    #[cfg(target_os = "macos")]
    native_menu: native_menu::NativeMenus,
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
    with_ui(|ui| {
        ui.menu_revision = ui.menu_revision.wrapping_add(1);
        let top_menu = ui.menu_buttons.iter().find_map(|menu| match menu.button.same_object(&anchor) {
            Ok(true) => Some(menu.name),
            Ok(false) => None,
            Err(error) => {
                eprintln!("Capture window menu anchor: {error:?}");
                None
            }
        });
        ui.active_menu.set(top_menu);
        ui.anchor = Some(anchor);
    });
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
    let recording = scope.signal(false);
    let timing = scope.signal(CaptureTiming::Manual);
    let interval_running = scope.signal(false);
    let active_schedule = scope.signal(None::<String>);
    let action = scope.signal((String::from("Capture"), String::new(), String::new()));
    #[cfg(windows)]
    let custom_caption = scope.signal(true);
    let (window_ref, fields) = (NodeRef::new(), NodeRef::new());
    // The static controls' subscriptions last as long as the window.
    let subs = Subscriptions::default();
    let menu_bar = NodeRef::new();
    let active_menu = scope.signal(None::<&'static str>);
    let menu_buttons = Rc::new(RefCell::new(Vec::<MenuButton>::new()));

    #[cfg(not(target_os = "macos"))]
    let menu_button = |label: &str, which: &'static str| {
        let buttons = menu_buttons.clone();
        let hover_subs = subs.clone();
        let hover = scope.signal(false);
        #[cfg(windows)]
        let width = scope.signal(0.0);
        let button = flat(dark);
        #[cfg(windows)]
        let button = button.on_size_changed(move |args| width.set(args.new_size.width));
        on_click(
            button
                .name(format!("capture-menu-{which}"))
                .padding(0.0)
                .border_thickness(0.0)
                .content(
                    Border::el()
                        .padding((9.0, 4.0))
                        .corner_radius(5.0)
                        .background(move || if hover.get() || active_menu.get() == Some(which) { palette(dark).track } else { parts::TRANSPARENT })
                        .child(TextBlock::el().text(label.to_owned()).foreground(move || palette(dark).text)),
                )
                .on_pointer_entered(move |_| hover.set(true))
                .on_pointer_exited(move |_| hover.set(false)),
            &subs,
            move |button: &Button| request_top_menu(which, button, false),
        )
        .with_op(move |_, button| {
            buttons.borrow_mut().push(MenuButton {
                name: which,
                button: button.clone(),
                #[cfg(windows)]
                width,
            });
            let this = button.clone();
            hover_subs.keep(button.subscribe_pointer_entered(move |_| request_top_menu(which, &this, true))?);
            Ok(())
        })
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
                        .node_ref(menu_bar)
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
    #[cfg(not(any(windows, target_os = "macos")))]
    let title_bar = Grid::el()
        .column_definitions("Auto,Auto,*")
        .child(Image::el().source(super::icon_file()).width(20.0).height(20.0).margin((14.0, 0.0, 10.0, 0.0)).vertical_alignment(VerticalAlignment::Center))
        .child(
            StackPanel::el()
                .grid_column(1)
                .orientation(Orientation::Horizontal)
                .node_ref(menu_bar)
                .spacing(2.0)
                .vertical_alignment(VerticalAlignment::Center)
                .child(menu_button("File", "file"))
                .child(menu_button("Edit", "edit"))
                .child(menu_button("View", "view"))
                .child(menu_button("Help", "help")),
        );
    #[cfg(target_os = "macos")]
    let title_bar = StackPanel::el().height(0.0).node_ref(menu_bar);

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
            .child(glyph(MODE_GLYPHS[0], 28.0).text(move || super::icons::glyph(MODE_GLYPHS[tab.get().min(2)]).to_owned()).horizontal_alignment(HorizontalAlignment::Center).foreground(WHITE))
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
    // A fixed two-line slot keeps the capsule and button anchored across modes.
    let detail = TextBlock::el()
        .text(move || action.get().1)
        .width(144.0)
        .height(32.0)
        .max_lines(2)
        .text_trimming("CharacterEllipsis")
        .horizontal_alignment(HorizontalAlignment::Center)
        .text_alignment(TextAlignment::Center)
        .text_wrapping(TextWrapping::Wrap)
        .font_size(12.0)
        .foreground(move || palette(dark).text);
    let mode_capsule = Border::el()
        .width(168.0)
        .height(if cfg!(target_os = "linux") { 66.0 } else { 52.0 })
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
    let show_recording_controls = status_button(
        dark,
        &subs,
        "Show controls",
        "Restore the recording toolbar without restarting the recording",
        "show-recording-controls",
    );
    #[cfg(any(windows, target_os = "macos"))]
    let show_recording_controls = show_recording_controls.visible(|| {
        let json = super::main().sink.hud.get();
        serde_json::from_str::<serde_json::Value>(&json)
            .is_ok_and(|state| super::hud::has_recording_toolbar(&state))
    });
    #[cfg(target_os = "linux")]
    let show_recording_controls = show_recording_controls.visible(false);
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
                                    recording.get()
                                            || (interval_running.get() && timing.get() != CaptureTiming::Interval)
                                            || (active_schedule.get().is_some() && timing.get() != CaptureTiming::Schedule)
                                })
                                .child(
                                    StackPanel::el()
                                        .orientation(Orientation::Horizontal)
                                        .spacing(8.0)
                                        .visible(recording)
                                        .child(
                                            TextBlock::el()
                                                .text("Recording is running.")
                                                .font_size(12.0)
                                                .vertical_alignment(VerticalAlignment::Center)
                                                .foreground(move || palette(dark).muted),
                                        )
                                        .child(show_recording_controls),
                                )
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
    #[cfg(windows)]
    let loaded_scope = scope.clone();
    let window = Window::el().title(if cfg!(target_os = "macos") { TITLE } else { "" });
    #[cfg(windows)]
    let window = window.on_loaded(move |_| {
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
    });
    let window = window
        .icon(super::icon_file())
        .width(if cfg!(target_os = "linux") { 660.0 } else { 560.0 })
        .height(if cfg!(target_os = "linux") { 400.0 } else { 320.0 })
        .can_resize(false)
        .can_maximize(false);
    #[cfg(windows)]
    let window = window.window_decorations(WindowDecorations::BorderOnly);
    #[cfg(not(target_os = "macos"))]
    let window = window.extend_client_area_to_decorations_hint(true).extend_client_area_title_bar_height_hint(40.0);
    let window = window
        .window_startup_location(WindowStartupLocation::CenterScreen)
        .background(move || palette(dark).back)
        .foreground(move || palette(dark).text)
        .on_actual_theme_variant_changed(move |_| {
            dark.set_if_changed(parts::dark_theme(&theme_scope));
            post(&theme_scope, "Capture window menu theme", dismiss_menu);
        })
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
                .row_definitions(if cfg!(target_os = "macos") { "Auto,*,44" } else { "40,*,44" })
                .child(title_bar)
                .child(Grid::el().grid_row(1).margin((8.0, 0.0, 8.0, 0.0)).child(card))
                .child(bottom_bar),
        )
        .node_ref(window_ref)
        .build(scope)?;
    #[cfg(target_os = "macos")]
    let native_menu = native_menu::NativeMenus::install(scope, &window)?;

    UI.with(|ui| {
        *ui.borrow_mut() = Some(Ui {
            scope: scope.clone(),
            window: window_ref,
            dark,
            tab,
            recording,
            timing,
            interval_running,
            active_schedule,
            action,
            fields,
            _controls: subs,
            rows: Subscriptions::default(),
            popup: Subscriptions::default(),
            anchor: None,
            menu_bar,
            menu_buttons: menu_buttons.borrow_mut().drain(..).collect(),
            open_menu: None,
            active_menu,
            menu_revision: 0,
            topmost: false,
            #[cfg(target_os = "macos")]
            native_menu,
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
        #[cfg(any(windows, target_os = "linux"))]
        let json = sink.view.get();
        #[cfg(any(windows, target_os = "linux"))]
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
        let request = sink.menu.get();
        if payload(&request).is_some() {
            let revision = with_ui(|ui| ui.menu_revision);
            post(&s, "Capture window menu", move || {
                if with_ui(|ui| ui.menu_revision) == revision && sink.menu.get_untracked() == request {
                    if let Some(json) = payload(&request) {
                        show_menu(json)?;
                    }
                }
                Ok(())
            });
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
            // The Fluent template changes its foreground on hover and press.
            .content(TextBlock::el().text(label).foreground(parts::TRANSPARENT)),
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
    #[cfg(target_os = "macos")]
    with_ui(|ui| ui.native_menu.update(&view.menus)).transpose()?;
    let Some((scope, dark, fields, rows, (tab, recording, timing, interval_running, active_schedule, action))) =
        with_ui(|ui| Some((ui.scope.clone(), ui.dark, ui.fields.get()?, ui.rows.clone(), (ui.tab, ui.recording, ui.timing, ui.interval_running, ui.active_schedule, ui.action))))
            .flatten()
    else {
        return Ok(());
    };
    tab.set_if_changed(view.tab);
    recording.set_if_changed(view.recording);
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
    let command = e.key_modifiers.contains(super::COMMAND_MODIFIER);
    match e.key {
        Key::Return => run("capture".into()),
        Key::Escape => close_to_tray(),
        key @ (Key::D1 | Key::D2 | Key::D3) if command => run(format!("tab:{}", key as i32 - Key::D1 as i32)),
        _ => return,
    }
    e.handled = true;
}

/// Hides the window (it stays mounted for next time) and tells the model.
fn close_to_tray() {
    if let Err(error) = dismiss_menu() {
        eprintln!("Capture window menu: {error:?}");
    }
    if let Some(window) = with_ui(|ui| ui.window.get()).flatten() {
        let _ = window.hide();
    }
    run("closed".into());
}

// ---- menus -------------------------------------------------------------------

fn dismiss_menu() -> Result<()> {
    let open = with_ui(|ui| {
        ui.menu_revision = ui.menu_revision.wrapping_add(1);
        ui.active_menu.set(None);
        ui.open_menu.take()
    }).flatten();
    if let Some(open) = open {
        open.hide()?;
    }
    Ok(())
}

#[cfg(windows)]
pub(super) fn request_menu_for_selftest(which: &'static str, hover: bool) {
    let (scope, window, x) = with_ui(|ui| {
        let mut x = 44.0; // The title icon's width and margins.
        for menu in &ui.menu_buttons {
            let width = menu.width.get_untracked();
            assert!(width > 0.0, "the {} menu has not been laid out", menu.name);
            if menu.name == which {
                return Some((ui.scope.clone(), ui.window.get()?, x + width / 2.0));
            }
            x += width + 2.0;
        }
        None
    }).flatten().expect("the Capture menu button is mounted");
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE};
    let hwnd = scope.platform_handle(&window).expect("the Capture fixture has a native handle") as windows_sys::Win32::Foundation::HWND;
    // SAFETY: hwnd belongs to this fixture; the messages have no borrowed data
    // and deliver pointer input without changing the desktop pointer.
    unsafe {
        let scale = f64::from(GetDpiForWindow(hwnd)) / 96.0;
        let point = (x * scale).round() as u16 as isize | (((20.0 * scale).round() as u16 as isize) << 16);
        assert_ne!(PostMessageW(hwnd, WM_MOUSEMOVE, 0, point), 0, "could not post the fixture mouse move");
        if !hover {
            assert_ne!(PostMessageW(hwnd, WM_LBUTTONDOWN, 1, point), 0, "could not post the fixture mouse press");
            assert_ne!(PostMessageW(hwnd, WM_LBUTTONUP, 0, point), 0, "could not post the fixture mouse release");
        }
    }
}

#[cfg(windows)]
pub(super) fn menu_for_selftest() -> Option<(&'static str, MenuFlyout)> {
    with_ui(|ui| Some((ui.active_menu.get_untracked()?, ui.open_menu.clone()?))).flatten()
}

#[cfg(not(target_os = "macos"))]
fn request_top_menu(which: &'static str, button: &Button, hover: bool) {
    let Some(scope) = with_ui(|ui| ui.scope.clone()) else { return };
    let button = button.clone();
    post(&scope, "Capture window menu switch", move || {
        let Some((active, open)) = with_ui(|ui| (ui.active_menu.get_untracked(), ui.open_menu.clone())) else { return Ok(()) };
        if hover && (active.is_none() || active == Some(which)) {
            return Ok(());
        }
        let toggle_off = !hover && active == Some(which);
        if toggle_off {
            return dismiss_menu();
        }
        if let Some(open) = open {
            open.hide()?;
        }
        request("menu", which, button.upcast());
        Ok(())
    });
}

fn show_menu(json: &str) -> Result<()> {
    let Ok(entries) = serde_json::from_str::<Vec<MenuEntry>>(json) else { return Ok(()) };
    let Some((_, _, anchor, popup)) = popup_target() else { return Ok(()) };
    let Some((previous, menu_bar, active, dark)) = with_ui(|ui| (ui.open_menu.take(), ui.menu_bar.get(), ui.active_menu, ui.dark)) else { return Ok(()) };
    if let Some(previous) = previous {
        previous.hide()?;
    }
    popup.clear();
    let flyout = {
        let control = MenuFlyout::new()?;
        control.set_placement(PlacementMode::BottomEdgeAlignedLeft)?;
        control
    };
    if active.get_untracked().is_some() {
        flyout.set_overlay_input_pass_through_element(menu_bar.as_ref().map(|bar| -> Control { bar.clone().upcast() }))?;
    }
    let id = flyout.object_id()?;
    popup.keep(flyout.subscribe_closed(move |_| {
        with_ui(|ui| {
            match ui.open_menu.as_ref().map(MenuFlyout::object_id).transpose() {
                Ok(Some(current)) if current == id => {
                    ui.menu_revision = ui.menu_revision.wrapping_add(1);
                    ui.open_menu = None;
                    ui.active_menu.set(None);
                }
                Err(error) => eprintln!("Capture window menu dismissal: {error:?}"),
                _ => {}
            }
        });
    })?);
    let items = flyout.items()?;
    for entry in &entries {
        items.add(menu_entry(entry, &popup, dark)?)?;
    }
    flyout.show_at_with_control(&anchor)?;
    with_ui(|ui| ui.open_menu = Some(flyout.clone()));
    #[cfg(windows)]
    if dark.get_untracked() {
        let presenter = flyout.popup()?.child()?.and_then(|child| child.cast::<TemplatedControl>())
            .ok_or_else(|| rustolonia::Error::Async { hresult: 0x80004005_u32 as i32, message: "Capture menu presenter is not a templated control".into() })?;
        presenter.set_background(Brush::solid(parts::rgb(dark_blue::WINDOW)))?;
        presenter.set_border_brush(Brush::solid(parts::rgb(dark_blue::INPUT_BORDER)))?;
        presenter.set_foreground(Brush::solid(WHITE))?;
    }
    Ok(())
}

fn menu_entry(entry: &MenuEntry, subs: &Subscriptions, _dark: Signal<bool>) -> Result<Control> {
    if entry.separator {
        let separator = Separator::new()?;
        #[cfg(windows)]
        if _dark.get_untracked() {
            separator.set_background(Brush::solid(parts::rgb(dark_blue::INPUT_BORDER)))?;
        }
        return Ok(separator.upcast());
    }
    let (label, keys) = match entry.label.split_once('\t') {
        Some((label, keys)) => (label, Some(keys)),
        None => (entry.label.as_str(), None),
    };
    let item = {
        let control = MenuItem::new()?;
        control.set_enabled(entry.enabled)?;
        control.set_stays_open_on_click(entry.keep_open)?;
        control
    };
    #[cfg(windows)]
    if _dark.get_untracked() {
        item.set_background(Brush::solid(parts::rgb(dark_blue::WINDOW)))?;
        item.set_foreground(Brush::solid(WHITE))?;
    }
    match keys {
        // A shortcut or preset hotkey, as text: not every one parses as a gesture.
        Some(keys) => {
            let header = {
                let control = DockPanel::new()?;
                control.set_min_width(220.0)?;
                control.children()?.add({
                    let control = TextBlock::new()?;
                    control.set_text(keys)?;
                    control.set_opacity(0.65)?;
                    control.set_margin((24.0, 0.0, 0.0, 0.0))?;
                    control.set_dock_panel_dock(Dock::Right)?;
                    control
                })?;
                control.children()?.add({
                    let control = TextBlock::new()?;
                    control.set_text(label)?;
                    control
                })?;
                control
            };
            item.set_header(&header)?;
        }
        None => item.set_header_text(label)?,
    }
    if !entry.items.is_empty() {
        let children = item.items()?;
        for child in &entry.items {
            children.add(menu_entry(child, subs, _dark)?)?;
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
            let checked = this.is_checked().unwrap_or(false);
            let _ = this.set_checked(!checked);
        }
        pick(&id);
    })?);
    Ok(item.upcast())
}

/// Runs a menu entry's `id`, as a click on it does.
fn pick(id: &str) {
    if let Some(command) = id.strip_prefix("window:") {
        window_command(command);
    } else if !id.is_empty() {
        run(format!("pick:{id}"));
    }
}

#[cfg(target_os = "macos")]
fn pick_native(id: &str) -> Result<()> {
    dismiss_menu()?;
    with_ui(|ui| ui.anchor = ui.menu_bar.get().map(|bar| bar.upcast()))
        .ok_or_else(|| rustolonia::Error::Load("the Capture window is unavailable".into()))?;
    pick(id);
    Ok(())
}

#[cfg(target_os = "macos")]
pub(super) fn native_menu_anchor_for_selftest() -> Result<bool> {
    with_ui(|ui| match (ui.anchor.as_ref(), ui.menu_bar.get()) {
        (Some(anchor), Some(bar)) => anchor.same_object(&bar),
        _ => Ok(false),
    }).unwrap_or(Ok(false))
}

#[cfg(target_os = "macos")]
pub(super) fn native_menu_layout_for_selftest() -> Result<(usize, f64)> {
    with_ui(|ui| {
        let anchor = ui.menu_bar.get().ok_or_else(|| rustolonia::Error::Load("the Capture menu anchor is unavailable".into()))?;
        Ok((anchor.children()?.len()?, anchor.height()?))
    }).ok_or_else(|| rustolonia::Error::Load("the Capture window is unavailable".into()))?
}

/// The title menus in the macOS menu bar while the window is key.
#[cfg(target_os = "macos")]
mod native_menu {
    use super::{pick_native, MenuEntry};
    use crate::ui::main_window::menu_entry::update_native_states;
    use rustolonia::{AppScope, NativeMenu, NativeMenuItem, Result, Window};
    use std::sync::{Arc, Mutex};

    pub(super) struct NativeMenus {
        menu: NativeMenu,
        ids: Arc<Mutex<Vec<String>>>,
        shown: Vec<NativeMenuItem>,
    }

    impl NativeMenus {
        pub fn install(scope: &AppScope, window: &Window) -> Result<Self> {
            let ids = Arc::<Mutex<Vec<String>>>::default();
            let dispatch = ids.clone();
            let menu = scope.create_native_menu(move |id| {
                let picked = id.checked_sub(1).and_then(|index| dispatch.lock().expect("Capture menu ids poisoned").get(index as usize).cloned());
                let picked = picked.ok_or_else(|| rustolonia::Error::Load(format!("unknown Capture native menu command: {id}")))?;
                pick_native(&picked)
            })?;
            scope.set_window_menu(window, Some(&menu))?;
            Ok(Self { menu, ids, shown: Vec::new() })
        }

        pub fn update(&mut self, entries: &[MenuEntry]) -> Result<()> {
            let mut ids = Vec::new();
            let items = entries.iter().map(|entry| item(entry, &mut ids)).collect::<Vec<_>>();
            if items != self.shown {
                if !update_native_states(&self.menu, &self.shown, &items)? {
                    self.menu.set_items(&items)?;
                }
                self.shown = items;
            }
            *self.ids.lock().expect("Capture menu ids poisoned") = ids;
            Ok(())
        }
    }

    fn item(entry: &MenuEntry, ids: &mut Vec<String>) -> NativeMenuItem {
        if entry.separator {
            return NativeMenuItem::separator();
        }
        // NativeMenu interprets access-key markers; the labels are literal.
        let label = entry.label.replace('_', "__");
        if !entry.items.is_empty() {
            return NativeMenuItem::submenu(label, entry.items.iter().map(|child| item(child, ids)).collect());
        }
        ids.push(entry.id.clone());
        let id = ids.len() as u32;
        let item = if entry.id == "widget" || entry.checked || entry.keep_open {
            NativeMenuItem::check(id, label, entry.checked)
        } else {
            NativeMenuItem::command(id, label)
        };
        item.enabled(entry.enabled && !entry.id.is_empty())
    }
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

#[cfg(any(all(test, not(target_os = "linux")), target_os = "macos"))]
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
    dismiss_menu()?;
    let Ok(panel) = serde_json::from_str::<Panel>(json) else { return Ok(None) };
    let Some((scope, window, anchor, popup)) = popup_target() else { return Ok(None) };
    let Some(owner) = with_ui(|ui| ui.window) else { return Ok(None) };
    popup.clear();

    let body = {
        let control = StackPanel::new()?;
        control.set_spacing(8.0)?;
        control.set_margin(4.0)?;
        control.set_min_width(280.0)?;
        control
    };
    let children = body.children()?;
    children.add({
        let control = TextBlock::new()?;
        control.set_text(&panel.title)?;
        control.set_font_weight(FontWeight::DemiBold)?;
        control.set_font_size(14.0)?;
        control.set_margin((0.0, 0.0, 0.0, 4.0))?;
        control
    })?;
    #[cfg(target_os = "macos")]
    let (rows, scroll) = if is_timing_panel(&panel.id) {
        let content = {
            let control = StackPanel::new()?;
            control.set_spacing(8.0)?;
            control
        };
        let scroll = {
            let control = ScrollViewer::new()?;
            control.set_max_height(max_height.unwrap_or_else(|| delay_rows_height(&scope, &window)))?;
            control.set_vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)?;
            control.set_content(&content)?;
            control
        };
        children.add(&scroll)?;
        (content.children()?, Some(scroll))
    } else {
        (body.children()?, None)
    };
    #[cfg(any(windows, target_os = "linux"))]
    let (rows, scroll) = {
        let _ = (max_height, window);
        (body.children()?, None::<ScrollViewer>)
    };
    let mut inputs: Vec<Input> = Vec::new();
    for row in &panel.rows {
        rows.add(parts::row(&scope, Form::Panel, row, owner, &mut inputs, &popup)?)?;
    }
    let flyout = {
        let control = Flyout::new()?;
        control.set_placement(PlacementMode::BottomEdgeAlignedLeft)?;
        control.set_content(&body as &dyn AsControl)?;
        control
    };

    let mut panel_action: Option<Rc<dyn Fn()>> = None;
    if let Some(action) = &panel.action {
        let button = {
            let control = Button::new()?;
            control.set_content_text(&action.label)?;
            control.set_horizontal_alignment(HorizontalAlignment::Stretch)?;
            control
        };
        let (id, open, values) = (action.id.clone(), flyout.clone(), inputs.clone());
        popup.keep(button.subscribe_click(move |_| activate_panel_action(&id, &open, &values))?);
        let (id, open, values) = (action.id.clone(), flyout.clone(), inputs.clone());
        panel_action = Some(Rc::new(move || activate_panel_action(&id, &open, &values)));
        children.add(&button)?;
    }

    let buttons = {
        let control = StackPanel::new()?;
        control.set_orientation(Orientation::Horizontal)?;
        control.set_spacing(8.0)?;
        control.set_horizontal_alignment(HorizontalAlignment::Right)?;
        control.set_margin((0.0, 8.0, 0.0, 0.0))?;
        control
    };
    if let Some(ok) = &panel.ok {
        let apply = {
            let control = Button::new()?;
            control.set_content_text(ok)?;
            control.set_default(true)?;
            control.set_min_width(72.0)?;
            control.set_horizontal_content_alignment(HorizontalAlignment::Center)?;
            control
        };
        let (id, open) = (panel.id.clone(), flyout.clone());
        popup.keep(apply.subscribe_click(move |_| {
            let _ = open.hide();
            run(format!("apply:{id}:{}", parts::values(&inputs)));
        })?);
        buttons.children()?.add(&apply)?;
    }
    let close = {
        let control = Button::new()?;
        control.set_content_text(if panel.ok.is_some() { "Cancel" } else { "Close" })?;
        control.set_min_width(72.0)?;
        control.set_horizontal_content_alignment(HorizontalAlignment::Center)?;
        control
    };
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
    dismiss_menu()?;
    let Some((_, _, anchor, _)) = popup_target() else { return Ok(()) };
    let note = {
        let control = TextBlock::new()?;
        control.set_text(text)?;
        control.set_text_wrapping(TextWrapping::Wrap)?;
        control.set_max_width(320.0)?;
        control.set_margin(4.0)?;
        control
    };
    {
        let control = Flyout::new()?;
        control.set_placement(PlacementMode::BottomEdgeAlignedLeft)?;
        control.set_content(&note as &dyn AsControl)?;
        control
    }.show_at_with_control(&anchor)
}

#[cfg(all(test, not(target_os = "linux")))]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn caption_buttons_keep_labels_hidden_on_hover() {
        use rustolonia::{App, PixelPoint, ThemeVariant};
        use std::{
            sync::mpsc,
            time::{Duration, Instant},
        };
        use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
        use windows_sys::Win32::UI::WindowsAndMessaging::{SendMessageW, WM_MOUSEMOVE};

        struct Observation {
            dark: bool,
            label: &'static str,
            pointer_over: bool,
            text: Option<String>,
            tooltip: Option<String>,
            foreground: Option<rustolonia::Paint>,
            overridden_foreground: Option<rustolonia::Paint>,
            background: Option<rustolonia::Paint>,
        }

        let (sender, receiver) = mpsc::channel();
        App::load_from_env()
            .expect("load the Rustolonia host")
            .without_startup_arguments()
            .run(move |scope| {
                let subs = Subscriptions::default();
                let dark = scope.signal(true);
                let visible = scope.signal(true);
                let grid = Grid::el()
                    .column_definitions("*,*")
                    .child(caption_button(scope, dark, visible, &subs, 0, "Minimize", false))
                    .child(caption_button(scope, dark, visible, &subs, 1, "Close", true))
                    .build(scope)?;
                let mut buttons = Vec::new();
                for (index, label) in ["Minimize", "Close"].into_iter().enumerate() {
                    let wrapper = grid.children()?.get(index)?.cast::<Grid>().expect("caption wrapper");
                    let background = wrapper.children()?.get(0)?.cast::<Border>().expect("caption background");
                    let button = wrapper.children()?.get(2)?.cast::<Button>().expect("caption button");
                    let text = button.content()?.and_then(|content| content.cast::<TextBlock>()).expect("caption label");
                    buttons.push((label, button, text, background));
                }
                let window = Window::el()
                    .title("Jiffshot caption hover test")
                    .window_decorations(WindowDecorations::None)
                    .window_startup_location(WindowStartupLocation::Manual)
                    // Keep the fixture off-screen and never activate it.
                    .position(PixelPoint::new(-30_000, -30_000))
                    .show_activated(false)
                    .width(2.0 * caption::BUTTON_WIDTH)
                    .height(caption::HEIGHT)
                    .content(rustolonia::AnyEl::from_control(grid))
                    .build(scope)?;
                scope.mount(window.clone())?;
                let hwnd = scope.platform_handle(&window)?;
                let app = scope.clone();
                scope.spawn_local(async move {
                    // SAFETY: hwnd belongs to the mounted fixture window on this UI thread.
                    let dpi = unsafe { GetDpiForWindow(hwnd as windows_sys::Win32::Foundation::HWND) };
                    let result = if dpi == 0 {
                        Err(format!("get DPI for the caption fixture: {}", std::io::Error::last_os_error()))
                    } else {
                        let scale = f64::from(dpi) / 96.0;
                        let observations: Result<Vec<Observation>> = async {
                            let mut observed = Vec::new();
                            for (theme, is_dark) in [(ThemeVariant::Dark, true), (ThemeVariant::Light, false)] {
                                app.set_requested_theme_variant(theme)?;
                                dark.set(is_dark);
                                for (index, (label, button, text, background)) in buttons.iter().enumerate() {
                                    let x = (caption::BUTTON_WIDTH * (index as f64 + 0.5) * scale) as isize;
                                    let y = (caption::HEIGHT / 2.0 * scale) as isize;
                                    let deadline = Instant::now() + Duration::from_secs(5);
                                    // The first hit test may precede the window's initial layout.
                                    let pointer_over = loop {
                                        // SAFETY: this pointer-free message targets only our fixture's live window.
                                        unsafe { SendMessageW(hwnd as windows_sys::Win32::Foundation::HWND, WM_MOUSEMOVE, 0, x | (y << 16)) };
                                        let pointer_over = button.classes()?.contains(":pointerover")?;
                                        if pointer_over || Instant::now() >= deadline {
                                            break pointer_over;
                                        }
                                        app.delay(Duration::from_millis(25))?.await?;
                                    };
                                    let foreground = text.foreground()?;
                                    button.set_foreground(Brush::solid(if is_dark { WHITE } else { parts::rgb(0x000000) }))?;
                                    observed.push(Observation {
                                        dark: is_dark,
                                        label,
                                        pointer_over,
                                        text: text.text()?,
                                        tooltip: button.tool_tip_tip()?,
                                        foreground,
                                        overridden_foreground: text.foreground()?,
                                        background: background.background()?,
                                    });
                                    if !pointer_over {
                                        return Ok(observed);
                                    }
                                }
                            }
                            Ok(observed)
                        }.await;
                        observations.map_err(|error| error.to_string())
                    };
                    sender.send(result).expect("send caption observations");
                    app.shutdown().expect("shut down the caption test");
                })?;
                Ok(())
            })
            .expect("run the caption hover test");
        let observations = receiver.recv().expect("receive caption observations").expect("inspect caption controls");
        let count = observations.len();
        for observed in observations {
            let theme = if observed.dark { "dark" } else { "light" };
            let label = observed.label;
            let background = if label == "Close" { parts::rgb(0xC42B1C) } else if observed.dark { parts::rgb(dark_blue::HOVER) } else { parts::rgb(0xE2E2E2) };
            assert!(observed.pointer_over, "{theme} {label}: hover did not reach the button");
            assert_eq!(observed.text.as_deref(), Some(label), "{theme} {label}: accessible text changed");
            assert_eq!(observed.tooltip.as_deref(), Some(label), "{theme} {label}: tooltip changed");
            let color = |paint| match paint {
                rustolonia::Paint::Solid(brush) => brush.color,
                _ => panic!("{theme} {label}: expected a solid brush"),
            };
            assert_eq!(observed.foreground.map(|paint| color(paint).a), Some(0), "{theme} {label}: hovered text became visible");
            assert_eq!(observed.overridden_foreground.map(|paint| color(paint).a), Some(0), "{theme} {label}: parent foreground exposed the text");
            assert_eq!(observed.background.map(color), Some(background), "{theme} {label}: hover background changed");
        }
        assert_eq!(count, 4, "both buttons must be observed in both themes");
    }

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
