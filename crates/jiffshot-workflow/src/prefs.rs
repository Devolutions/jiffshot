//! The capture side's tabs of the Preferences dialog, Capture and Hotkeys:
//! their rows (as in the Capture window's panels, see `panel.rs`) and how
//! the values the dialog sends back change the settings.

use crate::panel::{check_row, choice, int, note, number, on, string, text, Row, FPS};
use crate::settings::Settings;
use serde_json::Value;

const AUDIO: [(&str, &str); 4] = [
    ("system", "What the computer plays"),
    ("microphone", "The microphone"),
    ("both", "Both, mixed"),
    ("none", "No sound"),
];

/// The Capture tab.
pub fn capture(s: &Settings, runs_at_startup: bool) -> Vec<Row> {
    let fps = FPS.iter().position(|&f| f == s.record_fps).unwrap_or(2);
    let audio = AUDIO
        .iter()
        .position(|(v, _)| *v == s.record_audio)
        .unwrap_or(0);
    vec![
        check_row(
            if cfg!(target_os = "macos") {
                "Run Jiffshot at login"
            } else {
                "Run Jiffshot when Windows starts"
            },
            runs_at_startup,
        ),
        check_row(
            "Show the Capture Widget at the top of the screen",
            s.show_widget,
        ),
        check_row("Copy captures to the clipboard", s.copy_to_clipboard),
        check_row("Open captures in the editor", s.open_in_editor),
        check_row(
            "Keep the pointer as a movable object in the editor",
            s.cursor_as_object,
        ),
        number("Time delay (s)", s.delay_seconds as i64, 0, 60),
        number("Recording countdown (s)", s.countdown as i64, 0, 10),
        choice("Recording frame rate", &["15", "24", "30", "60"], fps),
        choice("Recording sound", &AUDIO.map(|(_, label)| label), audio),
        check_row("Draw the pointer into recordings", s.record_cursor),
        check_row("Offer Screen Draw while recording", s.screen_draw),
        number(
            "Screen Draw marks fade after (s, 0 keeps them)",
            s.draw_fade_seconds as i64,
            0,
            60,
        ),
    ]
}

/// Applies the Capture tab; returns the Run-at-startup choice when it changed.
pub fn apply_capture(s: &mut Settings, values: &[Value], runs_at_startup: bool) -> Option<bool> {
    let clamp = |i: usize, min: i64, max: i64| int(values, i).clamp(min, max);
    s.show_widget = on(values, 1);
    s.copy_to_clipboard = on(values, 2);
    s.open_in_editor = on(values, 3);
    s.cursor_as_object = on(values, 4);
    s.delay_seconds = clamp(5, 0, 60) as u32;
    s.countdown = clamp(6, 0, 10) as u32;
    s.record_fps = FPS[clamp(7, 0, 3) as usize];
    s.record_audio = AUDIO[clamp(8, 0, 3) as usize].0.to_owned();
    s.record_cursor = on(values, 9);
    s.screen_draw = on(values, 10);
    s.draw_fade_seconds = clamp(11, 0, 60) as u32;
    (on(values, 0) != runs_at_startup).then(|| on(values, 0))
}

/// The Hotkeys tab: configurable global shortcuts and a hotkey per preset.
pub fn hotkeys(s: &Settings, capture_key: &str) -> Vec<Row> {
    let key = crate::panel::key_name(capture_key);
    let mut rows = vec![
        note(format!("{key}: current capture key; another key may be used while the configured key is unavailable.")),
        text("Capture", s.capture_hotkey.clone().unwrap_or_default()),
        text("Full screen", s.screen_hotkey.clone().unwrap_or_default()),
        text("Repeat last region", s.repeat_hotkey.clone().unwrap_or_default()),
        note(format!(
            "Leave these blank for the defaults: {}, {}, {}.",
            crate::keys::display(crate::keys::CAPTURE.2),
            crate::keys::display(crate::keys::SCREEN.2),
            crate::keys::display(crate::keys::REPEAT.2),
        )),
        note(format!("While recording: {}", crate::recorder::keys_text(", "))),
        note(format!("Presets: a hotkey for each, for example {}; empty for none.", crate::keys::EXAMPLE)),
    ];
    rows.extend(
        s.presets
            .iter()
            .map(|p| text(&p.name, p.hotkey.clone().unwrap_or_default())),
    );
    if s.presets.is_empty() {
        rows.push(note(
            "No presets yet; the Capture window's Presets menu makes them.",
        ));
    }
    rows
}

/// Checks configured global and preset hotkeys as one set.
pub fn validate_hotkeys(settings: &Settings) -> Result<(), String> {
    fn checked(key: &str, label: &str) -> Result<(crate::sys::HOT_KEY_MODIFIERS, u16), String> {
        let parsed = crate::keys::parse(key)
            .filter(|(_, vk)| crate::keys::supported(*vk))
            .ok_or_else(|| {
                format!(
                    "“{key}” (for {label}) is not a supported hotkey; try {}.",
                    crate::keys::EXAMPLE
                )
            })?;
        if crate::keys::reserved(parsed.0, parsed.1) {
            return Err(format!("“{key}” is reserved for macOS screenshots."));
        }
        Ok(parsed)
    }

    let mut seen = std::collections::HashMap::new();
    for (label, key) in [
        ("capture", Some(settings.capture_key())),
        ("full screen", Some(settings.screen_key())),
        ("repeat last region", Some(settings.repeat_key())),
    ]
    .into_iter()
    .chain(
        settings
            .presets
            .iter()
            .map(|preset| (preset.name.as_str(), preset.hotkey.as_deref())),
    ) {
        let Some(key) = key else { continue };
        let parsed = checked(key, label)?;
        if let Some(previous) = seen.insert(parsed, label) {
            return Err(format!("“{key}” is used by both {previous} and {label}."));
        }
    }
    Ok(())
}

/// Validates the whole tab before changing any setting.
pub fn apply_hotkeys(settings: &mut Settings, values: &[Value]) -> Result<(), String> {
    let mut updated = settings.clone();
    let optional = |index| {
        let value = string(values, index);
        (!value.is_empty()).then_some(value)
    };
    updated.capture_hotkey = optional(1);
    updated.screen_hotkey = optional(2);
    updated.repeat_hotkey = optional(3);
    for (i, preset) in updated.presets.iter_mut().enumerate() {
        preset.hotkey = optional(7 + i);
    }
    validate_hotkeys(&updated)?;
    *settings = updated;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_capture_tab_round_trips_and_reports_a_startup_change() {
        let mut s = Settings::default();
        let rows = capture(&s, false);
        let values: Vec<Value> = rows
            .iter()
            .map(|r| match r {
                Row::Check { value, .. } => json!(value),
                Row::Number { value, .. } => json!(value),
                Row::Choice { value, .. } => json!(value),
                _ => Value::Null,
            })
            .collect();
        let before = serde_json::to_value(&s).unwrap();
        assert_eq!(apply_capture(&mut s, &values, false), None);
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            before,
            "unchanged values change nothing"
        );
        let mut changed = values.clone();
        changed[0] = json!(true);
        changed[7] = json!(3);
        changed[8] = json!(1);
        assert_eq!(apply_capture(&mut s, &changed, false), Some(true));
        assert_eq!((s.record_fps, s.record_audio.as_str()), (60, "microphone"));
    }

    #[test]
    fn unchanged_startup_choice_uses_the_dialogs_original_status() {
        let mut settings = Settings::default();
        let values = [json!(false)];
        assert_eq!(apply_capture(&mut settings, &values, false), None);
        assert_eq!(apply_capture(&mut settings, &values, true), Some(false));
    }

    #[test]
    fn preset_hotkeys_are_checked() {
        let mut settings = Settings::default();
        let with = |keys: &[&str]| {
            std::iter::repeat_n(Value::Null, 7)
                .chain(keys.iter().map(|k| json!(k)))
                .collect::<Vec<_>>()
        };
        apply_hotkeys(&mut settings, &with(&["Ctrl+Shift+1", ""])).unwrap();
        assert_eq!(settings.presets[0].hotkey.as_deref(), Some("Ctrl+Shift+1"));
        assert_eq!(settings.presets[1].hotkey, None);
        let before = serde_json::to_value(&settings).unwrap();
        assert!(apply_hotkeys(&mut settings, &with(&["Banana", ""])).is_err());
        assert!(apply_hotkeys(&mut settings, &with(&["Ctrl+Shift+1", "shift+control+1"])).is_err());
        assert_eq!(serde_json::to_value(&settings).unwrap(), before);
    }

    #[test]
    fn global_hotkeys_are_editable_and_checked_against_presets() {
        let mut settings = Settings::default();
        let mut values = vec![Value::Null; 7 + settings.presets.len()];
        values[1] = json!("Ctrl+Alt+9");
        values[2] = json!("Ctrl+Alt+8");
        values[3] = json!("Ctrl+Alt+7");
        values[7] = json!("Shift+F9");
        apply_hotkeys(&mut settings, &values).unwrap();
        assert_eq!(settings.capture_key(), "Ctrl+Alt+9");
        assert_eq!(settings.screen_key(), "Ctrl+Alt+8");
        assert_eq!(settings.repeat_key(), "Ctrl+Alt+7");
        values[7] = json!("Alt+Control+9");
        assert!(apply_hotkeys(&mut settings, &values).is_err());
        #[cfg(target_os = "macos")]
        {
            values[7] = Value::Null;
            values[2] = json!("Cmd+Shift+3");
            assert!(apply_hotkeys(&mut settings, &values).is_err());
            values[2] = json!("Ctrl+Cmd+Shift+4");
            assert!(apply_hotkeys(&mut settings, &values).is_err());
        }
    }
}
