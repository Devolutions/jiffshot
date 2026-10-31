//! Persistent Linux preferences used by the capture and editor workflows.

use crate::capture_linux::{
    CaptureCursor, RecordingFrameRate, WebcamConfiguration, WebcamCorner, WebcamLayout, WebcamShape,
};
use crate::schedule_linux::{ScheduleDay, ScheduleTime};
use crate::shortcut_linux::{RecordingStartShortcut, ShortcutOptions};
use serde_json::{Map, Value};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

const SETTINGS_FILE: &str = "settings.json";
const CAPTURE_DIRECTORY_KEY: &str = "capture_directory";
const THEME_KEY: &str = "theme";
const GLOBAL_CAPTURE_SHORTCUT_KEY: &str = "global_capture_shortcut";
const GLOBAL_RECORDING_SHORTCUTS_KEY: &str = "global_recording_shortcuts";
const GLOBAL_RECORDING_START_KEY: &str = "linux_global_recording_start";
const GLOBAL_SHORTCUTS_ENABLED_KEY: &str = "linux_global_shortcuts_enabled";
const TRAY_ENABLED_KEY: &str = "linux_tray_enabled";
const COPY_CAPTURE_TO_CLIPBOARD_KEY: &str = "copy_to_clipboard";
const AUTOMATIC_COPY_DIRECTORY_KEY: &str = "linux_auto_copy_directory";
const CAPTURE_DELAY_SECONDS_KEY: &str = "capture_delay_seconds";
const SCREEN_CAPTURE_PRESETS_KEY: &str = "linux_screen_capture_presets";
const INTERVAL_SECONDS_KEY: &str = "interval_seconds";
const INTERVAL_SKIP_IDENTICAL_KEY: &str = "interval_skip_identical";
const SCHEDULE_KEY: &str = "schedule";
const SCHEDULE_DEFAULT_KEY: &str = "schedule_default";
const SCHEDULE_LAST_FIRED_KEY: &str = "linux_schedule_last_fired";
const CAPTURE_CURSOR_KEY: &str = "capture_cursor";
const RECORDING_CURSOR_KEY: &str = "recording_cursor";
const RECORDING_FRAME_RATE_KEY: &str = "linux_recording_frame_rate";
const WEBCAM_DEVICE_KEY: &str = "webcam_device";
const WEBCAM_CORNER_KEY: &str = "webcam_corner";
const WEBCAM_SIZE_PERCENT_KEY: &str = "webcam_size_percent";
const WEBCAM_SHAPE_KEY: &str = "webcam_shape";
pub const DEFAULT_GLOBAL_CAPTURE_SHORTCUT: &str = "CTRL+SHIFT+X";

pub fn capture_directory() -> Result<PathBuf, String> {
    let default_directory = dirs::picture_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join("Pictures")))
        .map(|pictures| pictures.join("Jiffshot"))
        .ok_or_else(|| "could not find the user's Pictures folder".to_owned())?;
    capture_directory_from(&settings_path()?, &default_directory)
}

fn capture_directory_from(path: &Path, default_directory: &Path) -> Result<PathBuf, String> {
    let settings = read_settings(path)?;
    if let Some(directory) = settings
        .get(CAPTURE_DIRECTORY_KEY)
        .and_then(Value::as_str)
        .filter(|directory| !directory.is_empty())
    {
        return Ok(PathBuf::from(directory));
    }
    Ok(default_directory.to_path_buf())
}

pub fn set_capture_directory(directory: &Path) -> Result<(), String> {
    set_capture_directory_in(&settings_path()?, directory)
}

fn set_capture_directory_in(path: &Path, directory: &Path) -> Result<(), String> {
    let _lock = lock_settings(path)?;
    let mut settings = read_settings(path)?;
    settings.insert(
        CAPTURE_DIRECTORY_KEY.to_owned(),
        Value::String(directory.to_string_lossy().into_owned()),
    );
    write_settings(path, &settings)
}

pub fn theme() -> Result<String, String> {
    let settings = read_settings(&settings_path()?)?;
    Ok(theme_from_settings(&settings)?.to_owned())
}

pub fn set_theme(theme: &str) -> Result<(), String> {
    set_theme_in(&settings_path()?, theme)
}

pub fn global_capture_shortcut() -> Result<String, String> {
    global_capture_shortcut_from(&settings_path()?)
}

pub fn global_recording_shortcuts() -> Result<bool, String> {
    global_recording_shortcuts_from(&settings_path()?)
}

pub(super) fn global_recording_start() -> Result<RecordingStartShortcut, String> {
    global_recording_start_from(&settings_path()?)
}

pub(super) fn global_shortcuts_enabled() -> Result<bool, String> {
    global_shortcuts_enabled_from(&settings_path()?)
}

pub(super) fn set_global_shortcuts(
    shortcut: &str,
    options: ShortcutOptions,
) -> Result<String, String> {
    set_global_shortcuts_in(&settings_path()?, shortcut, options)
}

pub(super) fn disable_global_shortcuts() -> Result<(), String> {
    disable_global_shortcuts_in(&settings_path()?)
}

pub(super) fn tray_enabled() -> Result<bool, String> {
    tray_enabled_from(&settings_path()?)
}

pub(super) fn set_tray_enabled(enabled: bool) -> Result<(), String> {
    set_tray_enabled_in(&settings_path()?, enabled)
}

pub fn copy_capture_to_clipboard() -> Result<bool, String> {
    copy_capture_to_clipboard_from(&settings_path()?)
}

pub fn set_copy_capture_to_clipboard(enabled: bool) -> Result<(), String> {
    set_copy_capture_to_clipboard_in(&settings_path()?, enabled)
}

pub(super) fn automatic_copy_directory() -> Result<Option<PathBuf>, String> {
    automatic_copy_directory_from(&settings_path()?)
}

pub(super) fn set_automatic_copy_directory(directory: Option<&Path>) -> Result<(), String> {
    set_automatic_copy_directory_in(&settings_path()?, directory)
}

pub(super) fn capture_delay_seconds() -> Result<u64, String> {
    capture_delay_seconds_from(&settings_path()?)
}

pub(super) fn set_capture_delay_seconds(seconds: u64) -> Result<(), String> {
    set_capture_delay_seconds_in(&settings_path()?, seconds)
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(super) struct ScreenCapturePreset {
    name: String,
    delay_seconds: u64,
    include_cursor: bool,
    #[serde(flatten)]
    other: Map<String, Value>,
}

impl ScreenCapturePreset {
    fn new(name: &str, delay_seconds: u64, cursor: CaptureCursor) -> Result<Self, String> {
        let name = validate_screen_capture_preset_name(name)?;
        validate_capture_delay(delay_seconds)?;
        Ok(Self {
            name: name.to_owned(),
            delay_seconds,
            include_cursor: cursor.included(),
            other: Map::new(),
        })
    }

    fn validate(&self) -> Result<(), String> {
        if validate_screen_capture_preset_name(&self.name)? != self.name {
            return Err(
                "a screen capture preset name cannot have surrounding whitespace".to_owned(),
            );
        }
        validate_capture_delay(self.delay_seconds)
    }

    pub(super) fn name(&self) -> &str {
        &self.name
    }

    pub(super) fn delay_seconds(&self) -> u64 {
        self.delay_seconds
    }

    pub(super) fn cursor(&self) -> CaptureCursor {
        CaptureCursor::from_included(self.include_cursor)
    }
}

pub(super) fn screen_capture_presets() -> Result<Vec<ScreenCapturePreset>, String> {
    screen_capture_presets_from(&settings_path()?)
}

pub(super) fn screen_capture_preset(name: &str) -> Result<ScreenCapturePreset, String> {
    let name = validate_screen_capture_preset_name(name)?;
    screen_capture_presets()?
        .into_iter()
        .find(|preset| preset.name == name)
        .ok_or_else(|| format!("no screen capture preset named '{name}'"))
}

pub(super) fn save_screen_capture_preset(
    name: &str,
    delay_seconds: u64,
    cursor: CaptureCursor,
) -> Result<Vec<ScreenCapturePreset>, String> {
    save_screen_capture_preset_in(&settings_path()?, name, delay_seconds, cursor)
}

pub(super) fn delete_screen_capture_preset(name: &str) -> Result<Vec<ScreenCapturePreset>, String> {
    delete_screen_capture_preset_in(&settings_path()?, name)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct IntervalPreferences {
    seconds: NonZeroU32,
    skip_identical: bool,
}

impl IntervalPreferences {
    pub(super) fn new(seconds: u32, skip_identical: bool) -> Result<Self, String> {
        let seconds = NonZeroU32::new(seconds)
            .ok_or_else(|| "the interval must be at least one second".to_owned())?;
        Ok(Self {
            seconds,
            skip_identical,
        })
    }

    pub(super) fn seconds(self) -> u32 {
        self.seconds.get()
    }

    pub(super) fn skip_identical(self) -> bool {
        self.skip_identical
    }
}

impl Default for IntervalPreferences {
    fn default() -> Self {
        Self {
            seconds: NonZeroU32::new(10).expect("the default interval is nonzero"),
            skip_identical: true,
        }
    }
}

pub(super) fn interval_preferences() -> Result<IntervalPreferences, String> {
    interval_preferences_from(&settings_path()?)
}

pub(super) fn set_interval_preferences(preferences: IntervalPreferences) -> Result<(), String> {
    set_interval_preferences_in(&settings_path()?, preferences)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SchedulePreferences {
    pub(super) time: ScheduleTime,
    pub(super) enabled: bool,
    pub(super) last_fired: Option<ScheduleDay>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ScheduleClaim {
    Claimed,
    AlreadyClaimed,
    Changed,
}

impl Default for SchedulePreferences {
    fn default() -> Self {
        Self {
            time: "09:00"
                .parse()
                .expect("the default daily capture time is valid"),
            enabled: false,
            last_fired: None,
        }
    }
}

pub(super) fn schedule_preferences() -> Result<SchedulePreferences, String> {
    schedule_preferences_from(&settings_path()?)
}

pub(super) fn set_schedule_preferences(
    time: ScheduleTime,
    enabled: bool,
) -> Result<SchedulePreferences, String> {
    set_schedule_preferences_in(&settings_path()?, time, enabled)
}

pub(super) fn mark_schedule_fired(
    time: ScheduleTime,
    day: ScheduleDay,
) -> Result<ScheduleClaim, String> {
    mark_schedule_fired_in(&settings_path()?, time, day)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CursorPreferences {
    pub(super) capture: CaptureCursor,
    pub(super) recording: CaptureCursor,
}

impl Default for CursorPreferences {
    fn default() -> Self {
        Self {
            capture: CaptureCursor::Included,
            recording: CaptureCursor::Included,
        }
    }
}

pub(super) fn capture_cursor() -> Result<CaptureCursor, String> {
    cursor_from(&settings_path()?, CAPTURE_CURSOR_KEY)
}

pub(super) fn recording_cursor() -> Result<CaptureCursor, String> {
    cursor_from(&settings_path()?, RECORDING_CURSOR_KEY)
}

pub(super) fn set_cursor_preferences(preferences: CursorPreferences) -> Result<(), String> {
    set_cursor_preferences_in(&settings_path()?, preferences)
}

pub(super) fn recording_frame_rate() -> Result<RecordingFrameRate, String> {
    recording_frame_rate_from(&settings_path()?)
}

pub(super) fn set_recording_frame_rate(frame_rate: RecordingFrameRate) -> Result<(), String> {
    set_recording_frame_rate_in(&settings_path()?, frame_rate)
}

pub(super) fn webcam_configuration() -> Result<WebcamConfiguration, String> {
    webcam_configuration_from(&settings_path()?)
}

pub(super) fn set_webcam_configuration(configuration: &WebcamConfiguration) -> Result<(), String> {
    set_webcam_configuration_in(&settings_path()?, configuration)
}

fn webcam_configuration_from(path: &Path) -> Result<WebcamConfiguration, String> {
    let settings = read_settings(path)?;
    let default = WebcamConfiguration::default();
    let device = match settings.get(WEBCAM_DEVICE_KEY) {
        None => default.device(),
        Some(value) => value
            .as_str()
            .ok_or_else(|| "the webcam device preference must be text".to_owned())?,
    };
    let corner = match settings.get(WEBCAM_CORNER_KEY) {
        None => default.layout().corner(),
        Some(value) => WebcamCorner::from_name(
            value
                .as_str()
                .ok_or_else(|| "the webcam corner preference must be text".to_owned())?,
        )?,
    };
    let size_percent = match settings.get(WEBCAM_SIZE_PERCENT_KEY) {
        None => default.layout().size_percent(),
        Some(value) => value
            .as_u64()
            .and_then(|size| u8::try_from(size).ok())
            .ok_or_else(|| "the webcam size preference must be a whole percentage".to_owned())?,
    };
    let shape = match settings.get(WEBCAM_SHAPE_KEY) {
        None => default.layout().shape(),
        Some(value) => WebcamShape::from_name(
            value
                .as_str()
                .ok_or_else(|| "the webcam shape preference must be text".to_owned())?,
        )?,
    };
    WebcamConfiguration::new(
        device,
        WebcamLayout::new(corner, size_percent)?.with_shape(shape),
    )
}

fn set_webcam_configuration_in(
    path: &Path,
    configuration: &WebcamConfiguration,
) -> Result<(), String> {
    let _lock = lock_settings(path)?;
    let mut settings = read_settings(path)?;
    settings.insert(
        WEBCAM_DEVICE_KEY.to_owned(),
        Value::String(configuration.device().to_owned()),
    );
    settings.insert(
        WEBCAM_CORNER_KEY.to_owned(),
        Value::String(configuration.layout().corner().name().to_owned()),
    );
    settings.insert(
        WEBCAM_SIZE_PERCENT_KEY.to_owned(),
        Value::from(configuration.layout().size_percent()),
    );
    settings.insert(
        WEBCAM_SHAPE_KEY.to_owned(),
        Value::String(configuration.layout().shape().name().to_owned()),
    );
    write_settings(path, &settings)
}

fn tray_enabled_from(path: &Path) -> Result<bool, String> {
    let settings = read_settings(path)?;
    match settings.get(TRAY_ENABLED_KEY) {
        None => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| "the tray preference must be a boolean".to_owned()),
    }
}

fn set_tray_enabled_in(path: &Path, enabled: bool) -> Result<(), String> {
    let _lock = lock_settings(path)?;
    let mut settings = read_settings(path)?;
    settings.insert(TRAY_ENABLED_KEY.to_owned(), Value::Bool(enabled));
    write_settings(path, &settings)
}

fn copy_capture_to_clipboard_from(path: &Path) -> Result<bool, String> {
    let settings = read_settings(path)?;
    match settings.get(COPY_CAPTURE_TO_CLIPBOARD_KEY) {
        None => Ok(true),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| "the copy-to-clipboard preference must be a boolean".to_owned()),
    }
}

fn set_copy_capture_to_clipboard_in(path: &Path, enabled: bool) -> Result<(), String> {
    let _lock = lock_settings(path)?;
    let mut settings = read_settings(path)?;
    settings.insert(
        COPY_CAPTURE_TO_CLIPBOARD_KEY.to_owned(),
        Value::Bool(enabled),
    );
    write_settings(path, &settings)
}

fn automatic_copy_directory_from(path: &Path) -> Result<Option<PathBuf>, String> {
    let settings = read_settings(path)?;
    match settings.get(AUTOMATIC_COPY_DIRECTORY_KEY) {
        None => Ok(None),
        Some(Value::String(directory))
            if !directory.is_empty() && Path::new(directory).is_absolute() =>
        {
            Ok(Some(PathBuf::from(directory)))
        }
        Some(_) => Err("the automatic copy folder must be an absolute local path".to_owned()),
    }
}

fn set_automatic_copy_directory_in(path: &Path, directory: Option<&Path>) -> Result<(), String> {
    let directory = directory
        .map(validate_automatic_copy_directory)
        .transpose()?;
    let _lock = lock_settings(path)?;
    let mut settings = read_settings(path)?;
    match directory {
        Some(directory) => {
            settings.insert(
                AUTOMATIC_COPY_DIRECTORY_KEY.to_owned(),
                Value::String(directory.to_owned()),
            );
        }
        None => {
            settings.remove(AUTOMATIC_COPY_DIRECTORY_KEY);
        }
    }
    write_settings(path, &settings)
}

fn validate_automatic_copy_directory(directory: &Path) -> Result<&str, String> {
    if !directory.is_absolute() {
        return Err("the automatic copy folder must be an absolute local path".to_owned());
    }
    let text = directory
        .to_str()
        .ok_or_else(|| "the automatic copy folder path must be valid UTF-8".to_owned())?;
    let metadata = std::fs::metadata(directory)
        .map_err(|error| format!("could not inspect {}: {error}", directory.display()))?;
    if !metadata.is_dir() {
        return Err(format!("{} is not a folder", directory.display()));
    }
    Ok(text)
}

fn capture_delay_seconds_from(path: &Path) -> Result<u64, String> {
    let settings = read_settings(path)?;
    let seconds = match settings.get(CAPTURE_DELAY_SECONDS_KEY) {
        None => return Ok(0),
        Some(value) => value
            .as_u64()
            .ok_or_else(|| "the capture delay must be a whole number of seconds".to_owned())?,
    };
    validate_capture_delay(seconds)?;
    Ok(seconds)
}

fn set_capture_delay_seconds_in(path: &Path, seconds: u64) -> Result<(), String> {
    validate_capture_delay(seconds)?;
    let _lock = lock_settings(path)?;
    let mut settings = read_settings(path)?;
    settings.insert(CAPTURE_DELAY_SECONDS_KEY.to_owned(), Value::from(seconds));
    write_settings(path, &settings)
}

fn validate_capture_delay(seconds: u64) -> Result<(), String> {
    if seconds > 60 {
        return Err("the capture delay must be between 0 and 60 seconds".to_owned());
    }
    Ok(())
}

fn validate_screen_capture_preset_name(name: &str) -> Result<&str, String> {
    let contains_control = name.chars().any(char::is_control);
    let name = name.trim();
    if name.is_empty() || name.len() > 128 || contains_control {
        return Err(
            "the screen capture preset name must be nonempty text without control characters (maximum 128 bytes)"
                .to_owned(),
        );
    }
    Ok(name)
}

fn screen_capture_presets_from(path: &Path) -> Result<Vec<ScreenCapturePreset>, String> {
    screen_capture_presets_from_settings(&read_settings(path)?)
}

fn screen_capture_presets_from_settings(
    settings: &Map<String, Value>,
) -> Result<Vec<ScreenCapturePreset>, String> {
    let Some(value) = settings.get(SCREEN_CAPTURE_PRESETS_KEY) else {
        return Ok(Vec::new());
    };
    let presets: Vec<ScreenCapturePreset> = serde_json::from_value(value.clone())
        .map_err(|error| format!("the screen capture presets are invalid: {error}"))?;
    let mut names = std::collections::HashSet::with_capacity(presets.len());
    for preset in &presets {
        preset.validate()?;
        if !names.insert(&preset.name) {
            return Err(format!(
                "duplicate screen capture preset name: {}",
                preset.name
            ));
        }
    }
    Ok(presets)
}

fn save_screen_capture_preset_in(
    path: &Path,
    name: &str,
    delay_seconds: u64,
    cursor: CaptureCursor,
) -> Result<Vec<ScreenCapturePreset>, String> {
    let mut preset = ScreenCapturePreset::new(name, delay_seconds, cursor)?;
    let _lock = lock_settings(path)?;
    let mut settings = read_settings(path)?;
    let mut presets = screen_capture_presets_from_settings(&settings)?;
    if let Some(existing) = presets.iter_mut().find(|saved| saved.name == preset.name) {
        preset.other = std::mem::take(&mut existing.other);
        *existing = preset;
    } else {
        presets.push(preset);
    }
    write_screen_capture_presets(path, &mut settings, &presets)?;
    Ok(presets)
}

fn delete_screen_capture_preset_in(
    path: &Path,
    name: &str,
) -> Result<Vec<ScreenCapturePreset>, String> {
    let name = validate_screen_capture_preset_name(name)?;
    let _lock = lock_settings(path)?;
    let mut settings = read_settings(path)?;
    let mut presets = screen_capture_presets_from_settings(&settings)?;
    let index = presets
        .iter()
        .position(|preset| preset.name == name)
        .ok_or_else(|| format!("no screen capture preset named '{name}'"))?;
    presets.remove(index);
    write_screen_capture_presets(path, &mut settings, &presets)?;
    Ok(presets)
}

fn write_screen_capture_presets(
    path: &Path,
    settings: &mut Map<String, Value>,
    presets: &[ScreenCapturePreset],
) -> Result<(), String> {
    let value = serde_json::to_value(presets)
        .map_err(|error| format!("could not serialize screen capture presets: {error}"))?;
    settings.insert(SCREEN_CAPTURE_PRESETS_KEY.to_owned(), value);
    write_settings(path, settings)
}

fn interval_preferences_from(path: &Path) -> Result<IntervalPreferences, String> {
    let settings = read_settings(path)?;
    let default = IntervalPreferences::default();
    let seconds = match settings.get(INTERVAL_SECONDS_KEY) {
        None => default.seconds(),
        Some(value) => value
            .as_u64()
            .and_then(|seconds| u32::try_from(seconds).ok())
            .ok_or_else(|| "the interval must be a whole number of seconds".to_owned())?,
    };
    let skip_identical = match settings.get(INTERVAL_SKIP_IDENTICAL_KEY) {
        None => default.skip_identical,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| "the skip-identical interval preference must be a boolean".to_owned())?,
    };
    IntervalPreferences::new(seconds, skip_identical)
}

fn set_interval_preferences_in(
    path: &Path,
    preferences: IntervalPreferences,
) -> Result<(), String> {
    let _lock = lock_settings(path)?;
    let mut settings = read_settings(path)?;
    settings.insert(
        INTERVAL_SECONDS_KEY.to_owned(),
        Value::from(preferences.seconds()),
    );
    settings.insert(
        INTERVAL_SKIP_IDENTICAL_KEY.to_owned(),
        Value::Bool(preferences.skip_identical()),
    );
    write_settings(path, &settings)
}

fn schedule_preferences_from(path: &Path) -> Result<SchedulePreferences, String> {
    schedule_preferences_from_settings(&read_settings(path)?)
}

fn schedule_preferences_from_settings(
    settings: &Map<String, Value>,
) -> Result<SchedulePreferences, String> {
    let default = SchedulePreferences::default();
    let default_time = match settings.get(SCHEDULE_DEFAULT_KEY) {
        None | Some(Value::Null) => default.time,
        Some(value) => value
            .as_str()
            .ok_or_else(|| "the default daily capture time must be text".to_owned())?
            .parse::<ScheduleTime>()?,
    };
    let (time, enabled) = match settings.get(SCHEDULE_KEY) {
        None | Some(Value::Null) => (default_time, false),
        Some(value) => (
            value
                .as_str()
                .ok_or_else(|| "the daily capture schedule must be text or null".to_owned())?
                .parse::<ScheduleTime>()?,
            true,
        ),
    };
    let last_fired = match settings.get(SCHEDULE_LAST_FIRED_KEY) {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .ok_or_else(|| "the last daily capture date must be text".to_owned())?
                .parse::<ScheduleDay>()?,
        ),
    };
    Ok(SchedulePreferences {
        time,
        enabled,
        last_fired,
    })
}

fn set_schedule_preferences_in(
    path: &Path,
    time: ScheduleTime,
    enabled: bool,
) -> Result<SchedulePreferences, String> {
    let _lock = lock_settings(path)?;
    let mut settings = read_settings(path)?;
    let time = time.to_string();
    settings.insert(SCHEDULE_DEFAULT_KEY.to_owned(), Value::String(time.clone()));
    settings.insert(
        SCHEDULE_KEY.to_owned(),
        if enabled {
            Value::String(time)
        } else {
            Value::Null
        },
    );
    let preferences = schedule_preferences_from_settings(&settings)?;
    write_settings(path, &settings)?;
    Ok(preferences)
}

fn mark_schedule_fired_in(
    path: &Path,
    time: ScheduleTime,
    day: ScheduleDay,
) -> Result<ScheduleClaim, String> {
    let _lock = lock_settings(path)?;
    let mut settings = read_settings(path)?;
    let preferences = schedule_preferences_from_settings(&settings)?;
    if !preferences.enabled || preferences.time != time {
        return Ok(ScheduleClaim::Changed);
    }
    if preferences.last_fired.is_some_and(|last| day <= last) {
        return Ok(ScheduleClaim::AlreadyClaimed);
    }
    settings.insert(
        SCHEDULE_LAST_FIRED_KEY.to_owned(),
        Value::String(day.to_string()),
    );
    write_settings(path, &settings)?;
    Ok(ScheduleClaim::Claimed)
}

fn lock_settings(path: &Path) -> Result<std::fs::File, String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("settings path {} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    let lock_path = parent.join("settings.json.schedule.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|error| format!("could not open {}: {error}", lock_path.display()))?;
    file.try_lock()
        .map_err(|error| format!("could not lock {}: {error}", lock_path.display()))?;
    Ok(file)
}

fn cursor_from(path: &Path, key: &str) -> Result<CaptureCursor, String> {
    let settings = read_settings(path)?;
    match settings.get(key) {
        None => Ok(CaptureCursor::Included),
        Some(value) => value
            .as_bool()
            .map(CaptureCursor::from_included)
            .ok_or_else(|| format!("the {key} preference must be a boolean")),
    }
}

fn set_cursor_preferences_in(path: &Path, preferences: CursorPreferences) -> Result<(), String> {
    let _lock = lock_settings(path)?;
    let mut settings = read_settings(path)?;
    settings.insert(
        CAPTURE_CURSOR_KEY.to_owned(),
        Value::Bool(preferences.capture.included()),
    );
    settings.insert(
        RECORDING_CURSOR_KEY.to_owned(),
        Value::Bool(preferences.recording.included()),
    );
    write_settings(path, &settings)
}

fn recording_frame_rate_from(path: &Path) -> Result<RecordingFrameRate, String> {
    let settings = read_settings(path)?;
    match settings.get(RECORDING_FRAME_RATE_KEY) {
        None => Ok(RecordingFrameRate::Source),
        Some(value) => {
            let name = value
                .as_str()
                .ok_or_else(|| "the recording frame rate preference must be text".to_owned())?;
            RecordingFrameRate::from_stored_name(name)
        }
    }
}

fn set_recording_frame_rate_in(path: &Path, frame_rate: RecordingFrameRate) -> Result<(), String> {
    let _lock = lock_settings(path)?;
    let mut settings = read_settings(path)?;
    settings.insert(
        RECORDING_FRAME_RATE_KEY.to_owned(),
        Value::String(frame_rate.stored_name().to_owned()),
    );
    write_settings(path, &settings)
}

fn global_capture_shortcut_from(path: &Path) -> Result<String, String> {
    let settings = read_settings(path)?;
    let Some(value) = settings.get(GLOBAL_CAPTURE_SHORTCUT_KEY) else {
        return Ok(DEFAULT_GLOBAL_CAPTURE_SHORTCUT.to_owned());
    };
    let shortcut = value
        .as_str()
        .ok_or_else(|| "the configured global capture shortcut must be text".to_owned())?;
    validate_global_capture_shortcut(shortcut).map(str::to_owned)
}

fn global_recording_shortcuts_from(path: &Path) -> Result<bool, String> {
    let settings = read_settings(path)?;
    match settings.get(GLOBAL_RECORDING_SHORTCUTS_KEY) {
        None => Ok(false),
        Some(value) => value.as_bool().ok_or_else(|| {
            "the global recording shortcuts preference must be a boolean".to_owned()
        }),
    }
}

fn global_recording_start_from(path: &Path) -> Result<RecordingStartShortcut, String> {
    let settings = read_settings(path)?;
    match settings.get(GLOBAL_RECORDING_START_KEY) {
        None => Ok(RecordingStartShortcut::Disabled),
        Some(value) => {
            let name = value
                .as_str()
                .ok_or_else(|| "the global recording start preference must be text".to_owned())?;
            RecordingStartShortcut::from_stored_name(name)
        }
    }
}

fn global_shortcuts_enabled_from(path: &Path) -> Result<bool, String> {
    let settings = read_settings(path)?;
    match settings.get(GLOBAL_SHORTCUTS_ENABLED_KEY) {
        None => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| "the global shortcuts enabled preference must be a boolean".to_owned()),
    }
}

fn set_global_shortcuts_in(
    path: &Path,
    shortcut: &str,
    options: ShortcutOptions,
) -> Result<String, String> {
    let shortcut = validate_global_capture_shortcut(shortcut)?;
    let _lock = lock_settings(path)?;
    let mut settings = read_settings(path)?;
    settings.insert(
        GLOBAL_CAPTURE_SHORTCUT_KEY.to_owned(),
        Value::String(shortcut.to_owned()),
    );
    settings.insert(
        GLOBAL_RECORDING_SHORTCUTS_KEY.to_owned(),
        Value::Bool(options.recording_controls),
    );
    settings.insert(
        GLOBAL_RECORDING_START_KEY.to_owned(),
        Value::String(options.recording_start.stored_name().to_owned()),
    );
    settings.insert(GLOBAL_SHORTCUTS_ENABLED_KEY.to_owned(), Value::Bool(true));
    write_settings(path, &settings)?;
    Ok(shortcut.to_owned())
}

fn disable_global_shortcuts_in(path: &Path) -> Result<(), String> {
    let _lock = lock_settings(path)?;
    let mut settings = read_settings(path)?;
    settings.insert(GLOBAL_SHORTCUTS_ENABLED_KEY.to_owned(), Value::Bool(false));
    write_settings(path, &settings)
}

fn validate_global_capture_shortcut(shortcut: &str) -> Result<&str, String> {
    let contains_control = shortcut.chars().any(char::is_control);
    let shortcut = shortcut.trim();
    if shortcut.is_empty() || shortcut.len() > 128 || contains_control {
        return Err(
            "the preferred global capture shortcut must be nonempty text without control characters (maximum 128 bytes)"
                .to_owned(),
        );
    }
    Ok(shortcut)
}

fn set_theme_in(path: &Path, theme: &str) -> Result<(), String> {
    if !is_supported_theme(theme) {
        return Err("theme must be Dark, Light, or System".to_owned());
    }
    let _lock = lock_settings(path)?;
    let mut settings = read_settings(path)?;
    settings.insert(THEME_KEY.to_owned(), Value::String(theme.to_owned()));
    write_settings(path, &settings)
}

fn theme_from_settings(settings: &Map<String, Value>) -> Result<&str, String> {
    let Some(theme) = settings.get(THEME_KEY) else {
        return Ok("System");
    };
    theme
        .as_str()
        .filter(|theme| is_supported_theme(theme))
        .ok_or_else(|| "the configured theme must be Dark, Light, or System".to_owned())
}

fn is_supported_theme(theme: &str) -> bool {
    matches!(theme, "Dark" | "Light" | "System")
}

fn settings_path() -> Result<PathBuf, String> {
    dirs::config_dir()
        .map(|directory| directory.join("Jiffshot").join(SETTINGS_FILE))
        .ok_or_else(|| "could not find the user's configuration folder".to_owned())
}

fn read_settings(path: &Path) -> Result<Map<String, Value>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => {
            return Err(format!(
                "could not read settings from {}: {error}",
                path.display()
            ));
        }
    };
    match serde_json::from_slice::<Value>(&bytes)
        .map_err(|error| format!("could not parse settings at {}: {error}", path.display()))?
    {
        Value::Object(settings) => Ok(settings),
        _ => Err(format!(
            "settings at {} must contain a JSON object",
            path.display()
        )),
    }
}

fn write_settings(path: &Path, settings: &Map<String, Value>) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("settings path {} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    let bytes = serde_json::to_vec_pretty(settings)
        .map_err(|error| format!("could not serialize settings: {error}"))?;
    let staging = tempfile::tempdir_in(parent)
        .map_err(|error| format!("could not create a temporary settings file: {error}"))?;
    let staged_path = staging.path().join(SETTINGS_FILE);
    std::fs::write(&staged_path, bytes)
        .map_err(|error| format!("could not write {}: {error}", staged_path.display()))?;
    std::fs::rename(&staged_path, path)
        .map_err(|error| format!("could not save settings to {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_capture_presets_default_empty_without_writing_settings() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);

        assert_eq!(
            screen_capture_presets_from(&path).expect("missing settings should load"),
            Vec::new()
        );
        assert!(!path.exists(), "reading defaults must not create settings");

        let original = br#"{"theme":"Dark","other":{"keep":true}}"#;
        std::fs::write(&path, original).expect("write preferences without presets");
        assert_eq!(
            screen_capture_presets_from(&path).expect("missing preset key should load"),
            Vec::new()
        );
        assert_eq!(std::fs::read(&path).expect("read preferences"), original);
    }

    #[test]
    fn screen_capture_preset_save_trims_and_preserves_normal_preferences() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        std::fs::write(
            &path,
            br#"{"theme":"Dark","capture_delay_seconds":9,"capture_cursor":true,"recording_cursor":false,"copy_to_clipboard":true,"other":{"keep":[1,"two"]}}"#,
        )
        .expect("write existing preferences");

        let saved = save_screen_capture_preset_in(&path, "  work  ", 3, CaptureCursor::Hidden)
            .expect("the preset should save");
        assert_eq!(
            saved,
            vec![ScreenCapturePreset::new("work", 3, CaptureCursor::Hidden).unwrap()]
        );
        assert_eq!(
            screen_capture_presets_from(&path).expect("saved preset should load"),
            saved
        );
        assert_eq!(capture_delay_seconds_from(&path).unwrap(), 9);
        assert_eq!(
            cursor_from(&path, CAPTURE_CURSOR_KEY).unwrap(),
            CaptureCursor::Included
        );
        assert_eq!(
            cursor_from(&path, RECORDING_CURSOR_KEY).unwrap(),
            CaptureCursor::Hidden
        );
        assert!(copy_capture_to_clipboard_from(&path).unwrap());
        assert_eq!(
            Value::Object(read_settings(&path).unwrap()),
            serde_json::json!({
                "theme": "Dark",
                "capture_delay_seconds": 9,
                "capture_cursor": true,
                "recording_cursor": false,
                "copy_to_clipboard": true,
                "other": {"keep": [1, "two"]},
                "linux_screen_capture_presets": [
                    {"name": "work", "delay_seconds": 3, "include_cursor": false}
                ]
            })
        );
    }

    #[test]
    fn screen_capture_preset_accepts_name_and_delay_boundaries() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        let ascii = "a".repeat(128);
        let padded_ascii = format!("  {ascii}  ");
        let accented = "é".repeat(64);
        let emoji = "🚀".repeat(32);
        let cases = [
            (padded_ascii.as_str(), 0, CaptureCursor::Included),
            (accented.as_str(), 1, CaptureCursor::Hidden),
            (emoji.as_str(), 59, CaptureCursor::Included),
            ("\u{2003}middle\u{2003}", 60, CaptureCursor::Hidden),
        ];
        let mut expected = Vec::new();
        for (name, delay, cursor) in cases {
            expected.push(ScreenCapturePreset::new(name, delay, cursor).unwrap());
            assert_eq!(
                save_screen_capture_preset_in(&path, name, delay, cursor).unwrap(),
                expected,
                "valid name {name:?} with delay {delay} must be saved in order"
            );
        }
        let loaded = screen_capture_presets_from(&path).unwrap();
        assert_eq!(loaded, expected);
        assert_eq!(loaded[0].name(), ascii);
        assert_eq!(loaded[1].name(), accented);
        assert_eq!(loaded[2].name(), emoji);
        assert_eq!(loaded[3].name(), "middle");
        assert_eq!(
            loaded
                .iter()
                .map(|preset| preset.delay_seconds())
                .collect::<Vec<_>>(),
            [0, 1, 59, 60]
        );
        assert_eq!(
            loaded
                .iter()
                .map(|preset| preset.cursor())
                .collect::<Vec<_>>(),
            [
                CaptureCursor::Included,
                CaptureCursor::Hidden,
                CaptureCursor::Included,
                CaptureCursor::Hidden
            ]
        );
    }

    #[test]
    fn screen_capture_preset_replacement_preserves_order_and_unknown_fields() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        let original = serde_json::json!({
            "other": {"future": [1, 2]},
            "capture_delay_seconds": 8,
            "linux_screen_capture_presets": [
                {"name": "first", "delay_seconds": 1, "include_cursor": false},
                {
                    "name": "middle",
                    "delay_seconds": 2,
                    "include_cursor": false,
                    "future": {"nested": [true, "keep"]},
                    "nullable": null
                },
                {"name": "last", "delay_seconds": 3, "include_cursor": true}
            ]
        });
        std::fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();

        let presets = save_screen_capture_preset_in(&path, " middle ", 60, CaptureCursor::Included)
            .expect("replacement should save");
        assert_eq!(
            presets
                .iter()
                .map(ScreenCapturePreset::name)
                .collect::<Vec<_>>(),
            ["first", "middle", "last"]
        );
        assert_eq!(presets[1].delay_seconds(), 60);
        assert_eq!(presets[1].cursor(), CaptureCursor::Included);
        assert_eq!(
            Value::Object(read_settings(&path).unwrap()),
            serde_json::json!({
                "other": {"future": [1, 2]},
                "capture_delay_seconds": 8,
                "linux_screen_capture_presets": [
                    {"name": "first", "delay_seconds": 1, "include_cursor": false},
                    {
                        "name": "middle",
                        "delay_seconds": 60,
                        "include_cursor": true,
                        "future": {"nested": [true, "keep"]},
                        "nullable": null
                    },
                    {"name": "last", "delay_seconds": 3, "include_cursor": true}
                ]
            })
        );
    }

    #[test]
    fn screen_capture_preset_deletion_preserves_remaining_entries() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        let original = serde_json::json!({
            "other": {"keep": true},
            "linux_screen_capture_presets": [
                {"name": "first", "delay_seconds": 0, "include_cursor": true, "future": 42},
                {"name": "middle", "delay_seconds": 2, "include_cursor": false},
                {"name": "last", "delay_seconds": 60, "include_cursor": false, "future": {"x": 1}}
            ]
        });
        std::fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();

        let remaining = delete_screen_capture_preset_in(&path, " middle ")
            .expect("existing preset should be deleted");
        assert_eq!(
            remaining
                .iter()
                .map(ScreenCapturePreset::name)
                .collect::<Vec<_>>(),
            ["first", "last"]
        );
        assert_eq!(
            Value::Object(read_settings(&path).unwrap()),
            serde_json::json!({
                "other": {"keep": true},
                "linux_screen_capture_presets": [
                    {"name": "first", "delay_seconds": 0, "include_cursor": true, "future": 42},
                    {"name": "last", "delay_seconds": 60, "include_cursor": false, "future": {"x": 1}}
                ]
            })
        );
        assert_eq!(screen_capture_presets_from(&path).unwrap(), remaining);
    }

    #[test]
    fn screen_capture_preset_survives_normal_preference_changes() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        let saved =
            save_screen_capture_preset_in(&path, "work", 60, CaptureCursor::Hidden).unwrap();
        set_capture_delay_seconds_in(&path, 12).unwrap();
        set_cursor_preferences_in(
            &path,
            CursorPreferences {
                capture: CaptureCursor::Included,
                recording: CaptureCursor::Hidden,
            },
        )
        .unwrap();
        set_copy_capture_to_clipboard_in(&path, true).unwrap();

        assert_eq!(screen_capture_presets_from(&path).unwrap(), saved);
        assert_eq!(
            Value::Object(read_settings(&path).unwrap()),
            serde_json::json!({
                "capture_delay_seconds": 12,
                "capture_cursor": true,
                "recording_cursor": false,
                "copy_to_clipboard": true,
                "linux_screen_capture_presets": [
                    {"name": "work", "delay_seconds": 60, "include_cursor": false}
                ]
            })
        );
    }

    #[test]
    fn screen_capture_preset_missing_deletion_preserves_settings() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        assert_eq!(
            delete_screen_capture_preset_in(&path, "absent").unwrap_err(),
            "no screen capture preset named 'absent'"
        );
        assert!(!path.exists(), "missing deletion must not create settings");

        let original = br#"{ "other": true, "linux_screen_capture_presets": [{"name":"keep","delay_seconds":0,"include_cursor":false}] }"#;
        std::fs::write(&path, original).unwrap();
        assert_eq!(
            delete_screen_capture_preset_in(&path, " absent ").unwrap_err(),
            "no screen capture preset named 'absent'"
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn screen_capture_preset_invalid_names_preserve_settings() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        let original = br#"{ "theme": "Dark", "linux_screen_capture_presets": [{"name":"keep","delay_seconds":0,"include_cursor":false}] }"#;
        std::fs::write(&path, original).unwrap();
        let too_long_ascii = "a".repeat(129);
        let too_long_utf8 = "é".repeat(65);
        let one_byte_over_utf8 = format!("{}é", "a".repeat(127));
        for name in [
            "",
            "   ",
            "\tvalid",
            "valid\n",
            "a\u{0007}b",
            "a\u{007f}b",
            "a\u{0085}b",
            too_long_ascii.as_str(),
            too_long_utf8.as_str(),
            one_byte_over_utf8.as_str(),
        ] {
            let save_error = save_screen_capture_preset_in(&path, name, 0, CaptureCursor::Hidden)
                .expect_err("invalid name must not save");
            assert!(
                save_error.contains("nonempty"),
                "name {name:?}: {save_error}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), original, "save {name:?}");
            let delete_error =
                delete_screen_capture_preset_in(&path, name).expect_err("invalid name must fail");
            assert!(
                delete_error.contains("nonempty"),
                "name {name:?}: {delete_error}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), original, "delete {name:?}");
        }
    }

    #[test]
    fn screen_capture_preset_invalid_delays_preserve_settings() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        let original = br#"{ "theme": "Dark", "linux_screen_capture_presets": [{"name":"keep","delay_seconds":0,"include_cursor":false}] }"#;
        std::fs::write(&path, original).unwrap();
        for delay in [61, u64::MAX] {
            let error =
                save_screen_capture_preset_in(&path, "keep", delay, CaptureCursor::Included)
                    .expect_err("out-of-range delays must fail");
            assert!(error.contains("between 0 and 60"), "delay {delay}: {error}");
            assert_eq!(std::fs::read(&path).unwrap(), original, "delay {delay}");
        }
    }

    #[test]
    fn screen_capture_presets_malformed_values_are_not_overwritten() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        let malformed = [
            ("null list", serde_json::json!(null)),
            ("object list", serde_json::json!({})),
            ("non-object entry", serde_json::json!([7])),
            (
                "missing name",
                serde_json::json!([{"delay_seconds": 0, "include_cursor": true}]),
            ),
            (
                "missing delay",
                serde_json::json!([{"name": "x", "include_cursor": true}]),
            ),
            (
                "missing cursor",
                serde_json::json!([{"name": "x", "delay_seconds": 0}]),
            ),
            (
                "non-string name",
                serde_json::json!([{"name": 1, "delay_seconds": 0, "include_cursor": true}]),
            ),
            (
                "empty name",
                serde_json::json!([{"name": "", "delay_seconds": 0, "include_cursor": true}]),
            ),
            (
                "untrimmed name",
                serde_json::json!([{"name": " x ", "delay_seconds": 0, "include_cursor": true}]),
            ),
            (
                "control in name",
                serde_json::json!([{"name": "x\n", "delay_seconds": 0, "include_cursor": true}]),
            ),
            (
                "long UTF-8 name",
                serde_json::json!([{"name": "é".repeat(65), "delay_seconds": 0, "include_cursor": true}]),
            ),
            (
                "negative delay",
                serde_json::json!([{"name": "x", "delay_seconds": -1, "include_cursor": true}]),
            ),
            (
                "fractional delay",
                serde_json::json!([{"name": "x", "delay_seconds": 1.5, "include_cursor": true}]),
            ),
            (
                "string delay",
                serde_json::json!([{"name": "x", "delay_seconds": "1", "include_cursor": true}]),
            ),
            (
                "delay above limit",
                serde_json::json!([{"name": "x", "delay_seconds": 61, "include_cursor": true}]),
            ),
            (
                "string cursor",
                serde_json::json!([{"name": "x", "delay_seconds": 0, "include_cursor": "yes"}]),
            ),
            (
                "null cursor",
                serde_json::json!([{"name": "x", "delay_seconds": 0, "include_cursor": null}]),
            ),
        ];
        for (description, value) in malformed {
            let original = serde_json::to_vec(&serde_json::json!({
                "other": {"preserve": true},
                "linux_screen_capture_presets": value
            }))
            .unwrap();
            std::fs::write(&path, &original).unwrap();
            let error = screen_capture_presets_from(&path)
                .expect_err("malformed stored presets must be reported");
            assert!(!error.is_empty(), "{description} must have an error");
            assert_eq!(
                save_screen_capture_preset_in(&path, "new", 0, CaptureCursor::Hidden).unwrap_err(),
                error,
                "save must report {description}"
            );
            assert_eq!(
                delete_screen_capture_preset_in(&path, "x").unwrap_err(),
                error,
                "delete must report {description}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), original, "{description}");
        }

        for original in [b"not json".as_slice(), b"[]".as_slice()] {
            std::fs::write(&path, original).unwrap();
            let error = screen_capture_presets_from(&path).unwrap_err();
            assert_eq!(
                save_screen_capture_preset_in(&path, "new", 0, CaptureCursor::Hidden).unwrap_err(),
                error
            );
            assert_eq!(
                delete_screen_capture_preset_in(&path, "x").unwrap_err(),
                error
            );
            assert_eq!(std::fs::read(&path).unwrap(), original);
        }
    }

    #[test]
    fn screen_capture_presets_duplicate_names_are_not_overwritten() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        let original = br#"{ "other": 42, "linux_screen_capture_presets": [{"name":"dup","delay_seconds":0,"include_cursor":false},{"name":"dup","delay_seconds":60,"include_cursor":true}] }"#;
        std::fs::write(&path, original).unwrap();
        let error = "duplicate screen capture preset name: dup";
        assert_eq!(screen_capture_presets_from(&path).unwrap_err(), error);
        assert_eq!(
            save_screen_capture_preset_in(&path, "dup", 2, CaptureCursor::Hidden).unwrap_err(),
            error
        );
        assert_eq!(
            delete_screen_capture_preset_in(&path, "dup").unwrap_err(),
            error
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn screen_capture_preset_writers_respect_settings_lock() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        save_screen_capture_preset_in(&path, "keep", 1, CaptureCursor::Hidden).unwrap();
        let original = std::fs::read(&path).unwrap();
        let lock = lock_settings(&path).expect("acquire the shared settings lock");

        let save_error =
            save_screen_capture_preset_in(&path, "other", 2, CaptureCursor::Included).unwrap_err();
        let delete_error = delete_screen_capture_preset_in(&path, "keep").unwrap_err();
        assert!(save_error.contains("could not lock"), "{save_error}");
        assert!(delete_error.contains("could not lock"), "{delete_error}");
        assert_eq!(std::fs::read(&path).unwrap(), original);

        drop(lock);
        assert_eq!(
            save_screen_capture_preset_in(&path, "other", 2, CaptureCursor::Included)
                .unwrap()
                .len(),
            2
        );
        let remaining = delete_screen_capture_preset_in(&path, "keep").unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].name(), "other");
    }

    #[test]
    fn automatic_copy_folder_round_trips_and_preserves_unrelated_preferences() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        let target = directory.path().join("shared copies");
        std::fs::create_dir(&target).expect("create the automatic copy folder");
        std::fs::write(&path, br#"{"theme":"Dark","other":{"keep":true}}"#)
            .expect("write existing preferences");
        assert_eq!(
            automatic_copy_directory_from(&path).expect("the default setting should load"),
            None
        );
        set_automatic_copy_directory_in(&path, Some(&target))
            .expect("the folder preference should save");
        assert_eq!(
            automatic_copy_directory_from(&path).expect("the saved folder should load"),
            Some(target.clone())
        );
        let settings = read_settings(&path).expect("settings should remain readable");
        assert_eq!(settings["theme"], "Dark");
        assert_eq!(settings["other"], serde_json::json!({"keep": true}));

        set_automatic_copy_directory_in(&path, None).expect("automatic copying should disable");
        assert_eq!(
            automatic_copy_directory_from(&path).expect("the disabled setting should load"),
            None
        );
        let settings = read_settings(&path).expect("settings should remain readable");
        assert!(!settings.contains_key(AUTOMATIC_COPY_DIRECTORY_KEY));
        assert_eq!(settings["theme"], "Dark");
        assert_eq!(settings["other"], serde_json::json!({"keep": true}));
    }

    #[test]
    fn invalid_automatic_copy_folders_do_not_change_preferences() {
        use std::os::unix::ffi::OsStringExt;

        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        let target = directory.path().join("target");
        std::fs::create_dir(&target).expect("create the automatic copy folder");
        set_automatic_copy_directory_in(&path, Some(&target))
            .expect("the initial folder preference should save");
        let expected = std::fs::read(&path).expect("read the saved preference");
        let regular_file = directory.path().join("not-a-folder");
        std::fs::write(&regular_file, b"file").expect("create a regular file");
        let missing = directory.path().join("missing");
        let non_utf8 = directory
            .path()
            .join(std::ffi::OsString::from_vec(b"invalid-\xff".to_vec()));
        for invalid in [Path::new("relative"), &regular_file, &missing, &non_utf8] {
            assert!(
                set_automatic_copy_directory_in(&path, Some(invalid)).is_err(),
                "{} must be rejected",
                invalid.display()
            );
            assert_eq!(std::fs::read(&path).unwrap(), expected);
        }
        for invalid in [
            serde_json::json!("relative"),
            serde_json::json!(""),
            serde_json::json!(false),
            serde_json::json!(null),
        ] {
            std::fs::write(
                &path,
                serde_json::to_vec(&serde_json::json!({
                    "linux_auto_copy_directory": invalid
                }))
                .expect("serialize invalid setting"),
            )
            .expect("write invalid setting");
            assert!(
                automatic_copy_directory_from(&path).is_err(),
                "invalid folder preference must be reported"
            );
        }
    }

    #[test]
    fn tray_preference_round_trips_and_preserves_existing_settings() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        assert!(!tray_enabled_from(&path).expect("the default tray setting should load"));

        std::fs::write(&path, br#"{"theme":"Dark","other":{"keep":true}}"#)
            .expect("initial preferences should be written");
        set_tray_enabled_in(&path, true).expect("enabling the tray should be saved");
        assert!(tray_enabled_from(&path).expect("the saved tray setting should load"));
        assert_eq!(
            Value::Object(read_settings(&path).expect("settings should remain readable")),
            serde_json::json!({
                "theme": "Dark",
                "other": {"keep": true},
                "linux_tray_enabled": true
            })
        );

        set_tray_enabled_in(&path, false).expect("disabling the tray should be saved");
        assert!(!tray_enabled_from(&path).expect("the disabled setting should load"));
        assert_eq!(
            read_settings(&path).expect("other settings should remain readable")["other"],
            serde_json::json!({"keep": true})
        );
    }

    #[test]
    fn invalid_tray_preference_is_reported_without_rewriting_settings() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        for invalid in [
            r#"{"linux_tray_enabled":"yes"}"#,
            r#"{"linux_tray_enabled":null}"#,
        ] {
            std::fs::write(&path, invalid).expect("invalid preference fixture should be written");
            assert_eq!(
                tray_enabled_from(&path).expect_err("invalid tray settings must be rejected"),
                "the tray preference must be a boolean",
                "settings: {invalid}"
            );
            assert_eq!(
                std::fs::read_to_string(&path).expect("fixture should remain readable"),
                invalid
            );
        }
    }

    #[test]
    fn daily_schedule_preferences_persist_and_preserve_unrelated_values() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        std::fs::write(&path, br#"{"theme":"Dark","other":{"keep":true}}"#)
            .expect("initial preferences should be written");
        assert_eq!(
            schedule_preferences_from(&path).unwrap(),
            SchedulePreferences::default()
        );
        let time: ScheduleTime = "09:30".parse().unwrap();
        let day: ScheduleDay = "2026-030".parse().unwrap();
        let preferences = set_schedule_preferences_in(&path, time, true).unwrap();
        assert_eq!(preferences.time, time);
        assert!(preferences.enabled);
        assert_eq!(
            mark_schedule_fired_in(&path, time, day).unwrap(),
            ScheduleClaim::Claimed
        );
        assert_eq!(
            mark_schedule_fired_in(&path, time, day).unwrap(),
            ScheduleClaim::AlreadyClaimed
        );
        assert_eq!(
            schedule_preferences_from(&path).unwrap().last_fired,
            Some(day)
        );
        assert_eq!(
            Value::Object(read_settings(&path).unwrap()),
            serde_json::json!({
                "theme": "Dark",
                "other": {"keep": true},
                "schedule": "09:30",
                "schedule_default": "09:30",
                "linux_schedule_last_fired": "2026-030"
            })
        );
        assert!(
            !set_schedule_preferences_in(&path, time, false)
                .unwrap()
                .enabled
        );
        assert_eq!(
            mark_schedule_fired_in(&path, time, "2026-031".parse().unwrap()).unwrap(),
            ScheduleClaim::Changed
        );
        assert_eq!(
            set_schedule_preferences_in(&path, time, true)
                .unwrap()
                .last_fired,
            Some(day)
        );
        let changed: ScheduleTime = "10:00".parse().unwrap();
        assert_eq!(
            set_schedule_preferences_in(&path, changed, true)
                .unwrap()
                .last_fired,
            Some(day)
        );
        assert_eq!(
            mark_schedule_fired_in(&path, time, "2026-031".parse().unwrap()).unwrap(),
            ScheduleClaim::Changed
        );
        assert_eq!(
            mark_schedule_fired_in(&path, changed, day).unwrap(),
            ScheduleClaim::AlreadyClaimed
        );
        assert_eq!(
            mark_schedule_fired_in(&path, changed, "2026-031".parse().unwrap()).unwrap(),
            ScheduleClaim::Claimed
        );
    }

    #[test]
    fn daily_schedule_preferences_reject_invalid_settings_without_overwriting_them() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        for invalid in [
            r#"{"schedule":true}"#,
            r#"{"schedule":"24:00"}"#,
            r#"{"schedule_default":17}"#,
            r#"{"schedule_default":"9:00"}"#,
            r#"{"linux_schedule_last_fired":"2025-366"}"#,
            r#"{"linux_schedule_last_fired":false}"#,
        ] {
            std::fs::write(&path, invalid).expect("invalid preference fixture should be written");
            assert!(
                schedule_preferences_from(&path).is_err(),
                "invalid settings must be rejected: {invalid}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
        }
    }

    #[test]
    fn daily_schedule_claim_refuses_a_competing_lock_without_writing() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        let time: ScheduleTime = "10:00".parse().unwrap();
        let day: ScheduleDay = "2026-031".parse().unwrap();
        set_schedule_preferences_in(&path, time, true).unwrap();
        let before = std::fs::read(&path).unwrap();
        let lock = lock_settings(&path).unwrap();
        let error = mark_schedule_fired_in(&path, time, day)
            .expect_err("a competing process must not claim the same schedule");
        assert!(error.contains("could not lock"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        drop(lock);
        assert_eq!(
            mark_schedule_fired_in(&path, time, day).unwrap(),
            ScheduleClaim::Claimed
        );
    }

    #[test]
    fn preference_updates_do_not_overwrite_a_competing_schedule_claim() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        let time: ScheduleTime = "10:00".parse().unwrap();
        let day: ScheduleDay = "2026-031".parse().unwrap();
        set_schedule_preferences_in(&path, time, true).unwrap();
        let before = std::fs::read(&path).unwrap();
        let lock = lock_settings(&path).unwrap();
        let attempts = [
            (
                "capture directory",
                set_capture_directory_in(&path, directory.path()),
            ),
            ("theme", set_theme_in(&path, "Dark")),
            ("tray", set_tray_enabled_in(&path, true)),
            ("clipboard", set_copy_capture_to_clipboard_in(&path, false)),
            ("delay", set_capture_delay_seconds_in(&path, 3)),
            (
                "interval",
                set_interval_preferences_in(&path, IntervalPreferences::new(3, false).unwrap()),
            ),
            (
                "cursor",
                set_cursor_preferences_in(&path, CursorPreferences::default()),
            ),
            (
                "webcam",
                set_webcam_configuration_in(&path, &WebcamConfiguration::default()),
            ),
            (
                "shortcuts",
                set_global_shortcuts_in(
                    &path,
                    "CTRL+SHIFT+X",
                    ShortcutOptions {
                        recording_controls: true,
                        ..ShortcutOptions::default()
                    },
                )
                .map(|_| ()),
            ),
            (
                "schedule",
                set_schedule_preferences_in(&path, time, false).map(|_| ()),
            ),
        ];
        for (name, result) in attempts {
            let error = result.expect_err("settings updates must share the schedule lock");
            assert!(error.contains("could not lock"), "{name}: {error}");
        }
        assert_eq!(std::fs::read(&path).unwrap(), before);
        drop(lock);
        assert_eq!(
            mark_schedule_fired_in(&path, time, day).unwrap(),
            ScheduleClaim::Claimed
        );
        set_theme_in(&path, "Dark").unwrap();
        assert_eq!(
            mark_schedule_fired_in(&path, time, day).unwrap(),
            ScheduleClaim::AlreadyClaimed
        );
        assert_eq!(
            theme_from_settings(&read_settings(&path).unwrap()).unwrap(),
            "Dark"
        );
    }

    #[test]
    fn interval_preferences_round_trip_and_preserve_other_settings() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        assert_eq!(
            interval_preferences_from(&path).expect("default interval settings should load"),
            IntervalPreferences::default()
        );
        std::fs::write(&path, br#"{"theme":"Dark","other":{"keep":true}}"#)
            .expect("initial preferences should be written");
        let preferences =
            IntervalPreferences::new(1, false).expect("one second should be a valid interval");
        set_interval_preferences_in(&path, preferences)
            .expect("interval preferences should be saved");
        assert_eq!(
            interval_preferences_from(&path).expect("saved interval preferences should load"),
            preferences
        );
        assert_eq!(
            Value::Object(read_settings(&path).expect("settings should remain readable")),
            serde_json::json!({
                "theme": "Dark",
                "other": {"keep": true},
                "interval_seconds": 1,
                "interval_skip_identical": false
            })
        );
        let longest = IntervalPreferences::new(u32::MAX, true)
            .expect("the largest representable interval should be valid");
        set_interval_preferences_in(&path, longest)
            .expect("long interval preferences should be saved");
        assert_eq!(
            interval_preferences_from(&path).expect("long interval preferences should load"),
            longest
        );
    }

    #[test]
    fn interval_preferences_reject_invalid_values_without_overwriting_settings() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        for invalid in [
            r#"{"interval_seconds":0}"#,
            r#"{"interval_seconds":-1}"#,
            r#"{"interval_seconds":1.5}"#,
            r#"{"interval_seconds":4294967296}"#,
            r#"{"interval_seconds":"10"}"#,
            r#"{"interval_skip_identical":"yes"}"#,
        ] {
            std::fs::write(&path, invalid).expect("invalid preference fixture should be written");
            assert!(
                interval_preferences_from(&path).is_err(),
                "invalid settings must be rejected: {invalid}"
            );
            assert_eq!(
                std::fs::read_to_string(&path).expect("settings fixture should remain readable"),
                invalid
            );
        }
        assert!(
            IntervalPreferences::new(0, true).is_err(),
            "zero seconds must not construct an interval"
        );
    }

    #[test]
    fn webcam_configuration_round_trips_without_overwriting_other_settings() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        let default =
            webcam_configuration_from(&path).expect("default webcam settings should load");
        assert_eq!(default.device(), "/dev/video0");
        assert_eq!(default.layout().corner(), WebcamCorner::BottomRight);
        assert_eq!(default.layout().size_percent(), 25);
        assert_eq!(default.layout().shape(), WebcamShape::Rectangle);

        let initial = serde_json::json!({
            "theme": "Dark",
            "capture_directory": "/home/example/Captures",
            "copy_to_clipboard": false,
            "other": {"nested": [true, 7, "keep"]}
        });
        std::fs::write(
            &path,
            serde_json::to_vec(&initial).expect("initial preferences should serialize"),
        )
        .expect("initial preferences should be written");

        let cases = [
            (WebcamCorner::TopLeft, 10, "top-left", "/dev/video1"),
            (
                WebcamCorner::TopRight,
                11,
                "top-right",
                "/dev/v4l/by-id/front camera",
            ),
            (WebcamCorner::BottomLeft, 49, "bottom-left", "/dev/video9"),
            (
                WebcamCorner::BottomRight,
                50,
                "bottom-right",
                "/dev/video42",
            ),
        ];
        for (corner, size, name, device) in cases {
            for (shape, shape_name) in [
                (WebcamShape::Rectangle, "rectangle"),
                (WebcamShape::Circle, "circle"),
            ] {
                let layout = WebcamLayout::new(corner, size)
                    .expect("webcam size should be valid")
                    .with_shape(shape);
                let configuration = WebcamConfiguration::new(device, layout)
                    .expect("absolute webcam device path should be valid");
                set_webcam_configuration_in(&path, &configuration)
                    .expect("webcam configuration should be saved");

                let loaded = webcam_configuration_from(&path)
                    .expect("saved webcam configuration should load");
                assert_eq!(loaded.device(), device);
                assert_eq!(loaded.layout().corner(), corner);
                assert_eq!(loaded.layout().size_percent(), size);
                assert_eq!(loaded.layout().shape(), shape);
                assert_eq!(
                    Value::Object(
                        read_settings(&path).expect("saved settings should remain readable")
                    ),
                    serde_json::json!({
                        "theme": "Dark",
                        "capture_directory": "/home/example/Captures",
                        "copy_to_clipboard": false,
                        "other": {"nested": [true, 7, "keep"]},
                        "webcam_device": device,
                        "webcam_corner": name,
                        "webcam_size_percent": size,
                        "webcam_shape": shape_name
                    }),
                    "saving {name} {shape_name} at {size} percent must preserve unrelated settings"
                );
            }
        }
    }

    #[test]
    fn webcam_configuration_defaults_only_missing_fields() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        let cases = [
            (
                r#"{"webcam_corner":"top-left","webcam_size_percent":10}"#,
                "/dev/video0",
                WebcamCorner::TopLeft,
                10,
                WebcamShape::Rectangle,
            ),
            (
                r#"{"webcam_device":"/dev/video7","webcam_size_percent":50}"#,
                "/dev/video7",
                WebcamCorner::BottomRight,
                50,
                WebcamShape::Rectangle,
            ),
            (
                r#"{"webcam_device":"/dev/video9","webcam_corner":"bottom-left"}"#,
                "/dev/video9",
                WebcamCorner::BottomLeft,
                25,
                WebcamShape::Rectangle,
            ),
            (
                r#"{"webcam_shape":"circle"}"#,
                "/dev/video0",
                WebcamCorner::BottomRight,
                25,
                WebcamShape::Circle,
            ),
            (
                r#"{"webcam_shape":"rectangle","webcam_corner":"top-right"}"#,
                "/dev/video0",
                WebcamCorner::TopRight,
                25,
                WebcamShape::Rectangle,
            ),
        ];
        for (settings, device, corner, size, shape) in cases {
            std::fs::write(&path, settings).expect("partial preferences should be written");
            let loaded = webcam_configuration_from(&path).expect("partial preferences should load");
            assert_eq!(loaded.device(), device, "settings: {settings}");
            assert_eq!(loaded.layout().corner(), corner, "settings: {settings}");
            assert_eq!(loaded.layout().size_percent(), size, "settings: {settings}");
            assert_eq!(loaded.layout().shape(), shape, "settings: {settings}");
        }
    }

    #[test]
    fn invalid_webcam_preferences_are_reported() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        let invalid_device = "webcam device must be an absolute path without control characters";
        let invalid_size = "webcam size must be between 10 and 50 percent";
        let non_integer_size = "the webcam size preference must be a whole percentage";
        for (invalid, expected) in [
            (
                r#"{"webcam_device":false}"#,
                "the webcam device preference must be text",
            ),
            (
                r#"{"webcam_device":null}"#,
                "the webcam device preference must be text",
            ),
            (r#"{"webcam_device":""}"#, invalid_device),
            (r#"{"webcam_device":"relative"}"#, invalid_device),
            (r#"{"webcam_device":"/dev/vid\u0000eo0"}"#, invalid_device),
            (
                r#"{"webcam_corner":"center"}"#,
                "unknown webcam corner: center",
            ),
            (
                r#"{"webcam_corner":42}"#,
                "the webcam corner preference must be text",
            ),
            (
                r#"{"webcam_corner":null}"#,
                "the webcam corner preference must be text",
            ),
            (r#"{"webcam_size_percent":9}"#, invalid_size),
            (r#"{"webcam_size_percent":51}"#, invalid_size),
            (r#"{"webcam_size_percent":-1}"#, non_integer_size),
            (r#"{"webcam_size_percent":256}"#, non_integer_size),
            (r#"{"webcam_size_percent":25.5}"#, non_integer_size),
            (r#"{"webcam_size_percent":null}"#, non_integer_size),
            (r#"{"webcam_shape":"oval"}"#, "unknown webcam shape: oval"),
            (
                r#"{"webcam_shape":"Circle"}"#,
                "unknown webcam shape: Circle",
            ),
            (
                r#"{"webcam_shape":false}"#,
                "the webcam shape preference must be text",
            ),
            (
                r#"{"webcam_shape":42}"#,
                "the webcam shape preference must be text",
            ),
            (
                r#"{"webcam_shape":null}"#,
                "the webcam shape preference must be text",
            ),
            (
                r#"{"webcam_shape":{}}"#,
                "the webcam shape preference must be text",
            ),
            (
                r#"{"webcam_shape":[]}"#,
                "the webcam shape preference must be text",
            ),
        ] {
            std::fs::write(&path, invalid).expect("invalid preference should be written");
            assert_eq!(
                webcam_configuration_from(&path),
                Err(expected.to_owned()),
                "invalid settings must report their cause: {invalid}"
            );
            assert_eq!(
                std::fs::read_to_string(&path).expect("invalid preferences should remain readable"),
                invalid,
                "rejecting preferences must not rewrite them"
            );
        }
    }

    #[test]
    fn webcam_preferences_reject_control_characters_at_either_end() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        let inputs = [
            r#"{"webcam_device":"\t/dev/video1"}"#,
            r#"{"webcam_device":"\n/dev/video1"}"#,
            r#"{"webcam_device":"/dev/video1\t"}"#,
            r#"{"webcam_device":"/dev/video1\n"}"#,
            r#"{"webcam_device":"/dev/video1\r"}"#,
            r#"{"webcam_device":" \t/dev/video1 "}"#,
        ];
        let results: Vec<_> = inputs
            .iter()
            .map(|input| {
                std::fs::write(&path, input).expect("invalid preference should be written");
                webcam_configuration_from(&path)
            })
            .collect();
        let expected =
            vec![
                Err("webcam device must be an absolute path without control characters".to_owned());
                inputs.len()
            ];
        assert_eq!(
            results, expected,
            "saved device paths with control characters at either end must be rejected"
        );
    }

    #[test]
    fn webcam_configuration_preserves_malformed_settings_on_write_failure() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        let malformed = r#"{"theme":"Dark","#;
        std::fs::write(&path, malformed).expect("malformed preferences should be written");

        let error = set_webcam_configuration_in(&path, &WebcamConfiguration::default())
            .expect_err("malformed preferences should prevent webcam settings from being saved");
        assert!(
            error.starts_with(&format!("could not parse settings at {}:", path.display()))
                && error.contains("EOF while parsing"),
            "the error should identify the malformed settings file: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("original preferences should remain readable"),
            malformed
        );
    }

    #[test]
    fn copying_captures_defaults_on_and_preserves_other_preferences() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        assert!(
            copy_capture_to_clipboard_from(&path).expect("default should be available"),
            "new installations should copy captures like the Windows app"
        );
        set_theme_in(&path, "Dark").expect("theme should be saved");
        set_copy_capture_to_clipboard_in(&path, false).expect("opt-out should be saved");
        assert!(
            !copy_capture_to_clipboard_from(&path).expect("opt-out should be readable"),
            "disabled clipboard copying should persist"
        );
        assert_eq!(
            theme_from_settings(&read_settings(&path).expect("settings should remain readable"))
                .expect("theme should remain valid"),
            "Dark"
        );
        set_copy_capture_to_clipboard_in(&path, true).expect("opt-in should be saved");
        assert!(
            copy_capture_to_clipboard_from(&path).expect("opt-in should be readable"),
            "clipboard copying should be re-enabled"
        );
    }

    #[test]
    fn invalid_capture_clipboard_preference_is_reported() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        std::fs::write(&path, r#"{"copy_to_clipboard":"yes"}"#)
            .expect("invalid preference should be written");
        let error = copy_capture_to_clipboard_from(&path)
            .expect_err("a non-boolean preference must not silently enable copying");
        assert!(error.contains("must be a boolean"), "{error}");
    }

    #[test]
    fn capture_delay_defaults_to_zero_and_preserves_other_preferences() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        assert_eq!(
            capture_delay_seconds_from(&path).expect("default delay should be readable"),
            0
        );
        set_theme_in(&path, "Dark").expect("theme should be saved");
        for seconds in [1, 60, 0] {
            set_capture_delay_seconds_in(&path, seconds).expect("valid delay should be saved");
            assert_eq!(
                capture_delay_seconds_from(&path).expect("saved delay should be readable"),
                seconds
            );
        }
        assert_eq!(
            theme_from_settings(&read_settings(&path).expect("settings should remain readable"))
                .expect("theme should remain valid"),
            "Dark"
        );
        let previous = std::fs::read(&path).expect("settings should be readable");
        let error = set_capture_delay_seconds_in(&path, 61)
            .expect_err("an out-of-range delay should be rejected");
        assert!(error.contains("between 0 and 60"), "{error}");
        assert_eq!(
            std::fs::read(&path).expect("settings should remain readable"),
            previous,
            "invalid input must not overwrite saved preferences"
        );
    }

    #[test]
    fn invalid_capture_delays_are_reported() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        for value in ["-1", "61", "1.5", "\"3\"", "null"] {
            std::fs::write(&path, format!(r#"{{"capture_delay_seconds":{value}}}"#))
                .expect("invalid preference should be written");
            assert!(
                capture_delay_seconds_from(&path).is_err(),
                "invalid delay must be reported: {value}"
            );
        }
    }

    #[test]
    fn cursor_preferences_default_on_and_round_trip_independently() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        for key in [CAPTURE_CURSOR_KEY, RECORDING_CURSOR_KEY] {
            assert_eq!(
                cursor_from(&path, key).expect("default cursor setting should be readable"),
                CaptureCursor::Included
            );
        }
        set_theme_in(&path, "Dark").expect("theme should be saved");
        for preferences in [
            CursorPreferences {
                capture: CaptureCursor::Hidden,
                recording: CaptureCursor::Included,
            },
            CursorPreferences {
                capture: CaptureCursor::Included,
                recording: CaptureCursor::Hidden,
            },
        ] {
            set_cursor_preferences_in(&path, preferences)
                .expect("cursor preferences should be saved");
            assert_eq!(
                cursor_from(&path, CAPTURE_CURSOR_KEY).expect("capture cursor should be readable"),
                preferences.capture
            );
            assert_eq!(
                cursor_from(&path, RECORDING_CURSOR_KEY)
                    .expect("recording cursor should be readable"),
                preferences.recording
            );
        }
        assert_eq!(
            theme_from_settings(&read_settings(&path).expect("settings should remain readable"))
                .expect("theme should remain valid"),
            "Dark"
        );
    }

    #[test]
    fn invalid_cursor_settings_are_reported_without_affecting_the_other_option() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        for (invalid, valid) in [
            (CAPTURE_CURSOR_KEY, RECORDING_CURSOR_KEY),
            (RECORDING_CURSOR_KEY, CAPTURE_CURSOR_KEY),
        ] {
            std::fs::write(&path, format!(r#"{{"{invalid}":"yes","{valid}":false}}"#))
                .expect("invalid preference should be written");
            let error = cursor_from(&path, invalid).expect_err("invalid cursor setting must fail");
            assert!(error.contains("must be a boolean"), "{error}");
            assert_eq!(
                cursor_from(&path, valid).expect("other cursor setting should remain readable"),
                CaptureCursor::Hidden
            );
        }
    }

    #[test]
    fn setting_capture_directory_preserves_other_preferences() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        let mut settings = Map::new();
        settings.insert("theme".to_owned(), Value::String("Dark".to_owned()));
        write_settings(&path, &settings).expect("initial preferences should be written");

        settings.insert(
            CAPTURE_DIRECTORY_KEY.to_owned(),
            Value::String("/tmp/Jiffshot captures".to_owned()),
        );
        write_settings(&path, &settings).expect("capture directory preference should be written");

        let saved = read_settings(&path).expect("preferences should be readable");
        assert_eq!(
            saved[CAPTURE_DIRECTORY_KEY], "/tmp/Jiffshot captures",
            "capture directory should round-trip with spaces"
        );
        assert_eq!(
            saved["theme"], "Dark",
            "saving the capture directory should preserve other preferences"
        );
    }

    #[test]
    fn rejects_malformed_settings_instead_of_overwriting_them() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        std::fs::write(&path, b"{not-json").expect("malformed fixture should be written");

        assert!(
            read_settings(&path).is_err(),
            "malformed settings must be reported"
        );
        let contents = std::fs::read(&path).expect("malformed settings should remain intact");
        assert_eq!(
            contents, b"{not-json",
            "failed reads must not overwrite settings"
        );
    }

    #[test]
    fn missing_settings_produce_an_empty_preference_set() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);

        assert!(
            read_settings(&path)
                .expect("missing settings should use defaults")
                .is_empty(),
            "missing settings should not require a first-run file"
        );
        assert_eq!(
            capture_directory_from(&path, Path::new("/tmp/default"))
                .expect("missing settings should select the default folder"),
            PathBuf::from("/tmp/default"),
            "the default capture directory should be used until configured"
        );
    }

    #[test]
    fn configured_capture_directory_overrides_the_default() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        let mut settings = Map::new();
        settings.insert(
            CAPTURE_DIRECTORY_KEY.to_owned(),
            Value::String("/tmp/custom captures".to_owned()),
        );
        write_settings(&path, &settings).expect("preferences should be written");

        assert_eq!(
            capture_directory_from(&path, Path::new("/tmp/default"))
                .expect("configured capture directory should load"),
            PathBuf::from("/tmp/custom captures"),
            "the configured folder should take precedence over the default"
        );
    }

    #[test]
    fn theme_settings_round_trip_and_reject_unsupported_variants() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        let mut settings = Map::new();
        settings.insert(
            CAPTURE_DIRECTORY_KEY.to_owned(),
            Value::String("/tmp/custom captures".to_owned()),
        );
        write_settings(&path, &settings).expect("other preferences should be written");
        set_theme_in(&path, "Light").expect("supported theme should save");

        let saved = read_settings(&path).expect("preferences should be readable");
        assert_eq!(
            theme_from_settings(&saved).expect("supported theme should load"),
            "Light",
            "the selected theme should persist"
        );
        assert_eq!(
            saved[CAPTURE_DIRECTORY_KEY], "/tmp/custom captures",
            "setting the theme should preserve the capture folder"
        );
        assert_eq!(
            theme_from_settings(&Map::new()).expect("missing theme should use the system"),
            "System",
            "system theme should be the first-run default"
        );
        assert!(
            set_theme_in(&path, "Automatic").is_err(),
            "unsupported theme names should not be saved"
        );
        assert_eq!(
            theme_from_settings(&read_settings(&path).expect("preferences should be readable"))
                .expect("previous preference should remain valid"),
            "Light",
            "rejecting an unsupported theme must preserve the previous setting"
        );
    }

    #[test]
    fn global_capture_shortcut_persists_without_overwriting_other_settings() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        assert_eq!(
            global_capture_shortcut_from(&path).unwrap(),
            DEFAULT_GLOBAL_CAPTURE_SHORTCUT
        );
        assert!(!global_recording_shortcuts_from(&path).unwrap());
        assert!(!global_shortcuts_enabled_from(&path).unwrap());
        set_theme_in(&path, "Dark").unwrap();
        assert_eq!(
            set_global_shortcuts_in(
                &path,
                "  CTRL+ALT+P  ",
                ShortcutOptions {
                    recording_controls: true,
                    ..ShortcutOptions::default()
                },
            )
            .unwrap(),
            "CTRL+ALT+P"
        );
        assert_eq!(global_capture_shortcut_from(&path).unwrap(), "CTRL+ALT+P");
        assert!(global_recording_shortcuts_from(&path).unwrap());
        assert!(global_shortcuts_enabled_from(&path).unwrap());
        assert_eq!(read_settings(&path).unwrap()[THEME_KEY], "Dark");
        disable_global_shortcuts_in(&path).unwrap();
        assert!(!global_shortcuts_enabled_from(&path).unwrap());
        assert_eq!(global_capture_shortcut_from(&path).unwrap(), "CTRL+ALT+P");
        assert!(global_recording_shortcuts_from(&path).unwrap());
        assert_eq!(read_settings(&path).unwrap()[THEME_KEY], "Dark");
        set_global_shortcuts_in(&path, "CTRL+ALT+P", ShortcutOptions::default()).unwrap();
        assert!(!global_recording_shortcuts_from(&path).unwrap());
        assert!(global_shortcuts_enabled_from(&path).unwrap());
    }

    #[test]
    fn invalid_global_capture_shortcuts_do_not_replace_saved_settings() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        set_global_shortcuts_in(
            &path,
            "CTRL+ALT+P",
            ShortcutOptions {
                recording_controls: true,
                ..ShortcutOptions::default()
            },
        )
        .unwrap();
        for invalid in ["", "  ", "CTRL+\nP", "\tCTRL+P", &"X".repeat(129)] {
            assert!(set_global_shortcuts_in(&path, invalid, ShortcutOptions::default()).is_err());
            assert_eq!(global_capture_shortcut_from(&path).unwrap(), "CTRL+ALT+P");
            assert!(global_recording_shortcuts_from(&path).unwrap());
        }
        std::fs::write(&path, br#"{"global_capture_shortcut":42}"#).unwrap();
        assert!(global_capture_shortcut_from(&path).is_err());
        std::fs::write(&path, br#"{"global_recording_shortcuts":"yes"}"#).unwrap();
        assert!(global_recording_shortcuts_from(&path).is_err());
        std::fs::write(&path, br#"{"linux_global_shortcuts_enabled":"yes"}"#).unwrap();
        assert!(global_shortcuts_enabled_from(&path).is_err());
    }

    #[test]
    fn recording_start_shortcut_settings_preserve_opt_in_and_audio_choice() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join(SETTINGS_FILE);
        assert_eq!(
            global_recording_start_from(&path).unwrap(),
            RecordingStartShortcut::Disabled,
            "existing users must not gain a recording-start binding"
        );
        set_theme_in(&path, "Dark").unwrap();
        for choice in RecordingStartShortcut::ALL {
            set_global_shortcuts_in(
                &path,
                "CTRL+ALT+P",
                ShortcutOptions {
                    recording_controls: true,
                    recording_start: choice,
                },
            )
            .unwrap();
            assert_eq!(global_recording_start_from(&path).unwrap(), choice);
            assert_eq!(read_settings(&path).unwrap()[THEME_KEY], "Dark");
        }
        disable_global_shortcuts_in(&path).unwrap();
        assert_eq!(
            global_recording_start_from(&path).unwrap(),
            RecordingStartShortcut::SystemAndMicrophone,
            "disabling bindings must preserve the configured audio choice"
        );
        for malformed in [
            br#"{"linux_global_recording_start":true}"#.as_slice(),
            br#"{"linux_global_recording_start":"invalid"}"#,
        ] {
            std::fs::write(&path, malformed).unwrap();
            assert!(global_recording_start_from(&path).is_err());
        }
    }
}

#[cfg(test)]
#[path = "../tests/recording_frame_rate/settings_tests.rs"]
mod recording_frame_rate_regression;
