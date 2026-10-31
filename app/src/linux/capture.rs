//! Permission-aware monitor capture through the XDG ScreenCast portal and PipeWire.

use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType, Streams};
use ashpd::desktop::{PersistMode, Response, Session};
use futures_util::StreamExt;
use gst::prelude::*;
use gstreamer as gst;
use std::collections::HashMap;
use std::future::Future;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Maximum number of saved images and recordings shown in the recent-captures row.
const RECENT_CAPTURE_LIMIT: usize = 8;
static NEXT_PORTAL_REQUEST: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CaptureCursor {
    Included,
    Hidden,
}

impl CaptureCursor {
    pub(super) fn from_included(included: bool) -> Self {
        if included {
            Self::Included
        } else {
            Self::Hidden
        }
    }

    pub(super) fn included(self) -> bool {
        matches!(self, Self::Included)
    }

    pub(super) fn portal_mode(self) -> CursorMode {
        match self {
            Self::Included => CursorMode::Embedded,
            Self::Hidden => CursorMode::Hidden,
        }
    }
}

pub(super) async fn capture_monitor(
    cursor: CaptureCursor,
) -> Result<jiffshot_doc::Document, String> {
    let stream = open_portal_stream(cursor.portal_mode()).await?;
    let result = capture_stream_document(&stream);
    stream.finish(result).await
}

pub(super) async fn capture_monitor_after_delay(
    cursor: CaptureCursor,
    delay: Duration,
    cancel: &Receiver<()>,
    on_wait: impl FnOnce(),
    on_wait_finished: impl FnOnce(),
) -> Result<Option<jiffshot_doc::Document>, String> {
    let stream = open_portal_stream(cursor.portal_mode()).await?;
    let outcome = wait_for_capture_delay(delay, cancel, on_wait);
    on_wait_finished();
    let result = match outcome {
        Ok(CaptureDelayOutcome::Elapsed) => capture_stream_document(&stream).map(Some),
        Ok(CaptureDelayOutcome::Cancelled) => Ok(None),
        Err(error) => Err(error),
    };
    stream.finish(result).await
}

#[derive(Debug, Eq, PartialEq)]
enum CaptureDelayOutcome {
    Elapsed,
    Cancelled,
}

fn wait_for_capture_delay(
    delay: Duration,
    cancel: &Receiver<()>,
    on_wait: impl FnOnce(),
) -> Result<CaptureDelayOutcome, String> {
    match cancel.try_recv() {
        Ok(()) => return Ok(CaptureDelayOutcome::Cancelled),
        Err(TryRecvError::Disconnected) => {
            return Err("the delayed capture cancellation channel closed".to_owned())
        }
        Err(TryRecvError::Empty) => {}
    }
    on_wait();
    match cancel.recv_timeout(delay) {
        Ok(()) => Ok(CaptureDelayOutcome::Cancelled),
        Err(RecvTimeoutError::Timeout) => Ok(CaptureDelayOutcome::Elapsed),
        Err(RecvTimeoutError::Disconnected) => {
            Err("the delayed capture cancellation channel closed".to_owned())
        }
    }
}

fn capture_stream_document(stream: &PortalStream) -> Result<jiffshot_doc::Document, String> {
    gst::init().map_err(|error| format!("could not initialize GStreamer: {error}"))?;
    let directory =
        tempfile::tempdir().map_err(|error| format!("could not create a capture file: {error}"))?;
    let path = directory.path().join("capture.png");
    save_pipewire_frame(stream.remote.as_raw_fd(), stream.node_id, &path)?;
    jiffshot_doc::io::open(&path).map_err(|error| format!("could not read captured image: {error}"))
}

pub(super) struct PortalStream {
    _portal: Screencast<'static>,
    session: Session<'static, Screencast<'static>>,
    pub(super) remote: OwnedFd,
    pub(super) node_id: u32,
}

impl PortalStream {
    pub(super) async fn finish<T>(self, result: Result<T, String>) -> Result<T, String> {
        close_portal_session(&self.session, result).await
    }
}

async fn close_portal_session<T>(
    session: &Session<'static, Screencast<'static>>,
    result: Result<T, String>,
) -> Result<T, String> {
    let closed = session
        .close()
        .await
        .map_err(|error| format!("could not close screen capture session: {error}"));
    crate::linux::portal::combine_session_results(result, closed)
}

pub(super) async fn open_portal_stream(cursor: CursorMode) -> Result<PortalStream, String> {
    let portal = Screencast::new()
        .await
        .map_err(|error| format!("could not connect to the screen capture portal: {error}"))?;
    let session = portal
        .create_session()
        .await
        .map_err(|error| format!("could not create a screen capture session: {error}"))?;
    let result = async {
        portal
            .select_sources(
                &session,
                cursor,
                SourceType::Monitor | SourceType::Window,
                false,
                None,
                PersistMode::DoNot,
            )
            .await
            .map_err(|error| format!("could not select a screen capture source: {error}"))?
            .response()
            .map_err(|error| format!("screen capture source selection was rejected: {error}"))?;
        let streams = portal
            .start(&session, None)
            .await
            .map_err(|error| format!("could not start screen capture: {error}"))?
            .response()
            .map_err(|error| format!("screen capture was cancelled or rejected: {error}"))?;
        let node_id = streams
            .streams()
            .first()
            .ok_or_else(|| "the screen capture portal returned no streams".to_owned())?
            .pipe_wire_node_id();
        let remote = portal
            .open_pipe_wire_remote(&session)
            .await
            .map_err(|error| {
                format!("could not connect to the PipeWire capture stream: {error}")
            })?;
        Ok((remote, node_id))
    }
    .await;

    match result {
        Ok((remote, node_id)) => Ok(PortalStream {
            _portal: portal,
            session,
            remote,
            node_id,
        }),
        Err(error) => close_portal_session(&session, Err(error)).await,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingAudio {
    Disabled,
    Microphone,
    System,
    SystemAndMicrophone,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum RecordingFrameRate {
    #[default]
    Source,
    Fps15,
    Fps24,
    Fps30,
    Fps60,
}

impl RecordingFrameRate {
    pub(super) const ALL: [Self; 5] = [
        Self::Source,
        Self::Fps15,
        Self::Fps24,
        Self::Fps30,
        Self::Fps60,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Source => "Source",
            Self::Fps15 => "15 fps",
            Self::Fps24 => "24 fps",
            Self::Fps30 => "30 fps",
            Self::Fps60 => "60 fps",
        }
    }

    pub(super) fn stored_name(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Fps15 => "15",
            Self::Fps24 => "24",
            Self::Fps30 => "30",
            Self::Fps60 => "60",
        }
    }

    pub(super) fn from_stored_name(name: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|rate| rate.stored_name() == name)
            .ok_or_else(|| format!("unknown recording frame rate preference: {name}"))
    }

    pub(super) fn index(self) -> i32 {
        match self {
            Self::Source => 0,
            Self::Fps15 => 1,
            Self::Fps24 => 2,
            Self::Fps30 => 3,
            Self::Fps60 => 4,
        }
    }

    pub(super) fn from_index(index: i32) -> Result<Self, String> {
        usize::try_from(index)
            .ok()
            .and_then(|index| Self::ALL.get(index))
            .copied()
            .ok_or_else(|| "select a recording frame rate".to_owned())
    }

    fn frames_per_second(self) -> Option<i32> {
        match self {
            Self::Source => None,
            Self::Fps15 => Some(15),
            Self::Fps24 => Some(24),
            Self::Fps30 => Some(30),
            Self::Fps60 => Some(60),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RecordingWebcam {
    Disabled,
    Enabled(WebcamConfiguration),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WebcamCorner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl WebcamCorner {
    pub(super) const ALL: [Self; 4] = [
        Self::TopLeft,
        Self::TopRight,
        Self::BottomLeft,
        Self::BottomRight,
    ];

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::TopLeft => "top-left",
            Self::TopRight => "top-right",
            Self::BottomLeft => "bottom-left",
            Self::BottomRight => "bottom-right",
        }
    }

    pub(super) fn from_name(name: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|corner| corner.name() == name)
            .ok_or_else(|| format!("unknown webcam corner: {name}"))
    }

    pub(super) fn index(self) -> i32 {
        match self {
            Self::TopLeft => 0,
            Self::TopRight => 1,
            Self::BottomLeft => 2,
            Self::BottomRight => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WebcamShape {
    Rectangle,
    Circle,
}

impl WebcamShape {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Rectangle => "rectangle",
            Self::Circle => "circle",
        }
    }

    pub(super) fn from_name(name: &str) -> Result<Self, String> {
        match name {
            "rectangle" => Ok(Self::Rectangle),
            "circle" => Ok(Self::Circle),
            _ => Err(format!("unknown webcam shape: {name}")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WebcamLayout {
    corner: WebcamCorner,
    size_percent: u8,
    shape: WebcamShape,
}

impl WebcamLayout {
    pub(super) fn new(corner: WebcamCorner, size_percent: u8) -> Result<Self, String> {
        if !(10..=50).contains(&size_percent) {
            return Err("webcam size must be between 10 and 50 percent".to_owned());
        }
        Ok(Self {
            corner,
            size_percent,
            shape: WebcamShape::Rectangle,
        })
    }

    pub(super) fn with_shape(mut self, shape: WebcamShape) -> Self {
        self.shape = shape;
        self
    }

    pub(super) fn corner(self) -> WebcamCorner {
        self.corner
    }

    pub(super) fn size_percent(self) -> u8 {
        self.size_percent
    }

    pub(super) fn shape(self) -> WebcamShape {
        self.shape
    }
}

impl Default for WebcamLayout {
    fn default() -> Self {
        Self {
            corner: WebcamCorner::BottomRight,
            size_percent: 25,
            shape: WebcamShape::Rectangle,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WebcamConfiguration {
    device: String,
    layout: WebcamLayout,
}

impl WebcamConfiguration {
    pub(super) fn new(device: &str, layout: WebcamLayout) -> Result<Self, String> {
        let contains_control = device.chars().any(char::is_control);
        let device = device.trim();
        if device.is_empty()
            || device.len() > 4096
            || contains_control
            || !Path::new(device).is_absolute()
        {
            return Err(
                "webcam device must be an absolute path without control characters".to_owned(),
            );
        }
        Ok(Self {
            device: device.to_owned(),
            layout,
        })
    }

    pub(super) fn device(&self) -> &str {
        &self.device
    }

    pub(super) fn layout(&self) -> WebcamLayout {
        self.layout
    }
}

impl Default for WebcamConfiguration {
    fn default() -> Self {
        Self {
            device: "/dev/video0".to_owned(),
            layout: WebcamLayout::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingCommand {
    TogglePause,
    Stop,
    Restart,
    Cancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingState {
    Starting,
    Recording,
    Paused,
    Stopping,
    Restarting,
    Cancelling,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordingRun {
    Finished,
    Restart,
    Cancelled,
}

enum SetupNext<T> {
    Ready(T),
    Command(Result<RecordingCommand, async_channel::RecvError>),
}

async fn wait_for_recording_setup<T>(
    future: impl Future<Output = T>,
    commands: &async_channel::Receiver<RecordingCommand>,
    state_changed: &mut impl FnMut(RecordingState),
) -> Option<T> {
    let mut future = std::pin::pin!(future);
    loop {
        match futures_lite::future::or(async { SetupNext::Ready(future.as_mut().await) }, async {
            SetupNext::Command(commands.recv().await)
        })
        .await
        {
            SetupNext::Ready(value) => return Some(value),
            SetupNext::Command(Ok(RecordingCommand::Stop | RecordingCommand::Cancel) | Err(_)) => {
                state_changed(RecordingState::Cancelling);
                return None;
            }
            SetupNext::Command(Ok(RecordingCommand::TogglePause | RecordingCommand::Restart)) => {
                state_changed(RecordingState::Starting);
            }
        }
    }
}

struct RecordingPortalStream {
    portal: Screencast<'static>,
    session: zbus::zvariant::OwnedObjectPath,
    remote: OwnedFd,
    node_id: u32,
}

impl RecordingPortalStream {
    async fn finish<T>(self, result: Result<T, String>) -> Result<T, String> {
        close_recording_session(&self.portal, &self.session, result).await
    }
}

async fn recording_portal_stream(
    commands: &async_channel::Receiver<RecordingCommand>,
    cursor: CaptureCursor,
    state_changed: &mut impl FnMut(RecordingState),
) -> Result<Option<RecordingPortalStream>, String> {
    let Some(portal) = wait_for_recording_setup(Screencast::new(), commands, state_changed).await
    else {
        return Ok(None);
    };
    let portal = portal
        .map_err(|error| format!("could not connect to the screen capture portal: {error}"))?;
    let session_token = next_portal_token();
    let session_path = format!(
        "/org/freedesktop/portal/desktop/session/{}/{}",
        recording_portal_sender(&portal)?,
        session_token
    );
    let session = zbus::zvariant::OwnedObjectPath::try_from(session_path)
        .map_err(|error| format!("could not prepare a screen capture session: {error}"))?;
    let created = recording_portal_request::<HashMap<String, zbus::zvariant::OwnedValue>>(
        &portal,
        RecordingPortalRequest::CreateSession(&session_token),
        commands,
        state_changed,
    )
    .await;
    let created = match created {
        Ok(Some(created)) => created,
        Ok(None) => return close_recording_session(&portal, &session, Ok(None)).await,
        Err(error) => return close_recording_session(&portal, &session, Err(error)).await,
    };
    let actual_session = match recording_session_handle(&created) {
        Ok(actual) => actual,
        Err(error) => return close_recording_session(&portal, &session, Err(error)).await,
    };
    if actual_session != session {
        let error = Err(format!(
            "screen capture returned an unexpected session handle: {actual_session}"
        ));
        let error = close_recording_session(&portal, &actual_session, error).await;
        return close_recording_session(&portal, &session, error).await;
    }
    let result = async {
        let selection: Option<()> = recording_portal_request(
            &portal,
            RecordingPortalRequest::SelectSources(&session, cursor),
            commands,
            state_changed,
        )
        .await?;
        if selection.is_none() {
            return Ok(None);
        }

        let Some(streams) = recording_portal_request::<Streams>(
            &portal,
            RecordingPortalRequest::Start(&session),
            commands,
            state_changed,
        )
        .await?
        else {
            return Ok(None);
        };
        let node_id = streams
            .streams()
            .first()
            .ok_or_else(|| "the screen capture portal returned no streams".to_owned())?
            .pipe_wire_node_id();
        let remote = wait_for_recording_setup(
            recording_pipewire_remote(&portal, &session),
            commands,
            state_changed,
        )
        .await;
        let Some(remote) = remote else {
            return Ok(None);
        };
        let remote = remote?;
        Ok(Some((remote, node_id)))
    }
    .await;

    match result {
        Ok(Some((remote, node_id))) => Ok(Some(RecordingPortalStream {
            portal,
            session,
            remote,
            node_id,
        })),
        Ok(None) => close_recording_session(&portal, &session, Ok(None)).await,
        Err(error) => close_recording_session(&portal, &session, Err(error)).await,
    }
}

#[derive(Clone, Copy)]
enum RecordingPortalRequest<'a> {
    CreateSession(&'a str),
    SelectSources(&'a zbus::zvariant::OwnedObjectPath, CaptureCursor),
    Start(&'a zbus::zvariant::OwnedObjectPath),
}

fn next_portal_token() -> String {
    format!(
        "jiffshot_{}_{}",
        std::process::id(),
        NEXT_PORTAL_REQUEST.fetch_add(1, Ordering::Relaxed)
    )
}

fn recording_portal_sender(portal: &Screencast<'_>) -> Result<String, String> {
    portal
        .connection()
        .unique_name()
        .map(|name| name.trim_start_matches(':').replace('.', "_"))
        .ok_or_else(|| "the screen capture portal connection has no unique name".to_owned())
}

fn recording_session_handle(
    response: &HashMap<String, zbus::zvariant::OwnedValue>,
) -> Result<zbus::zvariant::OwnedObjectPath, String> {
    let value = response
        .get("session_handle")
        .ok_or_else(|| "screen capture did not return a session handle".to_owned())?;
    let path = if let Ok(value) = value.downcast_ref::<&str>() {
        zbus::zvariant::ObjectPath::try_from(value).map_err(|error| {
            format!("screen capture returned an invalid session handle: {error}")
        })?
    } else if let Ok(value) = value.downcast_ref::<zbus::zvariant::ObjectPath<'_>>() {
        value
    } else {
        return Err("screen capture returned a session handle with an invalid type".to_owned());
    };
    Ok(path.into())
}

async fn close_recording_session<T>(
    portal: &Screencast<'_>,
    session: &zbus::zvariant::OwnedObjectPath,
    result: Result<T, String>,
) -> Result<T, String> {
    let closed = async {
        let proxy = zbus::Proxy::new(
            portal.connection(),
            "org.freedesktop.portal.Desktop",
            session.as_str(),
            "org.freedesktop.portal.Session",
        )
        .await
        .map_err(|error| format!("could not prepare the screen capture session close: {error}"))?;
        match proxy.call::<_, _, ()>("Close", &()).await {
            Ok(()) => Ok(()),
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.freedesktop.DBus.Error.UnknownObject" =>
            {
                Ok(())
            }
            Err(error) => Err(format!(
                "could not close the screen capture session: {error}"
            )),
        }
    }
    .await;
    crate::linux::portal::combine_session_results(result, closed)
}

async fn recording_pipewire_remote(
    portal: &Screencast<'_>,
    session: &zbus::zvariant::OwnedObjectPath,
) -> Result<OwnedFd, String> {
    let options = HashMap::<&str, zbus::zvariant::Value<'_>>::new();
    let response = portal
        .call_method("OpenPipeWireRemote", &(session, options))
        .await
        .map_err(|error| format!("could not connect to the PipeWire capture stream: {error}"))?;
    let fd: zbus::zvariant::OwnedFd = response
        .body()
        .deserialize()
        .map_err(|error| format!("screen capture returned an invalid PipeWire handle: {error}"))?;
    Ok(fd.into())
}

async fn recording_portal_request<T>(
    portal: &Screencast<'_>,
    kind: RecordingPortalRequest<'_>,
    commands: &async_channel::Receiver<RecordingCommand>,
    state_changed: &mut impl FnMut(RecordingState),
) -> Result<Option<T>, String>
where
    T: for<'de> serde::Deserialize<'de> + zbus::zvariant::Type + std::fmt::Debug,
{
    let sender = recording_portal_sender(portal)?;
    let token = next_portal_token();
    let request_path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
    let request = zbus::Proxy::new(
        portal.connection(),
        "org.freedesktop.portal.Desktop",
        request_path.as_str(),
        "org.freedesktop.portal.Request",
    )
    .await
    .map_err(|error| format!("could not prepare the screen capture request: {error}"))?;
    let mut responses = request
        .receive_signal("Response")
        .await
        .map_err(|error| format!("could not monitor the screen capture request: {error}"))?;
    let mut options =
        HashMap::from([("handle_token", zbus::zvariant::Value::from(token.as_str()))]);
    if let RecordingPortalRequest::CreateSession(session_token) = kind {
        options.insert(
            "session_handle_token",
            zbus::zvariant::Value::from(session_token),
        );
    }
    if let RecordingPortalRequest::SelectSources(_, cursor) = kind {
        options.insert(
            "types",
            zbus::zvariant::Value::from(SourceType::Monitor as u32 | SourceType::Window as u32),
        );
        options.insert("multiple", zbus::zvariant::Value::from(false));
        options.insert(
            "cursor_mode",
            zbus::zvariant::Value::from(cursor.portal_mode() as u32),
        );
        options.insert(
            "persist_mode",
            zbus::zvariant::Value::from(PersistMode::DoNot as u32),
        );
    }
    let method = async {
        match kind {
            RecordingPortalRequest::CreateSession(_) => {
                portal.call_method("CreateSession", &options).await
            }
            RecordingPortalRequest::SelectSources(session, _) => {
                portal
                    .call_method("SelectSources", &(session, &options))
                    .await
            }
            RecordingPortalRequest::Start(session) => {
                portal.call_method("Start", &(session, "", &options)).await
            }
        }
    };
    let started = wait_for_recording_setup(method, commands, state_changed).await;
    let Some(started) = started else {
        close_recording_request(&request).await?;
        return Ok(None);
    };
    let started = started.map_err(|error| format!("could not start screen capture: {error}"))?;
    let actual_path: zbus::zvariant::OwnedObjectPath = started
        .body()
        .deserialize()
        .map_err(|error| format!("screen capture returned an invalid request handle: {error}"))?;
    if actual_path.as_str() != request_path {
        let actual_request = zbus::Proxy::new(
            portal.connection(),
            "org.freedesktop.portal.Desktop",
            actual_path.as_str(),
            "org.freedesktop.portal.Request",
        )
        .await
        .map_err(|error| {
            format!("could not close the unexpected screen capture request: {error}")
        })?;
        return crate::linux::portal::combine_session_results(
            Err(format!(
                "screen capture returned an unexpected request handle: {actual_path}"
            )),
            close_recording_request(&actual_request).await,
        );
    }
    let response = wait_for_recording_setup(responses.next(), commands, state_changed).await;
    let Some(response) = response else {
        close_recording_request(&request).await?;
        return Ok(None);
    };
    let response =
        response.ok_or_else(|| "the screen capture request ended unexpectedly".to_owned())?;
    let response: Response<T> = response
        .body()
        .deserialize()
        .map_err(|error| format!("screen capture returned an invalid response: {error}"))?;
    match response {
        Response::Ok(value) => Ok(Some(value)),
        Response::Err(error) => Err(format!("screen capture was cancelled or rejected: {error}")),
    }
}

async fn close_recording_request(request: &zbus::Proxy<'_>) -> Result<(), String> {
    match request.call::<_, _, ()>("Close", &()).await {
        Ok(()) => Ok(()),
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.freedesktop.DBus.Error.UnknownObject" =>
        {
            Ok(())
        }
        Err(error) => Err(format!(
            "could not close the screen capture request: {error}"
        )),
    }
}

#[derive(Default)]
struct RecordingTimeline {
    pauses: Vec<(gst::ClockTime, Option<gst::ClockTime>)>,
    video_received: bool,
}

impl RecordingTimeline {
    fn pause(&mut self, at: gst::ClockTime) -> Result<(), String> {
        if self
            .pauses
            .last()
            .is_some_and(|(_, end)| end.is_none_or(|end| at < end))
        {
            return Err("recording pause timestamps are inconsistent".to_owned());
        }
        self.pauses.push((at, None));
        Ok(())
    }

    fn resume(&mut self, at: gst::ClockTime) -> Result<(), String> {
        let Some((started, end)) = self.pauses.last_mut() else {
            return Err("recording has no paused interval".to_owned());
        };
        if at < *started || end.is_some() {
            return Err("recording pause timestamps are inconsistent".to_owned());
        }
        *end = Some(at);
        Ok(())
    }

    fn timestamp(&self, timestamp: gst::ClockTime) -> Option<gst::ClockTime> {
        let mut adjusted = timestamp.nseconds();
        for &(start, end) in &self.pauses {
            if timestamp < start {
                break;
            }
            match end {
                Some(end) if timestamp >= end => {
                    adjusted = adjusted.checked_sub(end.nseconds() - start.nseconds())?;
                }
                _ => return None,
            }
        }
        Some(gst::ClockTime::from_nseconds(adjusted))
    }
}

fn recording_audio_sources(audio: RecordingAudio) -> Result<Vec<gst::Element>, String> {
    let mut sources = Vec::new();
    if matches!(
        audio,
        RecordingAudio::System | RecordingAudio::SystemAndMicrophone
    ) {
        sources.push(
            gst::ElementFactory::make("pulsesrc")
                .name("system-audio")
                .property("device", "@DEFAULT_MONITOR@")
                .property("do-timestamp", true)
                .build()
                .map_err(|error| format!("could not create system audio source: {error}"))?,
        );
    }
    if matches!(
        audio,
        RecordingAudio::Microphone | RecordingAudio::SystemAndMicrophone
    ) {
        sources.push(
            gst::ElementFactory::make("pulsesrc")
                .name("microphone-audio")
                .property("do-timestamp", true)
                .build()
                .map_err(|error| format!("could not create microphone audio source: {error}"))?,
        );
    }
    Ok(sources)
}

pub(super) async fn record_monitor_with_audio(
    commands: async_channel::Receiver<RecordingCommand>,
    audio: RecordingAudio,
    webcam: RecordingWebcam,
    cursor: CaptureCursor,
    frame_rate: RecordingFrameRate,
    state_changed: impl FnMut(RecordingState),
) -> Result<Option<PathBuf>, String> {
    gst::init().map_err(|error| format!("could not initialize GStreamer: {error}"))?;
    let layout = match &webcam {
        RecordingWebcam::Disabled => WebcamLayout::default(),
        RecordingWebcam::Enabled(configuration) => configuration.layout,
    };
    record_portal_sources(
        commands,
        audio,
        cursor,
        frame_rate,
        move || match &webcam {
            RecordingWebcam::Disabled => Ok(None),
            RecordingWebcam::Enabled(configuration) => webcam_source(configuration).map(Some),
        },
        layout,
        state_changed,
    )
    .await
}

fn webcam_source(configuration: &WebcamConfiguration) -> Result<gst::Element, String> {
    gst::ElementFactory::make("v4l2src")
        .name("webcam")
        .property("device", configuration.device.as_str())
        .property("do-timestamp", true)
        .build()
        .map_err(|error| format!("could not create webcam source: {error}"))
}

async fn record_portal_sources(
    commands: async_channel::Receiver<RecordingCommand>,
    audio: RecordingAudio,
    cursor: CaptureCursor,
    frame_rate: RecordingFrameRate,
    mut webcam_source: impl FnMut() -> Result<Option<gst::Element>, String>,
    webcam_layout: WebcamLayout,
    mut state_changed: impl FnMut(RecordingState),
) -> Result<Option<PathBuf>, String> {
    let Some(stream) = recording_portal_stream(&commands, cursor, &mut state_changed).await? else {
        return Ok(None);
    };
    let result = record_opened_portal(
        &stream,
        commands,
        audio,
        frame_rate,
        &mut webcam_source,
        webcam_layout,
        &mut state_changed,
    )
    .await;
    stream.finish(result).await
}

async fn record_opened_portal(
    stream: &RecordingPortalStream,
    commands: async_channel::Receiver<RecordingCommand>,
    audio: RecordingAudio,
    frame_rate: RecordingFrameRate,
    webcam_source: &mut impl FnMut() -> Result<Option<gst::Element>, String>,
    webcam_layout: WebcamLayout,
    state_changed: &mut impl FnMut(RecordingState),
) -> Result<Option<PathBuf>, String> {
    let directory = capture_directory()?;
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    let staging = tempfile::tempdir_in(&directory)
        .map_err(|error| format!("could not create a temporary recording: {error}"))?;
    let staged_path = staging.path().join("recording.mp4");
    let mut restarted_remote = None;
    loop {
        let remote = restarted_remote.as_ref().unwrap_or(&stream.remote);
        let source = pipewire_source(remote.as_raw_fd(), stream.node_id)?;
        let audio_sources = recording_audio_sources(audio)?;
        let pipeline = recording_pipeline_with_layout(
            source,
            webcam_source()?,
            webcam_layout,
            audio_sources,
            frame_rate,
            &staged_path,
        )?;
        let result = run_recording(&pipeline, commands.clone(), &mut *state_changed)?;
        drop(pipeline);
        match result {
            RecordingRun::Finished => {
                if !staged_path.is_file() {
                    return Err(format!(
                        "recording did not create {}",
                        staged_path.display()
                    ));
                }
                let path = recording_path(&directory)?;
                std::fs::rename(&staged_path, &path)
                    .map_err(|error| format!("could not save {}: {error}", path.display()))?;
                return Ok(Some(path));
            }
            RecordingRun::Cancelled => return Ok(None),
            RecordingRun::Restart => {
                match std::fs::remove_file(&staged_path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!(
                            "could not discard {} before restarting: {error}",
                            staged_path.display()
                        ));
                    }
                }
                let remote = wait_for_recording_setup(
                    recording_pipewire_remote(&stream.portal, &stream.session),
                    &commands,
                    state_changed,
                )
                .await;
                let Some(remote) = remote else {
                    return Ok(None);
                };
                restarted_remote = Some(remote?);
            }
        }
    }
}

fn run_recording(
    pipeline: &gst::Pipeline,
    commands: async_channel::Receiver<RecordingCommand>,
    mut state_changed: impl FnMut(RecordingState),
) -> Result<RecordingRun, String> {
    let bus = pipeline
        .bus()
        .ok_or_else(|| "recording pipeline has no event bus".to_owned())?;
    let timeline = recording_timeline(pipeline)?;
    if let Err(error) = pipeline.set_state(gst::State::Playing) {
        let detail = bus
            .timed_pop_filtered(
                gst::ClockTime::from_mseconds(250),
                &[gst::MessageType::Error],
            )
            .and_then(|message| match message.view() {
                gst::MessageView::Error(error) => Some(error.error().to_string()),
                _ => None,
            })
            .unwrap_or_else(|| error.to_string());
        return Err(stop_failed_recording(
            pipeline,
            format!("could not start recording: {detail}"),
        ));
    }
    state_changed(RecordingState::Recording);
    let mut paused = false;
    loop {
        match commands.try_recv() {
            Ok(RecordingCommand::Stop) | Err(async_channel::TryRecvError::Closed) => {
                state_changed(RecordingState::Stopping);
                let video_received = timeline.lock().video_received;
                if !video_received {
                    return Err(stop_failed_recording(
                        pipeline,
                        "screen recording ended before receiving a video frame".to_owned(),
                    ));
                }
                finish_recording(pipeline)?;
                return Ok(RecordingRun::Finished);
            }
            Ok(command @ (RecordingCommand::Cancel | RecordingCommand::Restart)) => {
                let (result, state) = if command == RecordingCommand::Cancel {
                    (RecordingRun::Cancelled, RecordingState::Cancelling)
                } else {
                    (RecordingRun::Restart, RecordingState::Restarting)
                };
                state_changed(state);
                pipeline
                    .set_state(gst::State::Null)
                    .map_err(|error| format!("could not stop recording pipeline: {error}"))?;
                return Ok(result);
            }
            Ok(RecordingCommand::TogglePause) => {
                let Some(at) = pipeline.current_running_time() else {
                    state_changed(RecordingState::Starting);
                    continue;
                };
                // Keep PipeWire running while omitting paused samples from the output timeline.
                let result = if paused {
                    timeline.lock().resume(at)
                } else {
                    timeline.lock().pause(at)
                };
                if let Err(error) = result {
                    return Err(stop_failed_recording(pipeline, error));
                }
                paused = !paused;
                state_changed(if paused {
                    RecordingState::Paused
                } else {
                    RecordingState::Recording
                });
            }
            Err(async_channel::TryRecvError::Empty) => {}
        }
        let Some(message) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(250),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) else {
            continue;
        };
        let error = match message.view() {
            gst::MessageView::Error(error) => {
                format!("recording pipeline failed: {}", error.error())
            }
            gst::MessageView::Eos(_) => "screen capture stream ended unexpectedly".to_owned(),
            _ => "recording pipeline ended unexpectedly".to_owned(),
        };
        return Err(stop_failed_recording(pipeline, error));
    }
}

fn recording_timeline(
    pipeline: &gst::Pipeline,
) -> Result<Arc<parking_lot::Mutex<RecordingTimeline>>, String> {
    let timeline = Arc::new(parking_lot::Mutex::new(RecordingTimeline::default()));
    for (name, required) in [
        ("recording-video-converter", true),
        ("recording-audio-converter", false),
    ] {
        let Some(element) = pipeline.by_name(name) else {
            if required {
                return Err(format!("recording pipeline has no {name}"));
            }
            continue;
        };
        let pad = element
            .static_pad("src")
            .ok_or_else(|| format!("{name} has no output pad"))?;
        let timeline = Arc::clone(&timeline);
        let weak_pipeline = pipeline.downgrade();
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            let Some(buffer) = info.buffer_mut() else {
                return gst::PadProbeReturn::Ok;
            };
            let Some(timestamp) = buffer.pts() else {
                if let Some(pipeline) = weak_pipeline.upgrade() {
                    gst::element_error!(
                        pipeline,
                        gst::StreamError::Format,
                        ("recording buffer has no timestamp")
                    );
                }
                return gst::PadProbeReturn::Drop;
            };
            let mut timeline = timeline.lock();
            let Some(pts) = timeline.timestamp(timestamp) else {
                return gst::PadProbeReturn::Drop;
            };
            let dts = match buffer.dts() {
                Some(timestamp) => match timeline.timestamp(timestamp) {
                    Some(timestamp) => Some(timestamp),
                    None => return gst::PadProbeReturn::Drop,
                },
                None => None,
            };
            if name == "recording-video-converter" {
                timeline.video_received = true;
            }
            let buffer = buffer.make_mut();
            buffer.set_pts(pts);
            buffer.set_dts(dts);
            gst::PadProbeReturn::Ok
        })
        .ok_or_else(|| format!("could not observe {name} buffers"))?;
    }
    Ok(timeline)
}

fn stop_failed_recording(pipeline: &gst::Pipeline, error: String) -> String {
    match pipeline.set_state(gst::State::Null) {
        Ok(_) => error,
        Err(stop_error) => format!("{error}; could not stop recording pipeline: {stop_error}"),
    }
}

pub fn save_capture(document: &jiffshot_doc::Document) -> Result<PathBuf, String> {
    let directory = capture_directory()?;
    save_capture_to(document, &directory)
}

pub fn capture_directory() -> Result<PathBuf, String> {
    crate::linux::settings::capture_directory()
}

pub fn recent_captures() -> Result<Vec<PathBuf>, String> {
    let directory = capture_directory()?;
    recent_captures_in(&directory)
}

fn recent_captures_in(directory: &Path) -> Result<Vec<PathBuf>, String> {
    let mut captures = crate::linux::library::capture_files_in(directory)?;
    captures.sort_by(|(left_path, left_time), (right_path, right_time)| {
        right_time
            .cmp(left_time)
            .then_with(|| right_path.cmp(left_path))
    });
    captures.truncate(RECENT_CAPTURE_LIMIT);
    Ok(captures.into_iter().map(|(path, _)| path).collect())
}

fn save_capture_to(document: &jiffshot_doc::Document, directory: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    let path = capture_path(directory)?;
    jiffshot_doc::io::export(document, &path)
        .map_err(|error| format!("could not save {}: {error}", path.display()))?;
    Ok(path)
}

fn recording_path(directory: &Path) -> Result<PathBuf, String> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock is before the Unix epoch: {error}"))?;
    Ok(directory.join(format!(
        "Recording-{}-{:09}.mp4",
        timestamp.as_secs(),
        timestamp.subsec_nanos()
    )))
}

fn capture_path(directory: &Path) -> Result<PathBuf, String> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock is before the Unix epoch: {error}"))?;
    Ok(directory.join(format!(
        "Screenshot-{}-{:09}.png",
        timestamp.as_secs(),
        timestamp.subsec_nanos()
    )))
}

pub(super) fn recording_pipeline(
    source: gst::Element,
    path: &Path,
) -> Result<gst::Pipeline, String> {
    recording_pipeline_with_audio(source, Vec::new(), path)
}

fn recording_pipeline_with_audio(
    source: gst::Element,
    audio_sources: Vec<gst::Element>,
    path: &Path,
) -> Result<gst::Pipeline, String> {
    recording_pipeline_with_sources(source, None, audio_sources, path)
}

fn recording_pipeline_with_sources(
    source: gst::Element,
    webcam_source: Option<gst::Element>,
    audio_sources: Vec<gst::Element>,
    path: &Path,
) -> Result<gst::Pipeline, String> {
    recording_pipeline_with_layout(
        source,
        webcam_source,
        WebcamLayout::default(),
        audio_sources,
        RecordingFrameRate::Source,
        path,
    )
}

fn recording_pipeline_with_layout(
    source: gst::Element,
    webcam_source: Option<gst::Element>,
    webcam_layout: WebcamLayout,
    audio_sources: Vec<gst::Element>,
    frame_rate: RecordingFrameRate,
    path: &Path,
) -> Result<gst::Pipeline, String> {
    let convert = gst::ElementFactory::make("videoconvert")
        .name("recording-video-converter")
        .build()
        .map_err(|error| format!("could not create video converter: {error}"))?;
    // Scene-cut intra frames can be indexed as seekable even when they are not IDR frames.
    let encoder = gst::ElementFactory::make("x264enc")
        .property("bitrate", 4_000u32)
        .property_from_str("tune", "zerolatency")
        .property("option-string", "scenecut=0")
        .build()
        .map_err(|error| format!("could not create H.264 encoder: {error}"))?;
    let parser = gst::ElementFactory::make("h264parse")
        .build()
        .map_err(|error| format!("could not create H.264 parser: {error}"))?;
    let muxer = gst::ElementFactory::make("mp4mux")
        .build()
        .map_err(|error| format!("could not create MP4 muxer: {error}"))?;
    let sink = gst::ElementFactory::make("filesink")
        .property("location", path.to_string_lossy().as_ref())
        .build()
        .map_err(|error| format!("could not create recording file: {error}"))?;
    let pipeline = gst::Pipeline::new();
    let video_source = if let Some(webcam_source) = webcam_source {
        compose_webcam(&pipeline, source, webcam_source, webcam_layout)?
    } else {
        pipeline
            .add(&source)
            .map_err(|error| format!("could not add screen recording source: {error}"))?;
        source
    };
    pipeline
        .add(&convert)
        .map_err(|error| format!("could not add recording video converter: {error}"))?;
    gst::Element::link_many([&video_source, &convert])
        .map_err(|error| format!("could not connect recording video source: {error}"))?;
    let video_output = if let Some(frames_per_second) = frame_rate.frames_per_second() {
        let rate = gst::ElementFactory::make("videorate")
            // Do not synthesize video before the first frame when audio starts earlier.
            .property("skip-to-first", true)
            .build()
            .map_err(|error| format!("could not create recording frame-rate converter: {error}"))?;
        let filter = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("framerate", gst::Fraction::new(frames_per_second, 1))
                    .build(),
            )
            .build()
            .map_err(|error| format!("could not set recording frame rate: {error}"))?;
        pipeline
            .add_many([&rate, &filter])
            .map_err(|error| format!("could not build recording frame-rate converter: {error}"))?;
        gst::Element::link_many([&convert, &rate, &filter]).map_err(|error| {
            format!("could not connect recording frame-rate converter: {error}")
        })?;
        filter
    } else {
        convert
    };
    if !audio_sources.is_empty() {
        let mixer = gst::ElementFactory::make("audiomixer")
            .build()
            .map_err(|error| format!("could not create recording audio mixer: {error}"))?;
        let audio_convert = gst::ElementFactory::make("audioconvert")
            .name("recording-audio-converter")
            .build()
            .map_err(|error| format!("could not create recording audio converter: {error}"))?;
        let audio_resample = gst::ElementFactory::make("audioresample")
            .build()
            .map_err(|error| format!("could not create recording audio resampler: {error}"))?;
        let audio_encoder = gst::ElementFactory::make("voaacenc")
            .build()
            .map_err(|error| format!("could not create AAC audio encoder: {error}"))?;
        let audio_parser = gst::ElementFactory::make("aacparse")
            .build()
            .map_err(|error| format!("could not create AAC parser: {error}"))?;
        let video_queue = gst::ElementFactory::make("queue")
            .build()
            .map_err(|error| format!("could not create video recording queue: {error}"))?;
        let audio_queue = gst::ElementFactory::make("queue")
            .build()
            .map_err(|error| format!("could not create audio recording queue: {error}"))?;
        pipeline
            .add_many([
                &encoder,
                &parser,
                &video_queue,
                &mixer,
                &audio_convert,
                &audio_resample,
                &audio_encoder,
                &audio_parser,
                &audio_queue,
                &muxer,
                &sink,
            ])
            .map_err(|error| format!("could not build audio recording pipeline: {error}"))?;
        gst::Element::link_many([
            &video_output,
            &encoder,
            &parser,
            &video_queue,
            &muxer,
            &sink,
        ])
        .map_err(|error| format!("could not connect video recording pipeline: {error}"))?;
        gst::Element::link_many([
            &mixer,
            &audio_convert,
            &audio_resample,
            &audio_encoder,
            &audio_parser,
            &audio_queue,
            &muxer,
        ])
        .map_err(|error| format!("could not connect audio recording pipeline: {error}"))?;
        for audio_source in audio_sources {
            let input_queue = gst::ElementFactory::make("queue")
                .build()
                .map_err(|error| format!("could not create audio input queue: {error}"))?;
            let input_convert = gst::ElementFactory::make("audioconvert")
                .build()
                .map_err(|error| format!("could not create audio input converter: {error}"))?;
            let input_resample = gst::ElementFactory::make("audioresample")
                .build()
                .map_err(|error| format!("could not create audio input resampler: {error}"))?;
            pipeline
                .add_many([&audio_source, &input_queue, &input_convert, &input_resample])
                .map_err(|error| format!("could not add audio recording input: {error}"))?;
            gst::Element::link_many([
                &audio_source,
                &input_queue,
                &input_convert,
                &input_resample,
                &mixer,
            ])
            .map_err(|error| format!("could not connect audio recording input: {error}"))?;
        }
    } else {
        pipeline
            .add_many([&encoder, &parser, &muxer, &sink])
            .map_err(|error| format!("could not build recording pipeline: {error}"))?;
        gst::Element::link_many([&video_output, &encoder, &parser, &muxer, &sink])
            .map_err(|error| format!("could not connect recording pipeline: {error}"))?;
    }
    Ok(pipeline)
}

#[derive(Debug, Eq, PartialEq)]
struct WebcamGeometry {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

fn webcam_geometry(width: i32, height: i32, layout: WebcamLayout) -> Option<WebcamGeometry> {
    if width <= 0 || height <= 0 {
        return None;
    }
    let overlay_height = (i64::from(height) * i64::from(layout.size_percent) / 100).max(1) as i32;
    let overlay_width = match layout.shape {
        WebcamShape::Rectangle => {
            (i64::from(width) * i64::from(layout.size_percent) / 100).max(1) as i32
        }
        WebcamShape::Circle => overlay_height.min(width),
    };
    let overlay_height = match layout.shape {
        WebcamShape::Rectangle => overlay_height,
        WebcamShape::Circle => overlay_width,
    };
    let margin = (width.min(height) / 40).min(16);
    let left = margin.min(width - overlay_width);
    let top = margin.min(height - overlay_height);
    let right = width - overlay_width - left;
    let bottom = height - overlay_height - top;
    let (x, y) = match layout.corner {
        WebcamCorner::TopLeft => (left, top),
        WebcamCorner::TopRight => (right, top),
        WebcamCorner::BottomLeft => (left, bottom),
        WebcamCorner::BottomRight => (right, bottom),
    };
    Some(WebcamGeometry {
        x,
        y,
        width: overlay_width,
        height: overlay_height,
    })
}

fn mask_webcam_frame(pad: &gst::Pad, probe: &mut gst::PadProbeInfo<'_>) -> Result<(), String> {
    use gstreamer_video::prelude::*;

    let caps = pad
        .current_caps()
        .ok_or_else(|| "the webcam frame format is unavailable".to_owned())?;
    let info = gstreamer_video::VideoInfo::from_caps(&caps)
        .map_err(|error| format!("invalid webcam frame format: {error}"))?;
    if info.format() != gstreamer_video::VideoFormat::Rgba {
        return Err("the webcam frame has an unsupported pixel format".to_owned());
    }
    let buffer = probe
        .buffer_mut()
        .ok_or_else(|| "the webcam frame has no buffer".to_owned())?;
    let mut frame =
        gstreamer_video::VideoFrameRef::from_buffer_ref_writable(buffer.make_mut(), &info)
            .map_err(|error| format!("could not map webcam frame: {error}"))?;
    let stride = frame
        .plane_stride()
        .first()
        .and_then(|stride| usize::try_from(*stride).ok())
        .ok_or_else(|| "the webcam frame has an invalid row stride".to_owned())?;
    let plane = frame
        .plane_data_mut(0)
        .map_err(|error| format!("could not access webcam pixels: {error}"))?;
    apply_circular_alpha(plane, stride, info.width(), info.height())
}

fn apply_circular_alpha(
    plane: &mut [u8],
    stride: usize,
    width: u32,
    height: u32,
) -> Result<(), String> {
    let width = width as usize;
    let height = height as usize;
    let row_bytes = width
        .checked_mul(4)
        .filter(|_| width > 0 && height > 0)
        .ok_or_else(|| "the webcam frame has invalid dimensions".to_owned())?;
    let required = (height - 1)
        .checked_mul(stride)
        .and_then(|offset| offset.checked_add(row_bytes))
        .ok_or_else(|| "the webcam frame is too large".to_owned())?;
    if stride < row_bytes || plane.len() < required {
        return Err("the webcam frame has an incomplete pixel buffer".to_owned());
    }

    let center_x = width as f32 / 2.0;
    let center_y = height as f32 / 2.0;
    let radius = width.min(height) as f32 / 2.0;
    let inner_squared = (radius - 0.5).max(0.0).powi(2);
    let outer = radius + 0.5;
    let outer_squared = outer * outer;
    for y in 0..height {
        let dy = y as f32 + 0.5 - center_y;
        let row = &mut plane[y * stride..y * stride + row_bytes];
        for (x, pixel) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let dx = x as f32 + 0.5 - center_x;
            let distance_squared = dx * dx + dy * dy;
            if distance_squared >= outer_squared {
                pixel[3] = 0;
            } else if distance_squared > inner_squared {
                let coverage = (outer - distance_squared.sqrt()).clamp(0.0, 1.0);
                pixel[3] = (f32::from(pixel[3]) * coverage).round() as u8;
            }
        }
    }
    Ok(())
}

fn compose_webcam(
    pipeline: &gst::Pipeline,
    screen: gst::Element,
    webcam: gst::Element,
    layout: WebcamLayout,
) -> Result<gst::Element, String> {
    use gstreamer_video::gst_base::{self, prelude::*};

    let screen_queue = gst::ElementFactory::make("queue")
        .build()
        .map_err(|error| format!("could not create screen input queue: {error}"))?;
    let screen_convert = gst::ElementFactory::make("videoconvert")
        .build()
        .map_err(|error| format!("could not create screen input converter: {error}"))?;
    let webcam_queue = gst::ElementFactory::make("queue")
        .build()
        .map_err(|error| format!("could not create webcam input queue: {error}"))?;
    let webcam_gate = gst::ElementFactory::make("valve")
        .name("webcam-input-gate")
        .property("drop", true)
        .property_from_str("drop-mode", "forward-sticky-events")
        .build()
        .map_err(|error| format!("could not create webcam input gate: {error}"))?;
    let webcam_convert = gst::ElementFactory::make("videoconvert")
        .build()
        .map_err(|error| format!("could not create webcam input converter: {error}"))?;
    let compositor = gst::ElementFactory::make("compositor")
        .name("recording-webcam-compositor")
        .property_from_str("start-time-selection", "first")
        .build()
        .map_err(|error| format!("could not create webcam compositor: {error}"))?;
    pipeline
        .add_many([
            &screen,
            &screen_queue,
            &screen_convert,
            &webcam,
            &webcam_queue,
            &webcam_gate,
            &webcam_convert,
            &compositor,
        ])
        .map_err(|error| format!("could not build webcam recording pipeline: {error}"))?;
    gst::Element::link_many([&screen, &screen_queue, &screen_convert])
        .map_err(|error| format!("could not connect screen input: {error}"))?;
    gst::Element::link_many([&webcam, &webcam_queue, &webcam_gate, &webcam_convert])
        .map_err(|error| format!("could not connect webcam input: {error}"))?;

    let webcam_output = if layout.shape == WebcamShape::Circle {
        let scale = gst::ElementFactory::make("videoscale")
            .build()
            .map_err(|error| format!("could not create webcam scaler: {error}"))?;
        let format = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("format", "RGBA")
                    .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
                    .build(),
            )
            .build()
            .map_err(|error| format!("could not set webcam pixel format: {error}"))?;
        let crop = gst::ElementFactory::make("aspectratiocrop")
            .property("aspect-ratio", gst::Fraction::new(1, 1))
            .build()
            .map_err(|error| format!("could not create circular webcam crop: {error}"))?;
        pipeline
            .add_many([&scale, &format, &crop])
            .map_err(|error| format!("could not build circular webcam pipeline: {error}"))?;
        gst::Element::link_many([&webcam_convert, &scale, &format, &crop])
            .map_err(|error| format!("could not connect circular webcam pipeline: {error}"))?;
        let output = crop
            .static_pad("src")
            .ok_or_else(|| "circular webcam crop has no output pad".to_owned())?;
        let weak_compositor = compositor.downgrade();
        output
            .add_probe(gst::PadProbeType::BUFFER, move |pad, probe| {
                if let Err(error) = mask_webcam_frame(pad, probe) {
                    if let Some(compositor) = weak_compositor.upgrade() {
                        gst::element_error!(
                            compositor,
                            gst::StreamError::Format,
                            ("could not process circular webcam frame: {error}")
                        );
                    }
                    return gst::PadProbeReturn::Drop;
                }
                gst::PadProbeReturn::Ok
            })
            .ok_or_else(|| "could not apply the circular webcam mask".to_owned())?;
        output
    } else {
        webcam_convert
            .static_pad("src")
            .ok_or_else(|| "webcam converter has no output pad".to_owned())?
    };

    let screen_pad = compositor
        .request_pad_simple("sink_%u")
        .ok_or_else(|| "could not create screen compositor input".to_owned())?;
    let webcam_pad = compositor
        .request_pad_simple("sink_%u")
        .ok_or_else(|| "could not create webcam compositor input".to_owned())?;
    screen_pad.set_property("zorder", 0u32);
    screen_pad.set_property("repeat-after-eos", true);
    webcam_pad.set_property("zorder", 1u32);
    webcam_pad.set_property("width", 1i32);
    webcam_pad.set_property("height", 1i32);
    webcam_pad.set_property_from_str("sizing-policy", "keep-aspect-ratio");

    let aggregator = compositor
        .clone()
        .downcast::<gst_base::Aggregator>()
        .map_err(|_| "webcam compositor is not an aggregator".to_owned())?;
    let aggregator_screen_pad = screen_pad
        .clone()
        .downcast::<gst_base::AggregatorPad>()
        .map_err(|_| "screen compositor input is not an aggregator pad".to_owned())?;
    let weak_screen_pad = aggregator_screen_pad.downgrade();
    let weak_webcam_gate = webcam_gate.downgrade();
    let screen_selected = Arc::new(AtomicBool::new(false));
    let selected_for_signal = Arc::clone(&screen_selected);
    aggregator.set_property("emit-signals", true);
    aggregator.connect_samples_selected(move |aggregator, _, _, _, _, _| {
        if !selected_for_signal.load(Ordering::Acquire)
            && weak_screen_pad
                .upgrade()
                .is_some_and(|pad| aggregator.peek_next_sample(&pad).is_some())
        {
            selected_for_signal.store(true, Ordering::Release);
            if let Some(webcam_gate) = weak_webcam_gate.upgrade() {
                webcam_gate.set_property("drop", false);
            }
        }
    });
    compositor
        .static_pad("src")
        .ok_or_else(|| "webcam compositor has no output pad".to_owned())?
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            if screen_selected.load(Ordering::Acquire) {
                gst::PadProbeReturn::Ok
            } else {
                gst::PadProbeReturn::Drop
            }
        })
        .ok_or_else(|| "could not wait for the first screen frame".to_owned())?;

    let screen_output = screen_convert
        .static_pad("src")
        .ok_or_else(|| "screen converter has no output pad".to_owned())?;
    let weak_webcam_pad = webcam_pad.downgrade();
    let weak_compositor = compositor.downgrade();
    screen_output
        .add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
            if let Some(gst::EventView::Caps(event)) = info.event().map(|event| event.view()) {
                let geometry = event.caps().structure(0).and_then(|caps| {
                    webcam_geometry(
                        caps.get::<i32>("width").ok()?,
                        caps.get::<i32>("height").ok()?,
                        layout,
                    )
                });
                if let Some(geometry) = geometry {
                    if let Some(webcam_pad) = weak_webcam_pad.upgrade() {
                        webcam_pad.set_property("xpos", geometry.x);
                        webcam_pad.set_property("ypos", geometry.y);
                        webcam_pad.set_property("width", geometry.width);
                        webcam_pad.set_property("height", geometry.height);
                    }
                } else {
                    if let Some(compositor) = weak_compositor.upgrade() {
                        gst::element_error!(
                            compositor,
                            gst::CoreError::Negotiation,
                            ("screen video dimensions are unavailable for the webcam overlay")
                        );
                    }
                    return gst::PadProbeReturn::Drop;
                }
            }
            gst::PadProbeReturn::Ok
        })
        .ok_or_else(|| "could not observe screen video dimensions".to_owned())?;
    screen_output
        .link(&screen_pad)
        .map_err(|error| format!("could not connect screen to webcam compositor: {error}"))?;
    webcam_output
        .link(&webcam_pad)
        .map_err(|error| format!("could not connect webcam to compositor: {error}"))?;
    Ok(compositor)
}

pub(super) fn finish_recording(pipeline: &gst::Pipeline) -> Result<(), String> {
    if pipeline
        .by_name("webcam-input-gate")
        .is_some_and(|gate| gate.property::<bool>("drop"))
    {
        let stop_result = pipeline
            .set_state(gst::State::Null)
            .map_err(|error| format!("could not stop recording pipeline: {error}"));
        return match stop_result {
            Ok(_) => Err("screen capture ended before receiving a video frame".to_owned()),
            Err(error) => Err(format!(
                "screen capture ended before receiving a video frame; {error}"
            )),
        };
    }
    let result = if pipeline.send_event(gst::event::Eos::new()) {
        pipeline
            .bus()
            .ok_or_else(|| "recording pipeline has no event bus".to_owned())
            .and_then(|bus| {
                bus.timed_pop_filtered(
                    gst::ClockTime::from_seconds(20),
                    &[gst::MessageType::Eos, gst::MessageType::Error],
                )
                .ok_or_else(|| "timed out while finalizing the recording".to_owned())
                .and_then(|message| match message.view() {
                    gst::MessageView::Eos(_) => Ok(()),
                    gst::MessageView::Error(error) => {
                        Err(format!("recording pipeline failed: {}", error.error()))
                    }
                    _ => Err("recording pipeline ended unexpectedly".to_owned()),
                })
            })
    } else {
        Err("could not finalize the recording".to_owned())
    };
    let stop_result = pipeline
        .set_state(gst::State::Null)
        .map_err(|error| format!("could not stop recording pipeline: {error}"));
    result.and(stop_result.map(|_| ()))
}

fn save_pipewire_frame(fd: i32, node_id: u32, path: &Path) -> Result<(), String> {
    let path = path.to_string_lossy().into_owned();
    let source = pipewire_source(fd, node_id)?;
    source.set_property("num-buffers", 1i32);
    let convert = gst::ElementFactory::make("videoconvert")
        .build()
        .map_err(|error| format!("could not create video converter: {error}"))?;
    let encoder = gst::ElementFactory::make("pngenc")
        .build()
        .map_err(|error| format!("could not create PNG encoder: {error}"))?;
    let sink = gst::ElementFactory::make("filesink")
        .property("location", path)
        .build()
        .map_err(|error| format!("could not create capture file: {error}"))?;
    let pipeline = gst::Pipeline::new();
    pipeline
        .add_many([&source, &convert, &encoder, &sink])
        .map_err(|error| format!("could not build screen capture pipeline: {error}"))?;
    gst::Element::link_many([&source, &convert, &encoder, &sink])
        .map_err(|error| format!("could not connect screen capture pipeline: {error}"))?;

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|error| format!("could not start screen capture pipeline: {error}"))?;
    let result = pipeline
        .bus()
        .ok_or_else(|| "screen capture pipeline has no event bus".to_owned())?
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(Duration::from_secs(15).as_secs()),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .ok_or_else(|| "timed out waiting for a screen capture frame".to_owned())
        .and_then(|message| match message.view() {
            gst::MessageView::Eos(_) => Ok(()),
            gst::MessageView::Error(error) => {
                Err(format!("screen capture pipeline failed: {}", error.error()))
            }
            _ => Err("screen capture pipeline ended unexpectedly".to_owned()),
        });
    let stop_result = pipeline
        .set_state(gst::State::Null)
        .map_err(|error| format!("could not stop screen capture pipeline: {error}"));
    result.and(stop_result.map(|_| ()))
}

pub(super) fn pipewire_source(fd: i32, node_id: u32) -> Result<gst::Element, String> {
    let source = gst::ElementFactory::make("pipewiresrc")
        .property("fd", fd)
        .property("path", node_id.to_string())
        .build()
        .map_err(|error| format!("could not create PipeWire capture source: {error}"))?;
    crate::linux::portal_frame::guard_screen_frames(&source)?;
    Ok(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delayed_capture_waits_and_can_be_cancelled() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let started = std::time::Instant::now();
        let mut waiting = false;
        assert_eq!(
            wait_for_capture_delay(Duration::from_millis(80), &receiver, || waiting = true),
            Ok(CaptureDelayOutcome::Elapsed)
        );
        assert!(waiting, "the countdown should be announced");
        assert!(
            started.elapsed() >= Duration::from_millis(80),
            "a delayed capture must not happen early"
        );
        sender.send(()).expect("cancellation should be accepted");
        let mut waiting = false;
        assert_eq!(
            wait_for_capture_delay(Duration::from_secs(2), &receiver, || waiting = true),
            Ok(CaptureDelayOutcome::Cancelled)
        );
        assert!(
            !waiting,
            "a prior cancellation should not announce a countdown"
        );

        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let (waiting_sender, waiting_receiver) = std::sync::mpsc::sync_channel(1);
        let canceller = std::thread::spawn(move || {
            waiting_receiver.recv().expect("countdown should start");
            sender.send(()).expect("cancellation should be accepted");
        });
        let started = std::time::Instant::now();
        assert_eq!(
            wait_for_capture_delay(Duration::from_secs(5), &receiver, || {
                waiting_sender
                    .send(())
                    .expect("countdown should be announced")
            }),
            Ok(CaptureDelayOutcome::Cancelled)
        );
        canceller.join().expect("canceller should finish");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "cancellation must interrupt the countdown"
        );
    }

    #[test]
    fn delayed_capture_reports_a_disconnected_cancellation_channel() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        drop(sender);
        let error = wait_for_capture_delay(Duration::from_secs(1), &receiver, || {})
            .expect_err("a disconnected cancellation channel must be reported");
        assert!(error.contains("cancellation channel closed"), "{error}");
    }

    #[test]
    fn portal_close_failures_and_capture_failures_are_reported() {
        use crate::linux::portal::combine_session_results;
        assert_eq!(combine_session_results(Ok(7), Ok(())), Ok(7));
        assert_eq!(
            combine_session_results::<u8>(Err("capture failed".into()), Ok(())),
            Err("capture failed".into())
        );
        assert_eq!(
            combine_session_results(Ok(7), Err("close failed".into())),
            Err("close failed".into())
        );
        assert_eq!(
            combine_session_results::<u8>(Err("capture failed".into()), Err("close failed".into())),
            Err("capture failed; close failed".into())
        );
    }

    #[test]
    fn recording_session_handle_accepts_portal_string_and_object_path_responses() {
        use zbus::zvariant::{ObjectPath, OwnedValue, Value};

        let expected = "/org/freedesktop/portal/desktop/session/1_2/test";
        for value in [
            Value::from(expected),
            Value::from(ObjectPath::try_from(expected).expect("test path should be valid")),
        ] {
            let response = HashMap::from([(
                "session_handle".to_owned(),
                OwnedValue::try_from(value).expect("test value should be owned"),
            )]);
            assert_eq!(
                recording_session_handle(&response)
                    .expect("portal session handle should be accepted")
                    .as_str(),
                expected
            );
        }
    }

    #[test]
    fn recording_session_handle_rejects_missing_invalid_or_wrong_type_values() {
        use zbus::zvariant::{OwnedValue, Value};

        let missing = HashMap::new();
        let invalid = HashMap::from([(
            "session_handle".to_owned(),
            OwnedValue::try_from(Value::from("not/an/object/path"))
                .expect("test value should be owned"),
        )]);
        let wrong_type = HashMap::from([(
            "session_handle".to_owned(),
            OwnedValue::try_from(Value::from(42u32)).expect("test value should be owned"),
        )]);
        for response in [missing, invalid, wrong_type] {
            assert!(
                recording_session_handle(&response).is_err(),
                "invalid portal session handles must be reported"
            );
        }
    }

    fn assert_expected_desktop_color(document: &jiffshot_doc::Document) {
        let Ok(value) = std::env::var("JIFFSHOT_PORTAL_TEST_RGB") else {
            return;
        };
        let expected =
            u32::from_str_radix(value.trim_start_matches('#'), 16).expect("test RGB must be hex");
        assert_desktop_color(document, expected);
    }

    fn assert_desktop_color(document: &jiffshot_doc::Document, expected: u32) {
        assert!(expected <= 0x00ff_ffff, "test RGB must have three channels");
        let expected = expected.to_be_bytes();
        let (width, height) = document.size();
        let pixmap = document.flatten();
        let pixel = pixmap
            .pixel(width / 4, height / 4)
            .expect("captured desktop should contain the test pixel")
            .demultiply();
        let actual = [pixel.red(), pixel.green(), pixel.blue()];
        for (actual, expected) in actual.into_iter().zip(expected[1..].iter().copied()) {
            assert!(
                actual.abs_diff(expected) <= 12,
                "captured desktop color differs: actual {actual}, expected {expected}"
            );
        }
    }

    #[test]
    fn pipewire_source_configures_the_node_id_as_a_string() {
        gst::init().expect("GStreamer should initialize");
        let source = pipewire_source(-1, u32::MAX).expect("PipeWire source should be created");
        assert_eq!(source.property::<i32>("fd"), -1);
        assert_eq!(source.property::<String>("path"), u32::MAX.to_string());
    }

    #[test]
    fn recording_audio_selection_uses_default_monitor_and_microphone() {
        gst::init().expect("GStreamer should initialize");
        let silent = recording_audio_sources(RecordingAudio::Disabled)
            .expect("silent recording should not need audio sources");
        assert!(silent.is_empty());
        let microphone = recording_audio_sources(RecordingAudio::Microphone)
            .expect("microphone source should be created");
        assert_eq!(microphone.len(), 1);
        assert_eq!(microphone[0].name(), "microphone-audio");
        assert_eq!(microphone[0].property::<Option<String>>("device"), None);
        let system = recording_audio_sources(RecordingAudio::System)
            .expect("system audio source should be created");
        assert_eq!(system.len(), 1);
        assert_eq!(system[0].name(), "system-audio");
        assert_eq!(
            system[0].property::<Option<String>>("device").as_deref(),
            Some("@DEFAULT_MONITOR@")
        );
        let both = recording_audio_sources(RecordingAudio::SystemAndMicrophone)
            .expect("both audio sources should be created");
        assert_eq!(
            both.iter().map(|source| source.name()).collect::<Vec<_>>(),
            ["system-audio", "microphone-audio"]
        );
    }

    #[test]
    fn pending_recording_setup_ignores_pause_and_restart_until_cancelled() {
        let (commands, receiver) = async_channel::bounded(3);
        commands
            .send_blocking(RecordingCommand::TogglePause)
            .expect("pause should be queued");
        commands
            .send_blocking(RecordingCommand::Restart)
            .expect("restart should be queued");
        commands
            .send_blocking(RecordingCommand::Cancel)
            .expect("cancel should be queued");
        let mut states = Vec::new();
        let result = futures_lite::future::block_on(wait_for_recording_setup(
            std::future::pending::<()>(),
            &receiver,
            &mut |state| states.push(state),
        ));
        assert_eq!(result, None, "cancellation must end pending setup");
        assert_eq!(
            states,
            [
                RecordingState::Starting,
                RecordingState::Starting,
                RecordingState::Cancelling,
            ],
            "pause and restart must not start a pending recording"
        );
    }

    #[test]
    fn cancelling_before_the_first_frame_stops_without_an_empty_video_error() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join("cancelled.mp4");
        let source = gstreamer_app::AppSrc::builder()
            .caps(
                &gst::Caps::builder("video/x-raw")
                    .field("format", "RGBA")
                    .field("width", 320i32)
                    .field("height", 240i32)
                    .field("framerate", gst::Fraction::new(30, 1))
                    .build(),
            )
            .format(gst::Format::Time)
            .is_live(true)
            .build();
        let pipeline = recording_pipeline(source.upcast(), &path)
            .expect("recording pipeline should be constructible");
        let observed_pipeline = pipeline.clone();
        let (commands, receiver) = async_channel::bounded(1);
        let (states, observed) = std::sync::mpsc::channel();
        let recorder = std::thread::spawn(move || {
            run_recording(&pipeline, receiver, |state| {
                states
                    .send(state)
                    .expect("test should receive recording states")
            })
        });
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(10))
                .expect("recording should report its state"),
            RecordingState::Recording
        );
        commands
            .send_blocking(RecordingCommand::Cancel)
            .expect("cancel should be accepted");
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(10))
                .expect("recording should report cancellation"),
            RecordingState::Cancelling
        );
        assert_eq!(
            recorder
                .join()
                .expect("recording thread should finish")
                .expect("cancellation should not require a video frame"),
            RecordingRun::Cancelled
        );
        assert_eq!(
            observed_pipeline.current_state(),
            gst::State::Null,
            "cancellation must release the pipeline"
        );
    }

    #[test]
    #[ignore = "requires a ScreenCast portal and PipeWire desktop session"]
    fn portal_delayed_capture_waits_and_cancels() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let color_change = std::env::var_os("JIFFSHOT_DELAY_COLOR_FILE").map(|path| {
            let (ready, waiting) = std::sync::mpsc::sync_channel(1);
            let worker = std::thread::spawn(move || {
                waiting.recv().expect("countdown should start");
                std::thread::sleep(Duration::from_millis(150));
                std::fs::write(path, "804020").expect("desktop color should change");
            });
            (ready, worker)
        });
        let mut started = None;
        let document = futures_lite::future::block_on(capture_monitor_after_delay(
            CaptureCursor::Included,
            Duration::from_millis(700),
            &receiver,
            || {
                started = Some(std::time::Instant::now());
                if let Some((ready, _)) = &color_change {
                    ready.send(()).expect("color change should start");
                }
            },
            || {},
        ))
        .expect("delayed portal capture should succeed")
        .expect("the countdown should elapse");
        assert!(
            started.expect("countdown should start").elapsed() >= Duration::from_millis(700),
            "capture must wait after portal selection"
        );
        if let Some((_, worker)) = color_change {
            worker.join().expect("desktop color change should finish");
            assert_desktop_color(&document, 0x804020);
        } else {
            assert_expected_desktop_color(&document);
        }
        drop(sender);

        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let (waiting_sender, waiting_receiver) = std::sync::mpsc::sync_channel(1);
        let canceller = std::thread::spawn(move || {
            waiting_receiver.recv().expect("countdown should start");
            sender.send(()).expect("cancellation should be accepted");
        });
        let started = std::time::Instant::now();
        let cancelled = futures_lite::future::block_on(capture_monitor_after_delay(
            CaptureCursor::Included,
            Duration::from_secs(5),
            &receiver,
            || {
                waiting_sender
                    .send(())
                    .expect("countdown should be announced")
            },
            || {},
        ))
        .expect("cancelled portal capture should close its session");
        canceller.join().expect("canceller should finish");
        assert!(
            cancelled.is_none(),
            "cancellation must not produce an image"
        );
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "cancellation should not wait for the full countdown"
        );
    }

    #[test]
    #[ignore = "requires a ScreenCast portal with a visible pointer at (300, 200)"]
    fn portal_cursor_visibility_changes_capture_and_recording_pixels() {
        fn cursor_pixel_difference(
            included: &jiffshot_doc::Document,
            hidden: &jiffshot_doc::Document,
        ) {
            assert_eq!(included.size(), hidden.size());
            let included = included.flatten();
            let hidden = hidden.flatten();
            let mut changed = 0;
            let mut hidden_marks = 0;
            let background = hidden
                .pixel(280, 180)
                .expect("capture should contain the background")
                .demultiply();
            for y in 200..250 {
                for x in 300..350 {
                    if included.pixel(x, y) != hidden.pixel(x, y) {
                        changed += 1;
                    }
                    let pixel = hidden
                        .pixel(x, y)
                        .expect("capture should contain the pointer area")
                        .demultiply();
                    if pixel.red().abs_diff(background.red()) > 20
                        || pixel.green().abs_diff(background.green()) > 20
                        || pixel.blue().abs_diff(background.blue()) > 20
                    {
                        hidden_marks += 1;
                    }
                }
            }
            assert!(
                changed >= 10,
                "the requested cursor should change pixels near (300, 200), got {changed}"
            );
            assert!(
                hidden_marks < 10,
                "the hidden-cursor capture should contain only the plain background, got {hidden_marks} marked pixels"
            );
        }

        fn record(cursor: CaptureCursor) -> jiffshot_doc::Document {
            let (sender, receiver) = async_channel::bounded(1);
            let stopper = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(2));
                sender
                    .send_blocking(RecordingCommand::Stop)
                    .expect("recording should accept stop")
            });
            let path = futures_lite::future::block_on(record_monitor_with_audio(
                receiver,
                RecordingAudio::Disabled,
                RecordingWebcam::Disabled,
                cursor,
                RecordingFrameRate::Source,
                |_| {},
            ))
            .expect("portal recording should succeed")
            .expect("portal recording should be saved");
            stopper.join().expect("recording stopper should finish");
            let frame = crate::linux::video::frame_at(&path, 0.5)
                .expect("recording should contain a decodable frame");
            std::fs::remove_file(path).expect("test recording should be removed");
            frame
        }

        let included = futures_lite::future::block_on(capture_monitor(CaptureCursor::Included))
            .expect("portal screenshot should include the cursor");
        // The headless compositor can reuse the previous frame until it repaints.
        std::thread::sleep(Duration::from_millis(300));
        let hidden = futures_lite::future::block_on(capture_monitor(CaptureCursor::Hidden))
            .expect("portal screenshot should hide the cursor");
        cursor_pixel_difference(&included, &hidden);
        cursor_pixel_difference(
            &record(CaptureCursor::Included),
            &record(CaptureCursor::Hidden),
        );
    }

    #[test]
    #[ignore = "requires a ScreenCast portal and PipeWire desktop session"]
    fn portal_capture_and_recording_produce_decodable_media() {
        let document = futures_lite::future::block_on(capture_monitor(CaptureCursor::Included))
            .expect("portal screenshot should succeed");
        let (width, height) = document.size();
        assert!(
            width >= 320 && height >= 200,
            "portal screenshot should contain a desktop-sized image, got {width}x{height}"
        );
        assert_expected_desktop_color(&document);
        let hidden = futures_lite::future::block_on(capture_monitor(CaptureCursor::Hidden))
            .expect("portal screenshot without a cursor should succeed");
        assert_eq!(hidden.size(), document.size());
        assert_expected_desktop_color(&hidden);
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let screenshot =
            save_capture_to(&document, directory.path()).expect("portal screenshot should save");
        assert_eq!(
            jiffshot_doc::io::open(&screenshot)
                .expect("portal screenshot should reopen")
                .size(),
            (width, height)
        );

        let (send_stop, stop) = async_channel::bounded(1);
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            send_stop.send_blocking(RecordingCommand::Stop)
        });
        let recording = futures_lite::future::block_on(record_monitor_with_audio(
            stop,
            RecordingAudio::Disabled,
            RecordingWebcam::Disabled,
            CaptureCursor::Included,
            RecordingFrameRate::Source,
            |_| {},
        ));
        let stop_result = stopper.join().expect("recording stopper should finish");
        let recording = recording
            .expect("portal recording should finalize")
            .expect("portal recording should be saved");
        stop_result.expect("recording should still accept a stop request");
        let frame = crate::linux::video::frame_at(&recording, 0.5)
            .expect("recording should contain a decodable frame after half a second");
        assert_eq!(frame.size(), (width, height));
        assert_expected_desktop_color(&frame);
        std::fs::remove_file(&recording).expect("test recording should be removed");
    }

    #[test]
    #[ignore = "requires a ScreenCast portal and PipeWire desktop session"]
    fn portal_recording_cancel_discards_the_staged_file() {
        let directory = capture_directory().expect("capture directory should be available");
        std::fs::create_dir_all(&directory).expect("capture directory should be creatable");
        let entries = || {
            let mut names = std::fs::read_dir(&directory)
                .expect("capture directory should be readable")
                .map(|entry| {
                    entry
                        .expect("directory entry should be readable")
                        .file_name()
                })
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        let before = entries();
        let (commands, receiver) = async_channel::bounded(2);
        let (states, observed) = std::sync::mpsc::channel();
        let controller = std::thread::spawn(move || {
            let next_state = || {
                observed
                    .recv_timeout(Duration::from_secs(20))
                    .expect("portal recording should report its state")
            };
            assert_eq!(next_state(), RecordingState::Recording);
            std::thread::sleep(Duration::from_millis(800));
            commands
                .send_blocking(RecordingCommand::TogglePause)
                .expect("pause should be accepted");
            assert_eq!(next_state(), RecordingState::Paused);
            commands
                .send_blocking(RecordingCommand::Cancel)
                .expect("cancel should be accepted while paused");
            assert_eq!(next_state(), RecordingState::Cancelling);
        });
        let result = futures_lite::future::block_on(record_monitor_with_audio(
            receiver,
            RecordingAudio::Disabled,
            RecordingWebcam::Disabled,
            CaptureCursor::Hidden,
            RecordingFrameRate::Source,
            move |state| {
                states
                    .send(state)
                    .expect("portal test should receive recording states")
            },
        ));
        controller
            .join()
            .expect("recording controller should finish");
        assert_eq!(
            result.expect("cancellation should close the portal session"),
            None,
            "cancelled recordings must not have a saved path"
        );
        assert_eq!(
            entries(),
            before,
            "cancellation must remove the staged file and its temporary directory"
        );
    }

    #[test]
    #[ignore = "requires a ScreenCast portal and test system/microphone tones at 440Hz and 880Hz"]
    fn portal_recording_restart_discards_previous_frames_and_restores_sources() {
        gst::init().expect("GStreamer should initialize");
        let (commands, receiver) = async_channel::bounded(4);
        let (states, observed) = std::sync::mpsc::channel();
        let controller = std::thread::spawn(move || {
            let next_state = || {
                observed
                    .recv_timeout(Duration::from_secs(20))
                    .expect("portal recording should report its state")
            };
            assert_eq!(next_state(), RecordingState::Recording);
            std::thread::sleep(Duration::from_millis(2500));
            commands
                .send_blocking(RecordingCommand::TogglePause)
                .expect("pause should be accepted");
            assert_eq!(next_state(), RecordingState::Paused);
            commands
                .send_blocking(RecordingCommand::Restart)
                .expect("restart should be accepted while paused");
            assert_eq!(next_state(), RecordingState::Restarting);
            assert_eq!(next_state(), RecordingState::Recording);
            std::thread::sleep(Duration::from_millis(1100));
            commands
                .send_blocking(RecordingCommand::Stop)
                .expect("stop should be accepted after restart");
            assert_eq!(next_state(), RecordingState::Stopping);
        });
        let result = futures_lite::future::block_on(record_portal_sources(
            receiver,
            RecordingAudio::SystemAndMicrophone,
            CaptureCursor::Included,
            RecordingFrameRate::Source,
            || {
                gst::ElementFactory::make("videotestsrc")
                    .property("is-live", true)
                    .property_from_str("pattern", "blue")
                    .build()
                    .map(Some)
                    .map_err(|error| error.to_string())
            },
            WebcamLayout::default(),
            move |state| {
                states
                    .send(state)
                    .expect("portal test should receive recording states")
            },
        ));
        controller
            .join()
            .expect("recording controller should finish");
        let recording = result
            .expect("restarted recording should finalize")
            .expect("restarted recording should be saved");
        let uri =
            gst::glib::filename_to_uri(&recording, None).expect("recording should have a URI");
        let discoverer = gstreamer_pbutils::Discoverer::new(gst::ClockTime::from_seconds(10))
            .expect("media discoverer should be available");
        let duration = discoverer
            .discover_uri(uri.as_str())
            .expect("recording should be discoverable")
            .duration()
            .expect("recording should report its duration")
            .mseconds();
        assert!(
            (700..=1900).contains(&duration),
            "restart must discard the previous 2.5-second segment, got {duration}ms"
        );
        let first = crate::linux::video::first_frame(&recording)
            .expect("restarted recording should contain a first frame");
        let frame = crate::linux::video::frame_at(&recording, 0.5)
            .expect("restarted recording should contain a new frame");
        assert_eq!(first.size(), frame.size());
        assert_expected_desktop_color(&frame);
        let last_timestamp = last_recorded_video_timestamp(&recording);
        assert!(
            last_timestamp > gst::ClockTime::from_mseconds(700),
            "restarted recording should retain its final screen frame"
        );
        let last = crate::linux::video::frame_at(
            &recording,
            last_timestamp.nseconds() as f64 / 1_000_000_000.0,
        )
        .expect("restarted recording should contain a final frame");
        assert_expected_desktop_color(&last);
        let (width, height) = frame.size();
        let geometry = webcam_geometry(width as i32, height as i32, WebcamLayout::default())
            .expect("desktop dimensions should be valid");
        let pixel = frame
            .flatten()
            .pixel(
                (geometry.x + geometry.width / 2) as u32,
                (geometry.y + geometry.height / 2) as u32,
            )
            .expect("webcam overlay should be inside the recording")
            .demultiply();
        assert!(
            pixel.blue() > 200 && pixel.red() < 60,
            "restart should restore the blue webcam overlay: {pixel:?}"
        );
        let audio = decode_recording_audio(&recording);
        assert_audio_frequency(&audio, 440.0);
        assert_audio_frequency(&audio, 880.0);
        std::fs::remove_file(&recording).expect("test recording should be removed");
    }

    #[test]
    #[ignore = "requires a ScreenCast portal and test system/microphone tones at 440Hz and 880Hz"]
    fn portal_recording_captures_system_audio_without_microphone() {
        let recording = record_from_portal(RecordingAudio::System);
        let audio = decode_recording_audio(&recording);
        assert_audio_frequency(&audio, 440.0);
        assert!(
            audio_frequency_amplitude(&audio, 880.0) < 0.03,
            "system-only recording should not contain the microphone tone"
        );
        std::fs::remove_file(&recording).expect("test recording should be removed");
    }

    #[test]
    #[ignore = "requires a ScreenCast portal and test system/microphone tones at 440Hz and 880Hz"]
    fn portal_recording_captures_microphone_without_system_audio() {
        let recording = record_from_portal(RecordingAudio::Microphone);
        let audio = decode_recording_audio(&recording);
        assert_audio_frequency(&audio, 880.0);
        assert!(
            audio_frequency_amplitude(&audio, 440.0) < 0.03,
            "microphone-only recording should not contain the system tone"
        );
        std::fs::remove_file(&recording).expect("test recording should be removed");
    }

    #[test]
    #[ignore = "requires a ScreenCast portal and test system/microphone tones at 440Hz and 880Hz"]
    fn portal_recording_mixes_live_system_and_microphone_audio() {
        let recording = record_from_portal(RecordingAudio::SystemAndMicrophone);
        let audio = decode_recording_audio(&recording);
        assert_audio_frequency(&audio, 440.0);
        assert_audio_frequency(&audio, 880.0);
        let frame = crate::linux::video::frame_at(&recording, 0.5)
            .expect("recording with audio should contain a decodable frame");
        assert_expected_desktop_color(&frame);
        std::fs::remove_file(&recording).expect("test recording should be removed");
    }

    #[test]
    #[ignore = "requires a ScreenCast portal and test system/microphone tones at 440Hz and 880Hz"]
    fn portal_recording_pause_preserves_both_live_audio_sources() {
        let (commands, receiver) = async_channel::bounded(8);
        let (states, observed) = std::sync::mpsc::channel();
        let controller = std::thread::spawn(move || {
            let next_state = || {
                observed
                    .recv_timeout(Duration::from_secs(20))
                    .expect("portal recording should report its state")
            };
            assert_eq!(next_state(), RecordingState::Recording);
            std::thread::sleep(Duration::from_millis(1000));
            commands
                .send_blocking(RecordingCommand::TogglePause)
                .expect("portal recording should accept pause");
            assert_eq!(next_state(), RecordingState::Paused);
            std::thread::sleep(Duration::from_millis(2500));
            commands
                .send_blocking(RecordingCommand::TogglePause)
                .expect("portal recording should accept resume");
            assert_eq!(next_state(), RecordingState::Recording);
            std::thread::sleep(Duration::from_millis(1000));
            commands
                .send_blocking(RecordingCommand::Stop)
                .expect("portal recording should accept stop");
            assert_eq!(next_state(), RecordingState::Stopping);
        });
        let recording = futures_lite::future::block_on(record_monitor_with_audio(
            receiver,
            RecordingAudio::SystemAndMicrophone,
            RecordingWebcam::Disabled,
            CaptureCursor::Included,
            RecordingFrameRate::Source,
            move |state| {
                states
                    .send(state)
                    .expect("portal test should receive recording states")
            },
        ));
        controller
            .join()
            .expect("portal recording controller should finish");
        let recording = recording
            .expect("paused portal recording should finalize")
            .expect("paused portal recording should be saved");
        let uri =
            gst::glib::filename_to_uri(&recording, None).expect("recording should have a URI");
        let discoverer = gstreamer_pbutils::Discoverer::new(gst::ClockTime::from_seconds(10))
            .expect("media discoverer should be available");
        let duration = discoverer
            .discover_uri(uri.as_str())
            .expect("recording should be discoverable")
            .duration()
            .expect("recording should report its duration")
            .mseconds();
        assert!(
            (1600..=3000).contains(&duration),
            "portal recording should exclude the 2.5-second pause, got {duration}ms"
        );
        for time in [0.2, 1.5] {
            let frame = crate::linux::video::frame_at(&recording, time)
                .expect("portal recording should contain frames around the pause");
            assert_expected_desktop_color(&frame);
        }
        assert!(
            last_recorded_video_timestamp(&recording) >= gst::ClockTime::from_mseconds(1400),
            "portal recording must contain new video frames after resuming"
        );
        let audio = decode_recording_audio(&recording);
        assert_audio_frequency(&audio, 440.0);
        assert_audio_frequency(&audio, 880.0);
        let resumed_audio = audio
            .get(67_200..)
            .expect("portal recording should contain audio after the pause");
        assert_audio_frequency(resumed_audio, 440.0);
        assert_audio_frequency(resumed_audio, 880.0);
        std::fs::remove_file(&recording).expect("test recording should be removed");
    }

    #[test]
    #[ignore = "requires a ScreenCast portal and test system/microphone tones at 440Hz and 880Hz"]
    fn portal_recording_composites_webcam_and_mixes_live_audio() {
        gst::init().expect("GStreamer should initialize");
        let (send_stop, stop) = async_channel::bounded(1);
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            send_stop.send_blocking(RecordingCommand::Stop)
        });
        let recording = futures_lite::future::block_on(record_portal_sources(
            stop,
            RecordingAudio::SystemAndMicrophone,
            CaptureCursor::Included,
            RecordingFrameRate::Source,
            || {
                gst::ElementFactory::make("videotestsrc")
                    .property("is-live", true)
                    .property_from_str("pattern", "blue")
                    .build()
                    .map(Some)
                    .map_err(|error| error.to_string())
            },
            WebcamLayout::default(),
            |_| {},
        ));
        let stop_result = stopper.join().expect("recording stopper should finish");
        let recording = recording
            .expect("portal recording with webcam should finalize")
            .expect("portal recording with webcam should be saved");
        stop_result.expect("recording should still accept a stop request");

        let first = crate::linux::video::first_frame(&recording)
            .expect("webcam recording should contain a decodable first frame");
        assert_expected_desktop_color(&first);
        let frame = crate::linux::video::frame_at(&recording, 0.8)
            .expect("webcam recording should contain a decodable frame");
        assert_expected_desktop_color(&frame);
        let (width, height) = frame.size();
        let geometry = webcam_geometry(width as i32, height as i32, WebcamLayout::default())
            .expect("desktop dimensions should be valid");
        let pixel = frame
            .flatten()
            .pixel(
                (geometry.x + geometry.width / 2) as u32,
                (geometry.y + geometry.height / 2) as u32,
            )
            .expect("webcam overlay should be inside the recording")
            .demultiply();
        assert!(
            pixel.blue() > 200 && pixel.red() < 60,
            "recording should contain the blue webcam overlay: {pixel:?}"
        );
        let audio = decode_recording_audio(&recording);
        assert_audio_frequency(&audio, 440.0);
        assert_audio_frequency(&audio, 880.0);
        std::fs::remove_file(&recording).expect("test recording should be removed");
    }

    #[test]
    #[ignore = "requires the 1280x720 Sway ScreenCast portal, desktop-color fixture, and PipeWire tones"]
    fn portal_recording_places_and_masks_a_circular_webcam() {
        gst::init().expect("GStreamer should initialize");
        let expected_rgb = std::env::var("JIFFSHOT_PORTAL_TEST_RGB")
            .expect("the real-portal desktop color fixture must be configured");
        let expected_rgb = u32::from_str_radix(expected_rgb.trim_start_matches('#'), 16)
            .expect("the real-portal desktop color must be hexadecimal");
        assert!(expected_rgb <= 0x00ff_ffff, "desktop color must be RGB");
        let expected = expected_rgb.to_be_bytes();
        let (send_stop, stop) = async_channel::bounded(1);
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            send_stop.send_blocking(RecordingCommand::Stop)
        });
        let layout = WebcamLayout::new(WebcamCorner::TopRight, 40)
            .expect("40 percent is valid")
            .with_shape(WebcamShape::Circle);
        let recording = futures_lite::future::block_on(record_portal_sources(
            stop,
            RecordingAudio::SystemAndMicrophone,
            CaptureCursor::Hidden,
            RecordingFrameRate::Source,
            || {
                gst::ElementFactory::make("videotestsrc")
                    .property("is-live", true)
                    .property_from_str("pattern", "blue")
                    .build()
                    .map(Some)
                    .map_err(|error| error.to_string())
            },
            layout,
            |_| {},
        ));
        let stop_result = stopper.join().expect("recording stopper should finish");
        let recording = recording
            .expect("portal circular webcam recording should finalize")
            .expect("portal circular webcam recording should be saved");
        stop_result.expect("recording should still accept a stop request");

        let frame = crate::linux::video::frame_at(&recording, 0.8)
            .expect("portal circular webcam recording should contain a decodable frame");
        assert_eq!(frame.size(), (1280, 720), "Sway fixture dimensions changed");
        assert_expected_desktop_color(&frame);
        let pixmap = frame.flatten();
        let pixel = |x, y| {
            pixmap
                .pixel(x, y)
                .expect("test pixel should be inside the portal recording")
                .demultiply()
        };
        // For a 1280x720 desktop, a 40% circle is 288 pixels wide at (976, 16).
        for (x, y) in [
            (1120, 160),
            (986, 160),
            (1253, 160),
            (1120, 26),
            (1120, 293),
        ] {
            let camera = pixel(x, y);
            assert!(
                camera.blue() > 200 && camera.red() < 60 && camera.green() < 60,
                "camera should occupy ({x}, {y}) within the expected circle: {camera:?}"
            );
        }
        for (x, y) in [
            (976, 16),
            (1263, 16),
            (976, 303),
            (1263, 303),
            (980, 20),
            (1259, 20),
            (980, 299),
            (1259, 299),
            (968, 160),
            (1272, 160),
            (1120, 8),
            (1120, 312),
        ] {
            let background = pixel(x, y);
            for (actual, expected) in [background.red(), background.green(), background.blue()]
                .into_iter()
                .zip(expected[1..].iter().copied())
            {
                assert!(
                    actual.abs_diff(expected) <= 12,
                    "circle corner or outside ({x}, {y}) must retain the desktop color: {background:?}"
                );
            }
        }
        let audio = decode_recording_audio(&recording);
        assert_audio_frequency(&audio, 440.0);
        assert_audio_frequency(&audio, 880.0);
        std::fs::remove_file(&recording).expect("test recording should be removed");
    }

    fn record_from_portal(audio: RecordingAudio) -> PathBuf {
        let (send_stop, stop) = async_channel::bounded(1);
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            send_stop.send_blocking(RecordingCommand::Stop)
        });
        let recording = futures_lite::future::block_on(record_monitor_with_audio(
            stop,
            audio,
            RecordingWebcam::Disabled,
            CaptureCursor::Included,
            RecordingFrameRate::Source,
            |_| {},
        ));
        let stop_result = stopper.join().expect("recording stopper should finish");
        let recording = recording
            .expect("portal recording with audio should finalize")
            .expect("portal recording with audio should be saved");
        stop_result.expect("recording should still accept a stop request");
        recording
    }

    #[test]
    fn save_capture_writes_a_reopenable_png() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let document = jiffshot_doc::Document::blank(3, 2, jiffshot_doc::Rgba::WHITE);
        let capture_directory = directory.path().join("Jiffshot");
        let path = save_capture_to(&document, &capture_directory).expect("capture should save");
        let reopened = jiffshot_doc::io::open(&path).expect("saved capture should reopen");
        assert_eq!(reopened.size(), (3, 2));
        assert_eq!(path.parent(), Some(capture_directory.as_path()));
    }

    #[test]
    fn recent_captures_limits_results_to_supported_capture_files() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let set_modified = |path: &Path, seconds| {
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .expect("capture should be reopenable")
                .set_times(
                    std::fs::FileTimes::new()
                        .set_modified(UNIX_EPOCH + Duration::from_secs(seconds)),
                )
                .expect("capture modification time should be set");
        };
        for index in 0..10 {
            let path = directory.path().join(format!("Screenshot-{index}.png"));
            std::fs::write(&path, b"capture").expect("capture fixture should be written");
            set_modified(&path, index);
        }
        let recording_path = directory.path().join("Recording.mp4");
        std::fs::write(&recording_path, b"recording").expect("recording fixture should be written");
        set_modified(&recording_path, 20);
        std::fs::write(directory.path().join("notes.txt"), b"not a capture")
            .expect("unrelated fixture should be written");
        std::fs::create_dir(directory.path().join("nested.png"))
            .expect("nested fixture should be created");

        let captures = recent_captures_in(directory.path()).expect("captures should be listed");
        assert_eq!(captures.len(), RECENT_CAPTURE_LIMIT);
        assert!(
            captures.iter().all(|path| matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("png" | "mp4")
            )),
            "the library should omit unsupported files and directories"
        );
        assert_eq!(
            captures[0].file_name().and_then(|name| name.to_str()),
            Some("Recording.mp4"),
            "newest recording should appear first"
        );
        assert_eq!(
            captures[1].file_name().and_then(|name| name.to_str()),
            Some("Screenshot-9.png"),
            "screenshots should be ordered newest first"
        );
        assert!(
            !captures
                .iter()
                .any(|path| path.ends_with("Screenshot-2.png")),
            "captures older than the eight newest should be omitted"
        );
    }

    #[test]
    fn recording_timeline_excludes_each_pause_at_its_boundaries() {
        let ms = gst::ClockTime::from_mseconds;
        let mut timeline = RecordingTimeline::default();
        timeline.pause(ms(100)).expect("first pause should start");
        assert_eq!(timeline.timestamp(ms(99)), Some(ms(99)));
        assert_eq!(timeline.timestamp(ms(100)), None);
        assert_eq!(timeline.timestamp(ms(250)), None);
        timeline.resume(ms(300)).expect("first pause should end");
        assert_eq!(timeline.timestamp(ms(299)), None);
        assert_eq!(timeline.timestamp(ms(300)), Some(ms(100)));
        assert_eq!(timeline.timestamp(ms(400)), Some(ms(200)));
        timeline.pause(ms(500)).expect("second pause should start");
        assert_eq!(timeline.timestamp(ms(500)), None);
        timeline.resume(ms(750)).expect("second pause should end");
        assert_eq!(timeline.timestamp(ms(749)), None);
        assert_eq!(timeline.timestamp(ms(750)), Some(ms(300)));
        assert_eq!(timeline.timestamp(ms(900)), Some(ms(450)));
        assert_eq!(
            timeline.timestamp(ms(400)),
            Some(ms(200)),
            "a delayed buffer from an earlier interval should retain its original mapping"
        );
        assert!(
            timeline.pause(ms(749)).is_err(),
            "a pause cannot start before the previous pause ended"
        );
        assert!(
            timeline.resume(ms(800)).is_err(),
            "a completed pause cannot end twice"
        );
    }

    #[test]
    fn recording_pause_excludes_paused_video_and_audio() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join("paused-recording.mp4");
        let video = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .property_from_str("pattern", "red")
            .build()
            .expect("test video source should be available");
        let audio = gst::ElementFactory::make("audiotestsrc")
            .property("is-live", true)
            .property("do-timestamp", true)
            .property("freq", 440.0f64)
            .property("volume", 0.3f64)
            .build()
            .expect("test audio source should be available");
        let second_audio = gst::ElementFactory::make("audiotestsrc")
            .property("is-live", true)
            .property("do-timestamp", true)
            .property("freq", 660.0f64)
            .property("volume", 0.3f64)
            .build()
            .expect("second test audio source should be available");
        let webcam = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .property_from_str("pattern", "blue")
            .build()
            .expect("test webcam should be available");
        let pipeline = recording_pipeline_with_sources(
            video.clone(),
            Some(webcam),
            vec![audio.clone(), second_audio.clone()],
            &path,
        )
        .expect("recording pipeline should be constructible");
        let (commands, receiver) = async_channel::bounded(8);
        let (states, observed) = std::sync::mpsc::channel();
        let recorder = std::thread::spawn(move || {
            run_recording(&pipeline, receiver, |state| {
                states
                    .send(state)
                    .expect("test should receive recording states")
            })
        });
        let next_state = || {
            observed
                .recv_timeout(Duration::from_secs(10))
                .expect("recording should report its state")
        };
        assert_eq!(next_state(), RecordingState::Recording);
        std::thread::sleep(Duration::from_millis(900));
        commands
            .send_blocking(RecordingCommand::TogglePause)
            .expect("pause should be accepted");
        assert_eq!(next_state(), RecordingState::Paused);
        video.set_property_from_str("pattern", "blue");
        audio.set_property("freq", 1000.0f64);
        second_audio.set_property("freq", 1200.0f64);
        std::thread::sleep(Duration::from_millis(2500));
        video.set_property_from_str("pattern", "green");
        audio.set_property("freq", 880.0f64);
        second_audio.set_property("freq", 1100.0f64);
        commands
            .send_blocking(RecordingCommand::TogglePause)
            .expect("resume should be accepted");
        assert_eq!(next_state(), RecordingState::Recording);
        std::thread::sleep(Duration::from_millis(900));
        commands
            .send_blocking(RecordingCommand::Stop)
            .expect("stop should be accepted");
        assert_eq!(next_state(), RecordingState::Stopping);
        recorder
            .join()
            .expect("recording thread should finish")
            .expect("paused recording should finalize");

        let uri = gst::glib::filename_to_uri(&path, None).expect("recording should have a URI");
        let discoverer = gstreamer_pbutils::Discoverer::new(gst::ClockTime::from_seconds(10))
            .expect("media discoverer should be available");
        let info = discoverer
            .discover_uri(uri.as_str())
            .expect("recording should be discoverable");
        let duration = info
            .duration()
            .expect("recording should report its duration")
            .mseconds();
        assert!(
            (1400..=2500).contains(&duration),
            "recording duration should exclude the 2.5-second pause, got {duration}ms"
        );
        for (time, expected) in [(0.2, "red"), (1.2, "green")] {
            let frame = crate::linux::video::frame_at(&path, time)
                .expect("recording should contain frames before and after the pause");
            let pixel = frame
                .flatten()
                .pixel(10, 10)
                .expect("test frame should contain the selected pixel")
                .demultiply();
            match expected {
                "red" => assert!(
                    pixel.red() > 200 && pixel.green() < 60 && pixel.blue() < 60,
                    "frame before pause should be red, got {pixel:?}"
                ),
                _ => assert!(
                    pixel.green() > 180 && pixel.red() < 60 && pixel.blue() < 60,
                    "frame after pause should be green, got {pixel:?}"
                ),
            }
            let (width, height) = frame.size();
            let geometry = webcam_geometry(width as i32, height as i32, WebcamLayout::default())
                .expect("test frame should have valid webcam geometry");
            let overlay = frame
                .flatten()
                .pixel(
                    (geometry.x + geometry.width / 2) as u32,
                    (geometry.y + geometry.height / 2) as u32,
                )
                .expect("webcam overlay should be inside the test frame")
                .demultiply();
            assert!(
                overlay.blue() > 200 && overlay.red() < 60,
                "webcam overlay should survive the pause, got {overlay:?}"
            );
        }
        let samples = decode_recording_audio(&path);
        assert_audio_frequency(&samples, 440.0);
        assert_audio_frequency(&samples, 660.0);
        let resumed_audio = samples
            .get(57_600..)
            .expect("recording should contain audio after the pause");
        assert_audio_frequency(resumed_audio, 880.0);
        assert_audio_frequency(resumed_audio, 1100.0);
        assert!(
            audio_frequency_amplitude(resumed_audio, 1000.0) < 0.03
                && audio_frequency_amplitude(resumed_audio, 1200.0) < 0.03,
            "audio produced during the pause must not be recorded"
        );
    }

    #[test]
    fn stopping_while_paused_finalizes_the_recording() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join("stopped-while-paused.mp4");
        let source = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .build()
            .expect("test video source should be available");
        let audio = gst::ElementFactory::make("audiotestsrc")
            .property("is-live", true)
            .property("freq", 440.0f64)
            .build()
            .expect("test audio source should be available");
        let pipeline = recording_pipeline_with_audio(source, vec![audio], &path)
            .expect("recording pipeline should be constructible");
        let (commands, receiver) = async_channel::bounded(8);
        let (states, observed) = std::sync::mpsc::channel();
        let recorder = std::thread::spawn(move || {
            run_recording(&pipeline, receiver, |state| {
                states
                    .send(state)
                    .expect("test should receive recording states")
            })
        });
        let next_state = || {
            observed
                .recv_timeout(Duration::from_secs(10))
                .expect("recording should report its state")
        };
        assert_eq!(next_state(), RecordingState::Recording);
        std::thread::sleep(Duration::from_millis(700));
        commands
            .send_blocking(RecordingCommand::TogglePause)
            .expect("pause should be accepted");
        assert_eq!(next_state(), RecordingState::Paused);
        std::thread::sleep(Duration::from_millis(1000));
        commands
            .send_blocking(RecordingCommand::Stop)
            .expect("stop should be accepted while paused");
        assert_eq!(next_state(), RecordingState::Stopping);
        recorder
            .join()
            .expect("recording thread should finish")
            .expect("recording stopped while paused should finalize");
        crate::linux::video::first_frame(&path)
            .expect("recording stopped while paused should contain decodable video");
        assert_audio_frequency(&decode_recording_audio(&path), 440.0);
        let uri = gst::glib::filename_to_uri(&path, None).expect("recording should have a URI");
        let discoverer = gstreamer_pbutils::Discoverer::new(gst::ClockTime::from_seconds(10))
            .expect("media discoverer should be available");
        let duration = discoverer
            .discover_uri(uri.as_str())
            .expect("recording should be discoverable")
            .duration()
            .expect("recording should report its duration")
            .mseconds();
        assert!(
            (400..=1400).contains(&duration),
            "stopping while paused should exclude the pause, got {duration}ms"
        );
    }

    #[test]
    fn stopping_before_a_video_frame_reports_an_error() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join("recording-without-video.mp4");
        let source = gstreamer_app::AppSrc::builder()
            .caps(
                &gst::Caps::builder("video/x-raw")
                    .field("format", "RGBA")
                    .field("width", 320i32)
                    .field("height", 240i32)
                    .field("framerate", gst::Fraction::new(30, 1))
                    .build(),
            )
            .format(gst::Format::Time)
            .is_live(true)
            .build();
        let pipeline = recording_pipeline(source.upcast(), &path)
            .expect("recording pipeline should be constructible");
        let (commands, receiver) = async_channel::bounded(8);
        let (states, observed) = std::sync::mpsc::channel();
        let recorder = std::thread::spawn(move || {
            run_recording(&pipeline, receiver, |state| {
                states
                    .send(state)
                    .expect("test should receive recording states")
            })
        });
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(10))
                .expect("recording should report its state"),
            RecordingState::Recording
        );
        commands
            .send_blocking(RecordingCommand::Stop)
            .expect("stop should be accepted");
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(10))
                .expect("recording should report it is stopping"),
            RecordingState::Stopping
        );
        let error = recorder
            .join()
            .expect("recording thread should finish")
            .expect_err("recording without a video frame must fail");
        assert!(
            error.contains("before receiving a video frame"),
            "failure should explain why no recording was saved: {error}"
        );
    }

    #[test]
    fn recording_pipeline_finalizes_an_mp4_from_a_test_source() {
        gst::init().expect("GStreamer should initialize");
        let missing_plugins = ["x264enc", "h264parse", "mp4mux"]
            .into_iter()
            .filter(|plugin| gst::ElementFactory::find(plugin).is_none())
            .collect::<Vec<_>>();
        if !missing_plugins.is_empty() {
            assert!(
                std::env::var_os("CI").is_none(),
                "CI must install the GStreamer MP4 plugins to exercise MP4 finalization; missing: {}",
                missing_plugins.join(", ")
            );
            eprintln!(
                "Skipping MP4 pipeline test because GStreamer plugins are unavailable: {}",
                missing_plugins.join(", ")
            );
            return;
        }
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join("recording.mp4");
        let source = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .property("num-buffers", 30i32)
            .build()
            .expect("GStreamer test video source should be available");
        let pipeline = recording_pipeline(source, &path)
            .expect("MP4 recording pipeline should be constructible");
        pipeline
            .set_state(gst::State::Playing)
            .expect("test recording should start");
        std::thread::sleep(Duration::from_millis(250));
        finish_recording(&pipeline).expect("recording should finalize with EOS");
        assert!(path.is_file(), "finalized MP4 should exist");
        let bytes = std::fs::read(&path).expect("finalized MP4 should be readable");
        assert!(
            bytes.len() > 1024,
            "finalized MP4 should contain encoded video"
        );
        for required_box in [b"ftyp", b"moov", b"mdat"] {
            assert!(
                bytes
                    .windows(required_box.len())
                    .any(|window| window == required_box),
                "finalized MP4 should contain the {required_box:?} box"
            );
        }
        let reopened =
            crate::linux::video::first_frame(&path).expect("finalized MP4 should be decodable");
        assert!(
            reopened.size().0 > 0 && reopened.size().1 > 0,
            "finalized MP4 should contain a decodable frame"
        );
    }

    #[test]
    fn recording_pipeline_muxes_microphone_audio_into_mp4() {
        gst::init().expect("GStreamer should initialize");
        let missing_plugins = ["x264enc", "h264parse", "mp4mux", "voaacenc", "aacparse"]
            .into_iter()
            .filter(|plugin| gst::ElementFactory::find(plugin).is_none())
            .collect::<Vec<_>>();
        if !missing_plugins.is_empty() {
            assert!(
                std::env::var_os("CI").is_none(),
                "CI must install GStreamer audio/video plugins to test microphone muxing; missing: {}",
                missing_plugins.join(", ")
            );
            eprintln!(
                "Skipping microphone recording test because GStreamer plugins are unavailable: {}",
                missing_plugins.join(", ")
            );
            return;
        }

        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join("recording-with-microphone.mp4");
        let video_source = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .property("num-buffers", 30i32)
            .build()
            .expect("GStreamer test video source should be available");
        let audio_source = gst::ElementFactory::make("audiotestsrc")
            .property("is-live", true)
            .property("num-buffers", 60i32)
            .build()
            .expect("GStreamer test audio source should be available");
        let pipeline = recording_pipeline_with_audio(video_source, vec![audio_source], &path)
            .expect("audio MP4 recording pipeline should be constructible");
        pipeline
            .set_state(gst::State::Playing)
            .expect("test audio recording should start");
        std::thread::sleep(Duration::from_millis(500));
        finish_recording(&pipeline).expect("audio recording should finalize with EOS");

        let bytes = std::fs::read(&path).expect("finalized MP4 should be readable");
        assert!(
            bytes.windows(4).any(|window| window == b"soun"),
            "MP4 should contain a microphone audio track"
        );
        crate::linux::video::first_frame(&path)
            .expect("audio recording MP4 should retain decodable video");
        assert_audio_frequency(&decode_recording_audio(&path), 440.0);
    }

    #[test]
    fn recording_pipeline_mixes_system_and_microphone_sources() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join("recording-with-mixed-audio.mp4");
        let video = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .build()
            .expect("test video source should be available");
        let audio_sources = [440.0f64, 880.0]
            .into_iter()
            .map(|frequency| {
                gst::ElementFactory::make("audiotestsrc")
                    .property("is-live", true)
                    .property("freq", frequency)
                    .property("volume", 0.25f64)
                    .build()
                    .expect("test audio source should be available")
            })
            .collect();
        let pipeline = recording_pipeline_with_audio(video, audio_sources, &path)
            .expect("mixed audio recording pipeline should be constructible");
        pipeline
            .set_state(gst::State::Playing)
            .expect("mixed recording should start");
        std::thread::sleep(Duration::from_millis(900));
        finish_recording(&pipeline).expect("mixed recording should finalize");

        let audio = decode_recording_audio(&path);
        assert_audio_frequency(&audio, 440.0);
        assert_audio_frequency(&audio, 880.0);
        crate::linux::video::first_frame(&path)
            .expect("mixed audio recording should retain decodable video");
    }

    #[test]
    fn webcam_source_uses_the_configured_device() {
        gst::init().expect("GStreamer should initialize");
        let configuration =
            WebcamConfiguration::new("/dev/v4l/by-id/chosen-camera", WebcamLayout::default())
                .expect("an absolute webcam device path should be valid");
        let source = webcam_source(&configuration).expect("V4L2 source should be available");
        assert_eq!(
            source.property::<String>("device"),
            "/dev/v4l/by-id/chosen-camera",
            "recording must use the configured device rather than the default"
        );
        assert!(
            source.property::<bool>("do-timestamp"),
            "webcam frames should use capture timestamps"
        );
    }

    #[test]
    fn webcam_geometry_stays_inside_the_screen() {
        assert_eq!(
            webcam_geometry(1920, 1080, WebcamLayout::default()),
            Some(WebcamGeometry {
                x: 1424,
                y: 794,
                width: 480,
                height: 270,
            })
        );
        assert_eq!(
            webcam_geometry(1, 1, WebcamLayout::default()),
            Some(WebcamGeometry {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            })
        );
        assert_eq!(webcam_geometry(0, 1080, WebcamLayout::default()), None);
        assert_eq!(webcam_geometry(1920, -1, WebcamLayout::default()), None);
    }

    #[test]
    fn webcam_layout_defaults_to_rectangle_and_preserves_shape_configuration() {
        assert_eq!(WebcamLayout::default().shape(), WebcamShape::Rectangle);
        let layout = WebcamLayout::new(WebcamCorner::TopRight, 40).expect("40 percent is valid");
        assert_eq!(layout.shape(), WebcamShape::Rectangle);

        let circle = layout.with_shape(WebcamShape::Circle);
        assert_eq!(circle.shape(), WebcamShape::Circle);
        assert_eq!(circle.corner(), WebcamCorner::TopRight);
        assert_eq!(circle.size_percent(), 40);
        assert_eq!(
            circle.with_shape(WebcamShape::Rectangle),
            layout,
            "a circular layout can be switched back to a rectangle"
        );
        assert_eq!(
            WebcamShape::from_name("rectangle"),
            Ok(WebcamShape::Rectangle)
        );
        assert_eq!(WebcamShape::from_name("circle"), Ok(WebcamShape::Circle));
        assert_eq!(
            WebcamShape::from_name("Circle"),
            Err("unknown webcam shape: Circle".to_owned())
        );
    }

    #[test]
    fn circular_webcam_geometry_places_every_corner_at_the_requested_size() {
        let cases = [
            (
                10,
                48,
                [
                    (WebcamCorner::TopLeft, 12, 12),
                    (WebcamCorner::TopRight, 580, 12),
                    (WebcamCorner::BottomLeft, 12, 420),
                    (WebcamCorner::BottomRight, 580, 420),
                ],
            ),
            (
                40,
                192,
                [
                    (WebcamCorner::TopLeft, 12, 12),
                    (WebcamCorner::TopRight, 436, 12),
                    (WebcamCorner::BottomLeft, 12, 276),
                    (WebcamCorner::BottomRight, 436, 276),
                ],
            ),
            (
                50,
                240,
                [
                    (WebcamCorner::TopLeft, 12, 12),
                    (WebcamCorner::TopRight, 388, 12),
                    (WebcamCorner::BottomLeft, 12, 228),
                    (WebcamCorner::BottomRight, 388, 228),
                ],
            ),
        ];
        for (size, diameter, corners) in cases {
            for (corner, x, y) in corners {
                let layout = WebcamLayout::new(corner, size)
                    .expect("webcam size should be valid")
                    .with_shape(WebcamShape::Circle);
                assert_eq!(
                    webcam_geometry(640, 480, layout),
                    Some(WebcamGeometry {
                        x,
                        y,
                        width: diameter,
                        height: diameter,
                    }),
                    "incorrect circle geometry for {corner:?} at {size} percent"
                );
            }
        }

        let layout = WebcamLayout::new(WebcamCorner::BottomRight, 10)
            .expect("10 percent is valid")
            .with_shape(WebcamShape::Circle);
        assert_eq!(
            webcam_geometry(101, 79, layout),
            Some(WebcamGeometry {
                x: 93,
                y: 71,
                width: 7,
                height: 7,
            }),
            "fractional heights are rounded down before positioning"
        );
        assert_eq!(
            webcam_geometry(1, 1, layout),
            Some(WebcamGeometry {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            })
        );
        for (width, height) in [(0, 480), (-1, 480), (640, 0), (640, -1)] {
            assert_eq!(webcam_geometry(width, height, layout), None);
        }
    }

    #[test]
    fn circular_webcam_geometry_caps_diameter_to_narrow_frame_width() {
        for (corner, y) in [
            (WebcamCorner::TopLeft, 2),
            (WebcamCorner::TopRight, 2),
            (WebcamCorner::BottomLeft, 1118),
            (WebcamCorner::BottomRight, 1118),
        ] {
            let layout = WebcamLayout::new(corner, 25)
                .expect("25 percent is valid")
                .with_shape(WebcamShape::Circle);
            assert_eq!(
                webcam_geometry(80, 1200, layout),
                Some(WebcamGeometry {
                    x: 0,
                    y,
                    width: 80,
                    height: 80,
                }),
                "the circle must fit the width at {corner:?}"
            );
        }

        let layout = WebcamLayout::new(WebcamCorner::TopLeft, 25)
            .expect("25 percent is valid")
            .with_shape(WebcamShape::Circle);
        for (frame_width, x, diameter) in [(299, 0, 299), (300, 0, 300), (301, 1, 300)] {
            assert_eq!(
                webcam_geometry(frame_width, 1200, layout),
                Some(WebcamGeometry {
                    x,
                    y: 7,
                    width: diameter,
                    height: diameter,
                }),
                "circle diameter must be capped exactly at frame width {frame_width}"
            );
        }
    }

    #[test]
    fn circular_alpha_preserves_color_and_source_alpha_with_padded_rows() {
        let (width, height, stride) = (7usize, 7usize, 31usize);
        let mut plane = vec![0xe7; stride * height];
        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                plane[offset..offset + 4].copy_from_slice(&[11 + x as u8, 41 + y as u8, 91, 128]);
            }
        }
        plane[3 * stride + 3 * 4 + 3] = 73;
        plane[3 * stride + 2 * 4 + 3] = 0;
        let original = plane.clone();

        apply_circular_alpha(&mut plane, stride, width as u32, height as u32)
            .expect("a padded RGBA frame should be accepted");

        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                assert_eq!(
                    &plane[offset..offset + 3],
                    &original[offset..offset + 3],
                    "RGB must be unchanged at ({x}, {y})"
                );
            }
            assert_eq!(
                &plane[y * stride + width * 4..(y + 1) * stride],
                &original[y * stride + width * 4..(y + 1) * stride],
                "row {y} padding must not be modified"
            );
        }
        let alpha = |x: usize, y: usize| plane[y * stride + x * 4 + 3];
        assert_eq!(alpha(3, 3), 73, "the center keeps its source alpha");
        assert_eq!(alpha(2, 3), 0, "transparent source pixels stay transparent");
        assert_eq!(alpha(0, 3), 128, "the interior keeps its source alpha");
        assert_eq!(alpha(0, 2), 107, "the inner edge scales source alpha");
        assert_eq!(alpha(0, 1), 50, "the outer edge fades smoothly");
        for (x, y) in [(0, 0), (6, 0), (0, 6), (6, 6)] {
            assert_eq!(alpha(x, y), 0, "corner ({x}, {y}) must be transparent");
        }

        let mut without_last_padding = vec![255; (height - 1) * stride + width * 4];
        apply_circular_alpha(
            &mut without_last_padding,
            stride,
            width as u32,
            height as u32,
        )
        .expect("the last row does not require trailing padding");
        assert_eq!(without_last_padding[3], 0);
        assert_eq!(without_last_padding[3 * stride + 3 * 4 + 3], 255);
    }

    #[test]
    fn circular_alpha_handles_even_and_non_square_frames() {
        for (width, height, center, edge, expected_edge) in [
            (8usize, 8usize, (4, 4), (0, 3), 123),
            (9, 7, (4, 3), (1, 1), 50),
        ] {
            let stride = width * 4;
            let mut plane = [17, 34, 51, 128].repeat(width * height);
            apply_circular_alpha(&mut plane, stride, width as u32, height as u32)
                .expect("valid RGBA frame should be accepted");
            let pixel = |(x, y): (usize, usize)| {
                let offset = y * stride + x * 4;
                &plane[offset..offset + 4]
            };
            assert_eq!(
                pixel(center),
                &[17, 34, 51, 128],
                "the center of a {width}x{height} frame keeps its color and alpha"
            );
            assert_eq!(
                pixel(edge),
                &[17, 34, 51, expected_edge],
                "the edge of a {width}x{height} frame keeps RGB and fades alpha"
            );
            for corner in [
                (0, 0),
                (width - 1, 0),
                (0, height - 1),
                (width - 1, height - 1),
            ] {
                assert_eq!(
                    pixel(corner),
                    &[17, 34, 51, 0],
                    "the {width}x{height} corner {corner:?} should be transparent"
                );
            }
        }
    }

    #[test]
    fn circular_alpha_rejects_invalid_buffers_without_modifying_them() {
        let invalid = [
            (0, 0, 1, 0, "the webcam frame has invalid dimensions"),
            (0, 1, 0, 0, "the webcam frame has invalid dimensions"),
            (
                27,
                7,
                7,
                217,
                "the webcam frame has an incomplete pixel buffer",
            ),
            (
                31,
                7,
                7,
                213,
                "the webcam frame has an incomplete pixel buffer",
            ),
            (usize::MAX, 1, 2, 4, "the webcam frame is too large"),
        ];
        for (stride, width, height, length, expected) in invalid {
            let mut plane = vec![91; length];
            let original = plane.clone();
            assert_eq!(
                apply_circular_alpha(&mut plane, stride, width, height),
                Err(expected.to_owned()),
                "stride={stride}, width={width}, height={height}, length={length}"
            );
            assert_eq!(
                plane, original,
                "invalid input must not be partially changed"
            );
        }
    }

    #[test]
    fn webcam_layout_places_each_corner_at_the_requested_size() {
        let expected = [
            (
                10,
                64,
                48,
                [
                    (WebcamCorner::TopLeft, 12, 12),
                    (WebcamCorner::TopRight, 564, 12),
                    (WebcamCorner::BottomLeft, 12, 420),
                    (WebcamCorner::BottomRight, 564, 420),
                ],
            ),
            (
                40,
                256,
                192,
                [
                    (WebcamCorner::TopLeft, 12, 12),
                    (WebcamCorner::TopRight, 372, 12),
                    (WebcamCorner::BottomLeft, 12, 276),
                    (WebcamCorner::BottomRight, 372, 276),
                ],
            ),
            (
                50,
                320,
                240,
                [
                    (WebcamCorner::TopLeft, 12, 12),
                    (WebcamCorner::TopRight, 308, 12),
                    (WebcamCorner::BottomLeft, 12, 228),
                    (WebcamCorner::BottomRight, 308, 228),
                ],
            ),
        ];
        for (size, width, height, corners) in expected {
            for (corner, x, y) in corners {
                let layout = WebcamLayout::new(corner, size).expect("webcam size should be valid");
                assert_eq!(
                    webcam_geometry(640, 480, layout),
                    Some(WebcamGeometry {
                        x,
                        y,
                        width,
                        height,
                    }),
                    "incorrect geometry for {corner:?} at {size} percent"
                );
            }
        }

        let small_layout =
            WebcamLayout::new(WebcamCorner::BottomRight, 10).expect("10 percent is valid");
        assert_eq!(
            webcam_geometry(101, 79, small_layout),
            Some(WebcamGeometry {
                x: 90,
                y: 71,
                width: 10,
                height: 7,
            })
        );
        assert_eq!(
            webcam_geometry(1, 1, small_layout),
            Some(WebcamGeometry {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            })
        );
        for size in [9, 51] {
            assert_eq!(
                WebcamLayout::new(WebcamCorner::TopLeft, size),
                Err("webcam size must be between 10 and 50 percent".to_owned()),
                "size {size} must be rejected"
            );
        }
    }

    #[test]
    fn webcam_device_validation_rejects_invalid_paths_and_preserves_valid_paths() {
        let layout = WebcamLayout::new(WebcamCorner::TopRight, 10).expect("10 percent is valid");
        let configuration = WebcamConfiguration::new("  /dev/v4l/by-id/front camera  ", layout)
            .expect("an absolute device path should be valid");
        assert_eq!(configuration.device(), "/dev/v4l/by-id/front camera");
        assert_eq!(configuration.layout(), layout);

        let longest = format!("/{}", "x".repeat(4095));
        let configuration = WebcamConfiguration::new(&longest, layout)
            .expect("a 4096-byte absolute device path should be valid");
        assert_eq!(configuration.device(), longest);

        let too_long = format!("{longest}x");
        for device in [
            "",
            "  ",
            "video0",
            "/dev/vid\0eo0",
            "/dev/vid\teo0",
            &too_long,
        ] {
            assert_eq!(
                WebcamConfiguration::new(device, layout),
                Err("webcam device must be an absolute path without control characters".to_owned()),
                "invalid device path {device:?} must be rejected"
            );
        }
    }

    #[test]
    fn webcam_device_rejects_control_characters_at_either_end() {
        let devices = [
            "\t/dev/video1",
            "\n/dev/video1",
            "/dev/video1\t",
            "/dev/video1\n",
            "/dev/video1\r",
            " \t/dev/video1 ",
        ];
        let results: Vec<_> = devices
            .iter()
            .map(|device| WebcamConfiguration::new(device, WebcamLayout::default()))
            .collect();
        let expected =
            vec![
                Err("webcam device must be an absolute path without control characters".to_owned());
                devices.len()
            ];
        assert_eq!(
            results, expected,
            "control characters at either end of a device path must be rejected"
        );
    }

    #[test]
    fn recording_pipeline_uses_configured_webcam_corner_and_size() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let source = |pattern: &str| {
            gst::parse::bin_from_description(
                &format!(
                    "videotestsrc num-buffers=24 is-live=true pattern={pattern} \
                     ! video/x-raw,width=320,height=240,framerate=30/1 ! identity"
                ),
                true,
            )
            .expect("test video source should be available")
            .upcast::<gst::Element>()
        };
        let cases = [
            (WebcamCorner::TopLeft, 40, (126, 94), (142, 54), (70, 110)),
            (WebcamCorner::TopRight, 40, (194, 94), (178, 54), (250, 110)),
            (
                WebcamCorner::BottomLeft,
                40,
                (126, 146),
                (142, 186),
                (70, 130),
            ),
            (
                WebcamCorner::BottomRight,
                40,
                (194, 146),
                (178, 186),
                (250, 130),
            ),
            (WebcamCorner::TopLeft, 10, (22, 18), (46, 18), (22, 38)),
            (
                WebcamCorner::BottomRight,
                50,
                (162, 122),
                (146, 174),
                (234, 106),
            ),
        ];
        for (corner, size, inside, outside_x, outside_y) in cases {
            let path = directory
                .path()
                .join(format!("{}-{size}.mp4", corner.name()));
            let layout = WebcamLayout::new(corner, size).expect("webcam size should be valid");
            let pipeline = recording_pipeline_with_layout(
                source("red"),
                Some(source("blue")),
                layout,
                Vec::new(),
                RecordingFrameRate::Source,
                &path,
            )
            .expect("webcam recording pipeline should be constructible");
            pipeline
                .set_state(gst::State::Playing)
                .expect("webcam recording should start");
            let message = pipeline
                .bus()
                .expect("webcam recording should have a bus")
                .timed_pop_filtered(
                    gst::ClockTime::from_seconds(15),
                    &[gst::MessageType::Eos, gst::MessageType::Error],
                )
                .expect("finite webcam recording should finish");
            match message.view() {
                gst::MessageView::Eos(_) => {}
                gst::MessageView::Error(error) => {
                    panic!("webcam recording failed: {}", error.error())
                }
                _ => unreachable!("only EOS and error messages were requested"),
            }
            pipeline
                .set_state(gst::State::Null)
                .expect("webcam recording should stop");

            let frame = crate::linux::video::frame_at(&path, 0.4)
                .expect("webcam recording should contain a decodable frame");
            assert_eq!(frame.size(), (320, 240));
            let pixmap = frame.flatten();
            let pixel = |(x, y)| {
                pixmap
                    .pixel(x, y)
                    .expect("test pixel should be inside the recording")
                    .demultiply()
            };
            let overlay = pixel(inside);
            assert!(
                overlay.blue() > 200 && overlay.red() < 60,
                "webcam should cover {inside:?} at {corner:?}, {size}%: {overlay:?}"
            );
            for outside in [outside_x, outside_y] {
                let screen = pixel(outside);
                assert!(
                    screen.red() > 200 && screen.blue() < 60,
                    "screen should remain visible at {outside:?} for {corner:?}, {size}%: {screen:?}"
                );
            }
            if size == 40 {
                let center = pixel((160, 120));
                assert!(
                    center.red() > 200 && center.blue() < 60,
                    "screen center should remain uncovered for {corner:?}: {center:?}"
                );
            }
        }
    }

    #[test]
    fn recording_pipeline_crops_and_masks_a_circular_webcam() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join("circular-webcam.mp4");
        let screen = gst::parse::bin_from_description(
            "videotestsrc num-buffers=24 is-live=true pattern=red \
             ! video/x-raw,width=320,height=240,framerate=30/1 ! identity",
            true,
        )
        .expect("red screen source should be available")
        .upcast::<gst::Element>();
        let webcam = gst::parse::bin_from_description(
            "videotestsrc num-buffers=24 is-live=true pattern=smpte \
             ! video/x-raw,width=320,height=160,framerate=30/1,pixel-aspect-ratio=1/1 ! identity",
            true,
        )
        .expect("non-square patterned webcam source should be available")
        .upcast::<gst::Element>();
        let layout = WebcamLayout::new(WebcamCorner::TopLeft, 40)
            .expect("40 percent is valid")
            .with_shape(WebcamShape::Circle);
        let pipeline = recording_pipeline_with_layout(
            screen,
            Some(webcam),
            layout,
            Vec::new(),
            RecordingFrameRate::Source,
            &path,
        )
        .expect("circular webcam recording pipeline should be constructible");
        let crop = pipeline
            .iterate_elements()
            .into_iter()
            .map(|element| element.expect("pipeline elements should be iterable"))
            .find(|element| {
                element
                    .factory()
                    .is_some_and(|factory| factory.name() == "aspectratiocrop")
            })
            .expect("circular webcam pipeline should use aspectratiocrop");
        pipeline
            .set_state(gst::State::Playing)
            .expect("circular webcam recording should start");
        let message = pipeline
            .bus()
            .expect("recording should have a bus")
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(15),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("finite circular webcam recording should finish");
        match message.view() {
            gst::MessageView::Eos(_) => {}
            gst::MessageView::Error(error) => {
                panic!("circular webcam recording failed: {}", error.error())
            }
            _ => unreachable!("only EOS and error messages were requested"),
        }
        let caps = crop
            .static_pad("src")
            .expect("camera crop should have an output pad")
            .current_caps()
            .expect("camera crop should negotiate output caps");
        let info = gstreamer_video::VideoInfo::from_caps(&caps)
            .expect("camera crop should negotiate valid video");
        assert_eq!(
            info.format(),
            gstreamer_video::VideoFormat::Rgba,
            "the circular mask must receive RGBA pixels"
        );
        assert_eq!(
            info.width(),
            info.height(),
            "the camera must be square-cropped before masking"
        );
        pipeline
            .set_state(gst::State::Null)
            .expect("circular webcam recording should stop");

        let frame = crate::linux::video::frame_at(&path, 0.4)
            .expect("circular webcam recording should contain a decodable frame");
        assert_eq!(frame.size(), (320, 240));
        let pixmap = frame.flatten();
        let pixel = |x, y| {
            pixmap
                .pixel(x, y)
                .expect("test pixel should be inside the recording")
                .demultiply()
        };

        // A 96-pixel circle begins at (6, 6). Its center is in the green
        // SMPTE bar, while the central crop's quarter points are cyan/magenta.
        let center = pixel(54, 54);
        assert!(
            center.green() > 150 && center.red() < 90 && center.blue() < 90,
            "circle center should show the green camera bar: {center:?}"
        );
        let left = pixel(30, 54);
        assert!(
            left.green() > 150 && left.blue() > 150 && left.red() < 90,
            "center-cropped camera should show cyan, not stretched yellow: {left:?}"
        );
        let right = pixel(78, 54);
        assert!(
            right.red() > 150 && right.blue() > 150 && right.green() < 90,
            "center-cropped camera should show magenta, not stretched red: {right:?}"
        );
        for (x, y) in [
            (6, 6),
            (101, 6),
            (6, 101),
            (101, 101),
            (9, 9),
            (98, 9),
            (9, 98),
            (98, 98),
            (120, 54),
            (54, 120),
        ] {
            let background = pixel(x, y);
            assert!(
                background.red() > 200 && background.green() < 60 && background.blue() < 60,
                "transparent square corner or outside ({x}, {y}) should show the red screen: {background:?}"
            );
        }
    }

    #[test]
    fn recording_pipeline_composites_webcam_and_mixes_audio() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join("recording-with-webcam.mp4");
        let video_source = |pattern: &str, width, height| {
            gst::parse::bin_from_description(
                &format!(
                    "videotestsrc is-live=true pattern={pattern} \
                     ! video/x-raw,width={width},height={height},framerate=30/1 ! identity"
                ),
                true,
            )
            .expect("test video source should be available")
            .upcast::<gst::Element>()
        };
        let screen = video_source("red", 640, 480);
        let webcam = video_source("blue", 160, 120);
        let audio_sources = [440.0f64, 880.0]
            .into_iter()
            .map(|frequency| {
                gst::ElementFactory::make("audiotestsrc")
                    .property("is-live", true)
                    .property("freq", frequency)
                    .property("volume", 0.25f64)
                    .build()
                    .expect("test audio source should be available")
            })
            .collect();
        let pipeline = recording_pipeline_with_sources(screen, Some(webcam), audio_sources, &path)
            .expect("webcam recording pipeline should be constructible");
        pipeline
            .set_state(gst::State::Playing)
            .expect("webcam recording should start");
        std::thread::sleep(Duration::from_millis(1200));
        finish_recording(&pipeline).expect("webcam recording should finalize");

        let frame = crate::linux::video::frame_at(&path, 0.5)
            .expect("webcam recording should contain a decodable frame");
        assert_eq!(
            frame.size(),
            (640, 480),
            "webcam must not enlarge the recording"
        );
        let pixmap = frame.flatten();
        let screen_pixel = pixmap
            .pixel(320, 240)
            .expect("screen pixel should be within the frame")
            .demultiply();
        assert!(
            screen_pixel.red() > 200 && screen_pixel.blue() < 60,
            "screen should remain red outside the webcam overlay: {screen_pixel:?}"
        );
        let webcam_pixel = pixmap
            .pixel(548, 408)
            .expect("webcam pixel should be within the frame")
            .demultiply();
        assert!(
            webcam_pixel.blue() > 200 && webcam_pixel.red() < 60,
            "webcam should appear in the bottom-right overlay: {webcam_pixel:?}"
        );
        let audio = decode_recording_audio(&path);
        assert_audio_frequency(&audio, 440.0);
        assert_audio_frequency(&audio, 880.0);
    }

    #[test]
    fn webcam_recording_waits_for_the_first_screen_frame() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join("recording-with-delayed-screen.mp4");
        let screen = gstreamer_app::AppSrc::builder()
            .caps(
                &gst::Caps::builder("video/x-raw")
                    .field("format", "RGBA")
                    .field("width", 640i32)
                    .field("height", 480i32)
                    .field("framerate", gst::Fraction::new(0, 1))
                    .build(),
            )
            .format(gst::Format::Time)
            .is_live(true)
            .build();
        let webcam = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .property_from_str("pattern", "blue")
            .build()
            .expect("test webcam should be available");
        let pipeline = recording_pipeline_with_sources(
            screen.clone().upcast(),
            Some(webcam),
            Vec::new(),
            &path,
        )
        .expect("webcam recording pipeline should be constructible");
        let compositor = pipeline
            .by_name("recording-webcam-compositor")
            .expect("webcam compositor should exist");
        let screen_pad = compositor
            .static_pad("sink_0")
            .expect("webcam compositor should have a screen input");
        screen_pad
            .add_probe(gst::PadProbeType::BUFFER, |_, _| {
                std::thread::sleep(Duration::from_millis(250));
                gst::PadProbeReturn::Remove
            })
            .expect("screen delivery should be delayable");
        pipeline
            .set_state(gst::State::Playing)
            .expect("webcam recording should start");
        std::thread::sleep(Duration::from_millis(500));
        for timestamp in [500, 600] {
            let mut buffer = gst::Buffer::from_mut_slice([255u8, 0, 0, 255].repeat(640 * 480));
            buffer
                .get_mut()
                .expect("screen buffer should be writable")
                .set_pts(gst::ClockTime::from_mseconds(timestamp));
            screen
                .push_buffer(buffer)
                .expect("screen source should accept a frame");
            std::thread::sleep(Duration::from_millis(100));
        }
        std::thread::sleep(Duration::from_millis(700));
        finish_recording(&pipeline).expect("webcam recording should finalize");

        let frame = crate::linux::video::first_frame(&path)
            .expect("recording should contain a decodable first frame");
        let pixel = frame
            .flatten()
            .pixel(320, 240)
            .expect("screen pixel should be within the frame")
            .demultiply();
        assert!(
            pixel.red() > 200 && pixel.blue() < 60,
            "the first recording frame must contain the screen, not the compositor background: {pixel:?}"
        );
    }

    #[test]
    fn webcam_recording_retains_the_last_screen_frame_after_screen_eos() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join("recording-with-early-screen-eos.mp4");
        let screen = gst::parse::bin_from_description(
            "videotestsrc is-live=true num-buffers=3 pattern=red \
             ! video/x-raw,width=640,height=480,framerate=30/1 ! identity",
            true,
        )
        .expect("test screen source should be available")
        .upcast();
        let webcam = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .property_from_str("pattern", "blue")
            .build()
            .expect("test webcam should be available");
        let pipeline = recording_pipeline_with_sources(screen, Some(webcam), Vec::new(), &path)
            .expect("webcam recording pipeline should be constructible");
        pipeline
            .set_state(gst::State::Playing)
            .expect("webcam recording should start");
        std::thread::sleep(Duration::from_millis(1100));
        finish_recording(&pipeline).expect("webcam recording should finalize");

        let frame = crate::linux::video::frame_at(&path, 0.8)
            .expect("recording should contain a late frame");
        let pixel = frame
            .flatten()
            .pixel(320, 240)
            .expect("screen pixel should be within the frame")
            .demultiply();
        assert!(
            pixel.red() > 200 && pixel.blue() < 60,
            "the last screen frame must remain visible while the webcam finishes: {pixel:?}"
        );
    }

    #[test]
    fn stopping_a_webcam_recording_without_screen_frames_reports_an_error() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join("recording-without-screen.mp4");
        let screen = gstreamer_app::AppSrc::builder()
            .caps(
                &gst::Caps::builder("video/x-raw")
                    .field("format", "RGBA")
                    .field("width", 640i32)
                    .field("height", 480i32)
                    .field("framerate", gst::Fraction::new(0, 1))
                    .build(),
            )
            .format(gst::Format::Time)
            .is_live(true)
            .build();
        let webcam = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .build()
            .expect("test webcam should be available");
        let pipeline =
            recording_pipeline_with_sources(screen.upcast(), Some(webcam), Vec::new(), &path)
                .expect("webcam recording pipeline should be constructible");
        pipeline
            .set_state(gst::State::Playing)
            .expect("webcam recording should start");
        std::thread::sleep(Duration::from_millis(100));
        let error =
            finish_recording(&pipeline).expect_err("recording without a screen frame must fail");
        assert!(
            error.contains("before receiving a video frame"),
            "failure should explain why no recording was saved: {error}"
        );
    }

    #[test]
    fn unavailable_webcam_reports_a_pipeline_error() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join("recording-with-missing-webcam.mp4");
        let screen = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .build()
            .expect("test screen source should be available");
        let webcam = gst::ElementFactory::make("v4l2src")
            .property("device", "/dev/jiffshot-test-missing-webcam")
            .build()
            .expect("V4L2 source should be available");
        let pipeline = recording_pipeline_with_sources(screen, Some(webcam), Vec::new(), &path)
            .expect("webcam recording pipeline should be constructible");
        let bus = pipeline
            .bus()
            .expect("recording pipeline should have a bus");
        let _start_result = pipeline.set_state(gst::State::Playing);
        let message = bus
            .timed_pop_filtered(gst::ClockTime::from_seconds(5), &[gst::MessageType::Error])
            .expect("missing webcam must produce an error");
        let gst::MessageView::Error(error) = message.view() else {
            panic!("the webcam failure should be an error message");
        };
        assert!(
            error
                .error()
                .to_string()
                .contains("jiffshot-test-missing-webcam"),
            "webcam error should identify the inaccessible device: {}",
            error.error()
        );
        pipeline
            .set_state(gst::State::Null)
            .expect("failed recording pipeline should stop");
    }

    fn last_recorded_video_timestamp(path: &Path) -> gst::ClockTime {
        use gstreamer_app as gst_app;

        let pipeline = gst::parse::launch(
            "filesrc name=input ! qtdemux name=demux \
             demux.video_0 ! queue ! decodebin ! appsink name=video sync=false",
        )
        .expect("recorded video decoder should parse")
        .downcast::<gst::Pipeline>()
        .expect("recorded video decoder should be a pipeline");
        pipeline
            .by_name("input")
            .expect("decoder should have a file source")
            .set_property("location", path.to_string_lossy().as_ref());
        let sink = pipeline
            .by_name("video")
            .expect("decoder should have a video sink")
            .downcast::<gst_app::AppSink>()
            .expect("video sink should be an appsink");
        pipeline
            .set_state(gst::State::Playing)
            .expect("recorded video should start decoding");
        let mut last = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "recorded video decoding should finish"
            );
            if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(250)) {
                let timestamp = sample
                    .buffer()
                    .and_then(|buffer| buffer.pts())
                    .expect("decoded frame should have a timestamp");
                last = Some(last.map_or(timestamp, |last: gst::ClockTime| last.max(timestamp)));
            } else if sink.is_eos() {
                break;
            } else if let Some(message) = pipeline
                .bus()
                .expect("video decoder should have a bus")
                .timed_pop_filtered(gst::ClockTime::ZERO, &[gst::MessageType::Error])
            {
                panic!("recorded video decoding failed: {message:?}");
            }
        }
        pipeline
            .set_state(gst::State::Null)
            .expect("video decoder should stop");
        last.expect("recording should contain a video frame")
    }

    fn decode_recording_audio(path: &Path) -> Vec<f32> {
        use gstreamer_app as gst_app;

        let pipeline = gst::parse::launch(
            "filesrc name=input ! qtdemux name=demux \
             demux.audio_0 ! queue ! decodebin ! audioconvert ! audioresample \
             ! audio/x-raw,format=F32LE,channels=1,rate=48000,layout=interleaved \
             ! appsink name=audio sync=false",
        )
        .expect("recorded audio decoder should parse")
        .downcast::<gst::Pipeline>()
        .expect("recorded audio decoder should be a pipeline");
        pipeline
            .by_name("input")
            .expect("decoder should have a file source")
            .set_property("location", path.to_string_lossy().as_ref());
        let sink = pipeline
            .by_name("audio")
            .expect("decoder should have an audio sink")
            .downcast::<gst_app::AppSink>()
            .expect("audio sink should be an appsink");
        pipeline
            .set_state(gst::State::Playing)
            .expect("recorded audio should start decoding");
        let mut audio = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "recorded audio decoding should finish"
            );
            if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(250)) {
                let buffer = sample.buffer().expect("audio sample should have a buffer");
                let mapped = buffer
                    .map_readable()
                    .expect("audio buffer should be readable");
                audio.extend(
                    mapped
                        .as_slice()
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|bytes| f32::from_le_bytes(*bytes)),
                );
            } else if sink.is_eos() {
                break;
            } else if let Some(message) = pipeline
                .bus()
                .expect("audio decoder should have a bus")
                .timed_pop_filtered(gst::ClockTime::ZERO, &[gst::MessageType::Error])
            {
                panic!("recorded audio decoding failed: {message:?}");
            }
        }
        pipeline
            .set_state(gst::State::Null)
            .expect("audio decoder should stop");
        audio
    }

    fn assert_audio_frequency(audio: &[f32], frequency: f64) {
        let amplitude = audio_frequency_amplitude(audio, frequency);
        assert!(
            amplitude > 0.03,
            "recorded audio should contain {frequency}Hz, measured amplitude {amplitude}"
        );
    }

    fn audio_frequency_amplitude(audio: &[f32], frequency: f64) -> f64 {
        let samples = audio
            .get(4_800..9_600)
            .expect("recording should contain audio between 0.1 and 0.2 seconds");
        let (sine, cosine) =
            samples
                .iter()
                .enumerate()
                .fold((0.0, 0.0), |(sine, cosine), (index, sample)| {
                    let phase = std::f64::consts::TAU * frequency * index as f64 / 48_000.0;
                    let sample = f64::from(*sample);
                    (sine + sample * phase.sin(), cosine + sample * phase.cos())
                });
        2.0 * sine.hypot(cosine) / samples.len() as f64
    }

    #[test]
    fn stopping_a_recording_finalizes_a_local_video_container() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join("recording.avi");
        let source = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .property("num-buffers", 30i32)
            .build()
            .expect("GStreamer test video source should be available");
        let convert = gst::ElementFactory::make("videoconvert")
            .build()
            .expect("GStreamer video converter should be available");
        let encoder = gst::ElementFactory::make("jpegenc")
            .build()
            .expect("GStreamer JPEG encoder should be available");
        let muxer = gst::ElementFactory::make("avimux")
            .build()
            .expect("GStreamer AVI muxer should be available");
        let sink = gst::ElementFactory::make("filesink")
            .property("location", path.to_string_lossy().as_ref())
            .build()
            .expect("GStreamer file sink should be available");
        let pipeline = gst::Pipeline::new();
        pipeline
            .add_many([&source, &convert, &encoder, &muxer, &sink])
            .expect("video test pipeline should be assembled");
        gst::Element::link_many([&source, &convert, &encoder, &muxer, &sink])
            .expect("video test elements should link");
        pipeline
            .set_state(gst::State::Playing)
            .expect("test recording should start");
        std::thread::sleep(Duration::from_millis(250));
        finish_recording(&pipeline).expect("stopping should finalize the video container");

        let bytes = std::fs::read(&path).expect("finalized video should be readable");
        assert!(bytes.len() > 1024, "video should contain encoded frames");
        assert_eq!(&bytes[..4], b"RIFF", "AVI should have a RIFF header");
        assert_eq!(&bytes[8..12], b"AVI ", "RIFF file should identify as AVI");
    }
}

#[cfg(test)]
#[path = "../../tests/recording_frame_rate/capture_tests.rs"]
mod recording_frame_rate_regression;
