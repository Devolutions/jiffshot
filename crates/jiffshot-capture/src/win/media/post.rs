//! Shared, transactional video post-production for the editor and agent control server.

use super::{AudioSpec, Reader, VideoFrame, VideoSpec, Writer, HNS_PER_SECOND};
use jiffshot_doc::{raster, Pixmap};
use serde::Serialize;
use std::fmt;
use std::fs;
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

/// One source range, measured in seconds.
pub type Segment = (PathBuf, f64, f64);

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
    pub audio: Audio,
    /// Static PNG/image or .jsd overlays, timed on the joined output timeline.
    pub overlays: Vec<Overlay>,
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

/// GIF encoding choices, shared with the editor.
#[derive(Clone, Debug)]
pub struct GifOptions {
    pub fps: f64,
    pub max_width: u32,
    pub looped: bool,
    pub fade: bool,
    pub dither: bool,
    /// Static overlays placed after GIF resizing, timed from the output's start.
    pub overlays: Vec<Overlay>,
}

impl Default for GifOptions {
    fn default() -> Self {
        Self {
            fps: 10.0,
            max_width: 960,
            looped: true,
            fade: false,
            dither: true,
            overlays: Vec::new(),
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
}

/// The decoded audio format.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct AudioMetadata {
    pub sample_rate: u32,
    pub channels: u32,
}

/// Inspects a finalized video without playing it.
///
/// # Errors
/// Returns an error if the file is missing or Media Foundation cannot decode it.
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
        let (mut frames, mut duration) = (0_u64, 0.0);
        for frame in decoder.into_frames() {
            let frame = frame.map_err(|e| ExportError(e.to_string()))?;
            let (n, d) = frame.delay().numer_denom_ms();
            duration += n as f64 / d.max(1) as f64 / 1000.0;
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
        });
    }
    let r = Reader::open(path).map_err(|e| ExportError(format!("{}: {e}", path.display())))?;
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
    })
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

/// Fits an image into a BGRA canvas, centered over black.
pub fn paste_fitted(pm: &Pixmap, out: &mut [u8], w: usize, h: usize) {
    if w == 0 || h == 0 || out.len() != w.saturating_mul(h).saturating_mul(4) {
        return;
    }
    let scale = (w as f64 / pm.width() as f64).min(h as f64 / pm.height() as f64);
    let sw = ((pm.width() as f64 * scale).round() as u32).clamp(1, w as u32);
    let sh = ((pm.height() as f64 * scale).round() as u32).clamp(1, h as u32);
    let small = if (sw, sh) == (pm.width(), pm.height()) {
        Some(pm.clone())
    } else {
        raster::resample(pm, sw, sh)
    };
    let Some(small) = small else { return };
    let (x0, y0) = ((w - sw as usize) / 2, (h - sh as usize) / 2);
    for (i, p) in small.pixels().iter().enumerate() {
        let d = ((y0 + i / sw as usize) * w + x0 + i % sw as usize) * 4;
        out[d..d + 4].copy_from_slice(&[p.blue(), p.green(), p.red(), 0]);
    }
}

fn fit_frame(frame: &VideoFrame, w: usize, h: usize) -> Result<Vec<u8>> {
    let pm = to_pixmap(frame).ok_or_else(|| ExportError("invalid decoded frame".into()))?;
    let mut out = vec![0; w * h * 4];
    let (fw, fh) = (frame.width as usize, frame.height as usize);
    if fw >= w && fh >= h && fw - w < 2 && fh - h < 2 {
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
    if segments.is_empty() || segments.len() > 256 {
        return Err(ExportError("provide 1..256 segments".into()));
    }
    let sources: Vec<Metadata> = segments
        .iter()
        .map(|(p, a, b)| {
            let m = inspect(p)?;
            validate_range(*a, *b, m.duration)?;
            Ok(m)
        })
        .collect::<Result<_>>()?;
    let spec = spec(&sources[0], &options)?;
    let (audio, audio_note) = choose_audio(&sources, options.audio)?;
    let total: f64 = segments.iter().map(|(_, a, b)| b - a).sum();
    let overlays = load_overlays(&options.overlays, total)?;
    let paths: Vec<&Path> = segments
        .iter()
        .map(|s| s.0.as_path())
        .chain(options.overlays.iter().map(|o| o.path.as_path()))
        .collect();
    let output = Output::new(out, &paths, "mp4")?;
    let mut writer = Writer::create(&output.staging, spec, audio).map_err(|e| {
        ExportError(format!(
            "cannot encode {}x{} at {} fps: {e}",
            spec.width, spec.height, spec.fps
        ))
    })?;
    let mut base = 0_i64;
    let mut frame_count = 0;
    for (path, from, to) in segments {
        let (start, end) = (hns(*from), hns(*to));
        let mut video = Reader::open(path)?;
        video.seek(start)?;
        let mut current = video
            .next_video()?
            .ok_or_else(|| ExportError("segment has no frames".into()))?;
        let mut next = video.next_video()?;
        let mut sound = if audio.is_some() {
            let mut r = Reader::open(path)?;
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
        let mut n = 0_i64;
        loop {
            let at = start + n * HNS_PER_SECOND / spec.fps as i64;
            if at >= end {
                break;
            }
            while next.as_ref().is_some_and(|f| f.time <= at) {
                current = next
                    .take()
                    .ok_or_else(|| ExportError("missing pending frame".into()))?;
                next = video.next_video()?;
            }
            let finish = (start + (n + 1) * HNS_PER_SECOND / spec.fps as i64).min(end);
            let mut pixels = fit_frame(&current, spec.width as usize, spec.height as usize)?;
            composite_bgra(
                &mut pixels,
                spec.width,
                spec.height,
                &overlays,
                seconds(base + at - start),
            );
            writer.write_frame(&pixels, base + at - start, finish - at)?;
            if let Some(a) = audio {
                while pending.as_ref().is_some_and(|(t, _)| *t < finish) {
                    let (time, pcm) = pending
                        .take()
                        .ok_or_else(|| ExportError("missing audio block".into()))?;
                    if let Some((clipped, relative, duration)) =
                        clip_audio(time, &pcm, a, start, end)
                    {
                        writer.write_audio(clipped, base + relative, duration)?;
                    }
                    pending = match &mut sound {
                        Some(r) => r.next_audio()?,
                        None => None,
                    };
                }
            }
            n += 1;
            frame_count += 1;
            progress(((seconds(base + finish - start)) / total).clamp(0.0, 1.0));
        }
        base += end - start;
    }
    writer.finish()?;
    let mut metadata = inspect(&output.staging)?;
    metadata.path = out.to_path_buf();
    metadata.frames = Some(frame_count);
    metadata.audio_note = audio_note;
    output.publish()?;
    progress(1.0);
    Ok(metadata)
}

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

fn dither(rgba: &mut image::RgbaImage) {
    const BAYER: [[i32; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];
    for (x, y, p) in rgba.enumerate_pixels_mut() {
        let d = (BAYER[(y % 4) as usize][(x % 4) as usize] - 8) * 2;
        for c in 0..3 {
            p.0[c] = (p.0[c] as i32 + d).clamp(0, 255) as u8;
        }
    }
}

struct LoadedOverlay {
    pm: Pixmap,
    x: i32,
    y: i32,
    from: f64,
    to: f64,
}

fn load_overlays(overlays: &[Overlay], duration: f64) -> Result<Vec<LoadedOverlay>> {
    if overlays.len() > 256 {
        return Err(ExportError("provide at most 256 overlays".into()));
    }
    overlays
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
            })
        })
        .collect()
}

fn composite_bgra(pixels: &mut [u8], width: u32, height: u32, overlays: &[LoadedOverlay], at: f64) {
    for o in overlays.iter().filter(|o| at >= o.from && at < o.to) {
        for (i, p) in o.pm.pixels().iter().enumerate() {
            let x = o.x as i64 + (i % o.pm.width() as usize) as i64;
            let y = o.y as i64 + (i / o.pm.width() as usize) as i64;
            if x < 0 || y < 0 || x >= width as i64 || y >= height as i64 {
                continue;
            }
            let index = (y as usize * width as usize + x as usize) * 4;
            let inverse = 255 - p.alpha() as u32;
            for (c, value) in [p.blue(), p.green(), p.red()].into_iter().enumerate() {
                pixels[index + c] = (value as u32
                    + (pixels[index + c] as u32 * inverse + 127) / 255)
                    .min(255) as u8;
            }
        }
    }
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
    use image::codecs::gif::{GifEncoder, Repeat};
    let metadata = inspect(source)?;
    validate_range(from, to, metadata.duration)?;
    if !options.fps.is_finite()
        || !(1.0..=50.0).contains(&options.fps)
        || !(1..=8192).contains(&options.max_width)
    {
        return Err(ExportError(
            "GIF fps must be 1..50 and max_width 1..8192".into(),
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
    let overlays = load_overlays(&options.overlays, to - from)?;
    let paths: Vec<&Path> = std::iter::once(source)
        .chain(options.overlays.iter().map(|o| o.path.as_path()))
        .collect();
    let output = Output::new(out, &paths, "gif")?;
    let mut writer = std::io::BufWriter::new(fs::File::create(&output.staging)?);
    let delay_ms = ((100.0 / options.fps).round().max(1.0) * 10.0) as u32;
    let mut count = 0_u64;
    {
        let mut encoder = GifEncoder::new_with_speed(&mut writer, 10);
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
        loop {
            let output_time = count as f64 * delay_ms as f64 / 1000.0;
            let at = from + output_time;
            if output_time >= to - from {
                break;
            }
            while next.as_ref().is_some_and(|f| seconds(f.time) <= at) {
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
                small.draw_pixmap(
                    o.x,
                    o.y,
                    o.pm.as_ref(),
                    &tiny_skia::PixmapPaint::default(),
                    tiny_skia::Transform::identity(),
                    None,
                );
            }
            let mut rgba = jiffshot_doc::io::to_rgba_image(&small);
            if options.fade && to - at < 0.5 {
                let k = ((to - at) / 0.5).clamp(0.0, 1.0);
                for p in rgba.pixels_mut() {
                    for c in 0..3 {
                        p.0[c] = (p.0[c] as f64 * k) as u8;
                    }
                }
            }
            if options.dither {
                dither(&mut rgba);
            }
            encoder
                .encode_frame(image::Frame::from_parts(
                    rgba,
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(delay_ms, 1),
                ))
                .map_err(|e| ExportError(e.to_string()))?;
            count += 1;
            progress(((at - from) / (to - from)).clamp(0.0, 1.0));
        }
    }
    use std::io::Write;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    let metadata = Metadata {
        path: out.to_path_buf(),
        format: "gif".into(),
        width: w,
        height: h,
        fps: 1000.0 / delay_ms as f64,
        duration: count as f64 * delay_ms as f64 / 1000.0,
        audio: None,
        audio_note: if metadata.audio.is_some() || metadata.audio_note.is_some() {
            Some("GIF cannot retain source audio".into())
        } else {
            None
        },
        bytes: fs::metadata(&output.staging)?.len(),
        frames: Some(count),
    };
    output.publish()?;
    progress(1.0);
    Ok(metadata)
}

struct Output {
    staging: PathBuf,
    destination: PathBuf,
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
                    })
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(ExportError("cannot reserve export staging file".into()))
    }

    fn publish(self) -> Result<()> {
        fs::OpenOptions::new()
            .write(true)
            .open(&self.staging)?
            .sync_all()?;
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
        Ok(())
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.staging);
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

    fn fixture(path: &Path, color: [u8; 4], size: (u32, u32), fps: u32, audio: Option<AudioSpec>) {
        let spec = VideoSpec::for_size(size.0, size.1, fps);
        let mut writer = Writer::create(path, spec, audio).expect("fixture MP4 encoder");
        let frame = color.repeat((size.0 * size.1) as usize);
        for n in 0..fps {
            let at = n as i64 * HNS_PER_SECOND / fps as i64;
            let end = (n + 1) as i64 * HNS_PER_SECOND / fps as i64;
            writer
                .write_frame(&frame, at, end - at)
                .expect("fixture video frame");
            if let Some(a) = audio {
                let samples = a.sample_rate as usize / fps as usize;
                writer
                    .write_audio(
                        &vec![1000; samples * a.channels as usize],
                        at,
                        samples as i64 * HNS_PER_SECOND / a.sample_rate as i64,
                    )
                    .expect("fixture audio");
            }
        }
        writer.finish().expect("finalized fixture MP4");
    }

    #[test]
    fn trims_resamples_joins_and_extracts_finalized_video_without_source_overwrites() {
        let dir = FixtureDir::new();
        let (red, green, out) = (
            dir.path("red.mp4"),
            dir.path("green.mp4"),
            dir.path("joined.mp4"),
        );
        fixture(&red, [0, 0, 240, 0], (128, 96), 12, None);
        fixture(&green, [0, 240, 0, 0], (80, 60), 8, None);
        let segments = [(red.clone(), 0.25, 0.75), (green.clone(), 0.25, 0.75)];
        let mut progress = Vec::new();
        let last = std::sync::Mutex::new(&mut progress);
        let metadata = export_mp4(
            &segments,
            &out,
            Mp4Options {
                fps: Some(20),
                width: Some(64),
                ..Mp4Options::default()
            },
            |value| last.lock().expect("progress").push(value),
        )
        .expect("joined MP4");
        assert_eq!(
            (metadata.width, metadata.height, metadata.frames),
            (64, 48, Some(20))
        );
        assert!(
            (metadata.duration - 1.0).abs() < 0.02,
            "duration {}",
            metadata.duration
        );
        assert_eq!(metadata.path, out);
        assert_eq!(
            metadata.bytes,
            fs::metadata(&out).expect("output file").len()
        );
        assert!(progress.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(progress.last(), Some(&1.0));
        let (first, _) = frame(&out, 0.1).expect("first segment frame");
        let (second, _) = frame(&out, 0.6).expect("second segment frame");
        assert!(first.pixels()[0].red() > 200 && first.pixels()[0].green() < 20);
        assert!(second.pixels()[0].green() > 200 && second.pixels()[0].red() < 20);
        let png = dir.path("still.png");
        let (w, h, at, bytes) = save_frame(&out, &png, 0.6).expect("PNG still");
        assert_eq!((w, h), (64, 48));
        assert!((at - 0.6).abs() < 0.06);
        assert_eq!(bytes, fs::metadata(png).expect("still file").len());
        let original = fs::read(&red).expect("source bytes");
        assert!(export_mp4(&segments, &red, Mp4Options::default(), |_| {}).is_err());
        assert_eq!(fs::read(&red).expect("preserved source"), original);
        assert!(frame(&out, 1.0).is_err());
        assert!(frame(&out, f64::NAN).is_err());
        assert!(export_mp4(&[(red, 0.0, 2.0)], &out, Mp4Options::default(), |_| {}).is_err());
        assert_eq!(
            inspect(&out).expect("previous output survived").frames,
            None
        );
        assert!(fs::read_dir(&dir.0).expect("directory").all(|entry| !entry
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .starts_with(".jiffshot-export")));
    }

    #[test]
    fn encodes_gif_delay_resize_repeat_and_frame_count() {
        use image::AnimationDecoder;
        let dir = FixtureDir::new();
        let (source, out) = (dir.path("source.mp4"), dir.path("animation.gif"));
        fixture(&source, [0, 0, 240, 0], (64, 48), 12, None);
        let metadata = export_gif(
            &source,
            &out,
            (0.25, 0.75),
            GifOptions {
                fps: 20.0,
                max_width: 32,
                fade: true,
                dither: false,
                ..GifOptions::default()
            },
            |_| {},
        )
        .expect("GIF export");
        assert_eq!(
            (metadata.width, metadata.height, metadata.frames),
            (32, 24, Some(10))
        );
        assert_eq!((metadata.duration, metadata.fps), (0.5, 20.0));
        assert!(metadata.audio.is_none());
        let bytes = fs::read(&out).expect("GIF bytes");
        assert!(
            bytes.windows(11).any(|w| w == b"NETSCAPE2.0"),
            "loop extension"
        );
        let decoder = image::codecs::gif::GifDecoder::new(std::io::BufReader::new(
            fs::File::open(out).expect("GIF file"),
        ))
        .expect("GIF decoder");
        let frames = decoder.into_frames().collect_frames().expect("GIF frames");
        assert_eq!(frames.len(), 10);
        assert_eq!(frames[0].delay().numer_denom_ms(), (50, 1));
        assert_eq!(frames[0].buffer().dimensions(), (32, 24));
        assert!(
            frames[0].buffer().get_pixel(0, 0).0[0] > frames[9].buffer().get_pixel(0, 0).0[0],
            "fade changes pixels"
        );
        assert!(export_gif(&source, &source, (0.0, 0.5), GifOptions::default(), |_| {}).is_err());
        assert!(export_gif(
            &source,
            &dir.path("bad.gif"),
            (0.0, 0.5),
            GifOptions {
                fps: 0.0,
                ..GifOptions::default()
            },
            |_| {}
        )
        .is_err());
        assert!(!dir.path("bad.gif").exists());
    }

    #[test]
    fn preserves_compatible_audio_and_mutes_on_request_or_mismatch() {
        let dir = FixtureDir::new();
        let (source, silent) = (dir.path("sound.mp4"), dir.path("silent.mp4"));
        let audio = AudioSpec {
            sample_rate: 48_000,
            channels: 2,
        };
        fixture(&source, [50, 50, 50, 0], (64, 48), 12, Some(audio));
        fixture(&silent, [100, 100, 100, 0], (64, 48), 12, None);
        let range = [(source.clone(), 0.25, 0.75)];
        let kept = export_mp4(&range, &dir.path("kept.mp4"), Mp4Options::default(), |_| {})
            .expect("audio export");
        assert_eq!(
            kept.audio.map(|a| (a.sample_rate, a.channels)),
            Some((48_000, 2))
        );
        let muted = export_mp4(
            &range,
            &dir.path("muted.mp4"),
            Mp4Options {
                audio: Audio::Mute,
                ..Mp4Options::default()
            },
            |_| {},
        )
        .expect("mute export");
        assert!(muted.audio.is_none());
        let joined = export_mp4(
            &[(source, 0.0, 0.5), (silent, 0.0, 0.5)],
            &dir.path("joined.mp4"),
            Mp4Options::default(),
            |_| {},
        )
        .expect("mismatched audio export");
        assert!(joined.audio.is_none());
        assert!(joined
            .audio_note
            .as_ref()
            .is_some_and(|note| note.contains("--mute")));
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
        };
        for (at, expected) in [
            (0.249, [0, 0, 240, 0]),
            (0.25, [0, 100, 120, 0]),
            (0.749, [0, 100, 120, 0]),
            (0.75, [0, 0, 240, 0]),
        ] {
            let mut pixels = vec![0, 0, 240, 0, 0, 0, 240, 0];
            composite_bgra(&mut pixels, 2, 1, std::slice::from_ref(&overlay), at);
            assert_eq!(&pixels[..4], expected.as_slice(), "time {at}");
            assert_eq!(&pixels[4..], &[0, 0, 240, 0], "clipped image at {at}");
        }
    }

    #[test]
    fn exports_timed_image_and_document_overlays_in_both_formats() {
        let dir = FixtureDir::new();
        let source = dir.path("source.mp4");
        fixture(&source, [0, 0, 240, 0], (64, 48), 12, None);
        let overlay_path = dir.path("overlay.png");
        let mut pm = Pixmap::new(10, 10).expect("overlay");
        pm.fill(tiny_skia::Color::from_rgba8(0, 240, 0, 255));
        pm.save_png(&overlay_path).expect("PNG overlay");
        let doc_path = dir.path("overlay.jsd");
        jiffshot_doc::io::save_document(&jiffshot_doc::Document::new(pm), &doc_path)
            .expect("document overlay");
        let make = |path| {
            vec![Overlay {
                path,
                x: 5,
                y: 5,
                from: 0.25,
                to: 0.75,
            }]
        };
        let out = dir.path("overlay.mp4");
        export_mp4(
            &[(source.clone(), 0.0, 1.0)],
            &out,
            Mp4Options {
                overlays: make(doc_path),
                ..Mp4Options::default()
            },
            |_| {},
        )
        .expect("MP4 overlay");
        let before = frame(&out, 0.1).expect("before overlay").0;
        let during = frame(&out, 0.5).expect("during overlay").0;
        let after = frame(&out, 0.9).expect("after overlay").0;
        let i = 8 * 64 + 8;
        assert!(before.pixels()[i].red() > 200);
        assert!(during.pixels()[i].green() > 200 && during.pixels()[i].red() < 30);
        assert!(after.pixels()[i].red() > 200);
        let gif = dir.path("overlay.gif");
        let exported = export_gif(
            &source,
            &gif,
            (0.0, 1.0),
            GifOptions {
                fps: 20.0,
                overlays: make(overlay_path),
                ..GifOptions::default()
            },
            |_| {},
        )
        .expect("GIF overlay");
        let inspected = inspect(&gif).expect("GIF inspection");
        assert_eq!(
            (exported.width, exported.height, exported.frames),
            (inspected.width, inspected.height, inspected.frames)
        );
        assert!((inspected.duration - 1.0).abs() < 0.0001);
        let before = frame(&gif, 0.1).expect("GIF before overlay").0;
        let during = frame(&gif, 0.5).expect("GIF during overlay").0;
        let after = frame(&gif, 0.9).expect("GIF after overlay").0;
        assert!(before.pixels()[i].red() > 200);
        assert!(during.pixels()[i].green() > 200 && during.pixels()[i].red() < 30);
        assert!(after.pixels()[i].red() > 200);
        let invalid = Mp4Options {
            overlays: vec![Overlay {
                path: gif,
                x: 0,
                y: 0,
                from: 0.0,
                to: 2.0,
            }],
            ..Mp4Options::default()
        };
        assert!(export_mp4(&[(source, 0.0, 1.0)], &dir.path("bad.mp4"), invalid, |_| {}).is_err());
        assert!(!dir.path("bad.mp4").exists());
    }
}
