//! Jiffshot's one Preferences dialog: Capture, Editor, Library, Share and
//! Hotkeys.
//!
//! The model asks with the `prefs` signal ("serial\x1fJSON", from
//! `crate::prefs::dialog`): the tabs and their rows, in the Capture window's
//! row kinds (number, check, choice, text, folder, hotkey, note). OK sends one value
//! per row and tab as `prefs-apply:{"capture":[…],…}`. Each request builds a
//! new dialog, replacing the one showing.

use super::capture_view::parts::{self, post, Input, Subscriptions};
use super::theme_kit::{bind_window_theme, solid, with_role, ButtonRole, Surface};
use rustolonia::{
    AnyEl, AppScope, Border, Button, Control, Dock, DockPanel, Effect, FontWeight, HorizontalAlignment, NodeRef, Orientation, Result, ScrollBarVisibility,
    ScrollViewer, Signal, StackPanel, TabControl, TabItem, TextBlock, Window, WindowStartupLocation,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::cell::RefCell;

/// The dialog's JSON.
#[derive(Deserialize)]
struct Dialog {
    /// The tab to open on.
    #[serde(default)]
    tab: String,
    #[serde(default)]
    tabs: Vec<Tab>,
    #[cfg(any(windows, target_os = "macos"))]
    #[serde(default)]
    startup_initial: Option<bool>,
    #[cfg(target_os = "macos")]
    #[serde(default)]
    share_initial: Option<Value>,
}

#[derive(Deserialize)]
struct Tab {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    rows: Vec<parts::Row>,
}

/// The dialog showing, if any. UI thread only.
struct Open {
    window: Window,
    /// Its rows' and buttons' subscriptions, ended when it is replaced.
    subs: Subscriptions,
}

struct State {
    scope: AppScope,
    open: Option<Open>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

#[cfg(target_os = "macos")]
pub(super) fn showing_for_selftest() -> bool {
    with(|state| state.open.is_some()).unwrap_or(false)
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.borrow_mut().as_mut().map(f))
}

#[cfg(target_os = "macos")]
fn share_checkbox(value: std::result::Result<Option<bool>, String>) -> std::result::Result<Value, String> {
    value?.map(Value::Bool).ok_or_else(|| "a Share destination has no selected state".to_owned())
}

#[cfg(target_os = "macos")]
fn share_values(inputs: &[Input]) -> std::result::Result<Value, String> {
    if inputs.is_empty() {
        return Err("the Share preference fields are incomplete".into());
    }
    let values = inputs.iter().enumerate().map(|(index, input)| match (index, input) {
        (0, Input::Nothing) => Ok(Value::Null),
        (1.., Input::Check(control)) => share_checkbox(control.is_checked().map_err(|error| format!("could not read a Share destination: {error}"))),
        (1.., Input::Switch(control)) => share_checkbox(control.is_checked().map_err(|error| format!("could not read a Share destination: {error}"))),
        _ => Err("the Share preference fields are invalid".into()),
    }).collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(Value::Array(values))
}

/// Visual card section inside a Preferences tab.
#[derive(Debug, PartialEq, Eq)]
struct CardGroup {
    heading: &'static str,
    indices: Vec<usize>,
}

/// Concise helper subtitle explaining consequential or layout settings inline,
/// inspired by UniGetUI's grouped settings cards.
fn row_subtitle(tab_id: &str, row: &parts::Row) -> Option<&'static str> {
    let label = match row {
        parts::Row::Number { label, .. }
        | parts::Row::Check { label, .. }
        | parts::Row::Choice { label, .. }
        | parts::Row::Text { label, .. }
        | parts::Row::Folder { label, .. }
        | parts::Row::Hotkey { label, .. } => label.as_str(),
        _ => return None,
    };
    match (tab_id, label) {
        ("capture", l) if l.starts_with("Run Jiffshot") => Some("Launch in the background so global capture shortcuts stay ready"),
        ("capture", "Show the Capture Widget at the top of the screen") => Some("Keep a compact quick-capture bar docked to the top of the primary display"),
        ("capture", "Copy captures to the clipboard") | ("capture", "Copy each capture to the clipboard") => Some("Place every new screenshot on the clipboard automatically"),
        ("capture", "Open captures in the editor") => Some("Open screenshots in Jiffshot Editor right after capture"),
        ("capture", "Keep the pointer as a movable object in the editor") => Some("Capture the mouse cursor on its own layer so it can be moved or removed"),
        ("capture", "Time delay (s)") => Some("Wait before capturing so menus or hover tooltips can be opened"),
        ("capture", "Recording countdown (s)") => Some("Display a visual countdown before video recording starts"),
        ("capture", "Recording frame rate") => Some("Higher frame rates produce smoother motion and larger video files"),
        ("capture", "Recording sound") => Some("Choose which audio sources are mixed into screen recordings"),
        ("capture", "Draw the pointer into recordings") => Some("Include the mouse cursor in recorded videos"),
        ("capture", "Offer Screen Draw while recording") => Some("Allow live ink annotations on screen during recording"),
        ("capture", "Screen Draw marks fade after (s, 0 keeps them)") => Some("Automatically fade live ink marks after a set duration"),
        ("capture", "Show a tray icon") => Some("Keep Jiffshot accessible from the system notification area"),
        ("editor", "Color theme (every Jiffshot window)") => Some("Applies across the editor, Capture window, Preferences, and notifications"),
        ("editor", "Show tool names in the toolbar") => Some("Display captions below tool icons in the top toolbar"),
        ("editor", "Show the side bar (Quick Styles and properties)") => Some("Show Quick Styles and tool settings along the right side of the editor"),
        ("editor", "Show the Recent Captures tray") => Some("Dock recent library items along the bottom of the editor"),
        ("editor", "Export as") => Some("Default file format selected when exporting an image"),
        ("library", "Library folder") => Some("Directory where new screenshots and recordings are stored"),
        ("hotkeys", "Capture") => Some("Start an interactive region, window, or full-screen capture"),
        ("hotkeys", "Full screen") => Some("Capture the entire display immediately"),
        ("hotkeys", "Repeat last region") => Some("Capture the previous screen area again without re-selecting"),
        _ => None,
    }
}

/// Partitions `0..rows.len()` into logical card groups for `tab_id`.
/// Every row index appears exactly once, while `inputs` is always built in
/// strict `0..rows.len()` order beforehand.
fn group_tab_rows(tab_id: &str, rows: &[parts::Row]) -> Vec<CardGroup> {
    let len = rows.len();
    if len == 0 {
        return Vec::new();
    }
    match tab_id {
        "capture" if len >= 12 => {
            let mut startup = vec![0];
            startup.extend(12..len);
            startup.push(1);
            vec![
                CardGroup { heading: "Startup & Workspace", indices: startup },
                CardGroup { heading: "Screenshots & Clipboard", indices: (2..6).collect() },
                CardGroup { heading: "Video Recording", indices: (6..12).collect() },
            ]
        }
        "capture" => vec![CardGroup { heading: "General Capture", indices: (0..len).collect() }],
        "editor" if len >= 5 => vec![
            CardGroup { heading: "Appearance & Workspace", indices: (0..4).collect() },
            CardGroup { heading: "Export", indices: (4..len).collect() },
        ],
        "editor" => vec![CardGroup { heading: "Editor", indices: (0..len).collect() }],
        "library" => vec![CardGroup { heading: "Storage Location", indices: (0..len).collect() }],
        "share" => vec![CardGroup { heading: "External Destinations", indices: (0..len).collect() }],
        "hotkeys" if len >= 7 && matches!(rows.get(1), Some(parts::Row::Hotkey { .. })) => vec![
            CardGroup { heading: "Global Shortcuts", indices: (0..6).collect() },
            CardGroup { heading: "Capture Presets", indices: (6..len).collect() },
        ],
        "hotkeys" => vec![CardGroup { heading: "Global Shortcuts", indices: (0..len).collect() }],
        _ => vec![CardGroup { heading: "Settings", indices: (0..len).collect() }],
    }
}

fn build_card(scope: &AppScope, dark: Signal<bool>, heading: &str, controls: Vec<Control>) -> Result<StackPanel> {
    let section = StackPanel::new()?;
    section.set_spacing(6.0)?;
    section.children()?.add(
        TextBlock::el()
            .text(heading.to_owned())
            .font_size(12.5)
            .font_weight(FontWeight::DemiBold)
            .margin((2.0, 0.0, 0.0, 0.0))
            .foreground(move || solid(Surface::for_theme(dark.get()).secondary_text))
            .build(scope)?,
    )?;

    let list = StackPanel::new()?;
    let list_children = list.children()?;
    for (idx, control) in controls.into_iter().enumerate() {
        if idx > 0 {
            list_children.add(
                Border::el()
                    .height(1.0)
                    .background(move || solid(Surface::for_theme(dark.get()).divider))
                    .build(scope)?,
            )?;
        }
        list_children.add(control)?;
    }

    let card = Border::el()
        .corner_radius(8.0)
        .border_thickness(1.0)
        .background(move || solid(Surface::for_theme(dark.get()).card))
        .border_brush(move || solid(Surface::for_theme(dark.get()).border))
        .child(AnyEl::from_control(list))
        .build(scope)?;
    section.children()?.add(card)?;
    Ok(section)
}

/// Starts following the main view model's signals. Called once, on the UI
/// thread, after the main window is mounted.
pub fn install(scope: &AppScope) -> Result<()> {
    let prefs = super::main().sink.prefs;
    STATE.with(|s| *s.borrow_mut() = Some(State { scope: scope.clone(), open: None }));
    let scope = scope.clone();
    Effect::new(move || {
        if let Some(json) = parts::payload(&prefs.get()).map(str::to_owned) {
            post(&scope, "Preferences", move || show(&json));
        }
        Ok(())
    })?;
    Ok(())
}

/// Opens the dialog from its JSON, replacing the one showing.
fn show(json: &str) -> Result<()> {
    let Ok(dialog) = serde_json::from_str::<Dialog>(json) else { return Ok(()) };
    let Some((scope, previous)) = with(|s| (s.scope.clone(), s.open.take())) else { return Ok(()) };
    if let Some(previous) = previous {
        let _ = previous.window.close();
        previous.subs.clear();
        parts::pause_hotkeys(false);
    }

    let subs = Subscriptions::default();
    let window_ref = NodeRef::new();
    let dark = scope.signal(parts::dark_theme(&scope));
    let mut tabs: Vec<(String, Vec<Input>)> = Vec::new();
    let mut items = Vec::new();
    for tab in &dialog.tabs {
        let mut inputs = Vec::with_capacity(tab.rows.len());
        let mut rendered: Vec<Option<Control>> = Vec::with_capacity(tab.rows.len());
        for row in &tab.rows {
            let subtitle = row_subtitle(&tab.id, row);
            let ctrl = parts::prefs_row(&scope, dark, row, subtitle, window_ref, &mut inputs, &subs)?;
            rendered.push(Some(ctrl));
        }
        let page = StackPanel::new()?;
        page.set_spacing(16.0)?;
        page.set_margin((20.0, 14.0, 20.0, 20.0))?;
        let page_children = page.children()?;
        for group in group_tab_rows(&tab.id, &tab.rows) {
            let group_controls: Vec<Control> = group
                .indices
                .into_iter()
                .filter_map(|i| rendered.get_mut(i).and_then(Option::take))
                .collect();
            if !group_controls.is_empty() {
                page_children.add(build_card(&scope, dark, group.heading, group_controls)?)?;
            }
        }
        tabs.push((tab.id.clone(), inputs));
        items.push(
            TabItem::el()
                .class("jiffshot-prefs-tab")
                .header(tab.title.clone())
                .content(
                    ScrollViewer::el()
                        .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
                        .content(AnyEl::from_control(page)),
                ),
        );
    }
    let selected = dialog.tabs.iter().position(|t| t.id == dialog.tab).unwrap_or(0);
    #[cfg(any(windows, target_os = "macos"))]
    let startup_initial = dialog.startup_initial;
    #[cfg(target_os = "macos")]
    let share_initial = dialog.share_initial;

    let ok = parts::on_click(
        with_role(
            Button::el()
                .content("OK")
                .default(true)
                .horizontal_content_alignment(HorizontalAlignment::Center),
            dark,
            ButtonRole::Primary,
        ),
        &subs,
        move |_| {
            // The values to send, by tab.
            #[cfg(not(target_os = "macos"))]
            let values: Map<String, Value> = tabs.iter().map(|(id, inputs)| (id.clone(), parts::values(inputs))).collect();
            #[cfg(target_os = "macos")]
            let values: Map<String, Value> = {
                let mut values = Map::new();
                for (id, inputs) in &tabs {
                    let value = if id == "share" {
                        match share_values(inputs) {
                            Ok(value) => value,
                            Err(error) => {
                                crate::app::post(move |app| {
                                    app.set_status(format!("Could not apply Preferences: {error}"));
                                    app.toast("Preferences not applied", &error, true);
                                });
                                return;
                            }
                        }
                    } else {
                        parts::values(inputs)
                    };
                    values.insert(id.clone(), value);
                }
                values
            };
            #[cfg(any(windows, target_os = "macos"))]
            let values = {
                let mut values = values;
                if let Some(initial) = startup_initial {
                    values.insert("_startup_initial".into(), Value::Bool(initial));
                }
                #[cfg(target_os = "macos")]
                if let Some(initial) = &share_initial {
                    values.insert("_share_initial".into(), initial.clone());
                }
                values
            };
            let action = format!("prefs-apply:{}", Value::Object(values));
            super::main().model.run(|m| m.run(action));
            close_later();
        },
    );
    let cancel = parts::on_click(
        with_role(
            Button::el()
                .content("Cancel")
                .cancel(true)
                .horizontal_content_alignment(HorizontalAlignment::Center),
            dark,
            ButtonRole::Secondary,
        ),
        &subs,
        |_| close_later(),
    );
    let footer = Border::el()
        .dock_panel_dock(Dock::Bottom)
        .background(move || solid(Surface::for_theme(dark.get()).panel))
        .border_brush(move || solid(Surface::for_theme(dark.get()).border))
        .border_thickness((0.0, 1.0, 0.0, 0.0))
        .padding((20.0, 12.0))
        .child(
            StackPanel::el()
                .orientation(Orientation::Horizontal)
                .spacing(8.0)
                .horizontal_alignment(HorizontalAlignment::Right)
                .child(ok)
                .child(cancel),
        );
    let window = Window::el()
        .title("Jiffshot Preferences")
        .icon(super::icon_file())
        .width(700.0)
        .height(620.0)
        .min_width(560.0)
        .min_height(440.0)
        .window_startup_location(WindowStartupLocation::CenterScreen)
        .content(
            DockPanel::el()
                .child(footer)
                .child(TabControl::el().margin((0.0, 4.0, 0.0, 0.0)).items_iter(items).selected_index(i32::try_from(selected).unwrap_or(0))),
        )
        .node_ref(window_ref)
        .build(&scope)?;
    bind_window_theme(&scope, &window, dark)?;
    // A hotkey box may still have the focus.
    subs.keep(window.subscribe_closing(|_| parts::pause_hotkeys(false))?);

    // Over the editor when it is showing.
    match scope.main_window().filter(|main| main.is_visible().unwrap_or(false)) {
        Some(owner) => window.show_with_window(&owner)?,
        None => window.show()?,
    }
    if let Some(hwnd) = parts::hwnd(&scope, &window) {
        #[allow(clippy::unnecessary_cast)]
        crate::winutil::bring_forward(hwnd as isize);
    }
    with(|s| s.open = Some(Open { window, subs }));
    Ok(())
}

/// Closes the dialog once the click that asked for it has returned (its
/// subscriptions end with it).
fn close_later() {
    let Some(scope) = with(|s| s.scope.clone()) else { return };
    post(&scope, "Preferences", || {
        if let Some(open) = with(|s| s.open.take()).flatten() {
            let _ = open.window.close();
            open.subs.clear();
        }
        parts::pause_hotkeys(false);
        Ok(())
    });
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn share_input_errors_do_not_become_removals_or_empty_values() {
        assert_eq!(share_checkbox(Ok(Some(true))).unwrap(), Value::Bool(true));
        assert_eq!(share_checkbox(Ok(Some(false))).unwrap(), Value::Bool(false));
        assert!(share_checkbox(Ok(None)).is_err());
        assert_eq!(share_checkbox(Err("checkbox failed".into())).unwrap_err(), "checkbox failed");
        assert!(share_values(&[]).is_err());
        assert!(share_values(&[Input::Nothing, Input::Nothing, Input::Nothing]).is_err());
    }
}

#[cfg(test)]
mod grouping_tests {
    use super::*;

    fn check_row(label: &str) -> parts::Row {
        parts::Row::Check { label: label.to_owned(), value: false }
    }

    fn hotkey_row(label: &str) -> parts::Row {
        parts::Row::Hotkey { label: label.to_owned(), value: String::new(), placeholder: "None".into() }
    }

    fn note_row(text: &str) -> parts::Row {
        parts::Row::Note { text: text.to_owned() }
    }

    fn assert_bijection(groups: &[CardGroup], expected_len: usize) {
        let mut seen: Vec<usize> = groups.iter().flat_map(|g| g.indices.iter().copied()).collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..expected_len).collect::<Vec<_>>());
    }

    #[test]
    fn capture_tab_groups_cover_every_row_including_optional_startup_detail() {
        let mut rows: Vec<parts::Row> = (0..12).map(|i| check_row(&format!("row-{i}"))).collect();
        let groups = group_tab_rows("capture", &rows);
        assert_eq!(groups.len(), 3);
        assert_bijection(&groups, 12);

        rows.push(note_row("Startup requires approval"));
        let groups_with_detail = group_tab_rows("capture", &rows);
        assert_eq!(groups_with_detail[0].indices, vec![0, 12, 1]);
        assert_bijection(&groups_with_detail, 13);
    }

    #[test]
    fn editor_library_share_and_hotkeys_groups_preserve_all_indices() {
        let editor: Vec<parts::Row> = (0..5).map(|i| check_row(&format!("e-{i}"))).collect();
        let editor_groups = group_tab_rows("editor", &editor);
        assert_eq!(editor_groups.len(), 2);
        assert_bijection(&editor_groups, 5);

        let hotkeys = vec![
            note_row("active"),
            hotkey_row("Capture"),
            hotkey_row("Full screen"),
            hotkey_row("Repeat last region"),
            note_row("defaults"),
            note_row("recording"),
            note_row("presets"),
            hotkey_row("Preset 1"),
        ];
        let hotkey_groups = group_tab_rows("hotkeys", &hotkeys);
        assert_eq!(hotkey_groups.len(), 2);
        assert_bijection(&hotkey_groups, 8);
    }
}
