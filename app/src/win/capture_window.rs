//! The Capture window's model. What the window shows and offers comes from
//! `jiffshot_workflow::panel` as JSON ([`View`], menus, option panels); the
//! window sends back actions, and every change goes to the capture side,
//! which owns the settings.
//!
//! Actions: `tab:N`, `switch:ID`, `menu:NAME`, `pick:ID`, `panel:ID`,
//! `apply:ID:[values]`, `select-image-region:[values]`,
//! `select-video-region:[values]`, `capture`, `stop-interval`,
//! `cancel-schedule`, `show-recording-controls`, `editor`, `closed`.

use crate::app;
use crate::vm::{CaptureViewModel, CaptureViewModelSink};
use jiffshot_workflow::panel::{self, capture_context as context, Applied, Pick};
use jiffshot_workflow::settings::Settings;
use jiffshot_workflow::{capture, TrayAction, TrayState};
use rustolonia::{AppScope, Result};
use std::cell::RefCell;

#[cfg(target_os = "macos")]
use std::collections::VecDeque;
#[cfg(target_os = "macos")]
use jiffshot_workflow::capture::{WindowEdit, WindowEditResult, WindowEditStatus, WindowEditTicket, WindowEffect};

struct State {
    sink: Option<CaptureViewModelSink>,
    /// The settings last published by the capture side.
    settings: Settings,
    capture_key: String,
    timelapse: bool,
    window: Option<rustolonia::Window>,
    serial: u64,
    /// The options panel to open again when the window is next shown
    /// (after Select region on screen…).
    #[cfg(windows)]
    reopen_panel: Option<&'static str>,
    #[cfg(windows)]
    startup_initial: Option<bool>,
    #[cfg(target_os = "macos")]
    scope: Option<AppScope>,
    #[cfg(target_os = "macos")]
    pending: Option<PendingEdit>,
    #[cfg(target_os = "macos")]
    queued: VecDeque<QueuedAction>,
    #[cfg(target_os = "macos")]
    next_request: u64,
}

#[cfg(target_os = "macos")]
#[derive(serde::Serialize)]
struct NativeCaptureView {
    #[serde(flatten)]
    view: panel::View,
    menus: Vec<panel::MenuItem>,
}

#[cfg(target_os = "macos")]
struct PendingEdit {
    id: u64,
    ticket: WindowEditTicket,
    select_scheduled_area: bool,
    destination: EditDestination,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
enum EditDestination {
    Capture,
    Preferences,
}

#[cfg(target_os = "macos")]
enum QueuedAction {
    Action(String),
    Presets(Vec<jiffshot_workflow::settings::Preset>),
    Preferences { values: Vec<serde_json::Value>, hotkeys: Vec<serde_json::Value>, startup_initial: bool },
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State {
        sink: None,
        settings: Settings::default(),
        capture_key: jiffshot_workflow::keys::CAPTURE.2.into(),
        timelapse: false,
        window: None,
        serial: 0,
        #[cfg(windows)]
        reopen_panel: None,
        #[cfg(windows)]
        startup_initial: None,
        #[cfg(target_os = "macos")]
        scope: None,
        #[cfg(target_os = "macos")]
        pending: None,
        #[cfg(target_os = "macos")]
        queued: VecDeque::new(),
        #[cfg(target_os = "macos")]
        next_request: 0,
    });
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

/// Remembers the window mounted for this model (hidden until shown).
pub fn set_window(window: rustolonia::Window, scope: &AppScope) {
    with(|s| {
        s.window = Some(window);
        #[cfg(target_os = "macos")]
        {
            s.scope = Some(scope.clone());
        }
    });
    #[cfg(windows)]
    let _ = scope;
}

pub(crate) fn window() -> Option<rustolonia::Window> {
    with(|s| s.window.clone())
}

/// Shows the window with the capture side's current settings, in front.
pub fn show(scope: &AppScope) {
    with(|s| {
        #[cfg(target_os = "macos")]
        {
            s.scope = Some(scope.clone());
            if s.pending.is_none() {
                s.settings = jiffshot_workflow::settings_snapshot();
            }
        }
        #[cfg(windows)]
        {
            s.settings = jiffshot_workflow::settings_snapshot();
        }
    });
    publish_view();
    let Some(window) = window() else { return };
    let _ = window.show();
    if let Ok(hwnd) = scope.platform_handle(&window) {
        crate::winutil::bring_forward(hwnd);
    }
    #[cfg(windows)]
    if let Some(id) = with(|s| s.reopen_panel.take()) {
        open_panel(id);
    }
}

fn hide() {
    if let Some(window) = window() {
        if let Err(error) = crate::ui::capture_visibility::hide_window(&window) {
            problem(&format!("Could not hide the Capture window: {error}"));
        }
    }
}

/// The capture side changed something: show it.
#[cfg(windows)]
pub fn refresh(state: &TrayState) {
    let tab = with(|s| {
        s.capture_key = state.capture_key.clone();
        s.timelapse = state.time_lapse;
        s.settings.capture_window.tab
    });
    let mut settings = jiffshot_workflow::settings_snapshot();
    // The tab is this window's; the capture side may not have it yet.
    settings.capture_window.tab = tab;
    with(|s| s.settings = settings);
    publish_view();
}

/// Updates the main Capture button when a recording starts or finishes.
pub fn recording_changed() {
    publish_view();
}

/// Shows a consistent pair of settings and tray metadata from the capture side.
#[cfg(target_os = "macos")]
pub fn refresh(state: &TrayState, settings: jiffshot_workflow::settings::Settings) {
    let update_settings = with(|s| {
        s.capture_key = state.capture_key.clone();
        s.timelapse = state.time_lapse;
        s.pending.is_none()
    });
    if !update_settings {
        return;
    }
    with(|s| s.settings = settings);
    publish_view();
}

fn publish_view() {
    let (sink, json) = with(|s| {
        let view = panel::unified_view(&s.settings, &s.capture_key, s.timelapse, capture::is_recording());
        #[cfg(target_os = "macos")]
        let view = NativeCaptureView { view, menus: native_menus(&s.settings) };
        (s.sink.clone(), serde_json::to_string(&view))
    });
    match json {
        Ok(json) => {
            if let Some(sink) = sink {
                if let Err(error) = sink.set_view(json) {
                    eprintln!("Could not update the Capture window view: {error}");
                }
            }
        }
        Err(error) => eprintln!("Could not serialize the Capture window view: {error}"),
    }
}

/// Sends `json` as the next value of a "serial\x1fJSON" property.
fn send(set: impl Fn(&CaptureViewModelSink, String) -> Result<()>, json: String) {
    let (sink, serial) = with(|s| {
        s.serial += 1;
        (s.sink.clone(), s.serial)
    });
    if let Some(sink) = sink {
        let _ = set(&sink, format!("{serial}\u{1f}{json}"));
    }
}

fn problem(text: &str) {
    send(|k, v| k.set_problem(v), text.to_owned());
}

/// Saves the edited settings on the capture side and shows them.
#[cfg(windows)]
fn changed() {
    capture::apply(with(|s| s.settings.clone()));
    publish_view();
}

fn set_presets(presets: Vec<jiffshot_workflow::settings::Preset>) {
    #[cfg(target_os = "macos")]
    {
        let queued = with(|s| {
            if s.pending.is_some() {
                s.queued.push_back(QueuedAction::Presets(presets.clone()));
                true
            } else {
                false
            }
        });
        if !queued {
            submit_edit(WindowEdit::ReplacePresets(presets), false, EditDestination::Capture);
        }
    }
    #[cfg(windows)]
    {
        let validation = with(|s| {
            let mut candidate = s.settings.clone();
            candidate.presets = presets.clone();
            jiffshot_workflow::prefs::validate_hotkeys(&candidate)
        });
        if let Err(error) = validation {
            problem(&error);
            return;
        }
        with(|s| s.settings.presets = presets.clone());
        capture::set_presets(presets);
        publish_view();
    }
}

/// The capture hotkey as the capture side last published it.
pub fn capture_key() -> String {
    with(|s| s.capture_key.clone())
}

fn open_panel(id: &str) {
    // The capture preferences are a tab of the one Preferences dialog.
    if id == "prefs" {
        crate::app::post(|app| app.open_prefs("capture"));
        return;
    }
    let (tab, name) = context(id);
    let (mut settings, key) = with(|s| (s.settings.clone(), s.capture_key.clone()));
    settings.capture_window.tab = tab.unwrap_or(1);
    let startup_status = capture::startup_status();
    let startup = startup_status.requested;
    #[cfg(windows)]
    if name == "prefs" {
        with(|s| s.startup_initial = Some(startup));
    }
    match panel::panel(&settings, name, startup, &key) {
        Some(mut p) => {
            p.id = id.to_owned();
            #[cfg(windows)]
            if name == "prefs" {
                if let Some(detail) = startup_status.detail {
                    p.rows.push(panel::note(detail));
                }
            }
            send(|k, v| k.set_panel(v), serde_json::to_string(&p).unwrap_or_default());
        }
        None => problem("Nothing to set here."),
    }
}

fn picked(id: &str) {
    #[cfg(target_os = "macos")]
    submit_edit(WindowEdit::Pick(id.to_owned()), false, EditDestination::Capture);
    #[cfg(windows)]
    {
        let result = with(|s| panel::pick_unified(&mut s.settings, id));
        handle_pick(result);
    }
}

fn handle_pick(result: Pick) {
    match result {
        Pick::Nothing => {}
        Pick::Changed => {
            #[cfg(windows)]
            changed();
        }
        Pick::Panel(id) => open_panel(&id),
        Pick::RunPreset(i) => {
            hide();
            capture::run_preset(i);
        }
        Pick::StepCapture => {
            hide();
            capture::step_capture();
        }
        Pick::Presets(presets) => {
            #[cfg(windows)]
            set_presets(presets);
            #[cfg(target_os = "macos")]
            let _ = presets;
        }
        Pick::ImportPresets => app::post(|app| {
            app.pick_files("Import presets", false, "Presets", &["json"], |_, paths| {
                let Some(bytes) = paths.first().and_then(|p| std::fs::read(p).ok()) else { return };
                let current = with(|s| s.settings.presets.clone());
                match panel::import_presets(&current, &bytes) {
                    Some(presets) => set_presets(presets),
                    None => problem("That file has no presets in it."),
                }
            })
        }),
        Pick::ExportPresets => {
            let presets = with(|s| s.settings.presets.clone());
            app::post(move |app| {
                app.pick_save("Export presets", "Jiffshot presets.json", &[("Presets", "json")], move |_, path| {
                    let written = serde_json::to_vec_pretty(&presets).ok().and_then(|b| std::fs::write(&path, b).ok());
                    if written.is_none() {
                        problem("Could not write the file.");
                    }
                })
            });
        }
        Pick::OpenEditor => app::post(|app| app.show_editor()),
        Pick::OpenLibrary => capture::open_library(),
        Pick::Close => hide(),
        Pick::Exit => TrayAction::Exit.post(),
    }
}

fn applied(id: &str, values: &str) {
    #[cfg(target_os = "macos")]
    {
        let Ok(values) = serde_json::from_str::<Vec<serde_json::Value>>(values) else {
            problem("Could not read the Capture options.");
            return;
        };
        submit_edit(WindowEdit::Apply { id: id.to_owned(), values }, false, EditDestination::Capture);
    }
    #[cfg(windows)]
    {
        let values: Vec<serde_json::Value> = serde_json::from_str(values).unwrap_or_default();
        let (tab, name) = context(id);
        let startup = if name == "prefs" {
            with(|s| s.startup_initial.take()).unwrap_or_else(capture::runs_at_startup)
        } else {
            capture::runs_at_startup()
        };
        let result = with(|s| {
            let previous = s.settings.capture_window.tab;
            s.settings.capture_window.tab = tab.unwrap_or(1);
            let result = panel::apply(&mut s.settings, name, &values, startup);
            s.settings.capture_window.tab = previous;
            result
        });
        match result {
            Applied::Changed => changed(),
            Applied::MoveSchedule(at) => {
                changed();
                capture::move_schedule(at);
            }
            Applied::Presets(presets) => set_presets(presets),
            Applied::Startup(on) => {
                changed();
                capture::set_run_at_startup(on);
            }
            Applied::Problem(text) => problem(&text),
        }
    }
}

/// Select region on screen…: keeps the Region options as they stand, then
/// hides the window for the overlay; the capture side shows it again, with
/// the options.
#[cfg(windows)]
fn select_fixed_region(video: bool, values: &str) {
    let panel = if video { "video:selection" } else { "image:selection" };
    applied(panel, values);
    with(|s| s.reopen_panel = Some(panel));
    hide();
    capture::select_fixed_region(video);
}

#[cfg(target_os = "macos")]
fn select_scheduled_area(values: &str) {
    let Ok(values) = serde_json::from_str::<Vec<serde_json::Value>>(values) else {
        problem("Could not read the Time delay options.");
        return;
    };
    if values.len() < 5 {
        problem("The Time delay options are incomplete.");
        return;
    }
    submit_edit(WindowEdit::Apply { id: "delay".into(), values: values[..5].to_vec() }, true, EditDestination::Capture);
}

#[cfg(target_os = "macos")]
pub(crate) fn apply_preferences(values: Vec<serde_json::Value>, hotkeys: Vec<serde_json::Value>, startup_initial: bool) {
    let queued = with(|s| {
        if s.pending.is_some() {
            s.queued.push_back(QueuedAction::Preferences { values: values.clone(), hotkeys: hotkeys.clone(), startup_initial });
            true
        } else {
            false
        }
    });
    if !queued {
        submit_edit(WindowEdit::Preferences { values, hotkeys, startup_initial }, false, EditDestination::Preferences);
    }
}

#[cfg(target_os = "macos")]
fn open_scheduled_area_selector() {
    let Some(window) = with(|s| s.window.clone()) else {
        problem("The Capture window is unavailable.");
        return;
    };
    if let Err(error) = crate::ui::capture_visibility::hide_window(&window) {
        problem(&format!("Could not hide the Capture window: {error:?}"));
        return;
    }
    if !capture::select_scheduled_area() {
        if let Err(error) = window.show() {
            eprintln!("Could not restore the Capture window: {error:?}");
        }
        problem("The capture service is unavailable.");
    }
}

#[cfg(target_os = "macos")]
fn report_edit_problem(destination: EditDestination, text: String) {
    match destination {
        EditDestination::Capture => problem(&text),
        EditDestination::Preferences => app::report_capture_settings_notice("Capture preferences not fully applied", text),
    }
}

#[cfg(target_os = "macos")]
fn submit_edit(edit: WindowEdit, select_scheduled_area: bool, destination: EditDestination) {
    let prepared = with(|s| {
        let scope = s.scope.clone().ok_or_else(|| "The Capture window is unavailable.".to_owned())?;
        let id = s.next_request.checked_add(1).ok_or_else(|| "Capture settings request IDs were exhausted.".to_owned())?;
        s.next_request = id;
        Ok::<_, String>((scope, id))
    });
    let (scope, id) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            report_edit_problem(destination, error);
            return;
        }
    };
    let wake = scope.clone();
    let ticket = match capture::edit_window(edit, move || {
        if let Err(error) = wake.post(move || check_edit(id, false)) {
            jiffshot_workflow::log(&format!("Could not deliver a Capture settings acknowledgement: {error:?}"));
        }
    }) {
        Ok(ticket) => ticket,
        Err(error) => {
            report_edit_problem(destination, error);
            return;
        }
    };
    with(|s| s.pending = Some(PendingEdit { id, ticket: ticket.clone(), select_scheduled_area, destination }));
    if let Err(error) = arm_edit_watchdog(id, &scope) {
        match ticket.cancel_queued() {
            Ok(true) => finish_edit(id, Err(format!("Could not monitor the Capture settings request: {error}"))),
            Ok(false) => {
                jiffshot_workflow::log(&format!("Could not monitor a running Capture settings request: {error}"));
                report_edit_problem(destination, "The Capture settings request is still running, but its progress could not be monitored.".into());
            }
            Err(error) => finish_edit(id, Err(error)),
        }
    }
}

#[cfg(target_os = "macos")]
fn arm_edit_watchdog(id: u64, scope: &AppScope) -> std::result::Result<(), String> {
    let wait = scope.delay(std::time::Duration::from_secs(5)).map_err(|error| format!("{error:?}"))?;
    scope.spawn(async move {
        if let Err(error) = wait.await {
            if with(|s| s.pending.as_ref().is_some_and(|pending| pending.id == id)) {
                jiffshot_workflow::log(&format!("Capture settings watchdog failed: {error:?}"));
                check_edit(id, true);
            }
            return;
        }
        check_edit(id, true);
    }).map_err(|error| format!("{error:?}"))
}

#[cfg(target_os = "macos")]
fn check_edit(id: u64, timed: bool) {
    let pending = with(|s| {
        s.pending.as_ref().filter(|pending| pending.id == id)
            .and_then(|pending| s.scope.clone().map(|scope| (pending.ticket.clone(), scope, pending.destination)))
    });
    let Some((ticket, scope, destination)) = pending else { return };
    match ticket.status() {
        Ok(WindowEditStatus::Finished(result)) => finish_edit(id, result),
        Ok(WindowEditStatus::Cancelled) => finish_edit(id, Err("The Capture settings request was cancelled.".into())),
        Ok(WindowEditStatus::Consumed) => {}
        Ok(WindowEditStatus::Queued) if timed => match ticket.cancel_queued() {
            Ok(true) => finish_edit(id, Err("The capture service did not start the settings change. Nothing was changed; try again.".into())),
            Ok(false) => check_edit(id, true),
            Err(error) => finish_edit(id, Err(error)),
        },
        Ok(WindowEditStatus::Running) if timed => {
            match crate::capture_engine_macos::running() {
                Ok(false) => match ticket.status() {
                    Ok(WindowEditStatus::Finished(result)) => {
                        finish_edit(id, result);
                        return;
                    }
                    Err(error) => {
                        finish_edit(id, Err(error));
                        return;
                    }
                    _ => {
                        finish_edit(id, Err("The capture service stopped before acknowledging the settings change. Check the settings before retrying.".into()));
                        return;
                    }
                },
                Err(error) => jiffshot_workflow::log(&format!("Could not check the capture service while applying settings: {error}")),
                Ok(true) => {}
            }
            if let Err(error) = arm_edit_watchdog(id, &scope) {
                jiffshot_workflow::log(&format!("Could not monitor a running Capture settings request: {error}"));
                report_edit_problem(destination, "The Capture settings request is still running, but its progress could not be monitored.".into());
            }
        }
        Ok(WindowEditStatus::Queued | WindowEditStatus::Running) => {}
        Err(error) => finish_edit(id, Err(error)),
    }
}

#[cfg(target_os = "macos")]
fn finish_edit(id: u64, result: std::result::Result<WindowEditResult, String>) {
    let pending = with(|s| {
        if s.pending.as_ref().is_some_and(|pending| pending.id == id) {
            s.pending.take()
        } else {
            None
        }
    });
    let Some(pending) = pending else { return };
    with(|s| s.settings = jiffshot_workflow::settings_snapshot());
    publish_view();
    match result {
        Err(error) => report_edit_problem(pending.destination, error),
        Ok(WindowEditResult { effect, mut warnings }) => {
            let mut select = false;
            match effect {
                WindowEffect::Pick(pick) => handle_pick(pick),
                WindowEffect::Changed => {}
                WindowEffect::Applied(applied) => match applied {
                    Applied::Changed | Applied::MoveSchedule(_) => select = pending.select_scheduled_area,
                    Applied::ChangedWithWarning(warning) | Applied::MoveScheduleWithWarning(_, warning) => {
                        warnings.push(warning);
                        select = pending.select_scheduled_area;
                    }
                    Applied::Startup(on) => capture::set_run_at_startup(on),
                    Applied::Presets(_) => {}
                    Applied::Problem(error) => warnings.push(error),
                },
            }
            if !warnings.is_empty() {
                let text = warnings.join("\n");
                if select {
                    app::report_capture_settings_notice("Capture settings", text);
                } else {
                    report_edit_problem(pending.destination, text);
                }
            }
            if select {
                open_scheduled_area_selector();
            } else if pending.select_scheduled_area && warnings.is_empty() {
                problem("Could not apply the Time delay options.");
            }
        }
    }
    advance_queue();
}

#[cfg(target_os = "macos")]
fn advance_queue() {
    loop {
        let next = with(|s| if s.pending.is_none() { s.queued.pop_front() } else { None });
        match next {
            Some(QueuedAction::Action(action)) => act(&action),
            Some(QueuedAction::Presets(presets)) => set_presets(presets),
            Some(QueuedAction::Preferences { values, hotkeys, startup_initial }) => apply_preferences(values, hotkeys, startup_initial),
            None => break,
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn ui_only_selftest_action(action: &str) {
    let (verb, arg) = action.split_once(':').unwrap_or((action, ""));
    with(|s| match verb {
        "tab" => assert_eq!(panel::pick(&mut s.settings, action), Pick::Changed),
        "switch" => panel::flip(&mut s.settings, panel::Switch::from_id(arg).expect("unknown self-test switch")),
        _ => panic!("unsupported UI-only self-test action"),
    });
    publish_view();
}

fn interval_action(settings: &Settings, running: bool) -> Option<capture::IntervalAction> {
    if panel::capture_timing(settings, settings.capture_window.tab.min(2)) != panel::CaptureTiming::Interval {
        return None;
    }
    Some(if running { capture::IntervalAction::Stop } else { capture::IntervalAction::Start })
}

fn schedule_action(settings: &Settings) -> Option<capture::ScheduleAction> {
    if panel::capture_timing(settings, settings.capture_window.tab.min(2)) != panel::CaptureTiming::Schedule {
        return None;
    }
    Some(if settings.schedule.is_some() { capture::ScheduleAction::Stop } else { capture::ScheduleAction::Start })
}

fn capture_settings(settings: &Settings, target: &str) -> Settings {
    let mut settings = settings.clone();
    settings.capture_window.tab = match target {
        "select" => 0,
        "image" => 1,
        "video" => 2,
        _ => settings.capture_window.tab.min(2),
    };
    settings
}

fn form_values(arg: &str) -> Option<(&str, &str)> {
    let boundary = arg.find(":[")?;
    Some((&arg[..boundary], &arg[boundary + 1..]))
}

fn capture_menu(settings: &Settings, id: &str) -> Vec<panel::MenuItem> {
    if let Some(items) = panel::unified_menu(settings, id) {
        return items;
    }
    let (tab, name) = context(id);
    let mut settings = settings.clone();
    settings.capture_window.tab = tab.unwrap_or(1);
    let mut items = panel::menu(&settings, name);
    if let Some(tab) = tab {
        items = panel::scoped_items(if tab == 2 { "video" } else { "image" }, items);
    }
    match name {
        "view" => items.retain(|item| !item.id.starts_with("tab:") && !item.separator),
        "edit" => {
            for item in items.iter_mut().filter(|item| item.id == "reset") {
                item.label = "Reset image and video options".into();
            }
        }
        _ => {}
    }
    items
}

/// The title menus for the macOS menu bar, without what the application
/// menu already offers (About, Preferences and Quit).
#[cfg(target_os = "macos")]
fn native_menus(settings: &Settings) -> Vec<panel::MenuItem> {
    [("File", "file"), ("Edit", "edit"), ("View", "view"), ("Help", "help")]
        .into_iter()
        .filter_map(|(label, name)| {
            let mut items = capture_menu(settings, name);
            items.retain(|item| !matches!(item.id.as_str(), "exit" | "panel:prefs" | "window:about"));
            for item in &mut items {
                if let Some((text, _)) = item.label.split_once('\t') {
                    item.label = text.to_owned();
                }
            }
            let mut tidy: Vec<panel::MenuItem> = Vec::with_capacity(items.len());
            for item in items {
                if !item.separator || tidy.last().is_some_and(|last| !last.separator) {
                    tidy.push(item);
                }
            }
            if tidy.last().is_some_and(|last| last.separator) {
                tidy.pop();
            }
            (!tidy.is_empty()).then(|| panel::MenuItem { label: label.to_owned(), enabled: true, items: tidy, ..Default::default() })
        })
        .collect()
}

fn act(action: &str) {
    if action == "capture" && capture::is_recording() {
        capture::stop_recording();
        return;
    }
    if action == "show-recording-controls" {
        if !capture::is_recording() {
            problem("No recording is running. Start a new recording to show its controls.");
        } else if let Err(error) = crate::ui::restore_recording_controls() {
            problem(&error);
        }
        publish_view();
        return;
    }
    #[cfg(target_os = "macos")]
    if with(|s| {
        if s.pending.is_some() {
            s.queued.push_back(QueuedAction::Action(action.to_owned()));
            true
        } else {
            false
        }
    }) {
        return;
    }
    let (verb, arg) = action.split_once(':').unwrap_or((action, ""));
    match verb {
        "tab" => picked(action),
        "switch" => {
            if let Some(switch) = panel::Switch::from_id(arg) {
                #[cfg(target_os = "macos")]
                submit_edit(WindowEdit::Flip(switch), false, EditDestination::Capture);
                #[cfg(windows)]
                {
                    with(|s| panel::flip(&mut s.settings, switch));
                    changed();
                }
            }
        }
        "menu" => {
            let items = with(|s| s.settings.clone());
            let items = capture_menu(&items, arg);
            send(|k, v| k.set_menu(v), serde_json::to_string(&items).unwrap_or_default());
        }
        "pick" => picked(arg),
        "panel" => open_panel(arg),
        "apply" => {
            if let Some((id, values)) = form_values(arg) {
                applied(id, values);
            } else {
                problem("Could not apply these options. Reopen the options and try again.");
            }
        }
        #[cfg(target_os = "macos")]
        "select-scheduled-area" => select_scheduled_area(arg),
        #[cfg(windows)]
        "select-image-region" | "select-video-region" => select_fixed_region(verb == "select-video-region", arg),
        "capture" => {
            let (settings, running) = with(|s| (capture_settings(&s.settings, arg), s.timelapse));
            let request_id = crate::ui::capture_view::starting_capture(
                !settings.open_in_editor && settings.capture_window.tab != 2
                    && interval_action(&settings, running).is_none()
                    && schedule_action(&settings).is_none()
                    && panel::mode_of(&settings, settings.capture_window.tab) != "text",
            );
            #[cfg(target_os = "macos")]
            {
                let posted = if let Some(action) = interval_action(&settings, running) {
                    capture::set_interval_current(action)
                } else if let Some(action) = schedule_action(&settings) {
                    capture::set_schedule_current(action)
                } else {
                    capture::start_current(settings.capture_window.tab, request_id)
                };
                if posted {
                    hide();
                } else {
                    crate::ui::capture_view::cancel_pending_capture(request_id);
                    problem("The capture service is unavailable.");
                }
            }
            #[cfg(windows)]
            {
                hide();
                if let Some(action) = interval_action(&settings, running) {
                    capture::set_interval(settings, action);
                } else if let Some(action) = schedule_action(&settings) {
                    capture::set_schedule(settings, action);
                } else {
                    capture::start(settings, request_id);
                }
            }
        }
        "stop-interval" => capture::stop_timelapse(),
        "cancel-schedule" => capture::cancel_schedule(),
        "editor" => app::post(|app| app.show_editor()),
        _ => {}
    }
}

pub struct CaptureModel;

impl CaptureViewModel for CaptureModel {
    fn attach(&mut self, sink: CaptureViewModelSink) -> Result<()> {
        with(|s| {
            s.sink = Some(sink);
            #[cfg(target_os = "macos")]
            if s.pending.is_none() {
                s.settings = jiffshot_workflow::settings_snapshot();
            }
            #[cfg(windows)]
            {
                s.settings = jiffshot_workflow::settings_snapshot();
            }
        });
        publish_view();
        Ok(())
    }

    fn detach(&mut self) -> Result<()> {
        with(|s| s.sink = None);
        Ok(())
    }

    fn set_action(&mut self, _value: String) -> Result<()> {
        Ok(())
    }

    fn run(&mut self, value: String) -> Result<()> {
        act(&value);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn native_capture_menus_follow_settings_without_application_menu_duplicates() {
        let mut settings = Settings::default();
        settings.show_widget = false;
        let menus = native_menus(&settings);
        assert_eq!(menus.iter().map(|menu| menu.label.as_str()).collect::<Vec<_>>(), ["File", "Edit", "View", "Help"]);
        for menu in &menus {
            assert!(!menu.items.first().is_some_and(|item| item.separator), "{} starts with a separator", menu.label);
            assert!(!menu.items.last().is_some_and(|item| item.separator), "{} ends with a separator", menu.label);
            assert!(menu.items.windows(2).all(|pair| !(pair[0].separator && pair[1].separator)));
            assert!(menu.items.iter().all(|item| !matches!(item.id.as_str(), "exit" | "panel:prefs" | "window:about")));
        }
        let view = menus.iter().find(|menu| menu.label == "View").expect("View menu");
        assert!(!view.items.iter().find(|item| item.id == "widget").expect("widget item").checked);
        settings.show_widget = true;
        let menus = native_menus(&settings);
        let view = menus.iter().find(|menu| menu.label == "View").expect("View menu");
        assert!(view.items.iter().find(|item| item.id == "widget").expect("widget item").checked);

        let view = NativeCaptureView {
            view: panel::unified_view(&settings, "", false, false),
            menus,
        };
        let json = serde_json::to_value(view).expect("serialize native Capture view");
        assert_eq!(json["widget"], true);
        assert_eq!(json["menus"][2]["items"][0]["id"], "widget");
    }

    #[test]
    fn split_actions_use_independent_modes_and_keep_video_manual() {
        let mut settings = Settings::default();
        settings.capture_window.selection = "scrolling".into();
        settings.capture_window.video_selection = "fullscreen".into();
        settings.capture_window.timer = true;
        settings.capture_window.timing = "interval".into();
        settings.record_audio = "both".into();
        for tab in 0..=2 {
            settings.capture_window.tab = tab;
            let image = capture_settings(&settings, "image");
            let video = capture_settings(&settings, "video");
            assert_eq!(panel::mode_of(&image, image.capture_window.tab), "scrolling");
            assert_eq!(panel::mode_of(&video, video.capture_window.tab), "video-fullscreen");
            assert_eq!(interval_action(&image, false), Some(capture::IntervalAction::Start));
            assert_eq!(interval_action(&video, true), None, "video must not stop an interval");
            assert_eq!(video.record_audio, "both");
            settings.capture_window.timing = "schedule".into();
            settings.schedule = Some("12:45".into());
            assert_eq!(schedule_action(&capture_settings(&settings, "video")), None, "video must not cancel a schedule");
            assert_eq!(schedule_action(&capture_settings(&settings, "image")), Some(capture::ScheduleAction::Stop));
            settings.capture_window.timing = "interval".into();
        }
    }

    #[test]
    fn mode_switch_changes_the_main_action_and_relevant_controls() {
        let mut settings = Settings { delay_seconds: 0, ..Settings::default() };
        settings.capture_window.selection = "window".into();
        settings.capture_window.video_selection = "window".into();
        let expected = [
            ("Select", vec!["area", "send", "image:effects"], 6, "Then choose"),
            ("Capture", vec!["area", "send", "image:effects"], 3, ""),
            ("Record", vec!["video:selection", "send"], 5, ""),
        ];
        for (tab, (verb, fields, toggles, detail)) in expected.into_iter().enumerate() {
            settings.capture_window.tab = tab;
            let view = panel::unified_view(&settings, "", false, false);
            assert_eq!(view.tab, tab);
            assert_eq!(view.fields.iter().map(|row| row.id).collect::<Vec<_>>(), fields);
            assert_eq!(view.fields[0].text, "Window");
            assert_eq!(view.capture.verb, verb);
            assert_eq!(view.capture.detail, detail);
            assert_eq!(view.toggles.len(), toggles);
            assert_eq!(panel::mode_of(&capture_settings(&settings, "current"), tab), ["all", "window", "video"][tab]);
        }
        settings.capture_window.tab = 2;
        settings.capture_window.selection = "scrolling".into();
        assert_eq!(
            panel::unified_view(&settings, "", false, false).capture.detail,
            "Records Window"
        );
    }

    #[cfg(windows)]
    #[test]
    fn mode_actions_switch_tabs_and_update_the_view() {
        let expected = [
            ("Select", vec!["area", "send", "image:effects"], 6),
            ("Capture", vec!["area", "send", "image:effects"], 3),
            ("Record", vec!["video:selection", "send"], 5),
        ];
        for previous in 0..=2 {
            for (target, (verb, fields, toggles)) in expected.iter().enumerate() {
                let mut settings = Settings { delay_seconds: 0, ..Settings::default() };
                settings.capture_window.tab = previous;
                with(|s| s.settings = settings);

                act(&format!("tab:{target}"));

                with(|s| {
                    let view = panel::unified_view(&s.settings, "", false, false);
                    assert_eq!(s.settings.capture_window.tab, target, "{previous} -> {target}");
                    assert_eq!(view.tab, target, "{previous} -> {target}");
                    assert_eq!(view.capture.verb, *verb, "{previous} -> {target}");
                    assert_eq!(view.fields.iter().map(|row| row.id).collect::<Vec<_>>(), *fields, "{previous} -> {target}");
                    assert_eq!(view.toggles.len(), *toggles, "{previous} -> {target}");
                });
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn scoped_picks_preserve_the_selected_tab() {
        for tab in 0..=2 {
            let mut settings = Settings::default();
            settings.capture_window.tab = tab;
            with(|s| s.settings = settings);

            act("pick:video:vsel:window");
            act("pick:image:sel:scrolling");

            with(|s| {
                assert_eq!(s.settings.capture_window.tab, tab);
                assert_eq!(s.settings.capture_window.selection, "scrolling");
                assert_eq!(s.settings.capture_window.video_selection, "window");
            });
        }
    }

    #[cfg(windows)]
    #[test]
    fn shared_area_updates_both_targets_for_every_saved_tab() {
        for tab in 0..=2 {
            let mut settings = Settings::default();
            settings.capture_window.tab = tab;
            settings.capture_window.selection = "scrolling".into();
            settings.capture_window.video_selection = "region".into();
            with(|s| s.settings = settings);
            act("pick:area:window");
            with(|s| {
                let w = &s.settings.capture_window;
                assert_eq!((w.selection.as_str(), w.video_selection.as_str(), w.tab), ("window", "window", tab), "saved tab {tab}");
            });
            act("pick:area:region");
            with(|s| {
                let w = &s.settings.capture_window;
                assert_eq!((w.selection.as_str(), w.video_selection.as_str(), w.tab), ("image", "region", tab), "saved tab {tab}");
            });
        }
    }

    #[cfg(windows)]
    #[test]
    fn shared_area_and_cursor_apply_to_images_and_video() {
        with(|s| s.settings = Settings::default());
        act("pick:area:fullscreen");
        with(|s| {
            assert_eq!(s.settings.capture_window.selection, "fullscreen");
            assert_eq!(s.settings.capture_window.video_selection, "fullscreen");
        });
        act("pick:area:region");
        with(|s| assert_eq!((s.settings.capture_window.selection.as_str(), s.settings.capture_window.video_selection.as_str()), ("image", "region")));
        act("pick:image:sel:scrolling");
        with(|s| assert_eq!((s.settings.capture_window.selection.as_str(), s.settings.capture_window.video_selection.as_str()), ("scrolling", "region")));
        let before = with(|s| s.settings.include_cursor && s.settings.record_cursor);
        act("pick:cursor-both");
        with(|s| {
            assert_eq!(s.settings.include_cursor, !before);
            assert_eq!(s.settings.record_cursor, !before);
        });
    }

    #[test]
    fn unified_menus_route_selection_sharing_and_panels_to_their_target() {
        let mut settings = Settings::default();
        settings.capture_window.tab = 0;
        let area = capture_menu(&settings, "area");
        assert!(area.iter().any(|item| item.id == "image:sel:scrolling"));
        assert!(!area.iter().any(|item| item.id == "image:sel:image"), "Region is listed once");
        let send = capture_menu(&settings, "send");
        assert!(send.iter().any(|item| item.id.starts_with("image:share:")));
        assert!(send.iter().any(|item| item.id.starts_with("video:vshare:")));
        settings.capture_window.tab = 1;
        assert!(capture_menu(&settings, "send").iter().all(|item| !item.id.starts_with("video:")));
        settings.capture_window.tab = 2;
        assert!(capture_menu(&settings, "send").iter().all(|item| !item.id.starts_with("image:")));
        settings.capture_window.tab = 0;
        assert!(capture_menu(&settings, "image:effects").iter().any(|item| item.id.starts_with("image:fx:")));
        for item in capture_menu(&settings, "more").iter().filter(|item| item.id.contains("panel:")) {
            let (tab, id) = context(&item.id);
            let mut scoped_settings = settings.clone();
            scoped_settings.capture_window.tab = tab.unwrap_or(1);
            let Pick::Panel(id) = panel::pick(&mut scoped_settings, id) else { panic!("not a panel: {}", item.id) };
            assert!(panel::panel(&scoped_settings, &id, false, "").is_some(), "missing {}", item.id);
        }
        assert!(capture_menu(&settings, "view").iter().all(|item| !item.id.starts_with("tab:")));
    }

    #[cfg(windows)]
    #[test]
    fn edit_menu_reset_restores_both_modes_without_changing_the_selected_mode() {
        let defaults = Settings::default();
        for tab in 0..=2 {
            let mut settings = Settings::default();
            settings.capture_window.tab = tab;
            settings.capture_window.selection = "scrolling".into();
            settings.capture_window.video_selection = "fullscreen".into();
            settings.capture_window.timer = true;
            settings.record_audio = "both".into();
            settings.record_fps = 60;
            let reset = capture_menu(&settings, "edit").into_iter().find(|item| item.id == "reset").expect("Edit menu must contain reset");
            assert_eq!(reset.label, "Reset image and video options");

            with(|s| s.settings = settings);
            act(&format!("pick:{}", reset.id));
            with(|s| {
                assert_eq!(s.settings.capture_window.tab, tab);
                assert_eq!(s.settings.capture_window.selection, defaults.capture_window.selection);
                assert_eq!(s.settings.capture_window.video_selection, defaults.capture_window.video_selection);
                assert!(!s.settings.capture_window.timer);
                assert_eq!(s.settings.record_audio, defaults.record_audio);
                assert_eq!(s.settings.record_fps, defaults.record_fps);
            });
        }
    }

    #[cfg(windows)]
    #[test]
    fn scoped_form_submission_applies_video_settings_without_touching_image_settings() {
        let mut settings = Settings::default();
        settings.capture_window.tab = 0;
        settings.fixed_size = (800, 600);
        with(|s| s.settings = settings);
        act("apply:video:selection:[3,7]");
        with(|s| {
            assert_eq!(s.settings.record_fps, 60);
            assert_eq!(s.settings.countdown, 7);
            assert_eq!(s.settings.fixed_size, (800, 600));
            assert_eq!(s.settings.capture_window.tab, 0);
        });
    }

    #[cfg(windows)]
    #[test]
    fn selecting_a_region_on_screen_keeps_the_options_and_reopens_them() {
        let mut settings = Settings::default();
        settings.capture_window.tab = 1;
        with(|s| s.settings = settings);
        act("select-video-region:[true,1280,720,true,40,30,0,5]");
        with(|s| {
            assert!(s.settings.capture_window.video_fixed);
            assert_eq!(s.settings.video_fixed_size, (1280, 720));
            assert_eq!(s.settings.capture_window.video_fixed_at, Some((40, 30)));
            assert_eq!((s.settings.record_fps, s.settings.countdown), (15, 5));
            assert_eq!(s.settings.capture_window.tab, 1, "the saved mode stays");
            assert_eq!(s.reopen_panel.take(), Some("video:selection"));
        });
        act("select-image-region:[true,640,480,false,0,0,true]");
        with(|s| {
            assert_eq!((s.settings.fixed_size, s.settings.capture_window.fixed), ((640, 480), true));
            assert_eq!(s.reopen_panel.take(), Some("image:selection"));
        });
    }

    #[test]
    fn scoped_form_values_keep_the_target_and_embedded_colons() {
        assert_eq!(form_values("image:selection:[true,800,600]"),
            Some(("image:selection", "[true,800,600]")));
        assert_eq!(form_values("video:new-preset:[\"Meeting: demo\",\"\"]"),
            Some(("video:new-preset", "[\"Meeting: demo\",\"\"]")));
    }

    #[test]
    fn primary_action_preserves_the_displayed_interval_intent() {
        let mut settings = Settings::default();
        settings.capture_window.timer = true;
        settings.capture_window.timing = "interval".into();

        for tab in [0, 1] {
            settings.capture_window.tab = tab;
            assert_eq!(interval_action(&settings, false), Some(capture::IntervalAction::Start));
            assert_eq!(interval_action(&settings, true), Some(capture::IntervalAction::Stop));
        }
        settings.capture_window.tab = 2;
        assert_eq!(interval_action(&settings, true), None, "video remains a recording action");
        settings.capture_window.tab = 0;
        settings.capture_window.timer = false;
        assert_eq!(interval_action(&settings, true), None, "manual capture remains available");
        settings.capture_window.timer = true;
        settings.capture_window.timing = "schedule".into();
        assert_eq!(interval_action(&settings, true), None, "schedule remains a separate action");
    }

    #[test]
    fn primary_action_preserves_the_displayed_schedule_intent() {
        let mut settings = Settings::default();
        settings.capture_window.timer = true;
        settings.capture_window.timing = "schedule".into();

        for tab in [0, 1] {
            settings.capture_window.tab = tab;
            settings.schedule = None;
            assert_eq!(schedule_action(&settings), Some(capture::ScheduleAction::Start));
            settings.schedule = Some("12:45".into());
            assert_eq!(schedule_action(&settings), Some(capture::ScheduleAction::Stop));
        }
        settings.capture_window.tab = 2;
        assert_eq!(schedule_action(&settings), None, "video remains a recording action");
        settings.capture_window.tab = 0;
        settings.capture_window.timer = false;
        assert_eq!(schedule_action(&settings), None, "manual capture remains available");
        settings.capture_window.timer = true;
        settings.capture_window.timing = "interval".into();
        assert_eq!(schedule_action(&settings), None, "interval remains a separate action");
    }
}
