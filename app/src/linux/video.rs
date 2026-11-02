//! Video frame extraction for importing Linux video files into the editor.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use image::codecs::gif::{GifEncoder, Repeat};
use image::{Delay, Frame, RgbaImage};
use jiffshot_doc::{Document, Pixmap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

const FRAME_TIMEOUT: gst::ClockTime = gst::ClockTime::from_seconds(15);
const VIDEO_DECODE_QUEUE_LIMIT: u32 = 16;
const INITIAL_SEEK_BACKOFF_NS: u64 = 1_000_000_000;
const VIDEO_EXPORT_FPS: u32 = 30;
const VIDEO_QUEUE_LIMIT: u64 = 16_777_216;
const AUDIO_EXPORT_RATE: u64 = 48_000;
const AUDIO_FRAME_BYTES: usize = 4;
const AUDIO_CHUNK_FRAMES: usize = 4_800;
const AUDIO_QUEUE_LIMIT: u64 = 1_048_576;
// GIF delays are stored in centiseconds.
const GIF_TICK_NS: u64 = 10_000_000;

pub fn is_video_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("avi" | "mkv" | "mov" | "mp4" | "webm")
    )
}

pub fn first_frame(path: &Path) -> Result<Document, String> {
    gst::init().map_err(|error| format!("could not initialize GStreamer: {error}"))?;
    let path = canonical_path(path)?;
    let (player, sink) = player(&path, false, false)?;

    let frame = player
        .set_state(gst::State::Paused)
        .map_err(|error| format!("could not start decoding {}: {error}", path.display()))
        .and_then(|_| {
            sink.try_pull_preroll(FRAME_TIMEOUT)
                .ok_or_else(|| format!("timed out decoding a frame from {}", path.display()))
        })
        .and_then(|sample| document_from_sample(&sample));

    let stop = player
        .set_state(gst::State::Null)
        .map_err(|error| format!("could not stop video decoder: {error}"));
    finish_decoder(frame, stop)
}

pub fn frame_at(path: &Path, time_seconds: f64) -> Result<Document, String> {
    if !time_seconds.is_finite()
        || time_seconds < 0.0
        || time_seconds > u64::MAX as f64 / 1_000_000_000.0
    {
        return Err("video frame time must be finite and non-negative".to_owned());
    }

    gst::init().map_err(|error| format!("could not initialize GStreamer: {error}"))?;
    let path = canonical_path(path)?;
    let (player, sink) = player(&path, false, false)?;
    let time = gst::ClockTime::from_nseconds((time_seconds * 1_000_000_000.0) as u64);

    let frame = player
        .set_state(gst::State::Paused)
        .map_err(|error| format!("could not start decoding {}: {error}", path.display()))
        .and_then(|_| {
            sink.try_pull_preroll(FRAME_TIMEOUT)
                .ok_or_else(|| format!("timed out preparing video {}", path.display()))
        })
        .and_then(|_| {
            seek_decodable_video(&player, &sink, time).map_err(|error| {
                format!("could not decode frame at {time_seconds} seconds: {error}")
            })
        })
        .and_then(|sample| document_from_sample(&sample));

    let stop = player
        .set_state(gst::State::Null)
        .map_err(|error| format!("could not stop video decoder: {error}"));
    finish_decoder(frame, stop)
}

fn seek_decodable_video(
    decoder: &gst::Element,
    sink: &gst_app::AppSink,
    target: gst::ClockTime,
) -> Result<gst::Sample, String> {
    let bus = decoder
        .bus()
        .ok_or_else(|| "video decoder has no event bus".to_owned())?;
    let mut backoff_ns = 0u64;
    loop {
        let anchor = gst::ClockTime::from_nseconds(target.nseconds().saturating_sub(backoff_ns));
        decoder
            .set_state(gst::State::Paused)
            .map_err(|error| format!("could not pause video decoder before seeking: {error}"))?;
        decoder
            .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, anchor)
            .map_err(|error| format!("could not seek to {anchor}: {error}"))?;
        let preroll = sink
            .try_pull_preroll(FRAME_TIMEOUT)
            .ok_or_else(|| "timed out preparing the selected video position".to_owned())?;
        let damaged = video_sample_is_corrupted(&preroll)?;
        if !damaged {
            decoder.set_state(gst::State::Playing).map_err(|error| {
                format!("could not decode the selected video position: {error}")
            })?;
            loop {
                let Some(sample) = sink.try_pull_sample(FRAME_TIMEOUT) else {
                    if decoder_finished(&bus)? || sink.is_eos() {
                        return Err(format!("video contains no frame at {target}"));
                    }
                    return Err("timed out decoding the selected video position".to_owned());
                };
                let buffer = sample
                    .buffer()
                    .ok_or_else(|| "video decoder returned an empty frame buffer".to_owned())?;
                if buffer.flags().contains(gst::BufferFlags::CORRUPTED) {
                    break;
                }
                let pts = buffer.pts().ok_or_else(|| {
                    "video decoder returned a frame without a timestamp".to_owned()
                })?;
                let covers_target = buffer.duration().is_some_and(|duration| {
                    pts.nseconds().saturating_add(duration.nseconds()) > target.nseconds()
                });
                if pts >= target || covers_target {
                    return Ok(sample);
                }
            }
        }
        if anchor == gst::ClockTime::ZERO {
            return Err(format!("video frame at {target} is corrupted"));
        }
        // Some containers advertise non-IDR H.264 frames as seek points. Retry
        // from progressively earlier positions when the decoder reports damage.
        backoff_ns = if backoff_ns == 0 {
            INITIAL_SEEK_BACKOFF_NS
        } else {
            backoff_ns.saturating_mul(2)
        };
    }
}

fn video_sample_is_corrupted(sample: &gst::Sample) -> Result<bool, String> {
    let buffer = sample
        .buffer()
        .ok_or_else(|| "video decoder returned an empty frame buffer".to_owned())?;
    Ok(buffer.flags().contains(gst::BufferFlags::CORRUPTED))
}

pub fn play(
    path: &Path,
    stop: &AtomicBool,
    paused: &AtomicBool,
    show_frame: impl FnMut(Document),
) -> Result<(), String> {
    play_with_audio(path, stop, paused, true, show_frame)
}

fn play_with_audio(
    path: &Path,
    stop: &AtomicBool,
    paused: &AtomicBool,
    play_audio: bool,
    mut show_frame: impl FnMut(Document),
) -> Result<(), String> {
    gst::init().map_err(|error| format!("could not initialize GStreamer: {error}"))?;
    let path = canonical_path(path)?;
    let (player, sink) = player(&path, true, play_audio)?;
    let bus = player
        .bus()
        .ok_or_else(|| "video decoder has no event bus".to_owned())?;
    let playback = player
        .set_state(gst::State::Playing)
        .map_err(|error| format!("could not start playing {}: {error}", path.display()));
    let result = playback.and_then(|_| loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let state = if paused.load(Ordering::Acquire) {
            gst::State::Paused
        } else {
            gst::State::Playing
        };
        if player.current_state() != state {
            player
                .set_state(state)
                .map_err(|error| format!("could not change video playback state: {error}"))?;
        }
        if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
            show_frame(document_from_sample(&sample)?);
        }
        if let Some(message) = bus.timed_pop_filtered(
            gst::ClockTime::ZERO,
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) {
            return match message.view() {
                gst::MessageView::Eos(_) => Ok(()),
                gst::MessageView::Error(error) => {
                    Err(format!("video playback failed: {}", error.error()))
                }
                _ => Err("video playback ended unexpectedly".to_owned()),
            };
        }
        if player
            .query_duration::<gst::ClockTime>()
            .zip(player.query_position::<gst::ClockTime>())
            .is_some_and(|(duration, position)| position >= duration)
        {
            return Ok(());
        }
    });
    let stopped = player
        .set_state(gst::State::Null)
        .map_err(|error| format!("could not stop video playback: {error}"));
    finish_decoder(result, stopped)
}

pub fn trim_video(
    source: &Path,
    destination: &Path,
    start_seconds: f64,
    end_seconds: f64,
) -> Result<(), String> {
    if !start_seconds.is_finite()
        || !end_seconds.is_finite()
        || start_seconds < 0.0
        || end_seconds <= start_seconds
        || end_seconds > u64::MAX as f64 / 1_000_000_000.0
    {
        return Err(
            "video trim range must be finite, non-negative, and end after start".to_owned(),
        );
    }

    gst::init().map_err(|error| format!("could not initialize GStreamer: {error}"))?;
    let source = canonical_path(source)?;
    validate_edit_destination(&source, destination)?;
    let start = gst::ClockTime::from_nseconds((start_seconds * 1_000_000_000.0) as u64);
    let end = gst::ClockTime::from_nseconds((end_seconds * 1_000_000_000.0) as u64);
    let duration = video_duration(&source)?;
    if end > duration {
        return Err(format!(
            "video trim end ({end_seconds} seconds) exceeds the video duration ({:.3} seconds)",
            duration.nseconds() as f64 / 1_000_000_000.0
        ));
    }
    let staging = edit_staging_directory(destination)?;
    let video_path = staging.path().join("video.mp4");
    let (decoder, sink) = player(&source, false, false)?;
    let result = trim_video_pipeline(&decoder, &sink, &video_path, start, end);
    let stopped = decoder
        .set_state(gst::State::Null)
        .map_err(|error| format!("could not stop video decoder: {error}"));
    finish_decoder(result, stopped)?;
    finish_video_edit(
        &video_path,
        &staging.path().join("edited.mp4"),
        destination,
        &[VideoRange {
            source: &source,
            start,
            end,
        }],
    )
}

pub fn cut_video(
    source: &Path,
    destination: &Path,
    cut_start_seconds: f64,
    cut_end_seconds: f64,
) -> Result<(), String> {
    if !cut_start_seconds.is_finite()
        || !cut_end_seconds.is_finite()
        || cut_start_seconds < 0.0
        || cut_end_seconds <= cut_start_seconds
        || cut_end_seconds > u64::MAX as f64 / 1_000_000_000.0
    {
        return Err("video cut range must be finite, non-negative, and end after start".to_owned());
    }

    gst::init().map_err(|error| format!("could not initialize GStreamer: {error}"))?;
    let source = canonical_path(source)?;
    validate_edit_destination(&source, destination)?;
    let duration = video_duration(&source)?;
    let duration_seconds = duration.nseconds() as f64 / 1_000_000_000.0;
    if cut_end_seconds > duration_seconds {
        return Err(format!(
            "video cut end ({cut_end_seconds} seconds) exceeds the video duration ({duration_seconds:.3} seconds)"
        ));
    }
    if cut_start_seconds <= 0.0 && cut_end_seconds >= duration_seconds {
        return Err("the cut range cannot remove the entire video".to_owned());
    }

    let cut_start = gst::ClockTime::from_nseconds((cut_start_seconds * 1_000_000_000.0) as u64);
    let cut_end = gst::ClockTime::from_nseconds((cut_end_seconds * 1_000_000_000.0) as u64);
    let mut ranges = Vec::with_capacity(2);
    if cut_start_seconds > 0.0 {
        ranges.push(VideoRange {
            source: &source,
            start: gst::ClockTime::ZERO,
            end: cut_start,
        });
    }
    if cut_end_seconds < duration_seconds {
        ranges.push(VideoRange {
            source: &source,
            start: cut_end,
            end: duration,
        });
    }
    let staging = edit_staging_directory(destination)?;
    let video_path = staging.path().join("video.mp4");
    export_video_ranges(&video_path, &ranges, None)?;
    finish_video_edit(
        &video_path,
        &staging.path().join("edited.mp4"),
        destination,
        &ranges,
    )
}

pub fn combine_videos(sources: &[PathBuf], destination: &Path) -> Result<(), String> {
    if sources.len() < 2 {
        return Err("select at least two video files to combine".to_owned());
    }

    gst::init().map_err(|error| format!("could not initialize GStreamer: {error}"))?;
    let sources = sources
        .iter()
        .map(|source| {
            let source = canonical_path(source)?;
            validate_edit_destination(&source, destination)?;
            Ok(source)
        })
        .collect::<Result<Vec<_>, String>>()?;
    let dimensions = first_frame(&sources[0])?.size();
    let durations = sources
        .iter()
        .map(|source| {
            let duration = video_duration(source)?;
            if duration == gst::ClockTime::ZERO {
                return Err(format!("video {} has no duration", source.display()));
            }
            Ok(duration)
        })
        .collect::<Result<Vec<_>, String>>()?;
    let ranges = sources
        .iter()
        .zip(durations)
        .map(|(source, end)| VideoRange {
            source,
            start: gst::ClockTime::ZERO,
            end,
        })
        .collect::<Vec<_>>();
    let staging = edit_staging_directory(destination)?;
    let video_path = staging.path().join("video.mp4");
    export_video_ranges(&video_path, &ranges, Some(dimensions))?;
    finish_video_edit(
        &video_path,
        &staging.path().join("edited.mp4"),
        destination,
        &ranges,
    )
}

const SLIDESHOW_MAX_WIDTH: u32 = 1920;
const SLIDESHOW_MAX_HEIGHT: u32 = 1080;
pub const SLIDESHOW_MIN_SECONDS: f64 = 0.5;
pub const SLIDESHOW_MAX_SECONDS: f64 = 60.0;

/// The slideshow frame size: the first image's size, shrunk to fit 1080p, with even sides.
fn slideshow_dimensions(width: u32, height: u32) -> (u32, u32) {
    let scale = (f64::from(SLIDESHOW_MAX_WIDTH) / f64::from(width.max(1)))
        .min(f64::from(SLIDESHOW_MAX_HEIGHT) / f64::from(height.max(1)))
        .min(1.0);
    let even = |value: f64| (((value * scale).round() as u32).max(2) + 1) & !1;
    (even(f64::from(width)), even(f64::from(height)))
}

/// Scales `image` to fit a `width` x `height` black frame, centered, as RGBA bytes.
fn slideshow_frame(image: &RgbaImage, width: u32, height: u32) -> Vec<u8> {
    let scale = (f64::from(width) / f64::from(image.width().max(1)))
        .min(f64::from(height) / f64::from(image.height().max(1)));
    let fitted_width = ((f64::from(image.width()) * scale).round() as u32).clamp(1, width);
    let fitted_height = ((f64::from(image.height()) * scale).round() as u32).clamp(1, height);
    let fitted = image::imageops::resize(
        image,
        fitted_width,
        fitted_height,
        image::imageops::FilterType::Triangle,
    );
    let mut canvas = RgbaImage::from_pixel(width, height, image::Rgba([0, 0, 0, 255]));
    let (left, top) = ((width - fitted_width) / 2, (height - fitted_height) / 2);
    for (x, y, pixel) in fitted.enumerate_pixels() {
        let alpha = u32::from(pixel[3]);
        let blend = |channel: u8| ((u32::from(channel) * alpha + 127) / 255) as u8;
        canvas.put_pixel(
            left + x,
            top + y,
            image::Rgba([blend(pixel[0]), blend(pixel[1]), blend(pixel[2]), 255]),
        );
    }
    canvas.into_raw()
}

/// Writes a silent MP4 that shows each image for `seconds_per_slide` seconds, in order.
pub fn create_slideshow(
    images: &[PathBuf],
    seconds_per_slide: f64,
    destination: &Path,
) -> Result<(), String> {
    if images.is_empty() {
        return Err("select at least one image for the slideshow".to_owned());
    }
    if !(SLIDESHOW_MIN_SECONDS..=SLIDESHOW_MAX_SECONDS).contains(&seconds_per_slide) {
        return Err(format!(
            "slide duration must be between {SLIDESHOW_MIN_SECONDS} and {SLIDESHOW_MAX_SECONDS} seconds"
        ));
    }
    gst::init().map_err(|error| format!("could not initialize GStreamer: {error}"))?;
    for image in images {
        validate_edit_destination(image, destination)?;
    }
    let decoded = images
        .iter()
        .map(|path| {
            jiffshot_doc::io::load_image(path)
                .map(|pixmap| jiffshot_doc::io::to_rgba_image(&pixmap))
                .map_err(|error| format!("could not open {}: {error}", path.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let (width, height) = slideshow_dimensions(decoded[0].width(), decoded[0].height());
    let staging = edit_staging_directory(destination)?;
    let video_path = staging.path().join("slideshow.mp4");

    let pipeline = gst::parse::launch(
        "appsrc name=slides format=time block=true \
         ! videoconvert ! videorate ! video/x-raw,framerate=30/1 \
         ! x264enc tune=zerolatency ! h264parse ! mp4mux ! filesink name=output",
    )
    .map_err(|error| format!("could not create slideshow pipeline: {error}"))?
    .downcast::<gst::Pipeline>()
    .map_err(|_| "the slideshow pipeline is not a pipeline".to_owned())?;
    pipeline
        .by_name("output")
        .ok_or_else(|| "slideshow pipeline has no output".to_owned())?
        .set_property("location", video_path.to_string_lossy().as_ref());
    let source = pipeline
        .by_name("slides")
        .ok_or_else(|| "slideshow pipeline has no source".to_owned())?
        .downcast::<gst_app::AppSrc>()
        .map_err(|_| "slideshow source is not an appsrc".to_owned())?;
    source.set_caps(Some(
        &gst::Caps::builder("video/x-raw")
            .field("format", "RGBA")
            .field("width", width as i32)
            .field("height", height as i32)
            .field("framerate", gst::Fraction::new(1, 1))
            .build(),
    ));
    let bus = pipeline
        .bus()
        .ok_or_else(|| "slideshow pipeline has no bus".to_owned())?;
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|error| format!("could not start slideshow export: {error}"))?;
    let slide = gst::ClockTime::from_nseconds((seconds_per_slide * 1e9) as u64);
    let pushed = (|| {
        for (index, image) in decoded.iter().enumerate() {
            let mut buffer = gst::Buffer::from_mut_slice(slideshow_frame(image, width, height));
            {
                let buffer = buffer.get_mut().expect("a new buffer is writable");
                buffer.set_pts(slide * index as u64);
                buffer.set_duration(slide);
            }
            source
                .push_buffer(buffer)
                .map_err(|error| format!("could not encode slide {}: {error}", index + 1))?;
        }
        source
            .end_of_stream()
            .map(|_| ())
            .map_err(|error| format!("could not finish the slideshow: {error}"))
    })();
    let outcome = pushed.and_then(|()| {
        bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(300),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .ok_or_else(|| "timed out while encoding the slideshow".to_owned())
        .and_then(|message| match message.view() {
            gst::MessageView::Error(error) => {
                Err(format!("slideshow export failed: {}", error.error()))
            }
            _ => Ok(()),
        })
    });
    let _ = pipeline.set_state(gst::State::Null);
    outcome?;
    save_video_edit(&video_path, destination)
}

pub fn export_silent_video(source: &Path, destination: &Path) -> Result<(), String> {
    gst::init().map_err(|error| format!("could not initialize GStreamer: {error}"))?;
    let source = canonical_path(source)?;
    validate_edit_destination(&source, destination)?;
    let duration = video_duration(&source)?;
    if duration == gst::ClockTime::ZERO {
        return Err(format!("video {} has no duration", source.display()));
    }

    let staging = edit_staging_directory(destination)?;
    let video_path = staging.path().join("video.mp4");
    export_video_ranges(
        &video_path,
        &[VideoRange {
            source: &source,
            start: gst::ClockTime::ZERO,
            end: duration,
        }],
        None,
    )?;
    save_video_edit(&video_path, destination)
}

fn validate_edit_destination(source: &Path, destination: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    let destination_metadata = match std::fs::metadata(destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "could not inspect video output {}: {error}",
                destination.display()
            ));
        }
    };
    let source_metadata = std::fs::metadata(source).map_err(|error| {
        format!(
            "could not inspect video source {}: {error}",
            source.display()
        )
    })?;
    if destination_metadata.dev() == source_metadata.dev()
        && destination_metadata.ino() == source_metadata.ino()
    {
        return Err("video output must be different from the source file".to_owned());
    }
    Ok(())
}

fn edit_staging_directory(destination: &Path) -> Result<tempfile::TempDir, String> {
    let directory = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    tempfile::tempdir_in(directory)
        .map_err(|error| format!("could not create temporary video export: {error}"))
}

struct VideoRange<'a> {
    source: &'a Path,
    start: gst::ClockTime,
    end: gst::ClockTime,
}

fn finish_video_edit(
    video_path: &Path,
    audio_video_path: &Path,
    destination: &Path,
    ranges: &[VideoRange<'_>],
) -> Result<(), String> {
    let mut discovered = std::collections::HashMap::new();
    let mut audio_tracks = Vec::with_capacity(ranges.len());
    for range in ranges {
        let has_audio = match discovered.get(range.source) {
            Some(&has_audio) => has_audio,
            None => {
                let has_audio = discover_audio(range.source)?;
                discovered.insert(range.source, has_audio);
                has_audio
            }
        };
        audio_tracks.push(has_audio);
    }
    let completed = if audio_tracks.iter().any(|present| *present) {
        export_audio_ranges(video_path, audio_video_path, ranges, &audio_tracks)?;
        audio_video_path
    } else {
        video_path
    };
    save_video_edit(completed, destination)
}

fn save_video_edit(completed: &Path, destination: &Path) -> Result<(), String> {
    std::fs::rename(completed, destination).map_err(|error| {
        format!(
            "could not save edited video to {}: {error}",
            destination.display()
        )
    })
}

fn discover_audio(source: &Path) -> Result<bool, String> {
    let uri = gst::glib::filename_to_uri(source, None)
        .map_err(|error| format!("could not create a media URI: {error}"))?;
    let discoverer = gstreamer_pbutils::Discoverer::new(FRAME_TIMEOUT)
        .map_err(|error| format!("could not initialize media discovery: {error}"))?;
    let info = discoverer
        .discover_uri(uri.as_str())
        .map_err(|error| format!("could not inspect media in {}: {error}", source.display()))?;
    match info.result() {
        gstreamer_pbutils::DiscovererResult::Ok => Ok(!info.audio_streams().is_empty()),
        gstreamer_pbutils::DiscovererResult::MissingPlugins => {
            let missing = info
                .missing_elements_installer_details()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "media in {} requires missing GStreamer plugins: {missing}",
                source.display()
            ))
        }
        result => Err(format!(
            "could not inspect media in {}: {result:?}",
            source.display()
        )),
    }
}

fn with_audio_decoder<T>(
    source: &Path,
    operation: impl FnOnce(&gst::Element, &gst_app::AppSink) -> Result<T, String>,
) -> Result<T, String> {
    let uri = gst::glib::filename_to_uri(source, None)
        .map_err(|error| format!("could not create an audio URI: {error}"))?;
    let caps = gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE")
        .field("rate", AUDIO_EXPORT_RATE as i32)
        .field("channels", 2i32)
        .field("layout", "interleaved")
        .build();
    let sink = gst_app::AppSink::builder()
        .caps(&caps)
        .sync(false)
        .max_buffers(32)
        .build();
    let video_sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .map_err(|error| format!("could not create an audio export video sink: {error}"))?;
    let decoder = gst::ElementFactory::make("playbin")
        .property("uri", uri.as_str())
        .property("audio-sink", &sink)
        .property("video-sink", &video_sink)
        .build()
        .map_err(|error| format!("could not create an audio decoder: {error}"))?;
    let result = (|| {
        decoder
            .set_state(gst::State::Paused)
            .map_err(|error| format!("could not open audio from {}: {error}", source.display()))?;
        let (change, state, _) = decoder.state(FRAME_TIMEOUT);
        change.map_err(|error| {
            format!("could not decode audio from {}: {error}", source.display())
        })?;
        if state != gst::State::Paused {
            return Err(format!(
                "timed out preparing audio from {}",
                source.display()
            ));
        }
        operation(&decoder, &sink)
    })();
    let stopped = decoder
        .set_state(gst::State::Null)
        .map_err(|error| format!("could not stop audio decoder: {error}"));
    finish_decoder(result, stopped)
}

fn export_audio_ranges(
    video_path: &Path,
    destination: &Path,
    ranges: &[VideoRange<'_>],
    audio_tracks: &[bool],
) -> Result<(), String> {
    let pipeline = gst::parse::launch(
        "filesrc name=video ! qtdemux name=demux \
         demux.video_0 ! queue ! h264parse ! queue ! mux. \
         appsrc name=audio format=time is-live=false block=false \
           ! audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved \
           ! identity name=audio-encoder-input \
         aacparse name=audio-parser ! queue ! mux. \
         mp4mux name=mux movie-timescale=48000 ! filesink name=output",
    )
    .map_err(|error| format!("could not build audio export pipeline: {error}"))?
    .downcast::<gst::Pipeline>()
    .map_err(|_| "could not initialize audio export pipeline".to_owned())?;
    super::aac::add_to_pipeline(&pipeline, "audio-encoder-input", "audio-parser")?;
    pipeline
        .by_name("video")
        .ok_or_else(|| "audio export pipeline has no video source".to_owned())?
        .set_property("location", video_path.to_string_lossy().as_ref());
    pipeline
        .by_name("output")
        .ok_or_else(|| "audio export pipeline has no output".to_owned())?
        .set_property("location", destination.to_string_lossy().as_ref());
    let appsrc = pipeline
        .by_name("audio")
        .ok_or_else(|| "audio export pipeline has no audio source".to_owned())?
        .downcast::<gst_app::AppSrc>()
        .map_err(|_| "could not initialize audio export source".to_owned())?;
    let bus = pipeline
        .bus()
        .ok_or_else(|| "audio export pipeline has no event bus".to_owned())?;
    let result = (|| {
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| format!("could not start audio export: {error}"))?;
        let mut output_offset = 0u64;
        for (range, &has_audio) in ranges.iter().zip(audio_tracks) {
            let start = audio_frame_at(range.start);
            let end = audio_frame_at(range.end);
            let length = end
                .checked_sub(start)
                .ok_or_else(|| "audio export range is invalid".to_owned())?;
            if has_audio {
                with_audio_decoder(range.source, |decoder, sink| {
                    export_audio_range(decoder, sink, &appsrc, &bus, range, output_offset)
                })?;
            } else {
                push_silence(&appsrc, &bus, output_offset, length)?;
            }
            output_offset = output_offset
                .checked_add(length)
                .ok_or_else(|| "audio export duration is too long".to_owned())?;
        }
        appsrc
            .end_of_stream()
            .map_err(|error| format!("could not finish audio export: {error}"))?;
        let message = bus
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(30),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .ok_or_else(|| "timed out finalizing audio and video".to_owned())?;
        match message.view() {
            gst::MessageView::Eos(_) => Ok(()),
            gst::MessageView::Error(error) => {
                Err(format!("audio export failed: {}", error.error()))
            }
            _ => Err("audio export ended unexpectedly".to_owned()),
        }
    })();
    let timing = result.and_then(|_| super::aac::mp4_timing(&pipeline));
    let stopped = pipeline
        .set_state(gst::State::Null)
        .map_err(|error| format!("could not stop audio export pipeline: {error}"));
    let timing = finish_decoder(timing, stopped)?;
    super::aac::finalize_mp4(destination, timing)
}

fn export_audio_range(
    decoder: &gst::Element,
    sink: &gst_app::AppSink,
    appsrc: &gst_app::AppSrc,
    output_bus: &gst::Bus,
    range: &VideoRange<'_>,
    output_offset: u64,
) -> Result<(), String> {
    sink.try_pull_preroll(FRAME_TIMEOUT)
        .ok_or_else(|| "timed out preparing the source audio".to_owned())?;
    decoder
        .seek_simple(
            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
            range.start,
        )
        .map_err(|error| format!("could not seek to the audio range: {error}"))?;
    decoder
        .set_state(gst::State::Playing)
        .map_err(|error| format!("could not decode the selected audio range: {error}"))?;
    let decoder_bus = decoder
        .bus()
        .ok_or_else(|| "audio decoder has no event bus".to_owned())?;
    let start = audio_frame_at(range.start);
    let end = audio_frame_at(range.end);
    let length = end - start;
    let mut written = 0u64;
    while written < length {
        let sample = sink
            .try_pull_sample(gst::ClockTime::from_mseconds(250))
            .or_else(|| {
                if sink.is_eos() {
                    None
                } else {
                    sink.try_pull_sample(FRAME_TIMEOUT)
                }
            });
        let Some(sample) = sample else {
            if decoder_finished(&decoder_bus)? || sink.is_eos() {
                break;
            }
            return Err("timed out decoding the selected audio range".to_owned());
        };
        let buffer = sample
            .buffer()
            .ok_or_else(|| "audio decoder returned an empty buffer".to_owned())?;
        let pts = buffer
            .pts()
            .ok_or_else(|| "audio decoder returned a buffer without a timestamp".to_owned())?;
        let pts = audio_presentation_time(&sample, pts)?;
        let mapped = buffer
            .map_readable()
            .map_err(|error| format!("could not read decoded audio: {error}"))?;
        let data = mapped.as_slice();
        if data.len() % AUDIO_FRAME_BYTES != 0 {
            return Err("audio decoder returned an incomplete sample frame".to_owned());
        }
        let buffer_start = audio_frame_at(pts);
        let buffer_end = buffer_start.saturating_add((data.len() / AUDIO_FRAME_BYTES) as u64);
        if buffer_start >= end {
            break;
        }
        let overlap_start = buffer_start.max(start + written);
        let overlap_end = buffer_end.min(end);
        if overlap_start >= overlap_end {
            continue;
        }
        let gap = overlap_start - (start + written);
        if gap > 0 {
            push_silence(appsrc, output_bus, output_offset + written, gap)?;
            written += gap;
        }
        let first = ((overlap_start - buffer_start) as usize) * AUDIO_FRAME_BYTES;
        let last = ((overlap_end - buffer_start) as usize) * AUDIO_FRAME_BYTES;
        push_audio_bytes(
            appsrc,
            output_bus,
            output_offset + written,
            &data[first..last],
        )?;
        written += overlap_end - overlap_start;
    }
    if written < length {
        push_silence(
            appsrc,
            output_bus,
            output_offset + written,
            length - written,
        )?;
    }
    Ok(())
}

fn audio_frame_at(time: gst::ClockTime) -> u64 {
    ((u128::from(time.nseconds()) * u128::from(AUDIO_EXPORT_RATE) + 500_000_000) / 1_000_000_000)
        as u64
}

fn audio_presentation_time(
    sample: &gst::Sample,
    pts: gst::ClockTime,
) -> Result<gst::ClockTime, String> {
    sample
        .segment()
        .and_then(|segment| segment.downcast_ref::<gst::ClockTime>())
        .and_then(|segment| segment.to_stream_time(pts))
        .ok_or_else(|| {
            "audio decoder returned a timestamp outside its presentation segment".to_owned()
        })
}

fn push_silence(
    appsrc: &gst_app::AppSrc,
    bus: &gst::Bus,
    mut offset: u64,
    mut frames: u64,
) -> Result<(), String> {
    while frames > 0 {
        let count = frames.min(AUDIO_CHUNK_FRAMES as u64) as usize;
        push_audio_bytes(appsrc, bus, offset, &vec![0; count * AUDIO_FRAME_BYTES])?;
        offset += count as u64;
        frames -= count as u64;
    }
    Ok(())
}

fn push_audio_bytes(
    appsrc: &gst_app::AppSrc,
    bus: &gst::Bus,
    mut offset: u64,
    bytes: &[u8],
) -> Result<(), String> {
    for chunk in bytes.chunks(AUDIO_CHUNK_FRAMES * AUDIO_FRAME_BYTES) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while appsrc.property::<u64>("current-level-bytes") >= AUDIO_QUEUE_LIMIT {
            if let Some(message) = bus.timed_pop_filtered(
                gst::ClockTime::from_mseconds(100),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            ) {
                return match message.view() {
                    gst::MessageView::Error(error) => {
                        Err(format!("audio export failed: {}", error.error()))
                    }
                    _ => Err("audio export ended before all samples were written".to_owned()),
                };
            }
            if std::time::Instant::now() >= deadline {
                return Err("timed out writing audio to the video export".to_owned());
            }
        }
        let frames = (chunk.len() / AUDIO_FRAME_BYTES) as u64;
        let mut buffer = gst::Buffer::from_mut_slice(chunk.to_vec());
        let buffer = buffer
            .get_mut()
            .ok_or_else(|| "could not prepare audio samples for export".to_owned())?;
        let pts = ((u128::from(offset) * 1_000_000_000) / u128::from(AUDIO_EXPORT_RATE)) as u64;
        let next =
            ((u128::from(offset + frames) * 1_000_000_000) / u128::from(AUDIO_EXPORT_RATE)) as u64;
        buffer.set_pts(gst::ClockTime::from_nseconds(pts));
        buffer.set_duration(gst::ClockTime::from_nseconds(next - pts));
        appsrc
            .push_buffer(buffer.to_owned())
            .map_err(|error| format!("could not export audio samples: {error}"))?;
        offset += frames;
    }
    Ok(())
}

fn video_duration(source: &Path) -> Result<gst::ClockTime, String> {
    let (decoder, sink) = player(source, false, false)?;
    let duration = decoder
        .set_state(gst::State::Paused)
        .map_err(|error| format!("could not open video for editing: {error}"))
        .and_then(|_| {
            sink.try_pull_preroll(FRAME_TIMEOUT)
                .ok_or_else(|| "timed out preparing video for editing".to_owned())
        })
        .and_then(|_| {
            decoder
                .query_duration::<gst::ClockTime>()
                .ok_or_else(|| "could not determine the video duration".to_owned())
        });
    let stopped = decoder
        .set_state(gst::State::Null)
        .map_err(|error| format!("could not stop video decoder: {error}"));
    finish_decoder(duration, stopped)
}

fn export_video_ranges(
    destination: &Path,
    ranges: &[VideoRange<'_>],
    dimensions: Option<(u32, u32)>,
) -> Result<(), String> {
    let directory = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let staging = tempfile::tempdir_in(directory)
        .map_err(|error| format!("could not create temporary video export: {error}"))?;
    let staged_path = staging.path().join("video.mp4");
    let mut output_pipeline = None;
    let mut appsrc: Option<gst_app::AppSrc> = None;
    let mut output_offset_ns = 0u64;

    let result = (|| {
        for range in ranges {
            let start = range.start;
            let end = range.end;
            let output_offset = gst::ClockTime::from_nseconds(output_offset_ns);
            let (decoder, sink) = match dimensions {
                Some(size) => normalized_player(range.source, size)?,
                None => player(range.source, false, false)?,
            };
            let segment_result = (|| {
                decoder
                    .set_state(gst::State::Paused)
                    .map_err(|error| format!("could not open video for editing: {error}"))?;
                sink.try_pull_preroll(FRAME_TIMEOUT)
                    .ok_or_else(|| "timed out preparing video for editing".to_owned())?;
                let first = seek_decodable_video(&decoder, &sink, start)?;
                let bus = decoder
                    .bus()
                    .ok_or_else(|| "video decoder has no event bus".to_owned())?;
                if appsrc.is_none() {
                    let caps = first
                        .caps()
                        .ok_or_else(|| "video decoder returned no frame format".to_owned())?;
                    let source = gst::ElementFactory::make("appsrc")
                        .property("caps", caps.to_owned())
                        .property("format", gst::Format::Time)
                        .property("is-live", false)
                        .property("block", false)
                        .build()
                        .map_err(|error| format!("could not create video export source: {error}"))?
                        .downcast::<gst_app::AppSrc>()
                        .map_err(|_| "could not initialize the video export source".to_owned())?;
                    let pipeline = crate::linux::capture::recording_pipeline(
                        source.clone().upcast(),
                        &staged_path,
                    )?;
                    pipeline
                        .set_state(gst::State::Playing)
                        .map_err(|error| format!("could not start video export: {error}"))?;
                    appsrc = Some(source);
                    output_pipeline = Some(pipeline);
                }
                let output_bus = output_pipeline
                    .as_ref()
                    .and_then(gst::Pipeline::bus)
                    .ok_or_else(|| "video export pipeline has no event bus".to_owned())?;
                let count = export_trimmed_frames(
                    appsrc.as_ref().expect("export source is initialized"),
                    &sink,
                    &bus,
                    &output_bus,
                    first,
                    (start, end),
                    output_offset,
                )?;
                if count == 0 {
                    return Err("the selected video range contains no frames".to_owned());
                }
                Ok(())
            })();
            let stopped = decoder
                .set_state(gst::State::Null)
                .map_err(|error| format!("could not stop video decoder: {error}"));
            finish_decoder(segment_result, stopped).map_err(|error| {
                format!(
                    "could not export video range {}..{} from {}: {error}",
                    start,
                    end,
                    range.source.display()
                )
            })?;
            output_offset_ns = output_offset_ns
                .checked_add(end.nseconds() - start.nseconds())
                .filter(|&value| value < u64::MAX)
                .ok_or_else(|| "combined video duration is too long".to_owned())?;
        }

        let pipeline = output_pipeline
            .as_ref()
            .ok_or_else(|| "the selected video ranges contain no frames".to_owned())?;
        crate::linux::capture::finish_recording(pipeline)?;
        if !staged_path.is_file() {
            return Err("video export did not create an MP4 file".to_owned());
        }
        std::fs::rename(&staged_path, destination).map_err(|error| {
            format!(
                "could not save video export to {}: {error}",
                destination.display()
            )
        })
    })();

    if result.is_err() {
        if let Some(pipeline) = output_pipeline {
            let stopped = pipeline
                .set_state(gst::State::Null)
                .map_err(|error| format!("could not stop video export pipeline: {error}"));
            return finish_decoder(result, stopped);
        }
    }
    result
}

/// Size and smoothness trade-offs for GIF export.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GifPreset {
    /// Every frame at its original size.
    Original,
    ScreenRecording,
    HighMotion,
    SmallFile,
}

impl GifPreset {
    /// Frames closer together than this are dropped.
    fn min_frame_interval_ns(self) -> u64 {
        match self {
            Self::Original => 0,
            Self::ScreenRecording => 66_000_000,
            Self::HighMotion => 33_000_000,
            Self::SmallFile => 100_000_000,
        }
    }

    fn max_width(self) -> Option<u32> {
        match self {
            Self::Original => None,
            Self::ScreenRecording => Some(960),
            Self::HighMotion => Some(1280),
            Self::SmallFile => Some(480),
        }
    }

    fn encoder_speed(self) -> i32 {
        match self {
            Self::SmallFile => 20,
            _ => 10,
        }
    }
}

#[cfg(test)]
pub fn export_gif(source: &Path, destination: &Path) -> Result<(), String> {
    export_gif_with_preset(source, destination, GifPreset::Original)
}

pub fn export_gif_with_preset(
    source: &Path,
    destination: &Path,
    preset: GifPreset,
) -> Result<(), String> {
    gst::init().map_err(|error| format!("could not initialize GStreamer: {error}"))?;
    let source = canonical_path(source)?;
    validate_edit_destination(&source, destination)?;
    let directory = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let staging = tempfile::tempdir_in(directory)
        .map_err(|error| format!("could not create temporary GIF export: {error}"))?;
    let staged_path = staging.path().join("export.gif");
    let file = File::create(&staged_path)
        .map_err(|error| format!("could not create temporary GIF: {error}"))?;
    let mut encoder = GifEncoder::new_with_speed(file, preset.encoder_speed());
    encoder
        .set_repeat(Repeat::Infinite)
        .map_err(|error| format!("could not configure animated GIF looping: {error}"))?;

    let (decoder, sink) = player(&source, false, false)?;
    let bus = decoder
        .bus()
        .ok_or_else(|| "video decoder has no event bus".to_owned())?;
    let mut pending: Option<GifVideoFrame> = None;
    let mut last_interval = None;
    let mut timing = GifTiming::default();
    let mut frame_count = 0usize;
    let mut last_progress = std::time::Instant::now();
    let result = decoder
        .set_state(gst::State::Playing)
        .map_err(|error| format!("could not start video decoding: {error}"))
        .and_then(|_| loop {
            let sample = match sink.try_pull_sample(gst::ClockTime::from_mseconds(250)) {
                Some(sample) => sample,
                None if decoder_finished(&bus)? || sink.is_eos() => break Ok(()),
                None if last_progress.elapsed()
                    >= std::time::Duration::from_nanos(FRAME_TIMEOUT.nseconds()) =>
                {
                    break Err("timed out decoding video for GIF export".to_owned())
                }
                None => continue,
            };
            let current = GifVideoFrame::from_sample(&sample)?;
            if pending.as_ref().is_some_and(|previous| {
                current.timestamp_ns.saturating_sub(previous.timestamp_ns)
                    < preset.min_frame_interval_ns()
            }) {
                last_progress = std::time::Instant::now();
                continue;
            }
            if let Some(previous) = pending.take() {
                let interval = current
                    .timestamp_ns
                    .checked_sub(previous.timestamp_ns)
                    .filter(|interval| *interval > 0)
                    .ok_or_else(|| {
                        "video frames have non-increasing presentation timestamps".to_owned()
                    })?;
                if encode_gif_frame(
                    &mut encoder,
                    &mut timing,
                    &previous.document,
                    interval,
                    preset.max_width(),
                )? {
                    frame_count += 1;
                }
                last_interval = Some(interval);
            }
            pending = Some(current);
            last_progress = std::time::Instant::now();
        });
    let stopped = decoder
        .set_state(gst::State::Null)
        .map_err(|error| format!("could not stop video decoder: {error}"));
    finish_decoder(result, stopped)?;
    if let Some(last) = pending {
        let duration = last
            .duration_ns
            .or(last_interval)
            .or(last.nominal_duration_ns)
            .ok_or_else(|| "could not determine the final video frame duration".to_owned())?;
        if encode_gif_frame(
            &mut encoder,
            &mut timing,
            &last.document,
            duration,
            preset.max_width(),
        )? {
            frame_count += 1;
        }
    }
    if frame_count == 0 {
        return Err(
            "the selected video contains no frames with GIF-representable timing".to_owned(),
        );
    }
    drop(encoder);
    File::open(&staged_path)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("could not finalize animated GIF: {error}"))?;
    std::fs::rename(&staged_path, destination).map_err(|error| {
        format!(
            "could not save animated GIF to {}: {error}",
            destination.display()
        )
    })
}

struct GifVideoFrame {
    document: Document,
    timestamp_ns: u64,
    duration_ns: Option<u64>,
    nominal_duration_ns: Option<u64>,
}

impl GifVideoFrame {
    fn from_sample(sample: &gst::Sample) -> Result<Self, String> {
        let buffer = sample
            .buffer()
            .ok_or_else(|| "video decoder returned an empty frame buffer".to_owned())?;
        let timestamp_ns = buffer
            .pts()
            .ok_or_else(|| "video decoder returned a frame without a timestamp".to_owned())?
            .nseconds();
        if buffer.flags().contains(gst::BufferFlags::CORRUPTED) {
            return Err(format!("video frame at {timestamp_ns} ns is corrupted"));
        }
        let duration_ns = buffer
            .duration()
            .map(|duration| duration.nseconds())
            .filter(|duration| *duration > 0);
        let nominal_duration_ns = sample
            .caps()
            .and_then(|caps| caps.structure(0))
            .and_then(|caps| caps.get::<gst::Fraction>("framerate").ok())
            .and_then(|rate| {
                let numerator = u64::try_from(rate.numer())
                    .ok()
                    .filter(|value| *value > 0)?;
                let denominator = u64::try_from(rate.denom())
                    .ok()
                    .filter(|value| *value > 0)?;
                1_000_000_000u64
                    .checked_mul(denominator)?
                    .checked_div(numerator)
            })
            .filter(|duration| *duration > 0);
        Ok(Self {
            document: document_from_sample(sample)?,
            timestamp_ns,
            duration_ns,
            nominal_duration_ns,
        })
    }
}

#[derive(Default)]
struct GifTiming {
    elapsed_ns: u64,
    emitted_ticks: u64,
}

impl GifTiming {
    fn frame_delay_ms(&mut self, duration_ns: u64) -> Result<Option<u32>, String> {
        let elapsed_ns = self
            .elapsed_ns
            .checked_add(duration_ns)
            .ok_or_else(|| "video duration is too long for GIF export".to_owned())?;
        // Round the accumulated time rather than each frame to avoid timing drift.
        let ticks =
            elapsed_ns / GIF_TICK_NS + u64::from(elapsed_ns % GIF_TICK_NS >= GIF_TICK_NS / 2);
        let frame_ticks = u16::try_from(ticks - self.emitted_ticks)
            .map_err(|_| "a video frame lasts too long for GIF export".to_owned())?;
        self.elapsed_ns = elapsed_ns;
        self.emitted_ticks = ticks;
        Ok((frame_ticks > 0).then_some(u32::from(frame_ticks) * 10))
    }
}

fn encode_gif_frame(
    encoder: &mut GifEncoder<File>,
    timing: &mut GifTiming,
    document: &Document,
    duration_ns: u64,
    max_width: Option<u32>,
) -> Result<bool, String> {
    let Some(duration_ms) = timing.frame_delay_ms(duration_ns)? else {
        return Ok(false);
    };
    let frame = gif_frame_from_document(document, duration_ms, max_width)?;
    encoder
        .encode_frame(frame)
        .map_err(|error| format!("could not encode GIF frame: {error}"))?;
    Ok(true)
}

fn gif_frame_from_document(
    document: &Document,
    duration_ms: u32,
    max_width: Option<u32>,
) -> Result<Frame, String> {
    let pixmap = document.flatten();
    let mut rgba = Vec::with_capacity(pixmap.data().len());
    for pixel in pixmap.pixels() {
        let color = pixel.demultiply();
        rgba.extend_from_slice(&[color.red(), color.green(), color.blue(), color.alpha()]);
    }
    let mut image = RgbaImage::from_raw(pixmap.width(), pixmap.height(), rgba)
        .ok_or_else(|| "decoded video frame has an invalid pixel buffer".to_owned())?;
    if let Some(max_width) = max_width.filter(|max| image.width() > *max) {
        let height = (u64::from(image.height()) * u64::from(max_width) / u64::from(image.width()))
            .max(1) as u32;
        image = image::imageops::resize(
            &image,
            max_width,
            height,
            image::imageops::FilterType::Triangle,
        );
    }
    Ok(Frame::from_parts(
        image,
        0,
        0,
        Delay::from_numer_denom_ms(duration_ms, 1),
    ))
}

fn trim_video_pipeline(
    decoder: &gst::Element,
    sink: &gst_app::AppSink,
    destination: &Path,
    start: gst::ClockTime,
    end: gst::ClockTime,
) -> Result<(), String> {
    decoder
        .set_state(gst::State::Paused)
        .map_err(|error| format!("could not open video for trimming: {error}"))?;
    sink.try_pull_preroll(FRAME_TIMEOUT)
        .ok_or_else(|| "timed out preparing video for trimming".to_owned())?;
    let first = seek_decodable_video(decoder, sink, start)?;
    let bus = decoder
        .bus()
        .ok_or_else(|| "video decoder has no event bus".to_owned())?;

    let caps = first
        .caps()
        .ok_or_else(|| "video decoder returned no frame format".to_owned())?;
    let appsrc = gst::ElementFactory::make("appsrc")
        .property("caps", caps.to_owned())
        .property("format", gst::Format::Time)
        .property("is-live", false)
        .property("block", false)
        .build()
        .map_err(|error| format!("could not create video export source: {error}"))?
        .downcast::<gst_app::AppSrc>()
        .map_err(|_| "could not initialize the video export source".to_owned())?;
    let directory = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let staging = tempfile::tempdir_in(directory)
        .map_err(|error| format!("could not create temporary video export: {error}"))?;
    let staged_path: PathBuf = staging.path().join("trimmed.mp4");
    let pipeline =
        crate::linux::capture::recording_pipeline(appsrc.clone().upcast(), &staged_path)?;
    let output_bus = pipeline
        .bus()
        .ok_or_else(|| "video export pipeline has no event bus".to_owned())?;
    if let Err(error) = pipeline.set_state(gst::State::Playing) {
        let stopped = pipeline
            .set_state(gst::State::Null)
            .map_err(|error| format!("could not stop video export pipeline: {error}"));
        return finish_decoder(
            Err(format!("could not start video export: {error}")),
            stopped,
        );
    }
    let frame_count = match export_trimmed_frames(
        &appsrc,
        sink,
        &bus,
        &output_bus,
        first,
        (start, end),
        gst::ClockTime::ZERO,
    ) {
        Ok(frame_count) => frame_count,
        Err(error) => {
            let stopped = pipeline
                .set_state(gst::State::Null)
                .map_err(|error| format!("could not stop video export pipeline: {error}"));
            return finish_decoder(Err(error), stopped);
        }
    };
    if frame_count == 0 {
        let stopped = pipeline
            .set_state(gst::State::Null)
            .map_err(|error| format!("could not stop video export pipeline: {error}"));
        return finish_decoder(
            Err("the selected trim range contains no video frames".to_owned()),
            stopped,
        );
    }
    crate::linux::capture::finish_recording(&pipeline)?;
    if !staged_path.is_file() {
        return Err("video export did not create an MP4 file".to_owned());
    }
    std::fs::rename(&staged_path, destination).map_err(|error| {
        format!(
            "could not save trimmed video to {}: {error}",
            destination.display()
        )
    })
}

fn export_trimmed_frames(
    appsrc: &gst_app::AppSrc,
    sink: &gst_app::AppSink,
    decoder_bus: &gst::Bus,
    output_bus: &gst::Bus,
    first: gst::Sample,
    range: (gst::ClockTime, gst::ClockTime),
    output_offset: gst::ClockTime,
) -> Result<usize, String> {
    let (start, end) = range;
    let mut frame_count = 0usize;
    let mut sample = Some(first);
    while let Some(current) = sample {
        match push_trimmed_frame(appsrc, output_bus, &current, start, end, output_offset)? {
            TrimFrameResult::End => break,
            TrimFrameResult::Skipped => {}
            TrimFrameResult::Pushed => frame_count += 1,
        }
        sample = sink.try_pull_sample(gst::ClockTime::from_mseconds(250));
        if sample.is_none() {
            if decoder_finished(decoder_bus)? || sink.is_eos() {
                break;
            }
            sample = sink.try_pull_sample(FRAME_TIMEOUT);
            if sample.is_none() {
                if decoder_finished(decoder_bus)? || sink.is_eos() {
                    break;
                }
                return Err("timed out decoding the selected video range".to_owned());
            }
        }
    }
    Ok(frame_count)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrimFrameResult {
    End,
    Skipped,
    Pushed,
}

fn push_trimmed_frame(
    appsrc: &gst_app::AppSrc,
    output_bus: &gst::Bus,
    sample: &gst::Sample,
    start: gst::ClockTime,
    end: gst::ClockTime,
    output_offset: gst::ClockTime,
) -> Result<TrimFrameResult, String> {
    let input = sample
        .buffer()
        .ok_or_else(|| "video decoder returned an empty frame buffer".to_owned())?;
    let pts = input
        .pts()
        .ok_or_else(|| "video decoder returned a frame without a timestamp".to_owned())?;
    if pts >= end {
        return Ok(TrimFrameResult::End);
    }
    if pts < start {
        return Ok(TrimFrameResult::Skipped);
    }
    if input.flags().contains(gst::BufferFlags::CORRUPTED) {
        return Err(format!("video frame at {pts} is corrupted"));
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while appsrc.property::<u64>("current-level-bytes") >= VIDEO_QUEUE_LIMIT {
        if let Some(message) = output_bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(100),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) {
            return match message.view() {
                gst::MessageView::Error(error) => {
                    Err(format!("video export failed: {}", error.error()))
                }
                _ => Err("video export ended before all frames were written".to_owned()),
            };
        }
        if std::time::Instant::now() >= deadline {
            return Err("timed out writing frames to the video export".to_owned());
        }
    }
    let mut buffer = input
        .copy_deep()
        .map_err(|error| format!("could not prepare video frame for export: {error}"))?;
    let buffer = buffer
        .get_mut()
        .ok_or_else(|| "could not update the exported video frame timestamp".to_owned())?;
    buffer.set_pts(gst::ClockTime::from_nseconds(
        pts.nseconds()
            .saturating_sub(start.nseconds())
            .saturating_add(output_offset.nseconds()),
    ));
    if buffer.duration().is_none() {
        buffer.set_duration(gst::ClockTime::from_nseconds(
            1_000_000_000 / u64::from(VIDEO_EXPORT_FPS),
        ));
    }
    appsrc
        .push_buffer(buffer.to_owned())
        .map_err(|error| format!("could not export video frame: {error}"))?;
    Ok(TrimFrameResult::Pushed)
}

fn decoder_finished(bus: &gst::Bus) -> Result<bool, String> {
    let Some(message) = bus.timed_pop_filtered(
        gst::ClockTime::ZERO,
        &[gst::MessageType::Eos, gst::MessageType::Error],
    ) else {
        return Ok(false);
    };
    match message.view() {
        gst::MessageView::Eos(_) => Ok(true),
        gst::MessageView::Error(error) => Err(format!("could not decode video: {}", error.error())),
        _ => Ok(false),
    }
}

fn canonical_path(path: &Path) -> Result<std::path::PathBuf, String> {
    path.canonicalize()
        .map_err(|error| format!("could not open {}: {error}", path.display()))
}

fn player(
    path: &Path,
    sync: bool,
    play_audio: bool,
) -> Result<(gst::Element, gst_app::AppSink), String> {
    let uri = gst::glib::filename_to_uri(path, None)
        .map_err(|error| format!("could not create a video URI: {error}"))?;
    let sink = gst_app::AppSink::builder()
        .caps(
            &gst::Caps::builder("video/x-raw")
                .field("format", "RGBA")
                .build(),
        )
        .sync(sync)
        .max_buffers(VIDEO_DECODE_QUEUE_LIMIT)
        .build();
    let audio_sink = gst::ElementFactory::make(if play_audio {
        "autoaudiosink"
    } else {
        "fakesink"
    })
    .build()
    .map_err(|error| format!("could not create a video audio sink: {error}"))?;
    let player = gst::ElementFactory::make("playbin")
        .property("uri", uri.as_str())
        .property("video-sink", &sink)
        .property("audio-sink", &audio_sink)
        .build()
        .map_err(|error| format!("could not create a video decoder: {error}"))?;
    Ok((player, sink))
}

fn normalized_player(
    path: &Path,
    dimensions: (u32, u32),
) -> Result<(gst::Element, gst_app::AppSink), String> {
    let width = i32::try_from(dimensions.0)
        .map_err(|_| "video width exceeds GStreamer's supported dimensions".to_owned())?;
    let height = i32::try_from(dimensions.1)
        .map_err(|_| "video height exceeds GStreamer's supported dimensions".to_owned())?;
    let caps = gst::Caps::builder("video/x-raw")
        .field("format", "RGBA")
        .field("width", width)
        .field("height", height)
        .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
        .field("framerate", gst::Fraction::new(VIDEO_EXPORT_FPS as i32, 1))
        .build();
    let sink = gst_app::AppSink::builder()
        .caps(&caps)
        .sync(false)
        .max_buffers(VIDEO_DECODE_QUEUE_LIMIT)
        .build();
    let convert = gst::ElementFactory::make("videoconvert")
        .build()
        .map_err(|error| format!("could not create video converter: {error}"))?;
    let scale = gst::ElementFactory::make("videoscale")
        .property("add-borders", true)
        .build()
        .map_err(|error| format!("could not create video scaler: {error}"))?;
    let rate = gst::ElementFactory::make("videorate")
        .build()
        .map_err(|error| format!("could not create video frame-rate converter: {error}"))?;
    let video_sink = gst::Bin::new();
    video_sink
        .add_many([&convert, &scale, &rate, sink.upcast_ref()])
        .map_err(|error| format!("could not build normalized video sink: {error}"))?;
    gst::Element::link_many([&convert, &scale, &rate, sink.upcast_ref()])
        .map_err(|error| format!("could not connect normalized video sink: {error}"))?;
    let input = convert
        .static_pad("sink")
        .ok_or_else(|| "video converter has no input".to_owned())?;
    let input = gst::GhostPad::with_target(&input)
        .map_err(|error| format!("could not create normalized video input: {error}"))?;
    video_sink
        .add_pad(&input)
        .map_err(|error| format!("could not attach normalized video input: {error}"))?;
    let uri = gst::glib::filename_to_uri(path, None)
        .map_err(|error| format!("could not create a video URI: {error}"))?;
    let audio_sink = gst::ElementFactory::make("fakesink")
        .build()
        .map_err(|error| format!("could not create a video audio sink: {error}"))?;
    let decoder = gst::ElementFactory::make("playbin")
        .property("uri", uri.as_str())
        .property("video-sink", &video_sink)
        .property("audio-sink", &audio_sink)
        .build()
        .map_err(|error| format!("could not create a video decoder: {error}"))?;
    Ok((decoder, sink))
}

fn document_from_sample(sample: &gst::Sample) -> Result<Document, String> {
    if video_sample_is_corrupted(sample)? {
        return Err("video decoder returned a corrupted frame".to_owned());
    }
    let caps = sample
        .caps()
        .and_then(|caps| caps.structure(0))
        .ok_or_else(|| "video decoder returned no frame format".to_owned())?;
    let width = caps
        .get::<i32>("width")
        .map_err(|error| format!("video frame has invalid width: {error}"))?;
    let height = caps
        .get::<i32>("height")
        .map_err(|error| format!("video frame has invalid height: {error}"))?;
    let size = tiny_skia::IntSize::from_wh(width as u32, height as u32)
        .ok_or_else(|| "video decoder returned an empty frame".to_owned())?;
    let buffer = sample
        .buffer()
        .ok_or_else(|| "video decoder returned an empty frame buffer".to_owned())?;
    let mapped = buffer
        .map_readable()
        .map_err(|error| format!("could not read decoded video frame: {error}"))?;
    let pixmap = Pixmap::from_vec(mapped.as_slice().to_vec(), size)
        .ok_or_else(|| "decoded video frame has an invalid pixel buffer".to_owned())?;
    Ok(Document::new(pixmap))
}

fn finish_decoder<T>(
    result: Result<T, String>,
    stop: Result<gst::StateChangeSuccess, String>,
) -> Result<T, String> {
    match (result, stop) {
        (Ok(value), Ok(_)) => Ok(value),
        (Err(error), Ok(_)) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(stop_error)) => Err(format!("{error}; {stop_error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_presentation_uses_edit_and_seek_offsets_on_the_source_sample_grid() {
        gst::init().expect("GStreamer should initialize");
        let delay = gst::ClockTime::from_nseconds(2_048_000_000_000 / 48_000);
        for seek in [gst::ClockTime::ZERO, gst::ClockTime::from_mseconds(750)] {
            let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
            segment.set_start(delay + seek);
            segment.set_time(seek);
            segment.set_stop(delay + gst::ClockTime::from_seconds(3));
            for delta in [
                gst::ClockTime::ZERO,
                gst::ClockTime::from_nseconds(20_833),
                gst::ClockTime::from_mseconds(500),
            ] {
                let sample = gst::Sample::builder().segment(&segment).build();
                let stream = audio_presentation_time(&sample, delay + seek + delta)
                    .expect("audible timestamp");
                assert_eq!(
                    stream,
                    seek + delta,
                    "codec priming is not a source timeline offset"
                );
                assert_eq!(
                    audio_frame_at(stream),
                    audio_frame_at(seek) + audio_frame_at(delta)
                );
            }
        }
    }

    #[test]
    fn identifies_supported_video_extensions_without_case_sensitivity() {
        assert!(is_video_path(Path::new("clip.MP4")));
        assert!(is_video_path(Path::new("clip.webm")));
        assert!(!is_video_path(Path::new("image.png")));
    }

    #[test]
    fn extracts_the_first_frame_from_a_video() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join("test video.avi");
        let source = gst::ElementFactory::make("videotestsrc")
            .property("num-buffers", 1i32)
            .build()
            .expect("GStreamer test video source should be available");
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("width", 31i32)
                    .field("height", 23i32)
                    .build(),
            )
            .build()
            .expect("GStreamer caps filter should be available");
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
            .add_many([&source, &caps, &encoder, &muxer, &sink])
            .expect("test video pipeline should be assembled");
        gst::Element::link_many([&source, &caps, &encoder, &muxer, &sink])
            .expect("test video elements should link");
        pipeline
            .set_state(gst::State::Playing)
            .expect("test video should start");
        pipeline
            .bus()
            .expect("test pipeline should have a bus")
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("test video should finish encoding");
        pipeline
            .set_state(gst::State::Null)
            .expect("test video pipeline should stop");

        let document = first_frame(&path).expect("the first video frame should decode");
        assert_eq!(document.size(), (31, 23), "decoded frame dimensions");
    }

    #[test]
    fn extracts_a_video_frame_at_the_requested_timestamp() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let source_path = directory.path().join("source.avi");
        write_test_video(&source_path, 90);

        let frame = frame_at(&source_path, 1.0).expect("frame at one second should decode");
        assert_eq!(
            frame.size(),
            (320, 240),
            "timestamp-based extraction should preserve source dimensions"
        );
    }

    #[test]
    fn corrupted_decoded_frame_is_reported_instead_of_imported() {
        gst::init().expect("GStreamer should initialize");
        let mut buffer = gst::Buffer::new();
        buffer
            .get_mut()
            .expect("test buffer should be writable")
            .set_flags(gst::BufferFlags::CORRUPTED);
        let sample = gst::Sample::builder().buffer(&buffer).build();
        let error = document_from_sample(&sample).expect_err("damaged frames must not be imported");
        assert!(
            error.contains("corrupted frame"),
            "failure should explain that the frame is damaged: {error}"
        );
    }

    #[test]
    fn rejects_invalid_frame_timestamps_before_opening_the_video() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let source = directory.path().join("missing.mp4");
        for timestamp in [-1.0, f64::NAN, f64::INFINITY] {
            assert!(
                frame_at(&source, timestamp).is_err(),
                "frame timestamp {timestamp} should be rejected"
            );
        }
    }

    #[test]
    fn playback_delivers_video_frames_and_stops_on_request() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join("playback.avi");
        let source = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .property("num-buffers", 30i32)
            .build()
            .expect("GStreamer test video source should be available");
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
            .add_many([&source, &encoder, &muxer, &sink])
            .expect("test video pipeline should be assembled");
        gst::Element::link_many([&source, &encoder, &muxer, &sink])
            .expect("test video elements should link");
        pipeline
            .set_state(gst::State::Playing)
            .expect("test video should start");
        pipeline
            .bus()
            .expect("test pipeline should have a bus")
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("test video should finish encoding");
        pipeline
            .set_state(gst::State::Null)
            .expect("test video pipeline should stop");

        let stop = AtomicBool::new(false);
        let paused = AtomicBool::new(false);
        let mut frames = 0;
        play(&path, &stop, &paused, |_| {
            frames += 1;
            stop.store(true, Ordering::Release);
        })
        .expect("video playback should finish cleanly after stop");
        assert!(frames > 0, "playback should deliver a decoded frame");
    }

    #[test]
    fn trims_a_video_range_to_a_decodable_mp4_with_the_expected_duration() {
        gst::init().expect("GStreamer should initialize");
        let missing_plugins = ["x264enc", "h264parse", "mp4mux"]
            .into_iter()
            .filter(|plugin| gst::ElementFactory::find(plugin).is_none())
            .collect::<Vec<_>>();
        if !missing_plugins.is_empty() {
            assert!(
                std::env::var_os("CI").is_none(),
                "CI must install GStreamer MP4 plugins to test video trimming; missing: {}",
                missing_plugins.join(", ")
            );
            eprintln!(
                "Skipping video trim test because GStreamer plugins are unavailable: {}",
                missing_plugins.join(", ")
            );
            return;
        }

        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let source_path = directory.path().join("source.avi");
        let destination_path = directory.path().join("trimmed.mp4");
        write_test_video(&source_path, 90);

        trim_video(&source_path, &destination_path, 0.5, 1.5)
            .expect("video trim should export the selected range");
        let document =
            first_frame(&destination_path).expect("trimmed MP4 should contain a decodable frame");
        assert_eq!(document.size(), (320, 240), "trimmed frame dimensions");
        let bytes = std::fs::read(&destination_path).expect("trimmed MP4 should be readable");
        assert!(
            !bytes.windows(4).any(|window| window == b"soun"),
            "trimming a silent source must not introduce an audio track"
        );

        let (output, output_sink) =
            player(&destination_path, false, false).expect("trimmed MP4 should open in GStreamer");
        output
            .set_state(gst::State::Paused)
            .expect("trimmed MP4 should be queryable");
        output_sink
            .try_pull_preroll(FRAME_TIMEOUT)
            .expect("trimmed MP4 should preroll");
        let duration = output
            .query_duration::<gst::ClockTime>()
            .expect("trimmed MP4 should report a duration")
            .nseconds() as f64
            / 1_000_000_000.0;
        output
            .set_state(gst::State::Null)
            .expect("trimmed MP4 query pipeline should stop");
        assert!(
            (0.9..=1.1).contains(&duration),
            "trimmed duration should be approximately 1 second, got {duration}"
        );
    }

    #[test]
    fn silent_export_rejects_source_aliases_and_preserves_failed_outputs() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let source = directory.path().join("source.avi");
        let hard_link = directory.path().join("source hard link.mp4");
        let symlink = directory.path().join("source symlink.mp4");
        let invalid = directory.path().join("not a video.mp4");
        let destination = directory.path().join("existing destination.mp4");
        write_test_video(&source, 30);
        std::fs::hard_link(&source, &hard_link).expect("source hard link should be created");
        std::os::unix::fs::symlink(&source, &symlink).expect("source symlink should be created");
        std::fs::write(&invalid, b"not a video").expect("invalid source should be written");
        std::fs::write(&destination, b"existing output").expect("destination should be written");
        let original = std::fs::read(&source).expect("source should be readable");

        for alias in [&source, &hard_link, &symlink] {
            assert!(
                export_silent_video(&source, alias).is_err(),
                "silent export must reject the source and its aliases: {}",
                alias.display()
            );
        }
        assert!(
            export_silent_video(&invalid, &destination).is_err(),
            "invalid input must not produce an output"
        );
        assert_eq!(
            std::fs::read(&source).expect("source should remain readable"),
            original
        );
        assert_eq!(
            std::fs::read(&destination).expect("existing destination should remain readable"),
            b"existing output",
            "a failed export must preserve an existing destination"
        );
    }

    #[test]
    fn trim_rejects_source_overwrites_and_ranges_beyond_the_video() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let source = directory.path().join("source.avi");
        let destination = directory.path().join("destination.mp4");
        write_test_video(&source, 90);
        let original = std::fs::read(&source).expect("source should be readable");
        std::fs::write(&destination, b"existing output").expect("destination fixture should exist");

        assert!(
            trim_video(&source, &source, 0.5, 1.5).is_err(),
            "trimming must not overwrite the source"
        );
        assert!(
            trim_video(&source, &destination, 0.5, 3.5).is_err(),
            "trim end beyond the source duration must be rejected"
        );
        assert_eq!(
            std::fs::read(&source).expect("source should remain"),
            original
        );
        assert_eq!(
            std::fs::read(&destination).expect("destination should remain"),
            b"existing output",
            "a rejected edit must preserve an existing destination"
        );
        let hard_link = directory.path().join("source hard link.mp4");
        std::fs::hard_link(&source, &hard_link).expect("source hard link should be created");
        let symlink = directory.path().join("source symlink.mp4");
        std::os::unix::fs::symlink(&source, &symlink).expect("source symlink should be created");
        for alias in [&hard_link, &symlink] {
            assert!(
                trim_video(&source, alias, 0.5, 1.5).is_err(),
                "trimming must reject a destination alias of the source"
            );
            assert_eq!(
                std::fs::read(&source).expect("source should remain"),
                original
            );
        }
    }

    #[test]
    fn slideshow_frames_are_letterboxed_and_sized_for_the_encoder() {
        assert_eq!(slideshow_dimensions(64, 48), (64, 48));
        assert_eq!(slideshow_dimensions(3840, 2160), (1920, 1080));
        assert_eq!(slideshow_dimensions(101, 51), (102, 52));
        let wide = RgbaImage::from_pixel(40, 10, image::Rgba([200, 0, 0, 255]));
        let frame = slideshow_frame(&wide, 40, 40);
        assert_eq!(frame.len(), 40 * 40 * 4);
        let pixel = |x: usize, y: usize| &frame[(y * 40 + x) * 4..(y * 40 + x) * 4 + 4];
        assert_eq!(pixel(20, 2), [0, 0, 0, 255]);
        assert_eq!(pixel(20, 20), [200, 0, 0, 255]);
        let clear = RgbaImage::from_pixel(4, 4, image::Rgba([200, 0, 0, 0]));
        assert_eq!(&slideshow_frame(&clear, 4, 4)[..4], [0, 0, 0, 255]);
    }

    #[test]
    fn rejects_invalid_cut_ranges_before_accessing_the_source() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let source = directory.path().join("missing.mp4");
        let destination = directory.path().join("cut.mp4");
        for (start, end) in [
            (-1.0, 1.0),
            (0.0, 0.0),
            (1.0, 0.5),
            (f64::NAN, 1.0),
            (0.0, f64::INFINITY),
        ] {
            assert!(
                cut_video(&source, &destination, start, end).is_err(),
                "cut range {start}..{end} should be rejected"
            );
        }
        assert!(
            !destination.exists(),
            "invalid cut ranges must not create output"
        );
    }

    #[test]
    fn rejects_cut_ranges_that_exceed_or_remove_the_entire_video() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let source = directory.path().join("source.avi");
        let destination = directory.path().join("cut.mp4");
        write_test_video(&source, 90);
        let duration = video_duration(&source)
            .expect("source video should report a duration")
            .nseconds() as f64
            / 1_000_000_000.0;

        assert!(
            cut_video(&source, &destination, 0.5, duration + 0.1).is_err(),
            "cut end beyond the source duration should be rejected"
        );
        assert!(
            cut_video(&source, &destination, 0.0, duration).is_err(),
            "a cut that removes the entire source should be rejected"
        );
        assert!(
            cut_video(&source, &source, 0.5, 1.5).is_err(),
            "cut output must not overwrite the source"
        );
        let source_bytes = std::fs::read(&source).expect("source video should remain readable");
        assert_eq!(
            &source_bytes[..4],
            b"RIFF",
            "rejecting an in-place cut must preserve the source video"
        );
        assert!(
            !destination.exists(),
            "rejected cuts must not create output"
        );
    }

    #[test]
    fn rejects_invalid_trim_ranges_before_accessing_the_source() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let source = directory.path().join("missing.mp4");
        let destination = directory.path().join("trimmed.mp4");
        for (start, end) in [
            (-1.0, 1.0),
            (0.0, 0.0),
            (1.0, 0.5),
            (f64::NAN, 1.0),
            (0.0, f64::INFINITY),
        ] {
            assert!(
                trim_video(&source, &destination, start, end).is_err(),
                "trim range {start}..{end} should be rejected"
            );
        }
        assert!(
            !destination.exists(),
            "invalid trim ranges must not create output"
        );
    }

    #[test]
    fn exports_video_as_a_decodable_animated_gif() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let source_path = directory.path().join("source.avi");
        let destination_path = directory.path().join("animation.gif");
        write_gif_test_video(&source_path, 4, 10);

        export_gif(&source_path, &destination_path)
            .expect("video should export as an animated GIF");
        let file = File::open(&destination_path).expect("exported GIF should exist");
        let decoder = image::codecs::gif::GifDecoder::new(std::io::BufReader::new(file))
            .expect("exported GIF should be decodable");
        let frames = image::AnimationDecoder::into_frames(decoder)
            .collect_frames()
            .expect("exported GIF frames should decode");
        assert_eq!(
            frames.len(),
            4,
            "GIF should preserve all decoded video frames"
        );
        assert_eq!(
            frames[0].buffer().dimensions(),
            (32, 24),
            "GIF frames should preserve the source video dimensions"
        );
        for frame in &frames {
            assert_eq!(
                frame.delay().numer_denom_ms(),
                (100, 1),
                "GIF frame timing should match the 10 fps source"
            );
        }
    }

    #[test]
    fn gif_export_preserves_high_frame_rate_without_accumulating_drift() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let source = directory.path().join("source.avi");
        let destination = directory.path().join("animation.gif");
        write_gif_test_video(&source, 180, 60);

        export_gif(&source, &destination).expect("60 fps video should export as a GIF");
        let file = File::open(destination).expect("exported GIF should exist");
        let decoder = image::codecs::gif::GifDecoder::new(std::io::BufReader::new(file))
            .expect("exported GIF should be decodable");
        let frames = image::AnimationDecoder::into_frames(decoder)
            .collect_frames()
            .expect("exported GIF frames should decode");
        assert_eq!(frames.len(), 180, "GIF must drain the entire video");
        let mut total_ms = 0u32;
        for frame in &frames {
            let (numerator, denominator) = frame.delay().numer_denom_ms();
            assert_eq!(denominator, 1);
            assert!(
                numerator == 10 || numerator == 20,
                "60 fps frames should use the nearest GIF delays, got {numerator} ms"
            );
            total_ms += numerator;
        }
        assert_eq!(total_ms, 3_000, "GIF timing must not drift over 180 frames");
    }

    #[test]
    fn gif_small_file_preset_drops_frames_and_scales_down() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let source = directory.path().join("source.avi");
        let destination = directory.path().join("animation.gif");
        write_gif_test_video(&source, 60, 30);

        export_gif_with_preset(&source, &destination, GifPreset::SmallFile)
            .expect("30 fps video should export with the small file preset");
        let file = File::open(destination).expect("exported GIF should exist");
        let decoder = image::codecs::gif::GifDecoder::new(std::io::BufReader::new(file))
            .expect("exported GIF should be decodable");
        let frames = image::AnimationDecoder::into_frames(decoder)
            .collect_frames()
            .expect("exported GIF frames should decode");
        assert!(
            (15..=25).contains(&frames.len()),
            "10 fps output of two seconds should keep about 20 frames, got {}",
            frames.len()
        );
        let total_ms: u32 = frames.iter().map(|frame| frame.delay().numer_denom_ms().0).sum();
        assert!((1_800..=2_100).contains(&total_ms), "duration was {total_ms} ms");
    }

    #[test]
    fn gif_preset_limits_frame_width() {
        let document = jiffshot_doc::Document::new(jiffshot_doc::Pixmap::new(1000, 500).unwrap());
        let frame = gif_frame_from_document(&document, 100, Some(480)).unwrap();
        assert_eq!(frame.buffer().dimensions(), (480, 240));
        let frame = gif_frame_from_document(&document, 100, Some(2000)).unwrap();
        assert_eq!(frame.buffer().dimensions(), (1000, 500));
    }

    #[test]
    fn gif_export_preserves_variable_frame_timing() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let source = directory.path().join("variable.mkv");
        let destination = directory.path().join("animation.gif");
        let caps = gst::Caps::builder("video/x-raw")
            .field("format", "RGB")
            .field("width", 32i32)
            .field("height", 24i32)
            .field("framerate", gst::Fraction::new(0, 1))
            .build();
        let appsrc = gst_app::AppSrc::builder()
            .caps(&caps)
            .format(gst::Format::Time)
            .build();
        let muxer = gst::ElementFactory::make("matroskamux")
            .build()
            .expect("GStreamer Matroska muxer should be available");
        let sink = gst::ElementFactory::make("filesink")
            .property("location", source.to_string_lossy().as_ref())
            .build()
            .expect("GStreamer file sink should be available");
        let pipeline = gst::Pipeline::new();
        pipeline
            .add_many([appsrc.upcast_ref(), &muxer, &sink])
            .expect("variable-timing pipeline should be assembled");
        gst::Element::link_many([appsrc.upcast_ref(), &muxer, &sink])
            .expect("variable-timing pipeline should link");
        pipeline
            .set_state(gst::State::Playing)
            .expect("variable-timing pipeline should start");
        for (index, (pts_ms, duration_ms)) in
            [(0, 100), (100, 200), (300, 200)].into_iter().enumerate()
        {
            let mut buffer = gst::Buffer::from_mut_slice(vec![index as u8 * 80; 32 * 24 * 3]);
            let buffer = buffer.get_mut().expect("new buffer should be writable");
            buffer.set_pts(gst::ClockTime::from_mseconds(pts_ms));
            buffer.set_duration(gst::ClockTime::from_mseconds(duration_ms));
            appsrc
                .push_buffer(buffer.to_owned())
                .expect("test video frame should be accepted");
        }
        appsrc
            .end_of_stream()
            .expect("test video source should finish");
        let message = pipeline
            .bus()
            .expect("test pipeline should have a bus")
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("variable-timing video should finish encoding");
        assert!(
            matches!(message.view(), gst::MessageView::Eos(_)),
            "variable-timing test source should finish normally: {message:?}"
        );
        pipeline
            .set_state(gst::State::Null)
            .expect("test video pipeline should stop");

        export_gif(&source, &destination).expect("variable-timing video should export as a GIF");
        let file = File::open(destination).expect("exported GIF should exist");
        let decoder = image::codecs::gif::GifDecoder::new(std::io::BufReader::new(file))
            .expect("exported GIF should be decodable");
        let frames = image::AnimationDecoder::into_frames(decoder)
            .collect_frames()
            .expect("exported GIF frames should decode");
        let delays: Vec<_> = frames
            .iter()
            .map(|frame| frame.delay().numer_denom_ms())
            .collect();
        assert_eq!(
            delays,
            [(100, 1), (200, 1), (200, 1)],
            "GIF must preserve each source presentation interval"
        );
    }

    #[test]
    fn gif_export_preserves_a_single_frame_duration() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let source = directory.path().join("single.avi");
        let destination = directory.path().join("single.gif");
        write_gif_test_video(&source, 1, 10);

        export_gif(&source, &destination).expect("single-frame video should export as a GIF");
        let file = File::open(destination).expect("exported GIF should exist");
        let decoder = image::codecs::gif::GifDecoder::new(std::io::BufReader::new(file))
            .expect("exported GIF should be decodable");
        let frames = image::AnimationDecoder::into_frames(decoder)
            .collect_frames()
            .expect("exported GIF frame should decode");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].delay().numer_denom_ms(), (100, 1));
    }

    #[test]
    fn gif_timing_rounds_cumulatively_and_rejects_excessive_delays() {
        let mut timing = GifTiming::default();
        let delays: Vec<_> = (0..5)
            .map(|_| timing.frame_delay_ms(4_000_000).expect("delay should fit"))
            .collect();
        assert_eq!(delays, [None, Some(10), None, Some(10), None]);
        assert!(
            timing.frame_delay_ms(655_360_000_000).is_err(),
            "a delay exceeding GIF's 16-bit centisecond limit must be rejected"
        );
    }

    #[test]
    fn gif_frames_reject_missing_timestamps_and_corruption() {
        gst::init().expect("GStreamer should initialize");
        let buffer = gst::Buffer::with_size(4).expect("buffer should be allocated");
        let sample = gst::Sample::builder().buffer(&buffer).build();
        let error = GifVideoFrame::from_sample(&sample)
            .err()
            .expect("a frame without a timestamp must be rejected");
        assert!(error.contains("without a timestamp"), "{error}");

        let mut buffer = gst::Buffer::with_size(4).expect("buffer should be allocated");
        let writable = buffer.get_mut().expect("new buffer should be writable");
        writable.set_pts(gst::ClockTime::ZERO);
        writable.set_flags(gst::BufferFlags::CORRUPTED);
        let sample = gst::Sample::builder().buffer(&buffer).build();
        let error = GifVideoFrame::from_sample(&sample)
            .err()
            .expect("a corrupt frame must be rejected");
        assert!(error.contains("corrupted"), "{error}");
    }

    fn write_gif_test_video(path: &Path, num_buffers: i32, fps: i32) {
        let source = gst::ElementFactory::make("videotestsrc")
            .property("num-buffers", num_buffers)
            .build()
            .expect("GStreamer test video source should be available");
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("width", 32i32)
                    .field("height", 24i32)
                    .field("framerate", gst::Fraction::new(fps, 1))
                    .build(),
            )
            .build()
            .expect("GStreamer caps filter should be available");
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
            .add_many([&source, &caps, &encoder, &muxer, &sink])
            .expect("test video pipeline should be assembled");
        gst::Element::link_many([&source, &caps, &encoder, &muxer, &sink])
            .expect("test video elements should link");
        pipeline
            .set_state(gst::State::Playing)
            .expect("test video should start");
        let message = pipeline
            .bus()
            .expect("test pipeline should have a bus")
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("test video should finish encoding");
        assert!(
            matches!(message.view(), gst::MessageView::Eos(_)),
            "test source should finish normally"
        );
        pipeline
            .set_state(gst::State::Null)
            .expect("test video pipeline should stop");
    }

    #[test]
    fn gif_export_does_not_replace_the_source_or_its_links() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let source = directory.path().join("source.avi");
        let hard_link = directory.path().join("source-hard-link.gif");
        let symlink = directory.path().join("source-symlink.gif");
        write_test_video(&source, 4);
        std::fs::hard_link(&source, &hard_link).expect("hard link should be created");
        std::os::unix::fs::symlink(&source, &symlink).expect("symbolic link should be created");
        let original = std::fs::read(&source).expect("source video should be readable");

        for destination in [&source, &hard_link, &symlink] {
            assert!(
                export_gif(&source, destination).is_err(),
                "GIF export must reject the source and aliases: {}",
                destination.display()
            );
            assert_eq!(
                std::fs::read(&source).expect("source video should remain readable"),
                original,
                "rejected GIF output must preserve the source video"
            );
        }
    }

    #[test]
    fn gif_export_preserves_existing_destination_on_decode_failure() {
        gst::init().expect("GStreamer should initialize");
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let source = directory.path().join("invalid.avi");
        let destination = directory.path().join("existing.gif");
        std::fs::write(&source, b"not a video").expect("invalid video should be written");
        std::fs::write(&destination, b"existing output").expect("destination should be written");

        assert!(
            export_gif(&source, &destination).is_err(),
            "invalid video should fail to export"
        );
        assert_eq!(
            std::fs::read(destination).expect("existing destination should remain readable"),
            b"existing output",
            "failed GIF export must not replace an existing file"
        );
    }

    fn write_test_video(path: &Path, num_buffers: i32) {
        let source = gst::ElementFactory::make("videotestsrc")
            .property("num-buffers", num_buffers)
            .build()
            .expect("GStreamer test video source should be available");
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
            .add_many([&source, &encoder, &muxer, &sink])
            .expect("test video pipeline should be assembled");
        gst::Element::link_many([&source, &encoder, &muxer, &sink])
            .expect("test video elements should link");
        pipeline
            .set_state(gst::State::Playing)
            .expect("test video should start");
        let message = pipeline
            .bus()
            .expect("test pipeline should have a bus")
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("test video should finish encoding");
        assert!(
            matches!(message.view(), gst::MessageView::Eos(_)),
            "test source should finish normally"
        );
        pipeline
            .set_state(gst::State::Null)
            .expect("test video pipeline should stop");
    }

}
