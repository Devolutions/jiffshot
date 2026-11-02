//! The Capture window on Linux: the shared Capture view driven by a small
//! model over the portal capture and recording engines. The view speaks the
//! JSON of the Windows panel (`tab:N`, `menu:ID`, `pick:ID`, `capture`).

use crate::vm::{CaptureViewModel, CaptureViewModelSink};
use rustolonia::{Result, Window};
use serde_json::{json, Value};
use std::cell::RefCell;

const DELAYS: [u64; 4] = [0, 3, 5, 10];

struct State {
    sink: Option<CaptureViewModelSink>,
    window: Option<Window>,
    /// 1 captures images, 2 records video.
    tab: usize,
    cursor: bool,
    delay: u64,
    system_audio: bool,
    microphone: bool,
    serial: u64,
}

thread_local! {
    static STATE: RefCell<State> = const { RefCell::new(State {
        sink: None,
        window: None,
        tab: 1,
        cursor: true,
        delay: 0,
        system_audio: false,
        microphone: false,
        serial: 0,
    }) };
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

/// Remembers the window mounted for this model (hidden until shown).
pub fn set_window(window: Window) {
    with(|s| s.window = Some(window));
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

fn view_json() -> String {
    with(|s| {
        let image = s.tab != 2;
        let recording = super::control::recording_active();
        let delay = if s.delay == 0 {
            String::new()
        } else {
            format!("After {} s", s.delay)
        };
        let (verb, detail) = if image {
            ("Capture", delay)
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
                toggle("video:switch:microphone", "Microphone", s.microphone),
            ]
        };
        json!({
            "tab": s.tab,
            "timing": "manual",
            "interval_running": false,
            "active_schedule": null,
            "fields": [{
                "id": "selection",
                "label": "Area",
                "text": if image { "Screen or window" } else { "Screen" },
                "gear": null,
            }],
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
        "selection" => menu(vec![entry(
            "area:portal",
            "Screen or window (desktop dialog)",
            true,
        )]),
        "more" => {
            let delay = with(|s| s.delay);
            menu(
                DELAYS
                    .iter()
                    .map(|&n| {
                        let label = if n == 0 {
                            "No delay".to_owned()
                        } else {
                            format!("Delay {n} seconds")
                        };
                        entry(&format!("delay:{n}"), &label, delay == n)
                    })
                    .collect(),
            );
        }
        "file" => menu(vec![
            entry("open-editor", "Open the editor", false),
            entry("close", "Close", false),
        ]),
        _ => {}
    }
}

fn pick(id: &str) {
    match id {
        "image:switch:cursor" | "video:switch:record-cursor" => with(|s| s.cursor = !s.cursor),
        "video:switch:system-audio" => with(|s| s.system_audio = !s.system_audio),
        "video:switch:microphone" => with(|s| s.microphone = !s.microphone),
        "open-editor" => crate::app::post(|app| app.show_editor()),
        "close" => hide(),
        _ => {
            if let Some(n) = id.strip_prefix("delay:").and_then(|n| n.parse().ok()) {
                with(|s| s.delay = n);
            }
        }
    }
    publish_view();
}

fn act(action: &str) {
    let (verb, arg) = action.split_once(':').unwrap_or((action, ""));
    match verb {
        "tab" => {
            let tab = arg.parse::<usize>().unwrap_or(1);
            with(|s| s.tab = if tab == 2 { 2 } else { 1 });
            publish_view();
        }
        "menu" => open_menu(arg),
        "pick" => pick(arg),
        "capture" => {
            let (video, cursor, delay, audio) = with(|s| (s.tab == 2, s.cursor, s.delay, audio_name(s)));
            hide();
            if video {
                super::shared::toggle_recording_with(audio, cursor);
            } else {
                super::shared::capture_image_with(cursor, delay);
            }
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
