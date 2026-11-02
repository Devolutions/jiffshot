//! The Capture window on Linux: the shared Capture view driven by a small
//! model over the portal capture and recording engines. The view speaks the
//! JSON of the Windows panel (`tab:N`, `menu:ID`, `pick:ID`, `capture`).

use crate::vm::{CaptureViewModel, CaptureViewModelSink};
use rustolonia::{Result, Window};
use serde_json::{json, Value};
use std::cell::RefCell;

const DELAYS: [u64; 4] = [0, 3, 5, 10];
const INTERVALS: [u32; 5] = [0, 5, 10, 30, 60];
const AREAS: [(&str, &str); 11] = [
    ("region", "Region"),
    ("freehand", "Freehand region"),
    ("multi", "Multiple areas"),
    ("fixed", "Fixed size (1280 × 720)"),
    ("repeat", "Repeat last area"),
    ("fullscreen", "Full screen"),
    ("screen", "Window or screen (chooser)"),
    ("scrolling", "Scrolling capture (down)"),
    ("scrolling-h", "Scrolling capture (right)"),
    ("panoramic", "Panoramic capture"),
    ("text", "Text from screen"),
];

struct State {
    sink: Option<CaptureViewModelSink>,
    window: Option<Window>,
    /// 1 captures images, 2 records video.
    tab: usize,
    area: String,
    /// Seconds between interval captures; 0 captures once.
    interval: u32,
    cursor: bool,
    delay: u64,
    system_audio: bool,
    webcam: bool,
    microphone: bool,
    serial: u64,
}

thread_local! {
    static STATE: RefCell<State> = const { RefCell::new(State {
        sink: None,
        window: None,
        tab: 1,
        area: String::new(),
        interval: 0,
        cursor: true,
        delay: 0,
        system_audio: false,
        webcam: false,
        microphone: false,
        serial: 0,
    }) };
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

fn settings_path() -> Option<std::path::PathBuf> {
    Some(dirs::config_dir()?.join("jiffshot").join("capture-window.json"))
}

fn load() {
    let Some(value) = settings_path()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return;
    };
    with(|s| {
        if let Some(tab) = value["tab"].as_u64() {
            s.tab = if tab == 2 { 2 } else { 1 };
        }
        if let Some(area) = value["area"].as_str().filter(|a| AREAS.iter().any(|(id, _)| id == a)) {
            s.area = if area == "screen" { "fullscreen" } else { area }.to_owned();
        }
        s.interval = value["interval"].as_u64().map_or(0, |n| n as u32);
        s.delay = value["delay"].as_u64().unwrap_or(0);
        s.cursor = value["cursor"].as_bool().unwrap_or(true);
        s.system_audio = value["system_audio"].as_bool().unwrap_or(false);
        s.webcam = value["webcam"].as_bool().unwrap_or(false);
        s.microphone = value["microphone"].as_bool().unwrap_or(false);
    });
}

fn save() {
    let value = with(|s| {
        json!({
            "tab": s.tab, "area": s.area, "interval": s.interval, "delay": s.delay,
            "cursor": s.cursor, "system_audio": s.system_audio, "webcam": s.webcam, "microphone": s.microphone,
        })
    });
    let Some(path) = settings_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(error) = std::fs::write(&path, value.to_string()) {
        eprintln!("jiffshot: could not save the Capture window settings: {error}");
    }
}

/// Remembers the window mounted for this model (hidden until shown).
pub fn set_window(window: Window) {
    load();
    with(|s| {
        if s.area.is_empty() {
            s.area = "region".to_owned();
        }
        s.window = Some(window);
    });
}

/// Redraws the window after a capture started or ended elsewhere.
pub fn refresh() {
    crate::app::post(|_| publish_view());
}

/// Shows the window in front.
pub fn show() {
    publish_view();
    if let Some(window) = with(|s| s.window.clone()) {
        let _ = window.show();
    }
}

fn hide() {
    if let Some(window) = with(|s| s.window.clone()) {
        let _ = window.hide();
    }
}

fn audio_name(s: &State) -> &'static str {
    match (s.system_audio, s.microphone) {
        (true, true) => "both",
        (true, false) => "system",
        (false, true) => "microphone",
        (false, false) => "none",
    }
}

fn toggle(id: &'static str, label: &'static str, on: bool) -> Value {
    json!({"id": id, "label": label, "on": on, "enabled": true})
}

fn area_label(id: &str) -> &'static str {
    AREAS.iter().find(|(a, _)| *a == id).map_or("Screen or window", |(_, l)| l)
}

fn send_to_label() -> &'static str {
    if super::settings::copy_capture_to_clipboard().unwrap_or(false) {
        "Clipboard"
    } else {
        "None"
    }
}

fn view_json() -> String {
    let busy = super::shared::busy();
    with(|s| {
        let image = s.tab != 2;
        let recording = super::control::recording_active();
        let interval = image && s.area.ends_with("screen") && s.interval > 0;
        let (verb, detail) = if busy {
            ("Stop", "Finish the running capture".to_owned())
        } else if image {
            let delay = if s.delay == 0 {
                String::new()
            } else {
                format!("After {} s", s.delay)
            };
            match s.area.as_str() {
                "scrolling" | "scrolling-h" | "panoramic" => ("Start", "Then press Stop to finish".to_owned()),
                "text" => ("Read", "Copy the text found".to_owned()),
                _ if interval => ("Start", format!("Every {} s", s.interval)),
                _ => ("Capture", delay),
            }
        } else if recording {
            ("Stop", "Finish and open the video".to_owned())
        } else {
            ("Record", "Screen through the desktop dialog".to_owned())
        };
        let toggles = if image {
            vec![toggle("image:switch:cursor", "Include the cursor", s.cursor)]
        } else {
            vec![
                toggle("video:switch:record-cursor", "Record the cursor", s.cursor),
                toggle("video:switch:system-audio", "System audio", s.system_audio),
                toggle("video:switch:webcam", "Webcam", s.webcam),
                toggle("video:switch:microphone", "Microphone", s.microphone),
            ]
        };
        json!({
            "tab": s.tab,
            "timing": "manual",
            "interval_running": false,
            "active_schedule": null,
            "fields": if image {
                vec![
                    json!({"id": "selection", "label": "Area", "text": area_label(&s.area), "gear": null}),
                    json!({"id": "sendto", "label": "Send to", "text": send_to_label(), "gear": null}),
                ]
            } else {
                vec![json!({"id": "selection", "label": "Area", "text": "Screen", "gear": null})]
            },
            "capture": {"verb": verb, "detail": detail, "hint": ""},
            "widget": false,
            "toggles": toggles,
        })
        .to_string()
    })
}

fn publish_view() {
    if let Some(sink) = with(|s| s.sink) {
        let _ = sink.set_view(view_json());
    }
}

fn send(set: impl Fn(&CaptureViewModelSink, String) -> Result<()>, text: String) {
    let (sink, serial) = with(|s| {
        s.serial += 1;
        (s.sink, s.serial)
    });
    if let Some(sink) = sink {
        let _ = set(&sink, format!("{serial}\u{1f}{text}"));
    }
}

fn menu(items: Vec<Value>) {
    send(|sink, value| sink.set_menu(value), Value::Array(items).to_string());
}

fn entry(id: &str, label: &str, checked: bool) -> Value {
    json!({"id": id, "label": label, "checked": checked, "enabled": true})
}

fn open_menu(name: &str) {
    match name {
        "selection" => {
            let (image, area) = with(|s| (s.tab != 2, s.area.clone()));
            if image {
                menu(AREAS.iter().map(|(id, label)| entry(&format!("area:{id}"), label, *id == area)).collect());
            } else {
                menu(vec![entry("area:screen", "Screen (desktop dialog)", true)]);
            }
        }
        "more" => {
            let (delay, interval, busy) = with(|s| (s.delay, s.interval, super::shared::busy()));
            let delays: Vec<Value> = DELAYS
                .iter()
                .map(|&n| {
                    let label = if n == 0 { "No delay".to_owned() } else { format!("{n} seconds") };
                    entry(&format!("delay:{n}"), &label, delay == n)
                })
                .collect();
            let intervals: Vec<Value> = INTERVALS
                .iter()
                .map(|&n| {
                    let label = if n == 0 { "Off".to_owned() } else { format!("Every {n} seconds") };
                    entry(&format!("interval:{n}"), &label, interval == n)
                })
                .collect();
            let schedule = super::settings::schedule_preferences().ok();
            let times: Vec<Value> = std::iter::once(entry("schedule:off", "Off", !schedule.is_some_and(|p| p.enabled)))
                .chain(["09:00", "12:00", "15:00", "18:00"].map(|t| {
                    let on = schedule.is_some_and(|p| p.enabled && p.time.to_string() == t);
                    entry(&format!("schedule:{t}"), &format!("Every day at {t}"), on)
                }))
                .collect();
            let presets = super::settings::screen_capture_presets().unwrap_or_default();
            let mut preset_items: Vec<Value> = vec![entry("preset-save", "Save the current delay and cursor", false)];
            for preset in &presets {
                preset_items.push(entry(&format!("preset:{}", preset.name), &preset.name, false));
            }
            for preset in &presets {
                preset_items.push(entry(&format!("preset-del:{}", preset.name), &format!("Delete {}", preset.name), false));
            }
            let mut items = vec![
                json!({"id": "", "label": "Delay", "enabled": true, "items": delays}),
                json!({"id": "", "label": "Interval capture", "enabled": true, "items": intervals}),
                json!({"id": "", "label": "Daily capture", "enabled": true, "items": times}),
                json!({"id": "", "label": "Presets", "enabled": true, "items": preset_items}),
            ];
            if super::control::recording_active() {
                items.push(entry("pause", "Pause or resume the recording", false));
            }
            if busy {
                items.push(entry("cancel", "Cancel the running capture", false));
            }
            menu(items);
        }
        "file" => menu(vec![
            entry("open-editor", "Open the editor", false),
            entry("close", "Close", false),
        ]),
        "sendto" => {
            let copy = super::settings::copy_capture_to_clipboard().unwrap_or(false);
            menu(vec![entry("sendto:none", "None", !copy), entry("sendto:clipboard", "Clipboard", copy)]);
        }
        "edit" => menu(vec![entry("prefs", "Preferences…", false)]),
        "view" => menu(vec![entry("open-editor", "Open the editor", false)]),
        "help" => menu(vec![entry("website", "Jiffshot on GitHub", false)]),
        _ => {}
    }
}

fn pick(id: &str) {
    match id {
        "image:switch:cursor" | "video:switch:record-cursor" => with(|s| s.cursor = !s.cursor),
        "video:switch:system-audio" => with(|s| s.system_audio = !s.system_audio),
        "video:switch:webcam" => with(|s| s.webcam = !s.webcam),
        "video:switch:microphone" => with(|s| s.microphone = !s.microphone),
        "open-editor" => crate::app::post(|app| app.show_editor()),
        "close" => hide(),
        "cancel" => super::shared::finish_active(true),
        "sendto:none" | "sendto:clipboard" => {
            if let Err(error) = super::settings::set_copy_capture_to_clipboard(id == "sendto:clipboard") {
                super::shared::report(format!("Could not save the setting: {error}"));
            }
        }
        "prefs" => crate::app::post(|app| {
            app.show_editor();
            app.open_prefs("capture");
        }),
        "website" => {
            let _ = std::process::Command::new("xdg-open").arg("https://github.com/Devolutions/jiffshot").spawn();
        }
        "pause" => {
            let _ = super::control::record_pause();
        }
        "preset-save" => {
            let (delay, cursor) = with(|s| (s.delay, s.cursor));
            let name = format!("{delay} s delay, cursor {}", if cursor { "on" } else { "off" });
            let included = super::capture::CaptureCursor::from_included(cursor);
            if let Err(error) = super::settings::save_screen_capture_preset(&name, delay, included) {
                super::shared::report(format!("Could not save the preset: {error}"));
            }
        }
        _ if id.starts_with("preset-del:") => {
            let _ = super::settings::delete_screen_capture_preset(&id["preset-del:".len()..]);
        }
        _ if id.starts_with("preset:") => {
            if let Ok(preset) = super::settings::screen_capture_preset(&id["preset:".len()..]) {
                with(|s| {
                    s.delay = preset.delay_seconds();
                    s.cursor = matches!(preset.cursor(), super::capture::CaptureCursor::Included);
                });
            }
        }
        _ if id.starts_with("schedule:") => {
            let current = super::settings::schedule_preferences().map(|p| p.time).unwrap_or(super::settings::SchedulePreferences::default().time);
            let result = match id["schedule:".len()..].parse::<super::schedule::ScheduleTime>() {
                Ok(time) => super::settings::set_schedule_preferences(time, true),
                Err(_) => super::settings::set_schedule_preferences(current, false),
            };
            if let Err(error) = result {
                super::shared::report(format!("Could not save the schedule: {error}"));
            }
        }
        _ => {
            if let Some(n) = id.strip_prefix("delay:").and_then(|n| n.parse().ok()) {
                with(|s| s.delay = n);
            } else if let Some(n) = id.strip_prefix("interval:").and_then(|n| n.parse().ok()) {
                with(|s| s.interval = n);
            } else if let Some(area) = id.strip_prefix("area:") {
                with(|s| s.area = area.to_owned());
            }
        }
    }
    save();
    publish_view();
}

fn act(action: &str) {
    let (verb, arg) = action.split_once(':').unwrap_or((action, ""));
    match verb {
        "tab" => {
            let tab = arg.parse::<usize>().unwrap_or(1);
            with(|s| {
                s.tab = if tab == 2 { 2 } else { 1 };
                if tab == 0 {
                    s.area = "region".to_owned();
                }
            });
            save();
            publish_view();
        }
        "menu" => open_menu(arg),
        "pick" => pick(arg),
        "capture" => {
            if super::shared::busy() {
                super::shared::finish_active(false);
                publish_view();
                return;
            }
            let (video, cursor, delay, audio, area, interval, webcam) =
                with(|s| (s.tab == 2, s.cursor, s.delay, audio_name(s), s.area.clone(), s.interval, s.webcam));
            let long_running = !video && (area.contains("scrolling") || area == "panoramic" || (area.ends_with("screen") && interval > 0));
            if !long_running {
                hide();
            }
            if video {
                super::shared::toggle_recording_with(audio, cursor, webcam);
                return;
            }
            use super::overlay::Mode;
            use super::scroll::{Direction, Mode as ScrollMode};
            match area.as_str() {
                "region" => super::shared::capture_overlay(Mode::Normal, false, delay),
                "freehand" => super::shared::capture_overlay(Mode::Freehand, false, delay),
                "multi" => super::shared::capture_overlay(Mode::MultiArea, false, delay),
                "fixed" => super::shared::capture_overlay(Mode::Fixed(1280, 720), false, delay),
                "repeat" => super::shared::capture_overlay(Mode::Normal, true, delay),
                "text" => super::shared::capture_text(),
                "scrolling" => super::shared::capture_scrolling(ScrollMode::Scrolling(Direction::Vertical)),
                "scrolling-h" => super::shared::capture_scrolling(ScrollMode::Scrolling(Direction::Horizontal)),
                "panoramic" => super::shared::capture_scrolling(ScrollMode::Panoramic),
                "fullscreen" if interval == 0 => super::shared::capture_full_screen(cursor, delay),
                _ if interval > 0 => super::shared::capture_interval(interval, cursor),
                _ => super::shared::capture_image_with(cursor, delay),
            }
            publish_view();
        }
        "editor" => crate::app::post(|app| app.show_editor()),
        _ => {}
    }
}

pub struct CaptureModel;

impl CaptureViewModel for CaptureModel {
    fn attach(&mut self, sink: CaptureViewModelSink) -> Result<()> {
        with(|s| s.sink = Some(sink));
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
