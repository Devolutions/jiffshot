//! Shared, transactional video post-production for the editor and agent control server.

use super::{AudioSpec, Reader, VideoFrame, VideoSpec, Writer, HNS_PER_SECOND};
use jiffshot_doc::{raster, Pixmap};
use serde::Serialize;
use std::fmt;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A post-production error with input or output context.
#[derive(Debug)]
pub struct ExportError(String);

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ExportError {}
impl From<String> for ExportError {
    fn from(value: String) -> Self {
        Self(value)
    }
}
impl From<std::io::Error> for ExportError {
    fn from(value: std::io::Error) -> Self {
        Self(value.to_string())
    }
}

type Result<T> = std::result::Result<T, ExportError>;

/// One source range and playback speed, measured in seconds and a speed factor.
#[derive(Clone, Debug)]
pub struct Segment {
    pub path: PathBuf,
    pub from: f64,
    pub to: f64,
    pub speed: f64,
}

/// How sound is handled when re-encoding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Audio {
    /// Preserve sound only if all sources have matching AAC-compatible formats.
    #[default]
    PreserveCompatible,
    /// Preserve source sound or fail before writing if any sound would be discarded.
    RequireCompatible,
    /// Remove all sound.
    Mute,
}

/// MP4 encoding choices; omitted values use the first source's dimensions and rate.
#[derive(Clone, Debug, Default)]
pub struct Mp4Options {
    pub fps: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Requested output duration in seconds, rounded to the nearest frame.
    pub duration: Option<f64>,
    /// Overlap adjacent segments and crossfade their video and audio.
    pub transition_seconds: Option<f64>,
    /// Optional per-boundary transition selection, in segment order.
    #[doc(hidden)]
    pub transition_boundaries: Option<Vec<bool>>,
    pub audio: Audio,
    /// Static PNG/image or .jsd overlays, timed on the joined output timeline.
    pub overlays: Vec<Overlay>,
    /// Timed text captions, rendered with the document text engine.
    pub captions: Vec<Caption>,
}

/// A static overlay in output-canvas pixels, visible during `[from, to)` seconds.
#[derive(Clone, Debug)]
pub struct Overlay {
    pub path: PathBuf,
    pub x: i32,
    pub y: i32,
    pub from: f64,
    pub to: f64,
}

/// A text overlay in output-canvas pixels, visible during `[from, to)` seconds.
#[derive(Clone, Debug)]
pub struct Caption {
    pub text: String,
    pub x: i32,
    pub y: i32,
    pub from: f64,
    pub to: f64,
    /// Time basis for `from` and `to`; source-timeline captions need a segment index.
    pub timeline: CaptionTimeline,
    pub segment: Option<usize>,
    pub font: String,
    pub size: f32,
    pub color: jiffshot_doc::Rgba,
    pub background: jiffshot_doc::Rgba,
    pub padding: u32,
    pub fade: f64,
    /// Unclipped fade interval after mapping a source caption to an output timeline.
    #[doc(hidden)]
    pub fade_window: Option<(f64, f64)>,
    /// Canvas on which recorded-caption coordinates were measured.
    #[doc(hidden)]
    pub reference_canvas: Option<(u32, u32)>,
}

/// Timeline used by a caption's time range.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CaptionTimeline {
    #[default]
    Output,
    Source,
}

/// GIF encoding choices, shared with the editor.
#[derive(Clone, Debug)]
pub struct GifOptions {
    pub fps: f64,
    pub max_width: u32,
    /// Requested output duration in seconds, rounded to GIF frame delays.
    pub duration: Option<f64>,
    pub looped: bool,
    pub fade: bool,
    pub dither: bool,
    /// Selects the quantization dither; when absent, the legacy `dither` flag is used.
    pub dither_mode: Option<GifDither>,
    /// Optional maximum number of colors after RGB quantization (2..=256).
    pub palette_size: u16,
    /// Merge identical frames and encode changed opaque regions while preserving timing.
    pub optimize_frames: bool,
    /// Optional maximum file size; width and frame rate are reduced if needed.
    pub max_bytes: Option<u64>,
    /// Static overlays placed after GIF resizing, timed from the output's start.
    pub overlays: Vec<Overlay>,
    /// Timed text captions placed after GIF resizing.
    pub captions: Vec<Caption>,
}

/// Dithering used before reducing GIF colors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GifDither {
    /// Quantize without dithering.
    None,
    /// Apply a small ordered Bayer matrix.
    Ordered,
    /// Diffuse quantization error to neighboring pixels.
    FloydSteinberg,
}

impl Default for GifOptions {
    fn default() -> Self {
        Self {
            fps: 10.0,
            max_width: 960,
            duration: None,
            looped: true,
            fade: false,
            dither: true,
            dither_mode: None,
            palette_size: 256,
            optimize_frames: false,
            max_bytes: None,
            overlays: Vec::new(),
            captions: Vec::new(),
        }
    }
}

/// Finalized file metadata. GIF duration is calculated from encoded frame delays.
#[derive(Clone, Debug, Serialize)]
pub struct Metadata {
    pub path: PathBuf,
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration: f64,
    pub audio: Option<AudioMetadata>,
    /// Explains omitted source audio when the selected format or policy cannot keep it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_note: Option<String>,
    pub bytes: u64,
    pub frames: Option<u64>,
    /// Encoded codec when the platform reader exposes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// Pixel format returned by the shared decoder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pixel_format: Option<String>,
    /// Approximate keyframe interval in seconds when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyframe_interval: Option<f64>,
    /// Whether decoded frame timestamps are uniformly spaced when measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub constant_frame_rate: Option<bool>,
}

/// The decoded audio format.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct AudioMetadata {
    pub sample_rate: u32,
    pub channels: u32,
}

/// Finds intervals with identical decoded frames and no recorded input activity.
///
/// The returned source-time ranges use half-open `[start, end)` boundaries.
///
/// # Errors
/// Returns an error when the source cannot be decoded or the requested range is invalid.
pub fn idle_ranges(
    path: &Path,
    from: f64,
    to: f64,
    threshold: f64,
    activity_times_ms: &[u64],
) -> Result<Vec<(f64, f64)>> {
    if !from.is_finite()
        || !to.is_finite()
        || !threshold.is_finite()
        || from < 0.0
        || to <= from
        || threshold <= 0.0
    {
        return Err(ExportError(
            "idle detection requires 0 <= from < to and a positive threshold".into(),
        ));
    }

    let mut reader = Reader::open(path)?;
    let duration = seconds(reader.duration.max(0));
    let to = to.min(duration);
    if from >= to {
        return Err(ExportError(
            "idle detection range does not overlap the video".into(),
        ));
    }

    let mut activities = activity_times_ms.to_vec();
    activities.sort_unstable();
    let mut idle = Vec::new();
    let mut previous: Option<VideoFrame> = None;
    let mut stable_start = None;
    let mut stable_end = 0.0;
    while let Some(frame) = reader.next_video()? {
        let at = seconds(frame.time.max(0));
        if at >= to {
            break;
        }
        if let Some(last) = previous.as_ref() {
            if last.width == frame.width && last.height == frame.height && last.bgra == frame.bgra {
                stable_start.get_or_insert(seconds(last.time.max(0)));
                stable_end = at;
            } else if let Some(start) = stable_start.take() {
                collect_idle_range(&mut idle, start, at, from, to, threshold, &activities);
            }
        }
        previous = Some(frame);
    }
    if let Some(start) = stable_start {
        collect_idle_range(
            &mut idle,
            start,
            stable_end.max(duration.min(to)),
            from,
            to,
            threshold,
            &activities,
        );
    }
    Ok(idle)
}

fn collect_idle_range(
    idle: &mut Vec<(f64, f64)>,
    start: f64,
    end: f64,
    from: f64,
    to: f64,
    threshold: f64,
    activity_times_ms: &[u64],
) {
    const ACTIVITY_GRACE_SECONDS: f64 = 0.25;
    let start = start.max(from);
    let end = end.min(to);
    let first = activity_times_ms
        .partition_point(|at_ms| *at_ms as f64 / 1000.0 + ACTIVITY_GRACE_SECONDS <= start);
    let mut cursor = start;
    for at_ms in &activity_times_ms[first..] {
        let at = *at_ms as f64 / 1000.0;
        let excluded_start = at - ACTIVITY_GRACE_SECONDS;
        if excluded_start >= end {
            break;
        }
        if excluded_start - cursor >= threshold {
            idle.push((cursor, excluded_start));
        }
        cursor = cursor.max(at + ACTIVITY_GRACE_SECONDS);
        if cursor >= end {
            return;
        }
    }
    if end - cursor >= threshold {
        idle.push((cursor, end));
    }
}

/// Inspects a finalized video without playing it.
///
/// # Errors
/// Returns an error if the file is missing or the platform decoder cannot open it.
pub fn inspect(path: &Path) -> Result<Metadata> {
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("gif"))
    {
        use image::{AnimationDecoder, ImageDecoder};
        let decoder =
            image::codecs::gif::GifDecoder::new(std::io::BufReader::new(fs::File::open(path)?))
                .map_err(|e| ExportError(e.to_string()))?;
        let (width, height) = decoder.dimensions();
        let (mut frames, mut duration, mut frame_delay, mut constant_delay) =
            (0_u64, 0.0, None::<f64>, true);
        for frame in decoder.into_frames() {
            let frame = frame.map_err(|e| ExportError(e.to_string()))?;
            let (n, d) = frame.delay().numer_denom_ms();
            let delay = n as f64 / d.max(1) as f64;
            duration += delay / 1000.0;
            if frame_delay.is_some_and(|first| (first - delay).abs() > 0.01) {
                constant_delay = false;
            }
            frame_delay.get_or_insert(delay);
            frames += 1;
        }
        return Ok(Metadata {
            path: path.to_path_buf(),
            format: "gif".into(),
            width,
            height,
            fps: if duration > 0.0 {
                frames as f64 / duration
            } else {
                0.0
            },
            duration,
            audio: None,
            audio_note: None,
            bytes: fs::metadata(path)?.len(),
            frames: Some(frames),
            codec: Some("GIF".into()),
            pixel_format: Some("indexed color".into()),
            keyframe_interval: None,
            constant_frame_rate: Some(constant_delay),
        });
    }
    let mut r = Reader::open(path).map_err(|e| ExportError(format!("{}: {e}", path.display())))?;
    let keyframe_interval = if path.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("mp4")
            || extension.eq_ignore_ascii_case("mov")
            || extension.eq_ignore_ascii_case("m4v")
    }) {
        match super::mp4::keyframe_interval(path) {
            Ok(interval) => interval,
            Err(error) => {
                eprintln!(
                    "jiffshot: keyframe metadata unavailable for {}: {error}",
                    path.display()
                );
                None
            }
        }
    } else {
        None
    };
    Ok(Metadata {
        path: path.to_path_buf(),
        format: path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_else(|| "video".into()),
        width: r.width,
        height: r.height,
        fps: r.fps,
        duration: seconds(r.duration),
        audio: r.audio.map(|a| AudioMetadata {
            sample_rate: a.sample_rate,
            channels: a.channels,
        }),
        audio_note: r
            .audio_error()
            .map(|error| format!("source audio cannot be decoded: {error}")),
        bytes: fs::metadata(path)?.len(),
        frames: None,
        codec: r.codec.take(),
        pixel_format: Some("BGRA".into()),
        keyframe_interval,
        constant_frame_rate: None,
    })
}

/// Inspects video and measures frame count and timing by decoding the primary video stream.
///
/// Unlike [`verify`], this does not decode the audio stream.
///
/// # Errors
/// Returns an error if inspection or video decoding fails.
pub fn inspect_with_frame_timing(path: &Path) -> Result<Metadata> {
    let mut metadata = inspect(path)?;
    if metadata.format != "gif" {
        let mut reader = Reader::open(path)?;
        apply_frame_timing(&mut metadata, &mut reader)?;
    }
    Ok(metadata)
}

/// Decodes the primary video and audio streams and returns the verified frame count.
///
/// # Errors
/// Returns an error when the container cannot be opened or either stream fails to decode.
pub fn verify(path: &Path) -> Result<Metadata> {
    let mut metadata = inspect(path)?;
    if metadata.format == "gif" {
        if metadata.frames == Some(0) {
            return Err(ExportError("video contains no frames".into()));
        }
        return Ok(metadata);
    }
    let mut reader = Reader::open(path)?;
    if let Some(error) = reader.audio_error() {
        return Err(ExportError(format!("audio cannot be decoded: {error}")));
    }
    let has_audio = reader.audio.is_some();
    apply_frame_timing(&mut metadata, &mut reader)?;
    drop(reader);
    if has_audio {
        let mut audio = Reader::open(path)?;
        audio.select_video(false)?;
        audio.select_audio(true)?;
        verify_audio_blocks(|| audio.next_audio())?;
    }
    Ok(metadata)
}

fn apply_frame_timing(metadata: &mut Metadata, reader: &mut Reader) -> Result<()> {
    let (frames, constant_frame_rate) = decoded_frame_timing(
        || {
            reader
                .next_video()
                .map(|frame| frame.map(|frame| frame.time))
        },
        metadata.fps,
    )?;
    metadata.frames = Some(frames);
    metadata.constant_frame_rate = Some(constant_frame_rate);
    Ok(())
}

fn decoded_frame_timing(
    mut next: impl FnMut() -> super::Result<Option<i64>>,
    fps: f64,
) -> Result<(u64, bool)> {
    let mut frames = 0_u64;
    let mut previous_time: Option<i64> = None;
    let frame_interval = HNS_PER_SECOND as f64 / fps;
    let mut constant_frame_rate = frame_interval.is_finite() && frame_interval > 0.0;
    while let Some(time) =
        next().map_err(|error| ExportError(format!("frame {frames}: {error}")))?
    {
        if let Some(previous) = previous_time {
            if time.checked_sub(previous).is_none_or(|interval| {
                (interval as f64 - frame_interval).abs() > (frame_interval * 0.01).max(10_000.0)
            }) {
                constant_frame_rate = false;
            }
        }
        previous_time = Some(time);
        frames = frames
            .checked_add(1)
            .ok_or_else(|| ExportError("video contains too many frames to count".into()))?;
    }
    if frames == 0 {
        return Err(ExportError("video contains no frames".into()));
    }
    Ok((frames, constant_frame_rate))
}

fn verify_audio_blocks(
    mut next: impl FnMut() -> super::Result<Option<(i64, Vec<i16>)>>,
) -> Result<()> {
    let mut blocks = 0_u64;
    loop {
        match next().map_err(|error| ExportError(format!("audio block {blocks}: {error}")))? {
            Some(_) => {
                blocks = blocks.checked_add(1).ok_or_else(|| {
                    ExportError("audio contains too many blocks to verify".into())
                })?;
            }
            None => return Ok(()),
        }
    }
}

/// Converts decoded BGRA into an opaque image.
pub fn to_pixmap(frame: &VideoFrame) -> Option<Pixmap> {
    let expected = (frame.width as usize)
        .checked_mul(frame.height as usize)?
        .checked_mul(4)?;
    if frame.bgra.len() != expected {
        return None;
    }
    let mut rgba = Vec::with_capacity(expected);
    for p in frame.bgra.as_chunks::<4>().0 {
        rgba.extend_from_slice(&[p[2], p[1], p[0], 255]);
    }
    Pixmap::from_vec(
        rgba,
        tiny_skia::IntSize::from_wh(frame.width, frame.height)?,
    )
}

fn seconds(time: i64) -> f64 {
    time as f64 / HNS_PER_SECOND as f64
}
fn hns(seconds: f64) -> i64 {
    (seconds * HNS_PER_SECOND as f64).round() as i64
}

fn validate_range(from: f64, to: f64, duration: f64) -> Result<()> {
    if !from.is_finite()
        || !to.is_finite()
        || from < 0.0
        || to <= from
        || to > duration + 0.000_000_1
    {
        return Err(ExportError(format!(
            "range must satisfy 0 <= from < to <= duration ({duration}), not {from}..{to}"
        )));
    }
    if hns(to) <= hns(from) {
        return Err(ExportError(
            "range is shorter than the media time resolution".into(),
        ));
    }
    Ok(())
}

/// Decodes a still at a bounded source time.
///
/// # Errors
/// Rejects non-finite/out-of-bounds times and files with no frame at the requested time.
pub fn frame(path: &Path, at: f64) -> Result<(Pixmap, f64)> {
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("gif"))
    {
        use image::AnimationDecoder;
        let metadata = inspect(path)?;
        if !at.is_finite() || at < 0.0 || at >= metadata.duration {
            return Err(ExportError(format!(
                "at must satisfy 0 <= at < duration ({})",
                metadata.duration
            )));
        }
        let decoder =
            image::codecs::gif::GifDecoder::new(std::io::BufReader::new(fs::File::open(path)?))
                .map_err(|e| ExportError(e.to_string()))?;
        let mut time = 0.0;
        for decoded in decoder.into_frames() {
            let decoded = decoded.map_err(|e| ExportError(e.to_string()))?;
            let (n, d) = decoded.delay().numer_denom_ms();
            let end = time + n as f64 / d.max(1) as f64 / 1000.0;
            if at < end {
                return Ok((
                    jiffshot_doc::io::from_rgba_image(decoded.buffer())
                        .ok_or_else(|| ExportError("invalid GIF frame".into()))?,
                    time,
                ));
            }
            time = end;
        }
        return Err(ExportError("no GIF frame at requested time".into()));
    }
    let mut r = Reader::open(path)?;
    if !at.is_finite() || at < 0.0 || at >= seconds(r.duration) {
        return Err(ExportError(format!(
            "at must satisfy 0 <= at < duration ({})",
            seconds(r.duration)
        )));
    }
    let f = r
        .frame_at(hns(at))?
        .ok_or_else(|| ExportError("no frame at requested time".into()))?;
    let pm = to_pixmap(&f).ok_or_else(|| ExportError("invalid decoded frame dimensions".into()))?;
    Ok((pm, seconds(f.time)))
}

/// Saves a still without exposing partially written output.
///
/// # Errors
/// Rejects source overwrites, unsupported extensions and failed PNG writes.
pub fn save_frame(source: &Path, out: &Path, at: f64) -> Result<(u32, u32, f64, u64)> {
    let (pm, time) = frame(source, at)?;
    let output = Output::new(out, &[source], "png")?;
    pm.save_png(&output.staging)
        .map_err(|e| ExportError(e.to_string()))?;
    let bytes = fs::metadata(&output.staging)?.len();
    output.publish()?;
    Ok((pm.width(), pm.height(), time, bytes))
}

/// Placement of a source frame on an export canvas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FramePlacement {
    /// Horizontal offset of the source within the canvas.
    pub x: u32,
    /// Vertical offset of the source within the canvas.
    pub y: u32,
    /// Width of the scaled source, before any edge cropping.
    pub width: u32,
    /// Height of the scaled source, before any edge cropping.
    pub height: u32,
    /// Whether the source is copied without scaling, possibly cropping one edge pixel.
    pub unscaled: bool,
}

fn centered_placement(source: (u32, u32), canvas: (u32, u32)) -> Option<FramePlacement> {
    let ((width, height), (w, h)) = (source, canvas);
    if width == 0 || height == 0 || w == 0 || h == 0 {
        return None;
    }
    let scale = (w as f64 / width as f64).min(h as f64 / height as f64);
    let sw = ((width as f64 * scale).round() as u32).clamp(1, w);
    let sh = ((height as f64 * scale).round() as u32).clamp(1, h);
    Some(FramePlacement {
        x: (w - sw) / 2,
        y: (h - sh) / 2,
        width: sw,
        height: sh,
        unscaled: false,
    })
}

/// Returns the resize and centering used when a video frame is exported.
pub fn frame_placement(source: (u32, u32), canvas: (u32, u32)) -> Option<FramePlacement> {
    let ((width, height), (w, h)) = (source, canvas);
    if width == 0 || height == 0 || w == 0 || h == 0 {
        return None;
    }
    if width >= w && height >= h && width - w < 2 && height - h < 2 {
        return Some(FramePlacement {
            x: 0,
            y: 0,
            width,
            height,
            unscaled: true,
        });
    }
    centered_placement(source, canvas)
}

/// Fits an image into a BGRA canvas, centered over black.
pub fn paste_fitted(pm: &Pixmap, out: &mut [u8], w: usize, h: usize) {
    if w == 0 || h == 0 || out.len() != w.saturating_mul(h).saturating_mul(4) {
        return;
    }
    let Some(placement) = centered_placement((pm.width(), pm.height()), (w as u32, h as u32))
    else {
        return;
    };
    let (sw, sh) = (placement.width, placement.height);
    let small = if (sw, sh) == (pm.width(), pm.height()) {
        Some(pm.clone())
    } else {
        raster::resample(pm, sw, sh)
    };
    let Some(small) = small else { return };
    let (x0, y0) = (placement.x as usize, placement.y as usize);
    for (i, p) in small.pixels().iter().enumerate() {
        let d = ((y0 + i / sw as usize) * w + x0 + i % sw as usize) * 4;
        out[d..d + 4].copy_from_slice(&[p.blue(), p.green(), p.red(), 0]);
    }
}

fn fit_frame(frame: &VideoFrame, w: usize, h: usize) -> Result<Vec<u8>> {
    let pm = to_pixmap(frame).ok_or_else(|| ExportError("invalid decoded frame".into()))?;
    let mut out = vec![0; w * h * 4];
    let fw = frame.width as usize;
    let placement = frame_placement((frame.width, frame.height), (w as u32, h as u32))
        .ok_or_else(|| ExportError("invalid output frame dimensions".into()))?;
    if placement.unscaled {
        for y in 0..h {
            out[y * w * 4..(y + 1) * w * 4]
                .copy_from_slice(&frame.bgra[y * fw * 4..y * fw * 4 + w * 4]);
        }
    } else {
        paste_fitted(&pm, &mut out, w, h);
    }
    Ok(out)
}

fn spec(first: &Metadata, options: &Mp4Options) -> Result<VideoSpec> {
    let (w, h) = match (options.width, options.height) {
        (None, None) => (first.width, first.height),
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => (
            w,
            (first.height as f64 * w as f64 / first.width as f64).round() as u32,
        ),
        (None, Some(h)) => (
            (first.width as f64 * h as f64 / first.height as f64).round() as u32,
            h,
        ),
    };
    let fps = options
        .fps
        .unwrap_or(first.fps.round().clamp(1.0, 120.0) as u32);
    if !(2..=8192).contains(&w)
        || !(2..=8192).contains(&h)
        || !(1..=120).contains(&fps)
        || w as u64 * h as u64 > 33_177_600
    {
        return Err(ExportError(
            "dimensions must be 2..8192 (at most 33,177,600 pixels) and fps 1..120".into(),
        ));
    }
    Ok(VideoSpec::for_size(w, h, fps))
}

fn compatible(audio: Option<AudioMetadata>) -> Option<AudioSpec> {
    audio
        .filter(|a| matches!(a.sample_rate, 44_100 | 48_000) && (1..=2).contains(&a.channels))
        .map(|a| AudioSpec {
            sample_rate: a.sample_rate,
            channels: a.channels,
        })
}

fn choose_audio(
    sources: &[Metadata],
    policy: Audio,
) -> Result<(Option<AudioSpec>, Option<String>)> {
    if policy == Audio::Mute
        || sources
            .iter()
            .all(|m| m.audio.is_none() && m.audio_note.is_none())
    {
        return Ok((None, None));
    }
    let first = sources.first().and_then(|m| compatible(m.audio));
    if first.is_some()
        && sources
            .iter()
            .all(|m| compatible(m.audio) == first && m.audio_note.is_none())
    {
        return Ok((first, None));
    }
    let message = "source audio formats differ, a source has no audio, or audio is unsupported; keeping audio requires matching 44100/48000 Hz mono/stereo in every source; pass --mute to explicitly discard audio";
    if policy == Audio::RequireCompatible {
        return Err(ExportError(message.into()));
    }
    Ok((None, Some(message.into())))
}

/// Re-encodes trimmed pieces end-to-end, fitting later sources to the first one's canvas.
///
/// # Errors
/// Validates every source and range before creating output; never overwrites any source.
/// Failed exports remove their staging file and leave any previous output untouched.
pub fn export_mp4(
    segments: &[Segment],
    out: &Path,
    options: Mp4Options,
    progress: impl Fn(f64),
) -> Result<Metadata> {
    export_mp4_cancellable(segments, out, options, progress, || false)
}

/// Like [`export_mp4`], but abandons staging if cancellation is requested.
pub fn export_mp4_cancellable(
    segments: &[Segment],
    out: &Path,
    options: Mp4Options,
    progress: impl Fn(f64),
    cancelled: impl Fn() -> bool,
) -> Result<Metadata> {
    check_cancelled(&cancelled)?;
    if segments.is_empty() || segments.len() > 256 {
        return Err(ExportError("provide 1..256 segments".into()));
    }
    let sources: Vec<Metadata> = segments
        .iter()
        .map(|segment| {
            check_cancelled(&cancelled)?;
            let m = inspect(&segment.path)?;
            validate_range(segment.from, segment.to, m.duration)?;
            if !segment.speed.is_finite() || !(0.01..=100.0).contains(&segment.speed) {
                return Err(ExportError(format!(
                    "segment speed must be finite and in 0.01..=100, not {}",
                    segment.speed
                )));
            }
            Ok(m)
        })
        .collect::<Result<_>>()?;
    let spec = spec(&sources[0], &options)?;
    let transition_frames = match options.transition_seconds {
        Some(seconds)
            if seconds.is_finite() && (0.0..=10.0).contains(&seconds) && seconds > 0.0 =>
        {
            let frames = (seconds * f64::from(spec.fps)).round();
            if frames < 1.0 {
                return Err(ExportError(
                    "transition must span at least one output frame".into(),
                ));
            }
            if segments.len() < 2 {
                return Err(ExportError(
                    "transitions require at least two segments".into(),
                ));
            }
            frames as u64
        }
        Some(_) => {
            return Err(ExportError(
                "transition must be finite and in (0, 10] seconds".into(),
            ));
        }
        None => 0,
    };
    let boundaries = match options.transition_boundaries.as_deref() {
        Some(boundaries) if boundaries.len() != segments.len() - 1 => {
            return Err(ExportError(
                "transition boundaries must match the number of segment joins".into(),
            ));
        }
        Some(boundaries) => boundaries
            .iter()
            .map(|enabled| if *enabled { transition_frames } else { 0 })
            .collect::<Vec<_>>(),
        None => vec![transition_frames; segments.len() - 1],
    };
    if transition_frames > 0 && !boundaries.iter().any(|frames| *frames > 0) {
        return Err(ExportError(
            "transitions require at least two joined source segments".into(),
        ));
    }
    let (audio, audio_note) = choose_audio(&sources, options.audio)?;
    let natural_durations: Vec<f64> = segments
        .iter()
        .map(|segment| (segment.to - segment.from) / segment.speed)
        .collect();
    let natural_total: f64 = natural_durations.iter().sum();
    let overlap_frames = boundaries.iter().try_fold(0_u64, |total, frames| {
        total
            .checked_add(*frames)
            .ok_or_else(|| ExportError("transition duration is too large".into()))
    })?;
    let total_frames = match options.duration {
        Some(duration) if duration.is_finite() && duration > 0.0 => {
            let frames = (duration * f64::from(spec.fps)).round();
            if !frames.is_finite() || frames < 1.0 || frames > max_frame_count(spec.fps) as f64 {
                return Err(ExportError(
                    "duration must produce at least one representable output frame".into(),
                ));
            }
            frames as u64
        }
        Some(_) => {
            return Err(ExportError(
                "duration must be a finite positive number of seconds".into(),
            ));
        }
        None => {
            let frames = (natural_total * f64::from(spec.fps)).ceil();
            if !frames.is_finite() || frames < 1.0 || frames > max_frame_count(spec.fps) as f64 {
                return Err(ExportError("video duration is too large to export".into()));
            }
            (frames as u64).checked_sub(overlap_frames).ok_or_else(|| {
                ExportError("transition duration is too long for the joined video".into())
            })?
        }
    };
    let rendered_frames = total_frames
        .checked_add(overlap_frames)
        .filter(|frames| *frames <= max_frame_count(spec.fps))
        .ok_or_else(|| ExportError("video duration is too large to export".into()))?;
    let frame_counts = allocate_frames(rendered_frames, &natural_durations)?;
    for (index, frames) in frame_counts.iter().enumerate() {
        let incoming = index.checked_sub(1).map_or(0, |i| boundaries[i]);
        let outgoing = boundaries.get(index).copied().unwrap_or(0);
        let minimum = incoming
            .checked_add(outgoing)
            .ok_or_else(|| ExportError("transition duration is too large".into()))?
            .max(1);
        if *frames < minimum {
            return Err(ExportError(
                "transition duration is too long for one or more segments".into(),
            ));
        }
    }
    let output_duration = total_frames as f64 / f64::from(spec.fps);
    let captions = map_mp4_captions(
        &options.captions,
        segments,
        &frame_counts,
        spec.fps,
        &boundaries,
    )?;
    let overlays = load_overlays(
        &options.overlays,
        &captions,
        output_duration,
        spec.width,
        spec.height,
    )?;
    let paths: Vec<&Path> = segments
        .iter()
        .map(|s| s.path.as_path())
        .chain(options.overlays.iter().map(|o| o.path.as_path()))
        .collect();
    let output = Output::new(out, &paths, "mp4")?;
    let mut writer = Writer::create(&output.staging, spec, audio).map_err(|e| {
        ExportError(format!(
            "cannot encode {}x{} at {} fps: {e}",
            spec.width, spec.height, spec.fps
        ))
    })?;
    let mut frame_count = 0_u64;
    let mut previous_tail: Option<(fs::File, TempOutput)> = None;
    let mut previous_audio_tail: Option<Vec<i16>> = None;
    for (segment_index, (segment, segment_frames)) in segments.iter().zip(frame_counts).enumerate()
    {
        check_cancelled(&cancelled)?;
        let segment_output_start = frame_count;
        let incoming = segment_index
            .checked_sub(1)
            .map_or(0, |index| boundaries[index]);
        let outgoing = boundaries.get(segment_index).copied().unwrap_or(0);
        // Keep raw tails on disk; full-resolution transition frames can exceed memory.
        let mut next_tail = if outgoing > 0 {
            let spool = TempOutput::reserve("jiffshot-transition", "bgra")?;
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&spool.0)?;
            Some((file, spool))
        } else {
            None
        };
        let (start, end) = (hns(segment.from), hns(segment.to));
        let mut video = Reader::open(&segment.path)?;
        video.seek(start)?;
        let mut current = video
            .next_video()?
            .ok_or_else(|| ExportError("segment has no frames".into()))?;
        let mut next = video.next_video()?;
        let mut sound = if audio.is_some() {
            let mut r = Reader::open(&segment.path)?;
            r.select_video(false)?;
            r.select_audio(true)?;
            r.seek(start)?;
            Some(r)
        } else {
            None
        };
        let mut pending = match &mut sound {
            Some(r) => r.next_audio()?,
            None => None,
        };
        for n in 0..segment_frames {
            check_cancelled(&cancelled)?;
            let source_at =
                start + ((end - start) as u128 * u128::from(n) / u128::from(segment_frames)) as i64;
            while next.as_ref().is_some_and(|f| f.time <= source_at) {
                check_cancelled(&cancelled)?;
                current = next
                    .take()
                    .ok_or_else(|| ExportError("missing pending frame".into()))?;
                next = video.next_video()?;
            }
            let output_frame = segment_output_start + n;
            let at = frame_time(output_frame, spec.fps);
            let finish = frame_time(output_frame + 1, spec.fps);
            let mut pixels = fit_frame(&current, spec.width as usize, spec.height as usize)?;
            composite_bgra(
                &mut pixels,
                spec.width,
                spec.height,
                &overlays,
                seconds(at),
                Some(segment_index),
            );
            if incoming > 0 && n < incoming {
                let (spool, _) = previous_tail
                    .as_mut()
                    .ok_or_else(|| ExportError("missing outgoing transition frames".into()))?;
                let mut outgoing = vec![0; pixels.len()];
                spool.read_exact(&mut outgoing)?;
                blend_bgra(&mut pixels, &outgoing, n + 1, incoming);
            }
            let is_outgoing_tail = outgoing > 0 && n >= segment_frames - outgoing;
            if is_outgoing_tail {
                let (spool, _) = next_tail
                    .as_mut()
                    .ok_or_else(|| ExportError("missing transition frame staging".into()))?;
                spool.write_all(&pixels)?;
            } else {
                composite_bgra(
                    &mut pixels,
                    spec.width,
                    spec.height,
                    &overlays,
                    seconds(at),
                    None,
                );
                writer.write_frame(&pixels, at, finish - at)?;
                frame_count += 1;
                progress((frame_count as f64 / total_frames as f64).clamp(0.0, 1.0));
            }
        }
        if let Some((spool, _)) = &mut next_tail {
            spool.flush()?;
            spool.seek(SeekFrom::Start(0))?;
        }
        if let (Some(audio), Some(reader)) = (audio, &mut sound) {
            write_segment_audio(
                &mut writer,
                reader,
                &mut pending,
                audio,
                SegmentAudioTiming {
                    source_start: start,
                    source_end: end,
                    output_start_frame: segment_output_start,
                    output_frames: segment_frames,
                    fps: spec.fps,
                    incoming_frames: incoming,
                    outgoing_frames: outgoing,
                },
                &mut previous_audio_tail,
                &cancelled,
            )?;
        }
        previous_tail = next_tail;
    }
    if frame_count != total_frames {
        return Err(ExportError(format!(
            "transition timeline produced {frame_count} frames instead of {total_frames}"
        )));
    }
    check_cancelled(&cancelled)?;
    writer.finish()?;
    check_cancelled(&cancelled)?;
    let mut metadata = inspect(&output.staging)?;
    metadata.path = out.to_path_buf();
    metadata.frames = Some(frame_count);
    metadata.audio_note = audio_note;
    check_cancelled(&cancelled)?;
    output.publish()?;
    progress(1.0);
    Ok(metadata)
}

fn max_frame_count(fps: u32) -> u64 {
    (i64::MAX as u128 * u128::from(fps) / HNS_PER_SECOND as u128) as u64
}

fn frame_time(frame: u64, fps: u32) -> i64 {
    (u128::from(frame) * HNS_PER_SECOND as u128 / u128::from(fps)) as i64
}

fn allocate_frames(total: u64, durations: &[f64]) -> Result<Vec<u64>> {
    if durations.is_empty() || total < durations.len() as u64 {
        return Err(ExportError(
            "target duration must allow at least one frame per segment".into(),
        ));
    }
    let total_duration: f64 = durations.iter().sum();
    if !total_duration.is_finite() || total_duration <= 0.0 {
        return Err(ExportError(
            "segment durations must be finite and positive".into(),
        ));
    }
    let mut frames = Vec::with_capacity(durations.len());
    let mut fractions = Vec::with_capacity(durations.len());
    for duration in durations {
        if !duration.is_finite() || *duration <= 0.0 {
            return Err(ExportError(
                "segment durations must be finite and positive".into(),
            ));
        }
        let exact = total as f64 * duration / total_duration;
        let floor = exact.floor() as u64;
        frames.push(floor.max(1));
        fractions.push(exact - exact.floor());
    }
    let mut assigned: u64 = frames.iter().sum();
    while assigned > total {
        let index = frames
            .iter()
            .enumerate()
            .filter(|(_, count)| **count > 1)
            .min_by(|(left, _), (right, _)| fractions[*left].total_cmp(&fractions[*right]))
            .map(|(index, _)| index)
            .ok_or_else(|| ExportError("target duration is too short for these segments".into()))?;
        frames[index] -= 1;
        assigned -= 1;
    }
    while assigned < total {
        let index = fractions
            .iter()
            .enumerate()
            .max_by(|(left, a), (right, b)| a.total_cmp(b).then_with(|| right.cmp(left)))
            .map(|(index, _)| index)
            .ok_or_else(|| ExportError("cannot allocate output frames".into()))?;
        frames[index] += 1;
        fractions[index] = 0.0;
        assigned += 1;
    }
    Ok(frames)
}

struct SegmentAudioTiming {
    source_start: i64,
    source_end: i64,
    output_start_frame: u64,
    output_frames: u64,
    fps: u32,
    incoming_frames: u64,
    outgoing_frames: u64,
}

fn write_segment_audio(
    writer: &mut Writer,
    reader: &mut Reader,
    pending: &mut Option<(i64, Vec<i16>)>,
    audio: AudioSpec,
    timing: SegmentAudioTiming,
    previous_tail: &mut Option<Vec<i16>>,
    cancelled: &impl Fn() -> bool,
) -> Result<()> {
    let rate = i64::from(audio.sample_rate);
    let first_sample = sample_index_at_frame(timing.output_start_frame, timing.fps, rate);
    let end_frame = timing.output_start_frame + timing.output_frames;
    let end_sample = sample_index_at_frame(end_frame, timing.fps, rate);
    let incoming_end = sample_index_at_frame(
        timing.output_start_frame + timing.incoming_frames,
        timing.fps,
        rate,
    );
    let tail_start = if timing.outgoing_frames > 0 {
        sample_index_at_frame(end_frame - timing.outgoing_frames, timing.fps, rate)
    } else {
        end_sample
    };

    if timing.incoming_frames > 0 {
        let outgoing = previous_tail
            .take()
            .ok_or_else(|| ExportError("missing outgoing audio transition".into()))?;
        let mut incoming = read_segment_audio_range(
            reader,
            pending,
            audio,
            &timing,
            first_sample..incoming_end,
            cancelled,
        )?;
        if incoming.len() != outgoing.len() {
            return Err(ExportError(
                "audio transition sample counts do not match".into(),
            ));
        }
        blend_audio(&mut incoming, &outgoing, audio.channels as usize);
        write_audio_samples(writer, audio, first_sample, &incoming, cancelled)?;
    }

    let middle_start = if timing.incoming_frames > 0 {
        incoming_end
    } else {
        first_sample
    };
    write_segment_audio_range(
        writer,
        reader,
        pending,
        audio,
        &timing,
        middle_start..tail_start,
        cancelled,
    )?;

    if timing.outgoing_frames > 0 {
        *previous_tail = Some(read_segment_audio_range(
            reader,
            pending,
            audio,
            &timing,
            tail_start..end_sample,
            cancelled,
        )?);
    } else {
        write_segment_audio_range(
            writer,
            reader,
            pending,
            audio,
            &timing,
            tail_start..end_sample,
            cancelled,
        )?;
    }
    Ok(())
}

fn sample_index_at_frame(frame: u64, fps: u32, sample_rate: i64) -> i64 {
    (i128::from(frame_time(frame, fps)) * i128::from(sample_rate) / i128::from(HNS_PER_SECOND))
        as i64
}

fn read_segment_audio_range(
    reader: &mut Reader,
    pending: &mut Option<(i64, Vec<i16>)>,
    audio: AudioSpec,
    timing: &SegmentAudioTiming,
    samples: std::ops::Range<i64>,
    cancelled: &impl Fn() -> bool,
) -> Result<Vec<i16>> {
    let channels = audio.channels as usize;
    let frame_count = usize::try_from(samples.end.saturating_sub(samples.start))
        .map_err(|_| ExportError("audio transition is too large".into()))?;
    let mut output = Vec::with_capacity(frame_count.saturating_mul(channels));
    let mut batch_start = samples.start;
    while batch_start < samples.end {
        check_cancelled(cancelled)?;
        let batch_end = (batch_start + 1024).min(samples.end);
        output.extend(render_segment_audio_batch(
            reader,
            pending,
            audio,
            timing,
            batch_start,
            batch_end,
        )?);
        batch_start = batch_end;
    }
    Ok(output)
}

fn write_segment_audio_range(
    writer: &mut Writer,
    reader: &mut Reader,
    pending: &mut Option<(i64, Vec<i16>)>,
    audio: AudioSpec,
    timing: &SegmentAudioTiming,
    samples: std::ops::Range<i64>,
    cancelled: &impl Fn() -> bool,
) -> Result<()> {
    let mut batch_start = samples.start;
    while batch_start < samples.end {
        check_cancelled(cancelled)?;
        let batch_end = (batch_start + 1024).min(samples.end);
        let samples =
            render_segment_audio_batch(reader, pending, audio, timing, batch_start, batch_end)?;
        write_audio_samples(writer, audio, batch_start, &samples, cancelled)?;
        batch_start = batch_end;
    }
    Ok(())
}

fn render_segment_audio_batch(
    reader: &mut Reader,
    pending: &mut Option<(i64, Vec<i16>)>,
    audio: AudioSpec,
    timing: &SegmentAudioTiming,
    first_sample: i64,
    end_sample: i64,
) -> Result<Vec<i16>> {
    let channels = audio.channels as usize;
    let output_start = frame_time(timing.output_start_frame, timing.fps);
    let output_end = frame_time(timing.output_start_frame + timing.output_frames, timing.fps);
    let output_duration = output_end - output_start;
    let rate = i64::from(audio.sample_rate);
    let mut samples = Vec::with_capacity((end_sample - first_sample) as usize * channels);
    for sample_index in first_sample..end_sample {
        let output_at =
            (i128::from(sample_index) * i128::from(HNS_PER_SECOND) / i128::from(rate)) as i64;
        let offset = output_at.saturating_sub(output_start);
        let source_at = timing.source_start
            + ((i128::from(offset) * i128::from(timing.source_end - timing.source_start))
                / i128::from(output_duration)) as i64;
        if let Some(sample) = audio_sample_at(reader, pending, source_at, audio)? {
            samples.extend_from_slice(&sample[..channels]);
        } else {
            samples.resize(samples.len() + channels, 0);
        }
    }
    Ok(samples)
}

fn write_audio_samples(
    writer: &mut Writer,
    audio: AudioSpec,
    first_sample: i64,
    samples: &[i16],
    cancelled: &impl Fn() -> bool,
) -> Result<()> {
    let channels = audio.channels as usize;
    for (batch_index, batch) in samples.chunks(1024 * channels).enumerate() {
        check_cancelled(cancelled)?;
        let batch_first = first_sample + (batch_index * 1024) as i64;
        let sample_count = batch.len() / channels;
        let at = (i128::from(batch_first) * i128::from(HNS_PER_SECOND)
            / i128::from(audio.sample_rate)) as i64;
        let duration = (sample_count as i128 * i128::from(HNS_PER_SECOND)
            / i128::from(audio.sample_rate)) as i64;
        writer.write_audio(batch, at, duration)?;
    }
    Ok(())
}

fn blend_audio(incoming: &mut [i16], outgoing: &[i16], channels: usize) {
    let frames = incoming.len() / channels;
    for (frame, (incoming, outgoing)) in incoming
        .chunks_exact_mut(channels)
        .zip(outgoing.chunks_exact(channels))
        .enumerate()
    {
        let incoming_weight = (frame + 1) as i64;
        let outgoing_weight = frames as i64 - incoming_weight;
        for (incoming, outgoing) in incoming.iter_mut().zip(outgoing) {
            *incoming = ((i64::from(*outgoing) * outgoing_weight
                + i64::from(*incoming) * incoming_weight)
                / frames as i64) as i16;
        }
    }
}

fn audio_sample_at(
    reader: &mut Reader,
    pending: &mut Option<(i64, Vec<i16>)>,
    at: i64,
    audio: AudioSpec,
) -> Result<Option<[i16; 2]>> {
    let channels = audio.channels as usize;
    loop {
        if pending.is_none() {
            *pending = reader.next_audio()?;
        }
        let Some((time, sample_count)) = pending
            .as_ref()
            .map(|(time, samples)| (*time, samples.len()))
        else {
            return Ok(None);
        };
        let frame_count = sample_count / channels;
        if frame_count == 0 {
            *pending = reader.next_audio()?;
            continue;
        }
        let end = time + frame_count as i64 * HNS_PER_SECOND / i64::from(audio.sample_rate);
        if at < time {
            return Ok(None);
        }
        if at >= end {
            *pending = reader.next_audio()?;
            continue;
        }
        let scaled = i128::from(at - time) * i128::from(audio.sample_rate);
        let index = (scaled / i128::from(HNS_PER_SECOND)) as usize;
        let fraction = (scaled % i128::from(HNS_PER_SECOND)) as i64;
        let index = index.min(frame_count - 1);
        let next_index = (index + 1).min(frame_count - 1);
        let Some((_, samples)) = pending.as_ref() else {
            return Ok(None);
        };
        let mut output = [0; 2];
        for channel in 0..channels {
            let first = i64::from(samples[index * channels + channel]);
            let next = i64::from(samples[next_index * channels + channel]);
            output[channel] =
                ((first * (HNS_PER_SECOND - fraction) + next * fraction) / HNS_PER_SECOND) as i16;
        }
        return Ok(Some(output));
    }
}

#[cfg(test)]
fn clip_audio(
    time: i64,
    pcm: &[i16],
    audio: AudioSpec,
    start: i64,
    end: i64,
) -> Option<(&[i16], i64, i64)> {
    let (rate, ch) = (audio.sample_rate as i64, audio.channels as usize);
    let frames = pcm.len() / ch;
    let first = (((start - time).max(0) * rate + HNS_PER_SECOND - 1) / HNS_PER_SECOND) as usize;
    let last = (((end - time).max(0) * rate) / HNS_PER_SECOND) as usize;
    let last = last.min(frames);
    if first >= last {
        return None;
    }
    Some((
        &pcm[first * ch..last * ch],
        (time + first as i64 * HNS_PER_SECOND / rate - start).max(0),
        (last - first) as i64 * HNS_PER_SECOND / rate,
    ))
}

fn ordered_dither(rgba: &mut image::RgbaImage) {
    const BAYER: [[i32; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];
    for (x, y, p) in rgba.enumerate_pixels_mut() {
        let d = (BAYER[(y % 4) as usize][(x % 4) as usize] - 8) * 2;
        for c in 0..3 {
            p.0[c] = (p.0[c] as i32 + d).clamp(0, 255) as u8;
        }
    }
}

fn floyd_steinberg_dither(rgba: &mut image::RgbaImage) {
    let (width, height) = rgba.dimensions();
    for y in 0..height {
        for x in 0..width {
            let old = rgba.get_pixel(x, y).0;
            let mut new = old;
            for channel in 0..3 {
                let level = ((u16::from(old[channel]) + 16) / 32).min(7);
                new[channel] = (level * 32) as u8;
            }
            rgba.put_pixel(x, y, image::Rgba(new));
            for channel in 0..3 {
                let error = (f32::from(old[channel]) - f32::from(new[channel])) / 16.0;
                for (dx, dy, weight) in
                    [(1_i32, 0_i32, 7.0), (-1, 1, 3.0), (0, 1, 5.0), (1, 1, 1.0)]
                {
                    let nx = x as i32 + dx;
                    let ny = y as i32 + dy;
                    if nx >= 0 && ny >= 0 && nx < width as i32 && ny < height as i32 {
                        let pixel = rgba.get_pixel_mut(nx as u32, ny as u32);
                        pixel.0[channel] = (f32::from(pixel.0[channel]) + error * weight)
                            .round()
                            .clamp(0.0, 255.0) as u8;
                    }
                }
            }
        }
    }
}

fn quantize_palette(rgba: &mut image::RgbaImage, palette_size: u16) {
    let mut bits = [0_u8; 3];
    let mut colors = 1_u16;
    while colors.saturating_mul(2) <= palette_size {
        let channel = (0..3).min_by_key(|index| bits[*index]).unwrap_or(0);
        bits[channel] += 1;
        colors *= 2;
    }
    for pixel in rgba.pixels_mut() {
        for (channel, channel_bits) in bits.into_iter().enumerate() {
            if channel_bits == 0 {
                pixel.0[channel] = 128;
                continue;
            }
            let levels = (1_u16 << channel_bits) - 1;
            let step = 255.0 / f32::from(levels);
            pixel.0[channel] = (((f32::from(pixel.0[channel]) / step).round() * step).round())
                .clamp(0.0, 255.0) as u8;
        }
    }
}

struct LoadedOverlay {
    pm: Pixmap,
    x: i32,
    y: i32,
    from: f64,
    to: f64,
    fade: f64,
    fade_from: f64,
    fade_to: f64,
    segment: Option<usize>,
}

/// Maps source captions onto a previously exported, non-transitioned MP4 timeline.
///
/// The returned source captions can be applied when that MP4 is converted to GIF.
///
/// # Errors
/// Returns an error if the frame allocation or any source-caption mapping is invalid.
pub fn map_captions_for_gif(
    captions: &[Caption],
    segments: &[Segment],
    total_frames: u64,
    fps: u32,
) -> Result<Vec<Caption>> {
    if !(1..=120).contains(&fps) || total_frames > max_frame_count(fps) {
        return Err(ExportError("invalid video frame allocation".into()));
    }
    if captions
        .iter()
        .any(|caption| caption.timeline != CaptionTimeline::Source)
    {
        return Err(ExportError(
            "GIF caption mapping requires source-timeline captions".into(),
        ));
    }
    let durations: Vec<f64> = segments
        .iter()
        .map(|segment| (segment.to - segment.from) / segment.speed)
        .collect();
    let frame_counts = allocate_frames(total_frames, &durations)?;
    let boundaries = vec![0; segments.len().saturating_sub(1)];
    let mut mapped = map_mp4_captions(captions, segments, &frame_counts, fps, &boundaries)?;
    for caption in &mut mapped {
        caption.timeline = CaptionTimeline::Source;
        caption.segment = Some(0);
    }
    Ok(mapped)
}

fn caption_fade_window(caption: &Caption) -> Result<(f64, f64)> {
    let (from, to) = caption.fade_window.unwrap_or((caption.from, caption.to));
    if !from.is_finite()
        || !to.is_finite()
        || to <= from
        || !caption.fade.is_finite()
        || caption.fade < 0.0
        || caption.fade * 2.0 > to - from + 1e-9
    {
        return Err(ExportError(
            "caption text, size, padding, or fade is outside the supported range".into(),
        ));
    }
    Ok((from, to))
}

fn map_mp4_captions(
    captions: &[Caption],
    segments: &[Segment],
    frame_counts: &[u64],
    fps: u32,
    boundaries: &[u64],
) -> Result<Vec<Caption>> {
    if frame_counts.len() != segments.len()
        || boundaries.len() != segments.len().saturating_sub(1)
        || fps == 0
    {
        return Err(ExportError("invalid caption timeline allocation".into()));
    }
    let mut mapped = Vec::with_capacity(captions.len());
    let mut output_frame = 0_u64;
    let mut output_starts = Vec::with_capacity(frame_counts.len());
    let mut output_durations = Vec::with_capacity(frame_counts.len());
    for (index, frames) in frame_counts.iter().enumerate() {
        let start = seconds(frame_time(output_frame, fps));
        output_frame = output_frame
            .checked_add(*frames)
            .ok_or_else(|| ExportError("invalid caption timeline allocation".into()))?;
        output_starts.push(start);
        output_durations.push(seconds(frame_time(output_frame, fps)) - start);
        if let Some(overlap) = boundaries.get(index) {
            output_frame = output_frame
                .checked_sub(*overlap)
                .ok_or_else(|| ExportError("invalid caption transition overlap".into()))?;
        }
    }
    for caption in captions {
        let (fade_from, fade_to) = caption_fade_window(caption)?;
        if caption.timeline == CaptionTimeline::Output {
            mapped.push(caption.clone());
            continue;
        }
        let index = caption.segment.ok_or_else(|| {
            ExportError("source-timeline captions require a valid segment index".into())
        })?;
        let segment = segments.get(index).ok_or_else(|| {
            ExportError("source-timeline captions require a valid segment index".into())
        })?;
        let from = caption.from.max(segment.from);
        let to = caption.to.min(segment.to);
        if from >= to {
            continue;
        }
        let output_duration = output_durations[index];
        let source_duration = segment.to - segment.from;
        let scale = output_duration / source_duration;
        let mut caption = caption.clone();
        caption.from = output_starts[index] + (from - segment.from) * scale;
        caption.to = output_starts[index] + (to - segment.from) * scale;
        caption.fade *= scale;
        caption.fade_window = Some((
            output_starts[index] + (fade_from - segment.from) * scale,
            output_starts[index] + (fade_to - segment.from) * scale,
        ));
        caption.timeline = CaptionTimeline::Output;
        caption.segment = Some(index);
        mapped.push(caption);
    }
    Ok(mapped)
}

fn map_gif_captions(
    captions: &[Caption],
    from: f64,
    to: f64,
    output_duration: f64,
) -> Result<Vec<Caption>> {
    let mut mapped = Vec::with_capacity(captions.len());
    let scale = output_duration / (to - from);
    for caption in captions {
        let (fade_from, fade_to) = caption_fade_window(caption)?;
        if caption.timeline == CaptionTimeline::Output {
            mapped.push(caption.clone());
            continue;
        }
        if caption.segment.is_some_and(|segment| segment != 0) {
            return Err(ExportError(
                "GIF source-timeline captions must use segment 0".into(),
            ));
        }
        let caption_from = caption.from.max(from);
        let caption_to = caption.to.min(to);
        if caption_from >= caption_to {
            continue;
        }
        let mut caption = caption.clone();
        caption.from = (caption_from - from) * scale;
        caption.to = (caption_to - from) * scale;
        caption.fade *= scale;
        caption.fade_window = Some(((fade_from - from) * scale, (fade_to - from) * scale));
        caption.timeline = CaptionTimeline::Output;
        caption.segment = None;
        mapped.push(caption);
    }
    Ok(mapped)
}

fn load_overlays(
    overlays: &[Overlay],
    captions: &[Caption],
    duration: f64,
    width: u32,
    height: u32,
) -> Result<Vec<LoadedOverlay>> {
    if overlays.len() + captions.len() > 256 {
        return Err(ExportError(
            "provide at most 256 image overlays and captions".into(),
        ));
    }
    let mut loaded: Vec<LoadedOverlay> = overlays
        .iter()
        .map(|o| {
            validate_range(o.from, o.to, duration)?;
            if !o.path.is_absolute() {
                return Err(ExportError("overlay path must be absolute".into()));
            }
            let pm = if o
                .path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("jsd"))
            {
                let doc = jiffshot_doc::io::load_document(&o.path)
                    .map_err(|e| ExportError(e.to_string()))?;
                doc.render_area(doc.bounds(), false)
                    .map(|(_, pm)| pm)
                    .ok_or_else(|| ExportError("overlay document is empty".into()))?
            } else {
                jiffshot_doc::io::load_image(&o.path).map_err(|e| ExportError(e.to_string()))?
            };
            Ok(LoadedOverlay {
                pm,
                x: o.x,
                y: o.y,
                from: o.from,
                to: o.to,
                fade: 0.0,
                fade_from: o.from,
                fade_to: o.to,
                segment: None,
            })
        })
        .collect::<Result<_>>()?;
    for caption in captions {
        validate_range(caption.from, caption.to, duration)?;
        let (fade_from, fade_to) = caption_fade_window(caption)?;
        if caption.text.trim().is_empty()
            || caption.text.chars().count() > 4096
            || !caption.size.is_finite()
            || !(1.0..=512.0).contains(&caption.size)
            || caption.padding > 1024
        {
            return Err(ExportError(
                "caption text, size, padding, or fade is outside the supported range".into(),
            ));
        }
        let pm = render_caption(caption, width, height)?;
        loaded.push(LoadedOverlay {
            pm,
            x: caption.x,
            y: caption.y,
            from: caption.from,
            to: caption.to,
            fade: caption.fade,
            fade_from,
            fade_to,
            segment: caption.segment,
        });
    }
    Ok(loaded)
}

fn render_caption(caption: &Caption, width: u32, height: u32) -> Result<Pixmap> {
    let padding = caption.padding as f32;
    let max_width = (i64::from(width) - i64::from(caption.x) - i64::from(caption.padding) * 2)
        .clamp(1, 8192) as f32;
    let style = jiffshot_doc::text::TextStyle {
        family: &caption.font,
        size: caption.size,
        bold: false,
        italic: false,
    };
    let layout = jiffshot_doc::text::layout(&caption.text, &style, Some(max_width));
    if layout.width() <= 0.0 || layout.height() <= 0.0 {
        return Err(ExportError(
            "caption text could not be rendered with the requested font".into(),
        ));
    }
    let text_width = layout.width().ceil().max(1.0);
    let text_height = layout.height().ceil().max(1.0);
    let canvas_width = (text_width + padding * 2.0).ceil().clamp(1.0, 8192.0) as u32;
    let canvas_height = (text_height + padding * 2.0).ceil().clamp(1.0, 8192.0) as u32;
    if canvas_width == 0 || canvas_height == 0 || (width == 0 || height == 0) {
        return Err(ExportError("caption canvas dimensions are invalid".into()));
    }
    let mut pm = Pixmap::new(canvas_width, canvas_height)
        .ok_or_else(|| ExportError("caption image could not be allocated".into()))?;
    pm.fill(caption.background.to_color());
    jiffshot_doc::text::draw(
        &mut pm,
        (0, 0),
        &layout,
        padding,
        padding,
        text_width,
        jiffshot_doc::text::Align::Left,
        caption.color,
    );
    Ok(pm)
}

fn composite_bgra(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    overlays: &[LoadedOverlay],
    at: f64,
    segment: Option<usize>,
) {
    for o in overlays
        .iter()
        .filter(|o| o.segment == segment && at >= o.from && at < o.to)
    {
        for (i, p) in o.pm.pixels().iter().enumerate() {
            let x = o.x as i64 + (i % o.pm.width() as usize) as i64;
            let y = o.y as i64 + (i / o.pm.width() as usize) as i64;
            if x < 0 || y < 0 || x >= width as i64 || y >= height as i64 {
                continue;
            }
            let index = (y as usize * width as usize + x as usize) * 4;
            let opacity = overlay_opacity(o, at);
            let alpha = (f64::from(p.alpha()) * opacity).round() as u32;
            let inverse = 255 - alpha;
            for (c, value) in [p.blue(), p.green(), p.red()].into_iter().enumerate() {
                let value = (f64::from(value) * opacity).round() as u32;
                pixels[index + c] =
                    (value + (pixels[index + c] as u32 * inverse + 127) / 255).min(255) as u8;
            }
        }
    }
}

fn blend_bgra(incoming: &mut [u8], previous: &[u8], numerator: u64, denominator: u64) {
    let previous_weight = denominator - numerator;
    for (incoming, previous) in incoming.iter_mut().zip(previous) {
        *incoming = ((u64::from(*previous) * previous_weight + u64::from(*incoming) * numerator)
            / denominator) as u8;
    }
}

fn overlay_opacity(overlay: &LoadedOverlay, at: f64) -> f64 {
    if overlay.fade == 0.0 {
        return 1.0;
    }
    ((at - overlay.fade_from) / overlay.fade)
        .min((overlay.fade_to - at) / overlay.fade)
        .clamp(0.0, 1.0)
}

/// Exports a trimmed range as an animated GIF, without retaining audio.
///
/// # Errors
/// Rejects invalid ranges, GIF rates outside 1..50, widths outside 1..8192, and source overwrites.
pub fn export_gif(
    source: &Path,
    out: &Path,
    (from, to): (f64, f64),
    options: GifOptions,
    progress: impl Fn(f64),
) -> Result<Metadata> {
    export_gif_cancellable(source, out, (from, to), options, progress, || false)
}

/// Like [`export_gif`], but abandons staging if cancellation is requested.
pub fn export_gif_cancellable(
    source: &Path,
    out: &Path,
    (from, to): (f64, f64),
    options: GifOptions,
    progress: impl Fn(f64),
    cancelled: impl Fn() -> bool,
) -> Result<Metadata> {
    let Some(max_bytes) = options.max_bytes else {
        return export_gif_once(source, out, (from, to), options, progress, cancelled);
    };
    if max_bytes == 0 {
        return Err(ExportError("max_bytes must be greater than zero".into()));
    }
    let mut candidate = options;
    candidate.max_bytes = None;
    for attempt in 0..8 {
        check_cancelled(&cancelled)?;
        let temporary = TempOutput::reserve("jiffshot-size", "gif")?;
        let metadata = export_gif_once(
            source,
            &temporary.0,
            (from, to),
            candidate.clone(),
            |_| {},
            &cancelled,
        )?;
        if metadata.bytes <= max_bytes {
            let source_paths: Vec<&Path> = std::iter::once(source)
                .chain(
                    candidate
                        .overlays
                        .iter()
                        .map(|overlay| overlay.path.as_path()),
                )
                .collect();
            let output = Output::new(out, &source_paths, "gif")?;
            fs::copy(&temporary.0, &output.staging)?;
            fs::OpenOptions::new()
                .write(true)
                .open(&output.staging)?
                .sync_all()?;
            output.publish()?;
            let mut metadata = metadata;
            metadata.path = out.to_path_buf();
            metadata.bytes = fs::metadata(out)?.len();
            progress(1.0);
            return Ok(metadata);
        }
        if attempt == 7 {
            break;
        }
        let previous = (candidate.max_width, candidate.fps);
        candidate.max_width = ((f64::from(candidate.max_width) * 0.8).floor() as u32).max(1);
        candidate.fps = (candidate.fps * 0.8).max(1.0);
        if previous == (candidate.max_width, candidate.fps) {
            break;
        }
    }
    Err(ExportError(format!(
        "could not meet GIF max_bytes limit of {max_bytes} after reducing width and frame rate"
    )))
}

struct TempOutput(PathBuf, Option<tempfile::TempDir>);

#[cfg(unix)]
fn check_private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if fs::metadata(path)?.permissions().mode() & 0o077 != 0 {
        eprintln!(
            "jiffshot: filesystem did not enforce owner-only permissions for {}; access depends on its ACLs",
            path.display()
        );
    }
    Ok(())
}

impl TempOutput {
    fn reserve(prefix: &str, extension: &str) -> Result<Self> {
        let mut builder = tempfile::Builder::new();
        let prefix = format!(".{prefix}-");
        builder.prefix(&prefix);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o700));
        }
        let directory = builder.tempdir()?;
        #[cfg(unix)]
        check_private_directory(directory.path())?;
        let path = directory.path().join(format!("output.{extension}"));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&path)?;
        Ok(Self(path, Some(directory)))
    }
}

impl Drop for TempOutput {
    fn drop(&mut self) {
        if let Some(directory) = self.1.take() {
            if let Err(error) = directory.close() {
                eprintln!(
                    "could not remove temporary video directory for {}: {error}",
                    self.0.display()
                );
            }
        }
    }
}

fn export_gif_once(
    source: &Path,
    out: &Path,
    (from, to): (f64, f64),
    options: GifOptions,
    progress: impl Fn(f64),
    cancelled: impl Fn() -> bool,
) -> Result<Metadata> {
    use gif::{Encoder, Repeat};
    check_cancelled(&cancelled)?;
    let metadata = inspect(source)?;
    validate_range(from, to, metadata.duration)?;
    if !options.fps.is_finite()
        || !(1.0..=50.0).contains(&options.fps)
        || !(1..=8192).contains(&options.max_width)
        || !(2..=256).contains(&options.palette_size)
        || options
            .duration
            .is_some_and(|duration| !duration.is_finite() || duration <= 0.0)
    {
        return Err(ExportError(
            "GIF fps must be 1..50, max_width 1..8192, and palette_size 2..256".into(),
        ));
    }
    let scale = (options.max_width as f64 / metadata.width as f64).min(1.0);
    let (w, h) = (
        (metadata.width as f64 * scale).round().max(1.0) as u32,
        (metadata.height as f64 * scale).round().max(1.0) as u32,
    );
    if h > u16::MAX as u32 {
        return Err(ExportError("GIF height exceeds 65535".into()));
    }
    let delay_ms = ((100.0 / options.fps).round().max(1.0) * 10.0) as u32;
    let target_frames = match options.duration {
        Some(duration) => {
            let frames = (duration * 1000.0 / f64::from(delay_ms)).round();
            if !frames.is_finite() || !(1.0..=u64::MAX as f64).contains(&frames) {
                return Err(ExportError(
                    "duration must produce a representable number of GIF frames".into(),
                ));
            }
            Some(frames as u64)
        }
        None => None,
    };
    let output_duration = target_frames
        .map(|frames| f64::from(delay_ms) * frames as f64 / 1000.0)
        .unwrap_or(to - from);
    let mut captions = map_gif_captions(&options.captions, from, to, output_duration)?;
    for caption in &mut captions {
        if let Some((canvas_width, canvas_height)) = caption.reference_canvas {
            if canvas_width == 0 || canvas_height == 0 {
                return Err(ExportError(
                    "caption reference canvas must be non-zero".into(),
                ));
            }
            let scale_x = f64::from(w) / f64::from(canvas_width);
            let scale_y = f64::from(h) / f64::from(canvas_height);
            let font_scale = (scale_x + scale_y) / 2.0;
            caption.x = (f64::from(caption.x) * scale_x).round() as i32;
            caption.y = (f64::from(caption.y) * scale_y).round() as i32;
            caption.size = (f64::from(caption.size) * font_scale).max(1.0) as f32;
            caption.padding = (f64::from(caption.padding) * font_scale).round() as u32;
        }
    }
    let overlays = load_overlays(&options.overlays, &captions, output_duration, w, h)?;
    let paths: Vec<&Path> = std::iter::once(source)
        .chain(options.overlays.iter().map(|o| o.path.as_path()))
        .collect();
    let output = Output::new(out, &paths, "gif")?;
    let mut writer = std::io::BufWriter::new(fs::File::create(&output.staging)?);
    let mut count = 0_u64;
    {
        let mut encoder = Encoder::new(&mut writer, w as u16, h as u16, &[])
            .map_err(|error| ExportError(error.to_string()))?;
        encoder
            .set_repeat(if options.looped {
                Repeat::Infinite
            } else {
                Repeat::Finite(0)
            })
            .map_err(|e| ExportError(e.to_string()))?;
        let mut reader = Reader::open(source)?;
        reader.seek(hns(from))?;
        let mut current = reader
            .next_video()?
            .ok_or_else(|| ExportError("range has no frames".into()))?;
        let mut next = reader.next_video()?;
        let dither_mode = options.dither_mode.unwrap_or(if options.dither {
            GifDither::Ordered
        } else {
            GifDither::None
        });
        let mut pending: Option<GifPendingFrame> = None;
        loop {
            check_cancelled(&cancelled)?;
            let output_time = count as f64 * f64::from(delay_ms) / 1000.0;
            if target_frames.is_some_and(|frames| count >= frames)
                || (target_frames.is_none() && output_time >= to - from)
            {
                break;
            }
            let at = from
                + target_frames.map_or(output_time, |frames| {
                    count as f64 / frames as f64 * (to - from)
                });
            while next.as_ref().is_some_and(|f| seconds(f.time) <= at) {
                check_cancelled(&cancelled)?;
                current = next
                    .take()
                    .ok_or_else(|| ExportError("missing pending GIF frame".into()))?;
                next = reader.next_video()?;
            }
            let pm =
                to_pixmap(&current).ok_or_else(|| ExportError("invalid decoded frame".into()))?;
            let mut small = if (w, h) == (pm.width(), pm.height()) {
                pm
            } else {
                raster::resample(&pm, w, h).ok_or_else(|| ExportError("resize failed".into()))?
            };
            for o in overlays
                .iter()
                .filter(|o| output_time >= o.from && output_time < o.to)
            {
                let paint = tiny_skia::PixmapPaint {
                    opacity: overlay_opacity(o, output_time) as f32,
                    ..Default::default()
                };
                small.draw_pixmap(
                    o.x,
                    o.y,
                    o.pm.as_ref(),
                    &paint,
                    tiny_skia::Transform::identity(),
                    None,
                );
            }
            let mut rgba = jiffshot_doc::io::to_rgba_image(&small);
            if options.fade && output_duration - output_time < 0.5 {
                let k = ((output_duration - output_time) / 0.5).clamp(0.0, 1.0);
                for p in rgba.pixels_mut() {
                    for c in 0..3 {
                        p.0[c] = (p.0[c] as f64 * k) as u8;
                    }
                }
            }
            match dither_mode {
                GifDither::None => {}
                GifDither::Ordered => ordered_dither(&mut rgba),
                GifDither::FloydSteinberg => floyd_steinberg_dither(&mut rgba),
            }
            if options.palette_size < 256 {
                quantize_palette(&mut rgba, options.palette_size);
            }
            let same_frame = pending
                .as_ref()
                .is_some_and(|previous| previous.pixels == rgba);
            if options.optimize_frames
                && same_frame
                && pending
                    .as_ref()
                    .is_some_and(|previous| previous.delay_ms.saturating_add(delay_ms) <= 655_350)
            {
                pending
                    .as_mut()
                    .expect("matching pending GIF frame")
                    .delay_ms += delay_ms;
            } else {
                let delta = if options.optimize_frames {
                    pending
                        .as_ref()
                        .and_then(|previous| gif_delta_frame(&previous.pixels, &rgba))
                } else {
                    None
                };
                if let Some(frame) = pending.take() {
                    encode_gif_frame(&mut encoder, frame)?;
                }
                let (encoded, left, top) = delta.unwrap_or_else(|| (rgba.clone(), 0, 0));
                pending = Some(GifPendingFrame {
                    pixels: rgba,
                    encoded,
                    left,
                    top,
                    delay_ms,
                });
            }
            count += 1;
            progress(((at - from) / (to - from)).clamp(0.0, 1.0));
        }
        if let Some(frame) = pending {
            encode_gif_frame(&mut encoder, frame)?;
        }
    }
    check_cancelled(&cancelled)?;
    use std::io::Write;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    let mut gif_metadata = inspect(&output.staging)?;
    gif_metadata.path = out.to_path_buf();
    gif_metadata.audio_note = if metadata.audio.is_some() || metadata.audio_note.is_some() {
        Some("GIF cannot retain source audio".into())
    } else {
        None
    };
    check_cancelled(&cancelled)?;
    output.publish()?;
    progress(1.0);
    Ok(gif_metadata)
}

struct GifPendingFrame {
    pixels: image::RgbaImage,
    encoded: image::RgbaImage,
    left: u32,
    top: u32,
    delay_ms: u32,
}

fn gif_delta_frame(
    previous: &image::RgbaImage,
    current: &image::RgbaImage,
) -> Option<(image::RgbaImage, u32, u32)> {
    if previous.dimensions() != current.dimensions()
        || previous.pixels().any(|pixel| pixel.0[3] != 255)
        || current.pixels().any(|pixel| pixel.0[3] != 255)
    {
        return None;
    }

    let (mut min_x, mut min_y) = (u32::MAX, u32::MAX);
    let (mut max_x, mut max_y) = (0, 0);
    for (x, y, pixel) in current.enumerate_pixels() {
        if previous.get_pixel(x, y) != pixel {
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    (min_x != u32::MAX).then(|| {
        (
            image::imageops::crop_imm(current, min_x, min_y, max_x - min_x + 1, max_y - min_y + 1)
                .to_image(),
            min_x,
            min_y,
        )
    })
}

fn encode_gif_frame<W: std::io::Write>(
    encoder: &mut gif::Encoder<W>,
    frame: GifPendingFrame,
) -> Result<()> {
    let (width, height) = frame.encoded.dimensions();
    let mut rgba = frame.encoded.into_raw();
    let mut encoded = gif::Frame::from_rgba_speed(width as u16, height as u16, &mut rgba, 10);
    encoded.left = frame.left as u16;
    encoded.top = frame.top as u16;
    encoded.delay = (frame.delay_ms / 10) as u16;
    encoded.dispose = gif::DisposalMethod::Keep;
    encoder
        .write_frame(&encoded)
        .map_err(|error| ExportError(error.to_string()))
}

fn check_cancelled(cancelled: &impl Fn() -> bool) -> Result<()> {
    if cancelled() {
        Err(ExportError("video export cancelled".into()))
    } else {
        Ok(())
    }
}

#[derive(Debug)]
struct Output {
    staging: PathBuf,
    destination: PathBuf,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    staging_dir: PathBuf,
}

/// A completed export staged beside its destination, ready for atomic publication.
#[derive(Debug)]
pub struct PreparedOutput(Output);

/// Copies an already finalized export into a destination-side staging file.
///
/// # Errors
/// Rejects source overwrites and invalid destinations, and reports staging or copy failures.
pub fn prepare_output(
    source: &Path,
    destination: &Path,
    protected_inputs: &[&Path],
    extension: &str,
) -> Result<PreparedOutput> {
    let inputs: Vec<&Path> = std::iter::once(source)
        .chain(protected_inputs.iter().copied())
        .collect();
    let output = Output::new(destination, &inputs, extension)?;
    fs::copy(source, &output.staging).map_err(|error| {
        ExportError(format!(
            "cannot stage {} for {}: {error}",
            source.display(),
            destination.display()
        ))
    })?;
    Ok(PreparedOutput(output))
}

impl PreparedOutput {
    /// Atomically replaces this output's destination with the staged export.
    ///
    /// # Errors
    /// Returns an error if flushing or replacing the destination fails.
    pub fn publish(self) -> Result<()> {
        let destination = self.0.destination.clone();
        self.0.publish().map_err(|error| {
            ExportError(format!("cannot publish {}: {error}", destination.display()))
        })
    }
}

impl Output {
    fn new(out: &Path, inputs: &[&Path], extension: &str) -> Result<Self> {
        if !out.is_absolute()
            || out
                .extension()
                .and_then(|s| s.to_str())
                .is_none_or(|s| !s.eq_ignore_ascii_case(extension))
        {
            return Err(ExportError(format!(
                "output must be an absolute .{extension} path"
            )));
        }
        if out.exists() {
            let canonical = fs::canonicalize(out)?;
            for input in inputs {
                if fs::canonicalize(input)? == canonical {
                    return Err(ExportError("output must not overwrite an input".into()));
                }
            }
        }
        let parent = out
            .parent()
            .ok_or_else(|| ExportError("output has no parent directory".into()))?;
        fs::create_dir_all(parent)?;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..100 {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            {
                use std::os::unix::fs::DirBuilderExt;
                let staging_dir =
                    parent.join(format!(".jiffshot-export-{}-{id}", std::process::id()));
                match fs::DirBuilder::new().mode(0o700).create(&staging_dir) {
                    Ok(()) => {
                        let output = Self {
                            staging: staging_dir.join(format!("output.{extension}")),
                            destination: out.to_path_buf(),
                            staging_dir,
                        };
                        check_private_directory(&output.staging_dir)?;
                        return Ok(output);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            #[cfg(windows)]
            {
                let staging = parent.join(format!(
                    ".jiffshot-export-{}-{id}.{extension}",
                    std::process::id()
                ));
                match fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&staging)
                {
                    Ok(_) => {
                        return Ok(Self {
                            staging,
                            destination: out.to_path_buf(),
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Err(ExportError("cannot reserve export staging file".into()))
    }

    fn publish(self) -> Result<()> {
        fs::OpenOptions::new()
            .write(true)
            .open(&self.staging)?
            .sync_all()?;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        fs::rename(&self.staging, &self.destination)?;
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            let from: Vec<u16> = self
                .staging
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect();
            let to: Vec<u16> = self
                .destination
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect();
            // SAFETY: both paths are live, NUL-terminated UTF-16 buffers. Staging is on the destination volume.
            let ok = unsafe {
                windows_sys::Win32::Storage::FileSystem::MoveFileExW(
                    from.as_ptr(),
                    to.as_ptr(),
                    windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING
                        | windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH,
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        Ok(())
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.staging) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "could not remove video export staging file {}: {error}",
                    self.staging.display()
                );
            }
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Err(error) = fs::remove_dir(&self.staging_dir) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "could not remove video export staging directory {}: {error}",
                    self.staging_dir.display()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_ranges_and_accepts_exact_end() {
        for (from, to) in [
            (-1.0, 1.0),
            (1.0, 1.0),
            (2.0, 1.0),
            (0.0, 3.0001),
            (f64::NAN, 1.0),
            (0.0, f64::INFINITY),
        ] {
            assert!(validate_range(from, to, 3.0).is_err(), "{from}..{to}");
        }
        assert!(validate_range(0.0, 3.0, 3.0).is_ok());
        assert!(validate_range(0.0, 0.000_000_001, 3.0).is_err());
    }

    #[test]
    fn clips_audio_at_both_trim_boundaries_on_sample_alignment() {
        let pcm: Vec<i16> = (0..20).collect();
        let a = AudioSpec {
            sample_rate: 10,
            channels: 2,
        };
        let (data, at, duration) =
            clip_audio(0, &pcm, a, 2_500_000, 7_500_000).expect("overlapping samples");
        assert_eq!(data, &[6, 7, 8, 9, 10, 11, 12, 13]);
        assert_eq!(at, 500_000);
        assert_eq!(duration, 4_000_000);
        assert!(clip_audio(0, &pcm, a, HNS_PER_SECOND, 2 * HNS_PER_SECOND).is_none());
    }

    #[test]
    fn audio_crossfade_moves_linearly_from_outgoing_to_incoming_samples() {
        let mut incoming = vec![6_000; 8];
        let outgoing = [-6_000; 8];
        blend_audio(&mut incoming, &outgoing, 2);

        assert_eq!(incoming[0], -3_000);
        assert_eq!(incoming[2], 0);
        assert!(incoming[4] > 0);
        assert!(incoming[6] > 2_000);
    }

    #[test]
    fn fits_bgra_and_preserves_odd_size_crop() {
        let frame = VideoFrame {
            width: 3,
            height: 3,
            time: 0,
            bgra: (0..36).collect(),
        };
        let fitted = fit_frame(&frame, 2, 2).expect("valid frame");
        assert_eq!(
            fitted,
            [0, 1, 2, 3, 4, 5, 6, 7, 12, 13, 14, 15, 16, 17, 18, 19]
        );
        assert!(to_pixmap(&VideoFrame {
            width: 3,
            height: 3,
            time: 0,
            bgra: vec![0; 4]
        })
        .is_none());
        let pm = to_pixmap(&VideoFrame {
            width: 2,
            height: 1,
            time: 0,
            bgra: vec![1, 2, 3, 0, 4, 5, 6, 0],
        })
        .expect("valid image");
        let mut canvas = vec![0; 2 * 4 * 4];
        paste_fitted(&pm, &mut canvas, 2, 4);
        assert_eq!(&canvas[8..16], &[1, 2, 3, 0, 4, 5, 6, 0]);
        assert!(canvas[..8].iter().chain(&canvas[16..]).all(|b| *b == 0));
    }

    struct FixtureDir(PathBuf);
    impl FixtureDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::current_dir()
                .expect("working directory")
                .join(format!(
                    ".post-tests-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir(&path).expect("unique fixture directory");
            Self(path)
        }
        fn path(&self, file: &str) -> PathBuf {
            self.0.join(file)
        }
    }
    impl Drop for FixtureDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn optimized_gif_metadata_matches_its_encoded_frame_delays() {
        let dir = FixtureDir::new();
        let source = dir.path("changing.mp4");
        let mut writer =
            Writer::create(&source, VideoSpec::for_size(64, 48, 12), None).expect("encoder");
        for frame_index in 0..12 {
            let bgra = if frame_index < 2 {
                [0, 0, 240, 0]
            } else if frame_index < 6 {
                [0, 240, 0, 0]
            } else {
                [240, 0, 0, 0]
            };
            let at = frame_time(frame_index, 12);
            let finish = frame_time(frame_index + 1, 12);
            writer
                .write_frame(&bgra.repeat(64 * 48), at, finish - at)
                .expect("source frame");
        }
        writer.finish().expect("finalize source");

        let gif = dir.path("optimized.gif");
        let exported = export_gif(
            &source,
            &gif,
            (0.0, 1.0),
            GifOptions {
                fps: 12.0,
                max_width: 64,
                dither: false,
                optimize_frames: true,
                ..GifOptions::default()
            },
            |_| {},
        )
        .expect("export optimized GIF");
        let inspected = inspect(&gif).expect("inspect optimized GIF");

        assert_eq!(exported.frames, Some(3));
        assert_eq!(exported.fps, inspected.fps);
        assert_eq!(exported.constant_frame_rate, inspected.constant_frame_rate);
        assert_eq!(exported.constant_frame_rate, Some(false));
    }

    #[cfg(windows)]
    #[test]
    fn inspect_reports_the_native_h264_codec() {
        let dir = FixtureDir::new();
        let source = dir.path("codec.mp4");
        let mut writer =
            Writer::create(&source, VideoSpec::for_size(64, 48, 12), None).expect("encoder");
        let pixels = [0, 0, 0, 0].repeat(64 * 48);
        for index in 0..12 {
            writer
                .write_frame(&pixels, frame_time(index, 12), HNS_PER_SECOND / 12)
                .expect("source frame");
        }
        writer.finish().expect("finalize source");

        assert_eq!(
            inspect(&source).expect("inspect source").codec.as_deref(),
            Some("H.264")
        );
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn inspect_reports_the_measured_mp4_keyframe_interval() {
        let dir = FixtureDir::new();
        let source = dir.path("keyframes.mp4");
        let mut writer =
            Writer::create(&source, VideoSpec::for_size(64, 48, 12), None).expect("encoder");
        let pixels = [0, 0, 240, 0].repeat(64 * 48);
        for index in 0..72 {
            let at = frame_time(index, 12);
            let end = frame_time(index + 1, 12);
            writer
                .write_frame(&pixels, at, end - at)
                .expect("source frame");
        }
        writer.finish().expect("finalize source");
        let interval = inspect(&source)
            .expect("inspect source")
            .keyframe_interval
            .expect("the file has more than one sync sample");
        assert!(
            (interval - 2.0).abs() < 0.1,
            "expected approximately two seconds between sync samples, got {interval}"
        );
    }

    #[test]
    fn gif_delta_frames_encode_only_changed_opaque_pixels() {
        use image::AnimationDecoder;

        let first = image::RgbaImage::from_pixel(8, 8, image::Rgba([255, 0, 0, 255]));
        let mut second = first.clone();
        second.put_pixel(3, 4, image::Rgba([0, 0, 255, 255]));
        let (delta, left, top) = gif_delta_frame(&first, &second).expect("opaque changed region");

        assert_eq!((delta.width(), delta.height(), left, top), (1, 1, 3, 4));

        let mut bytes = Vec::new();
        {
            let mut encoder = gif::Encoder::new(&mut bytes, 8, 8, &[]).expect("GIF encoder");
            encoder
                .write_frame(&gif::Frame::from_rgba_speed(
                    8,
                    8,
                    &mut first.into_raw(),
                    10,
                ))
                .expect("encode base frame");
            encode_gif_frame(
                &mut encoder,
                GifPendingFrame {
                    pixels: second,
                    encoded: delta,
                    left,
                    top,
                    delay_ms: 100,
                },
            )
            .expect("encode delta frame");
        }
        let decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes))
            .expect("decode delta GIF");
        let frames = decoder
            .into_frames()
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("decode composed frames");

        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].buffer().dimensions(), (8, 8));
        assert_eq!(frames[1].buffer().get_pixel(3, 4).0, [0, 0, 255, 255]);
        assert_eq!(frames[1].buffer().get_pixel(2, 4).0, [255, 0, 0, 255]);
    }

    #[test]
    fn gif_delta_frames_fall_back_for_transparency_or_dimension_changes() {
        let opaque = image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]));
        let transparent = image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 0, 0]));
        let different_size = image::RgbaImage::new(1, 2);

        assert!(gif_delta_frame(&opaque, &transparent).is_none());
        assert!(gif_delta_frame(&opaque, &different_size).is_none());
    }

    #[test]
    fn gif_fade_uses_output_time_after_duration_remapping() {
        use image::AnimationDecoder;

        let dir = FixtureDir::new();
        let source = dir.path("fade-source.mp4");
        let mut writer =
            Writer::create(&source, VideoSpec::for_size(64, 48, 12), None).expect("encoder");
        for frame_index in 0..12 {
            let at = frame_time(frame_index, 12);
            let finish = frame_time(frame_index + 1, 12);
            writer
                .write_frame(&[0, 0, 240, 0].repeat(64 * 48), at, finish - at)
                .expect("source frame");
        }
        writer.finish().expect("finalize source");

        let gif = dir.path("fade.gif");
        export_gif(
            &source,
            &gif,
            (0.0, 1.0),
            GifOptions {
                fps: 12.0,
                max_width: 64,
                duration: Some(2.0),
                fade: true,
                dither: false,
                ..GifOptions::default()
            },
            |_| {},
        )
        .expect("export faded GIF");
        let decoder = image::codecs::gif::GifDecoder::new(std::io::BufReader::new(
            fs::File::open(&gif).expect("open GIF"),
        ))
        .expect("GIF decoder");
        let frames = decoder
            .into_frames()
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("decode GIF frames");
        let initial_red = frames[0].buffer().get_pixel(32, 24).0[0];
        let red_at_1_52_seconds = frames[19].buffer().get_pixel(32, 24).0[0];

        assert!(
            red_at_1_52_seconds > initial_red.saturating_mul(4) / 5,
            "fade should use 2-second output time, not the 1-second source range: \
             first red={initial_red}, red at 1.52s={red_at_1_52_seconds}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn preserves_audio_samples_at_trimmed_and_joined_timeline_positions() {
        let dir = FixtureDir::new();
        let source = dir.path("alternating.mp4");
        let audio = AudioSpec {
            sample_rate: 48_000,
            channels: 2,
        };
        let mut writer =
            Writer::create(&source, VideoSpec::for_size(64, 48, 12), Some(audio)).unwrap();
        for n in 0..12 {
            let at = n * HNS_PER_SECOND / 12;
            let end = (n + 1) * HNS_PER_SECOND / 12;
            writer
                .write_frame(&[50, 50, 50, 0].repeat(64 * 48), at, end - at)
                .unwrap();
            let amplitude = if n < 6 { 6000 } else { -6000 };
            writer
                .write_audio(&vec![amplitude; 48_000 / 12 * 2], at, end - at)
                .unwrap();
        }
        writer.finish().unwrap();

        let trimmed = dir.path("trim-audio.mp4");
        export_mp4(
            &[Segment {
                path: source.clone(),
                from: 0.25,
                to: 0.75,
                speed: 1.0,
            }],
            &trimmed,
            Mp4Options::default(),
            |_| {},
        )
        .unwrap();
        let joined = dir.path("join-audio.mp4");
        let segments = [
            Segment {
                path: source.clone(),
                from: 0.5,
                to: 1.0,
                speed: 1.0,
            },
            Segment {
                path: source,
                from: 0.0,
                to: 0.5,
                speed: 1.0,
            },
        ];
        export_mp4(&segments, &joined, Mp4Options::default(), |_| {}).unwrap();
        let crossfaded = dir.path("crossfaded-audio.mp4");
        let metadata = export_mp4(
            &segments,
            &crossfaded,
            Mp4Options {
                fps: Some(12),
                transition_seconds: Some(0.25),
                ..Mp4Options::default()
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(metadata.frames, Some(9));
        assert!((metadata.duration - 0.75).abs() < 0.02);

        let average = |path: &Path, from: f64, to: f64| {
            let mut reader = Reader::open(path).unwrap();
            reader.select_video(false).unwrap();
            reader.select_audio(true).unwrap();
            let (mut sum, mut count) = (0_i64, 0_usize);
            while let Some((time, samples)) = reader.next_audio().unwrap() {
                for (index, channels) in samples.as_chunks::<2>().0.iter().enumerate() {
                    let at = time + index as i64 * HNS_PER_SECOND / i64::from(audio.sample_rate);
                    if at >= hns(from) && at < hns(to) {
                        sum += i64::from(channels[0]) + i64::from(channels[1]);
                        count += 2;
                    }
                }
            }
            assert!(
                count >= 8_000,
                "too little audio in {from}..{to}: {count} samples"
            );
            sum as f64 / count as f64
        };
        assert!(average(&trimmed, 0.1, 0.2) > 4000.0);
        assert!(average(&trimmed, 0.3, 0.4) < -4000.0);
        assert!(average(&joined, 0.1, 0.2) < -4000.0);
        assert!(average(&joined, 0.6, 0.7) > 4000.0);
        assert!(average(&crossfaded, 0.25, 0.35) < -2_000.0);
        assert!(average(&crossfaded, 0.4, 0.5) > 2_000.0);
    }

    #[test]
    fn crossfade_export_blends_encoded_audio_through_the_overlap() {
        let dir = FixtureDir::new();
        let audio = AudioSpec {
            sample_rate: 48_000,
            channels: 2,
        };
        let fps = 20;
        let make_source = |name: &str, frequency: f64| {
            let path = dir.path(name);
            let mut writer = Writer::create(&path, VideoSpec::for_size(64, 48, fps), Some(audio))
                .expect("audio source encoder");
            let samples_per_frame = audio.sample_rate / fps;
            for frame in 0..fps {
                let at = frame_time(u64::from(frame), fps);
                let end = frame_time(u64::from(frame + 1), fps);
                writer
                    .write_frame(&[50, 50, 50, 0].repeat(64 * 48), at, end - at)
                    .expect("source video frame");
                let mut samples = Vec::with_capacity(samples_per_frame as usize * 2);
                for sample in 0..samples_per_frame {
                    let index = frame * samples_per_frame + sample;
                    let phase = std::f64::consts::TAU * frequency * f64::from(index)
                        / f64::from(audio.sample_rate);
                    let value = (phase.sin() * 8_000.0) as i16;
                    samples.extend_from_slice(&[value, value]);
                }
                writer
                    .write_audio(&samples, at, end - at)
                    .expect("source audio");
            }
            writer.finish().expect("finalize source");
            path
        };
        let first = make_source("first-tone.mp4", 440.0);
        let second = make_source("second-tone.mp4", 880.0);
        let output = dir.path("crossfaded-tones.mp4");
        let segments = [first.clone(), second.clone()].map(|path| Segment {
            path,
            from: 0.0,
            to: 1.0,
            speed: 1.0,
        });
        let metadata = export_mp4(
            &segments,
            &output,
            Mp4Options {
                fps: Some(fps),
                transition_seconds: Some(0.5),
                audio: Audio::RequireCompatible,
                ..Mp4Options::default()
            },
            |_| {},
        )
        .expect("crossfade export");
        assert_eq!(metadata.frames, Some(30));
        assert!(metadata.audio.is_some(), "output keeps its soundtrack");

        let mut reader = Reader::open(&output).expect("crossfade reader");
        reader.select_video(false).expect("disable video");
        reader.select_audio(true).expect("enable audio");
        let mut decoded = Vec::new();
        while let Some((at, samples)) = reader.next_audio().expect("decode audio") {
            for (index, channels) in samples.as_chunks::<2>().0.iter().enumerate() {
                let time = seconds(at) + index as f64 / f64::from(audio.sample_rate);
                decoded.push((time, f64::from(channels[0])));
            }
        }
        let strength = |from: f64, to: f64, frequency: f64| {
            let (mut sin_sum, mut cos_sum, mut count) = (0.0, 0.0, 0usize);
            for &(time, sample) in &decoded {
                if time >= from && time < to {
                    let phase = std::f64::consts::TAU * frequency * time;
                    sin_sum += sample * phase.sin();
                    cos_sum += sample * phase.cos();
                    count += 1;
                }
            }
            assert!(count >= 4_000, "missing audio in {from}..{to}: {count}");
            sin_sum.hypot(cos_sum) * 2.0 / count as f64
        };
        let before = (strength(0.2, 0.3, 440.0), strength(0.2, 0.3, 880.0));
        let early = (strength(0.55, 0.65, 440.0), strength(0.55, 0.65, 880.0));
        let late = (strength(0.85, 0.95, 440.0), strength(0.85, 0.95, 880.0));
        let after = (strength(1.2, 1.3, 440.0), strength(1.2, 1.3, 880.0));
        assert!(
            before.0 > 3_000.0 && before.0 > before.1 * 4.0,
            "{before:?}"
        );
        assert!(early.0 > early.1 * 1.5, "early overlap: {early:?}");
        assert!(late.1 > late.0 * 1.5, "late overlap: {late:?}");
        assert!(after.1 > 3_000.0 && after.1 > after.0 * 4.0, "{after:?}");

        let split_segments = [
            (first.clone(), 0.0, 0.5),
            (first, 0.5, 1.0),
            (second.clone(), 0.0, 0.5),
            (second, 0.5, 1.0),
        ]
        .map(|(path, from, to)| Segment {
            path,
            from,
            to,
            speed: 1.0,
        });
        let split_output = dir.path("crossfaded-split-tones.mp4");
        let split_metadata = export_mp4(
            &split_segments,
            &split_output,
            Mp4Options {
                fps: Some(fps),
                transition_seconds: Some(0.25),
                transition_boundaries: Some(vec![false, true, false]),
                audio: Audio::RequireCompatible,
                ..Mp4Options::default()
            },
            |_| {},
        )
        .expect("crossfade only the original source boundary");
        assert_eq!(split_metadata.frames, Some(35));
        let mut reader = Reader::open(&split_output).expect("split crossfade reader");
        reader.select_video(false).expect("disable video");
        reader.select_audio(true).expect("enable audio");
        let mut split_audio = Vec::new();
        while let Some((at, samples)) = reader.next_audio().expect("decode split audio") {
            for (index, channels) in samples.as_chunks::<2>().0.iter().enumerate() {
                let time = seconds(at) + index as f64 / f64::from(audio.sample_rate);
                split_audio.push((time, f64::from(channels[0])));
            }
        }
        let split_strength = |from: f64, to: f64, frequency: f64| {
            let (mut sin_sum, mut cos_sum, mut count) = (0.0, 0.0, 0usize);
            for &(time, sample) in &split_audio {
                if time >= from && time < to {
                    let phase = std::f64::consts::TAU * frequency * time;
                    sin_sum += sample * phase.sin();
                    cos_sum += sample * phase.cos();
                    count += 1;
                }
            }
            assert!(
                count >= 4_000,
                "missing split audio in {from}..{to}: {count}"
            );
            sin_sum.hypot(cos_sum) * 2.0 / count as f64
        };
        let internal = (
            split_strength(0.45, 0.55, 440.0),
            split_strength(0.45, 0.55, 880.0),
        );
        let overlap = (
            split_strength(0.8, 0.9, 440.0),
            split_strength(0.8, 0.9, 880.0),
        );
        assert!(internal.0 > 3_000.0 && internal.0 > internal.1 * 4.0);
        assert!(overlap.0 > 1_000.0 && overlap.1 > 1_000.0, "{overlap:?}");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn temporary_outputs_stay_private_after_replacement_and_are_removed() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempOutput::reserve("jiffshot-transition-test", "bgra").unwrap();
        let directory = temp.0.parent().unwrap().to_path_buf();
        assert_eq!(
            fs::metadata(&temp.0).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let output = Output::new(&temp.0, &[], "bgra").unwrap();
        fs::write(&output.staging, b"replacement").unwrap();
        fs::set_permissions(&output.staging, fs::Permissions::from_mode(0o644)).unwrap();
        output.publish().unwrap();
        assert_eq!(
            fs::metadata(&temp.0).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let path = temp.0.clone();
        drop(temp);
        assert!(!path.exists());
        assert!(!directory.exists());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn staging_directory_is_private_and_removed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let output = Output::new(&dir.path().join("output.mp4"), &[], "mp4").unwrap();
        let staging_dir = output.staging_dir.clone();
        assert_eq!(
            fs::metadata(&staging_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::write(&output.staging, b"unfinished").unwrap();
        drop(output);
        assert!(!staging_dir.exists());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn prepared_output_keeps_source_permissions_after_private_staging() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.mp4");
        let destination = dir.path().join("destination.mp4");
        fs::write(&source, b"rendered").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
        let staged = prepare_output(&source, &destination, &[], "mp4").unwrap();
        let staging_dir = staged.0.staging_dir.clone();
        assert_eq!(
            fs::metadata(&staging_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&staged.0.staging)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        assert!(!destination.exists());
        staged.publish().unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"rendered");
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert!(!staging_dir.exists());
    }

    #[test]
    fn strict_audio_rejects_unsupported_or_mismatched_sources_without_changing_gui_policy() {
        let source = |audio| Metadata {
            path: PathBuf::from("source.mp4"),
            format: "mp4".into(),
            width: 64,
            height: 48,
            fps: 30.0,
            duration: 1.0,
            audio,
            audio_note: None,
            bytes: 1,
            frames: None,
            codec: None,
            pixel_format: None,
            keyframe_interval: None,
            constant_frame_rate: None,
        };
        let supported = Some(AudioMetadata {
            sample_rate: 48_000,
            channels: 2,
        });
        let unsupported = Some(AudioMetadata {
            sample_rate: 32_000,
            channels: 1,
        });
        let surround = Some(AudioMetadata {
            sample_rate: 48_000,
            channels: 6,
        });
        let mono = Some(AudioMetadata {
            sample_rate: 48_000,
            channels: 1,
        });
        let mut undecodable = source(None);
        undecodable.audio_note = Some("source audio cannot be decoded".into());
        for sources in [
            vec![source(unsupported)],
            vec![source(surround)],
            vec![source(supported), source(None)],
            vec![source(None), source(supported)],
            vec![source(supported), source(mono)],
            vec![undecodable],
        ] {
            let error =
                choose_audio(&sources, Audio::RequireCompatible).expect_err("audio loss rejected");
            assert!(error.to_string().contains("--mute"), "{error}");
            let gui = choose_audio(&sources, Audio::PreserveCompatible).expect("GUI remains lossy");
            assert_eq!(gui.0, None);
            assert!(gui.1.is_some());
            assert_eq!(
                choose_audio(&sources, Audio::Mute).expect("explicit mute"),
                (None, None)
            );
        }
        assert_eq!(
            choose_audio(&[source(None)], Audio::RequireCompatible).expect("silent source"),
            (None, None)
        );
        assert_eq!(
            choose_audio(
                &[source(supported), source(supported)],
                Audio::RequireCompatible
            )
            .expect("compatible audio"),
            (
                Some(AudioSpec {
                    sample_rate: 48_000,
                    channels: 2
                }),
                None
            )
        );
    }

    #[test]
    fn staging_cleanup_keeps_previous_output_until_successful_atomic_replacement() {
        let dir = FixtureDir::new();
        let destination = dir.path("output.png");
        fs::write(&destination, b"old").expect("old output");
        let staging;
        {
            let output = Output::new(&destination, &[], "png").expect("staging");
            staging = output.staging.clone();
            fs::write(&staging, b"partial").expect("partial output");
            assert_eq!(fs::read(&destination).expect("existing output"), b"old");
        }
        assert!(!staging.exists());
        assert_eq!(fs::read(&destination).expect("existing output"), b"old");
        let output = Output::new(&destination, &[], "png").expect("staging");
        fs::write(&output.staging, b"final").expect("final output");
        output.publish().expect("atomic replace");
        assert_eq!(fs::read(destination).expect("replaced output"), b"final");
    }

    #[test]
    fn overlays_follow_half_open_output_times_and_blend_premultiplied_alpha() {
        let mut pm = Pixmap::new(2, 1).expect("overlay image");
        pm.fill(tiny_skia::Color::from_rgba8(0, 200, 0, 128));
        let overlay = LoadedOverlay {
            pm,
            x: -1,
            y: 0,
            from: 0.25,
            to: 0.75,
            fade: 0.0,
            fade_from: 0.25,
            fade_to: 0.75,
            segment: None,
        };
        for (at, expected) in [
            (0.249, [0, 0, 240, 0]),
            (0.25, [0, 100, 120, 0]),
            (0.749, [0, 100, 120, 0]),
            (0.75, [0, 0, 240, 0]),
        ] {
            let mut pixels = vec![0, 0, 240, 0, 0, 0, 240, 0];
            composite_bgra(&mut pixels, 2, 1, std::slice::from_ref(&overlay), at, None);
            assert_eq!(&pixels[..4], expected.as_slice(), "time {at}");
            assert_eq!(&pixels[4..], &[0, 0, 240, 0], "clipped image at {at}");
        }
    }

    #[test]
    fn allocates_target_frames_proportionally_to_speed_adjusted_durations() {
        let source_durations_and_speeds = [(2.0, 2.0), (3.0, 1.0)];
        let base_output_durations: Vec<f64> = source_durations_and_speeds
            .iter()
            .map(|(duration, speed)| duration / speed)
            .collect();

        let frames = allocate_frames(12, &base_output_durations).expect("frame allocation");

        assert_eq!(frames, [3, 9]);
        assert_eq!(frames.iter().sum::<u64>(), 12);
    }

    #[test]
    fn source_captions_map_through_trim_speed_join_and_duration_fit() {
        let caption = |from, to, segment| Caption {
            text: "caption".into(),
            x: 0,
            y: 0,
            from,
            to,
            timeline: CaptionTimeline::Source,
            segment: Some(segment),
            font: jiffshot_doc::text::DEFAULT_FAMILY.into(),
            size: 20.0,
            color: jiffshot_doc::Rgba::WHITE,
            background: jiffshot_doc::Rgba::new(0, 0, 0, 0),
            padding: 0,
            fade: 0.0,
            fade_window: None,
            reference_canvas: None,
        };
        let segments = [
            Segment {
                path: PathBuf::new(),
                from: 2.0,
                to: 6.0,
                speed: 2.0,
            },
            Segment {
                path: PathBuf::new(),
                from: 10.0,
                to: 14.0,
                speed: 1.0,
            },
        ];
        let captions = [caption(1.0, 3.0, 0), caption(11.0, 13.0, 1)];

        let mapped = map_mp4_captions(&captions, &segments, &[20, 40], 10, &[0])
            .expect("source caption mapping");

        assert_eq!(mapped.len(), 2);
        assert_eq!((mapped[0].from, mapped[0].to), (0.0, 0.5));
        assert_eq!((mapped[1].from, mapped[1].to), (3.0, 5.0));
        assert_eq!(mapped[0].segment, Some(0));
        assert_eq!(mapped[1].segment, Some(1));
        assert!(mapped
            .iter()
            .all(|caption| caption.timeline == CaptionTimeline::Output));

        let overlapped = map_mp4_captions(&captions, &segments, &[20, 40], 10, &[10])
            .expect("overlapped source caption mapping");
        assert_eq!((overlapped[1].from, overlapped[1].to), (2.0, 4.0));

        let gif = map_gif_captions(&[captions[0].clone()], 2.0, 6.0, 2.0)
            .expect("GIF source caption mapping");
        assert_eq!((gif[0].from, gif[0].to), (0.0, 0.5));

        let boundary = map_mp4_captions(&[caption(10.0, 14.0, 1)], &segments, &[1, 2], 12, &[0])
            .expect("caption at a fractional frame boundary");
        assert_eq!(boundary[0].from, seconds(frame_time(1, 12)));
    }

    #[test]
    fn source_caption_fades_follow_clipped_and_retimed_video() {
        let caption = Caption {
            text: "caption".into(),
            x: 0,
            y: 0,
            from: 0.0,
            to: 1.0,
            timeline: CaptionTimeline::Source,
            segment: Some(0),
            font: jiffshot_doc::text::DEFAULT_FAMILY.into(),
            size: 12.0,
            color: jiffshot_doc::Rgba::WHITE,
            background: jiffshot_doc::Rgba::rgb(0, 255, 0),
            padding: 2,
            fade: 0.4,
            fade_window: None,
            reference_canvas: None,
        };
        let segment = Segment {
            path: PathBuf::new(),
            from: 0.2,
            to: 0.8,
            speed: 2.0,
        };

        let mp4 = map_mp4_captions(std::slice::from_ref(&caption), &[segment], &[3], 10, &[])
            .expect("map trimmed MP4 caption");
        let gif = map_gif_captions(&[caption], 0.2, 0.8, 0.3).expect("map trimmed GIF caption");
        for (mapped, segment) in [(mp4, Some(0)), (gif, None)] {
            let overlays = load_overlays(&[], &mapped, 0.3, 64, 48).expect("render clipped fade");
            let pixel_at = |time| {
                let mut pixels = vec![0; 64 * 48 * 4];
                composite_bgra(&mut pixels, 64, 48, &overlays, time, segment);
                pixels[1]
            };
            assert!((120..=135).contains(&pixel_at(0.0)), "fade is halfway in");
            assert!(pixel_at(0.15) >= 250, "caption is fully visible");
            assert!((120..=135).contains(&pixel_at(0.299)), "fade continues out");
        }
    }

    #[test]
    fn source_caption_fade_progress_survives_internal_edit_boundaries() {
        let caption = Caption {
            text: "caption".into(),
            x: 0,
            y: 0,
            from: 0.0,
            to: 1.0,
            timeline: CaptionTimeline::Source,
            segment: Some(0),
            font: jiffshot_doc::text::DEFAULT_FAMILY.into(),
            size: 12.0,
            color: jiffshot_doc::Rgba::WHITE,
            background: jiffshot_doc::Rgba::rgb(0, 255, 0),
            padding: 2,
            fade: 0.4,
            fade_window: None,
            reference_canvas: None,
        };
        let segments =
            [(0.0, 0.2, 1.0), (0.2, 0.8, 2.0), (0.8, 1.0, 1.0)].map(|(from, to, speed)| Segment {
                path: PathBuf::new(),
                from,
                to,
                speed,
            });
        let captions = (0..segments.len())
            .map(|index| Caption {
                segment: Some(index),
                ..caption.clone()
            })
            .collect::<Vec<_>>();
        let mapped = map_mp4_captions(&captions, &segments, &[2, 3, 2], 10, &[0, 0])
            .expect("map split caption");
        let overlays = load_overlays(&[], &mapped, 0.7, 64, 48).expect("render split caption");
        let green_at = |time| {
            let mut pixels = vec![0; 64 * 48 * 4];
            let segment = if time < 0.2 {
                0
            } else if time < 0.5 {
                1
            } else {
                2
            };
            composite_bgra(&mut pixels, 64, 48, &overlays, time, Some(segment));
            pixels[1]
        };
        assert!((60..=68).contains(&green_at(0.1)));
        assert!((120..=135).contains(&green_at(0.2)));
        assert!((185..=195).contains(&green_at(0.25)));
        assert!(green_at(0.35) >= 250);
        assert!((120..=135).contains(&green_at(0.5)));
    }

    #[test]
    fn verify_rejects_a_gif_without_frames() {
        let dir = FixtureDir::new();
        let path = dir.path("empty.gif");
        {
            let mut file = fs::File::create(&path).expect("empty GIF file");
            let _encoder = gif::Encoder::new(&mut file, 1, 1, &[]).expect("GIF header");
        }
        assert!(
            verify(&path).is_err(),
            "a GIF with no decoded frames is not a verified video"
        );
    }

    #[test]
    fn verify_decodes_video_with_audio() {
        let dir = FixtureDir::new();
        let path = dir.path("video-with-audio.mp4");
        let audio = AudioSpec {
            sample_rate: 48_000,
            channels: 2,
        };
        let mut writer = Writer::create(&path, VideoSpec::for_size(64, 48, 12), Some(audio))
            .expect("audio and video writer");
        for frame in 0..12 {
            let at = frame * HNS_PER_SECOND / 12;
            let duration = (frame + 1) * HNS_PER_SECOND / 12 - at;
            writer
                .write_frame(&vec![0; 64 * 48 * 4], at, duration)
                .expect("video frame");
            writer
                .write_audio(&vec![1000; 4000 * 2], at, duration)
                .expect("audio block");
        }
        writer.finish().expect("finalize source");

        let metadata = verify(&path).expect("verify both streams");

        assert_eq!(metadata.frames, Some(12));
        assert_eq!(
            metadata
                .audio
                .map(|audio| (audio.sample_rate, audio.channels)),
            Some((48_000, 2))
        );
    }

    #[test]
    fn decoded_frame_timing_distinguishes_constant_and_variable_intervals() {
        let mut constant = [0, 333_333, 666_667].into_iter();
        assert_eq!(
            decoded_frame_timing(|| Ok(constant.next()), 30.0).unwrap(),
            (3, true)
        );
        let mut variable = [0, 333_333, 1_000_000].into_iter();
        assert_eq!(
            decoded_frame_timing(|| Ok(variable.next()), 30.0).unwrap(),
            (3, false)
        );
        let mut invalid_fps = [0, 333_333].into_iter();
        assert_eq!(
            decoded_frame_timing(|| Ok(invalid_fps.next()), 0.0).unwrap(),
            (2, false)
        );
    }

    #[test]
    fn decoded_frame_timing_rejects_empty_video_and_reports_late_decoder_errors() {
        assert!(decoded_frame_timing(|| Ok(None), 30.0)
            .unwrap_err()
            .to_string()
            .contains("no frames"));
        let mut calls = 0;
        let error = decoded_frame_timing(
            || {
                calls += 1;
                match calls {
                    1 => Ok(Some(0)),
                    2 => Ok(Some(333_333)),
                    _ => Err("corrupt video".into()),
                }
            },
            30.0,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("frame 2"), "{error}");
        assert!(error.contains("corrupt video"), "{error}");
    }

    #[test]
    fn exact_inspection_reports_the_encoded_video_frame_count_and_rate() {
        let dir = FixtureDir::new();
        let source = dir.path("exact-info.mp4");
        let mut writer =
            Writer::create(&source, VideoSpec::for_size(64, 48, 12), None).expect("encoder");
        let pixels = [0, 0, 240, 0].repeat(64 * 48);
        for frame in 0..12 {
            let at = frame_time(frame, 12);
            let end = frame_time(frame + 1, 12);
            writer
                .write_frame(&pixels, at, end - at)
                .expect("source frame");
        }
        writer.finish().expect("finalize source");

        assert_eq!(inspect(&source).unwrap().frames, None);
        let exact = inspect_with_frame_timing(&source).expect("decode frame timing");
        assert_eq!(exact.frames, Some(12));
        assert_eq!(exact.constant_frame_rate, Some(true));
        assert_eq!(exact.width, 64);
        assert_eq!(exact.height, 48);
    }

    #[test]
    fn verify_audio_reports_errors_after_previous_blocks() {
        let mut calls = 0;
        let error = verify_audio_blocks(|| {
            calls += 1;
            if calls <= 2 {
                Ok(Some((0, vec![1, 2])))
            } else {
                Err("corrupt audio".into())
            }
        })
        .expect_err("late audio failure");

        assert_eq!(calls, 3);
        assert!(error.to_string().contains("audio block 2: corrupt audio"));
    }

    #[test]
    fn rejects_frame_allocations_without_valid_segment_capacity() {
        for (total, durations) in [
            (0, &[][..]),
            (1, &[1.0, 1.0][..]),
            (2, &[1.0, 0.0][..]),
            (2, &[1.0, f64::INFINITY][..]),
            (2, &[1.0, f64::NAN][..]),
        ] {
            assert!(
                allocate_frames(total, durations).is_err(),
                "expected invalid allocation for {total} frames and {durations:?}"
            );
        }
    }

    #[test]
    fn palette_quantization_respects_the_requested_color_budget() {
        let mut pixels = image::RgbaImage::new(32, 32);
        for (x, y, pixel) in pixels.enumerate_pixels_mut() {
            *pixel = image::Rgba([x as u8 * 8, y as u8 * 8, (x ^ y) as u8 * 8, 255]);
        }

        quantize_palette(&mut pixels, 16);

        let colors: std::collections::HashSet<[u8; 3]> = pixels
            .pixels()
            .map(|pixel| [pixel.0[0], pixel.0[1], pixel.0[2]])
            .collect();
        assert!(colors.len() <= 16, "got {} colors", colors.len());
        assert!(pixels.pixels().all(|pixel| pixel.0[3] == 255));
    }

    #[test]
    fn floyd_steinberg_dither_handles_white_edges_without_wrapping_alpha() {
        let mut pixels = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 255, 255, 0]));

        floyd_steinberg_dither(&mut pixels);

        assert!(pixels.pixels().all(|pixel| pixel.0[3] == 0));
        assert!(pixels
            .pixels()
            .all(|pixel| pixel.0[..3].iter().all(|channel| *channel == 224)));
    }

    #[test]
    fn segment_speed_maps_output_time_to_the_corresponding_source_frame() {
        let dir = FixtureDir::new();
        let source = dir.path("speed-source.mp4");
        let mut writer =
            Writer::create(&source, VideoSpec::for_size(64, 48, 12), None).expect("source encoder");
        for frame_index in 0..12 {
            let at = frame_index * HNS_PER_SECOND / 12;
            let finish = (frame_index + 1) * HNS_PER_SECOND / 12;
            let bgra = if frame_index < 6 {
                [240, 0, 0, 0]
            } else {
                [0, 0, 240, 0]
            };
            writer
                .write_frame(&bgra.repeat(64 * 48), at, finish - at)
                .expect("source frame");
        }
        writer.finish().expect("finalized source");

        let output = dir.path("speed-output.mp4");
        let metadata = export_mp4(
            &[Segment {
                path: source.clone(),
                from: 0.0,
                to: 1.0,
                speed: 2.0,
            }],
            &output,
            Mp4Options {
                fps: Some(12),
                ..Mp4Options::default()
            },
            |_| {},
        )
        .expect("speed-adjusted output");

        assert_eq!(metadata.frames, Some(6));
        assert!((metadata.duration - 0.5).abs() < 0.02);

        let mut source_reader = Reader::open(&source).expect("source reader");
        let source_at_quarter = source_reader
            .frame_at(HNS_PER_SECOND / 4)
            .expect("source frame decode")
            .expect("source frame at 0.25 seconds");
        let source_at_half_speed = source_reader
            .frame_at(HNS_PER_SECOND / 2)
            .expect("source frame decode")
            .expect("source frame at 0.5 seconds");
        let mut output_reader = Reader::open(&output).expect("output reader");
        let output_at_quarter_second = output_reader
            .frame_at(HNS_PER_SECOND / 4)
            .expect("output frame decode")
            .expect("output frame at 0.25 seconds");
        let center_pixel = ((24 * 64 + 32) * 4) as usize;

        assert!(
            source_at_quarter.bgra[center_pixel] > source_at_quarter.bgra[center_pixel + 2] + 80,
            "unscaled source at 0.25 seconds should still be in the blue segment"
        );
        assert!(
            source_at_half_speed.bgra[center_pixel + 2]
                > source_at_half_speed.bgra[center_pixel] + 80,
            "source at 0.5 seconds should be in the red segment"
        );
        assert!(
            output_at_quarter_second.bgra[center_pixel + 2]
                > output_at_quarter_second.bgra[center_pixel] + 80,
            "2x output at 0.25 seconds should show source time 0.5 seconds"
        );
    }

    #[test]
    fn idle_detection_uses_repeated_frames_and_excludes_recorded_activity() {
        let dir = FixtureDir::new();
        let source = dir.path("idle-source.mp4");
        let mut writer =
            Writer::create(&source, VideoSpec::for_size(64, 48, 12), None).expect("encoder");
        for frame_index in 0..36 {
            let at = frame_index * HNS_PER_SECOND / 12;
            let finish = (frame_index + 1) * HNS_PER_SECOND / 12;
            let bgra = if frame_index < 6 {
                [30, 30, 30, 0]
            } else if frame_index < 30 {
                [120, 120, 120, 0]
            } else {
                [220, 220, 220, 0]
            };
            writer
                .write_frame(&bgra.repeat(64 * 48), at, finish - at)
                .expect("source frame");
        }
        writer.finish().expect("finalize source");

        let without_activity =
            idle_ranges(&source, 0.0, 3.0, 0.4, &[]).expect("decode idle intervals");
        assert!(
            without_activity
                .iter()
                .any(|(from, to)| *from <= 1.5 && *to >= 1.5),
            "the middle interval is idle without input: {without_activity:?}"
        );
        let idle = idle_ranges(&source, 0.0, 3.0, 0.4, &[1_500]).expect("decode idle intervals");

        assert!(
            idle.len() >= 2,
            "activity splits stable intervals: {idle:?}"
        );
        assert!(
            idle[0].0 <= 0.1 && (idle[0].1 - 0.5).abs() <= 0.1,
            "{idle:?}"
        );
        assert!(
            idle.iter().all(|(from, to)| *to <= 1.25 || *from >= 1.75),
            "{idle:?}"
        );
        assert!(
            idle.last().expect("final idle interval").1 >= 2.8,
            "{idle:?}"
        );
    }

    #[test]
    fn idle_detection_splits_long_still_spans_around_input_grace_periods() {
        let mut idle = Vec::new();
        collect_idle_range(&mut idle, 0.0, 4.0, 0.0, 4.0, 0.5, &[1_000, 1_100, 3_000]);

        assert_eq!(idle.len(), 3, "{idle:?}");
        for ((start, end), (expected_start, expected_end)) in
            idle.iter().zip([(0.0, 0.75), (1.35, 2.75), (3.25, 4.0)])
        {
            assert!((start - expected_start).abs() < 0.000_001, "{idle:?}");
            assert!((end - expected_end).abs() < 0.000_001, "{idle:?}");
        }
    }

    #[test]
    fn crossfade_overlaps_moving_segments_and_shortens_the_joined_duration() {
        let dir = FixtureDir::new();
        let make_source = |name: &str, bgra: [u8; 4]| {
            let path = dir.path(name);
            let mut writer =
                Writer::create(&path, VideoSpec::for_size(64, 48, 12), None).expect("encoder");
            for frame_index in 0..12 {
                let at = frame_time(frame_index, 12);
                let finish = frame_time(frame_index + 1, 12);
                let mut pixel = bgra;
                if pixel[2] > 0 {
                    pixel[1] = frame_index as u8 * 10;
                }
                writer
                    .write_frame(&pixel.repeat(64 * 48), at, finish - at)
                    .expect("source frame");
            }
            writer.finish().expect("finalize source");
            path
        };
        let red = make_source("red.mp4", [0, 0, 240, 0]);
        let blue = make_source("blue.mp4", [240, 0, 0, 0]);
        let segments = [
            Segment {
                path: red,
                from: 0.0,
                to: 1.0,
                speed: 1.0,
            },
            Segment {
                path: blue,
                from: 0.0,
                to: 1.0,
                speed: 1.0,
            },
        ];
        let invalid_output = dir.path("invalid-transition.mp4");
        assert!(export_mp4(
            &segments,
            &invalid_output,
            Mp4Options {
                transition_seconds: Some(0.0),
                ..Mp4Options::default()
            },
            |_| {}
        )
        .is_err());
        assert!(
            !invalid_output.exists(),
            "invalid transition creates no output"
        );

        let output = dir.path("crossfade.mp4");
        let metadata = export_mp4(
            &segments,
            &output,
            Mp4Options {
                fps: Some(12),
                transition_seconds: Some(0.5),
                ..Mp4Options::default()
            },
            |_| {},
        )
        .expect("crossfade output");
        assert_eq!(metadata.frames, Some(18));
        assert!((metadata.duration - 1.5).abs() < 0.02);

        let mut reader = Reader::open(&output).expect("crossfade reader");
        let mixed = reader
            .frame_at(HNS_PER_SECOND / 2)
            .expect("decode transition")
            .expect("transition frame");
        let settled = reader
            .frame_at(HNS_PER_SECOND * 5 / 4)
            .expect("decode settled frame")
            .expect("settled frame");
        let center = ((24 * 64 + 32) * 4) as usize;
        assert!(
            mixed.bgra[center] > 20 && mixed.bgra[center + 2] > mixed.bgra[center],
            "transition starts with both sources, favoring the outgoing frame"
        );
        assert!(
            settled.bgra[center] > settled.bgra[center + 2] + 80,
            "incoming source is fully visible after the transition"
        );
        assert!(
            (30..75).contains(&mixed.bgra[center + 1])
                && mixed.bgra[center + 1] > settled.bgra[center + 1] + 10,
            "outgoing frames advance during the transition instead of freezing"
        );

        let fitted_output = dir.path("crossfade-fitted.mp4");
        let fitted = export_mp4(
            &segments,
            &fitted_output,
            Mp4Options {
                fps: Some(12),
                duration: Some(1.0),
                transition_seconds: Some(0.5),
                ..Mp4Options::default()
            },
            |_| {},
        )
        .expect("duration-fitted crossfade");
        assert_eq!(fitted.frames, Some(12));
        assert!((fitted.duration - 1.0).abs() < 0.02);
    }

    #[test]
    fn idle_detection_rejects_invalid_ranges_and_thresholds() {
        let source = PathBuf::from("unused.mp4");
        for (from, to, threshold) in [
            (-1.0, 1.0, 0.5),
            (1.0, 1.0, 0.5),
            (0.0, 1.0, 0.0),
            (0.0, 1.0, f64::NAN),
        ] {
            assert!(
                idle_ranges(&source, from, to, threshold, &[]).is_err(),
                "{from}..{to}, threshold {threshold}"
            );
        }
    }

    #[test]
    fn target_duration_rounds_frame_count_and_emits_cfr_timestamps() {
        let dir = FixtureDir::new();
        let source = dir.path("duration-source.mp4");
        let mut writer =
            Writer::create(&source, VideoSpec::for_size(64, 48, 12), None).expect("source encoder");
        let bgra = [0_u8, 0, 200, 0].repeat(64 * 48);
        for frame_index in 0..12 {
            let at = frame_time(frame_index, 12);
            let finish = frame_time(frame_index + 1, 12);
            writer
                .write_frame(&bgra, at, finish - at)
                .expect("source frame");
        }
        writer.finish().expect("finalized source");

        let output = dir.path("duration-output.mp4");
        let metadata = export_mp4(
            &[Segment {
                path: source,
                from: 0.0,
                to: 1.0,
                speed: 1.0,
            }],
            &output,
            Mp4Options {
                fps: Some(12),
                duration: Some(0.625),
                ..Mp4Options::default()
            },
            |_| {},
        )
        .expect("target-duration output");

        assert_eq!(metadata.frames, Some(8), "7.5 requested frames round to 8");
        assert!(
            (metadata.duration - 8.0 / 12.0).abs() < 0.02,
            "encoded duration should equal the rounded frame count divided by fps"
        );

        let mut reader = Reader::open(&output).expect("output reader");
        let mut timestamps = Vec::new();
        while let Some(frame) = reader.next_video().expect("decoded output frame") {
            timestamps.push(frame.time);
        }
        assert_eq!(timestamps.len(), 8, "all requested CFR frames are present");
        let expected: Vec<i64> = (0..8).map(|index| frame_time(index, 12)).collect();
        assert_eq!(
            timestamps, expected,
            "timestamps follow the 12 fps CFR grid"
        );
    }

    #[test]
    fn captions_render_configured_text_background_padding_position_and_fade() {
        let caption = Caption {
            text: "A".into(),
            x: 8,
            y: 7,
            from: 0.25,
            to: 0.75,
            timeline: CaptionTimeline::Output,
            segment: None,
            font: jiffshot_doc::text::DEFAULT_FAMILY.into(),
            size: 24.0,
            color: jiffshot_doc::Rgba::WHITE,
            background: jiffshot_doc::Rgba::rgb(0, 255, 0),
            padding: 4,
            fade: 0.25,
            fade_window: None,
            reference_canvas: None,
        };
        let overlays =
            load_overlays(&[], &[caption], 1.0, 64, 48).expect("configured text caption renders");
        let rendered = &overlays[0].pm;
        assert!(
            rendered
                .pixels()
                .iter()
                .any(|pixel| { pixel.red() > 200 && pixel.green() > 200 && pixel.blue() > 200 }),
            "caption should contain visible white text"
        );
        assert!(
            rendered.width() > 8 && rendered.height() > 8,
            "font size and padding should produce a nontrivial caption raster"
        );

        let pixel_at = |x: usize, y: usize, time| {
            let mut canvas = vec![0_u8; 64 * 48 * 4];
            composite_bgra(&mut canvas, 64, 48, &overlays, time, None);
            let index = (y * 64 + x) * 4;
            [canvas[index], canvas[index + 1], canvas[index + 2]]
        };
        assert_eq!(
            pixel_at(9, 8, 0.25),
            [0, 0, 0],
            "caption is invisible at fade-in start"
        );
        assert_eq!(
            pixel_at(9, 8, 0.375),
            [0, 128, 0],
            "caption is half faded in"
        );
        assert_eq!(
            pixel_at(9, 8, 0.5),
            [0, 255, 0],
            "caption background is visible at its configured position"
        );
        assert_eq!(
            pixel_at(9, 8, 0.625),
            [0, 128, 0],
            "caption is half faded out"
        );
        assert_eq!(
            pixel_at(9, 8, 0.75),
            [0, 0, 0],
            "caption interval excludes its end"
        );
        assert_eq!(
            pixel_at(9, 8, 0.2),
            [0, 0, 0],
            "caption is absent before its interval"
        );
        assert_eq!(
            pixel_at(7, 7, 0.5),
            [0, 0, 0],
            "caption position leaves pixels before its configured origin unchanged"
        );
        assert_eq!(
            pixel_at(8, 7, 0.5),
            [0, 255, 0],
            "caption background starts at its configured origin"
        );
    }

    #[test]
    fn caption_pixels_change_only_during_its_half_open_output_interval() {
        let caption = Caption {
            text: "A".into(),
            x: 8,
            y: 7,
            from: 0.25,
            to: 0.75,
            timeline: CaptionTimeline::Output,
            segment: None,
            font: jiffshot_doc::text::DEFAULT_FAMILY.into(),
            size: 24.0,
            color: jiffshot_doc::Rgba::WHITE,
            background: jiffshot_doc::Rgba::rgb(0, 255, 0),
            padding: 4,
            fade: 0.0,
            fade_window: None,
            reference_canvas: None,
        };
        let overlays =
            load_overlays(&[], &[caption], 1.0, 64, 48).expect("configured text caption renders");
        let baseline = vec![0_u8; 64 * 48 * 4];
        let rendered_at = |time| {
            let mut canvas = baseline.clone();
            composite_bgra(&mut canvas, 64, 48, &overlays, time, None);
            canvas
        };

        assert_eq!(rendered_at(0.249), baseline, "no caption before its start");
        assert_ne!(
            rendered_at(0.25),
            baseline,
            "caption is visible at its start"
        );
        assert_ne!(
            rendered_at(0.5),
            baseline,
            "caption remains visible inside its interval"
        );
        assert_eq!(
            rendered_at(0.75),
            baseline,
            "caption is excluded at its end"
        );
        assert_eq!(
            rendered_at(0.751),
            baseline,
            "no caption after its interval"
        );
    }
}
