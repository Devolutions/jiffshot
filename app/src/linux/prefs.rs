//! The Preferences dialog on Linux: the shared dialog's JSON rows, applied to
//! the Linux settings.

use super::settings;
use super::shortcut::{RecordingStartShortcut, ShortcutOptions};
use crate::app::App;
use serde_json::{json, Value};

const THEMES: [&str; 3] = ["Dark", "Light", "System"];
const FORMATS: [(&str, &str); 7] = [
    ("The one used last", ""),
    ("PNG", "png"),
    ("JPEG", "jpg"),
    ("PDF", "pdf"),
    ("BMP", "bmp"),
    ("GIF", "gif"),
    ("WebP", "webp"),
];

fn check(label: &str, value: bool) -> Value {
    json!({"kind": "check", "label": label, "value": value})
}

fn choice(label: &str, choices: &[&str], value: usize) -> Value {
    json!({"kind": "choice", "label": label, "choices": choices, "value": value})
}

fn note(text: &str) -> Value {
    json!({"kind": "note", "text": text})
}

fn on(values: &[Value], i: usize) -> bool {
    values.get(i).and_then(Value::as_bool).unwrap_or(false)
}

fn index(values: &[Value], i: usize, max: usize) -> usize {
    (values.get(i).and_then(Value::as_u64).unwrap_or(0) as usize).min(max)
}

fn text(values: &[Value], i: usize) -> String {
    values.get(i).and_then(Value::as_str).unwrap_or("").trim().to_owned()
}

/// The dialog's JSON, opening on `tab`.
pub fn dialog(app: &App, tab: &str) -> String {
    let s = &app.settings;
    let theme = THEMES.iter().position(|t| Some(*t) == s.theme.as_deref()).unwrap_or(0);
    let format = FORMATS
        .iter()
        .position(|(_, ext)| *ext == s.export_format.as_deref().unwrap_or(""))
        .unwrap_or(0);
    let start: Vec<&str> = RecordingStartShortcut::ALL.iter().map(|c| c.label()).collect();
    let start_index = settings::global_recording_start().map_or(0, RecordingStartShortcut::index) as usize;
    json!({
        "tab": tab,
        "tabs": [
            {"id": "capture", "title": "Capture", "rows": [
                check("Show a tray icon", settings::tray_enabled().unwrap_or(false)),
                check("Copy each capture to the clipboard", settings::copy_capture_to_clipboard().unwrap_or(false)),
                note("The tray icon applies the next time Jiffshot starts."),
            ]},
            {"id": "editor", "title": "Editor", "rows": [
                choice("Color theme (every Jiffshot window)", &THEMES, theme),
                check("Show tool names in the toolbar", s.layout.labels),
                check("Show the side bar (Quick Styles and properties)", s.layout.side_bar),
                check("Show the Recent Captures tray", s.layout.tray),
                choice("Export as", &FORMATS.map(|(label, _)| label), format),
            ]},
            {"id": "library", "title": "Library", "rows": [
                {"kind": "folder", "label": "Library folder", "value": app.library.root.display().to_string()},
                note("New captures are saved here, and the editor lists them."),
            ]},
            {"id": "hotkeys", "title": "Hotkeys", "rows": [
                check("Enable global shortcuts (through the desktop portal)", settings::global_shortcuts_enabled().unwrap_or(false)),
                {"kind": "text", "label": "Preferred capture shortcut", "value": settings::global_capture_shortcut().unwrap_or_default()},
                check("Recording pause and stop shortcuts", settings::global_recording_shortcuts().unwrap_or(false)),
                choice("Recording start shortcut", &start, start_index),
                check("Capture preset shortcuts (Ctrl+Alt+1 to 9 for the first presets)", settings::global_preset_shortcuts().unwrap_or(false)),
                note("The desktop may ask you to confirm or change the keys; shortcut changes apply the next time Jiffshot starts."),
            ]},
        ],
    })
    .to_string()
}

/// Applies the dialog's values (`{"capture": [...], "editor": [...], …}`).
pub fn apply(app: &mut App, json: &str) {
    let Ok(Value::Object(tabs)) = serde_json::from_str::<Value>(json) else { return };
    let values = |id: &str| -> Vec<Value> { tabs.get(id).and_then(Value::as_array).cloned().unwrap_or_default() };
    let mut errors = Vec::new();

    let capture = values("capture");
    if !capture.is_empty() {
        errors.extend(settings::set_tray_enabled(on(&capture, 0)).err());
        errors.extend(settings::set_copy_capture_to_clipboard(on(&capture, 1)).err());
    }

    let editor = values("editor");
    if !editor.is_empty() {
        let theme = THEMES[index(&editor, 0, 2)];
        if app.settings.theme.as_deref() != Some(theme) {
            app.run(&format!("theme:{theme}"));
        }
        let (labels, side_bar, tray) = (on(&editor, 1), on(&editor, 2), on(&editor, 3));
        app.layout(|l| {
            l.labels = labels;
            l.side_bar = side_bar;
            l.tray = tray;
        });
        let format = FORMATS[index(&editor, 4, FORMATS.len() - 1)].1;
        if app.settings.export_format.as_deref().unwrap_or("") != format {
            app.run(&format!("export-format:{format}"));
        }
    }

    let folder = text(&values("library"), 0);
    if !folder.is_empty() && std::path::Path::new(&folder) != app.library.root {
        app.run(&format!("library-folder:{folder}"));
    }

    let hotkeys = values("hotkeys");
    if !hotkeys.is_empty() {
        let result = if on(&hotkeys, 0) {
            let start = RecordingStartShortcut::ALL[index(&hotkeys, 3, RecordingStartShortcut::ALL.len() - 1)];
            let options = ShortcutOptions { recording_controls: on(&hotkeys, 2), recording_start: start, preset_hotkeys: on(&hotkeys, 4) };
            settings::set_global_shortcuts(&text(&hotkeys, 1), options).map(|_| ())
        } else {
            settings::disable_global_shortcuts()
        };
        errors.extend(result.err());
    }

    match errors.first() {
        Some(error) => app.set_status(format!("Could not apply all Preferences: {error}")),
        None => app.set_status("Preferences saved"),
    }
}
