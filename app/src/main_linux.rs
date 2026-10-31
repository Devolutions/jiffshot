mod autostart_linux;
mod canvas;
mod capture_linux;
mod clipboard_linux;
mod editor_linux;
mod file_dialog_linux;
mod frame_stream_linux;
mod interval_linux;
mod launch_linux;
mod library_linux;
mod library_tags;
mod notification_linux;
mod ocr_linux;
mod portal_frame_linux;
mod portal_linux;
mod schedule_linux;
mod screenshot_linux;
mod scroll_linux;
mod settings_linux;
mod share_linux;
mod shortcut_linux;
mod template_linux;
mod trace;
mod tray_linux;
#[path = "ui_linux.rs"]
#[allow(dead_code)]
mod ui;
mod ui_only;
mod video_linux;

use file_dialog_linux::PickerOutcome;
use jiffshot_doc::combine::Arrangement;
use jiffshot_doc::editor::{Editor, Tool, TOOLS};
use jiffshot_doc::{effects, Document, Rgba};
use rustolonia::storage::{
    FileTypeFilter, FolderPickerOptions, OpenFilePickerOptions, SaveFilePickerOptions,
    WellKnownFolder,
};
use rustolonia::{
    AnyEl, App, AppScope, Button, CheckBox, ComboBox, ComboBoxItem, Dock, DockPanel, El, Key,
    NodeRef, Orientation, Result, ScrollBarVisibility, ScrollViewer, Signal, StackPanel, TextBlock,
    TextBox, ThemeVariant, Window, WindowCloseReason, WrapPanel,
};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;

static RECORDING: parking_lot::Mutex<
    Option<async_channel::Sender<capture_linux::RecordingCommand>>,
> = parking_lot::Mutex::new(None);
struct ActiveScrollCapture {
    mode: scroll_linux::Mode,
    stop: async_channel::Sender<scroll_linux::Action>,
}

static SCROLL_CAPTURE: parking_lot::Mutex<Option<ActiveScrollCapture>> =
    parking_lot::Mutex::new(None);
static INTERVAL_CAPTURE: parking_lot::Mutex<Option<async_channel::Sender<()>>> =
    parking_lot::Mutex::new(None);
static GLOBAL_SHORTCUT_STOP: parking_lot::Mutex<Option<async_channel::Sender<()>>> =
    parking_lot::Mutex::new(None);
struct DelayedCapture {
    id: u64,
    cancel: SyncSender<()>,
}
static DELAYED_CAPTURE: parking_lot::Mutex<Option<DelayedCapture>> = parking_lot::Mutex::new(None);
static NEXT_DELAYED_CAPTURE: AtomicU64 = AtomicU64::new(0);
struct ActiveSchedule {
    id: u64,
    stop: SyncSender<()>,
}
static DAILY_SCHEDULE: parking_lot::Mutex<Option<ActiveSchedule>> = parking_lot::Mutex::new(None);
static NEXT_SCHEDULE_ID: AtomicU64 = AtomicU64::new(0);
static SCHEDULED_CAPTURE_ACTIVE: AtomicBool = AtomicBool::new(false);
static SINGLE_CAPTURES_ACTIVE: AtomicUsize = AtomicUsize::new(0);
static LIBRARY_REQUEST: AtomicU64 = AtomicU64::new(0);
static LIBRARY_OPEN_REQUEST: AtomicU64 = AtomicU64::new(0);
static AUTOMATIC_COPY_REQUEST: AtomicU64 = AtomicU64::new(0);
static DOCUMENT_LOAD_REQUESTS: editor_linux::DocumentLoadRequests =
    editor_linux::DocumentLoadRequests::new();
static DOCUMENT_SOURCE: parking_lot::Mutex<Option<PathBuf>> = parking_lot::Mutex::new(None);
struct ActiveTray {
    id: u64,
    session: tray_linux::Session,
}
static TRAY: parking_lot::Mutex<Option<ActiveTray>> = parking_lot::Mutex::new(None);
static NEXT_TRAY_ID: AtomicU64 = AtomicU64::new(0);
static WINDOW_HIDDEN: AtomicBool = AtomicBool::new(false);
const INITIAL_WINDOW_HEIGHT: f64 = 800.0;
const CONTROLS_HEIGHT_FRACTION: f64 = 0.55;
const IMAGE_EXPORT_EXTENSIONS: [&str; 7] = ["png", "jpg", "jpeg", "bmp", "gif", "webp", "pdf"];

struct SingleCaptureGuard;

impl SingleCaptureGuard {
    fn new() -> Self {
        SINGLE_CAPTURES_ACTIVE.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

impl Drop for SingleCaptureGuard {
    fn drop(&mut self) {
        SINGLE_CAPTURES_ACTIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

struct ScheduledCaptureGuard;

impl Drop for ScheduledCaptureGuard {
    fn drop(&mut self) {
        SCHEDULED_CAPTURE_ACTIVE.store(false, Ordering::Release);
    }
}

struct VideoPlayback {
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
}
static VIDEO_PLAYBACK: parking_lot::Mutex<Option<VideoPlayback>> = parking_lot::Mutex::new(None);

#[derive(Clone, Copy)]
enum DocumentOcrAction {
    CopyText,
    SmartRedact,
    Simplify,
}

enum DocumentOcrResult {
    CopiedText(usize),
    Redact(Vec<jiffshot_doc::smart::TextLine>),
    Simplified(Document, usize),
    NoText,
}

#[derive(Clone, Copy)]
enum DocumentLoadKind {
    File,
    VideoFrame(f64),
}

enum DocumentLoadDelivery {
    Loaded(String),
    Failed(String),
    Superseded,
    DocumentChanged,
}

#[derive(Clone, Copy)]
struct EditorControls {
    visible: Signal<bool>,
    text: Signal<String>,
    stroke: Signal<String>,
    fill: Signal<String>,
    text_color: Signal<String>,
    width: Signal<String>,
    font: Signal<String>,
    font_size: Signal<String>,
    opacity: Signal<String>,
    bold: Signal<bool>,
    italic: Signal<bool>,
    shadow: Signal<bool>,
    effect_index: Signal<i32>,
    effect_parameter: Signal<String>,
}

#[derive(Clone, Copy)]
struct TemplateControls {
    visible: Signal<bool>,
    layout: Signal<i32>,
    theme: Signal<i32>,
    captions: Signal<bool>,
    numbers: Signal<bool>,
    footer: Signal<String>,
}

#[derive(Clone, Copy)]
struct ScreenPresetControls {
    visible: Signal<bool>,
    name: Signal<String>,
    delay: Signal<String>,
    cursor: Signal<bool>,
    presets: Signal<Vec<settings_linux::ScreenCapturePreset>>,
}

impl ScreenPresetControls {
    fn new(
        presets: Vec<settings_linux::ScreenCapturePreset>,
        delay: u64,
        cursor: capture_linux::CaptureCursor,
    ) -> Self {
        Self {
            visible: Signal::new(false),
            name: Signal::new(String::new()),
            delay: Signal::new(delay.to_string()),
            cursor: Signal::new(cursor.included()),
            presets: Signal::new(presets),
        }
    }
}

impl TemplateControls {
    fn new() -> Self {
        Self {
            visible: Signal::new(false),
            layout: Signal::new(0),
            theme: Signal::new(0),
            captions: Signal::new(false),
            numbers: Signal::new(false),
            footer: Signal::new(String::new()),
        }
    }
}

impl EditorControls {
    fn new() -> Self {
        Self {
            visible: Signal::new(false),
            text: Signal::new(String::new()),
            stroke: Signal::new(String::new()),
            fill: Signal::new(String::new()),
            text_color: Signal::new(String::new()),
            width: Signal::new(String::new()),
            font: Signal::new(String::new()),
            font_size: Signal::new(String::new()),
            opacity: Signal::new(String::new()),
            bold: Signal::new(false),
            italic: Signal::new(false),
            shadow: Signal::new(false),
            effect_index: Signal::new(0),
            effect_parameter: Signal::new(String::new()),
        }
    }

    fn synchronize(self) {
        let Some((style, text)) = canvas::with_editor(|editor| {
            (
                editor.current_style(),
                editor
                    .selected()
                    .and_then(|object| object.text())
                    .unwrap_or_default()
                    .to_owned(),
            )
        }) else {
            return;
        };
        self.text.set_if_changed(text);
        self.stroke.set_if_changed(style.stroke.to_hex());
        self.fill.set_if_changed(style.fill.to_hex());
        self.text_color.set_if_changed(style.text_color.to_hex());
        self.width.set_if_changed(style.width.to_string());
        self.font.set_if_changed(style.font);
        self.font_size.set_if_changed(style.font_size.to_string());
        self.opacity
            .set_if_changed((style.opacity * 100.0).to_string());
        self.bold.set_if_changed(style.bold);
        self.italic.set_if_changed(style.italic);
        self.shadow.set_if_changed(style.shadow);
    }

    fn style_inputs(self) -> editor_linux::StyleInputs {
        editor_linux::StyleInputs {
            stroke: self.stroke.get(),
            fill: self.fill.get(),
            text_color: self.text_color.get(),
            width: self.width.get(),
            font: self.font.get(),
            font_size: self.font_size.get(),
            opacity: self.opacity.get(),
            bold: self.bold.get(),
            italic: self.italic.get(),
            shadow: self.shadow.get(),
        }
    }
}

#[derive(Clone, Copy)]
struct LibraryControls {
    visible: Signal<bool>,
    selected: Signal<Option<PathBuf>>,
    tags: Signal<String>,
    pinned: Signal<bool>,
    query: Signal<String>,
    active_query: Signal<String>,
    filter: Signal<library_linux::CaptureFilter>,
    recent: Signal<bool>,
}

impl LibraryControls {
    fn new() -> Self {
        Self {
            visible: Signal::new(false),
            selected: Signal::new(None),
            tags: Signal::new(String::new()),
            pinned: Signal::new(false),
            query: Signal::new(String::new()),
            active_query: Signal::new(String::new()),
            filter: Signal::new(library_linux::CaptureFilter::All),
            recent: Signal::new(true),
        }
    }

    fn clear_selection(self) {
        self.selected.set_if_changed(None);
        self.tags.set_if_changed(String::new());
        self.pinned.set_if_changed(false);
    }

    fn selection_label(self) -> String {
        match self.selected.get() {
            Some(path) => format!(
                "Selected: {}{}",
                path.display(),
                if self.pinned.get() { " (Pinned)" } else { "" }
            ),
            None => "Select a capture in the library".to_owned(),
        }
    }
}

#[derive(Clone)]
struct ShortcutUi {
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    captures: Signal<Vec<PathBuf>>,
    library: LibraryControls,
    status: Signal<String>,
    preferred: Signal<String>,
    recording_controls: Signal<bool>,
    recording_start: Signal<i32>,
    webcam: Signal<bool>,
}

fn main() -> Result<()> {
    let launch = match launch_linux::parse(std::env::args_os().skip(1)) {
        Ok(launch) => launch,
        Err(error) => {
            eprintln!("jiffshot: {error}\n\n{}", launch_linux::USAGE);
            std::process::exit(2);
        }
    };
    match launch {
        launch_linux::Launch::Help => {
            println!("{}", launch_linux::USAGE);
            return Ok(());
        }
        launch_linux::Launch::Version => {
            println!("jiffshot {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        _ => {}
    }
    App::load_from_env()?.run(move |scope| {
        let document = Document::blank(1280, 720, Rgba::WHITE);
        let mut initial_status = "Open an image or .jsd document to start editing.".to_owned();
        match settings_linux::theme() {
            Ok(theme) => {
                if let Err(error) = scope.set_requested_theme_variant(theme_variant(&theme)) {
                    initial_status = format!("{initial_status} Could not apply theme: {error}");
                }
            }
            Err(error) => {
                initial_status =
                    format!("{initial_status} Could not load theme preference: {error}");
            }
        }
        let mut shortcuts_valid = true;
        let preferred_shortcut = match settings_linux::global_capture_shortcut() {
            Ok(shortcut) => shortcut,
            Err(error) => {
                shortcuts_valid = false;
                initial_status = format!(
                    "{initial_status} Could not load the global shortcut preference: {error}"
                );
                settings_linux::DEFAULT_GLOBAL_CAPTURE_SHORTCUT.to_owned()
            }
        };
        let recording_shortcut_preference = match settings_linux::global_recording_shortcuts() {
            Ok(enabled) => enabled,
            Err(error) => {
                shortcuts_valid = false;
                initial_status = format!(
                    "{initial_status} Could not load the recording shortcut preference: {error}"
                );
                false
            }
        };
        let recording_start_preference = match settings_linux::global_recording_start() {
            Ok(choice) => choice,
            Err(error) => {
                shortcuts_valid = false;
                initial_status = format!(
                    "{initial_status} Could not load the recording start shortcut preference: {error}"
                );
                shortcut_linux::RecordingStartShortcut::Disabled
            }
        };
        let shortcut_preference = match settings_linux::global_shortcuts_enabled() {
            Ok(enabled) => enabled,
            Err(error) => {
                initial_status = format!(
                    "{initial_status} Could not load the global shortcuts enabled preference: {error}"
                );
                false
            }
        };
        let copy_capture_preference = match settings_linux::copy_capture_to_clipboard() {
            Ok(enabled) => enabled,
            Err(error) => {
                initial_status = format!(
                    "{initial_status} Could not load the capture clipboard preference: {error}"
                );
                false
            }
        };
        let tray_preference = match settings_linux::tray_enabled() {
            Ok(enabled) => enabled,
            Err(error) => {
                initial_status =
                    format!("{initial_status} Could not load the tray preference: {error}");
                false
            }
        };
        let capture_delay_preference = match settings_linux::capture_delay_seconds() {
            Ok(seconds) => seconds,
            Err(error) => {
                initial_status =
                    format!("{initial_status} Could not load the capture delay: {error}");
                0
            }
        };
        let screen_capture_presets = match settings_linux::screen_capture_presets() {
            Ok(presets) => presets,
            Err(error) => {
                initial_status =
                    format!("Could not load screen capture presets: {error}. {initial_status}");
                Vec::new()
            }
        };
        let interval_preference = match settings_linux::interval_preferences() {
            Ok(preferences) => preferences,
            Err(error) => {
                initial_status =
                    format!("{initial_status} Could not load interval preferences: {error}");
                settings_linux::IntervalPreferences::default()
            }
        };
        let schedule_preference = match settings_linux::schedule_preferences() {
            Ok(preferences) => preferences,
            Err(error) => {
                initial_status =
                    format!("{initial_status} Could not load the daily capture schedule: {error}");
                settings_linux::SchedulePreferences::default()
            }
        };
        let default_cursors = settings_linux::CursorPreferences::default();
        let capture_cursor_preference = match settings_linux::capture_cursor() {
            Ok(cursor) => cursor,
            Err(error) => {
                initial_status =
                    format!("{initial_status} Could not load the capture cursor setting: {error}");
                default_cursors.capture
            }
        };
        let recording_cursor_preference = match settings_linux::recording_cursor() {
            Ok(cursor) => cursor,
            Err(error) => {
                initial_status = format!(
                    "{initial_status} Could not load the recording cursor setting: {error}"
                );
                default_cursors.recording
            }
        };
        let recording_frame_rate_preference = match settings_linux::recording_frame_rate() {
            Ok(frame_rate) => frame_rate,
            Err(error) => {
                initial_status = format!(
                    "{initial_status} Could not load the recording frame rate: {error}"
                );
                capture_linux::RecordingFrameRate::Source
            }
        };
        let webcam_preference = match settings_linux::webcam_configuration() {
            Ok(configuration) => configuration,
            Err(error) => {
                initial_status =
                    format!("{initial_status} Could not load webcam preferences: {error}");
                capture_linux::WebcamConfiguration::default()
            }
        };
        let mut editor = Editor::new(document);
        match ocr_linux::Recognizer::new() {
            Ok(recognizer) => editor.analyzer = Some(Box::new(recognizer)),
            Err(error) => {
                initial_status = format!("{initial_status} OCR unavailable: {error}");
            }
        }
        *canvas::EDITOR.lock() = Some(editor);

        let view = ui::canvas_view::CanvasView::new(scope)?;
        let canvas = ui::UiOnly::new(view.clone());
        let recent = match capture_linux::recent_captures() {
            Ok(recent) => recent,
            Err(error) => {
                initial_status = format!("{initial_status} Capture library unavailable: {error}");
                Vec::new()
            }
        };
        let captures = Signal::new(recent);
        let status = Signal::new(initial_status);
        let initial_load = match &launch {
            launch_linux::Launch::Editor(Some(_)) => begin_document_load(status),
            _ => None,
        };
        let webcam = Signal::new(false);
        let horizontal_scrolling = Signal::new(false);
        let preferred_shortcut = Signal::new(preferred_shortcut);
        let recording_shortcuts = Signal::new(recording_shortcut_preference);
        let recording_start = Signal::new(recording_start_preference.index());
        let copy_capture = Signal::new(copy_capture_preference);
        let capture_delay = Signal::new(capture_delay_preference.to_string());
        let interval_visible = Signal::new(false);
        let interval_seconds = Signal::new(interval_preference.seconds().to_string());
        let interval_skip_identical = Signal::new(interval_preference.skip_identical());
        let schedule_visible = Signal::new(false);
        let schedule_time = Signal::new(schedule_preference.time.to_string());
        let schedule_enabled = Signal::new(schedule_preference.enabled);
        let capture_cursor = Signal::new(capture_cursor_preference.included());
        let recording_cursor = Signal::new(recording_cursor_preference.included());
        let recording_frame_rate = Signal::new(recording_frame_rate_preference.index());
        let webcam_device = Signal::new(webcam_preference.device().to_owned());
        let webcam_size = Signal::new(webcam_preference.layout().size_percent().to_string());
        let webcam_corner = Signal::new(
            webcam_preference.layout().corner().index()
                + if webcam_preference.layout().shape() == capture_linux::WebcamShape::Circle {
                    4
                } else {
                    0
                },
        );
        let window_height = Signal::new(INITIAL_WINDOW_HEIGHT);
        let library = LibraryControls::new();
        let shortcut_ui = ShortcutUi {
            canvas: canvas.clone(),
            captures,
            library,
            status,
            preferred: preferred_shortcut,
            recording_controls: recording_shortcuts,
            recording_start,
            webcam,
        };
        let window_ref = NodeRef::<Window>::new();
        let tray_ui = TrayUi {
            scope: scope.clone(),
            window: window_ref,
            canvas: Arc::new(canvas.clone()),
            status,
            captures,
            library,
        };
        let editor_controls = EditorControls::new();
        let template_controls = TemplateControls::new();
        let preset_controls = ScreenPresetControls::new(
            screen_capture_presets,
            capture_delay_preference,
            capture_cursor_preference,
        );
        editor_controls.synchronize();
        let changed_status = status;
        view.on_changed(move |flags| {
            if flags & ui::canvas_view::FLAG_CHANGED != 0 {
                editor_controls.synchronize();
                changed_status.set_if_changed("Document changed".to_owned());
            }
            if flags & canvas::FLAG_FOCUS_TEXT != 0 {
                editor_controls.visible.set_if_changed(true);
                changed_status.set_if_changed(
                    "Edit the selected annotation using the text field above.".to_owned(),
                );
            }
        });
        view.on_edit_requested(move || {
            editor_controls.synchronize();
            editor_controls.visible.set_if_changed(true);
            status.set_if_changed(
                "Edit the selected annotation using the text field above.".to_owned(),
            );
        });
        let capture_toolbar = WrapPanel::el()
            .orientation(Orientation::Horizontal)
            .child(action_button("Capture screen or window…", {
                let canvas = canvas.clone();
                move |scope| capture_screen(scope, canvas.clone(), status, captures, library)
            }))
            .child(action_button("Scrolling capture… (start/finish)", {
                let canvas = canvas.clone();
                move |scope| {
                    let direction = if horizontal_scrolling.get() {
                        scroll_linux::Direction::Horizontal
                    } else {
                        scroll_linux::Direction::Vertical
                    };
                    toggle_scrolling_capture(
                        scope,
                        canvas.clone(),
                        status,
                        captures,
                        library,
                        scroll_linux::Mode::Scrolling(direction),
                    )
                }
            }))
            .child(action_button("Cancel scrolling", move |_| {
                cancel_scrolling_capture(status)
            })
            .tool_tip_tip("Cancel the active scrolling or panoramic capture"))
            .child(action_button("Open captures folder", {
                move |scope| open_capture_folder(scope, status)
            }))
            .child(action_button("Choose captures folder…", {
                move |scope| choose_capture_folder(scope, captures, library, status)
            }))
            .child(action_button("Record screen… (start/stop)", {
                move |scope| {
                    toggle_screen_recording(
                        scope,
                        status,
                        captures,
                        library,
                        capture_linux::RecordingAudio::Disabled,
                        webcam.get(),
                    )
                }
            }))
            .child(
                CheckBox::el()
                    .content("Webcam overlay")
                    .checked(webcam.two_way())
                    .tool_tip_tip("Include the configured webcam in the next recording"),
            )
            .child(action_button("Pause/resume recording", move |_| {
                toggle_recording_pause(status)
            }));
        let capture_actions_toolbar = WrapPanel::el()
            .orientation(Orientation::Horizontal)
            .child(TextBlock::el().text("Preferred capture shortcut:"))
            .child(
                TextBox::el()
                    .width(150.0)
                    .text(preferred_shortcut.two_way())
                    .tool_tip_tip(
                        "For example, CTRL+SHIFT+X; the desktop chooses the final binding",
                    ),
            )
            .child(action_button("Enable global capture shortcut…", {
                let shortcut_ui = shortcut_ui.clone();
                move |scope| enable_global_capture_shortcut(scope, shortcut_ui.clone())
            }))
            .child(action_button("Disable global shortcut", move |_| {
                disable_global_capture_shortcut(status)
            }))
            .child(
                CheckBox::el()
                    .content("Horizontal scrolling")
                    .checked(horizontal_scrolling.two_way())
                    .tool_tip_tip("Scroll right instead of down during the next scrolling capture"),
            )
            .child(action_button("Record screen + microphone…", {
                move |scope| {
                    toggle_screen_recording(
                        scope,
                        status,
                        captures,
                        library,
                        capture_linux::RecordingAudio::Microphone,
                        webcam.get(),
                    )
                }
            }))
            .child(action_button("Record screen + system sound…", {
                move |scope| {
                    toggle_screen_recording(
                        scope,
                        status,
                        captures,
                        library,
                        capture_linux::RecordingAudio::System,
                        webcam.get(),
                    )
                }
            }))
            .child(action_button(
                "Record screen + system sound + microphone…",
                {
                    move |scope| {
                        toggle_screen_recording(
                            scope,
                            status,
                            captures,
                            library,
                            capture_linux::RecordingAudio::SystemAndMicrophone,
                            webcam.get(),
                        )
                    }
                },
            ))
            .child(action_button("Restart recording", move |_| {
                send_recording_command(
                    capture_linux::RecordingCommand::Restart,
                    "Restarting screen recording…",
                    status,
                )
            }))
            .child(action_button("Cancel recording", move |_| {
                send_recording_command(
                    capture_linux::RecordingCommand::Cancel,
                    "Cancelling screen recording…",
                    status,
                )
            }))
            .child(
                CheckBox::el()
                    .content("Include global recording controls")
                    .checked(recording_shortcuts.two_way())
                    .tool_tip_tip(
                        "Include pause/resume and stop when enabling shortcuts; the desktop selects the final keys",
                    ),
            )
            .child(action_button("Panoramic capture… (start/finish)", {
                let canvas = canvas.clone();
                move |scope| {
                    toggle_scrolling_capture(
                        scope,
                        canvas.clone(),
                        status,
                        captures,
                        library,
                        scroll_linux::Mode::Panoramic,
                    )
                }
            }))
            .child(
                StackPanel::el()
                    .orientation(Orientation::Horizontal)
                    .spacing(4.0)
                    .visible(recording_shortcuts)
                    .child(TextBlock::el().text("Recording start shortcut:"))
                    .child(
                        template_selector(
                            shortcut_linux::RecordingStartShortcut::ALL
                                .into_iter()
                                .map(shortcut_linux::RecordingStartShortcut::label),
                            recording_start,
                            status,
                            "recording shortcut audio",
                        )
                        .tool_tip_tip(
                            "Choose audio for an optional start/stop shortcut; disable and re-enable shortcuts to apply",
                        ),
                    ),
            );
        let document_toolbar = WrapPanel::el()
            .orientation(Orientation::Horizontal)
            .child(action_button("Open…", {
                let canvas = canvas.clone();
                move |scope| open_document(scope, canvas.clone(), status)
            }))
            .child(action_button("Play video… / Stop", {
                let canvas = canvas.clone();
                move |scope| toggle_video_playback(scope, canvas.clone(), status)
            }))
            .child(action_button("Copy image", {
                move |scope| copy_image(scope, status)
            }))
            .child(action_button("Paste image", {
                let canvas = canvas.clone();
                move |scope| paste_image(scope, canvas.clone(), status)
            }))
            .child(action_button("Pause/resume video", {
                move |_| toggle_video_pause(status)
            }))
            .child(action_button("Export video as GIF…", {
                move |scope| export_video_dialog(scope, VideoExport::Gif, status)
            }))
            .child(action_button("Save .jsd…", {
                move |scope| save_document(scope, status)
            }))
            .child(action_button("Export…", {
                move |scope| export_image(scope, status)
            }));
        let image_actions_toolbar = WrapPanel::el()
            .orientation(Orientation::Horizontal)
            .child(action_button("Combine horizontal…", {
                let canvas = canvas.clone();
                move |scope| {
                    combine_images_dialog(scope, Arrangement::Horizontal, canvas.clone(), status)
                }
            }))
            .child(action_button("Combine vertical…", {
                let canvas = canvas.clone();
                move |scope| {
                    combine_images_dialog(scope, Arrangement::Vertical, canvas.clone(), status)
                }
            }))
            .child(action_button("Combine as grid…", {
                let canvas = canvas.clone();
                move |scope| combine_images_dialog(scope, Arrangement::Grid, canvas.clone(), status)
            }))
            .child(action_button("Undo", {
                let canvas = canvas.clone();
                move |_| history_action(&canvas, &status, true)
            }))
            .child(action_button("Redo", {
                let canvas = canvas.clone();
                move |_| history_action(&canvas, &status, false)
            }))
            .child(action_button("−", {
                let canvas = canvas.clone();
                move |_| canvas.get().zoom_to("out")
            }))
            .child(action_button("Fit", {
                let canvas = canvas.clone();
                move |_| canvas.get().zoom_to("fit")
            }))
            .child(action_button("+", {
                let canvas = canvas.clone();
                move |_| canvas.get().zoom_to("in")
            }))
            .child(action_button("Pixel grid", {
                let canvas = canvas.clone();
                move |_| {
                    let view = canvas.get();
                    view.set_show_pixel_grid(!view.show_pixel_grid());
                }
            }));
        let tool_palette =
            WrapPanel::el()
                .orientation(Orientation::Horizontal)
                .children_iter(TOOLS.iter().copied().map(|tool| {
                    AnyEl::from(tool_button(tool.name(), tool, canvas.clone(), status))
                }));
        let theme_toolbar = StackPanel::el()
            .orientation(Orientation::Horizontal)
            .spacing(4.0)
            .child(action_button("Theme: System", move |scope| {
                set_linux_theme(scope, "System", status)
            }))
            .child(action_button("Theme: Light", move |scope| {
                set_linux_theme(scope, "Light", status)
            }))
            .child(action_button("Theme: Dark", move |scope| {
                set_linux_theme(scope, "Dark", status)
            }));
        let variant_toolbar = WrapPanel::el()
            .orientation(Orientation::Horizontal)
            .child(action_button("Variant −", {
                let canvas = canvas.clone();
                move |_| cycle_variant(&canvas, &status, false)
            }))
            .child(action_button("Variant +", {
                let canvas = canvas.clone();
                move |_| cycle_variant(&canvas, &status, true)
            }))
            .child(
                CheckBox::el()
                    .content("Editor properties and effects")
                    .checked(editor_controls.visible.two_way()),
            )
            .child(
                CheckBox::el()
                    .content("Templates")
                    .checked(template_controls.visible.two_way()),
            )
            .child(
                CheckBox::el()
                    .content("Screen presets")
                    .checked(preset_controls.visible.two_way()),
            );
        let editor_properties = editor_properties_panel(editor_controls, canvas.clone(), status);
        let template_panel = template_panel(template_controls, canvas.clone(), status);
        let screen_preset_panel =
            screen_preset_panel(preset_controls, canvas.clone(), status, captures, library);
        let trim_start = Signal::new("0".to_owned());
        let trim_end = Signal::new("10".to_owned());
        let frame_time = Signal::new("0".to_owned());
        let video_frame_toolbar = WrapPanel::el()
            .orientation(Orientation::Horizontal)
            .child(TextBlock::el().text("Video frame (seconds):"))
            .child(
                TextBox::el()
                    .width(90.0)
                    .text(frame_time.two_way())
                    .placeholder_text("Time"),
            )
            .child(action_button("Import frame…", {
                let canvas = canvas.clone();
                move |scope| {
                    import_video_frame_dialog(scope, frame_time.get(), canvas.clone(), status)
                }
            }))
            .child(action_button("Combine videos…", {
                move |scope| combine_videos_dialog(scope, status)
            }))
            .child(action_button("Export silent MP4…", {
                move |scope| export_video_dialog(scope, VideoExport::SilentMp4, status)
            }));
        let video_trim_toolbar = WrapPanel::el()
            .orientation(Orientation::Horizontal)
            .child(TextBlock::el().text("Trim range (seconds):"))
            .child(
                TextBox::el()
                    .width(90.0)
                    .text(trim_start.two_way())
                    .placeholder_text("Start"),
            )
            .child(
                TextBox::el()
                    .width(90.0)
                    .text(trim_end.two_way())
                    .placeholder_text("End"),
            )
            .child(action_button("Trim video to MP4…", {
                move |scope| {
                    video_range_dialog(scope, trim_start.get(), trim_end.get(), false, status)
                }
            }))
            .child(action_button("Cut range out…", {
                move |scope| {
                    video_range_dialog(scope, trim_start.get(), trim_end.get(), true, status)
                }
            }));
        let capture_preferences_toolbar = WrapPanel::el()
            .orientation(Orientation::Horizontal)
            .child(
                CheckBox::el()
                    .content("Copy captures to clipboard")
                    .checked(copy_capture.two_way())
                    .tool_tip_tip("Save capture preferences to apply this choice"),
            )
            .child(action_button("Save capture preferences", move |_| {
                match settings_linux::set_copy_capture_to_clipboard(copy_capture.get()) {
                    Ok(()) => status.set_if_changed("Capture preferences saved.".to_owned()),
                    Err(error) => {
                        status.set_if_changed(format!("Could not save capture preferences: {error}"))
                    }
                }
            }))
            .child(action_button("Desktop screenshot chooser…", {
                let canvas = canvas.clone();
                move |scope| {
                    capture_interactive(scope, canvas.clone(), status, captures, library)
                }
            }))
            .child(
                action_button("Grab text…", move |scope| grab_screen_text(scope, status))
                    .tool_tip_tip("Select a screenshot in the desktop chooser and copy its text"),
            )
            .child(action_button("Enable tray", {
                let tray_ui = tray_ui.clone();
                move |_| tray_ui.start(TrayStart::SavePreference)
            })
            .tool_tip_tip("Keep Jiffshot running when its window is closed"))
            .child(
                action_button("Disable tray", move |_| disable_tray(status))
                    .tool_tip_tip("Restore normal window-close behavior"),
            )
            .child(
                action_button("Enable login startup", move |_| {
                    match autostart_linux::enable() {
                        Ok(()) => status.set_if_changed(
                            "Login startup enabled; Jiffshot uses the tray if available.".to_owned(),
                        ),
                        Err(error) => {
                            status.set_if_changed(format!("Could not enable login startup: {error}"))
                        }
                    }
                })
                .tool_tip_tip("Start Jiffshot when you log in to your desktop"),
            )
            .child(
                action_button("Disable login startup", move |_| {
                    match autostart_linux::disable() {
                        Ok(()) => status.set_if_changed("Login startup disabled.".to_owned()),
                        Err(error) => status
                            .set_if_changed(format!("Could not disable login startup: {error}")),
                    }
                })
                .tool_tip_tip("Stop starting Jiffshot when you log in"),
            );
        let cursor_preferences_toolbar = WrapPanel::el()
            .orientation(Orientation::Horizontal)
            .child(
                CheckBox::el()
                    .content("Include cursor in captures")
                    .checked(capture_cursor.two_way())
                    .tool_tip_tip(
                        "Save cursor preferences to apply to screen/window captures; other capture modes use their own cursor behavior",
                    ),
            )
            .child(
                CheckBox::el()
                    .content("Include cursor in recordings")
                    .checked(recording_cursor.two_way())
                    .tool_tip_tip("Save cursor preferences to apply to new recordings"),
            )
            .child(action_button("Save cursor preferences", move |_| {
                let preferences = settings_linux::CursorPreferences {
                    capture: capture_linux::CaptureCursor::from_included(capture_cursor.get()),
                    recording: capture_linux::CaptureCursor::from_included(recording_cursor.get()),
                };
                match settings_linux::set_cursor_preferences(preferences) {
                    Ok(()) => status.set_if_changed("Cursor preferences saved.".to_owned()),
                    Err(error) => {
                        status.set_if_changed(format!("Could not save cursor preferences: {error}"))
                    }
                }
            }));
        let recording_frame_rate_toolbar = WrapPanel::el()
            .orientation(Orientation::Horizontal)
            .child(TextBlock::el().text("Recording frame rate:"))
            .child(
                template_selector(
                    capture_linux::RecordingFrameRate::ALL
                        .into_iter()
                        .map(capture_linux::RecordingFrameRate::label),
                    recording_frame_rate,
                    status,
                    "recording frame rate",
                )
                .tool_tip_tip("Save the frame rate to apply it to new recordings"),
            )
            .child(action_button("Save recording frame rate", move |_| {
                let result = capture_linux::RecordingFrameRate::from_index(recording_frame_rate.get())
                    .and_then(settings_linux::set_recording_frame_rate);
                match result {
                    Ok(()) => status.set_if_changed("Recording frame rate saved.".to_owned()),
                    Err(error) => {
                        status.set_if_changed(format!("Could not save recording frame rate: {error}"))
                    }
                }
            }));
        let capture_delay_toolbar = WrapPanel::el()
            .orientation(Orientation::Horizontal)
            .child(TextBlock::el().text("Screen capture delay (seconds):"))
            .child(
                TextBox::el()
                    .width(60.0)
                    .text(capture_delay.two_way())
                    .tool_tip_tip("Wait 0–60 seconds after selecting a screen or window"),
            )
            .child(action_button("Save capture delay", move |_| {
                let result = capture_delay
                    .get()
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| "the capture delay must be a whole number of seconds".to_owned())
                    .and_then(settings_linux::set_capture_delay_seconds);
                match result {
                    Ok(()) => status.set_if_changed("Capture delay saved.".to_owned()),
                    Err(error) => {
                        status.set_if_changed(format!("Could not save capture delay: {error}"))
                    }
                }
            }))
            .child(action_button("Cancel delayed capture", move |_| {
                cancel_delayed_capture(status)
            }))
            .child(
                CheckBox::el()
                    .content("Interval options")
                    .checked(interval_visible.two_way()),
            );
        let webcam_preferences_toolbar = WrapPanel::el()
            .orientation(Orientation::Horizontal)
            .child(TextBlock::el().text("Webcam device:"))
            .child(
                TextBox::el()
                    .width(170.0)
                    .text(webcam_device.two_way())
                    .tool_tip_tip("Absolute V4L2 device path; save webcam preferences to apply"),
            )
            .child(TextBlock::el().text("Position:"))
            .child(template_selector(
                [
                    "Top left",
                    "Top right",
                    "Bottom left",
                    "Bottom right",
                    "Top left (circle)",
                    "Top right (circle)",
                    "Bottom left (circle)",
                    "Bottom right (circle)",
                ]
                .into_iter(),
                webcam_corner,
                status,
                "webcam position",
            ))
            .child(TextBlock::el().text("Size %:"))
            .child(
                TextBox::el()
                    .width(55.0)
                    .text(webcam_size.two_way())
                    .tool_tip_tip(
                        "10–50 percent of the recording size; circles use the recording height",
                    ),
            )
            .child(action_button("Save webcam preferences", move |_| {
                let result = (|| {
                    let index = usize::try_from(webcam_corner.get())
                        .map_err(|_| "select a webcam position and shape".to_owned())?;
                    let shape = match index {
                        0..=3 => capture_linux::WebcamShape::Rectangle,
                        4..=7 => capture_linux::WebcamShape::Circle,
                        _ => return Err("select a webcam position and shape".to_owned()),
                    };
                    let corner = capture_linux::WebcamCorner::ALL
                        .get(index % capture_linux::WebcamCorner::ALL.len())
                        .copied()
                        .ok_or_else(|| "select a webcam position and shape".to_owned())?;
                    let size = webcam_size
                        .get()
                        .trim()
                        .parse::<u8>()
                        .map_err(|_| "webcam size must be a whole percentage".to_owned())?;
                    let layout = capture_linux::WebcamLayout::new(corner, size)?.with_shape(shape);
                    let configuration =
                        capture_linux::WebcamConfiguration::new(&webcam_device.get(), layout)?;
                    settings_linux::set_webcam_configuration(&configuration)
                })();
                match result {
                    Ok(()) => status.set_if_changed("Webcam preferences saved.".to_owned()),
                    Err(error) => {
                        status.set_if_changed(format!("Could not save webcam preferences: {error}"))
                    }
                }
            }))
            .child(
                CheckBox::el()
                    .content("Daily schedule options")
                    .checked(schedule_visible.two_way()),
            );
        let interval_toolbar = WrapPanel::el()
            .orientation(Orientation::Horizontal)
            .visible(interval_visible)
            .child(TextBlock::el().text("Interval capture (seconds):"))
            .child(
                TextBox::el()
                    .width(70.0)
                    .text(interval_seconds.two_way())
                    .tool_tip_tip("Capture immediately, then repeat at this interval"),
            )
            .child(
                CheckBox::el()
                    .content("Skip identical frames")
                    .checked(interval_skip_identical.two_way()),
            )
            .child(action_button("Interval capture… (start/stop)", move |scope| {
                toggle_interval_capture(
                    scope,
                    status,
                    captures,
                    library,
                    &interval_seconds.get(),
                    interval_skip_identical.get(),
                )
            }));
        let schedule_toolbar = WrapPanel::el()
            .orientation(Orientation::Horizontal)
            .visible(schedule_visible)
            .child(TextBlock::el().text("Daily capture (HH:MM):"))
            .child(
                TextBox::el()
                    .width(70.0)
                    .text(schedule_time.two_way())
                    .tool_tip_tip("Local time; Jiffshot must remain open"),
            )
            .child(
                CheckBox::el()
                    .content("Enable daily capture")
                    .checked(schedule_enabled.two_way()),
            )
            .child(action_button("Save daily schedule", move |scope| {
                let result = schedule_time
                    .get()
                    .trim()
                    .parse::<schedule_linux::ScheduleTime>()
                    .and_then(|time| {
                        settings_linux::set_schedule_preferences(time, schedule_enabled.get())
                    });
                match result {
                    Ok(preferences) => {
                        schedule_time.set_if_changed(preferences.time.to_string());
                        match configure_daily_schedule(scope, status, captures, library, preferences)
                        {
                            Ok(()) if preferences.enabled => status.set_if_changed(format!(
                                "Daily capture set for {} local time while Jiffshot is open.",
                                preferences.time
                            )),
                            Ok(()) => status.set_if_changed(
                                "Daily capture disabled; close any open capture chooser to cancel a pending request."
                                    .to_owned(),
                            ),
                            Err(error) => status.set_if_changed(format!(
                                "Daily schedule was saved but could not start: {error}"
                            )),
                        }
                    }
                    Err(error) => {
                        status.set_if_changed(format!("Could not save daily schedule: {error}"))
                    }
                }
            }));
        let library_search = WrapPanel::el()
            .orientation(Orientation::Horizontal)
            .child(
                TextBox::el()
                    .width(190.0)
                    .text(library.query.two_way())
                    .placeholder_text("Search name, app, site, tag")
                    .tool_tip_tip(
                        "Combine text with app:, site:, tag:, age:day, or age:week qualifiers",
                    ),
            )
            .child(action_button("Search all", move |scope| {
                start_library_search(
                    scope,
                    captures,
                    library,
                    library_linux::CaptureFilter::All,
                    status,
                )
            }))
            .child(action_button("Images", move |scope| {
                start_library_search(
                    scope,
                    captures,
                    library,
                    library_linux::CaptureFilter::Images,
                    status,
                )
            }))
            .child(action_button("Videos", move |scope| {
                start_library_search(
                    scope,
                    captures,
                    library,
                    library_linux::CaptureFilter::Videos,
                    status,
                )
            }))
            .child(action_button("Pinned", move |scope| {
                start_library_search(
                    scope,
                    captures,
                    library,
                    library_linux::CaptureFilter::Pinned,
                    status,
                )
            }))
            .child(action_button("Tagged", move |scope| {
                start_library_search(
                    scope,
                    captures,
                    library,
                    library_linux::CaptureFilter::Tagged,
                    status,
                )
            }))
            .child(
                CheckBox::el()
                    .content("Organize captures")
                    .checked(library.visible.two_way()),
            );
        let library_organize = library_organization_panel(library, captures, status);
        let recent_items = move || captures.get();
        let recent_canvas = canvas.clone();
        let recent_library = DockPanel::el()
            .child(
                TextBlock::el()
                    .text("Capture library")
                    .margin((8.0, 4.0))
                    .dock_panel_dock(Dock::Left),
            )
            .child(
                action_button("Recent", move |_| {
                    library.recent.set_if_changed(true);
                    refresh_capture_library(captures, library, status, None)
                })
                .dock_panel_dock(Dock::Left),
            )
            .child(
                ScrollViewer::el()
                    .horizontal_scroll_bar_visibility(ScrollBarVisibility::Auto)
                    .vertical_scroll_bar_visibility(ScrollBarVisibility::Disabled)
                    .min_height(45.0)
                    .content(
                        WrapPanel::el()
                            .orientation(Orientation::Horizontal)
                            .children_for(
                                recent_items,
                                PathBuf::clone,
                                move |path: Signal<PathBuf>| {
                                    recent_capture_button(
                                        path,
                                        recent_canvas.clone(),
                                        library,
                                        status,
                                    )
                                },
                            ),
                    ),
            );
        let toolbar = StackPanel::el()
            .spacing(2.0)
            .child(capture_toolbar)
            .child(capture_actions_toolbar)
            .child(document_toolbar)
            .child(image_actions_toolbar)
            .child(tool_palette)
            .child(theme_toolbar)
            .child(variant_toolbar)
            .child(editor_properties)
            .child(template_panel)
            .child(screen_preset_panel)
            .child(video_frame_toolbar)
            .child(video_trim_toolbar)
            .child(capture_preferences_toolbar)
            .child(cursor_preferences_toolbar)
            .child(recording_frame_rate_toolbar)
            .child(capture_delay_toolbar)
            .child(webcam_preferences_toolbar)
            .child(interval_toolbar)
            .child(schedule_toolbar);

        let window = Window::el()
            .title("Jiffshot")
            .node_ref(window_ref)
            .width(1280.0)
            .height(INITIAL_WINDOW_HEIGHT)
            .min_width(800.0)
            .min_height(500.0)
            .on_size_changed(move |args| window_height.set_if_changed(args.new_size.height))
            .content(
                DockPanel::el()
                    .child(
                        ScrollViewer::el()
                            .horizontal_scroll_bar_visibility(ScrollBarVisibility::Disabled)
                            .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
                            .max_height(move || {
                                (window_height.get() * CONTROLS_HEIGHT_FRACTION).max(0.0)
                            })
                            .content(
                                StackPanel::el()
                                    .child(toolbar)
                                    .child(library_search)
                                    .child(library_organize)
                                    .child(recent_library),
                            )
                            .dock_panel_dock(Dock::Top),
                    )
                    .child(
                        TextBlock::el()
                            .text(status)
                            .font_size(12.0)
                            .margin((8.0, 4.0))
                            .dock_panel_dock(Dock::Bottom),
                    )
                    .child(AnyEl::from_control(view.control().clone())),
            )
            .build(scope)?;
        let closing_scope = scope.clone();
        let closing_window = Arc::new(ui::UiOnly::new(window.clone()));
        window.clone().on_closing(scope, move |args| {
            let ending = matches!(
                WindowCloseReason::try_from(args.close_reason),
                Ok(WindowCloseReason::ApplicationShutdown | WindowCloseReason::OSShutdown)
            );
            if ending || !tray_is_active() {
                return;
            }
            args.cancel = true;
            let window = Arc::clone(&closing_window);
            if let Err(error) = closing_scope.post(move || {
                if tray_is_active() {
                    match window.get().hide() {
                        Ok(()) => {
                            WINDOW_HIDDEN.store(true, Ordering::Release);
                            status.set_if_changed(
                                "Jiffshot is running in the system tray.".to_owned(),
                            );
                        }
                        Err(error) => status.set_if_changed(format!(
                            "Could not hide the Jiffshot window: {error}"
                        )),
                    }
                }
            }) {
                eprintln!("Could not hide the Jiffshot window: {error}");
            }
        })?;
        wire_editor_shortcuts(scope, &window, canvas.clone(), status, captures, library)?;
        scope.mount(window)?;
        if tray_preference {
            tray_ui.start(if matches!(launch, launch_linux::Launch::Background) {
                TrayStart::BackgroundLaunch
            } else {
                TrayStart::RestorePreference
            });
        }
        if shortcut_preference && shortcuts_valid {
            enable_global_capture_shortcut(scope, shortcut_ui);
        }
        if schedule_preference.enabled {
            if let Err(error) =
                configure_daily_schedule(scope, status, captures, library, schedule_preference)
            {
                status.set_if_changed(format!("Could not start daily capture schedule: {error}"));
            }
        }
        let launch_scope = scope.clone();
        let launch_canvas = canvas.clone();
        scope.post(move || match launch {
            launch_linux::Launch::Capture => {
                capture_screen(&launch_scope, launch_canvas, status, captures, library)
            }
            launch_linux::Launch::InteractiveCapture => {
                capture_interactive(&launch_scope, launch_canvas, status, captures, library)
            }
            launch_linux::Launch::GrabText => grab_screen_text(&launch_scope, status),
            launch_linux::Launch::Scrolling
            | launch_linux::Launch::HorizontalScrolling
            | launch_linux::Launch::Panoramic => {
                let mode = match launch {
                    launch_linux::Launch::HorizontalScrolling => {
                        horizontal_scrolling.set_if_changed(true);
                        scroll_linux::Mode::Scrolling(scroll_linux::Direction::Horizontal)
                    }
                    launch_linux::Launch::Panoramic => scroll_linux::Mode::Panoramic,
                    _ => scroll_linux::Mode::Scrolling(scroll_linux::Direction::Vertical),
                };
                toggle_scrolling_capture(
                    &launch_scope,
                    launch_canvas,
                    status,
                    captures,
                    library,
                    mode,
                );
            }
            launch_linux::Launch::Interval => {
                interval_visible.set_if_changed(true);
                match settings_linux::interval_preferences() {
                    Ok(preferences) => toggle_interval_capture(
                        &launch_scope,
                        status,
                        captures,
                        library,
                        &preferences.seconds().to_string(),
                        preferences.skip_identical(),
                    ),
                    Err(error) => status
                        .set_if_changed(format!("Could not load interval preferences: {error}")),
                }
            }
            launch_linux::Launch::Record => toggle_screen_recording(
                &launch_scope,
                status,
                captures,
                library,
                capture_linux::RecordingAudio::Disabled,
                false,
            ),
            launch_linux::Launch::Editor(Some(path)) => {
                if let Some(pending) = initial_load {
                    start_document_load(
                        &launch_scope,
                        path,
                        DocumentLoadKind::File,
                        pending,
                        launch_canvas,
                        status,
                        None,
                    );
                }
            }
            launch_linux::Launch::Background if !tray_preference => status.set_if_changed(
                "Login startup opened the editor; enable the tray to start hidden.".to_owned(),
            ),
            launch_linux::Launch::Background
            |
            launch_linux::Launch::Editor(None)
            | launch_linux::Launch::Help
            | launch_linux::Launch::Version => {}
        })?;
        Ok(())
    })
}

fn theme_variant(theme: &str) -> ThemeVariant {
    match theme {
        "Light" => ThemeVariant::Light,
        "Dark" => ThemeVariant::Dark,
        _ => ThemeVariant::Default,
    }
}

fn set_linux_theme(scope: &AppScope, theme: &'static str, status: Signal<String>) {
    match scope.set_requested_theme_variant(theme_variant(theme)) {
        Ok(()) => match settings_linux::set_theme(theme) {
            Ok(()) => status.set_if_changed(format!("Theme set to {theme}")),
            Err(error) => status.set_if_changed(format!(
                "Theme set to {theme} for this session, but could not save the preference: {error}"
            )),
        },
        Err(error) => status.set_if_changed(format!("Could not set theme: {error}")),
    }
}

fn wire_editor_shortcuts(
    scope: &AppScope,
    window: &Window,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
    captures: Signal<Vec<PathBuf>>,
    library: LibraryControls,
) -> Result<()> {
    let callback_scope = scope.clone();
    window
        .clone()
        .on_key_down(scope, move |args| {
            if args.handled {
                return;
            }
            let Ok(key) = Key::try_from(args.key) else {
                return;
            };
            if let Some(action) = selection_shortcut(key, args.key_modifiers) {
                apply_selection_action(&canvas, status, action);
                args.handled = true;
                return;
            }
            const SHIFT: i32 = 1;
            const CONTROL: i32 = 2;
            let modifiers = args.key_modifiers;
            if modifiers == (CONTROL | SHIFT) {
                match key {
                    Key::N => {
                        capture_screen(&callback_scope, canvas.clone(), status, captures, library)
                    }
                    Key::C => copy_image(&callback_scope, status),
                    Key::V => paste_image(&callback_scope, canvas.clone(), status),
                    Key::S => save_document(&callback_scope, status),
                    _ => return,
                }
            } else if modifiers == CONTROL {
                match key {
                    Key::O => open_document(&callback_scope, canvas.clone(), status),
                    Key::S => save_document(&callback_scope, status),
                    Key::E => export_image(&callback_scope, status),
                    Key::Z => history_action(&canvas, &status, true),
                    Key::Y => history_action(&canvas, &status, false),
                    Key::G => {
                        let view = canvas.get();
                        view.set_show_pixel_grid(!view.show_pixel_grid());
                    }
                    Key::D0 => canvas.get().zoom_to("actual"),
                    key @ (Key::D1
                    | Key::D2
                    | Key::D3
                    | Key::D4
                    | Key::D5
                    | Key::D6
                    | Key::D7
                    | Key::D8
                    | Key::D9) => {
                        let index = (key as usize) - (Key::D1 as usize);
                        if let Some(&tool) = TOOLS.get(index) {
                            canvas::with_editor(|editor| editor.set_tool(tool));
                            canvas.get().refresh();
                            status.set_if_changed(format!("{} tool selected", tool.name()));
                        }
                    }
                    _ => return,
                }
            } else {
                return;
            }
            args.handled = true;
        })
        .map(|_| ())
}

fn selection_shortcut(key: Key, modifiers: i32) -> Option<editor_linux::SelectionAction> {
    use editor_linux::SelectionAction as Action;
    const SHIFT: i32 = 1;
    const CONTROL: i32 = 2;
    const CONTROL_SHIFT: i32 = CONTROL | SHIFT;
    match (key, modifiers) {
        (Key::Delete, 0) => Some(Action::Delete),
        (Key::A, CONTROL) => Some(Action::SelectAll),
        (Key::D, CONTROL) => Some(Action::Deselect),
        (Key::T, CONTROL) => Some(Action::FlattenSelected),
        (Key::I, CONTROL_SHIFT) => Some(Action::Invert),
        (Key::G, CONTROL_SHIFT) => Some(Action::Group),
        (Key::U, CONTROL_SHIFT) => Some(Action::Ungroup),
        (Key::F, CONTROL_SHIFT) => Some(Action::BringForward),
        (Key::B, CONTROL_SHIFT) => Some(Action::SendBackward),
        (Key::T, CONTROL_SHIFT) => Some(Action::FlattenAll),
        _ => None,
    }
}

fn apply_selection_action(
    canvas: &ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
    action: editor_linux::SelectionAction,
) {
    let result = canvas::with_editor(|editor| editor_linux::selection_action(editor, action))
        .unwrap_or_else(|| Err("The editor is not available.".to_owned()));
    match result {
        Ok(message) => {
            canvas.get().refresh();
            status.set_if_changed(message.to_owned());
        }
        Err(error) => status.set_if_changed(error),
    }
}

fn action_button(label: &'static str, action: impl Fn(&AppScope) + Send + 'static) -> El<Button> {
    Button::el().content(label).with_op(move |scope, button| {
        let scope = scope.clone();
        let callback_scope = scope.clone();
        button
            .clone()
            .on_click(&scope, move |_| action(&callback_scope))?;
        Ok(())
    })
}

#[derive(Clone, Copy)]
enum TrayStart {
    SavePreference,
    RestorePreference,
    BackgroundLaunch,
}

#[derive(Clone)]
struct TrayUi {
    scope: AppScope,
    window: NodeRef<Window>,
    canvas: Arc<ui::UiOnly<ui::canvas_view::CanvasView>>,
    status: Signal<String>,
    captures: Signal<Vec<PathBuf>>,
    library: LibraryControls,
}

impl TrayUi {
    fn start(&self, preference: TrayStart) {
        if TRAY.lock().is_some() {
            self.status
                .set_if_changed("The system tray is already enabled or starting.".to_owned());
            return;
        }
        let id = NEXT_TRAY_ID.fetch_add(1, Ordering::Relaxed) + 1;
        let dispatcher = self.scope.clone();
        let ui = self.clone();
        let session = match tray_linux::Session::start(move |event| {
            let ui = ui.clone();
            if let Err(error) = dispatcher.post(move || ui.handle_event(id, preference, event)) {
                eprintln!("Could not report the system tray event: {error}");
            }
        }) {
            Ok(session) => session,
            Err(error) => {
                self.status
                    .set_if_changed(format!("Could not start the system tray: {error}"));
                return;
            }
        };
        *TRAY.lock() = Some(ActiveTray { id, session });
        self.status
            .set_if_changed("Connecting to the desktop system tray…".to_owned());
    }

    fn handle_event(&self, id: u64, preference: TrayStart, event: tray_linux::Event) {
        if !TRAY.lock().as_ref().is_some_and(|tray| tray.id == id) {
            return;
        }
        match event {
            tray_linux::Event::Ready => {
                if !TRAY
                    .lock()
                    .as_ref()
                    .is_some_and(|tray| tray.id == id && tray.session.activate())
                {
                    return;
                }
                if matches!(preference, TrayStart::SavePreference) {
                    if let Err(error) = settings_linux::set_tray_enabled(true) {
                        TRAY.lock().take();
                        self.status.set_if_changed(format!(
                            "Could not save the system tray preference: {error}"
                        ));
                        return;
                    }
                }
                if matches!(preference, TrayStart::BackgroundLaunch) {
                    match self.hide_window() {
                        Ok(()) => self
                            .status
                            .set_if_changed("Jiffshot started in the system tray.".to_owned()),
                        Err(error) => self
                            .status
                            .set_if_changed(format!("Could not hide the Jiffshot window: {error}")),
                    }
                } else {
                    self.status.set_if_changed(
                        "System tray enabled; closing the window keeps Jiffshot running."
                            .to_owned(),
                    );
                }
            }
            tray_linux::Event::Unavailable(error) => {
                TRAY.lock().take();
                let message = format!(
                    "System tray unavailable: {error}. Jiffshot will stay visible; try enabling the tray again."
                );
                match self.show_window() {
                    Ok(()) => self.status.set_if_changed(message),
                    Err(show_error) => {
                        eprintln!("{message} Could not restore the window: {show_error}");
                        self.status.set_if_changed(format!(
                            "{message} Could not restore the window: {show_error}"
                        ));
                    }
                }
            }
            tray_linux::Event::Open if tray_is_active() => {
                if let Err(error) = self.show_window() {
                    self.status
                        .set_if_changed(format!("Could not open the Jiffshot window: {error}"));
                }
            }
            tray_linux::Event::Capture if tray_is_active() => match self.show_window() {
                Ok(()) => capture_screen(
                    &self.scope,
                    self.canvas.as_ref().clone(),
                    self.status,
                    self.captures,
                    self.library,
                ),
                Err(error) => self
                    .status
                    .set_if_changed(format!("Could not open the Jiffshot window: {error}")),
            },
            tray_linux::Event::Quit if tray_is_active() => {
                if tray_capture_in_progress() {
                    if let Err(error) = self.show_window() {
                        eprintln!("Could not restore the Jiffshot window: {error}");
                    }
                    self.status.set_if_changed(
                        "Finish or cancel the active capture before quitting Jiffshot.".to_owned(),
                    );
                    return;
                }
                TRAY.lock().take();
                if let Err(error) = self.scope.shutdown() {
                    let _ = self.show_window().map_err(|show_error| {
                        eprintln!("Could not restore the Jiffshot window: {show_error}");
                    });
                    self.status
                        .set_if_changed(format!("Could not quit Jiffshot: {error}"));
                }
            }
            tray_linux::Event::Open | tray_linux::Event::Capture | tray_linux::Event::Quit => {}
        }
    }

    fn show_window(&self) -> std::result::Result<(), String> {
        self.window
            .get()
            .ok_or_else(|| "the editor window is unavailable".to_owned())?
            .show()
            .map_err(|error| error.to_string())?;
        WINDOW_HIDDEN.store(false, Ordering::Release);
        Ok(())
    }

    fn hide_window(&self) -> std::result::Result<(), String> {
        self.window
            .get()
            .ok_or_else(|| "the editor window is unavailable".to_owned())?
            .hide()
            .map_err(|error| error.to_string())?;
        WINDOW_HIDDEN.store(true, Ordering::Release);
        Ok(())
    }
}

fn report_capture_result(
    status: Signal<String>,
    summary: &'static str,
    message: String,
    urgency: notification_linux::Urgency,
) {
    if WINDOW_HIDDEN.load(Ordering::Acquire) {
        notification_linux::send(summary, message.clone(), urgency);
    }
    status.set_if_changed(message);
}

fn tray_is_active() -> bool {
    TRAY.lock()
        .as_ref()
        .is_some_and(|tray| tray.session.is_active())
}

fn tray_capture_in_progress() -> bool {
    RECORDING.lock().is_some()
        || SCROLL_CAPTURE.lock().is_some()
        || INTERVAL_CAPTURE.lock().is_some()
        || DELAYED_CAPTURE.lock().is_some()
        || SINGLE_CAPTURES_ACTIVE.load(Ordering::Acquire) != 0
        || SCHEDULED_CAPTURE_ACTIVE.load(Ordering::Acquire)
}

fn disable_tray(status: Signal<String>) {
    match settings_linux::set_tray_enabled(false) {
        Ok(()) => {
            TRAY.lock().take();
            status.set_if_changed(
                "System tray disabled; closing the window quits Jiffshot.".to_owned(),
            );
        }
        Err(error) => {
            status.set_if_changed(format!(
                "Could not save the system tray preference: {error}"
            ));
        }
    }
}

fn editor_properties_panel(
    controls: EditorControls,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
) -> El<StackPanel> {
    let effect_choices = effect_names().map(|name| ComboBoxItem::el().content(name));
    let effect_selector = ComboBox::el()
        .width(180.0)
        .items_iter(effect_choices)
        .selected_index(controls.effect_index.get())
        .with_op(move |scope, combo| {
            let reader = combo.clone();
            combo.clone().on_selection_changed(scope, move |_| {
                match reader.get_selected_index() {
                    Ok(index) => {
                        if controls.effect_index.get() != index {
                            controls.effect_parameter.set_if_changed(String::new());
                            controls.effect_index.set_if_changed(index);
                        }
                    }
                    Err(error) => status
                        .set_if_changed(format!("Could not read the selected effect: {error}")),
                }
            })?;
            Ok(())
        });
    let text_and_effects = WrapPanel::el()
        .orientation(Orientation::Horizontal)
        .child(TextBlock::el().text("Text:"))
        .child(TextBox::el().width(220.0).text(controls.text.two_way()))
        .child(action_button("Apply text", {
            let canvas = canvas.clone();
            move |_| {
                let text = controls.text.get();
                editor_action(&canvas, status, "Text updated", |editor| {
                    editor_linux::apply_text(editor, &text)
                });
            }
        }))
        .child(TextBlock::el().text("Image effect:"))
        .child(effect_selector)
        .child(action_button("Apply effect", {
            let canvas = canvas.clone();
            move |scope| {
                let name = selected_effect_name(controls.effect_index.get());
                let Some(name) = name else {
                    status.set_if_changed("Select an image effect first.".to_owned());
                    return;
                };
                let parameter = controls.effect_parameter.get();
                match name {
                    "Capture info" => {
                        match editor_linux::validate_effect_parameter(name, &parameter) {
                            Ok(()) => add_capture_info(scope, canvas.clone(), status),
                            Err(error) => status.set_if_changed(error),
                        }
                    }
                    "Watermark" => match editor_linux::watermark_opacity(&parameter) {
                        Ok(opacity) => choose_watermark(scope, canvas.clone(), status, opacity),
                        Err(error) => status.set_if_changed(error),
                    },
                    _ => editor_action(&canvas, status, &format!("{name} applied"), |editor| {
                        editor_linux::apply_effect(editor, name, &parameter)
                    }),
                }
            }
        }));
    let effect_parameter = WrapPanel::el()
        .orientation(Orientation::Horizontal)
        .child(TextBlock::el().text("Effect parameter (blank = default):"))
        .child(
            TextBox::el()
                .width(80.0)
                .text(controls.effect_parameter.two_way()),
        )
        .child(TextBlock::el().text(move || {
            selected_effect_name(controls.effect_index.get())
                .map(editor_linux::effect_parameter_hint)
                .unwrap_or("Select an image effect")
                .to_owned()
        }));
    let colors = WrapPanel::el()
        .orientation(Orientation::Horizontal)
        .child(TextBlock::el().text("Stroke:"))
        .child(
            TextBox::el()
                .width(102.0)
                .text(controls.stroke.two_way())
                .tool_tip_tip("Color as #RGB, #RRGGBB, or #AARRGGBB"),
        )
        .child(TextBlock::el().text("Fill:"))
        .child(
            TextBox::el()
                .width(102.0)
                .text(controls.fill.two_way())
                .tool_tip_tip("Color as #RGB, #RRGGBB, or #AARRGGBB"),
        )
        .child(TextBlock::el().text("Text color:"))
        .child(
            TextBox::el()
                .width(102.0)
                .text(controls.text_color.two_way())
                .tool_tip_tip("Color as #RGB, #RRGGBB, or #AARRGGBB"),
        )
        .child(TextBlock::el().text("Width:"))
        .child(TextBox::el().width(58.0).text(controls.width.two_way()))
        .child(TextBlock::el().text("Opacity %:"))
        .child(TextBox::el().width(58.0).text(controls.opacity.two_way()))
        .child(action_button("Apply style", {
            let canvas = canvas.clone();
            move |_| {
                let inputs = controls.style_inputs();
                editor_action(&canvas, status, "Style applied", |editor| {
                    editor_linux::apply_style(editor, &inputs)
                });
            }
        }));
    let typography = WrapPanel::el()
        .orientation(Orientation::Horizontal)
        .child(TextBlock::el().text("Font:"))
        .child(TextBox::el().width(155.0).text(controls.font.two_way()))
        .child(TextBlock::el().text("Size:"))
        .child(TextBox::el().width(58.0).text(controls.font_size.two_way()))
        .child(
            CheckBox::el()
                .content("Bold")
                .checked(controls.bold.two_way()),
        )
        .child(
            CheckBox::el()
                .content("Italic")
                .checked(controls.italic.two_way()),
        )
        .child(
            CheckBox::el()
                .content("Shadow")
                .checked(controls.shadow.two_way()),
        );
    let smart_actions = WrapPanel::el()
        .orientation(Orientation::Horizontal)
        .child(action_button("Auto trim", {
            let canvas = canvas.clone();
            move |_| {
                editor_action(&canvas, status, "Solid-colored edges trimmed.", |editor| {
                    editor_linux::auto_trim(editor)
                });
            }
        }))
        .child(action_button("Copy recognized text", {
            let canvas = canvas.clone();
            move |scope| document_ocr(scope, canvas.clone(), status, DocumentOcrAction::CopyText)
        }))
        .child(
            action_button("Smart redact", {
                let canvas = canvas.clone();
                move |scope| {
                    document_ocr(
                        scope,
                        canvas.clone(),
                        status,
                        DocumentOcrAction::SmartRedact,
                    )
                }
            })
            .tool_tip_tip(
                "OCR can miss sensitive text. Review before sharing; .jsd redactions are editable.",
            ),
        )
        .child(
            action_button("Simplify UI", {
                let canvas = canvas.clone();
                move |scope| {
                    document_ocr(scope, canvas.clone(), status, DocumentOcrAction::Simplify)
                }
            })
            .tool_tip_tip("Flattens annotations and replaces recognized text with editable bars."),
        );
    let mut selection_actions = WrapPanel::el().orientation(Orientation::Horizontal);
    for (label, action) in [
        ("Select all", editor_linux::SelectionAction::SelectAll),
        ("Deselect", editor_linux::SelectionAction::Deselect),
        ("Invert selection", editor_linux::SelectionAction::Invert),
        ("Delete selected", editor_linux::SelectionAction::Delete),
        (
            "Duplicate selected",
            editor_linux::SelectionAction::Duplicate,
        ),
        ("Group", editor_linux::SelectionAction::Group),
        ("Ungroup", editor_linux::SelectionAction::Ungroup),
        ("Bring forward", editor_linux::SelectionAction::BringForward),
        ("Send backward", editor_linux::SelectionAction::SendBackward),
        (
            "Flatten selected",
            editor_linux::SelectionAction::FlattenSelected,
        ),
        ("Flatten all", editor_linux::SelectionAction::FlattenAll),
    ] {
        let canvas = canvas.clone();
        selection_actions = selection_actions.child(action_button(label, move |_| {
            apply_selection_action(&canvas, status, action)
        }));
    }
    StackPanel::el()
        .visible(controls.visible)
        .child(text_and_effects)
        .child(effect_parameter)
        .child(colors)
        .child(typography)
        .child(smart_actions)
        .child(selection_actions)
}

fn document_ocr(
    scope: &AppScope,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
    action: DocumentOcrAction,
) {
    let Some(snapshot) = canvas::with_editor(|editor| editor.document().clone()) else {
        status.set_if_changed("The editor is not available.".to_owned());
        return;
    };
    status.set_if_changed("Recognizing text...".to_owned());
    let task_scope = scope.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("jiffshot-document-ocr".to_owned())
        .spawn(move || {
            let result = (|| {
                let image = snapshot.flatten();
                let lines = ocr_linux::Recognizer::new()?.recognize(&image)?;
                match action {
                    DocumentOcrAction::CopyText => {
                        let text = editor_linux::recognized_text(&lines);
                        if text.is_empty() {
                            return Ok(DocumentOcrResult::NoText);
                        }
                        if !canvas::with_editor(|editor| {
                            editor_linux::same_document(&snapshot, editor.document())
                        })
                        .unwrap_or(false)
                        {
                            return Err("The document changed before recognition completed."
                                .to_owned());
                        }
                        let count = text.lines().count();
                        clipboard_linux::copy_text(text)?;
                        Ok(DocumentOcrResult::CopiedText(count))
                    }
                    DocumentOcrAction::SmartRedact => Ok(DocumentOcrResult::Redact(lines)),
                    DocumentOcrAction::Simplify => {
                        let (document, bars) = editor_linux::simplify_document(&snapshot, &lines);
                        Ok(DocumentOcrResult::Simplified(document, bars))
                    }
                }
            })();
            if let Err(error) = task_scope.post(move || match result {
                Ok(DocumentOcrResult::CopiedText(count)) => {
                    status.set_if_changed(format!("Copied {count} recognized line(s)."));
                }
                Ok(DocumentOcrResult::NoText) => {
                    status.set_if_changed("No text recognized.".to_owned());
                }
                Ok(DocumentOcrResult::Redact(lines)) => {
                    let result = canvas::with_editor(|editor| {
                        if !editor_linux::same_document(&snapshot, editor.document()) {
                            return Err(
                                "The document changed before recognition completed.".to_owned()
                            );
                        }
                        Ok(editor_linux::smart_redact(editor, &lines))
                    })
                    .unwrap_or_else(|| Err("The editor is not available.".to_owned()));
                    match result {
                        Ok(0) => {
                            status.set_if_changed(
                                "Smart Redact found no matches. OCR can miss sensitive text."
                                    .to_owned(),
                            );
                        }
                        Ok(count) => {
                            canvas.get().refresh();
                            status.set_if_changed(format!(
                                "Covered {count} line(s). Review carefully: OCR can miss text and .jsd redactions are editable."
                            ));
                        }
                        Err(error) => status.set_if_changed(error),
                    }
                }
                Ok(DocumentOcrResult::Simplified(document, bars)) => {
                    let result = canvas::with_editor(|editor| {
                        if !editor_linux::same_document(&snapshot, editor.document()) {
                            return Err(
                                "The document changed before simplification completed.".to_owned()
                            );
                        }
                        editor_linux::apply_simplification(editor, document);
                        Ok(())
                    })
                    .unwrap_or_else(|| Err("The editor is not available.".to_owned()));
                    match result {
                        Ok(()) => {
                            canvas.get().refresh();
                            status.set_if_changed(format!(
                                "Simplified: {bars} text line(s) replaced by editable bars."
                            ));
                        }
                        Err(error) => status.set_if_changed(error),
                    }
                }
                Err(error) => status.set_if_changed(format!("Text recognition failed: {error}")),
            }) {
                eprintln!("Could not report text recognition result: {error}");
            }
        })
    {
        status.set_if_changed(format!("Could not start text recognition: {error}"));
    }
}

fn template_selector(
    choices: impl Iterator<Item = &'static str>,
    selected: Signal<i32>,
    status: Signal<String>,
    label: &'static str,
) -> El<ComboBox> {
    ComboBox::el()
        .width(170.0)
        .items_iter(choices.map(|name| ComboBoxItem::el().content(name)))
        .selected_index(selected.get())
        .with_op(move |scope, combo| {
            let reader = combo.clone();
            combo.clone().on_selection_changed(scope, move |_| {
                match reader.get_selected_index() {
                    Ok(index) => selected.set_if_changed(index),
                    Err(error) => status
                        .set_if_changed(format!("Could not read the selected {label}: {error}")),
                }
            })?;
            Ok(())
        })
}

fn screen_preset_panel(
    controls: ScreenPresetControls,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
    captures: Signal<Vec<PathBuf>>,
    library: LibraryControls,
) -> El<StackPanel> {
    let configuration = WrapPanel::el()
        .orientation(Orientation::Horizontal)
        .child(TextBlock::el().text("Preset name:"))
        .child(TextBox::el().width(150.0).text(controls.name.two_way()))
        .child(TextBlock::el().text("Delay (seconds):"))
        .child(TextBox::el().width(60.0).text(controls.delay.two_way()))
        .child(
            CheckBox::el()
                .content("Preset includes cursor")
                .checked(controls.cursor.two_way()),
        )
        .child(action_button("Save screen preset", move |_| {
            let name = controls.name.get();
            let result = controls
                .delay
                .get()
                .trim()
                .parse::<u64>()
                .map_err(|_| "the capture delay must be a whole number of seconds".to_owned())
                .and_then(|delay| {
                    settings_linux::save_screen_capture_preset(
                        &name,
                        delay,
                        capture_linux::CaptureCursor::from_included(controls.cursor.get()),
                    )
                });
            match result {
                Ok(presets) => {
                    controls.name.set_if_changed(name.trim().to_owned());
                    controls.presets.set_if_changed(presets);
                    status
                        .set_if_changed(format!("Screen capture preset '{}' saved.", name.trim()));
                }
                Err(error) => {
                    status.set_if_changed(format!("Could not save screen capture preset: {error}"))
                }
            }
        }))
        .child(action_button("Delete screen preset", move |_| {
            let name = controls.name.get();
            match settings_linux::delete_screen_capture_preset(&name) {
                Ok(presets) => {
                    controls.presets.set_if_changed(presets);
                    controls.name.set_if_changed(String::new());
                    status.set_if_changed(format!(
                        "Screen capture preset '{}' deleted.",
                        name.trim()
                    ));
                }
                Err(error) => status
                    .set_if_changed(format!("Could not delete screen capture preset: {error}")),
            }
        }));
    let presets = move || controls.presets.get();
    StackPanel::el()
        .visible(controls.visible)
        .child(configuration)
        .child(
            WrapPanel::el()
                .orientation(Orientation::Horizontal)
                .children_for(
                    presets,
                    |preset: &settings_linux::ScreenCapturePreset| preset.name().to_owned(),
                    move |preset| {
                        screen_preset_button(preset, canvas.clone(), status, captures, library)
                    },
                ),
        )
}

fn screen_preset_button(
    preset: Signal<settings_linux::ScreenCapturePreset>,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
    captures: Signal<Vec<PathBuf>>,
    library: LibraryControls,
) -> El<Button> {
    let name = preset.get_untracked().name().to_owned();
    Button::el()
        .content(format!("Capture: {name}"))
        .tool_tip_tip("Choose a screen or window using this preset's delay and cursor choices")
        .with_op(move |scope, button| {
            let action_scope = scope.clone();
            let canvas = canvas.clone();
            let name = name.clone();
            button.clone().on_click(
                scope,
                move |_| match settings_linux::screen_capture_preset(&name) {
                    Ok(preset) => capture_screen_with_options(
                        &action_scope,
                        canvas.clone(),
                        status,
                        captures,
                        library,
                        ScreenCaptureOptions {
                            seconds: preset.delay_seconds(),
                            cursor: preset.cursor(),
                        },
                    ),
                    Err(error) => status
                        .set_if_changed(format!("Could not run screen capture preset: {error}")),
                },
            )?;
            Ok(())
        })
}

fn template_panel(
    controls: TemplateControls,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
) -> El<StackPanel> {
    let configuration = WrapPanel::el()
        .orientation(Orientation::Horizontal)
        .child(TextBlock::el().text("Layout:"))
        .child(template_selector(
            jiffshot_doc::combine::TEMPLATES.iter().copied(),
            controls.layout,
            status,
            "template layout",
        ))
        .child(TextBlock::el().text("Theme:"))
        .child(template_selector(
            jiffshot_doc::combine::THEMES.iter().map(|theme| theme.name),
            controls.theme,
            status,
            "template theme",
        ))
        .child(
            CheckBox::el()
                .content("Captions")
                .checked(controls.captions.two_way()),
        )
        .child(
            CheckBox::el()
                .content("Numbers")
                .checked(controls.numbers.two_way()),
        )
        .child(TextBlock::el().text("Footer:"))
        .child(TextBox::el().width(150.0).text(controls.footer.two_way()))
        .child(action_button("Create template", {
            let canvas = canvas.clone();
            move |_| {
                let result = template_linux::create_template(
                    controls.layout.get(),
                    controls.theme.get(),
                    controls.captions.get(),
                    controls.numbers.get(),
                    &controls.footer.get(),
                );
                match result {
                    Ok(document) => {
                        if canvas::with_editor(|editor| {
                            load_editor_document(editor, document, None);
                            editor.set_tool(Tool::Select);
                        })
                        .is_some()
                        {
                            canvas.get().refresh();
                            status.set_if_changed(
                                "Template created. Select an image zone to fill it.".to_owned(),
                            );
                        } else {
                            status.set_if_changed("The editor is not available.".to_owned());
                        }
                    }
                    Err(error) => status.set_if_changed(error),
                }
            }
        }));
    let zone_actions = WrapPanel::el()
        .orientation(Orientation::Horizontal)
        .child(action_button("Paste into zone", {
            let canvas = canvas.clone();
            move |scope| paste_template_zone(scope, canvas.clone(), status)
        }))
        .child(action_button("Fit zone image", {
            let canvas = canvas.clone();
            move |_| {
                editor_action(&canvas, status, "Zone image fit to frame.", |editor| {
                    template_linux::set_selected_zone_fit(editor, true)
                });
            }
        }))
        .child(action_button("Crop zone image", {
            let canvas = canvas.clone();
            move |_| {
                editor_action(&canvas, status, "Zone image cropped to frame.", |editor| {
                    template_linux::set_selected_zone_fit(editor, false)
                });
            }
        }))
        .child(action_button("Clear zone", {
            let canvas = canvas.clone();
            move |_| {
                editor_action(&canvas, status, "Zone cleared.", |editor| {
                    template_linux::clear_selected_zone(editor)
                });
            }
        }))
        .child(action_button("Swap selected zones", {
            let canvas = canvas.clone();
            move |_| {
                editor_action(&canvas, status, "Zone images swapped.", |editor| {
                    template_linux::swap_selected_zones(editor)
                });
            }
        }));
    StackPanel::el()
        .visible(controls.visible)
        .child(configuration)
        .child(zone_actions)
}

fn paste_template_zone(
    scope: &AppScope,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
) {
    let target = canvas::with_editor(|editor| {
        template_linux::selected_zone(editor).map(|id| (id, editor.document().background_arc()))
    })
    .unwrap_or_else(|| Err("The editor is not available.".to_owned()));
    let (zone, background) = match target {
        Ok(target) => target,
        Err(error) => {
            status.set_if_changed(error);
            return;
        }
    };
    status.set_if_changed("Reading image from the clipboard...".to_owned());
    let task_scope = scope.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("jiffshot-template-paste".to_owned())
        .spawn(move || {
            let result =
                clipboard_linux::paste_document().map(|document| document.background().clone());
            if let Err(error) = task_scope.post(move || match result {
                Ok(image) => {
                    editor_action(&canvas, status, "Image pasted into zone.", |editor| {
                        if !Arc::ptr_eq(&background, &editor.document().background_arc())
                            || editor.selection() != Some(zone)
                        {
                            return Err(
                                "The selected template zone changed before paste completed."
                                    .to_owned(),
                            );
                        }
                        template_linux::fill_selected_zone(editor, image)
                    });
                }
                Err(error) => status.set_if_changed(error),
            }) {
                eprintln!("Could not update template from clipboard: {error}");
            }
        })
    {
        status.set_if_changed(format!("Could not start template paste: {error}"));
    }
}

fn editor_action(
    canvas: &ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
    success: &str,
    action: impl FnOnce(&mut Editor) -> std::result::Result<(), String>,
) {
    let result = canvas::with_editor(action)
        .unwrap_or_else(|| Err("The editor is not available.".to_owned()));
    match result {
        Ok(()) => {
            canvas.get().refresh();
            status.set_if_changed(success.to_owned());
        }
        Err(error) => status.set_if_changed(error),
    }
}

fn effect_names() -> impl Iterator<Item = &'static str> {
    effects::catalog(Rgba::BLACK)
        .into_iter()
        .map(|(name, _)| name)
        .chain(effects::INTERACTIVE.iter().copied())
}

fn selected_effect_name(index: i32) -> Option<&'static str> {
    usize::try_from(index)
        .ok()
        .and_then(|index| effect_names().nth(index))
}

fn load_editor_document(editor: &mut Editor, document: Document, source: Option<PathBuf>) {
    editor.load(document);
    *DOCUMENT_SOURCE.lock() = source;
}

fn add_capture_info(
    scope: &AppScope,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
) {
    let Some(pending) = begin_document_load(status) else {
        return;
    };
    let source = DOCUMENT_SOURCE.lock().clone();
    start_interactive_effect(scope, pending, "Capture info", canvas, status, move || {
        editor_linux::capture_info(source.as_deref())
            .map(|text| effects::Effect::CaptureInfo { text })
    });
}

fn choose_watermark(
    scope: &AppScope,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
    opacity: f32,
) {
    let Some(window) = scope.main_window() else {
        status.set_if_changed("The editor window is not available.".to_owned());
        return;
    };
    let Some(pending) = begin_document_load(status) else {
        return;
    };
    let filter = ["png", "jpg", "jpeg", "bmp", "gif", "webp"]
        .into_iter()
        .fold(FileTypeFilter::new("Image files"), |filter, extension| {
            filter.with_extension(extension)
        });
    let options = OpenFilePickerOptions::new()
        .title("Choose a watermark image")
        .file_type(filter);
    let operation = match file_dialog_linux::open_file_picker(scope, &window, &options) {
        Ok(operation) => operation,
        Err(error) => {
            status.set_if_changed(format!("Could not open watermark picker: {error}"));
            return;
        }
    };
    status.set_if_changed("Select a watermark image…".to_owned());
    let task_scope = scope.clone();
    if let Err(error) = scope.spawn(async move {
        let selection = operation.await;
        match canvas::with_editor(|editor| {
            DOCUMENT_LOAD_REQUESTS.state(&pending, editor.document())
        }) {
            Some(editor_linux::DocumentLoadState::Ready) => {}
            Some(editor_linux::DocumentLoadState::Superseded) => return,
            Some(editor_linux::DocumentLoadState::DocumentChanged) => {
                status.set_if_changed(
                    "The document changed before the watermark was selected.".to_owned(),
                );
                return;
            }
            None => {
                status.set_if_changed("The editor is not available.".to_owned());
                return;
            }
        }
        match selection {
            Ok(PickerOutcome::Selected(items)) => {
                let Some(path) = items
                    .first()
                    .and_then(|item| item.local_path().map(Path::to_path_buf))
                else {
                    status.set_if_changed("The watermark must be a local image file.".to_owned());
                    return;
                };
                start_interactive_effect(
                    &task_scope,
                    pending,
                    "Watermark",
                    canvas,
                    status,
                    move || {
                        jiffshot_doc::io::load_image(&path)
                            .map(|image| effects::Effect::Watermark {
                                image: Arc::new(image),
                                opacity,
                            })
                            .map_err(|error| {
                                format!("Could not open watermark {}: {error}", path.display())
                            })
                    },
                );
            }
            Ok(PickerOutcome::Cancelled) => {
                status.set_if_changed("Watermark selection cancelled.".to_owned());
            }
            Err(error) => {
                status.set_if_changed(format!("Could not select a watermark: {error}"));
            }
        }
    }) {
        status.set_if_changed(format!("Could not start watermark picker: {error}"));
    }
}

fn start_interactive_effect(
    scope: &AppScope,
    pending: editor_linux::PendingDocumentLoad,
    name: &'static str,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
    load: impl FnOnce() -> std::result::Result<effects::Effect, String> + Send + 'static,
) {
    status.set_if_changed(format!("Preparing {name}…"));
    let task_scope = scope.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("jiffshot-image-effect".to_owned())
        .spawn(move || {
            let result = load();
            if let Err(error) = task_scope.post(move || {
                let outcome = canvas::with_editor(|editor| {
                    match DOCUMENT_LOAD_REQUESTS.state(&pending, editor.document()) {
                        editor_linux::DocumentLoadState::Ready => Some(result.and_then(|effect| {
                            editor_linux::apply_document_effect(editor, name, effect)
                        })),
                        editor_linux::DocumentLoadState::Superseded => None,
                        editor_linux::DocumentLoadState::DocumentChanged => Some(Err(format!(
                            "The document changed before {name} was ready."
                        ))),
                    }
                });
                match outcome {
                    Some(Some(Ok(()))) => {
                        canvas.get().refresh();
                        status.set_if_changed(format!("{name} applied"));
                    }
                    Some(Some(Err(error))) => status.set_if_changed(error),
                    Some(None) => {}
                    None => status.set_if_changed("The editor is not available.".to_owned()),
                }
            }) {
                eprintln!("Could not finish {name}: {error}");
            }
        })
    {
        status.set_if_changed(format!("Could not prepare {name}: {error}"));
    }
}

fn library_organization_panel(
    library: LibraryControls,
    captures: Signal<Vec<PathBuf>>,
    status: Signal<String>,
) -> El<StackPanel> {
    let automatic_copy_label = Signal::new(match settings_linux::automatic_copy_directory() {
        Ok(directory) => format_automatic_copy_label(directory.as_deref()),
        Err(error) => {
            status.set_if_changed(format!(
                "Could not load the automatic copy preference: {error}"
            ));
            "Automatic copy unavailable".to_owned()
        }
    });
    let actions = WrapPanel::el()
        .orientation(Orientation::Horizontal)
        .child(TextBlock::el().text("Tags (comma or semicolon separated):"))
        .child(TextBox::el().width(220.0).text(library.tags.two_way()))
        .child(action_button("Save tags", move |scope| {
            let Some(path) = library.selected.get() else {
                status.set_if_changed("Select a capture before editing tags.".to_owned());
                return;
            };
            match library_linux::set_tags(&path, &library.tags.get()) {
                Ok(tags) => {
                    library.tags.set_if_changed(tags.join(", "));
                    let message = if tags.is_empty() {
                        "Tags removed."
                    } else {
                        "Tags saved."
                    };
                    refresh_active_library(scope, captures, library, status, message);
                }
                Err(error) => status.set_if_changed(format!("Could not save tags: {error}")),
            }
        }))
        .child(action_button("Pin / unpin", move |scope| {
            let Some(path) = library.selected.get() else {
                status.set_if_changed("Select a capture before changing its pin.".to_owned());
                return;
            };
            match library_linux::toggle_pin(&path) {
                Ok(pinned) => {
                    library.pinned.set_if_changed(pinned);
                    let message = if pinned {
                        "Capture pinned."
                    } else {
                        "Capture unpinned."
                    };
                    refresh_active_library(scope, captures, library, status, message);
                }
                Err(error) => status.set_if_changed(format!("Could not change pin: {error}")),
            }
        }));
    let sharing = WrapPanel::el()
        .orientation(Orientation::Horizontal)
        .child(
            action_button("Open saved capture with…", move |scope| {
                share_selected_capture(scope, library, status, ShareAction::OpenWith)
            })
            .tool_tip_tip("Open the selected saved file, not unsaved editor changes"),
        )
        .child(
            action_button("Email saved capture…", move |scope| {
                share_selected_capture(scope, library, status, ShareAction::Email)
            })
            .tool_tip_tip("Attach the selected saved file, not unsaved editor changes"),
        )
        .child(
            action_button("Copy saved capture to folder…", move |scope| {
                copy_selected_capture_dialog(scope, library, status)
            })
            .tool_tip_tip("Copy the selected saved file without overwriting existing files"),
        )
        .child(
            action_button("Print saved image…", move |scope| {
                share_selected_capture(scope, library, status, ShareAction::Print)
            })
            .tool_tip_tip("Print the selected saved image, not unsaved editor changes"),
        );
    let automatic_copy = WrapPanel::el()
        .orientation(Orientation::Horizontal)
        .child(
            action_button("Auto-copy new captures…", move |scope| {
                choose_automatic_copy_folder(scope, status, automatic_copy_label)
            })
            .tool_tip_tip("Automatically copy newly saved images and recordings to a folder"),
        )
        .child(action_button("Disable auto-copy", move |_| {
            AUTOMATIC_COPY_REQUEST.fetch_add(1, Ordering::AcqRel);
            match settings_linux::set_automatic_copy_directory(None) {
                Ok(()) => {
                    automatic_copy_label.set_if_changed(format_automatic_copy_label(None));
                    status.set_if_changed("Automatic capture copying disabled.".to_owned());
                }
                Err(error) => {
                    status.set_if_changed(format!("Could not disable automatic copying: {error}"));
                }
            }
        }))
        .child(TextBlock::el().text(automatic_copy_label));
    StackPanel::el()
        .visible(library.visible)
        .child(TextBlock::el().text(move || library.selection_label()))
        .child(actions)
        .child(sharing)
        .child(automatic_copy)
}

fn format_automatic_copy_label(directory: Option<&Path>) -> String {
    match directory {
        Some(directory) => format!("Automatic copies: {}", directory.display()),
        None => "Automatic copies disabled".to_owned(),
    }
}

fn choose_automatic_copy_folder(scope: &AppScope, status: Signal<String>, label: Signal<String>) {
    let Some(window) = scope.main_window() else {
        status.set_if_changed("The editor window is not available.".to_owned());
        return;
    };
    let request_id = AUTOMATIC_COPY_REQUEST
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1);
    let options = FolderPickerOptions::new().title("Choose folder for automatic capture copies");
    let operation = match file_dialog_linux::open_folder_picker(scope, &window, &options) {
        Ok(operation) => operation,
        Err(error) => {
            status.set_if_changed(format!(
                "Could not open automatic copy folder picker: {error}"
            ));
            return;
        }
    };
    status.set_if_changed("Choose a folder for automatic capture copies…".to_owned());
    let task_scope = scope.clone();
    if let Err(error) = scope.spawn(async move {
        let selection = operation.await;
        if AUTOMATIC_COPY_REQUEST.load(Ordering::Acquire) != request_id {
            return;
        }
        let result = match selection {
            Ok(PickerOutcome::Selected(items)) => items
                .first()
                .and_then(|item| item.local_path().map(Path::to_path_buf))
                .ok_or_else(|| {
                    "the selected folder is not available as a local directory".to_owned()
                })
                .map(Some),
            Ok(PickerOutcome::Cancelled) => Ok(None),
            Err(error) => Err(format!(
                "could not choose an automatic copy folder: {error}"
            )),
        };
        if let Err(error) = task_scope.post(move || {
            if AUTOMATIC_COPY_REQUEST.load(Ordering::Acquire) != request_id {
                return;
            }
            match result {
                Ok(Some(directory)) => {
                    match settings_linux::set_automatic_copy_directory(Some(&directory)) {
                        Ok(()) => {
                            label.set_if_changed(format_automatic_copy_label(Some(&directory)));
                            status.set_if_changed(format!(
                                "New captures will be copied to {}.",
                                directory.display()
                            ));
                        }
                        Err(error) => status.set_if_changed(format!(
                            "Could not set the automatic copy folder: {error}"
                        )),
                    }
                }
                Ok(None) => {
                    status.set_if_changed("Automatic copy folder selection cancelled.".to_owned());
                }
                Err(error) => status
                    .set_if_changed(format!("Could not set the automatic copy folder: {error}")),
            }
        }) {
            eprintln!("Could not report the automatic copy preference: {error}");
        }
    }) {
        status.set_if_changed(format!(
            "Could not start automatic copy folder picker: {error}"
        ));
    }
}

#[derive(Clone, Copy)]
enum ShareAction {
    OpenWith,
    Email,
    Print,
}

fn share_selected_capture(
    scope: &AppScope,
    library: LibraryControls,
    status: Signal<String>,
    action: ShareAction,
) {
    let Some(path) = library.selected.get() else {
        status.set_if_changed("Select a saved capture before sharing.".to_owned());
        return;
    };
    status.set_if_changed("Waiting for the desktop sharing dialog…".to_owned());
    let task_scope = scope.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("jiffshot-share-capture".to_owned())
        .spawn(move || {
            let result = futures_lite::future::block_on(async {
                match action {
                    ShareAction::OpenWith => share_linux::open_with(&path).await,
                    ShareAction::Email => share_linux::email(&path).await,
                    ShareAction::Print => share_linux::print_image(&path).await,
                }
            });
            let message = match (action, result) {
                (ShareAction::OpenWith, Ok(true)) => {
                    "Opened the saved capture with the selected application.".to_owned()
                }
                (ShareAction::Email, Ok(true)) => {
                    "Opened an email with the saved capture attached.".to_owned()
                }
                (ShareAction::Print, Ok(true)) => {
                    "Sent the saved image to the print portal.".to_owned()
                }
                (_, Ok(false)) => "Sharing cancelled.".to_owned(),
                (_, Err(error)) => format!("Could not share the saved capture: {error}"),
            };
            if let Err(error) = task_scope.post(move || status.set_if_changed(message)) {
                eprintln!("Could not report capture sharing: {error}");
            }
        })
    {
        status.set_if_changed(format!("Could not start capture sharing: {error}"));
    }
}

fn copy_selected_capture_dialog(
    scope: &AppScope,
    library: LibraryControls,
    status: Signal<String>,
) {
    let Some(source) = library.selected.get() else {
        status.set_if_changed("Select a saved capture before sharing.".to_owned());
        return;
    };
    let Some(window) = scope.main_window() else {
        status.set_if_changed("The editor window is not available.".to_owned());
        return;
    };
    let options = FolderPickerOptions::new().title("Copy saved capture to folder");
    let operation = match file_dialog_linux::open_folder_picker(scope, &window, &options) {
        Ok(operation) => operation,
        Err(error) => {
            status.set_if_changed(format!("Could not open folder picker: {error}"));
            return;
        }
    };
    status.set_if_changed("Choose a destination for the saved capture…".to_owned());
    let task_scope = scope.clone();
    if let Err(error) = scope.spawn(async move {
        let destination = match operation.await {
            Ok(PickerOutcome::Selected(items)) => items
                .first()
                .and_then(|item| item.local_path().map(Path::to_path_buf))
                .ok_or_else(|| "the selected folder is not a local directory".to_owned()),
            Ok(PickerOutcome::Cancelled) => {
                if let Err(error) =
                    task_scope.post(move || status.set_if_changed("Copy cancelled.".to_owned()))
                {
                    eprintln!("Could not report the cancelled copy: {error}");
                }
                return;
            }
            Err(error) => Err(format!("could not choose a folder: {error}")),
        };
        let directory = match destination {
            Ok(directory) => directory,
            Err(error) => {
                if let Err(post_error) = task_scope.post(move || status.set_if_changed(error)) {
                    eprintln!("Could not report the folder selection error: {post_error}");
                }
                return;
            }
        };
        let worker_scope = task_scope.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("jiffshot-copy-capture".to_owned())
            .spawn(move || {
                let result = share_linux::copy_to_folder(&source, &directory);
                if let Err(error) = worker_scope.post(move || match result {
                    Ok(path) => status
                        .set_if_changed(format!("Copied saved capture to {}.", path.display())),
                    Err(error) => status.set_if_changed(format!("Could not copy capture: {error}")),
                }) {
                    eprintln!("Could not report capture copying: {error}");
                }
            })
        {
            if let Err(post_error) = task_scope.post(move || {
                status.set_if_changed(format!("Could not start copying the capture: {error}"))
            }) {
                eprintln!("Could not report the capture copy error: {post_error}");
            }
        }
    }) {
        status.set_if_changed(format!("Could not start folder selection: {error}"));
    }
}

fn recent_capture_button(
    path: Signal<PathBuf>,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    library: LibraryControls,
    status: Signal<String>,
) -> El<Button> {
    let path = path.get_untracked();
    let label = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Capture".to_owned());
    Button::el().content(label).with_op(move |scope, button| {
        let scope = scope.clone();
        let callback_scope = scope.clone();
        let path = path.clone();
        let canvas = canvas.clone();
        button.clone().on_click(&scope, move |_| {
            open_capture(
                &callback_scope,
                path.clone(),
                canvas.clone(),
                library,
                status,
            );
        })?;
        Ok(())
    })
}

fn combine_images_dialog(
    scope: &AppScope,
    arrangement: Arrangement,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
) {
    let Some(window) = scope.main_window() else {
        status.set_if_changed("The editor window is not available.".to_owned());
        return;
    };
    let filter = ["png", "jpg", "jpeg", "bmp", "gif", "webp"]
        .into_iter()
        .fold(FileTypeFilter::new("Image files"), |filter, extension| {
            filter.with_extension(extension)
        });
    let options = OpenFilePickerOptions::new()
        .title("Select images to combine")
        .allow_multiple(true)
        .start_in(WellKnownFolder::Pictures)
        .file_type(filter);
    let operation = match file_dialog_linux::open_file_picker(scope, &window, &options) {
        Ok(operation) => operation,
        Err(error) => {
            status.set_if_changed(format!("Could not open image picker: {error}"));
            return;
        }
    };
    status.set_if_changed("Select at least two images to combine…".to_owned());
    let task_scope = scope.clone();
    let failed_status = status;
    if let Err(error) = scope.spawn(async move {
        let result: std::result::Result<Document, String> = match operation.await {
            Ok(PickerOutcome::Selected(items)) => {
                let paths = items
                    .iter()
                    .filter_map(|item| item.local_path().map(Path::to_path_buf))
                    .collect::<Vec<_>>();
                combine_image_files(&paths, arrangement)
            }
            Ok(PickerOutcome::Cancelled) => return,
            Err(error) => Err(format!("could not select images: {error}")),
        };
        if let Err(error) = task_scope.post(move || match result {
            Ok(document) => {
                canvas::with_editor(|editor| load_editor_document(editor, document, None));
                canvas.get().refresh();
                status.set_if_changed(
                    "Combined selected images into an editable document.".to_owned(),
                );
            }
            Err(error) => status.set_if_changed(error),
        }) {
            eprintln!("Could not update the editor with combined images: {error}");
        }
    }) {
        failed_status.set_if_changed(format!("Could not start image combining: {error}"));
    }
}

fn combine_image_files(
    paths: &[PathBuf],
    arrangement: Arrangement,
) -> std::result::Result<Document, String> {
    if paths.len() < 2 {
        return Err("select at least two local image files".to_owned());
    }
    let images = paths
        .iter()
        .map(|path| {
            jiffshot_doc::io::load_image(path)
                .map_err(|error| format!("could not open {}: {error}", path.display()))
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    jiffshot_doc::combine::combine(&images, arrangement, 12.0, Rgba::WHITE)
        .ok_or_else(|| "could not combine the selected images".to_owned())
}

fn open_capture(
    scope: &AppScope,
    path: PathBuf,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    library: LibraryControls,
    status: Signal<String>,
) {
    let request = LIBRARY_OPEN_REQUEST
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1);
    if let Some(pending) = begin_document_load(status) {
        start_document_load(
            scope,
            path,
            DocumentLoadKind::File,
            pending,
            canvas,
            status,
            Some((library, request)),
        );
    }
}

fn begin_document_load(status: Signal<String>) -> Option<editor_linux::PendingDocumentLoad> {
    match canvas::with_editor(|editor| editor.document().clone()) {
        Some(snapshot) => Some(DOCUMENT_LOAD_REQUESTS.begin(snapshot)),
        None => {
            status.set_if_changed("The editor is not available.".to_owned());
            None
        }
    }
}

fn start_document_load(
    scope: &AppScope,
    path: PathBuf,
    kind: DocumentLoadKind,
    pending: editor_linux::PendingDocumentLoad,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
    library: Option<(LibraryControls, u64)>,
) {
    match canvas::with_editor(|editor| DOCUMENT_LOAD_REQUESTS.state(&pending, editor.document())) {
        Some(editor_linux::DocumentLoadState::Ready) => {}
        Some(editor_linux::DocumentLoadState::Superseded) => return,
        Some(editor_linux::DocumentLoadState::DocumentChanged) => {
            status
                .set_if_changed("The document changed before the file could be opened.".to_owned());
            return;
        }
        None => {
            status.set_if_changed("The editor is not available.".to_owned());
            return;
        }
    }
    status.set_if_changed(format!("Opening {}…", path.display()));
    let task_scope = scope.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("jiffshot-open-document".to_owned())
        .spawn(move || {
            let metadata = library.map(|_| library_linux::read_metadata(&path));
            let opened = match kind {
                DocumentLoadKind::File if video_linux::is_video_path(&path) => {
                    video_linux::first_frame(&path)
                        .map(|document| (document, "Imported first video frame".to_owned()))
                }
                DocumentLoadKind::File => {
                    jiffshot_doc::io::open(&path).map(|document| (document, "Opened".to_owned()))
                }
                DocumentLoadKind::VideoFrame(timestamp) => video_linux::frame_at(&path, timestamp)
                    .map(|document| (document, format!("Imported frame at {timestamp} seconds"))),
            };
            if let Err(error) = task_scope.post(move || {
                if let Some((_, request)) = library {
                    if LIBRARY_OPEN_REQUEST.load(Ordering::Acquire) != request {
                        return;
                    }
                }
                let delivery = canvas::with_editor(|editor| {
                    match DOCUMENT_LOAD_REQUESTS.state(&pending, editor.document()) {
                        editor_linux::DocumentLoadState::Superseded => {
                            DocumentLoadDelivery::Superseded
                        }
                        editor_linux::DocumentLoadState::DocumentChanged => {
                            DocumentLoadDelivery::DocumentChanged
                        }
                        editor_linux::DocumentLoadState::Ready => match opened {
                            Ok((document, action)) => {
                                load_editor_document(editor, document, Some(path.clone()));
                                DocumentLoadDelivery::Loaded(action)
                            }
                            Err(error) => DocumentLoadDelivery::Failed(error),
                        },
                    }
                });
                let Some(delivery) = delivery else {
                    status.set_if_changed("The editor is not available.".to_owned());
                    return;
                };
                let result = match delivery {
                    DocumentLoadDelivery::Loaded(action) => Ok(action),
                    DocumentLoadDelivery::Failed(error) => Err(error),
                    DocumentLoadDelivery::Superseded => return,
                    DocumentLoadDelivery::DocumentChanged => {
                        status.set_if_changed(
                            "The document changed before the file finished opening.".to_owned(),
                        );
                        return;
                    }
                };
                let metadata_error = match (library, metadata) {
                    (Some((library, _)), Some(Ok(metadata))) => {
                        library.selected.set_if_changed(Some(path.clone()));
                        library.tags.set_if_changed(metadata.tags.join(", "));
                        library.pinned.set_if_changed(metadata.pinned);
                        None
                    }
                    (Some((library, _)), Some(Err(error))) => {
                        library.clear_selection();
                        Some(error)
                    }
                    _ => None,
                };
                let mut message = match result {
                    Ok(action) => {
                        canvas.get().refresh();
                        format!("{action} from {}", path.display())
                    }
                    Err(error) => {
                        format!("Could not open {}: {error}", path.display())
                    }
                };
                if let Some(error) = metadata_error {
                    message.push_str(&format!("; capture organization unavailable: {error}"));
                }
                status.set_if_changed(message);
            }) {
                eprintln!("Could not update the editor after opening a file: {error}");
            }
        })
    {
        status.set_if_changed(format!("Could not start opening the file: {error}"));
    }
}

fn library_status(context: Option<&str>, detail: &str) -> String {
    match context {
        Some(context) => format!("{context} {detail}"),
        None => detail.to_owned(),
    }
}

fn refresh_capture_library(
    captures: Signal<Vec<PathBuf>>,
    library: LibraryControls,
    status: Signal<String>,
    context: Option<&str>,
) {
    LIBRARY_REQUEST.fetch_add(1, Ordering::AcqRel);
    match capture_linux::recent_captures() {
        Ok(recent) => {
            let count = recent.len();
            replace_capture_library(captures, library, recent);
            status.set_if_changed(library_status(
                context,
                &format!("Loaded {count} recent captures."),
            ));
        }
        Err(error) => {
            status.set_if_changed(library_status(
                context,
                &format!("Could not refresh capture library: {error}"),
            ));
        }
    }
}

fn replace_capture_library(
    captures: Signal<Vec<PathBuf>>,
    library: LibraryControls,
    paths: Vec<PathBuf>,
) {
    LIBRARY_REQUEST.fetch_add(1, Ordering::AcqRel);
    library.recent.set_if_changed(true);
    captures.set_if_changed(paths);
}

fn start_library_search(
    scope: &AppScope,
    captures: Signal<Vec<PathBuf>>,
    library: LibraryControls,
    filter: library_linux::CaptureFilter,
    status: Signal<String>,
) {
    let query = library.query.get();
    library.active_query.set_if_changed(query.clone());
    library.filter.set_if_changed(filter);
    library.recent.set_if_changed(false);
    search_capture_library(scope, captures, query, filter, status, None);
}

fn refresh_active_library(
    scope: &AppScope,
    captures: Signal<Vec<PathBuf>>,
    library: LibraryControls,
    status: Signal<String>,
    context: &str,
) {
    if library.recent.get() {
        refresh_capture_library(captures, library, status, Some(context));
    } else {
        search_capture_library(
            scope,
            captures,
            library.active_query.get(),
            library.filter.get(),
            status,
            Some(context.to_owned()),
        );
    }
}

fn search_capture_library(
    scope: &AppScope,
    captures: Signal<Vec<PathBuf>>,
    query: String,
    filter: library_linux::CaptureFilter,
    status: Signal<String>,
    context: Option<String>,
) {
    let request = LIBRARY_REQUEST
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1);
    status.set_if_changed(library_status(
        context.as_deref(),
        "Searching capture library…",
    ));
    let task_scope = scope.clone();
    let launch_context = context.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("capture-library-search".to_owned())
        .spawn(move || {
            let result = library_linux::search_captures(&query, filter);
            if let Err(error) = task_scope.post(move || {
                if LIBRARY_REQUEST.load(Ordering::Acquire) != request {
                    return;
                }
                match result {
                    Ok(found) => {
                        let count = found.len();
                        captures.set_if_changed(found);
                        let plural = if count == 1 { "" } else { "s" };
                        status.set_if_changed(library_status(
                            context.as_deref(),
                            &format!("Found {count} {} capture{plural}.", filter.label()),
                        ));
                    }
                    Err(error) => {
                        status.set_if_changed(library_status(
                            context.as_deref(),
                            &format!("Could not search capture library: {error}"),
                        ));
                    }
                }
            }) {
                eprintln!("Could not report capture library search: {error}");
            }
        })
    {
        status.set_if_changed(library_status(
            launch_context.as_deref(),
            &format!("Could not start capture library search: {error}"),
        ));
    }
}

fn tool_button(
    label: &'static str,
    tool: Tool,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
) -> El<Button> {
    action_button(label, move |_| {
        canvas::with_editor(|editor| editor.set_tool(tool));
        canvas.get().refresh();
        status.set_if_changed(format!("{label} tool selected"));
    })
}

fn enable_global_capture_shortcut(scope: &AppScope, ui: ShortcutUi) {
    let ShortcutUi {
        canvas,
        captures,
        library,
        status,
        preferred,
        recording_controls,
        recording_start,
        webcam,
    } = ui;
    if GLOBAL_SHORTCUT_STOP.lock().is_some() {
        status.set_if_changed(
            "The global capture shortcut is already active or starting.".to_owned(),
        );
        return;
    }
    let recording_start =
        match shortcut_linux::RecordingStartShortcut::from_index(recording_start.get()) {
            Ok(choice) => choice,
            Err(error) => {
                status.set_if_changed(format!("Could not enable global shortcuts: {error}"));
                return;
            }
        };
    let options = shortcut_linux::ShortcutOptions {
        recording_controls: recording_controls.get(),
        recording_start,
    };
    let trigger = match settings_linux::set_global_shortcuts(&preferred.get(), options) {
        Ok(trigger) => trigger,
        Err(error) => {
            status.set_if_changed(format!(
                "Could not save global shortcut preference: {error}"
            ));
            return;
        }
    };
    preferred.set_if_changed(trigger.clone());
    let (stop, receiver) = async_channel::bounded(1);
    *GLOBAL_SHORTCUT_STOP.lock() = Some(stop);
    status.set_if_changed("Requesting the global capture shortcut…".to_owned());

    let task_scope = scope.clone();
    let failed_status = status;
    let spawn_result = scope.spawn(async move {
        let bound_scope = task_scope.clone();
        let activation_scope = task_scope.clone();
        let result = shortcut_linux::listen(
            trigger,
            options,
            receiver,
            move |bindings| {
                let message = if bindings.len() == 1 {
                    format!("Global capture shortcut enabled: {}", bindings[0].1)
                } else {
                    let descriptions: Vec<_> = bindings
                        .iter()
                        .map(|(action, trigger)| {
                            let label = match action {
                                shortcut_linux::ShortcutAction::Capture => "capture",
                                shortcut_linux::ShortcutAction::ToggleRecording => "start/stop",
                                shortcut_linux::ShortcutAction::PauseRecording => "pause/resume",
                                shortcut_linux::ShortcutAction::StopRecording => "stop",
                            };
                            format!("{label}: {trigger}")
                        })
                        .collect();
                    format!("Global shortcuts enabled: {}", descriptions.join("; "))
                };
                bound_scope
                    .post(move || status.set_if_changed(message))
                    .map_err(|error| format!("could not report global shortcut status: {error}"))
            },
            move |action| {
                let action_scope = activation_scope.clone();
                let action_canvas = canvas.clone();
                activation_scope
                    .post(move || match action {
                        shortcut_linux::ShortcutAction::Capture => {
                            capture_screen(&action_scope, action_canvas, status, captures, library)
                        }
                        shortcut_linux::ShortcutAction::ToggleRecording => {
                            if let Some(audio) = options.recording_start.audio() {
                                toggle_screen_recording(
                                    &action_scope,
                                    status,
                                    captures,
                                    library,
                                    audio,
                                    webcam.get(),
                                );
                            } else {
                                status.set_if_changed(
                                    "The recording start shortcut is disabled.".to_owned(),
                                );
                            }
                        }
                        shortcut_linux::ShortcutAction::PauseRecording => {
                            toggle_recording_pause(status)
                        }
                        shortcut_linux::ShortcutAction::StopRecording => send_recording_command(
                            capture_linux::RecordingCommand::Stop,
                            "Finalizing screen recording…",
                            status,
                        ),
                    })
                    .map_err(|error| format!("could not handle a global shortcut: {error}"))
            },
        )
        .await;

        *GLOBAL_SHORTCUT_STOP.lock() = None;
        let (message, failed) = match result {
            Ok(shortcut_linux::ShortcutEnd::Stopped) => {
                ("Global shortcuts disabled.".to_owned(), false)
            }
            Ok(shortcut_linux::ShortcutEnd::ClosedByDesktop) => {
                ("The desktop closed the global shortcuts.".to_owned(), true)
            }
            Err(error) => (error, true),
        };
        if let Err(error) = task_scope.post(move || {
            if failed {
                report_capture_result(
                    status,
                    "Global shortcuts unavailable",
                    message,
                    notification_linux::Urgency::Critical,
                );
            } else {
                status.set_if_changed(message);
            }
        }) {
            eprintln!("Could not report global shortcut status: {error}");
        }
    });
    if let Err(error) = spawn_result {
        *GLOBAL_SHORTCUT_STOP.lock() = None;
        failed_status.set_if_changed(format!(
            "Could not start global shortcut registration: {error}"
        ));
    }
}

fn disable_global_capture_shortcut(status: Signal<String>) {
    if let Err(error) = settings_linux::disable_global_shortcuts() {
        status.set_if_changed(format!(
            "Could not disable the global shortcut preference: {error}"
        ));
        return;
    }
    let stop = GLOBAL_SHORTCUT_STOP.lock().as_ref().cloned();
    match stop {
        Some(stop) => match stop.try_send(()) {
            Ok(()) => status.set_if_changed("Disabling the global capture shortcut…".to_owned()),
            Err(async_channel::TrySendError::Full(())) => {
                status.set_if_changed("The global capture shortcut is already stopping.".to_owned())
            }
            Err(async_channel::TrySendError::Closed(())) => {
                status.set_if_changed("The global capture shortcut is no longer active.".to_owned())
            }
        },
        None => status.set_if_changed("Global shortcuts disabled.".to_owned()),
    }
}

#[derive(Clone, Copy)]
struct ScreenCaptureOptions {
    seconds: u64,
    cursor: capture_linux::CaptureCursor,
}

fn screen_capture_available(status: Signal<String>) -> bool {
    if SCHEDULED_CAPTURE_ACTIVE.load(Ordering::Acquire) {
        report_capture_result(
            status,
            "Capture skipped",
            "Finish the daily capture before starting another capture.".to_owned(),
            notification_linux::Urgency::Normal,
        );
        return false;
    }
    true
}

fn capture_screen(
    scope: &AppScope,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
    captures: Signal<Vec<PathBuf>>,
    library: LibraryControls,
) {
    if !screen_capture_available(status) {
        return;
    }
    let seconds = match settings_linux::capture_delay_seconds() {
        Ok(seconds) => seconds,
        Err(error) => {
            report_capture_result(
                status,
                "Capture failed",
                format!("Could not read the capture delay: {error}"),
                notification_linux::Urgency::Critical,
            );
            return;
        }
    };
    let cursor = match settings_linux::capture_cursor() {
        Ok(cursor) => cursor,
        Err(error) => {
            report_capture_result(
                status,
                "Capture failed",
                format!("Could not read the capture cursor setting: {error}"),
                notification_linux::Urgency::Critical,
            );
            return;
        }
    };
    capture_screen_with_options(
        scope,
        canvas,
        status,
        captures,
        library,
        ScreenCaptureOptions { seconds, cursor },
    );
}

fn capture_screen_with_options(
    scope: &AppScope,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
    captures: Signal<Vec<PathBuf>>,
    library: LibraryControls,
    options: ScreenCaptureOptions,
) {
    if !screen_capture_available(status) {
        return;
    }
    let ScreenCaptureOptions { seconds, cursor } = options;
    let delayed = if seconds == 0 {
        None
    } else {
        let mut active = DELAYED_CAPTURE.lock();
        if active.is_some() {
            report_capture_result(
                status,
                "Capture skipped",
                "A delayed capture is already in progress.".to_owned(),
                notification_linux::Urgency::Normal,
            );
            return;
        }
        let (cancel, receiver) = mpsc::sync_channel(1);
        let id = NEXT_DELAYED_CAPTURE.fetch_add(1, Ordering::Relaxed);
        *active = Some(DelayedCapture { id, cancel });
        Some((id, receiver))
    };
    let delayed_id = delayed.as_ref().map(|(id, _)| *id);
    let Some(pending) = begin_document_load(status) else {
        if let Some(id) = delayed_id {
            finish_delayed_capture(id);
        }
        return;
    };
    let active = SingleCaptureGuard::new();
    status.set_if_changed("Choose a screen or window in the desktop dialog…".to_owned());
    let task_scope = scope.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("jiffshot-screen-capture".to_owned())
        .spawn(move || {
            let result = if let Some((id, receiver)) = delayed {
                let waiting_scope = task_scope.clone();
                let result =
                    futures_lite::future::block_on(capture_linux::capture_monitor_after_delay(
                        cursor,
                        Duration::from_secs(seconds),
                        &receiver,
                        move || {
                            if let Err(error) = waiting_scope.post(move || {
                                status.set_if_changed(format!(
                                    "Capturing in {seconds} seconds; open the content to capture."
                                ))
                            }) {
                                eprintln!("Could not report the capture countdown: {error}");
                            }
                        },
                        move || finish_delayed_capture(id),
                    ));
                finish_delayed_capture(id);
                result
            } else {
                futures_lite::future::block_on(capture_linux::capture_monitor(cursor)).map(Some)
            };
            drop(active);
            match result {
                Ok(Some(document)) => deliver_capture(
                    &task_scope,
                    CompletedCapture {
                        document,
                        pending,
                        delivery: CaptureDelivery::Screen,
                    },
                    canvas,
                    status,
                    captures,
                    library,
                ),
                Ok(None) => {
                    if let Err(error) = task_scope.post(move || {
                        status.set_if_changed("Delayed capture cancelled.".to_owned())
                    }) {
                        eprintln!("Could not report the cancelled capture: {error}");
                    }
                }
                Err(error) => {
                    if let Err(post_error) = task_scope.post(move || {
                        report_capture_result(
                            status,
                            "Capture failed",
                            error,
                            notification_linux::Urgency::Critical,
                        )
                    }) {
                        eprintln!("Could not report the screen capture error: {post_error}");
                    }
                }
            }
        })
    {
        if let Some(id) = delayed_id {
            finish_delayed_capture(id);
        }
        report_capture_result(
            status,
            "Capture failed",
            format!("Could not start screen capture: {error}"),
            notification_linux::Urgency::Critical,
        );
    }
}

fn finish_delayed_capture(id: u64) {
    let mut active = DELAYED_CAPTURE.lock();
    if active.as_ref().is_some_and(|capture| capture.id == id) {
        *active = None;
    }
}

fn cancel_delayed_capture(status: Signal<String>) {
    let active = DELAYED_CAPTURE.lock();
    let Some(capture) = active.as_ref() else {
        status.set_if_changed("There is no delayed capture in progress.".to_owned());
        return;
    };
    let message = match capture.cancel.try_send(()) {
        Ok(()) => "Cancellation requested; close the desktop chooser if it is still open.",
        Err(TrySendError::Full(())) => "Delayed capture cancellation is already requested.",
        Err(TrySendError::Disconnected(())) => "The delayed capture is already finishing.",
    };
    status.set_if_changed(message.to_owned());
}

fn capture_interactive(
    scope: &AppScope,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
    captures: Signal<Vec<PathBuf>>,
    library: LibraryControls,
) {
    if SCHEDULED_CAPTURE_ACTIVE.load(Ordering::Acquire) {
        status
            .set_if_changed("Finish the daily capture before starting another capture.".to_owned());
        return;
    }
    let Some(pending) = begin_document_load(status) else {
        return;
    };
    let active = SingleCaptureGuard::new();
    status.set_if_changed("Choose a screenshot in the desktop dialog…".to_owned());
    let task_scope = scope.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("jiffshot-desktop-screenshot".to_owned())
        .spawn(move || {
            let result = futures_lite::future::block_on(screenshot_linux::capture_interactive());
            drop(active);
            match result {
                Ok(Some(document)) => deliver_capture(
                    &task_scope,
                    CompletedCapture {
                        document,
                        pending,
                        delivery: CaptureDelivery::Interactive,
                    },
                    canvas,
                    status,
                    captures,
                    library,
                ),
                Ok(None) => {
                    if let Err(error) = task_scope.post(move || {
                        status.set_if_changed("Desktop screenshot cancelled.".to_owned())
                    }) {
                        eprintln!("Could not report screenshot cancellation: {error}");
                    }
                }
                Err(error) => {
                    if let Err(post_error) = task_scope.post(move || status.set_if_changed(error)) {
                        eprintln!("Could not report the screenshot error: {post_error}");
                    }
                }
            }
        })
    {
        status.set_if_changed(format!("Could not start desktop screenshot: {error}"));
    }
}

enum GrabTextResult {
    Cancelled,
    NoText,
    Copied(usize),
}

fn grab_screen_text(scope: &AppScope, status: Signal<String>) {
    if SCHEDULED_CAPTURE_ACTIVE.load(Ordering::Acquire) {
        status.set_if_changed("Finish the daily capture before grabbing text.".to_owned());
        return;
    }
    let active = SingleCaptureGuard::new();
    status.set_if_changed("Choose an area containing text in the desktop dialog…".to_owned());
    let task_scope = scope.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("jiffshot-grab-screen-text".to_owned())
        .spawn(move || {
            let result: std::result::Result<GrabTextResult, String> = (|| {
                let Some(document) =
                    futures_lite::future::block_on(screenshot_linux::capture_interactive())?
                else {
                    return Ok(GrabTextResult::Cancelled);
                };
                let lines = ocr_linux::Recognizer::new()?.recognize(&document.flatten())?;
                let text = editor_linux::recognized_text(&lines);
                if text.is_empty() {
                    return Ok(GrabTextResult::NoText);
                }
                let count = text.lines().count();
                clipboard_linux::copy_text(text)?;
                Ok(GrabTextResult::Copied(count))
            })();
            drop(active);
            if let Err(error) = task_scope.post(move || {
                let message = match result {
                    Ok(GrabTextResult::Cancelled) => "Text capture cancelled.".to_owned(),
                    Ok(GrabTextResult::NoText) => {
                        "No text recognized; the clipboard was not changed.".to_owned()
                    }
                    Ok(GrabTextResult::Copied(count)) => {
                        format!("Copied {count} recognized line(s) from the screen.")
                    }
                    Err(error) => format!("Could not grab screen text: {error}"),
                };
                status.set_if_changed(message);
            }) {
                eprintln!("Could not report screen text capture: {error}");
            }
        })
    {
        status.set_if_changed(format!("Could not start screen text capture: {error}"));
    }
}

#[derive(Clone, Copy)]
enum CaptureDelivery {
    Screen,
    Interactive,
    Scrolling {
        mode: scroll_linux::Mode,
        limit_reached: bool,
    },
}

struct CompletedCapture {
    document: Document,
    pending: editor_linux::PendingDocumentLoad,
    delivery: CaptureDelivery,
}

impl CaptureDelivery {
    fn description(self) -> &'static str {
        match self {
            Self::Screen => "Screen captured",
            Self::Interactive => "Desktop screenshot captured",
            Self::Scrolling {
                mode: scroll_linux::Mode::Panoramic,
                limit_reached: true,
            } => "Panoramic capture reached its size limit",
            Self::Scrolling {
                mode: scroll_linux::Mode::Panoramic,
                limit_reached: false,
            } => "Panoramic capture completed",
            Self::Scrolling {
                limit_reached: true,
                ..
            } => "Scrolling capture reached its size limit",
            Self::Scrolling {
                limit_reached: false,
                ..
            } => "Scrolling capture completed",
        }
    }

    fn complete(self) {
        if matches!(self, Self::Scrolling { .. }) {
            *SCROLL_CAPTURE.lock() = None;
        }
    }
}

fn deliver_capture(
    scope: &AppScope,
    completed: CompletedCapture,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
    captures: Signal<Vec<PathBuf>>,
    library: LibraryControls,
) {
    let CompletedCapture {
        document,
        pending,
        delivery,
    } = completed;
    let saved_path = capture_linux::save_capture(&document);
    let automatic_copy = saved_path
        .as_ref()
        .ok()
        .map(|path| share_linux::copy_automatically(path));
    let copied = match settings_linux::copy_capture_to_clipboard() {
        Ok(true) => Some(clipboard_linux::copy_pixmap(document.flatten())),
        Ok(false) => None,
        Err(error) => Some(Err(format!(
            "could not read the capture clipboard preference: {error}"
        ))),
    };
    let recent = capture_linux::recent_captures();
    let urgency = if saved_path.is_err()
        || copied.as_ref().is_some_and(std::result::Result::is_err)
        || automatic_copy
            .as_ref()
            .is_some_and(std::result::Result::is_err)
        || recent.is_err()
    {
        notification_linux::Urgency::Critical
    } else {
        notification_linux::Urgency::Normal
    };
    if let Err(error) = scope.post(move || {
        delivery.complete();
        let editor_state = canvas::with_editor(|editor| {
            let state = DOCUMENT_LOAD_REQUESTS.state(&pending, editor.document());
            if state == editor_linux::DocumentLoadState::Ready {
                load_editor_document(editor, document, saved_path.as_ref().ok().cloned());
            }
            state
        });
        let editor_notice = match editor_state {
            Some(editor_linux::DocumentLoadState::Ready) => {
                canvas.get().refresh();
                None
            }
            Some(editor_linux::DocumentLoadState::Superseded) => {
                Some("a newer open or capture request was kept")
            }
            Some(editor_linux::DocumentLoadState::DocumentChanged) => {
                Some("newer editor changes were kept")
            }
            None => Some("the editor was unavailable"),
        };
        let library_error = match recent {
            Ok(recent) => {
                replace_capture_library(captures, library, recent);
                None
            }
            Err(error) => Some(error),
        };
        let description = delivery.description();
        let mut message = match saved_path {
            Ok(path) => format!("{description} and saved to {}", path.display()),
            Err(error) => format!("{description} but could not save it: {error}"),
        };
        if let Some(error) = library_error {
            message.push_str(&format!("; capture library could not refresh: {error}"));
        }
        if let Some(notice) = editor_notice {
            message.push_str(&format!("; {notice}"));
        }
        match copied {
            Some(Ok(())) => message.push_str("; image copied to the clipboard"),
            Some(Err(error)) => {
                message = format!("Clipboard unavailable: {error}; {message}");
            }
            None => {}
        }
        if let Some(result) = automatic_copy {
            append_automatic_copy_result(&mut message, result);
        }
        report_capture_result(status, "Capture result", message, urgency);
    }) {
        delivery.complete();
        eprintln!("Could not open the captured image in the editor: {error}");
    }
}

fn append_automatic_copy_result(
    message: &mut String,
    result: std::result::Result<Option<PathBuf>, String>,
) {
    match result {
        Ok(Some(path)) => {
            message.push_str(&format!("; automatically copied to {}", path.display()))
        }
        Ok(None) => {}
        Err(error) => *message = format!("Automatic copy failed: {error}; {message}"),
    }
}

fn toggle_scrolling_capture(
    scope: &AppScope,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
    captures: Signal<Vec<PathBuf>>,
    library: LibraryControls,
    mode: scroll_linux::Mode,
) {
    if let Some(active) = SCROLL_CAPTURE.lock().as_ref() {
        let same_kind = matches!(
            (active.mode, mode),
            (
                scroll_linux::Mode::Scrolling(_),
                scroll_linux::Mode::Scrolling(_)
            ) | (scroll_linux::Mode::Panoramic, scroll_linux::Mode::Panoramic)
        );
        if !same_kind {
            status.set_if_changed(
                "Finish the active scrolling or panoramic capture first.".to_owned(),
            );
            return;
        }
        if active.stop.try_send(scroll_linux::Action::Finish).is_ok() {
            status.set_if_changed(
                match active.mode {
                    scroll_linux::Mode::Scrolling(_) => "Finishing scrolling capture…",
                    scroll_linux::Mode::Panoramic => "Finishing panoramic capture…",
                }
                .to_owned(),
            );
        } else {
            status.set_if_changed("Capture is already stopping.".to_owned());
        }
        return;
    }
    if RECORDING.lock().is_some() {
        status.set_if_changed("Finish screen recording before starting capture.".to_owned());
        return;
    }
    if SCHEDULED_CAPTURE_ACTIVE.load(Ordering::Acquire) {
        status.set_if_changed("Finish the daily capture before starting capture.".to_owned());
        return;
    }
    if INTERVAL_CAPTURE.lock().is_some() {
        status.set_if_changed("Stop interval capture before starting capture.".to_owned());
        return;
    }
    let Some(pending) = begin_document_load(status) else {
        return;
    };
    let (stop, receiver) = async_channel::bounded(1);
    *SCROLL_CAPTURE.lock() = Some(ActiveScrollCapture { mode, stop });
    status.set_if_changed(match mode {
        scroll_linux::Mode::Scrolling(_) => {
            "Choose a window or monitor for scrolling capture…".to_owned()
        }
        scroll_linux::Mode::Panoramic => {
            "Choose a window or monitor for panoramic capture…".to_owned()
        }
    });
    let task_scope = scope.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("jiffshot-scrolling-capture".to_owned())
        .spawn(move || {
            let progress_scope = task_scope.clone();
            let result = futures_lite::future::block_on(scroll_linux::capture(
                mode,
                receiver,
                move |progress| {
                    let message = match progress {
                        scroll_linux::Progress::Started => match mode {
                            scroll_linux::Mode::Scrolling(scroll_linux::Direction::Vertical) => {
                                "Scroll the selected content down slowly, then finish the capture."
                            }
                            scroll_linux::Mode::Scrolling(scroll_linux::Direction::Horizontal) => {
                                "Scroll the selected content right slowly, then finish the capture."
                            }
                            scroll_linux::Mode::Panoramic => {
                                "Pan the selected content slowly in any direction, then finish the capture."
                            }
                        }
                        .to_owned(),
                        scroll_linux::Progress::Advanced(extent) => {
                            format!(
                                "Scrolling capture: {extent} pixels. Keep scrolling or finish the capture."
                            )
                        }
                        scroll_linux::Progress::PanoramicAdvanced { width, height } => {
                            format!(
                                "Panoramic capture: {width} × {height} pixels. Keep panning or finish the capture."
                            )
                        }
                        scroll_linux::Progress::LostOverlap => {
                            "Lost overlap; move back a little and scroll more slowly.".to_owned()
                        }
                    };
                    if let Err(error) = progress_scope.post(move || status.set_if_changed(message))
                    {
                        eprintln!("Could not report scrolling capture progress: {error}");
                    }
                },
            ));
            match result {
                Ok(Some(captured)) => {
                    deliver_capture(
                        &task_scope,
                        CompletedCapture {
                            document: captured.document,
                            pending,
                            delivery: CaptureDelivery::Scrolling {
                                mode,
                                limit_reached: captured.limit_reached,
                            },
                        },
                        canvas,
                        status,
                        captures,
                        library,
                    );
                }
                Ok(None) => {
                    if let Err(error) = task_scope.post(move || {
                        *SCROLL_CAPTURE.lock() = None;
                        status.set_if_changed(match mode {
                            scroll_linux::Mode::Scrolling(_) => "Scrolling capture cancelled.",
                            scroll_linux::Mode::Panoramic => "Panoramic capture cancelled.",
                        }.to_owned());
                    }) {
                        *SCROLL_CAPTURE.lock() = None;
                        eprintln!("Could not report scrolling capture cancellation: {error}");
                    }
                }
                Err(error) => {
                    if let Err(post_error) = task_scope.post(move || {
                        *SCROLL_CAPTURE.lock() = None;
                        let description = match mode {
                            scroll_linux::Mode::Scrolling(_) => "Scrolling",
                            scroll_linux::Mode::Panoramic => "Panoramic",
                        };
                        report_capture_result(
                            status,
                            "Capture failed",
                            format!("{description} capture failed: {error}"),
                            notification_linux::Urgency::Critical,
                        );
                    }) {
                        *SCROLL_CAPTURE.lock() = None;
                        eprintln!("Could not report scrolling capture error: {post_error}");
                    }
                }
            }
        })
    {
        *SCROLL_CAPTURE.lock() = None;
        report_capture_result(
            status,
            "Capture failed",
            format!("Could not start scrolling capture: {error}"),
            notification_linux::Urgency::Critical,
        );
    }
}

fn toggle_interval_capture(
    scope: &AppScope,
    status: Signal<String>,
    captures: Signal<Vec<PathBuf>>,
    library: LibraryControls,
    seconds: &str,
    skip_identical: bool,
) {
    if let Some(stop) = INTERVAL_CAPTURE.lock().as_ref() {
        let message = match stop.try_send(()) {
            Ok(()) => "Stopping interval capture; close the desktop chooser if it is still open.",
            Err(async_channel::TrySendError::Full(())) => "Interval capture is already stopping.",
            Err(async_channel::TrySendError::Closed(())) => {
                "Interval capture is already finishing."
            }
        };
        status.set_if_changed(message.to_owned());
        return;
    }
    if RECORDING.lock().is_some() || SCROLL_CAPTURE.lock().is_some() {
        status.set_if_changed(
            "Finish screen recording, scrolling or panoramic capture before starting interval capture."
                .to_owned(),
        );
        return;
    }
    if SCHEDULED_CAPTURE_ACTIVE.load(Ordering::Acquire) {
        status.set_if_changed(
            "Finish the daily capture before starting interval capture.".to_owned(),
        );
        return;
    }
    let preferences = match seconds
        .trim()
        .parse::<u32>()
        .map_err(|_| "the interval must be a whole number of seconds".to_owned())
        .and_then(|seconds| settings_linux::IntervalPreferences::new(seconds, skip_identical))
    {
        Ok(preferences) => preferences,
        Err(error) => {
            status.set_if_changed(format!("Could not start interval capture: {error}"));
            return;
        }
    };
    let cursor = match settings_linux::capture_cursor() {
        Ok(cursor) => cursor,
        Err(error) => {
            status.set_if_changed(format!(
                "Could not read the capture cursor setting: {error}"
            ));
            return;
        }
    };
    if let Err(error) = settings_linux::set_interval_preferences(preferences) {
        status.set_if_changed(format!("Could not save interval preferences: {error}"));
        return;
    }
    let (stop, receiver) = async_channel::bounded(1);
    *INTERVAL_CAPTURE.lock() = Some(stop);
    status.set_if_changed("Choose a window or monitor for interval capture…".to_owned());
    let task_scope = scope.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("jiffshot-interval-capture".to_owned())
        .spawn(move || {
            let progress_scope = task_scope.clone();
            let mut automatic_copy_failures = 0_u64;
            let mut first_automatic_copy_error = None;
            let result = futures_lite::future::block_on(interval_linux::capture(
                preferences,
                cursor,
                receiver,
                |progress| match progress {
                    interval_linux::Progress::Saved { count, path } => {
                        let automatic_copy = share_linux::copy_automatically(&path);
                        if let Err(error) = &automatic_copy {
                            automatic_copy_failures = automatic_copy_failures.saturating_add(1);
                            if first_automatic_copy_error.is_none() {
                                first_automatic_copy_error = Some(error.clone());
                            }
                        }
                        let recent = capture_linux::recent_captures();
                        if let Err(error) = progress_scope.post(move || {
                            let mut message = format!(
                                "Interval capture saved {count} image(s); latest: {}.",
                                path.display()
                            );
                            match recent {
                                Ok(recent) => replace_capture_library(captures, library, recent),
                                Err(error) => message.push_str(&format!(
                                    " Capture library could not refresh: {error}"
                                )),
                            }
                            append_automatic_copy_result(&mut message, automatic_copy);
                            status.set_if_changed(message);
                        }) {
                            eprintln!("Could not report interval capture progress: {error}");
                        }
                    }
                    interval_linux::Progress::Skipped { count } => {
                        if let Err(error) = progress_scope.post(move || {
                            status.set_if_changed(format!(
                                "Interval capture active; skipped {count} unchanged frame(s)."
                            ))
                        }) {
                            eprintln!("Could not report interval capture progress: {error}");
                        }
                    }
                },
            ));
            if let Err(error) = task_scope.post(move || {
                *INTERVAL_CAPTURE.lock() = None;
                let urgency = if result.is_err() || automatic_copy_failures > 0 {
                    notification_linux::Urgency::Critical
                } else {
                    notification_linux::Urgency::Normal
                };
                let mut message = match result {
                    Ok(completed) => {
                        let mut message = format!(
                            "Interval capture stopped: {} image(s) saved, {} unchanged frame(s) skipped.",
                            completed.saved, completed.skipped
                        );
                        if let Some(path) = completed.last_path {
                            message.push_str(&format!(" Latest: {}.", path.display()));
                        }
                        message
                    }
                    Err(error) => format!(
                        "Interval capture failed: {error}; any images already saved remain in the captures folder."
                    ),
                };
                if let Some(error) = first_automatic_copy_error {
                    message = format!(
                        "Automatic copy failed for {automatic_copy_failures} image(s): {error}; {message} The originals remain saved."
                    );
                }
                report_capture_result(status, "Interval capture", message, urgency);
            }) {
                *INTERVAL_CAPTURE.lock() = None;
                eprintln!("Could not report the interval capture result: {error}");
            }
        })
    {
        *INTERVAL_CAPTURE.lock() = None;
        status.set_if_changed(format!("Could not start interval capture: {error}"));
    }
}

fn configure_daily_schedule(
    scope: &AppScope,
    status: Signal<String>,
    captures: Signal<Vec<PathBuf>>,
    library: LibraryControls,
    preferences: settings_linux::SchedulePreferences,
) -> std::result::Result<(), String> {
    if let Some(previous) = DAILY_SCHEDULE.lock().take() {
        let _ = previous.stop.try_send(());
    }
    if !preferences.enabled {
        return Ok(());
    }

    let (stop, receiver) = mpsc::sync_channel(1);
    let id = NEXT_SCHEDULE_ID.fetch_add(1, Ordering::Relaxed);
    *DAILY_SCHEDULE.lock() = Some(ActiveSchedule { id, stop });
    let task_scope = scope.clone();
    let result = std::thread::Builder::new()
        .name("jiffshot-daily-schedule".to_owned())
        .spawn(move || {
            let mut schedule =
                schedule_linux::DailySchedule::new(preferences.time, preferences.last_fired);
            loop {
                match schedule_linux::LocalMinute::now() {
                    Ok(now) if schedule.should_fire(now) => {
                        let callback_scope = task_scope.clone();
                        if let Err(error) = task_scope.post(move || {
                            trigger_daily_capture(
                                &callback_scope,
                                status,
                                captures,
                                library,
                                id,
                                preferences.time,
                                now.day(),
                            );
                        }) {
                            eprintln!("Could not dispatch daily capture: {error}");
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        if let Err(post_error) = task_scope.post(move || {
                            if schedule_is_current(id) {
                                stop_daily_schedule(id);
                                report_capture_result(
                                    status,
                                    "Daily capture schedule stopped",
                                    format!("Daily capture schedule stopped: {error}"),
                                    notification_linux::Urgency::Critical,
                                );
                            }
                        }) {
                            eprintln!("Could not report daily schedule error: {post_error}");
                        }
                        break;
                    }
                }
                match receiver.recv_timeout(Duration::from_secs(1)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        });
    if let Err(error) = result {
        stop_daily_schedule(id);
        return Err(format!("could not start daily schedule worker: {error}"));
    }
    Ok(())
}

fn schedule_is_current(id: u64) -> bool {
    DAILY_SCHEDULE
        .lock()
        .as_ref()
        .is_some_and(|schedule| schedule.id == id)
}

fn stop_daily_schedule(id: u64) {
    let mut active = DAILY_SCHEDULE.lock();
    if active.as_ref().is_some_and(|schedule| schedule.id == id) {
        if let Some(schedule) = active.take() {
            let _ = schedule.stop.try_send(());
        }
    }
}

fn trigger_daily_capture(
    scope: &AppScope,
    status: Signal<String>,
    captures: Signal<Vec<PathBuf>>,
    library: LibraryControls,
    id: u64,
    time: schedule_linux::ScheduleTime,
    day: schedule_linux::ScheduleDay,
) {
    if !schedule_is_current(id) {
        return;
    }
    match settings_linux::mark_schedule_fired(time, day) {
        Ok(settings_linux::ScheduleClaim::Claimed) => {}
        Ok(settings_linux::ScheduleClaim::AlreadyClaimed) => return,
        Ok(settings_linux::ScheduleClaim::Changed) => {
            stop_daily_schedule(id);
            report_capture_result(
                status,
                "Daily capture stopped",
                "Daily capture settings changed externally; save the schedule again to reload them."
                    .to_owned(),
                notification_linux::Urgency::Critical,
            );
            return;
        }
        Err(error) => {
            report_capture_result(
                status,
                "Daily capture failed",
                format!(
                    "Could not record the daily capture attempt; capture was not started: {error}"
                ),
                notification_linux::Urgency::Critical,
            );
            return;
        }
    }
    if RECORDING.lock().is_some()
        || SCROLL_CAPTURE.lock().is_some()
        || INTERVAL_CAPTURE.lock().is_some()
        || SINGLE_CAPTURES_ACTIVE.load(Ordering::Acquire) != 0
        || SCHEDULED_CAPTURE_ACTIVE.load(Ordering::Acquire)
    {
        report_capture_result(
            status,
            "Daily capture skipped",
            "Daily capture skipped because another capture or recording was active.".to_owned(),
            notification_linux::Urgency::Normal,
        );
        return;
    }
    let cursor = match settings_linux::capture_cursor() {
        Ok(cursor) => cursor,
        Err(error) => {
            report_capture_result(
                status,
                "Daily capture failed",
                format!("Daily capture could not read the capture cursor setting: {error}"),
                notification_linux::Urgency::Critical,
            );
            return;
        }
    };
    if SCHEDULED_CAPTURE_ACTIVE.swap(true, Ordering::AcqRel) {
        report_capture_result(
            status,
            "Daily capture skipped",
            "A daily capture is already in progress.".to_owned(),
            notification_linux::Urgency::Normal,
        );
        return;
    }
    let active = ScheduledCaptureGuard;
    status.set_if_changed(
        "Daily capture: choose a screen or window in the desktop dialog…".to_owned(),
    );
    let task_scope = scope.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("jiffshot-daily-capture".to_owned())
        .spawn(move || {
            let result = futures_lite::future::block_on(capture_linux::capture_monitor(cursor))
                .and_then(|document| capture_linux::save_capture(&document));
            let automatic_copy = result
                .as_ref()
                .ok()
                .map(|path| share_linux::copy_automatically(path));
            let recent = result
                .as_ref()
                .ok()
                .map(|_| capture_linux::recent_captures());
            drop(active);
            if let Err(error) = task_scope.post(move || match result {
                Ok(path) => {
                    let mut message = format!("Daily capture saved to {}.", path.display());
                    let mut urgency = notification_linux::Urgency::Normal;
                    if let Some(recent) = recent {
                        match recent {
                            Ok(recent) => replace_capture_library(captures, library, recent),
                            Err(error) => {
                                message.push_str(&format!(
                                    " Capture library could not refresh: {error}"
                                ));
                                urgency = notification_linux::Urgency::Critical;
                            }
                        }
                        if let Some(automatic_copy) = automatic_copy {
                            if automatic_copy.is_err() {
                                urgency = notification_linux::Urgency::Critical;
                            }
                            append_automatic_copy_result(&mut message, automatic_copy);
                        }
                    }
                    report_capture_result(status, "Daily capture completed", message, urgency);
                }
                Err(error) => report_capture_result(
                    status,
                    "Daily capture failed",
                    format!("Daily capture failed: {error}"),
                    notification_linux::Urgency::Critical,
                ),
            }) {
                eprintln!("Could not report daily capture result: {error}");
            }
        })
    {
        report_capture_result(
            status,
            "Daily capture failed",
            format!("Could not start daily capture: {error}"),
            notification_linux::Urgency::Critical,
        );
    }
}

fn cancel_scrolling_capture(status: Signal<String>) {
    match SCROLL_CAPTURE.lock().as_ref() {
        Some(active) if active.stop.try_send(scroll_linux::Action::Cancel).is_ok() => {
            status.set_if_changed(
                match active.mode {
                    scroll_linux::Mode::Scrolling(_) => "Cancelling scrolling capture…",
                    scroll_linux::Mode::Panoramic => "Cancelling panoramic capture…",
                }
                .to_owned(),
            );
        }
        Some(_) => status.set_if_changed("Scrolling capture is already stopping.".to_owned()),
        None => status
            .set_if_changed("There is no scrolling or panoramic capture to cancel.".to_owned()),
    }
}

fn open_capture_folder(scope: &AppScope, status: Signal<String>) {
    let task_scope = scope.clone();
    let failed_status = status;
    if let Err(error) = scope.spawn(async move {
        let result = async {
            let directory = capture_linux::capture_directory()?;
            std::fs::create_dir_all(&directory)
                .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
            let file = std::fs::File::open(&directory)
                .map_err(|error| format!("could not open {}: {error}", directory.display()))?;
            let request = ashpd::desktop::open_uri::OpenDirectoryRequest::default()
                .send(&file.as_fd())
                .await
                .map_err(|error| format!("could not open captures folder: {error}"))?;
            request
                .response()
                .map_err(|error| format!("could not open captures folder: {error}"))?;
            Ok::<_, String>(directory)
        }
        .await;
        if let Err(error) = task_scope.post(move || {
            status.set_if_changed(match result {
                Ok(directory) => format!("Opened captures folder: {}", directory.display()),
                Err(error) => error,
            });
        }) {
            eprintln!("Could not report captures folder status: {error}");
        }
    }) {
        failed_status.set_if_changed(format!("Could not open captures folder: {error}"));
    }
}

fn choose_capture_folder(
    scope: &AppScope,
    captures: Signal<Vec<PathBuf>>,
    library: LibraryControls,
    status: Signal<String>,
) {
    let Some(window) = scope.main_window() else {
        status.set_if_changed("The editor window is not available.".to_owned());
        return;
    };
    let options = FolderPickerOptions::new().title("Choose Jiffshot captures folder");
    let operation = match file_dialog_linux::open_folder_picker(scope, &window, &options) {
        Ok(operation) => operation,
        Err(error) => {
            status.set_if_changed(format!("Could not open folder picker: {error}"));
            return;
        }
    };
    status.set_if_changed("Choose a folder for saved captures…".to_owned());
    let task_scope = scope.clone();
    let failed_status = status;
    if let Err(error) = scope.spawn(async move {
        let result = match operation.await {
            Ok(PickerOutcome::Selected(items)) => items
                .first()
                .and_then(|item| item.local_path().map(Path::to_path_buf))
                .ok_or_else(|| {
                    "the selected folder is not available as a local directory".to_owned()
                })
                .and_then(|directory| {
                    settings_linux::set_capture_directory(&directory)?;
                    Ok((directory, capture_linux::recent_captures()))
                }),
            Ok(PickerOutcome::Cancelled) => return,
            Err(error) => Err(format!("could not choose captures folder: {error}")),
        };
        if let Err(error) = task_scope.post(move || match result {
            Ok((directory, recent)) => {
                LIBRARY_OPEN_REQUEST.fetch_add(1, Ordering::AcqRel);
                library.clear_selection();
                match recent {
                    Ok(recent) => {
                        replace_capture_library(captures, library, recent);
                        status.set_if_changed(format!(
                            "Capture folder set to {}",
                            directory.display()
                        ));
                    }
                    Err(error) => {
                        replace_capture_library(captures, library, Vec::new());
                        status.set_if_changed(format!(
                            "Capture folder set to {}, but the library could not refresh: {error}",
                            directory.display()
                        ));
                    }
                }
            }
            Err(error) => status.set_if_changed(error),
        }) {
            eprintln!("Could not report capture folder preference: {error}");
        }
    }) {
        failed_status.set_if_changed(format!("Could not start folder picker: {error}"));
    }
}

fn copy_image(scope: &AppScope, status: Signal<String>) {
    let Some(pixmap) = canvas::with_editor(|editor| editor.document().flatten()) else {
        status.set_if_changed("There is no image to copy.".to_owned());
        return;
    };
    let task_scope = scope.clone();
    let failed_status = status;
    if let Err(error) = scope.spawn(async move {
        let result = clipboard_linux::copy_pixmap(pixmap);
        if let Err(error) = task_scope.post(move || {
            status.set_if_changed(match result {
                Ok(()) => "Copied image to the clipboard.".to_owned(),
                Err(error) => error,
            });
        }) {
            eprintln!("Could not report clipboard status: {error}");
        }
    }) {
        failed_status.set_if_changed(format!("Could not start clipboard copy: {error}"));
    }
}

fn paste_image(
    scope: &AppScope,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
) {
    let task_scope = scope.clone();
    let failed_status = status;
    if let Err(error) = scope.spawn(async move {
        let result = clipboard_linux::paste_document();
        if let Err(error) = task_scope.post(move || match result {
            Ok(document) => {
                canvas::with_editor(|editor| load_editor_document(editor, document, None));
                canvas.get().refresh();
                status.set_if_changed("Pasted image from the clipboard.".to_owned());
            }
            Err(error) => status.set_if_changed(error),
        }) {
            eprintln!("Could not update editor from clipboard: {error}");
        }
    }) {
        failed_status.set_if_changed(format!("Could not start clipboard paste: {error}"));
    }
}

fn toggle_screen_recording(
    scope: &AppScope,
    status: Signal<String>,
    captures: Signal<Vec<PathBuf>>,
    library: LibraryControls,
    audio: capture_linux::RecordingAudio,
    webcam: bool,
) {
    if let Some(commands) = RECORDING.lock().as_ref() {
        match commands.try_send(capture_linux::RecordingCommand::Stop) {
            Ok(()) => status.set_if_changed("Finalizing screen recording…".to_owned()),
            Err(async_channel::TrySendError::Full(_)) => {
                status.set_if_changed("Recording controls are busy; try again.".to_owned())
            }
            Err(async_channel::TrySendError::Closed(_)) => {
                status.set_if_changed("Screen recording is no longer active.".to_owned())
            }
        }
        return;
    }
    if SCROLL_CAPTURE.lock().is_some() {
        status.set_if_changed(
            "Finish scrolling capture or panoramic capture before starting screen recording."
                .to_owned(),
        );
        return;
    }
    if INTERVAL_CAPTURE.lock().is_some() {
        status.set_if_changed("Stop interval capture before starting screen recording.".to_owned());
        return;
    }
    if SCHEDULED_CAPTURE_ACTIVE.load(Ordering::Acquire) {
        status.set_if_changed(
            "Finish the daily capture before starting screen recording.".to_owned(),
        );
        return;
    }

    let cursor = match settings_linux::recording_cursor() {
        Ok(cursor) => cursor,
        Err(error) => {
            status.set_if_changed(format!(
                "Could not read the recording cursor setting: {error}"
            ));
            return;
        }
    };
    let frame_rate = match settings_linux::recording_frame_rate() {
        Ok(frame_rate) => frame_rate,
        Err(error) => {
            status.set_if_changed(format!("Could not read the recording frame rate: {error}"));
            return;
        }
    };
    let webcam = if webcam {
        match settings_linux::webcam_configuration() {
            Ok(configuration) => capture_linux::RecordingWebcam::Enabled(configuration),
            Err(error) => {
                status.set_if_changed(format!("Could not load webcam preferences: {error}"));
                return;
            }
        }
    } else {
        capture_linux::RecordingWebcam::Disabled
    };
    let (commands, receiver) = async_channel::bounded(8);
    *RECORDING.lock() = Some(commands);
    let description = match audio {
        capture_linux::RecordingAudio::Disabled => "Screen recording",
        capture_linux::RecordingAudio::Microphone => "Screen and microphone recording",
        capture_linux::RecordingAudio::System => "Screen and system sound recording",
        capture_linux::RecordingAudio::SystemAndMicrophone => {
            "Screen, system sound and microphone recording"
        }
    };
    status.set_if_changed(format!(
        "Requesting permission for {}…",
        description.to_ascii_lowercase()
    ));
    let task_scope = scope.clone();
    let task_status = status;
    if let Err(error) = std::thread::Builder::new()
        .name("jiffshot-screen-recording".to_owned())
        .spawn(move || {
            let state_scope = task_scope.clone();
            let result = futures_lite::future::block_on(capture_linux::record_monitor_with_audio(
                receiver,
                audio,
                webcam,
                cursor,
                frame_rate,
                move |state| {
                    let message = match state {
                        capture_linux::RecordingState::Starting => {
                            "Screen recording is still starting; try again."
                        }
                        capture_linux::RecordingState::Recording => "Screen recording is active.",
                        capture_linux::RecordingState::Paused => "Screen recording is paused.",
                        capture_linux::RecordingState::Stopping => "Finalizing screen recording…",
                        capture_linux::RecordingState::Restarting => "Restarting screen recording…",
                        capture_linux::RecordingState::Cancelling => "Cancelling screen recording…",
                    };
                    if let Err(error) =
                        state_scope.post(move || task_status.set_if_changed(message.to_owned()))
                    {
                        eprintln!("Could not report recording state: {error}");
                    }
                },
            ));
            let automatic_copy = result
                .as_ref()
                .ok()
                .and_then(Option::as_ref)
                .map(|path| share_linux::copy_automatically(path));
            let recent = capture_linux::recent_captures();
            let posted_status = task_status;
            if let Err(post_error) = task_scope.post(move || {
                *RECORDING.lock() = None;
                let library_error = match recent {
                    Ok(recent) => {
                        replace_capture_library(captures, library, recent);
                        None
                    }
                    Err(error) => Some(error),
                };
                let urgency = if result.is_err()
                    || library_error.is_some()
                    || automatic_copy
                        .as_ref()
                        .is_some_and(std::result::Result::is_err)
                {
                    notification_linux::Urgency::Critical
                } else {
                    notification_linux::Urgency::Normal
                };
                let mut message = match result {
                    Ok(Some(path)) => format!("{description} saved to {}", path.display()),
                    Ok(None) => format!("{description} cancelled."),
                    Err(error) => format!("{description} failed: {error}"),
                };
                if let Some(error) = library_error {
                    message.push_str(&format!("; capture library could not refresh: {error}"));
                }
                if let Some(result) = automatic_copy {
                    append_automatic_copy_result(&mut message, result);
                }
                report_capture_result(posted_status, "Screen recording", message, urgency);
            }) {
                *RECORDING.lock() = None;
                eprintln!("Could not report the screen recording result: {post_error}");
            }
        })
    {
        *RECORDING.lock() = None;
        report_capture_result(
            status,
            "Screen recording failed",
            format!("Could not start screen recording: {error}"),
            notification_linux::Urgency::Critical,
        );
    }
}

fn toggle_recording_pause(status: Signal<String>) {
    send_recording_command(
        capture_linux::RecordingCommand::TogglePause,
        "Changing recording pause state…",
        status,
    );
}

fn send_recording_command(
    command: capture_linux::RecordingCommand,
    sent_message: &str,
    status: Signal<String>,
) {
    let commands = RECORDING.lock().as_ref().cloned();
    match commands {
        Some(commands) => match commands.try_send(command) {
            Ok(()) => status.set_if_changed(sent_message.to_owned()),
            Err(async_channel::TrySendError::Full(_)) => {
                status.set_if_changed("Recording controls are busy; try again.".to_owned())
            }
            Err(async_channel::TrySendError::Closed(_)) => {
                status.set_if_changed("Screen recording is no longer active.".to_owned())
            }
        },
        None => status.set_if_changed("Screen recording is not active.".to_owned()),
    }
}

fn toggle_video_playback(
    scope: &AppScope,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
) {
    if let Some(playback) = VIDEO_PLAYBACK.lock().as_ref() {
        playback.stop.store(true, Ordering::Release);
        status.set_if_changed("Stopping video playback…".to_owned());
        return;
    }

    let Some(window) = scope.main_window() else {
        status.set_if_changed("The editor window is not available.".to_owned());
        return;
    };
    let filter = ["avi", "mkv", "mov", "mp4", "webm"]
        .into_iter()
        .fold(FileTypeFilter::new("Video files"), |filter, extension| {
            filter.with_extension(extension)
        });
    let options = OpenFilePickerOptions::new()
        .title("Play video")
        .file_type(filter);
    let operation = match file_dialog_linux::open_file_picker(scope, &window, &options) {
        Ok(operation) => operation,
        Err(error) => {
            status.set_if_changed(format!("Could not open video picker: {error}"));
            return;
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(false));
    *VIDEO_PLAYBACK.lock() = Some(VideoPlayback {
        stop: stop.clone(),
        paused: paused.clone(),
    });
    status.set_if_changed("Select a video to play…".to_owned());
    let task_scope = scope.clone();
    let failed_status = status;
    let task_stop = stop.clone();
    let task_paused = paused.clone();
    if let Err(error) = scope.spawn(async move {
        match operation.await {
            Ok(PickerOutcome::Selected(items)) => {
                let Some(path) = items
                    .first()
                    .and_then(|item| item.local_path().map(Path::to_path_buf))
                else {
                    clear_video_playback(&task_stop);
                    let reported_status = status;
                    if let Err(error) = task_scope.post(move || {
                        reported_status.set_if_changed(
                            "The selected video is not available as a local file.".to_owned(),
                        )
                    }) {
                        eprintln!("Could not report video file access error: {error}");
                    }
                    return;
                };
                if task_stop.load(Ordering::Acquire) {
                    clear_video_playback(&task_stop);
                    return;
                }
                let playback_scope = task_scope.clone();
                let callback_scope = task_scope.clone();
                let callback_canvas = canvas.clone();
                let callback_stop = task_stop.clone();
                let callback_pending = Arc::new(AtomicBool::new(false));
                let pending_for_thread = callback_pending.clone();
                let thread_stop = task_stop.clone();
                let thread_paused = task_paused.clone();
                let thread_status = status;
                let path_label = path.display().to_string();
                let failed_scope = task_scope.clone();
                let failed_status = status;
                if let Err(error) = task_scope
                    .post(move || status.set_if_changed(format!("Starting playback: {path_label}")))
                {
                    clear_video_playback(&task_stop);
                    eprintln!("Could not report video playback status: {error}");
                    return;
                }
                let spawn = std::thread::Builder::new()
                    .name("jiffshot-video-playback".to_owned())
                    .spawn(move || {
                        let result =
                            video_linux::play(&path, &thread_stop, &thread_paused, |document| {
                                if pending_for_thread.swap(true, Ordering::AcqRel) {
                                    return;
                                }
                                let canvas = callback_canvas.clone();
                                let pending = pending_for_thread.clone();
                                let source = path.clone();
                                if let Err(error) = callback_scope.post(move || {
                                    canvas::with_editor(|editor| {
                                        load_editor_document(editor, document, Some(source));
                                    });
                                    canvas.get().refresh();
                                    pending.store(false, Ordering::Release);
                                }) {
                                    eprintln!("Could not display a video frame: {error}");
                                    pending_for_thread.store(false, Ordering::Release);
                                    callback_stop.store(true, Ordering::Release);
                                }
                            });
                        clear_video_playback(&thread_stop);
                        let message = match result {
                            Ok(()) => "Video playback stopped.".to_owned(),
                            Err(error) => format!("Video playback failed: {error}"),
                        };
                        if let Err(error) =
                            playback_scope.post(move || thread_status.set_if_changed(message))
                        {
                            eprintln!("Could not report video playback status: {error}");
                        }
                    });
                if let Err(error) = spawn {
                    clear_video_playback(&task_stop);
                    if let Err(post_error) = failed_scope.post(move || {
                        failed_status
                            .set_if_changed(format!("Could not start video playback: {error}"))
                    }) {
                        eprintln!("Could not report video playback error: {post_error}");
                    }
                }
            }
            Ok(PickerOutcome::Cancelled) => clear_video_playback(&task_stop),
            Err(error) => {
                clear_video_playback(&task_stop);
                let reported_status = status;
                if let Err(post_error) = task_scope.post(move || {
                    reported_status.set_if_changed(format!("Could not open video: {error}"))
                }) {
                    eprintln!("Could not report video picker error: {post_error}");
                }
            }
        }
    }) {
        clear_video_playback(&stop);
        failed_status.set_if_changed(format!("Could not start video picker: {error}"));
    }
}

fn toggle_video_pause(status: Signal<String>) {
    let playback = VIDEO_PLAYBACK.lock();
    if let Some(playback) = playback.as_ref() {
        let paused = !playback.paused.load(Ordering::Acquire);
        playback.paused.store(paused, Ordering::Release);
        status.set_if_changed(if paused {
            "Pausing video…".to_owned()
        } else {
            "Resuming video…".to_owned()
        });
    } else {
        status.set_if_changed("No video is playing.".to_owned());
    }
}

fn combine_videos_dialog(scope: &AppScope, status: Signal<String>) {
    let Some(window) = scope.main_window() else {
        status.set_if_changed("The editor window is not available.".to_owned());
        return;
    };
    let filter = ["avi", "mkv", "mov", "mp4", "webm"]
        .into_iter()
        .fold(FileTypeFilter::new("Video files"), |filter, extension| {
            filter.with_extension(extension)
        });
    let options = OpenFilePickerOptions::new()
        .title("Select videos to combine in filename order")
        .allow_multiple(true)
        .file_type(filter);
    let operation = match file_dialog_linux::open_file_picker(scope, &window, &options) {
        Ok(operation) => operation,
        Err(error) => {
            status.set_if_changed(format!("Could not open video picker: {error}"));
            return;
        }
    };
    status.set_if_changed(
        "Select at least two videos; clips are joined in filename order.".to_owned(),
    );
    let task_scope = scope.clone();
    let failed_status = status;
    if let Err(error) = scope.spawn(async move {
        match operation.await {
            Ok(PickerOutcome::Selected(items)) => {
                let paths = items
                    .iter()
                    .map(|item| item.local_path().map(Path::to_path_buf))
                    .collect::<Option<Vec<_>>>();
                let Some(mut paths) = paths else {
                    if let Err(error) = task_scope.post(move || {
                        status
                            .set_if_changed("Every selected video must be a local file.".to_owned())
                    }) {
                        eprintln!("Could not report video file access error: {error}");
                    }
                    return;
                };
                if paths.len() < 2 {
                    if let Err(error) = task_scope.post(move || {
                        status.set_if_changed("Select at least two videos to combine.".to_owned())
                    }) {
                        eprintln!("Could not report video selection error: {error}");
                    }
                    return;
                }
                order_video_sources(&mut paths);
                let save_scope = task_scope.clone();
                if let Err(error) =
                    task_scope.post(move || save_combined_video_dialog(&save_scope, paths, status))
                {
                    eprintln!("Could not open the combined video save dialog: {error}");
                }
            }
            Ok(PickerOutcome::Cancelled) => {}
            Err(error) => {
                if let Err(post_error) = task_scope.post(move || {
                    status.set_if_changed(format!("Could not select videos: {error}"))
                }) {
                    eprintln!("Could not report video picker error: {post_error}");
                }
            }
        }
    }) {
        failed_status.set_if_changed(format!("Could not start video picker: {error}"));
    }
}

fn order_video_sources(paths: &mut [PathBuf]) {
    paths.sort_by(|left, right| {
        left.file_name()
            .cmp(&right.file_name())
            .then_with(|| left.cmp(right))
    });
}

fn save_combined_video_dialog(scope: &AppScope, sources: Vec<PathBuf>, status: Signal<String>) {
    let Some(window) = scope.main_window() else {
        status.set_if_changed("The editor window is not available.".to_owned());
        return;
    };
    let options = SaveFilePickerOptions::new()
        .title("Save combined video")
        .suggested_file_name("Combined.mp4")
        .start_in(WellKnownFolder::Pictures)
        .default_extension("mp4")
        .file_type(FileTypeFilter::new("MP4 video").with_extension("mp4"));
    let operation = match file_dialog_linux::save_file_picker(scope, &window, &options) {
        Ok(operation) => operation,
        Err(error) => {
            status.set_if_changed(format!("Could not open video save dialog: {error}"));
            return;
        }
    };
    status.set_if_changed("Choose where to save the combined video…".to_owned());
    let task_scope = scope.clone();
    let failed_status = status;
    if let Err(error) = scope.spawn(async move {
        match operation.await {
            Ok(PickerOutcome::Selected(items)) => {
                let Some(destination) = items
                    .first()
                    .and_then(|item| item.local_path().map(Path::to_path_buf))
                else {
                    if let Err(error) = task_scope.post(move || {
                        status.set_if_changed(
                            "The selected destination is not available as a local file.".to_owned(),
                        )
                    }) {
                        eprintln!("Could not report video destination error: {error}");
                    }
                    return;
                };
                let destination_label = destination.display().to_string();
                let thread_scope = task_scope.clone();
                if let Err(error) = task_scope.post(move || {
                    status.set_if_changed("Combining videos in filename order…".to_owned())
                }) {
                    eprintln!("Could not report video combining progress: {error}");
                }
                let result = std::thread::Builder::new()
                    .name("jiffshot-video-combine".to_owned())
                    .spawn(move || {
                        let result = video_linux::combine_videos(&sources, &destination);
                        if let Err(error) = thread_scope.post(move || match result {
                            Ok(()) => status.set_if_changed(format!(
                                "Saved combined video to {destination_label}"
                            )),
                            Err(error) => {
                                status.set_if_changed(format!("Could not combine videos: {error}"))
                            }
                        }) {
                            eprintln!("Could not report video combining result: {error}");
                        }
                    });
                if let Err(error) = result {
                    if let Err(post_error) = task_scope.post(move || {
                        status.set_if_changed(format!("Could not start video combining: {error}"))
                    }) {
                        eprintln!("Could not report video combining startup error: {post_error}");
                    }
                }
            }
            Ok(PickerOutcome::Cancelled) => {}
            Err(error) => {
                if let Err(post_error) = task_scope.post(move || {
                    status.set_if_changed(format!("Could not select video destination: {error}"))
                }) {
                    eprintln!("Could not report video save dialog error: {post_error}");
                }
            }
        }
    }) {
        failed_status.set_if_changed(format!("Could not start video save dialog: {error}"));
    }
}

fn video_range_dialog(
    scope: &AppScope,
    start: String,
    end: String,
    cut: bool,
    status: Signal<String>,
) {
    let start = match start.parse::<f64>() {
        Ok(value) if value.is_finite() && value >= 0.0 => value,
        _ => {
            status.set_if_changed(
                "Video range start must be a non-negative number of seconds.".to_owned(),
            );
            return;
        }
    };
    let end = match end.parse::<f64>() {
        Ok(value) if value.is_finite() && value > start => value,
        _ => {
            status
                .set_if_changed("Video range end must be greater than the start time.".to_owned());
            return;
        }
    };
    let Some(window) = scope.main_window() else {
        status.set_if_changed("The editor window is not available.".to_owned());
        return;
    };
    let filter = ["avi", "mkv", "mov", "mp4", "webm"]
        .into_iter()
        .fold(FileTypeFilter::new("Video files"), |filter, extension| {
            filter.with_extension(extension)
        });
    let options = OpenFilePickerOptions::new()
        .title(if cut {
            "Select video to cut"
        } else {
            "Select video to trim"
        })
        .file_type(filter);
    let operation = match file_dialog_linux::open_file_picker(scope, &window, &options) {
        Ok(operation) => operation,
        Err(error) => {
            status.set_if_changed(format!("Could not open video picker: {error}"));
            return;
        }
    };
    status.set_if_changed(if cut {
        "Select a video to cut…".to_owned()
    } else {
        "Select a video to trim…".to_owned()
    });
    let task_scope = scope.clone();
    let failed_status = status;
    if let Err(error) = scope.spawn(async move {
        match operation.await {
            Ok(PickerOutcome::Selected(items)) => {
                let Some(path) = items
                    .first()
                    .and_then(|item| item.local_path().map(Path::to_path_buf))
                else {
                    let reported_status = status;
                    if let Err(error) = task_scope.post(move || {
                        reported_status.set_if_changed(
                            "The selected video is not available as a local file.".to_owned(),
                        )
                    }) {
                        eprintln!("Could not report video file access error: {error}");
                    }
                    return;
                };
                let save_scope = task_scope.clone();
                if let Err(error) = task_scope
                    .post(move || save_video_range(&save_scope, path, start, end, cut, status))
                {
                    eprintln!("Could not open the video save dialog: {error}");
                }
            }
            Ok(PickerOutcome::Cancelled) => {}
            Err(error) => {
                let reported_status = status;
                if let Err(post_error) = task_scope.post(move || {
                    reported_status.set_if_changed(format!("Could not select video: {error}"))
                }) {
                    eprintln!("Could not report video picker error: {post_error}");
                }
            }
        }
    }) {
        failed_status.set_if_changed(format!("Could not start video picker: {error}"));
    }
}

fn import_video_frame_dialog(
    scope: &AppScope,
    timestamp: String,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
) {
    let timestamp = match timestamp.parse::<f64>() {
        Ok(value) if value.is_finite() && value >= 0.0 => value,
        _ => {
            status
                .set_if_changed("Frame time must be a non-negative number of seconds.".to_owned());
            return;
        }
    };
    let Some(window) = scope.main_window() else {
        status.set_if_changed("The editor window is not available.".to_owned());
        return;
    };
    let filter = ["avi", "mkv", "mov", "mp4", "webm"]
        .into_iter()
        .fold(FileTypeFilter::new("Video files"), |filter, extension| {
            filter.with_extension(extension)
        });
    let options = OpenFilePickerOptions::new()
        .title("Select video to import a frame from")
        .file_type(filter);
    let operation = match file_dialog_linux::open_file_picker(scope, &window, &options) {
        Ok(operation) => operation,
        Err(error) => {
            status.set_if_changed(format!("Could not open video picker: {error}"));
            return;
        }
    };
    let Some(pending) = begin_document_load(status) else {
        return;
    };
    status.set_if_changed(format!("Select a video for frame {timestamp} seconds…"));
    let task_scope = scope.clone();
    let failed_status = status;
    if let Err(error) = scope.spawn(async move {
        match operation.await {
            Ok(PickerOutcome::Selected(items)) => {
                let Some(path) = items
                    .first()
                    .and_then(|item| item.local_path().map(Path::to_path_buf))
                else {
                    status.set_if_changed(
                        "The selected video is not available as a local file.".to_owned(),
                    );
                    return;
                };
                start_document_load(
                    &task_scope,
                    path,
                    DocumentLoadKind::VideoFrame(timestamp),
                    pending,
                    canvas,
                    status,
                    None,
                );
            }
            Ok(PickerOutcome::Cancelled) => {}
            Err(error) => {
                status.set_if_changed(format!("Could not select video: {error}"));
            }
        }
    }) {
        failed_status.set_if_changed(format!("Could not start video frame extraction: {error}"));
    }
}

fn save_video_range(
    scope: &AppScope,
    source: PathBuf,
    start: f64,
    end: f64,
    cut: bool,
    status: Signal<String>,
) {
    let Some(window) = scope.main_window() else {
        status.set_if_changed("The editor window is not available.".to_owned());
        return;
    };
    let options = SaveFilePickerOptions::new()
        .title(if cut {
            "Save video with range removed"
        } else {
            "Save trimmed video"
        })
        .suggested_file_name(if cut { "Cut.mp4" } else { "Trimmed.mp4" })
        .start_in(WellKnownFolder::Pictures)
        .default_extension("mp4")
        .file_type(FileTypeFilter::new("MP4 video").with_extension("mp4"));
    let operation = match file_dialog_linux::save_file_picker(scope, &window, &options) {
        Ok(operation) => operation,
        Err(error) => {
            status.set_if_changed(format!("Could not open video save dialog: {error}"));
            return;
        }
    };
    status.set_if_changed(if cut {
        "Choose where to save the cut video…".to_owned()
    } else {
        "Choose where to save the trimmed video…".to_owned()
    });
    let task_scope = scope.clone();
    let failed_status = status;
    if let Err(error) = scope.spawn(async move {
        match operation.await {
            Ok(PickerOutcome::Selected(items)) => {
                let Some(destination) = items
                    .first()
                    .and_then(|item| item.local_path().map(Path::to_path_buf))
                else {
                    let reported_status = status;
                    if let Err(error) = task_scope.post(move || {
                        reported_status.set_if_changed(
                            "The selected destination is not available as a local file.".to_owned(),
                        )
                    }) {
                        eprintln!("Could not report video destination error: {error}");
                    }
                    return;
                };
                let source_label = source.display().to_string();
                let destination_label = destination.display().to_string();
                let thread_scope = task_scope.clone();
                let result = std::thread::Builder::new()
                    .name(if cut {
                        "jiffshot-video-cut".to_owned()
                    } else {
                        "jiffshot-video-trim".to_owned()
                    })
                    .spawn(move || {
                        let result = if cut {
                            video_linux::cut_video(&source, &destination, start, end)
                        } else {
                            video_linux::trim_video(&source, &destination, start, end)
                        };
                        let reported_status = status;
                        if let Err(error) = thread_scope.post(move || match result {
                            Ok(()) => reported_status.set_if_changed(format!(
                                "Saved {} video to {destination_label}",
                                if cut { "cut" } else { "trimmed" }
                            )),
                            Err(error) => reported_status.set_if_changed(format!(
                                "Could not {} {}: {error}",
                                if cut { "cut" } else { "trim" },
                                source_label
                            )),
                        }) {
                            eprintln!("Could not report video trim result: {error}");
                        }
                    });
                if let Err(error) = result {
                    let reported_status = status;
                    if let Err(post_error) = task_scope.post(move || {
                        reported_status
                            .set_if_changed(format!("Could not start video trimming: {error}"))
                    }) {
                        eprintln!("Could not report video trim startup error: {post_error}");
                    }
                }
            }
            Ok(PickerOutcome::Cancelled) => {}
            Err(error) => {
                let reported_status = status;
                if let Err(post_error) = task_scope.post(move || {
                    reported_status
                        .set_if_changed(format!("Could not select video destination: {error}"))
                }) {
                    eprintln!("Could not report video save dialog error: {post_error}");
                }
            }
        }
    }) {
        failed_status.set_if_changed(format!("Could not start video save dialog: {error}"));
    }
}

#[derive(Clone, Copy)]
enum VideoExport {
    Gif,
    SilentMp4,
}

impl VideoExport {
    fn description(self) -> &'static str {
        match self {
            Self::Gif => "animated GIF",
            Self::SilentMp4 => "silent MP4",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Gif => "gif",
            Self::SilentMp4 => "mp4",
        }
    }

    fn suggested_file_name(self) -> &'static str {
        match self {
            Self::Gif => "Animation.gif",
            Self::SilentMp4 => "Silent video.mp4",
        }
    }

    fn thread_name(self) -> &'static str {
        match self {
            Self::Gif => "jiffshot-video-gif",
            Self::SilentMp4 => "jiffshot-video-silent",
        }
    }

    fn export(self, source: &Path, destination: &Path) -> std::result::Result<(), String> {
        match self {
            Self::Gif => video_linux::export_gif(source, destination),
            Self::SilentMp4 => video_linux::export_silent_video(source, destination),
        }
    }
}

fn export_video_dialog(scope: &AppScope, format: VideoExport, status: Signal<String>) {
    let Some(window) = scope.main_window() else {
        status.set_if_changed("The editor window is not available.".to_owned());
        return;
    };
    let filter = ["avi", "mkv", "mov", "mp4", "webm"]
        .into_iter()
        .fold(FileTypeFilter::new("Video files"), |filter, extension| {
            filter.with_extension(extension)
        });
    let options = OpenFilePickerOptions::new()
        .title(format!(
            "Select video to export as {}",
            format.description()
        ))
        .file_type(filter);
    let operation = match file_dialog_linux::open_file_picker(scope, &window, &options) {
        Ok(operation) => operation,
        Err(error) => {
            status.set_if_changed(format!("Could not open video picker: {error}"));
            return;
        }
    };
    status.set_if_changed(format!(
        "Select a video to export as {}…",
        format.description()
    ));
    let task_scope = scope.clone();
    let failed_status = status;
    if let Err(error) = scope.spawn(async move {
        match operation.await {
            Ok(PickerOutcome::Selected(items)) => {
                let Some(source) = items
                    .first()
                    .and_then(|item| item.local_path().map(Path::to_path_buf))
                else {
                    let reported_status = status;
                    if let Err(error) = task_scope.post(move || {
                        reported_status.set_if_changed(
                            "The selected video is not available as a local file.".to_owned(),
                        )
                    }) {
                        eprintln!("Could not report video file access error: {error}");
                    }
                    return;
                };
                let save_scope = task_scope.clone();
                if let Err(error) = task_scope
                    .post(move || save_video_export_dialog(&save_scope, source, format, status))
                {
                    eprintln!("Could not open the video export save dialog: {error}");
                }
            }
            Ok(PickerOutcome::Cancelled) => {}
            Err(error) => {
                let reported_status = status;
                if let Err(post_error) = task_scope.post(move || {
                    reported_status.set_if_changed(format!("Could not select video: {error}"))
                }) {
                    eprintln!("Could not report video picker error: {post_error}");
                }
            }
        }
    }) {
        failed_status.set_if_changed(format!("Could not start video picker: {error}"));
    }
}

fn save_video_export_dialog(
    scope: &AppScope,
    source: PathBuf,
    format: VideoExport,
    status: Signal<String>,
) {
    let Some(window) = scope.main_window() else {
        status.set_if_changed("The editor window is not available.".to_owned());
        return;
    };
    let options = SaveFilePickerOptions::new()
        .title(format!("Save {}", format.description()))
        .suggested_file_name(format.suggested_file_name())
        .start_in(WellKnownFolder::Pictures)
        .default_extension(format.extension())
        .file_type(FileTypeFilter::new(format.description()).with_extension(format.extension()));
    let operation = match file_dialog_linux::save_file_picker(scope, &window, &options) {
        Ok(operation) => operation,
        Err(error) => {
            status.set_if_changed(format!("Could not open video export save dialog: {error}"));
            return;
        }
    };
    status.set_if_changed(format!(
        "Choose where to save the {}…",
        format.description()
    ));
    let task_scope = scope.clone();
    let failed_status = status;
    if let Err(error) = scope.spawn(async move {
        match operation.await {
            Ok(PickerOutcome::Selected(items)) => {
                let Some(destination) = items
                    .first()
                    .and_then(|item| item.local_path().map(Path::to_path_buf))
                else {
                    let reported_status = status;
                    if let Err(error) = task_scope.post(move || {
                        reported_status.set_if_changed(format!(
                            "The selected {} destination is not available as a local file.",
                            format.description()
                        ))
                    }) {
                        eprintln!("Could not report video export destination error: {error}");
                    }
                    return;
                };
                let source_label = source.display().to_string();
                let destination_label = destination.display().to_string();
                let thread_scope = task_scope.clone();
                let result = std::thread::Builder::new()
                    .name(format.thread_name().to_owned())
                    .spawn(move || {
                        let result = format.export(&source, &destination);
                        let reported_status = status;
                        if let Err(error) = thread_scope.post(move || match result {
                            Ok(()) => reported_status.set_if_changed(format!(
                                "Saved {} to {destination_label}",
                                format.description()
                            )),
                            Err(error) => reported_status.set_if_changed(format!(
                                "Could not export {} from {}: {error}",
                                format.description(),
                                source_label,
                            )),
                        }) {
                            eprintln!("Could not report video export result: {error}");
                        }
                    });
                if let Err(error) = result {
                    let reported_status = status;
                    if let Err(post_error) = task_scope.post(move || {
                        reported_status
                            .set_if_changed(format!("Could not start video export: {error}"))
                    }) {
                        eprintln!("Could not report video export startup error: {post_error}");
                    }
                }
            }
            Ok(PickerOutcome::Cancelled) => {}
            Err(error) => {
                let reported_status = status;
                if let Err(post_error) = task_scope.post(move || {
                    reported_status.set_if_changed(format!(
                        "Could not select video export destination: {error}"
                    ))
                }) {
                    eprintln!("Could not report video export save dialog error: {post_error}");
                }
            }
        }
    }) {
        failed_status.set_if_changed(format!("Could not start video export save dialog: {error}"));
    }
}

fn clear_video_playback(stop: &Arc<AtomicBool>) {
    let mut playback = VIDEO_PLAYBACK.lock();
    if playback
        .as_ref()
        .is_some_and(|active| Arc::ptr_eq(&active.stop, stop))
    {
        playback.take();
    }
}

fn cycle_variant(
    canvas: &ui::UiOnly<ui::canvas_view::CanvasView>,
    status: &Signal<String>,
    forward: bool,
) {
    let updated = canvas::with_editor(|editor| {
        let tool = editor.tool();
        let variants = editor.variants_for(tool);
        if variants.is_empty() {
            return None;
        }
        let current = editor.variant(tool);
        let index = variants
            .iter()
            .position(|variant| variant == &current)
            .unwrap_or(0);
        let next = if forward {
            (index + 1) % variants.len()
        } else {
            (index + variants.len() - 1) % variants.len()
        };
        editor.set_variant(&variants[next]);
        Some(format!("{}: {}", tool.name(), variants[next]))
    })
    .flatten();
    if let Some(updated) = updated {
        canvas.get().refresh();
        status.set_if_changed(updated);
    } else {
        status.set_if_changed("The selected tool has no variants.".to_owned());
    }
}

fn history_action(
    canvas: &ui::UiOnly<ui::canvas_view::CanvasView>,
    status: &Signal<String>,
    undo: bool,
) {
    let changed = canvas::with_editor(|editor| if undo { editor.undo() } else { editor.redo() })
        .unwrap_or(false);
    if changed {
        canvas.get().refresh();
    }
    status.set_if_changed(if changed {
        if undo { "Undo" } else { "Redo" }.to_owned()
    } else {
        if undo {
            "Nothing to undo"
        } else {
            "Nothing to redo"
        }
        .to_owned()
    });
}

fn open_document(
    scope: &AppScope,
    canvas: ui::UiOnly<ui::canvas_view::CanvasView>,
    status: Signal<String>,
) {
    let Some(window) = scope.main_window() else {
        status.set_if_changed("The editor window is not available.".to_owned());
        return;
    };
    let filter = [
        "jsd", "png", "jpg", "jpeg", "bmp", "gif", "webp", "avi", "mkv", "mov", "mp4", "webm",
    ]
    .into_iter()
    .fold(
        FileTypeFilter::new("Jiffshot, image, and video files"),
        |filter, extension| filter.with_extension(extension),
    );
    let options = OpenFilePickerOptions::new()
        .title("Open image, video, or document")
        .file_type(filter);
    let operation = match file_dialog_linux::open_file_picker(scope, &window, &options) {
        Ok(operation) => operation,
        Err(error) => {
            status.set_if_changed(format!("Could not open file picker: {error}"));
            return;
        }
    };
    let Some(pending) = begin_document_load(status) else {
        return;
    };
    let task_scope = scope.clone();
    let failed_status = status;
    if let Err(error) = scope.spawn(async move {
        match operation.await {
            Ok(PickerOutcome::Selected(items)) => {
                let Some(path) = items
                    .first()
                    .and_then(|item| item.local_path().map(Path::to_path_buf))
                else {
                    status.set_if_changed(
                        "The selected file is not available as a local file.".to_owned(),
                    );
                    return;
                };
                start_document_load(
                    &task_scope,
                    path,
                    DocumentLoadKind::File,
                    pending,
                    canvas,
                    status,
                    None,
                );
            }
            Ok(PickerOutcome::Cancelled) => {}
            Err(error) => {
                status.set_if_changed(format!("Could not open file: {error}"));
            }
        }
    }) {
        failed_status.set_if_changed(format!("Could not start file picker: {error}"));
    }
}

fn save_document(scope: &AppScope, status: Signal<String>) {
    let Some(window) = scope.main_window() else {
        status.set_if_changed("The editor window is not available.".to_owned());
        return;
    };
    let options = SaveFilePickerOptions::new()
        .title("Save editable Jiffshot document")
        .suggested_file_name("Untitled.jsd")
        .start_in(WellKnownFolder::Pictures)
        .default_extension("jsd")
        .file_type(FileTypeFilter::new("Jiffshot document").with_extension("jsd"));
    let operation = match file_dialog_linux::save_file_picker(scope, &window, &options) {
        Ok(operation) => operation,
        Err(error) => {
            status.set_if_changed(format!("Could not open save dialog: {error}"));
            return;
        }
    };
    save_to_picker(scope, operation, status, true);
}

fn export_image(scope: &AppScope, status: Signal<String>) {
    let Some(window) = scope.main_window() else {
        status.set_if_changed("The editor window is not available.".to_owned());
        return;
    };
    let filter = IMAGE_EXPORT_EXTENSIONS
        .into_iter()
        .fold(FileTypeFilter::new("Image or PDF"), |filter, extension| {
            filter.with_extension(extension)
        });
    let options = SaveFilePickerOptions::new()
        .title("Export flattened image or PDF")
        .suggested_file_name("Jiffshot.png")
        .start_in(WellKnownFolder::Pictures)
        .default_extension("png")
        .file_type(filter);
    let operation = match file_dialog_linux::save_file_picker(scope, &window, &options) {
        Ok(operation) => operation,
        Err(error) => {
            status.set_if_changed(format!("Could not open export dialog: {error}"));
            return;
        }
    };
    save_to_picker(scope, operation, status, false);
}

fn save_to_picker(
    scope: &AppScope,
    operation: file_dialog_linux::PickerOperation,
    status: Signal<String>,
    document: bool,
) {
    let scope = scope.clone();
    let task_scope = scope.clone();
    let failed_status = status;
    if let Err(error) = scope.spawn(async move {
        match operation.await {
            Ok(PickerOutcome::Selected(items)) => {
                let Some(path) = items
                    .first()
                    .and_then(|item| item.local_path().map(Path::to_path_buf))
                else {
                    status.set_if_changed("The destination must be a local file.".to_owned());
                    return;
                };
                let posted_status = status;
                if let Err(error) = task_scope.post(move || {
                    let result = canvas::with_editor(|editor| {
                        if document {
                            jiffshot_doc::io::save_document(editor.document(), &path)
                        } else {
                            jiffshot_doc::io::export(editor.document(), &path)
                        }
                    });
                    match result {
                        Some(Ok(())) => {
                            posted_status.set_if_changed(format!("Saved {}", path.display()))
                        }
                        Some(Err(error)) => posted_status
                            .set_if_changed(format!("Could not save {}: {error}", path.display())),
                        None => {
                            posted_status.set_if_changed("There is no document to save.".to_owned())
                        }
                    }
                }) {
                    eprintln!("Could not update the editor after saving a file: {error}");
                }
            }
            Ok(PickerOutcome::Cancelled) => {}
            Err(error) => {
                let posted_status = status;
                if let Err(post_error) = task_scope.post(move || {
                    posted_status.set_if_changed(format!("Could not save file: {error}"))
                }) {
                    eprintln!("Could not report the save dialog error: {post_error}");
                }
            }
        }
    }) {
        failed_status.set_if_changed(format!("Could not start save dialog: {error}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_copy_status_distinguishes_disabled_success_and_failure() {
        let mut disabled = "Capture saved".to_owned();
        append_automatic_copy_result(&mut disabled, Ok(None));
        assert_eq!(disabled, "Capture saved");

        let mut copied = "Capture saved".to_owned();
        append_automatic_copy_result(&mut copied, Ok(Some(PathBuf::from("/copies/image.png"))));
        assert_eq!(
            copied,
            "Capture saved; automatically copied to /copies/image.png"
        );

        let mut failed = "Capture saved".to_owned();
        append_automatic_copy_result(&mut failed, Err("folder unavailable".to_owned()));
        assert_eq!(
            failed,
            "Automatic copy failed: folder unavailable; Capture saved"
        );
    }

    #[test]
    fn every_offered_image_export_extension_writes_the_requested_format() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let document = Document::blank(10, 8, Rgba::WHITE);
        for extension in IMAGE_EXPORT_EXTENSIONS {
            let path = directory.path().join(format!("capture.{extension}"));
            jiffshot_doc::io::export(&document, &path).expect("image export should succeed");
            let bytes = std::fs::read(&path).expect("exported image should be readable");
            let valid = match extension {
                "png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
                "jpg" | "jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
                "bmp" => bytes.starts_with(b"BM"),
                "gif" => bytes.starts_with(b"GIF8"),
                "webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
                "pdf" => bytes.starts_with(b"%PDF-1.4") && bytes.ends_with(b"%%EOF\n"),
                _ => false,
            };
            assert!(valid, "{extension} must contain the selected format");
        }
    }

    #[test]
    fn selection_shortcuts_require_their_exact_modifiers() {
        use editor_linux::SelectionAction as Action;
        for (key, modifiers, action) in [
            (Key::Delete, 0, Action::Delete),
            (Key::A, 2, Action::SelectAll),
            (Key::D, 2, Action::Deselect),
            (Key::T, 2, Action::FlattenSelected),
            (Key::I, 3, Action::Invert),
            (Key::G, 3, Action::Group),
            (Key::U, 3, Action::Ungroup),
            (Key::F, 3, Action::BringForward),
            (Key::B, 3, Action::SendBackward),
            (Key::T, 3, Action::FlattenAll),
        ] {
            assert_eq!(selection_shortcut(key, modifiers), Some(action));
        }
        assert_eq!(selection_shortcut(Key::G, 2), None);
        assert_eq!(selection_shortcut(Key::G, 1), None);
        assert_eq!(selection_shortcut(Key::A, 6), None);
        assert_eq!(selection_shortcut(Key::Delete, 2), None);
    }

    #[test]
    fn editable_documents_round_trip_with_app_json_features() {
        use jiffshot_doc::geom::Pt;
        use jiffshot_doc::object::{Kind, Style};

        let mut document = Document::blank(64, 32, Rgba::rgb(10, 20, 30));
        document.add(
            Kind::Line {
                a: Pt::new(1.25, 2.5),
                b: Pt::new(30.0, 20.0),
                control: None,
            },
            Style::default(),
        );
        let bytes = jiffshot_doc::io::document_bytes(&document)
            .expect("editable document should serialize");
        let restored =
            jiffshot_doc::io::document_from_bytes(bytes).expect("editable document should open");
        assert_eq!(restored.size(), document.size());
        assert_eq!(restored.objects, document.objects);
        assert_eq!(restored.background().data(), document.background().data());
    }

    #[test]
    fn combined_video_selection_is_sorted_by_filename_then_path() {
        let mut paths = [
            PathBuf::from("/b/02.mp4"),
            PathBuf::from("/z/01.mp4"),
            PathBuf::from("/a/02.mp4"),
        ];
        order_video_sources(&mut paths);
        assert_eq!(
            paths,
            [
                PathBuf::from("/z/01.mp4"),
                PathBuf::from("/a/02.mp4"),
                PathBuf::from("/b/02.mp4"),
            ]
        );
    }

    #[test]
    fn combines_selected_images_in_the_requested_arrangement() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let first = directory.path().join("first.png");
        let second = directory.path().join("second.png");
        let third = directory.path().join("third.png");
        image::RgbaImage::from_pixel(2, 3, image::Rgba([255, 0, 0, 255]))
            .save(&first)
            .expect("first test image should be written");
        image::RgbaImage::from_pixel(4, 1, image::Rgba([0, 0, 255, 255]))
            .save(&second)
            .expect("second test image should be written");
        image::RgbaImage::from_pixel(1, 4, image::Rgba([0, 255, 0, 255]))
            .save(&third)
            .expect("third test image should be written");
        let paths = [first, second, third];

        let horizontal = combine_image_files(&paths, Arrangement::Horizontal)
            .expect("images should combine horizontally");
        assert_eq!(
            horizontal.size(),
            (55, 28),
            "horizontal layout should include each image and configured gaps"
        );
        let vertical = combine_image_files(&paths, Arrangement::Vertical)
            .expect("images should combine vertically");
        assert_eq!(
            vertical.size(),
            (28, 56),
            "vertical layout should stack different-sized images"
        );
        let grid = combine_image_files(&paths, Arrangement::Grid)
            .expect("images should combine in a grid");
        assert_eq!(
            grid.size(),
            (42, 43),
            "grid layout should size cells from each row and column"
        );
    }

    #[test]
    fn combining_requires_at_least_two_image_files() {
        assert!(
            combine_image_files(&[], Arrangement::Grid).is_err(),
            "empty selection should be rejected"
        );
        assert!(
            combine_image_files(&[PathBuf::from("missing.png")], Arrangement::Grid).is_err(),
            "single selection should be rejected before attempting to open a file"
        );
    }
}
