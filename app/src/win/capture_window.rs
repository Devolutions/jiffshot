//! The Capture window's model. What the window shows and offers comes from
//! `jiffshot_workflow::panel` as JSON ([`View`], menus, option panels); the
//! window sends back actions, and every change goes to the capture side,
//! which owns the settings.
//!
//! Actions: `tab:N`, `switch:ID`, `menu:NAME`, `pick:ID`, `panel:ID`,
//! `apply:ID:[values]`, `capture`, `editor`, `closed`.

use crate::app;
use crate::vm::{CaptureViewModel, CaptureViewModelSink};
use jiffshot_workflow::panel::{self, Applied, Pick};
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
    let Some(window) = with(|s| s.window.clone()) else { return };
    let _ = window.show();
    if let Ok(hwnd) = scope.platform_handle(&window) {
        crate::winutil::bring_forward(hwnd);
    }
}

fn hide() {
    if let Some(window) = with(|s| s.window.clone()) {
        let _ = window.hide();
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
        let view = panel::view(&s.settings, &s.capture_key, s.timelapse);
        (s.sink.clone(), serde_json::to_string(&view).unwrap_or_default())
    });
    if let Some(sink) = sink {
        let _ = sink.set_view(json);
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
    let (settings, key) = with(|s| (s.settings.clone(), s.capture_key.clone()));
    match panel::panel(&settings, id, capture::runs_at_startup(), &key) {
        Some(p) => send(|k, v| k.set_panel(v), serde_json::to_string(&p).unwrap_or_default()),
        None => problem("Nothing to set here."),
    }
}

fn picked(id: &str) {
    #[cfg(target_os = "macos")]
    submit_edit(WindowEdit::Pick(id.to_owned()), false, EditDestination::Capture);
    #[cfg(windows)]
    {
        let result = with(|s| panel::pick(&mut s.settings, id));
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
        let startup = capture::runs_at_startup();
        let result = with(|s| panel::apply(&mut s.settings, id, &values, startup));
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
    if let Err(error) = window.hide() {
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

fn act(action: &str) {
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
            let items = with(|s| panel::menu(&s.settings, arg));
            send(|k, v| k.set_menu(v), serde_json::to_string(&items).unwrap_or_default());
        }
        "pick" => picked(arg),
        "panel" => open_panel(arg),
        "apply" => {
            if let Some((id, values)) = arg.split_once(':') {
                applied(id, values);
            }
        }
        #[cfg(target_os = "macos")]
        "select-scheduled-area" => select_scheduled_area(arg),
        "capture" => {
            #[cfg(target_os = "macos")]
            if capture::start_current() {
                hide();
            } else {
                problem("The capture service is unavailable.");
            }
            #[cfg(windows)]
            {
                hide();
                capture::start(with(|s| s.settings.clone()));
            }
        }
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
