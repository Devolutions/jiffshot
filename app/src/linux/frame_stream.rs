//! Shared frame conversion for desktop portal streams.

use gst_video::prelude::*;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use jiffshot_capture::Frame;
use jiffshot_doc::{Document, Pixmap};

const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

/// Discard the frame cached before hiding the UI, not just its decoded pixels.
pub(super) fn screenshot_sample(
    sink: &gst_app::AppSink,
    previous: gst::Sample,
) -> Result<gst::Sample, String> {
    let reference = std::time::Instant::now();
    let running_time = sink.current_running_time();
    let hidden = jiffshot_capture::visibility::hide().map_err(|error| error.to_string())?;
    let Some(hidden_since) = hidden.hidden_since() else {
        return Ok(previous);
    };
    let running_time = running_time.ok_or("the screenshot stream has no running clock")?.nseconds();
    let elapsed = if hidden_since >= reference {
        hidden_since.duration_since(reference)
    } else {
        reference.duration_since(hidden_since)
    };
    let offset = u64::try_from(elapsed.as_nanos())
        .map_err(|_| "the screenshot visibility timestamp is out of range")?;
    let cutoff = if hidden_since >= reference {
        running_time.checked_add(offset).ok_or("the screenshot timestamp overflowed")?
    } else {
        running_time.saturating_sub(offset)
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while std::time::Instant::now() < deadline {
        let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(100)) else {
            continue;
        };
        let pts = sample.buffer().and_then(|buffer| buffer.pts())
            .ok_or("the screenshot stream returned a frame without a timestamp")?;
        let segment = sample.segment().and_then(|segment| segment.downcast_ref::<gst::ClockTime>())
            .ok_or("the screenshot stream returned a frame without a time segment")?;
        let timestamp = segment.to_running_time(pts)
            .ok_or("the screenshot frame is outside its time segment")?;
        if timestamp.nseconds() >= cutoff {
            return Ok(sample);
        }
    }
    Err("timed out waiting for a screenshot frame with Jiffshot hidden".to_owned())
}

pub(super) fn with_source<T>(
    source: gst::Element,
    description: &str,
    capture: impl FnOnce(&gst_app::AppSink, &gst::Bus) -> Result<T, String>,
) -> Result<T, String> {
    let convert = gst::ElementFactory::make("videoconvert")
        .build()
        .map_err(|error| format!("could not create {description} converter: {error}"))?;
    let sink = gst_app::AppSink::builder()
        .caps(
            &gst::Caps::builder("video/x-raw")
                .field("format", "BGRA")
                .build(),
        )
        .sync(false)
        .max_buffers(1)
        .drop(true)
        .build();
    let pipeline = gst::Pipeline::new();
    pipeline
        .add_many([&source, &convert, sink.upcast_ref()])
        .map_err(|error| format!("could not build {description} pipeline: {error}"))?;
    gst::Element::link_many([&source, &convert, sink.upcast_ref()])
        .map_err(|error| format!("could not connect {description} pipeline: {error}"))?;
    let bus = pipeline
        .bus()
        .ok_or_else(|| format!("{description} pipeline has no event bus"))?;
    let result = pipeline
        .set_state(gst::State::Playing)
        .map_err(|error| format!("could not start {description}: {error}"))
        .and_then(|_| capture(&sink, &bus));
    let stopped = pipeline
        .set_state(gst::State::Null)
        .map_err(|error| format!("could not stop {description}: {error}"));
    match (result, stopped) {
        (Ok(value), Ok(_)) => Ok(value),
        (Err(error), Ok(_)) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(stop_error)) => Err(format!("{error}; {stop_error}")),
    }
}

pub(super) fn frame_from_sample(sample: &gst::Sample) -> Result<Frame, String> {
    let caps = sample
        .caps()
        .ok_or_else(|| "screen capture returned no frame format".to_owned())?;
    let info = gst_video::VideoInfo::from_caps(caps)
        .map_err(|error| format!("invalid screen capture frame format: {error}"))?;
    if info.format() != gst_video::VideoFormat::Bgra {
        return Err("screen capture returned an unsupported pixel format".to_owned());
    }
    let (width, height) = (info.width(), info.height());
    let row_bytes = (width as usize)
        .checked_mul(4)
        .ok_or_else(|| "screen capture width is too large".to_owned())?;
    let bytes = row_bytes
        .checked_mul(height as usize)
        .filter(|&bytes| bytes > 0 && bytes <= MAX_FRAME_BYTES)
        .ok_or_else(|| "screen capture frame is empty or too large".to_owned())?;
    let buffer = sample
        .buffer()
        .ok_or_else(|| "screen capture returned an empty frame".to_owned())?;
    let video = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
        .map_err(|error| format!("could not map screen capture frame: {error}"))?;
    let stride = usize::try_from(video.plane_stride()[0])
        .map_err(|_| "screen capture returned a negative row stride".to_owned())?;
    if stride < row_bytes {
        return Err("screen capture returned an invalid row stride".to_owned());
    }
    let plane = video
        .plane_data(0)
        .map_err(|error| format!("could not read screen capture frame: {error}"))?;
    let mut pixels = Vec::with_capacity(bytes);
    for row in 0..height as usize {
        let start = row
            .checked_mul(stride)
            .ok_or_else(|| "screen capture row offset is too large".to_owned())?;
        let end = start
            .checked_add(row_bytes)
            .ok_or_else(|| "screen capture row offset is too large".to_owned())?;
        pixels.extend_from_slice(
            plane
                .get(start..end)
                .ok_or_else(|| "screen capture returned an incomplete frame".to_owned())?,
        );
    }
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel[3] = 255;
    }
    Ok(Frame {
        width,
        height,
        pixels,
        alpha: false,
    })
}

pub(super) fn document_from_frame(mut frame: Frame) -> Result<Document, String> {
    for pixel in frame.pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
        if !frame.alpha {
            pixel[3] = 255;
        }
    }
    let size = tiny_skia::IntSize::from_wh(frame.width, frame.height)
        .ok_or_else(|| "captured frame has invalid dimensions".to_owned())?;
    let pixmap = Pixmap::from_vec(frame.pixels, size)
        .ok_or_else(|| "captured frame has an invalid pixel buffer".to_owned())?;
    Ok(Document::new(pixmap))
}
