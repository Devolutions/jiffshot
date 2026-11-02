//! Capture preferences: the `capture` section of Jiffshot's settings file
//! (see `jiffshot_capture::config`). The library folder is shared with the
//! editor and lives there too.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A macOS saved rectangle and the exact reference grid on which it was reviewed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SavedAreaConfirmation {
    pub version: u32,
    pub kind: String,
    pub rect: jiffshot_capture::Rect,
    pub coordinate_space: Vec<u64>,
    /// A missing display UUID permits use only until the current process exits.
    #[serde(default)]
    pub session_only: bool,
}

fn deserialize_confirmation<'de, D>(deserializer: D, label: &str) -> Result<Option<SavedAreaConfirmation>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let Some(value) = Option::<serde_json::Value>::deserialize(deserializer)? else { return Ok(None) };
    match serde_json::from_value(value) {
        Ok(confirmation) => Ok(Some(confirmation)),
        Err(error) => {
            eprintln!("Malformed macOS {label} saved-area confirmation; the coordinates require review: {error}");
            Ok(None)
        }
    }
}

fn deserialize_fixed_confirmation<'de, D>(deserializer: D) -> Result<Option<SavedAreaConfirmation>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_confirmation(deserializer, "fixed")
}

fn deserialize_schedule_confirmation<'de, D>(deserializer: D) -> Result<Option<SavedAreaConfirmation>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_confirmation(deserializer, "scheduled")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub copy_to_clipboard: bool,
    pub open_in_editor: bool,
    pub include_cursor: bool,
    pub delay_seconds: u32,
    /// Path to `jiffshot.exe`; `None` searches next to this
    /// executable and in the development tree.
    pub editor: Option<PathBuf>,
    /// Recording audio: "system" (what the speakers play), "microphone",
    /// "both" (mixed) or "none".
    pub record_audio: String,
    pub record_fps: u32,
    /// Silence the microphone between words.
    pub noise_gate: bool,
    /// Seconds counted down before recording starts (0: none).
    pub countdown: u32,
    pub cursor_highlight: bool,
    pub click_rings: bool,
    /// Webcam picture-in-picture corner ("bottom-right", …); off when unset.
    pub webcam: Option<String>,
    /// "circle" or "rectangle".
    pub webcam_shape: String,
    /// Height of the webcam picture as a share of the video's.
    pub webcam_size: f32,
    /// Screen Draw marks fade after this many seconds (0: they stay).
    pub draw_fade_seconds: u32,
    /// Size of a fixed-size capture.
    pub fixed_size: (i32, i32),
    /// Seconds between time-lapse captures.
    pub interval_seconds: u32,
    /// Time-lapse skips a capture identical to the one before.
    pub interval_skip_identical: bool,
    /// The pointer is drawn into recordings.
    pub record_cursor: bool,
    /// The recording panel offers Screen Draw.
    pub screen_draw: bool,
    /// Keep the pointer as a separate, movable object in the editor
    /// instead of drawing it into the image.
    pub cursor_as_object: bool,
    /// Effects applied to every image capture, by name (as in the editor's
    /// Effects list).
    pub capture_effects: Vec<String>,
    pub presets: Vec<Preset>,
    /// Global hotkeys; absent values use the platform defaults.
    pub capture_hotkey: Option<String>,
    pub screen_hotkey: Option<String>,
    pub repeat_hotkey: Option<String>,
    pub show_widget: bool,
    /// Daily capture time, "HH:MM", when on.
    pub schedule: Option<String>,
    /// The time the Scheduled capture menu item turns on.
    pub schedule_default: Option<String>,
    /// Area for scheduled captures (x, y, width, height); the screen under
    /// the pointer when unset.
    pub schedule_area: Option<(i32, i32, i32, i32)>,
    /// macOS reference-grid confirmations. Windows preserves but does not use them.
    #[serde(default, deserialize_with = "deserialize_fixed_confirmation", skip_serializing_if = "Option::is_none")]
    pub macos_fixed_area: Option<SavedAreaConfirmation>,
    #[serde(default, deserialize_with = "deserialize_schedule_confirmation", skip_serializing_if = "Option::is_none")]
    pub macos_schedule_area: Option<SavedAreaConfirmation>,
    /// Where image captures go after saving: "none", "file", "clipboard",
    /// "email", "word", "powerpoint", "excel", "onenote", "paint",
    /// "program" or "print".
    pub share: String,
    /// The same, for recordings.
    pub video_share: String,
    pub capture_window: CaptureWindow,
}

/// What the Capture window shows and remembers between uses.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureWindow {
    /// 0 All-in-One, 1 Image, 2 Video.
    pub tab: usize,
    /// The Image tab's selection, as a preset mode name.
    pub selection: String,
    /// The Video tab's selection: "region", "window" or "fullscreen".
    pub video_selection: String,
    /// A Region selection is a fixed-size rectangle (`Settings::fixed_size`).
    pub fixed: bool,
    /// Where a fixed-size capture goes, when it is placed exactly.
    pub fixed_at: Option<(i32, i32)>,
    /// The Time Delay switch is on.
    pub timer: bool,
    /// What it does: "delay", "interval" (time-lapse) or "schedule".
    pub timing: String,
    /// The delay it turns on, in seconds.
    pub delay: u32,
    /// The corner the Record Webcam switch puts the webcam in.
    pub webcam_corner: String,
}

impl Default for CaptureWindow {
    fn default() -> CaptureWindow {
        CaptureWindow {
            tab: 1,
            selection: "image".to_owned(),
            video_selection: "region".to_owned(),
            fixed: false,
            fixed_at: None,
            timer: false,
            timing: "delay".to_owned(),
            delay: 5,
            webcam_corner: "bottom-right".to_owned(),
        }
    }
}

/// A saved capture setup, run from the tray's Presets menu or its hotkey.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preset {
    pub name: String,
    /// all (All-in-One), image, window, fullscreen, scrolling, horizontal,
    /// both, panoramic, freehand, multi, fixed, menu, text, clipboard,
    /// file, video, video-fullscreen or steps.
    pub mode: String,
    pub delay: u32,
    pub cursor: bool,
    /// For example "Ctrl+Shift+1".
    pub hotkey: Option<String>,
    pub effects: Vec<String>,
    /// A share destination for this preset's captures (see
    /// [`Settings::share`]); the window's choice when unset.
    pub share: Option<String>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            copy_to_clipboard: true,
            open_in_editor: true,
            include_cursor: false,
            delay_seconds: 3,
            editor: None,
            record_audio: "system".to_owned(),
            record_fps: 30,
            noise_gate: true,
            countdown: 3,
            cursor_highlight: false,
            click_rings: true,
            webcam: None,
            webcam_shape: "circle".to_owned(),
            webcam_size: 0.25,
            draw_fade_seconds: 3,
            fixed_size: (1280, 720),
            interval_seconds: 10,
            interval_skip_identical: true,
            record_cursor: true,
            screen_draw: true,
            cursor_as_object: true,
            capture_effects: Vec::new(),
            presets: vec![
                Preset {
                    name: "Full screen in 3 s".into(),
                    mode: "image".into(),
                    delay: 3,
                    cursor: true,
                    hotkey: None,
                    effects: Vec::new(),
                    share: None,
                },
                Preset {
                    name: "Window with border".into(),
                    mode: "image".into(),
                    delay: 0,
                    cursor: false,
                    hotkey: None,
                    effects: vec!["Border".into()],
                    share: None,
                },
            ],
            capture_hotkey: None,
            screen_hotkey: None,
            repeat_hotkey: None,
            show_widget: false,
            schedule: None,
            schedule_default: None,
            schedule_area: None,
            macos_fixed_area: None,
            macos_schedule_area: None,
            share: "none".to_owned(),
            video_share: "none".to_owned(),
            capture_window: CaptureWindow::default(),
        }
    }
}

impl Settings {
    pub fn capture_key(&self) -> &str {
        self.capture_hotkey.as_deref().unwrap_or(crate::keys::CAPTURE.2)
    }

    pub fn screen_key(&self) -> &str {
        self.screen_hotkey.as_deref().unwrap_or(crate::keys::SCREEN.2)
    }

    pub fn repeat_key(&self) -> &str {
        self.repeat_hotkey.as_deref().unwrap_or(crate::keys::REPEAT.2)
    }

    pub fn load() -> Settings {
        serde_json::from_value(jiffshot_capture::config::section("capture")).unwrap_or_default()
    }

    pub fn save(&self) {
        if let Ok(value) = serde_json::to_value(self) {
            jiffshot_capture::config::set_section("capture", value);
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn try_save(&self) -> std::io::Result<()> {
        let value = serde_json::to_value(self).map_err(std::io::Error::other)?;
        jiffshot_capture::config::try_set_section("capture", value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_capture_settings_and_optional_coordinate_metadata_round_trip() {
        let legacy = serde_json::json!({
            "fixed_size": [640, 480],
            "capture_window": { "fixed_at": [-50, 25] },
            "schedule_area": [-50, 25, 640, 480]
        });
        let mut settings: Settings = serde_json::from_value(legacy).unwrap();
        assert_eq!(settings.capture_window.fixed_at, Some((-50, 25)));
        assert_eq!(settings.schedule_area, Some((-50, 25, 640, 480)));
        assert!(settings.macos_fixed_area.is_none());
        assert!(settings.macos_schedule_area.is_none());
        assert!(serde_json::to_value(&settings).unwrap().get("macos_fixed_area").is_none());

        let confirmation = SavedAreaConfirmation {
            version: 1,
            kind: "fixed".into(),
            rect: jiffshot_capture::Rect::new(-50, 25, 640, 480),
            coordinate_space: vec![2.0_f64.to_bits(), u64::MAX],
            session_only: false,
        };
        settings.macos_fixed_area = Some(confirmation.clone());
        let settings: Settings = serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(settings.macos_fixed_area, Some(confirmation.clone()));
        assert_eq!(settings.schedule_area, Some((-50, 25, 640, 480)));
        let mut old_proof = serde_json::to_value(&confirmation).unwrap();
        old_proof.as_object_mut().unwrap().remove("session_only");
        assert_eq!(serde_json::from_value::<SavedAreaConfirmation>(old_proof).unwrap(), confirmation);
        let mut session_only = confirmation;
        session_only.session_only = true;
        assert_eq!(serde_json::from_str::<SavedAreaConfirmation>(&serde_json::to_string(&session_only).unwrap()).unwrap(), session_only);
    }

    #[test]
    fn malformed_confirmations_do_not_discard_other_capture_settings() {
        let valid = SavedAreaConfirmation {
            version: 1,
            kind: "scheduled".into(),
            rect: jiffshot_capture::Rect::new(-50, 25, 640, 480),
            coordinate_space: vec![2.0_f64.to_bits(), u64::MAX],
            session_only: false,
        };
        for invalid in [
            serde_json::json!("corrupt"),
            serde_json::json!({}),
            serde_json::json!({"version": "1", "kind": "fixed", "rect": valid.rect, "coordinate_space": [1]}),
            serde_json::json!({"version": 1, "kind": "fixed", "rect": valid.rect, "coordinate_space": [-1]}),
            serde_json::json!({"version": 2, "kind": "fixed", "rect": valid.rect, "coordinate_space": [1], "session_only": "false"}),
            serde_json::json!({"version": 2, "kind": "fixed", "rect": valid.rect, "coordinate_space": [1], "session_only": null}),
        ] {
            let settings: Settings = serde_json::from_value(serde_json::json!({
                "copy_to_clipboard": false,
                "fixed_size": [640, 480],
                "capture_window": { "fixed_at": [-50, 25] },
                "schedule_area": [-50, 25, 640, 480],
                "macos_fixed_area": invalid,
                "macos_schedule_area": valid
            })).unwrap();
            assert!(!settings.copy_to_clipboard);
            assert_eq!(settings.fixed_size, (640, 480));
            assert_eq!(settings.capture_window.fixed_at, Some((-50, 25)));
            assert_eq!(settings.schedule_area, Some((-50, 25, 640, 480)));
            assert!(settings.macos_fixed_area.is_none());
            assert_eq!(settings.macos_schedule_area, Some(valid.clone()));
            let saved = serde_json::to_value(settings).unwrap();
            assert!(saved.get("macos_fixed_area").is_none());
            assert!(saved.get("macos_schedule_area").is_some());
        }

        let settings: Settings = serde_json::from_value(serde_json::json!({
            "fixed_size": [640, 480],
            "macos_fixed_area": valid,
            "macos_schedule_area": {"version": null}
        })).unwrap();
        assert_eq!(settings.fixed_size, (640, 480));
        assert_eq!(settings.macos_fixed_area, Some(valid));
        assert!(settings.macos_schedule_area.is_none());
    }
}
