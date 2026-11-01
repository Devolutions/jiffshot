//! Screen recording: frames of an area (with the pointer, its highlight and
//! click rings, and the webcam in a corner) and system audio, the
//! microphone or both mixed, encoded to an H.264/AAC MP4 in the library.
//! A countdown comes first; then a toolbar below the area stops, pauses,
//! restarts or cancels, starts Screen Draw, and switches the pointer, the
//! webcam, system sound and the microphone on and off while recording.
//! Shift+F9 / Shift+F10 pause and stop from anywhere, or the next free
//! keys when another app holds those ([`key_names`]).

#[cfg(target_os = "macos")]
use crate::audio_timeline::{TimedMixer, Track};
use crate::geometry::CaptureLayout;
use crate::hud::{self, Border};
use crate::settings::Settings;
use crate::sys::{self, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT, VK_F10, VK_F9};
#[cfg(windows)]
use crate::vfx::Mixer;
use crate::vfx::{self, Clicks, NoiseGate};
use crate::{log, screendraw};
use jiffshot_capture::library::Library;
use jiffshot_capture::media::{
    self, AudioCapture, Reader, VideoFrame, VideoSpec, Writer, HNS_PER_SECOND,
};
use jiffshot_capture::screen::CursorSnapshot;
use jiffshot_capture::{CaptureMapping, Frame, Rect};
use jiffshot_doc::object::{Kind, Style};
use jiffshot_doc::{Document, Pixmap, Rgba};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A recording key: modifiers, virtual key and how it is written.
type Key = (HOT_KEY_MODIFIERS, u16, &'static str);

/// The pause and stop keys, best first: another app (often another capture
/// tool) may hold Shift+F9 or Shift+F10, and a key registered by nobody
/// does nothing when pressed.
const PAUSE_KEYS: [Key; 3] = [
    (MOD_SHIFT, VK_F9, "Shift+F9"),
    (MOD_CONTROL | MOD_SHIFT, VK_F9, "Ctrl+Shift+F9"),
    (MOD_CONTROL | MOD_ALT, VK_F9, "Ctrl+Alt+F9"),
];
const STOP_KEYS: [Key; 3] = [
    (MOD_SHIFT, VK_F10, "Shift+F10"),
    (MOD_CONTROL | MOD_SHIFT, VK_F10, "Ctrl+Shift+F10"),
    (MOD_CONTROL | MOD_ALT, VK_F10, "Ctrl+Alt+F10"),
];

/// The pause and stop keys the last check found free.
static KEYS: Mutex<Option<(Option<Key>, Option<Key>)>> = Mutex::new(None);

#[derive(Clone, Copy)]
enum SourceKind {
    System,
    Microphone,
    Webcam,
}

#[derive(Default)]
struct SourceNotes {
    system: Option<String>,
    microphone: Option<String>,
    webcam: Option<String>,
}

/// The first of `keys` no other app holds.
#[cfg(windows)]
fn free(keys: &[Key]) -> Option<Key> {
    use crate::sys::{RegisterHotKey, UnregisterHotKey, MOD_NOREPEAT};
    const PROBE: i32 = 0x53F0;
    keys.iter().copied().find(|&(modifiers, vk, _)| {
        // SAFETY: a test registration for this thread, removed at once.
        unsafe {
            let ok = RegisterHotKey(
                std::ptr::null_mut(),
                PROBE,
                modifiers | MOD_NOREPEAT,
                vk as u32,
            ) != 0;
            if ok {
                UnregisterHotKey(std::ptr::null_mut(), PROBE);
            }
            ok
        }
    })
}

/// The first of `keys`: macOS lets several apps hold one key combination,
/// so there is nothing to probe.
#[cfg(not(windows))]
fn free(keys: &[Key]) -> Option<Key> {
    keys.first().copied()
}

/// Finds the pause and stop keys free now. Not while recording, which holds
/// them itself.
pub fn check_keys() -> (Option<Key>, Option<Key>) {
    let keys = (free(&PAUSE_KEYS), free(&STOP_KEYS));
    *KEYS.lock().unwrap_or_else(|e| e.into_inner()) = Some(keys);
    keys
}

/// How the pause and stop keys are written ("Shift+F9", "Shift+F10"), or
/// `None` for one that every candidate is taken for (the toolbar's button
/// still works).
pub fn key_names() -> (Option<&'static str>, Option<&'static str>) {
    let known = *KEYS.lock().unwrap_or_else(|e| e.into_inner());
    let (pause, stop) = known.unwrap_or_else(check_keys);
    (pause.map(|k| k.2), stop.map(|k| k.2))
}

/// "Shift+F9 pauses and resumes{sep}Shift+F10 stops", for the windows that
/// list keys.
pub fn keys_text(sep: &str) -> String {
    let (pause, stop) = key_names();
    let pause = pause.map_or("the toolbar pauses and resumes".to_owned(), |k| {
        format!("{k} pauses and resumes")
    });
    let stop = stop.map_or("the toolbar stops".to_owned(), |k| format!("{k} stops"));
    format!("{pause}{sep}{stop}")
}

/// State shared between the recording thread and the toolbar. The
/// switches are read on every frame, so they change a recording while it
/// runs.
#[derive(Default)]
pub struct Control {
    pub stop: AtomicBool,
    pub paused: AtomicBool,
    pub frames: AtomicU64,
    /// Recorded time so far (pauses excluded), in milliseconds.
    pub millis: AtomicU64,
    pub error: Mutex<Option<String>>,
    /// A note about the sound or the webcam ("no microphone: …").
    pub audio: Mutex<Option<String>>,
    source_notes: Mutex<SourceNotes>,
    /// The pointer is drawn into the frames.
    pub cursor: AtomicBool,
    pub highlight: AtomicBool,
    pub clicks: AtomicBool,
    pub webcam: AtomicBool,
    pub webcam_circle: AtomicBool,
    /// "top-left", "top-right", "bottom-left" or "bottom-right".
    pub webcam_corner: Mutex<String>,
    /// The sound sources wanted; one that cannot start turns itself off.
    pub system: AtomicBool,
    pub microphone: AtomicBool,
    pub system_silent: AtomicBool,
    pub microphone_silent: AtomicBool,
    pub(crate) max_duration: Option<Duration>,
    annotations: Mutex<Annotations>,
}

#[derive(Default)]
struct Annotations {
    size: Option<(u32, u32)>,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    next_id: u64,
    marks: Vec<(u64, Kind, Style, Option<u64>)>,
    image: Option<Pixmap>,
    document: Option<Document>,
    dirty: bool,
}

impl Annotations {
    fn blend(&mut self, frame: &mut Frame, now: u64) -> Result<(), String> {
        let before = self.marks.len();
        self.marks
            .retain(|(_, _, _, end)| end.is_none_or(|end| now < end));
        if self.dirty || self.marks.len() != before {
            self.image = None;
            self.document = None;
            if !self.marks.is_empty() {
                let mut doc = Document::blank(frame.width, frame.height, Rgba::TRANSPARENT);
                for (_, kind, style, _) in &self.marks {
                    doc.add(kind.clone(), style.clone());
                }
                if doc.objects.iter().any(|object| object.reads().is_some()) {
                    self.document = Some(doc);
                } else {
                    self.image = Some(doc.flatten());
                }
            }
            self.dirty = false;
        }
        if let Some(doc) = &mut self.document {
            let background =
                crate::stepcap::frame_to_pixmap(frame).ok_or("invalid recording frame")?;
            doc.set_background(background);
            screendraw::blend_image(frame, &doc.flatten());
        } else if let Some(image) = &self.image {
            screendraw::blend_image(frame, image);
        }
        Ok(())
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn add_annotation(
    control: &Control,
    kind: Kind,
    style: Style,
    duration: Duration,
) -> Result<u64, String> {
    let mut overlay = control
        .annotations
        .lock()
        .map_err(|_| "recording annotations are unavailable")?;
    if overlay.size.is_none() {
        return Err("recording frames are not ready".into());
    }
    if overlay.marks.len() >= 256 {
        return Err("at most 256 live annotations; use record.clear".into());
    }
    overlay.next_id = overlay
        .next_id
        .checked_add(1)
        .ok_or("too many recording annotations")?;
    let id = overlay.next_id;
    let end = (!duration.is_zero()).then(|| {
        control
            .millis
            .load(Ordering::Relaxed)
            .saturating_add(duration.as_millis() as u64)
    });
    overlay.marks.push((id, kind, style, end));
    overlay.dirty = true;
    Ok(id)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn clear_annotations(control: &Control) -> Result<(), String> {
    let mut overlay = control
        .annotations
        .lock()
        .map_err(|_| "recording annotations are unavailable")?;
    overlay.marks.clear();
    overlay.dirty = true;
    Ok(())
}

static RECORDING_ACTIVE: AtomicBool = AtomicBool::new(false);

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn is_recording() -> bool {
    RECORDING_ACTIVE.load(Ordering::Acquire)
}

pub(crate) struct RecordingLease {
    active: &'static AtomicBool,
}

impl RecordingLease {
    pub(crate) fn acquire() -> Result<Self, String> {
        Self::acquire_from(&RECORDING_ACTIVE)
    }

    fn acquire_from(active: &'static AtomicBool) -> Result<Self, String> {
        active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self { active })
            .map_err(|_| "another recording is already running (tray or CLI)".into())
    }
}

impl Drop for RecordingLease {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

impl Control {
    pub(crate) fn set_annotation_size(&self, width: u32, height: u32) -> Result<(), String> {
        self.annotations
            .lock()
            .map_err(|_| "recording annotations are unavailable")?
            .size = Some((width, height));
        Ok(())
    }

    /// The switches as `options` sets them.
    pub fn new(options: &Options) -> Control {
        let c = Control::default();
        c.cursor.store(options.cursor, Ordering::Relaxed);
        c.highlight
            .store(options.cursor_highlight, Ordering::Relaxed);
        c.clicks.store(options.click_rings, Ordering::Relaxed);
        c.webcam.store(options.webcam.is_some(), Ordering::Relaxed);
        c.webcam_circle
            .store(options.webcam_circle, Ordering::Relaxed);
        *c.webcam_corner.lock().unwrap() = options
            .webcam
            .clone()
            .unwrap_or_else(|| options.webcam_corner.clone());
        c.system.store(
            matches!(options.audio.as_str(), "system" | "both"),
            Ordering::Relaxed,
        );
        c.microphone.store(
            matches!(options.audio.as_str(), "microphone" | "both"),
            Ordering::Relaxed,
        );
        c
    }

    /// A fresh take with this one's switches (Restart).
    fn again(&self) -> Control {
        let c = Control::default();
        for (to, from) in [
            (&c.cursor, &self.cursor),
            (&c.highlight, &self.highlight),
            (&c.clicks, &self.clicks),
            (&c.webcam, &self.webcam),
            (&c.webcam_circle, &self.webcam_circle),
            (&c.system, &self.system),
            (&c.microphone, &self.microphone),
        ] {
            to.store(from.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        *c.webcam_corner.lock().unwrap() = self.webcam_corner.lock().unwrap().clone();
        c
    }

    fn note(&self, source: SourceKind, text: Option<String>) {
        let mut notes = self.source_notes.lock().unwrap();
        match source {
            SourceKind::System => notes.system = text,
            SourceKind::Microphone => notes.microphone = text,
            SourceKind::Webcam => notes.webcam = text,
        }
        let messages: Vec<&str> = [&notes.system, &notes.microphone, &notes.webcam]
            .into_iter()
            .filter_map(|note| note.as_deref())
            .collect();
        *self.audio.lock().unwrap() = (!messages.is_empty()).then(|| messages.join(" "));
    }

    fn on(&self, flag: &AtomicBool) -> bool {
        flag.load(Ordering::Relaxed)
    }
}

/// What goes into a recording besides the screen.
#[derive(Clone, Debug)]
pub struct Options {
    pub fps: u32,
    /// "system", "microphone", "both" or "none".
    pub audio: String,
    pub noise_gate: bool,
    pub cursor_highlight: bool,
    pub click_rings: bool,
    pub cursor: bool,
    pub webcam: Option<String>,
    /// Where the webcam goes when it is turned on while recording.
    pub webcam_corner: String,
    pub webcam_circle: bool,
    pub webcam_size: f32,
}

impl Options {
    pub fn from_settings(s: &Settings) -> Options {
        Options {
            fps: s.record_fps,
            audio: s.record_audio.clone(),
            noise_gate: s.noise_gate,
            cursor_highlight: s.cursor_highlight,
            click_rings: s.click_rings,
            cursor: s.record_cursor,
            webcam: s.webcam.clone(),
            webcam_corner: s
                .webcam
                .clone()
                .unwrap_or_else(|| s.capture_window.webcam_corner.clone()),
            webcam_circle: s.webcam_shape != "rectangle",
            webcam_size: s.webcam_size,
        }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn audio_only(fps: u32, audio: &str) -> Options {
        Options {
            fps,
            audio: audio.to_owned(),
            noise_gate: false,
            cursor_highlight: false,
            click_rings: false,
            cursor: true,
            webcam: None,
            webcam_corner: "bottom-right".into(),
            webcam_circle: true,
            webcam_size: 0.25,
        }
    }
}

/// The latest webcam picture, refreshed by a reader thread.
enum WebcamEvent {
    Ready,
    Failed(String),
}

struct Webcam {
    latest: Arc<Mutex<Option<(u32, u32, Vec<u8>)>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    events: std::sync::mpsc::Receiver<WebcamEvent>,
}

impl Webcam {
    fn start() -> Result<Webcam, String> {
        let webcam = Self::start_with(
            |stop| {
                #[cfg(target_os = "macos")]
                {
                    Reader::webcam_cancellable(stop)
                }
                #[cfg(windows)]
                {
                    let _ = stop;
                    Reader::webcam()
                }
            },
            |reader: &mut Reader| reader.next_video(),
        );
        #[cfg(target_os = "macos")]
        {
            Ok(webcam)
        }
        #[cfg(windows)]
        {
            webcam.wait_ready()
        }
    }

    #[cfg(windows)]
    fn wait_ready(mut self) -> Result<Webcam, String> {
        match self.events.recv_timeout(Duration::from_secs(5)) {
            Ok(WebcamEvent::Ready) => Ok(self),
            Ok(WebcamEvent::Failed(error)) => Err(error),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err("the webcam reader stopped unexpectedly".into())
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                self.stop.store(true, Ordering::Release);
                // A late open drops its reader instead of starting the frame loop.
                drop(self.thread.take());
                Err("the webcam did not start".into())
            }
        }
    }

    fn start_with<R: Send + 'static>(
        open: impl FnOnce(Arc<AtomicBool>) -> Result<R, String> + Send + 'static,
        mut next: impl FnMut(&mut R) -> Result<Option<VideoFrame>, String> + Send + 'static,
    ) -> Webcam {
        let latest: Arc<Mutex<Option<(u32, u32, Vec<u8>)>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (l, s) = (latest.clone(), stop.clone());
        let (events_tx, events) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut reader = match open(s.clone()) {
                Ok(reader) => reader,
                Err(error) => {
                    if !s.load(Ordering::Acquire) {
                        let _ = events_tx.send(WebcamEvent::Failed(error));
                    }
                    return;
                }
            };
            if !s.load(Ordering::Acquire) {
                let _ = events_tx.send(WebcamEvent::Ready);
            }
            while !s.load(Ordering::Acquire) {
                match next(&mut reader) {
                    Ok(Some(f)) => *l.lock().unwrap() = Some((f.width, f.height, f.bgra)),
                    Ok(None) => {
                        if !s.load(Ordering::Acquire) {
                            let _ = events_tx.send(WebcamEvent::Failed(
                                "the webcam frame stream ended".to_owned(),
                            ));
                        }
                        break;
                    }
                    Err(error) => {
                        if !s.load(Ordering::Acquire) {
                            let _ = events_tx.send(WebcamEvent::Failed(error));
                        }
                        break;
                    }
                }
            }
        });
        Webcam {
            latest,
            stop,
            thread: Some(thread),
            events,
        }
    }

    fn event(&self) -> Option<WebcamEvent> {
        match self.events.try_recv() {
            Ok(event) => Some(event),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(WebcamEvent::Failed(
                "the webcam reader stopped unexpectedly".into(),
            )),
        }
    }
}

impl Drop for Webcam {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn follow_webcam(webcam: &mut Option<Webcam>, control: &Control) {
    if !control.on(&control.webcam) {
        *webcam = None;
        return;
    }
    while let Some(event) = webcam.as_ref().and_then(Webcam::event) {
        match event {
            WebcamEvent::Ready => control.note(SourceKind::Webcam, None),
            WebcamEvent::Failed(error) => {
                control.note(SourceKind::Webcam, Some(format!("webcam stopped: {error}")));
                control.webcam.store(false, Ordering::Relaxed);
                *webcam = None;
                return;
            }
        }
    }
    if webcam.is_none() {
        match Webcam::start() {
            Ok(started) => {
                #[cfg(windows)]
                control.note(SourceKind::Webcam, None);
                *webcam = Some(started);
            }
            Err(error) => {
                control.note(SourceKind::Webcam, Some(format!("no webcam: {error}")));
                control.webcam.store(false, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(windows)]
fn start_audio(microphone: bool, control: &Control) -> Option<AudioCapture> {
    let source = if microphone {
        SourceKind::Microphone
    } else {
        SourceKind::System
    };
    match media::capture_audio(microphone) {
        Ok(c) => {
            control.note(source, None);
            Some(c)
        }
        Err(error) => {
            let what = if microphone {
                "microphone"
            } else {
                "system audio"
            };
            control.note(source, Some(format!("no {what}: {error}")));
            None
        }
    }
}

/// The sound track's format when no source is on at the start, so one
/// turned on later can still join.
const DEFAULT_AUDIO: media::AudioSpec = media::AudioSpec {
    sample_rate: 48_000,
    channels: 2,
};

#[cfg(target_os = "macos")]
struct PendingSource<T> {
    cancel: Arc<AtomicBool>,
    result: std::sync::mpsc::Receiver<Result<T, String>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "macos")]
impl<T: Send + 'static> PendingSource<T> {
    fn start(
        open: impl FnOnce(Arc<AtomicBool>) -> Result<T, String> + Send + 'static,
    ) -> PendingSource<T> {
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let (sender, result) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _ = sender.send(open(worker_cancel));
        });
        PendingSource {
            cancel,
            result,
            thread: Some(thread),
        }
    }

    fn result(&self) -> Option<Result<T, String>> {
        match self.result.try_recv() {
            Ok(result) => Some(result),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Some(Err("the audio source worker stopped unexpectedly".into()))
            }
        }
    }

    fn finished(&self) -> bool {
        self.thread
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }
}

#[cfg(target_os = "macos")]
impl<T> Drop for PendingSource<T> {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A sound source that follows its switch: started when turned on (if its
/// format matches the track's), stopped when turned off.
struct Source {
    microphone: bool,
    capture: Option<AudioCapture>,
    silence: SilenceMonitor,
    #[cfg(target_os = "macos")]
    late_frames: u64,
    #[cfg(target_os = "macos")]
    late_reported: bool,
    #[cfg(target_os = "macos")]
    pending: Option<PendingSource<AudioCapture>>,
}

impl Source {
    fn new(microphone: bool) -> Source {
        Source {
            microphone,
            capture: None,
            silence: SilenceMonitor::default(),
            #[cfg(target_os = "macos")]
            late_frames: 0,
            #[cfg(target_os = "macos")]
            late_reported: false,
            #[cfg(target_os = "macos")]
            pending: None,
        }
    }

    #[cfg(target_os = "macos")]
    fn stopped(
        &mut self,
        error: &str,
        wanted: &AtomicBool,
        silent: &AtomicBool,
        control: &Control,
    ) {
        let what = if self.microphone {
            "microphone"
        } else {
            "system audio"
        };
        let source = if self.microphone {
            SourceKind::Microphone
        } else {
            SourceKind::System
        };
        control.note(source, Some(format!("{what} stopped: {error}")));
        self.capture = None;
        self.silence.reset();
        wanted.store(false, Ordering::Relaxed);
        silent.store(false, Ordering::Relaxed);
    }

    fn follow(
        &mut self,
        wanted: &AtomicBool,
        silent: &AtomicBool,
        spec: media::AudioSpec,
        control: &Control,
    ) {
        #[cfg(target_os = "macos")]
        self.follow_macos(wanted, silent, spec, control);
        #[cfg(windows)]
        self.follow_with(wanted, silent, spec, control, start_audio);
    }

    #[cfg(target_os = "macos")]
    fn follow_macos(
        &mut self,
        wanted: &AtomicBool,
        silent: &AtomicBool,
        spec: media::AudioSpec,
        control: &Control,
    ) {
        self.follow_macos_with(
            wanted,
            silent,
            spec,
            control,
            media::capture_audio_cancellable,
        );
    }

    #[cfg(target_os = "macos")]
    fn follow_macos_with(
        &mut self,
        wanted: &AtomicBool,
        silent: &AtomicBool,
        spec: media::AudioSpec,
        control: &Control,
        open: impl FnOnce(bool, Arc<AtomicBool>) -> Result<AudioCapture, String> + Send + 'static,
    ) {
        if !wanted.load(Ordering::Relaxed) {
            self.capture = None;
            self.silence.reset();
            silent.store(false, Ordering::Relaxed);
            if let Some(pending) = &self.pending {
                pending.cancel.store(true, Ordering::Release);
                if pending.finished() {
                    self.pending = None;
                }
            }
            return;
        }
        if let Some(error) = self.capture.as_ref().and_then(AudioCapture::failure) {
            self.stopped(&error, wanted, silent, control);
            return;
        }
        if self.capture.is_some() {
            return;
        }
        if let Some(pending) = &self.pending {
            if pending.cancel.load(Ordering::Acquire) {
                if pending.finished() {
                    self.pending = None;
                } else {
                    return;
                }
            }
        }
        if let Some(result) = self.pending.as_ref().and_then(PendingSource::result) {
            self.pending = None;
            let source = if self.microphone {
                SourceKind::Microphone
            } else {
                SourceKind::System
            };
            let what = if self.microphone {
                "microphone"
            } else {
                "system audio"
            };
            match result {
                Ok(c) if c.spec == spec => {
                    control.note(source, None);
                    self.capture = Some(c);
                    self.silence.start(Instant::now());
                    self.late_frames = 0;
                    self.late_reported = false;
                }
                Ok(c) => {
                    control.note(
                        source,
                        Some(format!(
                            "{what} left out: {} Hz, the recording is {} Hz",
                            c.spec.sample_rate, spec.sample_rate
                        )),
                    );
                    wanted.store(false, Ordering::Relaxed);
                }
                Err(error) => {
                    control.note(source, Some(format!("no {what}: {error}")));
                    wanted.store(false, Ordering::Relaxed);
                }
            }
        } else if self.pending.is_none() {
            let microphone = self.microphone;
            self.pending = Some(PendingSource::start(move |cancel| open(microphone, cancel)));
        }
    }

    #[cfg(windows)]
    fn follow_with(
        &mut self,
        wanted: &AtomicBool,
        silent: &AtomicBool,
        spec: media::AudioSpec,
        control: &Control,
        start: impl FnOnce(bool, &Control) -> Option<AudioCapture>,
    ) {
        if !wanted.load(Ordering::Relaxed) {
            self.capture = None;
            self.silence.reset();
            silent.store(false, Ordering::Relaxed);
            return;
        }
        if self.capture.is_none() {
            match start(self.microphone, control) {
                Some(c) if c.spec == spec => {
                    self.capture = Some(c);
                    self.silence.start(Instant::now());
                }
                Some(c) => {
                    let what = if self.microphone {
                        "microphone"
                    } else {
                        "system sound"
                    };
                    let source = if self.microphone {
                        SourceKind::Microphone
                    } else {
                        SourceKind::System
                    };
                    control.note(
                        source,
                        Some(format!(
                            "{what} left out: {} Hz, the recording is {} Hz",
                            c.spec.sample_rate, spec.sample_rate
                        )),
                    );
                    wanted.store(false, Ordering::Relaxed);
                }
                None => {
                    wanted.store(false, Ordering::Relaxed);
                }
            }
        }
    }

    fn update_silence(&self, now: Instant, silent: &AtomicBool) {
        silent.store(
            self.capture.is_some() && self.silence.is_silent(now),
            Ordering::Relaxed,
        );
    }

    #[cfg(target_os = "macos")]
    fn report_late_audio(&mut self, frames: u64, rate: u32, control: &Control) {
        self.late_frames = self.late_frames.saturating_add(frames);
        if !self.late_reported && self.late_frames >= (u64::from(rate) / 20).max(1) {
            let (source, what) = if self.microphone {
                (SourceKind::Microphone, "microphone")
            } else {
                (SourceKind::System, "system audio")
            };
            control.note(
                source,
                Some(format!(
                    "{what}: some audio arrived too late to include in the recording"
                )),
            );
            self.late_reported = true;
        }
    }
}

#[cfg(target_os = "macos")]
const AUDIO_REORDER_DELAY: Duration = Duration::from_millis(500);

#[cfg(target_os = "macos")]
fn collect_timed_audio(
    mixer: &mut TimedMixer,
    system: &mut Source,
    microphone: &mut Source,
    gate: &mut Option<NoiseGate>,
    now: Instant,
    control: &Control,
) -> Result<(), String> {
    let mut microphone_late = 0u64;
    if let Some(capture) = &microphone.capture {
        for mut block in capture.samples.try_iter() {
            let captured_at = block
                .captured_at
                .ok_or("microphone supplied audio without a capture timestamp")?;
            microphone.silence.observe(&block.samples, now);
            if let Some(gate) = gate {
                gate.process(&mut block.samples);
            }
            microphone_late = microphone_late.saturating_add(mixer.push(
                Track::Microphone,
                captured_at,
                &block.samples,
            )?);
        }
        microphone.report_late_audio(microphone_late, capture.spec.sample_rate, control);
    }
    let mut system_late = 0u64;
    if let Some(capture) = &system.capture {
        for block in capture.samples.try_iter() {
            let captured_at = block
                .captured_at
                .ok_or("system audio supplied samples without a capture timestamp")?;
            system.silence.observe(&block.samples, now);
            system_late = system_late.saturating_add(mixer.push(
                Track::System,
                captured_at,
                &block.samples,
            )?);
        }
        system.report_late_audio(system_late, capture.spec.sample_rate, control);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn write_timed_audio(
    mixer: &mut TimedMixer,
    writer: &mut Writer,
    target: u64,
    rate: u32,
) -> Result<(), String> {
    let hns = |frames: u64| {
        i64::try_from(u128::from(frames) * HNS_PER_SECOND as u128 / u128::from(rate))
            .map_err(|_| "audio recording is too long".to_owned())
    };
    while mixer.written() < target {
        let start = mixer.written();
        let block = mixer.take_to(target);
        let time = hns(start)?;
        let duration = hns(mixer.written())? - time;
        writer.write_audio(&block, time, duration)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn video_audio_frames(written: i64, frame_time: i64, rate: u32) -> Result<u64, String> {
    let video_hns = i128::from(written) * i128::from(frame_time);
    u64::try_from(
        (video_hns * i128::from(rate) + i128::from(HNS_PER_SECOND) / 2)
            / i128::from(HNS_PER_SECOND),
    )
    .map_err(|_| "recording is too long".to_owned())
}

#[cfg(target_os = "macos")]
fn repeat_video_until(
    writer: &mut Writer,
    frame: &Frame,
    written: &mut i64,
    frame_time: i64,
    elapsed: Duration,
) -> Result<(), String> {
    if *written == 0 {
        return Ok(());
    }
    let elapsed = i64::try_from(elapsed.as_nanos() / 100).map_err(|_| "recording is too long")?;
    let due = elapsed / frame_time;
    while *written <= due {
        writer.write_frame(&frame.pixels, *written * frame_time, frame_time)?;
        *written += 1;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn write_video_through_due(
    writer: &mut Writer,
    previous: &Frame,
    current: &Frame,
    written: &mut i64,
    due: i64,
    frame_time: i64,
) -> Result<(), String> {
    let earlier = if *written == 0 { current } else { previous };
    while *written < due {
        writer.write_frame(&earlier.pixels, *written * frame_time, frame_time)?;
        *written += 1;
    }
    writer.write_frame(&current.pixels, *written * frame_time, frame_time)?;
    *written += 1;
    Ok(())
}

const SILENCE_WARNING_AFTER: Duration = Duration::from_secs(5);
const SIGNAL_RMS_THRESHOLD: u64 = 400;

#[derive(Default)]
struct SilenceMonitor {
    last_signal: Option<Instant>,
}

impl SilenceMonitor {
    fn start(&mut self, now: Instant) {
        self.last_signal = Some(now);
    }

    fn reset(&mut self) {
        self.last_signal = None;
    }

    fn observe(&mut self, samples: &[i16], now: Instant) {
        let energy = samples
            .iter()
            .map(|sample| u64::from(sample.unsigned_abs()).pow(2))
            .sum::<u64>();
        let has_signal =
            !samples.is_empty() && energy / samples.len() as u64 >= SIGNAL_RMS_THRESHOLD.pow(2);
        if has_signal {
            self.last_signal = Some(now);
        }
    }

    fn is_silent(&self, now: Instant) -> bool {
        self.last_signal.is_some_and(|last_signal| {
            now.saturating_duration_since(last_signal) >= SILENCE_WARNING_AFTER
        })
    }
}

fn cursor_point_in(
    mapping: CaptureMapping,
    (x, y): (f64, f64),
) -> Result<Option<(i32, i32)>, String> {
    if !x.is_finite() || !y.is_finite() {
        return Err("the recording pointer position is invalid".into());
    }
    if !mapping.contains_precise_point(x, y) {
        return Ok(None);
    }
    let (x, y) = mapping
        .image_point_precise(x, y)
        .ok_or("the recording pointer position is outside the image coordinate range")?;
    let (width, height) = mapping.size();
    Ok(Some((
        x.clamp(0, width as i32 - 1),
        y.clamp(0, height as i32 - 1),
    )))
}

fn cursor_in(
    mapping: CaptureMapping,
    layout: &CaptureLayout,
) -> Result<Option<(i32, i32)>, String> {
    #[cfg(target_os = "macos")]
    let point = {
        let (x, y) = jiffshot_capture::screen::try_cursor_pos_points()
            .map_err(|error| format!("could not locate the recording pointer: {error}"))?;
        let scale = layout.desktop_scale();
        (x * scale, y * scale)
    };
    #[cfg(windows)]
    let point = {
        let _ = layout;
        let (x, y) = sys::cursor_pos();
        (x as f64, y as f64)
    };
    cursor_point_in(mapping, point)
}

struct PartialRecording<'a> {
    path: &'a Path,
    started: bool,
}

impl Drop for PartialRecording<'_> {
    fn drop(&mut self) {
        if self.started {
            if let Err(error) = std::fs::remove_file(self.path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    eprintln!(
                        "could not remove incomplete recording {}: {error}",
                        self.path.display()
                    );
                }
            }
        }
    }
}

#[cfg(any(windows, test))]
fn discard_recording(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "could not remove recording {}: {error}",
            path.display()
        )),
    }
}

/// Records `rect` into `path` until `control.stop` is set. Runs on the
/// calling thread.
#[cfg(any(windows, test))]
pub fn record_to(
    path: &Path,
    rect: Rect,
    options: &Options,
    control: &Control,
) -> Result<VideoSpec, String> {
    let layout = CaptureLayout::current().map_err(|error| error.to_string())?;
    record_to_in_layout(path, rect, options, control, &layout)
}

pub(crate) fn recording_geometry(
    rect: Rect,
    fps: u32,
    layout: &CaptureLayout,
) -> Result<(CaptureMapping, VideoSpec), String> {
    let initial = layout.mapping(rect).map_err(|error| error.to_string())?;
    let (width, height) = initial.size();
    let spec = VideoSpec::for_size(width, height, fps.clamp(5, 60));
    if spec.width == 0 || spec.height == 0 {
        return Err("the recording area must be at least two pixels wide and high".to_owned());
    }
    #[cfg(windows)]
    let rect = Rect::new(rect.x, rect.y, spec.width as i32, spec.height as i32);
    #[cfg(windows)]
    let mapping =
        CaptureMapping::new(rect, spec.width, spec.height).ok_or("invalid recording dimensions")?;
    #[cfg(target_os = "macos")]
    let mapping = initial
        .resized(spec.width, spec.height)
        .ok_or("invalid recording dimensions")?;
    Ok((mapping, spec))
}

pub(crate) fn record_to_in_layout(
    path: &Path,
    rect: Rect,
    options: &Options,
    control: &Control,
    layout: &CaptureLayout,
) -> Result<VideoSpec, String> {
    let lease = RecordingLease::acquire()?;
    record_with_lease(path, rect, options, control, layout, lease)
}

#[cfg(target_os = "macos")]
pub(crate) struct SelftestPointer {
    pub position: (f64, f64),
    pub cursor: CursorSnapshot,
}

#[cfg(target_os = "macos")]
pub(crate) fn record_with_selftest_pointer(
    path: &Path,
    rect: Rect,
    options: &Options,
    control: &Control,
    layout: &CaptureLayout,
    pointer: SelftestPointer,
) -> Result<VideoSpec, String> {
    let lease = RecordingLease::acquire()?;
    record_inner(path, rect, options, control, layout, lease, Some(pointer))
}

pub(crate) fn record_with_lease(
    path: &Path,
    rect: Rect,
    options: &Options,
    control: &Control,
    layout: &CaptureLayout,
    _lease: RecordingLease,
) -> Result<VideoSpec, String> {
    #[cfg(target_os = "macos")]
    return record_inner(path, rect, options, control, layout, _lease, None);
    #[cfg(windows)]
    record_inner(path, rect, options, control, layout, _lease)
}

fn record_inner(
    path: &Path,
    rect: Rect,
    options: &Options,
    control: &Control,
    layout: &CaptureLayout,
    _lease: RecordingLease,
    #[cfg(target_os = "macos")] pointer_fixture: Option<SelftestPointer>,
) -> Result<VideoSpec, String> {
    #[cfg(target_os = "macos")]
    if crate::shutdown_macos::requested() {
        return Err("Jiffshot is shutting down".into());
    }
    layout.validate().map_err(|error| error.to_string())?;
    media::startup()?;
    let (mapping, spec) = recording_geometry(rect, options.fps, layout)?;
    #[cfg(windows)]
    let rect = mapping.rect();
    let mut system = Source::new(false);
    let mut microphone = Source::new(true);
    #[cfg(windows)]
    let audio_spec = {
        // The track takes the format of the first source that starts.
        if control.on(&control.system) {
            system.capture = start_audio(false, control);
            if system.capture.is_none() {
                control.system.store(false, Ordering::Relaxed);
            }
        }
        system.capture.as_ref().map(|c| c.spec).unwrap_or_else(|| {
            if control.on(&control.microphone) {
                microphone.capture = start_audio(true, control);
                if microphone.capture.is_none() {
                    control.microphone.store(false, Ordering::Relaxed);
                }
            }
            microphone
                .capture
                .as_ref()
                .map_or(DEFAULT_AUDIO, |c| c.spec)
        })
    };
    #[cfg(target_os = "macos")]
    let audio_spec = DEFAULT_AUDIO;
    let mut webcam: Option<Webcam> = None;
    #[cfg(target_os = "macos")]
    let mut grabber = layout
        .grabber_for_mapping(mapping)
        .map_err(|error| error.to_string())?;
    #[cfg(windows)]
    let mut grabber = layout.grabber(rect).map_err(|error| error.to_string())?;
    let mut partial = PartialRecording {
        path,
        started: false,
    };
    let mut writer = Writer::create(path, spec, Some(audio_spec))?;
    partial.started = true;
    let frame_time = HNS_PER_SECOND / spec.fps as i64;
    let rate = audio_spec.sample_rate as i64;
    let mut gate = options.noise_gate.then(|| NoiseGate::new(rate as u32));
    #[cfg(windows)]
    let mut mixer: Option<Mixer> = None;
    let mut clicks = Clicks::default();
    let started = Instant::now();
    if system.capture.is_some() {
        system.silence.start(started);
    }
    if microphone.capture.is_some() {
        microphone.silence.start(started);
    }
    let mut paused_for = Duration::ZERO;
    let mut paused_at: Option<Instant> = None;
    let mut written = 0i64;
    #[cfg(windows)]
    let mut audio_frames = 0i64;
    let mut frame = Frame::new(spec.width, spec.height);
    control.set_annotation_size(spec.width, spec.height)?;
    #[cfg(target_os = "macos")]
    let mut previous_frame = Frame::new(spec.width, spec.height);
    #[cfg(windows)]
    let drain = |c: &Option<AudioCapture>| {
        if let Some(c) = c {
            while c.samples.try_recv().is_ok() {}
        }
    };
    #[cfg(target_os = "macos")]
    let mut mixer = TimedMixer::new(started, audio_spec.sample_rate);
    // Finer sleeps while recording (the default tick is 15.6 ms).
    let fine_timer = sys::FineTimer::start();
    let mut checked_layout = Instant::now();
    let mut limit_started: Option<Instant> = None;
    while !control.stop.load(Ordering::Acquire) {
        #[cfg(target_os = "macos")]
        if crate::shutdown_macos::requested() {
            control.stop.store(true, Ordering::Release);
            break;
        }
        if control
            .max_duration
            .zip(limit_started)
            .is_some_and(|(limit, started)| started.elapsed() >= limit)
        {
            control.stop.store(true, Ordering::Release);
            break;
        }
        if checked_layout.elapsed() >= Duration::from_millis(250) {
            layout.validate().map_err(|error| error.to_string())?;
            checked_layout = Instant::now();
        }
        let now = Instant::now();
        let paused = control.paused.load(Ordering::Acquire);
        #[cfg(target_os = "macos")]
        {
            if paused {
                if paused_at.is_none() {
                    repeat_video_until(
                        &mut writer,
                        &previous_frame,
                        &mut written,
                        frame_time,
                        (now - started).saturating_sub(paused_for),
                    )?;
                    mixer.set_paused(true, now)?;
                    paused_at = Some(now);
                }
            } else if let Some(at) = paused_at.take() {
                paused_for += now - at;
                mixer.set_paused(false, now)?;
            }
            collect_timed_audio(
                &mut mixer,
                &mut system,
                &mut microphone,
                &mut gate,
                now,
                control,
            )?;
        }
        #[cfg(target_os = "macos")]
        let (had_system, had_microphone) = (system.capture.is_some(), microphone.capture.is_some());
        // The toolbar's switches.
        system.follow(&control.system, &control.system_silent, audio_spec, control);
        microphone.follow(
            &control.microphone,
            &control.microphone_silent,
            audio_spec,
            control,
        );
        #[cfg(target_os = "macos")]
        {
            if had_system && system.capture.is_none() {
                mixer.end_track(Track::System, now)?;
            }
            if had_microphone && microphone.capture.is_none() {
                mixer.end_track(Track::Microphone, now)?;
            }
            collect_timed_audio(
                &mut mixer,
                &mut system,
                &mut microphone,
                &mut gate,
                now,
                control,
            )?;
            let watermark = now.checked_sub(AUDIO_REORDER_DELAY).unwrap_or(started);
            let target = mixer.frame_at(watermark)?.min(video_audio_frames(
                written,
                frame_time,
                audio_spec.sample_rate,
            )?);
            write_timed_audio(&mut mixer, &mut writer, target, audio_spec.sample_rate)?;
        }
        #[cfg(windows)]
        match (
            system.capture.is_some() && microphone.capture.is_some(),
            mixer.is_some(),
        ) {
            (true, false) => mixer = Some(Mixer::default()),
            (false, true) => mixer = None,
            _ => {}
        }
        follow_webcam(&mut webcam, control);
        if paused {
            #[cfg(target_os = "macos")]
            grabber
                .refresh_exclusions()
                .map_err(|error| error.to_string())?;
            #[cfg(windows)]
            {
                paused_at.get_or_insert(now);
                drain(&system.capture);
                drain(&microphone.capture);
                if let Some(mixer) = &mut mixer {
                    mixer.clear();
                }
            }
            system.silence.start(now);
            microphone.silence.start(now);
            control.system_silent.store(false, Ordering::Relaxed);
            control.microphone_silent.store(false, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(15));
            continue;
        }
        #[cfg(windows)]
        if let Some(at) = paused_at.take() {
            paused_for += now - at;
        }
        let t = ((now - started - paused_for).as_nanos() / 100) as i64;
        control.millis.store((t / 10_000) as u64, Ordering::Relaxed);
        // Audio first: it arrives continuously, and silence is filled in so
        // the soundtrack keeps pace with the frames.
        #[cfg(windows)]
        {
            let mut out: Vec<Vec<i16>> = Vec::new();
            if let Some(m) = &microphone.capture {
                for block in m.samples.try_iter() {
                    let mut block = block.samples;
                    microphone.silence.observe(&block, now);
                    if let Some(g) = gate.as_mut() {
                        g.process(&mut block);
                    }
                    match mixer.as_mut() {
                        Some(mx) => mx.push_b(&block),
                        None => out.push(block),
                    }
                }
            }
            if let Some(s) = &system.capture {
                for block in s.samples.try_iter() {
                    let block = block.samples;
                    system.silence.observe(&block, now);
                    match mixer.as_mut() {
                        Some(mx) => mx.push_a(&block),
                        None => out.push(block),
                    }
                }
            }
            if let Some(mx) = mixer.as_mut() {
                out.push(mx.take((rate / 10) as usize));
            }
            for block in out {
                let n = (block.len() / 2) as i64;
                writer.write_audio(
                    &block,
                    audio_frames * HNS_PER_SECOND / rate,
                    n * HNS_PER_SECOND / rate,
                )?;
                audio_frames += n;
            }
            let expected = t * rate / HNS_PER_SECOND;
            if expected - audio_frames > rate / 5 {
                let gap = (expected - audio_frames) as usize;
                writer.write_audio(
                    &media::silence(gap),
                    audio_frames * HNS_PER_SECOND / rate,
                    gap as i64 * HNS_PER_SECOND / rate,
                )?;
                audio_frames += gap as i64;
            }
            system.update_silence(now, &control.system_silent);
            microphone.update_silence(now, &control.microphone_silent);
        }
        #[cfg(target_os = "macos")]
        {
            system.update_silence(now, &control.system_silent);
            microphone.update_silence(now, &control.microphone_silent);
        }
        // A constant frame rate: frame n shows at n * frame_time. Repeat
        // frames when a grab is delayed, so video keeps the audio's pace.
        let due = t / frame_time;
        if due < written {
            std::thread::sleep(Duration::from_millis(
                (((written * frame_time) - t) / 10_000).clamp(1, 5) as u64,
            ));
            continue;
        }
        let pixels = grabber.grab().map_err(|e| e.to_string())?;
        frame.pixels.copy_from_slice(pixels);
        // Screen Draw's window is left out of captures; its marks go in here.
        screendraw::blend_into(&mut frame)?;
        control
            .annotations
            .lock()
            .map_err(|_| "recording annotations are unavailable")?
            .blend(&mut frame, (t / 10_000) as u64)?;
        let highlight = control.on(&control.highlight);
        let show_clicks = control.on(&control.clicks);
        let pointer = if highlight || show_clicks {
            #[cfg(target_os = "macos")]
            if let Some(fixture) = &pointer_fixture {
                cursor_point_in(mapping, fixture.position)?
            } else {
                cursor_in(mapping, layout)?
            }
            #[cfg(windows)]
            cursor_in(mapping, layout)?
        } else {
            None
        };
        if let (true, Some(p)) = (highlight, pointer) {
            vfx::highlight(&mut frame, p);
        }
        if control.on(&control.cursor) {
            #[cfg(target_os = "macos")]
            let system_cursor = pointer_fixture
                .is_none()
                .then(|| CursorSnapshot::take_in_layout(layout.display()))
                .flatten();
            #[cfg(target_os = "macos")]
            let cursor = pointer_fixture
                .as_ref()
                .map(|fixture| &fixture.cursor)
                .or(system_cursor.as_ref());
            #[cfg(windows)]
            let cursor = CursorSnapshot::take();
            if let Some(cursor) = cursor {
                if crate::workflow::cursor_intersects(&cursor, mapping) {
                    #[cfg(target_os = "macos")]
                    cursor
                        .draw_onto_mapped(&mut frame, mapping)
                        .map_err(|error| format!("could not draw the recorded cursor: {error}"))?;
                    #[cfg(windows)]
                    let _ = cursor.draw_onto(&mut frame, (rect.x, rect.y));
                }
            }
        }
        if show_clicks {
            let at = pointer.unwrap_or((-1000, -1000));
            #[cfg(target_os = "macos")]
            if pointer_fixture.is_some() {
                clicks.update_buttons(at, control.frames.load(Ordering::Acquire) == 0, false);
            } else {
                clicks.update(at);
            }
            #[cfg(windows)]
            clicks.update(at);
            clicks.draw(&mut frame);
        }
        if let Some(cam) = &webcam {
            if let Some((w, h, bgra)) = cam.latest.lock().unwrap().as_ref() {
                let corner = control.webcam_corner.lock().unwrap().clone();
                vfx::picture_in_picture(
                    &mut frame,
                    bgra,
                    (*w, *h),
                    &corner,
                    options.webcam_size,
                    control.on(&control.webcam_circle),
                );
            }
        }
        #[cfg(windows)]
        while written <= due {
            writer.write_frame(&frame.pixels, written * frame_time, frame_time)?;
            written += 1;
        }
        #[cfg(target_os = "macos")]
        {
            write_video_through_due(
                &mut writer,
                &previous_frame,
                &frame,
                &mut written,
                due,
                frame_time,
            )?;
            std::mem::swap(&mut previous_frame.pixels, &mut frame.pixels);
        }
        control.frames.fetch_add(1, Ordering::Release);
        if limit_started.is_none() {
            limit_started = Some(Instant::now());
        }
    }
    #[cfg(target_os = "macos")]
    {
        let stopped = Instant::now();
        let last_active = paused_at.unwrap_or(stopped);
        repeat_video_until(
            &mut writer,
            &previous_frame,
            &mut written,
            frame_time,
            (last_active - started).saturating_sub(paused_for),
        )?;
        mixer.set_paused(true, stopped)?;
        let wait_until = if system.capture.is_some() || microphone.capture.is_some() {
            paused_at.unwrap_or(stopped) + AUDIO_REORDER_DELAY
        } else {
            stopped
        };
        loop {
            let now = Instant::now();
            collect_timed_audio(
                &mut mixer,
                &mut system,
                &mut microphone,
                &mut gate,
                now,
                control,
            )?;
            if now >= wait_until {
                break;
            }
            std::thread::sleep((wait_until - now).min(Duration::from_millis(10)));
        }
        let target = video_audio_frames(written, frame_time, audio_spec.sample_rate)?;
        write_timed_audio(&mut mixer, &mut writer, target, audio_spec.sample_rate)?;
    }
    drop(fine_timer);
    drop(webcam);
    drop(system);
    drop(microphone);
    layout.validate().map_err(|error| error.to_string())?;
    writer.finish()?;
    partial.started = false;
    Ok(spec)
}

fn clock(ms: u64) -> String {
    format!("{}:{:02}", ms / 60_000, ms / 1000 % 60)
}

/// Counts down next to `rect`; false when cancelled.
fn countdown(rect: Rect, seconds: u32) -> bool {
    if seconds == 0 {
        return true;
    }
    let start = Instant::now();
    let total = Duration::from_secs(seconds as u64);
    let choice = hud::run_panel(
        rect,
        &format!("Recording starts in {seconds}…"),
        &["Start now", "Cancel"],
        100,
        move || {
            let left = total.saturating_sub(start.elapsed());
            if left.is_zero() {
                hud::finish_panel(Some(0));
                return None;
            }
            Some(format!(
                "Recording starts in {}…  (Esc cancels)",
                left.as_secs() + 1
            ))
        },
    );
    choice == Some(0)
}

/// The toolbar's state, as the app draws it.
fn toolbar_state(control: &Control, settings: &Settings) -> serde_json::Value {
    let on = |f: &AtomicBool| f.load(Ordering::Relaxed);
    let mut notes = control
        .audio
        .lock()
        .unwrap()
        .clone()
        .into_iter()
        .collect::<Vec<_>>();
    if on(&control.system_silent) {
        notes.push(
            "No sound detected from system audio. Check the selected output device.".to_owned(),
        );
    }
    if on(&control.microphone_silent) {
        notes.push(
            "No sound detected from the microphone. Check the selected input device.".to_owned(),
        );
    }
    json!({
        "millis": control.millis.load(Ordering::Relaxed),
        "frames": control.frames.load(Ordering::Relaxed),
        "paused": on(&control.paused),
        "cursor": on(&control.cursor),
        "highlight": on(&control.highlight),
        "clicks": on(&control.clicks),
        "webcam": on(&control.webcam),
        "webcam_corner": control.webcam_corner.lock().unwrap().clone(),
        "webcam_circle": on(&control.webcam_circle),
        "system": on(&control.system),
        "microphone": on(&control.microphone),
        "system_silent": on(&control.system_silent),
        "microphone_silent": on(&control.microphone_silent),
        "draw": screendraw::active(),
        "draw_offered": settings.screen_draw,
        "note": (!notes.is_empty()).then(|| notes.join("\n")),
        "keys": { "pause": key_names().0, "stop": key_names().1 },
    })
}

/// Handles a toolbar action that does not end the take.
fn toolbar_action(action: &str, control: &Control, draw_mapping: CaptureMapping, fade: Duration) {
    let flip = |f: &AtomicBool| {
        f.fetch_xor(true, Ordering::AcqRel);
    };
    match action.split_once(':').unwrap_or((action, "")) {
        ("pause", _) => flip(&control.paused),
        ("draw", _) if screendraw::active() => screendraw::hide(),
        ("draw", _) => {
            if !screendraw::show(draw_mapping, fade) {
                log("could not start Screen Draw: another drawing session is active");
            }
        }
        ("toggle", "cursor") => flip(&control.cursor),
        ("toggle", "highlight") => flip(&control.highlight),
        ("toggle", "clicks") => flip(&control.clicks),
        ("toggle", "webcam") => flip(&control.webcam),
        ("toggle", "system") => flip(&control.system),
        ("toggle", "microphone") => flip(&control.microphone),
        ("corner", c) if matches!(c, "top-left" | "top-right" | "bottom-left" | "bottom-right") => {
            *control.webcam_corner.lock().unwrap() = c.to_owned();
            control.webcam.store(true, Ordering::Relaxed);
        }
        ("shape", shape) => control
            .webcam_circle
            .store(shape != "rectangle", Ordering::Relaxed),
        _ => {}
    }
}

/// Records with the selected display geometry, retaining it through restart.
pub(crate) fn record_in_layout(
    rect: Rect,
    settings: &Settings,
    layout: CaptureLayout,
) -> Result<Option<PathBuf>, String> {
    layout.validate().map_err(|error| error.to_string())?;
    let library = Library::new(jiffshot_capture::config::library_root());
    let _border = Border::show(rect);
    if !countdown(rect, settings.countdown) {
        return Ok(None);
    }
    layout.validate().map_err(|error| error.to_string())?;
    let options = Options::from_settings(settings);
    let (mapping, _) = recording_geometry(rect, options.fps, &layout)?;
    #[cfg(target_os = "macos")]
    let draw_mapping = mapping;
    #[cfg(windows)]
    let draw_mapping = {
        let _ = mapping;
        CaptureMapping::identity(rect).ok_or("invalid Screen Draw dimensions")?
    };
    let fade = Duration::from_secs(settings.draw_fade_seconds as u64);
    let mut control = Arc::new(Control::new(&options));
    loop {
        layout.validate().map_err(|error| error.to_string())?;
        #[cfg(target_os = "macos")]
        let staging = library
            .stage_recording()
            .map_err(|error| error.to_string())?;
        #[cfg(target_os = "macos")]
        let path = staging.path().to_path_buf();
        #[cfg(windows)]
        let path = library.recording_path().map_err(|e| e.to_string())?;
        let (c, p, o) = (control.clone(), path.clone(), options.clone());
        let capture_layout = layout.clone();
        let worker = std::thread::spawn(move || {
            let result = record_to_in_layout(&p, rect, &o, &c, &capture_layout);
            if let Err(error) = &result {
                *c.error.lock().unwrap() = Some(error.clone());
                c.stop.store(true, Ordering::Release);
            }
            result
        });
        // The pause and stop keys work wherever the focus is.
        let (pause, stop) = check_keys();
        for (key, best, what) in [
            (pause, PAUSE_KEYS[0].2, "pauses"),
            (stop, STOP_KEYS[0].2, "stops"),
        ] {
            match key {
                Some((.., name)) if name != best => log(&format!("{best} is held by another app; {name} {what} the recording")),
                None => log(&format!("{best} and its alternatives are held by other apps; only the toolbar {what} the recording")),
                _ => {}
            }
        }
        let keys: Vec<(u32, u16, &str)> = [(pause, "pause"), (stop, "stop")]
            .into_iter()
            .filter_map(|(key, action)| key.map(|(modifiers, vk, _)| (modifiers, vk, action)))
            .collect();
        let ended = hud::run_toolbar(
            rect,
            &keys,
            100,
            || {
                if control.stop.load(Ordering::Acquire) {
                    // The recorder stopped by itself (an error).
                    hud::finish_panel(Some(0));
                }
                toolbar_state(&control, settings)
            },
            |action| match action {
                "stop" | "cancel" | "restart" => true,
                other => {
                    toolbar_action(other, &control, draw_mapping, fade);
                    false
                }
            },
        );
        screendraw::hide();
        control.stop.store(true, Ordering::Release);
        let result = worker
            .join()
            .map_err(|_| "the recorder crashed".to_owned())?;
        crate::tray::remember_recording(&control);
        let spec = result?;
        match ended.as_str() {
            "restart" => {
                #[cfg(target_os = "macos")]
                staging.discard().map_err(|error| error.to_string())?;
                #[cfg(windows)]
                discard_recording(&path)?;
                log("recording restarted");
                control = Arc::new(control.again());
                continue;
            }
            "cancel" => {
                #[cfg(target_os = "macos")]
                staging.discard().map_err(|error| error.to_string())?;
                #[cfg(windows)]
                discard_recording(&path)?;
                return Ok(None);
            }
            _ => {}
        }
        #[cfg(target_os = "macos")]
        let path = staging.publish().map_err(|error| error.to_string())?;
        log(&format!(
            "recorded {}x{} at {} fps: {} frames, {} ({})",
            spec.width,
            spec.height,
            spec.fps,
            control.frames.load(Ordering::Relaxed),
            clock(control.millis.load(Ordering::Relaxed)),
            path.display()
        ));
        return Ok(Some(path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_pointer_effects_retain_fractional_coordinates() {
        for scale in [1.0, 1.5, 2.0, 3.0] {
            let bounds = [-3.25_f64, 4.25, 16.75, 14.25].map(|edge| edge * scale);
            let edges = bounds.map(|edge| edge.round() as i32);
            let rect = Rect::from_ltrb(edges[0], edges[1], edges[2], edges[3]);
            let mapping = CaptureMapping::with_desktop_bounds(rect, 40, 20, bounds).unwrap();
            assert_eq!(
                cursor_point_in(mapping, (-1.51 * scale, 5.74 * scale)).unwrap(),
                Some((3, 2))
            );
            assert_eq!(
                cursor_point_in(mapping, (16.75 * scale, 5.74 * scale)).unwrap(),
                None
            );
            assert!(cursor_point_in(mapping, (f64::NAN, 5.74)).is_err());
        }
    }

    #[test]
    fn recording_lease_prevents_competing_owners_and_releases() {
        static TEST_ACTIVE: AtomicBool = AtomicBool::new(false);
        let lease = RecordingLease::acquire_from(&TEST_ACTIVE).expect("first recorder");
        assert!(RecordingLease::acquire_from(&TEST_ACTIVE).is_err());
        drop(lease);
        assert!(RecordingLease::acquire_from(&TEST_ACTIVE).is_ok());
    }

    #[test]
    fn live_annotations_render_expire_and_clear() {
        use jiffshot_doc::object::ShapeKind;
        use jiffshot_doc::Box2;

        let control = Control::default();
        let kind = Kind::Shape {
            rect: Box2::from_xywh(10.0, 10.0, 30.0, 20.0),
            shape: ShapeKind::Rectangle,
        };
        let style = Style {
            fill: Rgba::RED,
            width: 0.0,
            ..Style::default()
        };
        assert!(add_annotation(&control, kind.clone(), style.clone(), Duration::ZERO).is_err());
        control.annotations.lock().unwrap().size = Some((80, 60));
        assert_eq!(
            add_annotation(
                &control,
                kind.clone(),
                style.clone(),
                Duration::from_millis(100)
            )
            .unwrap(),
            1
        );
        let mut frame = Frame::new(80, 60);
        control
            .annotations
            .lock()
            .unwrap()
            .blend(&mut frame, 99)
            .unwrap();
        assert_eq!(
            frame.rgb(15, 15),
            Some((Rgba::RED.r, Rgba::RED.g, Rgba::RED.b))
        );
        let mut expired = Frame::new(80, 60);
        control
            .annotations
            .lock()
            .unwrap()
            .blend(&mut expired, 100)
            .unwrap();
        assert_eq!(expired.rgb(15, 15), Some((0, 0, 0)));
        add_annotation(&control, kind, style, Duration::ZERO).unwrap();
        let mut persistent = Frame::new(80, 60);
        control
            .annotations
            .lock()
            .unwrap()
            .blend(&mut persistent, 100_000)
            .unwrap();
        assert_eq!(
            persistent.rgb(15, 15),
            Some((Rgba::RED.r, Rgba::RED.g, Rgba::RED.b))
        );
        clear_annotations(&control).unwrap();
        let mut cleared = Frame::new(80, 60);
        control
            .annotations
            .lock()
            .unwrap()
            .blend(&mut cleared, 100_000)
            .unwrap();
        assert_eq!(cleared.rgb(15, 15), Some((0, 0, 0)));
    }

    #[test]
    fn live_magnification_samples_each_current_frame() {
        use jiffshot_doc::object::Object;
        use jiffshot_doc::{Box2, Pt};

        let control = Control::default();
        control.annotations.lock().unwrap().size = Some((100, 60));
        let kind = Kind::Group {
            objects: vec![Object::new(
                1,
                Kind::Magnify {
                    rect: Box2::from_xywh(10.0, 10.0, 20.0, 20.0),
                    zoom: 2.0,
                    ellipse: false,
                    offset: Pt::new(40.0, 0.0),
                },
                Style {
                    width: 0.0,
                    ..Style::default()
                },
            )],
        };
        add_annotation(&control, kind, Style::default(), Duration::from_millis(100)).unwrap();
        for (now, source) in [(0, Rgba::RED), (50, Rgba::BLUE), (100, Rgba::RED)] {
            let mut frame = Frame::new(100, 60);
            for y in 10..30 {
                for x in 10..30 {
                    let at = y * frame.stride() + x * 4;
                    frame.pixels[at..at + 4].copy_from_slice(&[source.b, source.g, source.r, 0]);
                }
            }
            control
                .annotations
                .lock()
                .unwrap()
                .blend(&mut frame, now)
                .unwrap();
            let expected = if now < 100 {
                (source.r, source.g, source.b)
            } else {
                (0, 0, 0)
            };
            assert_eq!(frame.rgb(60, 20), Some(expected));
            assert_eq!(frame.rgb(5, 5), Some((0, 0, 0)));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn staged_recording_round_trips_through_the_media_writer() {
        let name = format!(
            "jiffshot-staged-video-{}-{}-\u{00e9}\u{1f3a5}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(name);
        std::fs::create_dir(&root).unwrap();
        let library = Library::new(root.clone());
        let staging = library.stage_recording().unwrap();
        let directory = staging.path().parent().unwrap().to_path_buf();
        let spec = VideoSpec::for_size(64, 64, 15);
        let mut writer = Writer::create(staging.path(), spec, None).unwrap();
        let mut frame = Frame::new(spec.width, spec.height);
        for pixel in frame.pixels.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[0, 0, 220, 255]);
        }
        let duration = HNS_PER_SECOND / i64::from(spec.fps);
        writer.write_frame(&frame.pixels, 0, duration).unwrap();
        writer
            .write_frame(&frame.pixels, duration, duration)
            .unwrap();
        writer.finish().unwrap();

        let path = staging.publish().unwrap();
        assert!(!directory.exists());
        let mut reader = Reader::open(&path).unwrap();
        let decoded = reader.frame_at(duration / 2).unwrap().unwrap();
        assert_eq!((decoded.width, decoded.height), (spec.width, spec.height));
        assert!(decoded.bgra[2] > 150);
        drop(reader);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incomplete_recordings_are_removed_but_finished_files_are_kept() {
        let name = format!(
            "jiffshot-partial-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let directory = std::env::temp_dir().join(name);
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.mp4");
        std::fs::write(&path, b"incomplete").unwrap();
        {
            let _partial = PartialRecording {
                path: &path,
                started: true,
            };
        }
        assert!(!path.exists());

        std::fs::write(&path, b"finished").unwrap();
        {
            let _partial = PartialRecording {
                path: &path,
                started: false,
            };
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"finished");
        discard_recording(&path).unwrap();
        discard_recording(&path).unwrap();
        assert!(discard_recording(&directory).is_err());
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn stale_recording_layout_does_not_touch_the_destination() {
        let name = format!(
            "jiffshot-stale-recording-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let directory = std::env::temp_dir().join(name);
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("recording.mp4");
        std::fs::write(&path, b"existing").unwrap();
        let layout = CaptureLayout::current().unwrap().invalidated_for_test();
        let options = Options::audio_only(15, "none");
        let error = record_to_in_layout(
            &path,
            Rect::new(0, 0, 64, 64),
            &options,
            &Control::new(&options),
            &layout,
        )
        .unwrap_err();
        assert!(error.contains("display configuration changed"));
        assert_eq!(std::fs::read(&path).unwrap(), b"existing");
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn recording_geometry_preserves_desktop_area_and_uses_even_output_pixels() {
        let layout = CaptureLayout::current().unwrap();
        let monitor = layout
            .monitors()
            .into_iter()
            .find(|monitor| monitor.primary)
            .unwrap();
        let rect = Rect::new(monitor.rect.x, monitor.rect.y, 202, 150);
        let layout = layout.with_output_size(rect, 101, 75).unwrap();
        let (mapping, spec) = recording_geometry(rect, 120, &layout).unwrap();
        assert_eq!(mapping.rect(), rect);
        assert_eq!(mapping.size(), (100, 74));
        assert_eq!((spec.width, spec.height, spec.fps), (100, 74, 60));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn timed_audio_and_repeated_video_round_trip_through_mp4() {
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(self.0.join("timed.mp4"));
                let _ = std::fs::remove_dir(&self.0);
            }
        }

        let name = format!(
            "jiffshot-timed-audio-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let scratch = Scratch(std::env::temp_dir().join(name));
        std::fs::create_dir(&scratch.0).unwrap();
        let path = scratch.0.join("timed.mp4");
        let spec = VideoSpec::for_size(64, 64, 10);
        let mut writer = Writer::create(&path, spec, Some(DEFAULT_AUDIO)).unwrap();
        let frame = Frame::new(spec.width, spec.height);
        let frame_time = HNS_PER_SECOND / i64::from(spec.fps);
        writer.write_frame(&frame.pixels, 0, frame_time).unwrap();
        let mut written = 1;
        repeat_video_until(
            &mut writer,
            &frame,
            &mut written,
            frame_time,
            Duration::from_millis(1_250),
        )
        .unwrap();
        assert_eq!(written, 13);

        let start = Instant::now();
        let mut mixer = TimedMixer::new(start, DEFAULT_AUDIO.sample_rate);
        mixer
            .set_paused(true, start + Duration::from_millis(800))
            .unwrap();
        mixer
            .set_paused(false, start + Duration::from_millis(1_300))
            .unwrap();
        let rate = DEFAULT_AUDIO.sample_rate as usize;
        let tone = |wall_frame: usize| {
            let value = ((wall_frame as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin()
                * 6_000.0) as i16;
            [value, value]
        };
        for block in (2..8).rev() {
            let first = block * rate / 10;
            let pcm: Vec<_> = (first..first + rate / 10).flat_map(tone).collect();
            mixer
                .push(
                    Track::System,
                    start + Duration::from_millis(block as u64 * 100),
                    &pcm,
                )
                .unwrap();
        }
        for block in (4..18).rev() {
            let first = block * rate / 10;
            let pcm: Vec<_> = (first..first + rate / 10).flat_map(tone).collect();
            mixer
                .push(
                    Track::Microphone,
                    start + Duration::from_millis(block as u64 * 100),
                    &pcm,
                )
                .unwrap();
        }
        let target = video_audio_frames(written, frame_time, DEFAULT_AUDIO.sample_rate).unwrap();
        write_timed_audio(&mut mixer, &mut writer, target, DEFAULT_AUDIO.sample_rate).unwrap();
        writer.finish().unwrap();

        let mut reader = Reader::open(&path).unwrap();
        assert_eq!(reader.audio, Some(DEFAULT_AUDIO));
        reader.select_video(false).unwrap();
        reader.select_audio(true).unwrap();
        reader.seek(0).unwrap();
        let mut decoded = vec![0i16; target as usize * 2];
        while let Some((time, pcm)) = reader.next_audio().unwrap() {
            let first = ((i128::from(time) * rate as i128 + i128::from(HNS_PER_SECOND) / 2)
                / i128::from(HNS_PER_SECOND))
            .max(0) as usize
                * 2;
            if first >= decoded.len() {
                continue;
            }
            let count = pcm.len().min(decoded.len().saturating_sub(first));
            decoded[first..first + count].copy_from_slice(&pcm[..count]);
        }
        let rms = |from: usize, to: usize| {
            let samples = &decoded[from * rate / 1_000 * 2..to * rate / 1_000 * 2];
            (samples
                .iter()
                .map(|sample| f64::from(*sample).powi(2))
                .sum::<f64>()
                / samples.len() as f64)
                .sqrt()
        };
        let silent = rms(50, 150);
        let system = rms(250, 350);
        let mixed = rms(500, 700);
        let resumed = rms(900, 1_100);
        assert!(silent < 200.0, "silent interval RMS {silent}");
        assert!(system > 2_000.0, "system interval RMS {system}");
        assert!(
            mixed > system * 1.6,
            "mixed RMS {mixed}, system RMS {system}"
        );
        assert!(
            resumed > 2_000.0 && resumed < mixed * 0.8,
            "resumed RMS {resumed}, mixed RMS {mixed}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn delayed_grabs_repeat_the_previous_video_frame() {
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(self.0.join("delayed.mp4"));
                let _ = std::fs::remove_dir(&self.0);
            }
        }

        let name = format!(
            "jiffshot-delayed-video-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let scratch = Scratch(std::env::temp_dir().join(name));
        std::fs::create_dir(&scratch.0).unwrap();
        let path = scratch.0.join("delayed.mp4");
        let spec = VideoSpec::for_size(64, 64, 10);
        let mut writer = Writer::create(&path, spec, None).unwrap();
        let frame_time = HNS_PER_SECOND / i64::from(spec.fps);
        let mut previous = Frame::new(spec.width, spec.height);
        let mut current = Frame::new(spec.width, spec.height);
        for pixel in current.pixels.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[0, 0, 240, 255]);
        }
        let mut written = 0;
        write_video_through_due(
            &mut writer,
            &previous,
            &current,
            &mut written,
            2,
            frame_time,
        )
        .unwrap();
        std::mem::swap(&mut previous.pixels, &mut current.pixels);
        for pixel in current.pixels.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[240, 0, 0, 255]);
        }
        write_video_through_due(
            &mut writer,
            &previous,
            &current,
            &mut written,
            5,
            frame_time,
        )
        .unwrap();
        std::mem::swap(&mut previous.pixels, &mut current.pixels);
        repeat_video_until(
            &mut writer,
            &previous,
            &mut written,
            frame_time,
            Duration::from_millis(600),
        )
        .unwrap();
        assert_eq!(written, 7);
        writer.finish().unwrap();

        let mut reader = Reader::open(&path).unwrap();
        let mut colors = Vec::new();
        while let Some(frame) = reader.next_video().unwrap() {
            let center = ((frame.height / 2 * frame.width + frame.width / 2) * 4) as usize;
            colors.push((frame.bgra[center + 2], frame.bgra[center]));
        }
        assert_eq!(colors.len(), 7);
        for (index, (red, blue)) in colors.into_iter().enumerate() {
            if index < 5 {
                assert!(
                    red > 180 && blue < 60,
                    "frame {index} should remain red, got ({red}, {blue})"
                );
            } else {
                assert!(
                    blue > 180 && red < 60,
                    "frame {index} should be blue, got ({red}, {blue})"
                );
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires an active display and Screen Recording permission"]
    fn macos_screen_recording_round_trips() {
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(self.0.join("recording.mp4"));
                let _ = std::fs::remove_dir(&self.0);
            }
        }

        let name = format!(
            "jiffshot-recording-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let scratch = Scratch(std::env::temp_dir().join(name));
        std::fs::create_dir(&scratch.0).unwrap();
        let path = scratch.0.join("recording.mp4");
        let monitor = jiffshot_capture::screen::monitors()
            .into_iter()
            .find(|monitor| monitor.primary)
            .expect("a primary display");
        let width = monitor.rect.width.min(640) & !1;
        let height = monitor.rect.height.min(480) & !1;
        assert!(width >= 2 && height >= 2);
        let rect = Rect::new(monitor.rect.x, monitor.rect.y, width, height);
        let audio = std::env::var("JIFFSHOT_TEST_RECORD_AUDIO").unwrap_or_else(|_| "none".into());
        let mut options = Options::audio_only(15, &audio);
        options.cursor = false;
        let control = Arc::new(Control::new(&options));
        let worker_control = control.clone();
        let worker_path = path.clone();
        let worker =
            std::thread::spawn(move || record_to(&worker_path, rect, &options, &worker_control));
        let target_frames = if std::env::var_os("JIFFSHOT_TEST_REQUIRE_AUDIO_SIGNAL").is_some() {
            45
        } else {
            10
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        while control.frames.load(Ordering::Acquire) < target_frames
            && !worker.is_finished()
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        control.stop.store(true, Ordering::Release);
        let spec = worker
            .join()
            .expect("recording thread panicked")
            .expect("screen recording failed");
        assert!(
            control.frames.load(Ordering::Acquire) >= target_frames,
            "too few captured frames"
        );
        assert!(
            control.audio.lock().unwrap().is_none(),
            "audio capture reported a problem"
        );
        let mut reader = media::Reader::open(&path).expect("recorded video must open");
        assert_eq!((reader.width, reader.height), (spec.width, spec.height));
        assert!(reader.duration > 0);
        assert!(reader
            .frame_at(reader.duration / 2)
            .expect("recorded frame must decode")
            .is_some());
        if audio != "none" {
            assert!(reader.audio.is_some(), "recorded audio track is missing");
            reader.select_video(false).unwrap();
            reader.select_audio(true).unwrap();
            reader.seek(0).unwrap();
            let mut peak = 0i32;
            let mut count = 0;
            while let Some((_, block)) = reader.next_audio().expect("recorded audio must decode") {
                count += block.len();
                peak = peak.max(
                    block
                        .iter()
                        .map(|&sample| (sample as i32).abs())
                        .max()
                        .unwrap_or(0),
                );
            }
            assert!(count > 0, "recorded audio has no samples");
            if std::env::var_os("JIFFSHOT_TEST_REQUIRE_AUDIO_SIGNAL").is_some() {
                assert!(peak > 0, "recorded audio is silent");
            }
        }
    }

    #[test]
    fn silence_warning_waits_five_seconds_and_clears_on_signal() {
        let start = Instant::now();
        let mut monitor = SilenceMonitor::default();
        monitor.start(start);

        assert!(!monitor.is_silent(start + SILENCE_WARNING_AFTER - Duration::from_millis(1)));
        assert!(monitor.is_silent(start + SILENCE_WARNING_AFTER));

        monitor.observe(&[1000; 960], start + SILENCE_WARNING_AFTER);
        assert!(!monitor.is_silent(start + SILENCE_WARNING_AFTER));
        assert!(monitor.is_silent(start + SILENCE_WARNING_AFTER * 2));
    }

    #[test]
    fn silence_monitor_ignores_levels_below_the_signal_threshold() {
        let start = Instant::now();
        let mut monitor = SilenceMonitor::default();
        monitor.start(start);

        monitor.observe(
            &[399; 960],
            start + SILENCE_WARNING_AFTER - Duration::from_millis(1),
        );
        assert!(monitor.is_silent(start + SILENCE_WARNING_AFTER));

        monitor.observe(&[400; 960], start + SILENCE_WARNING_AFTER);
        assert!(!monitor.is_silent(start + SILENCE_WARNING_AFTER));
    }

    #[test]
    fn silence_monitor_reset_starts_a_fresh_warning_window() {
        let start = Instant::now();
        let mut monitor = SilenceMonitor::default();
        monitor.start(start);
        monitor.observe(&[1000; 960], start + Duration::from_secs(4));

        monitor.reset();
        assert!(!monitor.is_silent(start + Duration::from_secs(30)));

        monitor.start(start + Duration::from_secs(30));
        assert!(!monitor.is_silent(start + Duration::from_secs(35) - Duration::from_millis(1)));
        assert!(monitor.is_silent(start + Duration::from_secs(35)));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn stopped_audio_source_is_disabled_and_reported() {
        let control = Control::default();
        control.system.store(true, Ordering::Relaxed);
        control.system_silent.store(true, Ordering::Relaxed);
        let mut source = Source::new(false);
        source.silence.start(Instant::now());

        source.stopped(
            "device disconnected",
            &control.system,
            &control.system_silent,
            &control,
        );

        assert!(!control.system.load(Ordering::Relaxed));
        assert!(!control.system_silent.load(Ordering::Relaxed));
        assert!(source.silence.last_signal.is_none());
        assert_eq!(
            control.audio.lock().unwrap().as_deref(),
            Some("system audio stopped: device disconnected")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn late_audio_is_reported_without_hiding_other_source_notes() {
        let control = Control::default();
        control.note(SourceKind::Webcam, Some("webcam stopped".into()));
        let mut source = Source::new(true);
        source.report_late_audio(2_399, 48_000, &control);
        assert_eq!(
            control.audio.lock().unwrap().as_deref(),
            Some("webcam stopped")
        );
        source.report_late_audio(1, 48_000, &control);
        assert_eq!(
            control.audio.lock().unwrap().as_deref(),
            Some("microphone: some audio arrived too late to include in the recording webcam stopped")
        );
        source.report_late_audio(10_000, 48_000, &control);
        assert!(source.late_reported);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_failed_audio_source_can_be_retried_during_the_same_take() {
        let control = Control::default();
        let mut source = Source::new(true);
        let attempts = Arc::new(AtomicU64::new(0));
        for expected in 1..=2 {
            control.microphone.store(true, Ordering::Relaxed);
            let deadline = Instant::now() + Duration::from_secs(2);
            while control.microphone.load(Ordering::Relaxed) && Instant::now() < deadline {
                let attempts = attempts.clone();
                source.follow_macos_with(
                    &control.microphone,
                    &control.microphone_silent,
                    DEFAULT_AUDIO,
                    &control,
                    move |microphone, _| {
                        assert!(microphone);
                        attempts.fetch_add(1, Ordering::Relaxed);
                        Err("device unavailable".into())
                    },
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(attempts.load(Ordering::Relaxed), expected);
            assert!(!control.microphone.load(Ordering::Relaxed));
            assert_eq!(
                control.audio.lock().unwrap().as_deref(),
                Some("no microphone: device unavailable")
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pending_audio_start_is_cancelled_without_blocking_the_recording() {
        let control = Control::default();
        let mut source = Source::new(true);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (cancelled_tx, cancelled_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        control.microphone.store(true, Ordering::Relaxed);
        source.follow_macos_with(
            &control.microphone,
            &control.microphone_silent,
            DEFAULT_AUDIO,
            &control,
            move |_, cancel| {
                started_tx.send(()).unwrap();
                while !cancel.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                cancelled_tx.send(()).unwrap();
                let _ = release_rx.recv();
                Err("cancelled".into())
            },
        );
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        control.microphone.store(false, Ordering::Relaxed);
        source.follow_macos_with(
            &control.microphone,
            &control.microphone_silent,
            DEFAULT_AUDIO,
            &control,
            |_, _| unreachable!(),
        );
        cancelled_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(source.pending.is_some());

        control.microphone.store(true, Ordering::Relaxed);
        source.follow_macos_with(
            &control.microphone,
            &control.microphone_silent,
            DEFAULT_AUDIO,
            &control,
            |_, _| unreachable!(),
        );
        assert!(source.pending.is_some());
        control.microphone.store(false, Ordering::Relaxed);
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while source
            .pending
            .as_ref()
            .is_some_and(|pending| !pending.finished())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        source.follow_macos_with(
            &control.microphone,
            &control.microphone_silent,
            DEFAULT_AUDIO,
            &control,
            |_, _| unreachable!(),
        );
        assert!(source.pending.is_none());
        assert!(control.audio.lock().unwrap().is_none());
    }

    #[test]
    fn webcam_startup_is_cancellable_without_blocking_recording() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let observed_cancel = cancelled.clone();
        let webcam = Webcam::start_with(
            move |stop| {
                started_tx.send(()).unwrap();
                while !stop.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                observed_cancel.store(true, Ordering::Release);
                Err::<(), _>("cancelled".into())
            },
            |_: &mut ()| unreachable!(),
        );
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(webcam);
        assert!(cancelled.load(Ordering::Acquire));
    }

    #[test]
    fn webcam_reader_failure_disables_the_camera_and_reports_it() {
        let mut first = true;
        let webcam = Webcam::start_with(
            |_| Ok(()),
            move |_: &mut ()| {
                if std::mem::take(&mut first) {
                    Ok(Some(VideoFrame {
                        time: 0,
                        width: 1,
                        height: 1,
                        bgra: vec![0; 4],
                    }))
                } else {
                    Err("device disconnected".into())
                }
            },
        );
        let latest = webcam.latest.clone();
        let mut webcam = Some(webcam);
        let control = Control::default();
        control.webcam.store(true, Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(2);
        while control.webcam.load(Ordering::Relaxed) && Instant::now() < deadline {
            follow_webcam(&mut webcam, &control);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            latest.lock().unwrap().is_some(),
            "the test camera never delivered its frame"
        );
        assert!(!control.webcam.load(Ordering::Relaxed));
        assert!(webcam.is_none());
        assert_eq!(
            control.audio.lock().unwrap().as_deref(),
            Some("webcam stopped: device disconnected")
        );
    }

    #[test]
    fn webcam_start_failure_is_reported_without_opening_a_device() {
        let mut webcam = Some(Webcam::start_with(
            |_| Err::<(), _>("permission denied".into()),
            |_: &mut ()| unreachable!(),
        ));
        let control = Control::default();
        control.webcam.store(true, Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(2);
        while control.webcam.load(Ordering::Relaxed) && Instant::now() < deadline {
            follow_webcam(&mut webcam, &control);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!control.webcam.load(Ordering::Relaxed));
        assert!(webcam.is_none());
        assert_eq!(
            control.audio.lock().unwrap().as_deref(),
            Some("webcam stopped: permission denied")
        );
    }

    #[test]
    fn source_notes_are_preserved_independently_and_clear_after_recovery() {
        let control = Control::default();
        control.note(SourceKind::System, Some("system audio stopped".into()));
        control.note(SourceKind::Microphone, Some("microphone stopped".into()));
        control.note(SourceKind::Webcam, Some("webcam stopped".into()));
        assert_eq!(
            control.audio.lock().unwrap().as_deref(),
            Some("system audio stopped microphone stopped webcam stopped")
        );
        control.note(SourceKind::Microphone, None);
        assert_eq!(
            control.audio.lock().unwrap().as_deref(),
            Some("system audio stopped webcam stopped")
        );
        control.note(SourceKind::System, None);
        control.note(SourceKind::Webcam, None);
        assert!(control.audio.lock().unwrap().is_none());
    }

    #[test]
    fn recovered_webcam_clears_only_its_own_note() {
        let webcam = Webcam::start_with(
            |stop| Ok(stop),
            |stop: &mut Arc<AtomicBool>| {
                while !stop.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(None)
            },
        );
        let mut webcam = Some(webcam);
        let control = Control::default();
        control.note(SourceKind::Webcam, Some("webcam stopped".into()));
        control.note(SourceKind::System, Some("system audio stopped".into()));
        control.webcam.store(true, Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(2);
        while control.audio.lock().unwrap().as_deref() != Some("system audio stopped")
            && Instant::now() < deadline
        {
            follow_webcam(&mut webcam, &control);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            control.audio.lock().unwrap().as_deref(),
            Some("system audio stopped")
        );
        assert!(control.webcam.load(Ordering::Relaxed));
        drop(webcam);
    }

    #[test]
    fn toolbar_note_names_silent_sources_without_hiding_capture_notes() {
        let control = Control::default();
        control.system_silent.store(true, Ordering::Relaxed);
        control.microphone_silent.store(true, Ordering::Relaxed);
        control.note(
            SourceKind::Webcam,
            Some("no webcam: unavailable".to_owned()),
        );

        let note = toolbar_state(&control, &Settings::default())["note"]
            .as_str()
            .unwrap()
            .to_owned();

        assert!(note.contains("no webcam: unavailable"), "{note}");
        assert!(
            note.contains("No sound detected from system audio."),
            "{note}"
        );
        assert!(
            note.contains("No sound detected from the microphone."),
            "{note}"
        );
        assert!(
            toolbar_state(&control, &Settings::default())["system_silent"]
                .as_bool()
                .unwrap()
        );
        assert!(
            toolbar_state(&control, &Settings::default())["microphone_silent"]
                .as_bool()
                .unwrap()
        );
    }

    #[test]
    fn toolbar_state_identifies_only_the_silent_source() {
        let control = Control::default();
        control.microphone_silent.store(true, Ordering::Relaxed);

        let state = toolbar_state(&control, &Settings::default());

        assert_eq!(state["system_silent"], false);
        assert_eq!(state["microphone_silent"], true);
        assert!(state["note"].as_str().unwrap().contains("microphone"));
        assert!(!state["note"].as_str().unwrap().contains("system audio"));
    }
}
