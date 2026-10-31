//! Manual scrolling and panoramic capture through a portal-selected stream.

use ashpd::desktop::screencast::CursorMode;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use jiffshot_capture::{stitch, Frame};
use jiffshot_doc::Document;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use crate::frame_stream_linux::{document_from_frame, frame_from_sample};

const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(15);
const LOST_NOTICE_INTERVAL: Duration = Duration::from_secs(2);
const MAX_CAPTURE_BYTES: usize = 256 * 1024 * 1024;
const MAX_CAPTURE_EXTENT: u32 = 32_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Direction {
    Vertical,
    Horizontal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Mode {
    Scrolling(Direction),
    Panoramic,
}

impl Mode {
    fn description(self) -> &'static str {
        match self {
            Self::Scrolling(_) => "scrolling",
            Self::Panoramic => "panoramic",
        }
    }
}

impl Direction {
    fn orient(self, frame: Frame) -> Frame {
        match self {
            Self::Vertical => frame,
            Self::Horizontal => transpose(frame),
        }
    }

    fn scroll_instruction(self) -> &'static str {
        match self {
            Self::Vertical => "down",
            Self::Horizontal => "right",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Action {
    Finish,
    Cancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Progress {
    Started,
    Advanced(u32),
    PanoramicAdvanced { width: u32, height: u32 },
    LostOverlap,
}

pub(super) struct Captured {
    pub(super) document: Document,
    pub(super) limit_reached: bool,
}

#[derive(Clone, Copy)]
struct Limits {
    extent: u32,
    stored_bytes: usize,
    output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            extent: MAX_CAPTURE_EXTENT,
            stored_bytes: MAX_CAPTURE_BYTES,
            output_bytes: MAX_CAPTURE_BYTES,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum FrameUpdate {
    Unchanged,
    Added(u32),
    PanoramicAdded { width: u32, height: u32 },
    LostOverlap,
    LimitReached,
}

struct ScrollingFrames {
    frames: Vec<Frame>,
    output_extent: u32,
    limits: Limits,
    direction: Direction,
}

impl ScrollingFrames {
    fn new(frame: Frame, limits: Limits, direction: Direction) -> Result<Self, String> {
        validate_frame(&frame)?;
        let frame = direction.orient(frame);
        if frame.height > limits.extent
            || frame.pixels.len() > limits.stored_bytes
            || frame.pixels.len() > limits.output_bytes
        {
            return Err("the selected source exceeds the scrolling capture size limit".to_owned());
        }
        Ok(Self {
            output_extent: frame.height,
            frames: vec![frame],
            limits,
            direction,
        })
    }
}

struct PanoramicFrames {
    frames: Vec<(Frame, (i32, i32))>,
    bounds: (i32, i32, i32, i32),
    limits: Limits,
}

impl PanoramicFrames {
    fn new(frame: Frame, limits: Limits) -> Result<Self, String> {
        validate_frame(&frame)?;
        if frame.width > limits.extent
            || frame.height > limits.extent
            || frame.pixels.len() > limits.stored_bytes
            || frame.pixels.len() > limits.output_bytes
        {
            return Err("the selected source exceeds the panoramic capture size limit".to_owned());
        }
        let width = i32::try_from(frame.width)
            .map_err(|_| "the selected source is too wide for panoramic capture".to_owned())?;
        let height = i32::try_from(frame.height)
            .map_err(|_| "the selected source is too tall for panoramic capture".to_owned())?;
        Ok(Self {
            frames: vec![(frame, (0, 0))],
            bounds: (0, 0, width, height),
            limits,
        })
    }

    fn push(&mut self, frame: Frame) -> Result<FrameUpdate, String> {
        validate_frame(&frame)?;
        let (last, position) = self
            .frames
            .last()
            .ok_or_else(|| "panoramic capture has no initial frame".to_owned())?;
        if (frame.width, frame.height) != (last.width, last.height) {
            return Err("the selected source changed size during panoramic capture".to_owned());
        }
        if frame.pixels == last.pixels {
            return Ok(FrameUpdate::Unchanged);
        }
        let Some((dx, dy)) = stitch::translation(last, &frame) else {
            return Ok(FrameUpdate::LostOverlap);
        };
        if (dx, dy) == (0, 0) {
            return Ok(FrameUpdate::Unchanged);
        }
        let Some(x) = position.0.checked_add(dx) else {
            return Ok(FrameUpdate::LimitReached);
        };
        let Some(y) = position.1.checked_add(dy) else {
            return Ok(FrameUpdate::LimitReached);
        };
        let Some(right) = x.checked_add(frame.width as i32) else {
            return Ok(FrameUpdate::LimitReached);
        };
        let Some(bottom) = y.checked_add(frame.height as i32) else {
            return Ok(FrameUpdate::LimitReached);
        };
        let (left, top, old_right, old_bottom) = self.bounds;
        let bounds = (
            left.min(x),
            top.min(y),
            old_right.max(right),
            old_bottom.max(bottom),
        );
        let width = i64::from(bounds.2) - i64::from(bounds.0);
        let height = i64::from(bounds.3) - i64::from(bounds.1);
        let output_bytes = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(4));
        let stored_bytes = self
            .frames
            .len()
            .checked_add(1)
            .and_then(|count| count.checked_mul(frame.pixels.len()));
        if width > i64::from(self.limits.extent)
            || height > i64::from(self.limits.extent)
            || output_bytes.is_none_or(|bytes| bytes > self.limits.output_bytes)
            || stored_bytes.is_none_or(|bytes| bytes > self.limits.stored_bytes)
        {
            return Ok(FrameUpdate::LimitReached);
        }
        self.frames.push((frame, (x, y)));
        self.bounds = bounds;
        Ok(FrameUpdate::PanoramicAdded {
            width: width as u32,
            height: height as u32,
        })
    }

    fn finish(self) -> Result<Document, String> {
        let frame = stitch::compose(&self.frames)
            .ok_or_else(|| "could not stitch the panoramic capture".to_owned())?;
        drop(self.frames);
        document_from_frame(frame)
    }
}

enum CaptureFrames {
    Scrolling(ScrollingFrames),
    Panoramic(PanoramicFrames),
}

impl CaptureFrames {
    fn new(frame: Frame, mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Scrolling(direction) => {
                ScrollingFrames::new(frame, Limits::default(), direction).map(Self::Scrolling)
            }
            Mode::Panoramic => PanoramicFrames::new(frame, Limits::default()).map(Self::Panoramic),
        }
    }

    fn push(&mut self, frame: Frame) -> Result<FrameUpdate, String> {
        match self {
            Self::Scrolling(frames) => frames.push(frame),
            Self::Panoramic(frames) => frames.push(frame),
        }
    }

    fn finish(self) -> Result<Document, String> {
        match self {
            Self::Scrolling(frames) => frames.finish(),
            Self::Panoramic(frames) => frames.finish(),
        }
    }
}

impl ScrollingFrames {
    fn push(&mut self, frame: Frame) -> Result<FrameUpdate, String> {
        validate_frame(&frame)?;
        let frame = self.direction.orient(frame);
        let last = self
            .frames
            .last()
            .ok_or_else(|| "scrolling capture has no initial frame".to_owned())?;
        if (frame.width, frame.height) != (last.width, last.height) {
            return Err("the selected source changed size during scrolling capture".to_owned());
        }
        if frame.pixels == last.pixels {
            return Ok(FrameUpdate::Unchanged);
        }
        let Some(step) = stitch::vertical_step(last, &frame) else {
            return Ok(FrameUpdate::LostOverlap);
        };
        let Some(extent) = self.output_extent.checked_add(step.dy as u32) else {
            return Ok(FrameUpdate::LimitReached);
        };
        let output_bytes = (frame.width as usize)
            .checked_mul(extent as usize)
            .and_then(|pixels| pixels.checked_mul(4));
        let stored_bytes = self
            .frames
            .len()
            .checked_add(1)
            .and_then(|count| count.checked_mul(frame.pixels.len()));
        if extent > self.limits.extent
            || output_bytes.is_none_or(|bytes| bytes > self.limits.output_bytes)
            || stored_bytes.is_none_or(|bytes| bytes > self.limits.stored_bytes)
        {
            return Ok(FrameUpdate::LimitReached);
        }
        self.frames.push(frame);
        self.output_extent = extent;
        Ok(FrameUpdate::Added(extent))
    }

    fn finish(self) -> Result<Document, String> {
        let Self {
            frames, direction, ..
        } = self;
        if frames.len() < 2 {
            return Err(format!(
                "no scrolling was detected; scroll {} slowly before finishing capture",
                direction.scroll_instruction()
            ));
        }
        let frame = stitch::stitch_vertical(&frames)
            .ok_or_else(|| "could not stitch the captured frames".to_owned())?;
        drop(frames);
        document_from_frame(direction.orient(frame))
    }
}

// Treat columns as rows so the vertical stitcher also preserves fixed side bands.
fn transpose(frame: Frame) -> Frame {
    let width = frame.width as usize;
    let height = frame.height as usize;
    let mut pixels = vec![0; frame.pixels.len()];
    for y in 0..height {
        for x in 0..width {
            let source = (y * width + x) * 4;
            let destination = (x * height + y) * 4;
            pixels[destination..destination + 4].copy_from_slice(&frame.pixels[source..source + 4]);
        }
    }
    Frame {
        width: frame.height,
        height: frame.width,
        pixels,
        alpha: frame.alpha,
    }
}

fn validate_frame(frame: &Frame) -> Result<(), String> {
    let expected = (frame.width as usize)
        .checked_mul(frame.height as usize)
        .and_then(|pixels| pixels.checked_mul(4));
    if frame.width == 0
        || frame.height == 0
        || expected != Some(frame.pixels.len())
        || frame.pixels.len() > MAX_CAPTURE_BYTES
    {
        return Err("the screen capture stream returned an invalid or oversized frame".to_owned());
    }
    Ok(())
}

pub(super) async fn capture(
    mode: Mode,
    stop: async_channel::Receiver<Action>,
    progress: impl FnMut(Progress),
) -> Result<Option<Captured>, String> {
    gst::init().map_err(|error| format!("could not initialize GStreamer: {error}"))?;
    let stream = crate::capture_linux::open_portal_stream(CursorMode::Hidden).await?;
    let result = crate::capture_linux::pipewire_source(stream.remote.as_raw_fd(), stream.node_id)
        .and_then(|source| capture_from_source(source, mode, stop, progress));
    stream.finish(result).await
}

fn capture_from_source(
    source: gst::Element,
    mode: Mode,
    stop: async_channel::Receiver<Action>,
    mut progress: impl FnMut(Progress),
) -> Result<Option<Captured>, String> {
    let description = format!("{} capture", mode.description());
    crate::frame_stream_linux::with_source(source, &description, |sink, bus| {
        capture_frames(sink, bus, mode, &stop, &mut progress)
    })
}

fn capture_frames(
    sink: &gst_app::AppSink,
    bus: &gst::Bus,
    mode: Mode,
    stop: &async_channel::Receiver<Action>,
    progress: &mut impl FnMut(Progress),
) -> Result<Option<Captured>, String> {
    let started = Instant::now();
    let mut next_sample = started;
    let mut last_lost_notice = None;
    let mut frames: Option<CaptureFrames> = None;
    let mut pending = None;
    loop {
        match stop.try_recv() {
            Ok(Action::Cancel) | Err(async_channel::TryRecvError::Closed) => return Ok(None),
            Ok(Action::Finish) => {
                let limit_reached = if let Some(sample) = pending.take() {
                    process_sample(sample, mode, &mut frames, &mut last_lost_notice, progress)?
                } else {
                    false
                };
                return finish_capture(frames, mode, limit_reached);
            }
            Err(async_channel::TryRecvError::Empty) => {}
        }
        if let Some(message) = bus.timed_pop_filtered(
            gst::ClockTime::ZERO,
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) {
            return match message.view() {
                gst::MessageView::Error(error) => Err(format!(
                    "{} capture failed: {}",
                    mode.description(),
                    error.error()
                )),
                _ => Err(format!(
                    "the {} capture stream ended unexpectedly",
                    mode.description()
                )),
            };
        }
        if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
            pending = Some(sample);
        } else {
            if matches!(
                sink.current_state(),
                gst::State::Paused | gst::State::Playing
            ) && sink.is_eos()
            {
                return Err(format!(
                    "the {} capture stream ended unexpectedly",
                    mode.description()
                ));
            }
            if frames.is_none() && started.elapsed() >= FIRST_FRAME_TIMEOUT {
                return Err(format!(
                    "timed out waiting for the first {} capture frame",
                    mode.description()
                ));
            }
        }
        if pending.is_none() || Instant::now() < next_sample {
            continue;
        }
        next_sample = Instant::now() + SAMPLE_INTERVAL;
        if let Some(sample) = pending.take() {
            if process_sample(sample, mode, &mut frames, &mut last_lost_notice, progress)? {
                return finish_capture(frames, mode, true);
            }
        }
    }
}

fn process_sample(
    sample: gst::Sample,
    mode: Mode,
    frames: &mut Option<CaptureFrames>,
    last_lost_notice: &mut Option<Instant>,
    progress: &mut impl FnMut(Progress),
) -> Result<bool, String> {
    let frame = frame_from_sample(&sample)?;
    let Some(existing) = frames else {
        *frames = Some(CaptureFrames::new(frame, mode)?);
        progress(Progress::Started);
        return Ok(false);
    };
    match existing.push(frame)? {
        FrameUpdate::Unchanged => {}
        FrameUpdate::Added(extent) => {
            *last_lost_notice = None;
            progress(Progress::Advanced(extent));
        }
        FrameUpdate::PanoramicAdded { width, height } => {
            *last_lost_notice = None;
            progress(Progress::PanoramicAdvanced { width, height });
        }
        FrameUpdate::LostOverlap => {
            if last_lost_notice.is_none_or(|last| last.elapsed() >= LOST_NOTICE_INTERVAL) {
                *last_lost_notice = Some(Instant::now());
                progress(Progress::LostOverlap);
            }
        }
        FrameUpdate::LimitReached => return Ok(true),
    }
    Ok(false)
}

fn finish_capture(
    frames: Option<CaptureFrames>,
    mode: Mode,
    limit_reached: bool,
) -> Result<Option<Captured>, String> {
    let frames = frames.ok_or_else(|| {
        format!(
            "{} capture stopped before its first frame",
            mode.description()
        )
    })?;
    Ok(Some(Captured {
        document: frames.finish()?,
        limit_reached,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gstreamer_video as gst_video;

    fn view(top: u32) -> Frame {
        let (width, height) = (128, 100);
        let mut frame = Frame::new(width, height);
        for y in 0..height {
            let source_y = if y < 5 {
                y
            } else if y >= height - 5 {
                1000 + y
            } else {
                y + top
            };
            for x in 0..width {
                let at = (y as usize * width as usize + x as usize) * 4;
                frame.pixels[at..at + 4].copy_from_slice(&[
                    (source_y * 37) as u8,
                    (source_y / 256) as u8,
                    (x * 3 + source_y * 13) as u8,
                    255,
                ]);
            }
        }
        frame
    }

    fn horizontal_view(left: u32) -> Frame {
        let (width, height) = (100, 128);
        let mut frame = Frame::new(width, height);
        for y in 0..height {
            for x in 0..width {
                let source_x = if x < 5 {
                    x
                } else if x >= width - 5 {
                    1000 + x
                } else {
                    x + left
                };
                let at = (y as usize * width as usize + x as usize) * 4;
                let pixel = if y < 6 || y >= height - 6 {
                    [(x * 11 + y * 7) as u8, y as u8, (x * 3) as u8, 255]
                } else {
                    [
                        (source_x * 37) as u8,
                        (source_x / 256) as u8,
                        (y * 3 + source_x * 13) as u8,
                        255,
                    ]
                };
                frame.pixels[at..at + 4].copy_from_slice(&pixel);
            }
        }
        frame
    }

    fn panoramic_pixel(x: u32, y: u32) -> [u8; 4] {
        let mut value = (x / 6).wrapping_mul(0x9e37_79b9) ^ (y / 6).wrapping_mul(0x85eb_ca6b);
        value ^= value >> 15;
        value = value.wrapping_mul(0x2c1b_3c6d);
        [
            (value >> 24) as u8,
            (value >> 16) as u8,
            (y % 251) as u8,
            255,
        ]
    }

    fn panoramic_view(left: u32, top: u32, width: u32, height: u32) -> Frame {
        let mut frame = Frame::new(width, height);
        for y in 0..height {
            for x in 0..width {
                let at = (y as usize * width as usize + x as usize) * 4;
                frame.pixels[at..at + 4].copy_from_slice(&panoramic_pixel(left + x, top + y));
            }
        }
        frame
    }

    #[test]
    fn panoramic_capture_stitches_both_axes_and_keeps_uncovered_pixels_transparent() {
        let mut frames =
            PanoramicFrames::new(panoramic_view(48, 48, 96, 80), Limits::default()).unwrap();
        assert_eq!(
            frames.push(panoramic_view(72, 60, 96, 80)).unwrap(),
            FrameUpdate::PanoramicAdded {
                width: 120,
                height: 92,
            }
        );
        assert_eq!(
            frames.push(panoramic_view(36, 24, 96, 80)).unwrap(),
            FrameUpdate::PanoramicAdded {
                width: 132,
                height: 116,
            }
        );
        let document = frames.finish().unwrap();
        assert_eq!(document.size(), (132, 116));
        let expected = panoramic_pixel(41, 29);
        let pixel = document.background().pixel(5, 5).unwrap().demultiply();
        assert_eq!(
            [pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()],
            [expected[2], expected[1], expected[0], 255]
        );
        assert_eq!(
            document.background().pixel(131, 0).unwrap().alpha(),
            0,
            "a corner never seen by any captured frame must remain transparent"
        );
    }

    #[test]
    fn panoramic_capture_recovers_from_lost_overlap_and_rejects_resizing() {
        let first = panoramic_view(48, 48, 96, 80);
        let mut frames = PanoramicFrames::new(first.clone(), Limits::default()).unwrap();
        assert_eq!(frames.push(first).unwrap(), FrameUpdate::Unchanged);
        assert_eq!(
            frames.push(Frame::new(96, 80)).unwrap(),
            FrameUpdate::LostOverlap
        );
        assert!(frames
            .push(Frame::new(97, 80))
            .unwrap_err()
            .contains("changed size"));
        assert_eq!(
            frames.push(panoramic_view(72, 60, 96, 80)).unwrap(),
            FrameUpdate::PanoramicAdded {
                width: 120,
                height: 92,
            }
        );
        assert_eq!(frames.finish().unwrap().size(), (120, 92));
    }

    #[test]
    fn panoramic_capture_enforces_exact_extent_storage_and_output_limits() {
        let first = panoramic_view(48, 48, 96, 80);
        let bytes = first.pixels.len();
        let limits = Limits {
            extent: 120,
            stored_bytes: bytes * 2,
            output_bytes: 120 * 92 * 4,
        };
        let mut frames = PanoramicFrames::new(first.clone(), limits).unwrap();
        assert_eq!(
            frames.push(panoramic_view(72, 60, 96, 80)).unwrap(),
            FrameUpdate::PanoramicAdded {
                width: 120,
                height: 92,
            }
        );
        assert_eq!(
            frames.push(panoramic_view(36, 24, 96, 80)).unwrap(),
            FrameUpdate::LimitReached
        );
        assert_eq!(frames.finish().unwrap().size(), (120, 92));
        for too_small in [
            Limits {
                extent: 95,
                ..limits
            },
            Limits {
                stored_bytes: bytes - 1,
                ..limits
            },
            Limits {
                output_bytes: bytes - 1,
                ..limits
            },
        ] {
            assert!(PanoramicFrames::new(first.clone(), too_small).is_err());
        }
    }

    #[test]
    fn panoramic_capture_without_motion_saves_the_first_frame() {
        let frames =
            PanoramicFrames::new(panoramic_view(48, 48, 96, 80), Limits::default()).unwrap();
        let document = frames.finish().unwrap();
        assert_eq!(document.size(), (96, 80));
        assert_eq!(document.background().pixel(0, 0).unwrap().alpha(), 255);
    }

    #[test]
    fn stitches_scrolling_views_with_fixed_bands_and_correct_colors() {
        let mut frames =
            ScrollingFrames::new(view(0), Limits::default(), Direction::Vertical).unwrap();
        assert_eq!(frames.push(view(40)).unwrap(), FrameUpdate::Added(140));
        assert_eq!(frames.push(view(80)).unwrap(), FrameUpdate::Added(180));
        let document = frames.finish().unwrap();
        assert_eq!(document.size(), (128, 180));
        let pixels = document.background().data();
        let at = (70 * 128 + 9) * 4;
        assert_eq!(
            &pixels[at..at + 4],
            &[(9 * 3 + 70 * 13) as u8, 0, (70 * 37) as u8, 255]
        );
        let footer = ((180 - 1) * 128 + 9) * 4;
        assert_eq!(
            &pixels[footer..footer + 4],
            &[
                (9 * 3 + 1099 * 13) as u8,
                (1099 / 256) as u8,
                (1099 * 37) as u8,
                255
            ]
        );
    }

    #[test]
    fn stitches_horizontal_views_with_fixed_bands_and_correct_colors() {
        let mut frames =
            ScrollingFrames::new(horizontal_view(0), Limits::default(), Direction::Horizontal)
                .unwrap();
        assert_eq!(
            frames.push(horizontal_view(40)).unwrap(),
            FrameUpdate::Added(140)
        );
        assert_eq!(
            frames.push(horizontal_view(80)).unwrap(),
            FrameUpdate::Added(180)
        );
        let document = frames.finish().unwrap();
        assert_eq!(document.size(), (180, 128));
        let pixels = document.background().data();
        for x in [70, 150] {
            let at = (9 * 180 + x) * 4;
            assert_eq!(
                &pixels[at..at + 4],
                &[(9 * 3 + x * 13) as u8, 0, (x * 37) as u8, 255],
                "column {x}"
            );
        }
        let footer = (9 * 180 + 179) * 4;
        assert_eq!(
            &pixels[footer..footer + 4],
            &[
                (9 * 3 + 1099 * 13) as u8,
                (1099 / 256) as u8,
                (1099 * 37) as u8,
                255
            ]
        );
    }

    #[test]
    fn horizontal_capture_recovers_from_lost_overlap_and_respects_extent_limit() {
        let limits = Limits {
            extent: 150,
            ..Limits::default()
        };
        let mut frames =
            ScrollingFrames::new(horizontal_view(0), limits, Direction::Horizontal).unwrap();
        assert_eq!(
            frames.push(horizontal_view(0)).unwrap(),
            FrameUpdate::Unchanged
        );
        let mut unrelated = horizontal_view(0);
        unrelated.pixels.fill(0);
        assert_eq!(frames.push(unrelated).unwrap(), FrameUpdate::LostOverlap);
        assert_eq!(
            frames.push(horizontal_view(40)).unwrap(),
            FrameUpdate::Added(140)
        );
        assert_eq!(
            frames.push(horizontal_view(80)).unwrap(),
            FrameUpdate::LimitReached
        );
        assert_eq!(frames.finish().unwrap().size(), (140, 128));

        let still =
            ScrollingFrames::new(horizontal_view(0), limits, Direction::Horizontal).unwrap();
        assert!(still.finish().unwrap_err().contains("scroll right"));
    }

    #[test]
    fn horizontal_capture_enforces_exact_byte_limits_and_rejects_resized_sources() {
        let frame_bytes = horizontal_view(0).pixels.len();
        let limits = Limits {
            stored_bytes: frame_bytes * 2,
            output_bytes: 140 * 128 * 4,
            ..Limits::default()
        };
        let mut frames =
            ScrollingFrames::new(horizontal_view(0), limits, Direction::Horizontal).unwrap();
        assert!(frames
            .push(Frame::new(101, 128))
            .unwrap_err()
            .contains("changed size"));
        assert_eq!(
            frames.push(horizontal_view(40)).unwrap(),
            FrameUpdate::Added(140)
        );
        assert_eq!(
            frames.push(horizontal_view(80)).unwrap(),
            FrameUpdate::LimitReached
        );
        assert_eq!(frames.finish().unwrap().size(), (140, 128));
        let too_small = Limits {
            output_bytes: frame_bytes - 1,
            ..Limits::default()
        };
        assert!(
            ScrollingFrames::new(horizontal_view(0), too_small, Direction::Horizontal).is_err()
        );
    }

    #[test]
    fn ignores_duplicate_and_unmatched_frames_then_recovers() {
        let mut frames =
            ScrollingFrames::new(view(0), Limits::default(), Direction::Vertical).unwrap();
        assert_eq!(frames.push(view(0)).unwrap(), FrameUpdate::Unchanged);
        let mut unrelated = view(0);
        unrelated.pixels.fill(0);
        assert_eq!(frames.push(unrelated).unwrap(), FrameUpdate::LostOverlap);
        assert_eq!(frames.push(view(40)).unwrap(), FrameUpdate::Added(140));
        assert_eq!(frames.finish().unwrap().height(), 140);
    }

    #[test]
    fn rejects_no_motion_and_changed_source_dimensions() {
        let mut frames =
            ScrollingFrames::new(view(0), Limits::default(), Direction::Vertical).unwrap();
        let changed = Frame::new(129, 100);
        assert!(frames.push(changed).unwrap_err().contains("changed size"));
        assert!(frames.finish().unwrap_err().contains("no scrolling"));
    }

    #[test]
    fn finishes_at_the_last_complete_frame_when_a_limit_is_reached() {
        let limits = Limits {
            extent: 150,
            ..Limits::default()
        };
        let mut frames = ScrollingFrames::new(view(0), limits, Direction::Vertical).unwrap();
        assert_eq!(frames.push(view(40)).unwrap(), FrameUpdate::Added(140));
        assert_eq!(frames.push(view(80)).unwrap(), FrameUpdate::LimitReached);
        assert_eq!(frames.finish().unwrap().height(), 140);
    }

    #[test]
    fn enforces_storage_and_output_limits_at_the_exact_boundary() {
        let frame_bytes = view(0).pixels.len();
        let limits = Limits {
            stored_bytes: frame_bytes * 2,
            output_bytes: 128 * 140 * 4,
            ..Limits::default()
        };
        let mut frames = ScrollingFrames::new(view(0), limits, Direction::Vertical).unwrap();
        assert_eq!(frames.push(view(40)).unwrap(), FrameUpdate::Added(140));
        assert_eq!(frames.push(view(80)).unwrap(), FrameUpdate::LimitReached);
        assert_eq!(frames.finish().unwrap().height(), 140);

        let too_small = Limits {
            stored_bytes: frame_bytes - 1,
            ..Limits::default()
        };
        assert!(ScrollingFrames::new(view(0), too_small, Direction::Vertical).is_err());
    }

    #[test]
    fn rejects_invalid_frame_buffers() {
        let mut frame = view(0);
        frame.pixels.pop();
        assert!(ScrollingFrames::new(frame, Limits::default(), Direction::Vertical).is_err());
        assert!(
            ScrollingFrames::new(Frame::new(0, 0), Limits::default(), Direction::Vertical).is_err()
        );
    }

    #[test]
    fn copies_bgra_rows_with_video_metadata_padding() {
        gst::init().unwrap();
        let caps = gst::Caps::builder("video/x-raw")
            .field("format", "BGRA")
            .field("width", 2i32)
            .field("height", 2i32)
            .field("framerate", gst::Fraction::new(1, 1))
            .build();
        let mut buffer = gst::Buffer::from_mut_slice(vec![
            3, 2, 1, 8, 6, 5, 4, 9, 99, 99, 99, 99, 9, 8, 7, 10, 12, 11, 10, 11, 99, 99, 99, 99,
        ]);
        gst_video::VideoMeta::add_full(
            buffer.get_mut().unwrap(),
            gst_video::VideoFrameFlags::empty(),
            gst_video::VideoFormat::Bgra,
            2,
            2,
            &[0],
            &[12],
        )
        .unwrap();
        let sample = gst::Sample::builder().buffer(&buffer).caps(&caps).build();
        let frame = frame_from_sample(&sample).unwrap();
        assert_eq!(
            frame.pixels,
            vec![3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255]
        );
    }

    #[test]
    fn captures_streamed_frames_and_finishes_after_a_scroll() {
        gst::init().unwrap();
        let source = gst_app::AppSrc::builder()
            .caps(
                &gst::Caps::builder("video/x-raw")
                    .field("format", "BGRA")
                    .field("width", 128i32)
                    .field("height", 100i32)
                    .field("framerate", gst::Fraction::new(0, 1))
                    .build(),
            )
            .format(gst::Format::Time)
            .is_live(true)
            .build();
        let (stop, receiver) = async_channel::bounded(1);
        let (events, observed) = std::sync::mpsc::channel();
        let producer = source.clone();
        let worker = std::thread::spawn(move || {
            producer
                .push_buffer(gst::Buffer::from_mut_slice(view(0).pixels))
                .unwrap();
            assert_eq!(
                observed.recv_timeout(Duration::from_secs(10)).unwrap(),
                Progress::Started
            );
            producer
                .push_buffer(gst::Buffer::from_mut_slice(view(40).pixels))
                .unwrap();
            assert_eq!(
                observed.recv_timeout(Duration::from_secs(10)).unwrap(),
                Progress::Advanced(140)
            );
            stop.send_blocking(Action::Finish).unwrap();
        });
        let captured = capture_from_source(
            source.upcast(),
            Mode::Scrolling(Direction::Vertical),
            receiver,
            move |event| {
                events.send(event).unwrap();
            },
        )
        .unwrap()
        .expect("finishing should return an image");
        worker.join().unwrap();
        assert!(!captured.limit_reached);
        assert_eq!(captured.document.size(), (128, 140));
    }

    #[test]
    fn captures_horizontal_streamed_frames_and_finishes_after_a_scroll() {
        gst::init().unwrap();
        let source = gst_app::AppSrc::builder()
            .caps(
                &gst::Caps::builder("video/x-raw")
                    .field("format", "BGRA")
                    .field("width", 100i32)
                    .field("height", 128i32)
                    .field("framerate", gst::Fraction::new(0, 1))
                    .build(),
            )
            .format(gst::Format::Time)
            .is_live(true)
            .build();
        let (stop, receiver) = async_channel::bounded(1);
        let (events, observed) = std::sync::mpsc::channel();
        let producer = source.clone();
        let worker = std::thread::spawn(move || {
            producer
                .push_buffer(gst::Buffer::from_mut_slice(horizontal_view(0).pixels))
                .unwrap();
            assert_eq!(
                observed.recv_timeout(Duration::from_secs(10)).unwrap(),
                Progress::Started
            );
            producer
                .push_buffer(gst::Buffer::from_mut_slice(horizontal_view(40).pixels))
                .unwrap();
            assert_eq!(
                observed.recv_timeout(Duration::from_secs(10)).unwrap(),
                Progress::Advanced(140)
            );
            stop.send_blocking(Action::Finish).unwrap();
        });
        let captured = capture_from_source(
            source.upcast(),
            Mode::Scrolling(Direction::Horizontal),
            receiver,
            move |event| {
                events.send(event).unwrap();
            },
        )
        .unwrap()
        .expect("finishing should return an image");
        worker.join().unwrap();
        assert!(!captured.limit_reached);
        assert_eq!(captured.document.size(), (140, 128));
    }

    #[test]
    fn captures_panoramic_streamed_frames_and_finishes_after_panning() {
        gst::init().unwrap();
        let source = gst_app::AppSrc::builder()
            .caps(
                &gst::Caps::builder("video/x-raw")
                    .field("format", "BGRA")
                    .field("width", 96i32)
                    .field("height", 80i32)
                    .field("framerate", gst::Fraction::new(0, 1))
                    .build(),
            )
            .format(gst::Format::Time)
            .is_live(true)
            .build();
        let (stop, receiver) = async_channel::bounded(1);
        let (events, observed) = std::sync::mpsc::channel();
        let producer = source.clone();
        let worker = std::thread::spawn(move || {
            producer
                .push_buffer(gst::Buffer::from_mut_slice(
                    panoramic_view(48, 48, 96, 80).pixels,
                ))
                .unwrap();
            assert_eq!(
                observed.recv_timeout(Duration::from_secs(10)).unwrap(),
                Progress::Started
            );
            producer
                .push_buffer(gst::Buffer::from_mut_slice(
                    panoramic_view(72, 60, 96, 80).pixels,
                ))
                .unwrap();
            assert_eq!(
                observed.recv_timeout(Duration::from_secs(10)).unwrap(),
                Progress::PanoramicAdvanced {
                    width: 120,
                    height: 92,
                }
            );
            stop.send_blocking(Action::Finish).unwrap();
        });
        let captured =
            capture_from_source(source.upcast(), Mode::Panoramic, receiver, move |event| {
                events.send(event).unwrap();
            })
            .unwrap()
            .expect("finishing should return an image");
        worker.join().unwrap();
        assert!(!captured.limit_reached);
        assert_eq!(captured.document.size(), (120, 92));
        assert_eq!(
            captured
                .document
                .background()
                .pixel(119, 0)
                .unwrap()
                .alpha(),
            0
        );
    }

    #[test]
    fn cancellation_before_the_first_frame_discards_the_capture() {
        gst::init().unwrap();
        for mode in [Mode::Scrolling(Direction::Vertical), Mode::Panoramic] {
            let source = gst::ElementFactory::make("videotestsrc")
                .property("is-live", true)
                .build()
                .unwrap();
            let (stop, receiver) = async_channel::bounded(1);
            stop.send_blocking(Action::Cancel).unwrap();
            assert!(capture_from_source(source, mode, receiver, |_| {})
                .unwrap()
                .is_none());
        }
    }

    #[test]
    fn reports_an_ended_stream_instead_of_saving_a_partial_capture() {
        gst::init().unwrap();
        let source = gst::ElementFactory::make("videotestsrc")
            .property("num-buffers", 1i32)
            .property("is-live", true)
            .build()
            .unwrap();
        let (_stop, receiver) = async_channel::bounded(1);
        let result = capture_from_source(
            source,
            Mode::Scrolling(Direction::Vertical),
            receiver,
            |_| {},
        );
        assert!(
            matches!(result, Err(error) if error.contains("ended unexpectedly")),
            "an ended source should be reported as a failure"
        );
    }

    #[test]
    #[ignore = "requires the isolated Sway, PipeWire, and ScreenCast portal test environment"]
    fn portal_stream_stitches_scrolled_desktop_pixels() {
        verify_portal_scroll(Direction::Vertical);
    }

    #[test]
    #[ignore = "requires the isolated Sway, PipeWire, and ScreenCast portal test environment"]
    fn portal_stream_stitches_horizontal_desktop_pixels() {
        verify_portal_scroll(Direction::Horizontal);
    }

    #[test]
    #[ignore = "requires the isolated Sway, PipeWire, and ScreenCast portal test environment"]
    fn portal_stream_stitches_panoramic_desktop_pixels() {
        let directory = tempfile::tempdir().unwrap();
        let image_at = |left, top| {
            let mut image = image::RgbaImage::new(1280, 720);
            for (x, y, pixel) in image.enumerate_pixels_mut() {
                let bgra = panoramic_pixel(left + x, top + y);
                *pixel = image::Rgba([bgra[2], bgra[1], bgra[0], 255]);
            }
            let path = directory.path().join(format!("{left}-{top}.png"));
            image.save(&path).unwrap();
            path
        };
        let first = image_at(48, 48);
        let second = image_at(128, 108);
        let set_background = |path: &std::path::Path| {
            let output = std::process::Command::new("swaymsg")
                .args(["output", "HEADLESS-1", "bg"])
                .arg(path)
                .arg("stretch")
                .output()
                .expect("swaymsg should update the private test output");
            assert!(
                output.status.success(),
                "could not set test background: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        set_background(&first);
        std::thread::sleep(Duration::from_secs(1));
        let (stop, receiver) = async_channel::bounded(1);
        let captured = futures_lite::future::block_on(capture(
            Mode::Panoramic,
            receiver,
            |event| match event {
                Progress::Started => set_background(&second),
                Progress::PanoramicAdvanced { .. } => {
                    stop.try_send(Action::Finish).unwrap();
                }
                Progress::Advanced(_) | Progress::LostOverlap => {}
            },
        ))
        .unwrap()
        .expect("the portal capture should produce an image");
        assert!(!captured.limit_reached);
        assert_eq!(captured.document.size(), (1360, 780));
        let pixel = captured
            .document
            .background()
            .pixel(1350, 760)
            .unwrap()
            .demultiply();
        let expected = panoramic_pixel(1398, 808);
        assert_eq!(
            [pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()],
            [expected[2], expected[1], expected[0], 255]
        );
        assert_eq!(
            captured
                .document
                .background()
                .pixel(1359, 0)
                .unwrap()
                .alpha(),
            0
        );
    }

    fn verify_portal_scroll(direction: Direction) {
        let directory = tempfile::tempdir().unwrap();
        let image_at = |offset: u32| {
            let mut image = image::RgbaImage::new(1280, 720);
            for (x, y, pixel) in image.enumerate_pixels_mut() {
                if direction == Direction::Horizontal && y < 32 {
                    *pixel = image::Rgba([(x * 11 + y * 7) as u8, y as u8, (x * 3) as u8, 255]);
                    continue;
                }
                let (cross, source) = match direction {
                    Direction::Vertical => (x, y + offset),
                    Direction::Horizontal => (y, x + offset),
                };
                *pixel = image::Rgba([
                    (cross * 3 + source * 13) as u8,
                    (source / 256) as u8,
                    (source * 37) as u8,
                    255,
                ]);
            }
            let path = directory.path().join(format!("{offset}.png"));
            image.save(&path).unwrap();
            path
        };
        let first = image_at(0);
        let second = image_at(160);
        let set_background = |path: &std::path::Path| {
            let output = std::process::Command::new("swaymsg")
                .args(["output", "HEADLESS-1", "bg"])
                .arg(path)
                .arg("stretch")
                .output()
                .expect("swaymsg should update the private test output");
            assert!(
                output.status.success(),
                "could not set test background: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        set_background(&first);
        std::thread::sleep(Duration::from_secs(1));
        let (stop, receiver) = async_channel::bounded(1);
        let mut started = false;
        let captured = futures_lite::future::block_on(capture(
            Mode::Scrolling(direction),
            receiver,
            |event| match event {
                Progress::Started => {
                    assert!(!started);
                    started = true;
                    set_background(&second);
                }
                Progress::Advanced(_) => stop.try_send(Action::Finish).unwrap(),
                Progress::LostOverlap | Progress::PanoramicAdvanced { .. } => {}
            },
        ))
        .unwrap()
        .expect("the portal capture should produce an image");
        assert!(started);
        assert!(!captured.limit_reached);
        let (dimensions, point) = match direction {
            Direction::Vertical => ((1280, 880), (100, 800)),
            Direction::Horizontal => ((1440, 720), (1400, 100)),
        };
        assert_eq!(captured.document.size(), dimensions);
        let pixel = captured
            .document
            .background()
            .pixel(point.0, point.1)
            .unwrap()
            .demultiply();
        let (cross, source) = match direction {
            Direction::Vertical => (point.0, point.1),
            Direction::Horizontal => (point.1, point.0),
        };
        assert_eq!(
            (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
            (
                (cross * 3 + source * 13) as u8,
                (source / 256) as u8,
                (source * 37) as u8,
                255
            )
        );
    }
}
