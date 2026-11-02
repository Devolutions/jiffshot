//! Video in the editor: scrubbing and playback (frames shown on the
//! canvas), trimming, export to MP4 or animated GIF, and taking a frame
//! into the image editor. Decoding and encoding use Media Foundation or
//! AVFoundation through `jiffshot_capture::media`.

use crate::app::{self, App};
use crate::canvas::with_editor;
use jiffshot_capture::media::{
    AudioBlock, AudioCapture, AudioSpec, Reader, VideoFrame, VideoSpec, Writer, HNS_PER_SECOND,
};
use jiffshot_doc::{raster, Document, Pixmap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn hns(seconds: f64) -> i64 {
    (seconds * HNS_PER_SECOND as f64) as i64
}

fn seconds(hns: i64) -> f64 {
    hns as f64 / HNS_PER_SECOND as f64
}

/// A decoded frame as an opaque pixmap.
pub fn to_pixmap(frame: &VideoFrame) -> Option<Pixmap> {
    let mut rgba = Vec::with_capacity(frame.bgra.len());
    for p in frame.bgra.chunks_exact(4) {
        rgba.extend_from_slice(&[p[2], p[1], p[0], 255]);
    }
    Pixmap::from_vec(
        rgba,
        tiny_skia::IntSize::from_wh(frame.width, frame.height)?,
    )
}

struct Player {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
    sound: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "macos")]
impl Player {
    fn request_shutdown(self) -> PlaybackShutdown {
        self.stop.store(true, Ordering::Release);
        PlaybackShutdown(self)
    }
}

#[cfg(target_os = "macos")]
pub(crate) struct PlaybackShutdown(Player);

#[cfg(target_os = "macos")]
impl PlaybackShutdown {
    pub(crate) fn join(self) -> Result<(), String> {
        let frame = self
            .0
            .thread
            .join()
            .map_err(|_| "the video playback thread panicked".to_owned());
        let sound = self
            .0
            .sound
            .map(|thread| {
                thread
                    .join()
                    .map_err(|_| "the video audio thread panicked".to_owned())
            })
            .unwrap_or(Ok(()));
        frame.and(sound)
    }
}

pub struct Session {
    path: PathBuf,
    reader: Reader,
    position: f64,
    trim: (f64, f64),
    player: Option<Player>,
    current: Option<Pixmap>,
    /// Playback volume as 0..=0xFFFF, read by the sound thread.
    volume: Arc<AtomicU32>,
}

impl Session {
    pub fn open(path: &Path) -> Result<Session, String> {
        let reader = Reader::open(path)?;
        let duration = seconds(reader.duration);
        Ok(Session {
            path: path.to_path_buf(),
            reader,
            position: 0.0,
            trim: (0.0, duration),
            player: None,
            current: None,
            volume: Arc::new(AtomicU32::new(0xFFFF)),
        })
    }

    pub fn duration(&self) -> f64 {
        seconds(self.reader.duration)
    }

    pub fn info(&self) -> String {
        format!(
            "{} × {} · {:.0} fps · {}",
            self.reader.width,
            self.reader.height,
            self.reader.fps,
            if self.reader.audio.is_some() {
                "with audio"
            } else {
                "no audio"
            }
        )
    }

    pub fn frame_at(&mut self, at: f64) -> Option<Pixmap> {
        let frame = self.reader.frame_at(hns(at)).ok().flatten()?;
        let pm = to_pixmap(&frame);
        self.current = pm.clone();
        pm
    }

    fn stop_playing(&mut self) -> bool {
        match self.player.take() {
            Some(p) => {
                p.stop.store(true, Ordering::Release);
                let _ = p.thread.join();
                if let Some(s) = p.sound {
                    let _ = s.join();
                }
                true
            }
            None => false,
        }
    }

    pub fn stop(mut self) {
        self.stop_playing();
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn stop_for_shutdown(mut self) -> Option<PlaybackShutdown> {
        self.player.take().map(Player::request_shutdown)
    }
}

/// Puts a frame on the canvas, keeping the view.
fn show(app: &mut App, pm: Pixmap) {
    with_editor(|e| e.load(Document::new(pm)));
    app.bump_frame();
}

fn publish(app: &mut App) {
    let Some(v) = &app.video else { return };
    let (position, trim, playing) = (v.position, v.trim, v.player.is_some());
    if let Some(sink) = &app.sink {
        let _ = sink.set_video_position(position);
        let _ = sink.set_trim_start(trim.0);
        let _ = sink.set_trim_end(trim.1);
        let _ = sink.set_playing(playing);
    }
}

/// The slider moved.
pub fn seek(app: &mut App, at: f64) {
    let Some(v) = app.video.as_mut() else { return };
    if (v.position - at).abs() < 1e-3 {
        return;
    }
    v.stop_playing();
    v.position = at.clamp(0.0, v.duration());
    let at = v.position;
    if let Some(pm) = v.frame_at(at) {
        show(app, pm);
    }
    publish(app);
}

fn play(app: &mut App) {
    let Some(v) = app.video.as_mut() else { return };
    if v.stop_playing() {
        return publish(app);
    }
    let (path, mut from, end) = (v.path.clone(), v.position, v.trim.1);
    if from >= end - 0.05 {
        from = v.trim.0;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let sound = v.reader.audio.is_some().then(|| {
        let (path, stop, volume) = (path.clone(), stop.clone(), v.volume.clone());
        std::thread::spawn(move || sound::play(&path, from, end, &stop, &volume))
    });
    // Frames go to the UI thread one at a time; if it is still showing the
    // last one, the next is dropped rather than queued.
    let showing = Arc::new(AtomicBool::new(false));
    let thread = std::thread::spawn(move || {
        let Ok(mut reader) = Reader::open(&path) else {
            return;
        };
        if reader.seek(hns(from)).is_err() {
            return;
        }
        let started = Instant::now();
        while !flag.load(Ordering::Acquire) {
            let Ok(Some(frame)) = reader.next_video() else {
                break;
            };
            let t = seconds(frame.time);
            if t < from - 0.02 {
                continue;
            }
            if t > end {
                break;
            }
            let wait = (t - from) - started.elapsed().as_secs_f64();
            if wait > 0.0 {
                std::thread::sleep(Duration::from_secs_f64(wait.min(0.5)));
            }
            if showing.swap(true, Ordering::AcqRel) {
                continue;
            }
            let Some(pm) = to_pixmap(&frame) else { break };
            let done = showing.clone();
            app::post(move |app| {
                if let Some(v) = app.video.as_mut() {
                    v.position = t;
                    v.current = Some(pm.clone());
                    show(app, pm);
                    if let Some(sink) = &app.sink {
                        let _ = sink.set_video_position(t);
                    }
                }
                done.store(false, Ordering::Release);
            });
        }
        app::post(|app| {
            if let Some(v) = app.video.as_mut() {
                if let Some(p) = v.player.take() {
                    p.stop.store(true, Ordering::Release);
                }
            }
            publish(app);
        });
    });
    v.player = Some(Player {
        stop,
        thread,
        sound,
    });
    publish(app);
}

/// Fits a frame into `w` x `h` (BGRA), scaled to fit and centered on
/// black; a frame of that size is copied as it is.
fn fit_frame(frame: &VideoFrame, w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 4];
    let (fw, fh) = (frame.width as usize, frame.height as usize);
    if fw >= w && fh >= h && fw - w < 2 && fh - h < 2 {
        // Even sizes for H.264: drop an odd last row or column.
        for y in 0..h {
            let src = y * fw * 4;
            out[y * w * 4..(y + 1) * w * 4].copy_from_slice(&frame.bgra[src..src + w * 4]);
        }
        return out;
    }
    if let Some(pm) = to_pixmap(frame) {
        paste_fitted(&pm, &mut out, w, h);
    }
    out
}

/// Draws `pm` scaled to fit and centered into the BGRA buffer `out`.
fn paste_fitted(pm: &Pixmap, out: &mut [u8], w: usize, h: usize) {
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
        let (x, y) = (i % sw as usize, i / sw as usize);
        // Premultiplied, so this is the picture over black.
        let d = ((y0 + y) * w + x0 + x) * 4;
        out[d..d + 4].copy_from_slice(&[p.blue(), p.green(), p.red(), 0]);
    }
}

/// One piece of a new video: `(file, from, to)` in seconds.
pub type Segment = (PathBuf, f64, f64);

/// Writes the segments one after another to a new MP4 (re-encoded) at the
/// first one's size; others are fitted in. Audio is kept when `audio` is
/// set and every source has the same sound format.
fn export_segments(
    segments: &[Segment],
    out: &Path,
    audio: bool,
    progress: impl Fn(f64),
) -> Result<(), String> {
    let first = Reader::open(&segments.first().ok_or("nothing to export")?.0)?;
    let spec = VideoSpec::for_size(first.width, first.height, first.fps.round().max(1.0) as u32);
    let usable = |a: Option<jiffshot_capture::media::AudioSpec>| {
        a.filter(|a| (a.sample_rate == 44_100 || a.sample_rate == 48_000) && a.channels <= 2)
    };
    let mut audio_spec = if audio { usable(first.audio) } else { None };
    drop(first);
    for (path, ..) in &segments[1..] {
        if audio_spec.is_some() && usable(Reader::open(path)?.audio) != audio_spec {
            audio_spec = None;
        }
    }
    let mut writer = Writer::create(out, spec, audio_spec)?;
    let frame_time = HNS_PER_SECOND / spec.fps as i64;
    let rate = audio_spec.map_or(48_000, |a| a.sample_rate) as i64;
    let channels = audio_spec.map_or(2, |a| a.channels) as i64;
    let (w, h) = (spec.width as usize, spec.height as usize);
    let total = segments
        .iter()
        .map(|(_, a, b)| (b - a).max(0.0))
        .sum::<f64>()
        .max(0.01);
    let mut base = 0i64;
    let mut done = 0.0;
    for (source, from, to) in segments {
        let mut video = Reader::open(source)?;
        let mut sound = match audio_spec {
            Some(_) => {
                let mut r = Reader::open(source)?;
                r.select_video(false)?;
                r.select_audio(true)?;
                r.seek(hns(*from))?;
                Some(r)
            }
            None => None,
        };
        let (start, end) = (hns(*from), hns(*to));
        video.seek(start)?;
        let mut pending_audio: Option<(i64, Vec<i16>)> = None;
        let mut audio_done = sound.is_none();
        while let Some(frame) = video.next_video()? {
            if frame.time + frame_time / 2 < start {
                continue;
            }
            if frame.time >= end {
                break;
            }
            writer.write_frame(
                &fit_frame(&frame, w, h),
                base + (frame.time - start).max(0),
                frame_time,
            )?;
            // Audio up to this frame's time, from the second reader.
            while !audio_done {
                let block = match pending_audio.take() {
                    Some(b) => Some(b),
                    None => sound.as_mut().unwrap().next_audio()?,
                };
                let Some((at, pcm)) = block else {
                    audio_done = true;
                    break;
                };
                if at > frame.time + frame_time {
                    pending_audio = Some((at, pcm));
                    break;
                }
                if at >= start && at < end {
                    let n = pcm.len() as i64 / channels;
                    writer.write_audio(&pcm, base + at - start, n * HNS_PER_SECOND / rate)?;
                }
            }
            progress((done + seconds(frame.time - start)) / total);
        }
        base += end - start;
        done += to - from;
    }
    writer.finish()
}

/// Copies the trimmed range to a new MP4 (re-encoded, audio included).
fn export_mp4(
    source: &Path,
    out: &Path,
    (from, to): (f64, f64),
    progress: impl Fn(f64),
) -> Result<(), String> {
    export_segments(&[(source.to_path_buf(), from, to)], out, true, progress)
}

/// GIF choices: frames a second, largest width, loop, fade the end to
/// black, dither.
#[derive(Clone, Copy, Debug)]
pub struct GifOptions {
    pub fps: f64,
    pub max_width: u32,
    pub looped: bool,
    pub fade: bool,
    pub dither: bool,
}

/// A 4x4 ordered-dither offset, so smooth gradients survive the 256-color
/// palette as fine patterns instead of bands.
fn dither(rgba: &mut image::RgbaImage) {
    const BAYER: [[i32; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];
    for (x, y, p) in rgba.enumerate_pixels_mut() {
        let d = (BAYER[(y % 4) as usize][(x % 4) as usize] - 8) * 2;
        for c in 0..3 {
            p.0[c] = (p.0[c] as i32 + d).clamp(0, 255) as u8;
        }
    }
}

fn slide_frame_counts(
    seconds: &[f64],
    count: usize,
    fps: u32,
    narrated: bool,
) -> Result<Vec<usize>, String> {
    if fps == 0 {
        return Err("invalid slideshow frame rate".into());
    }
    let minimum = if narrated { 1.0 / f64::from(fps) } else { 0.5 };
    let mut exact_end = 0.0;
    let mut previous_end = 0usize;
    let mut result = Vec::with_capacity(count);
    for i in 0..count {
        let duration = seconds.get(i).or(seconds.last()).copied().unwrap_or(3.0);
        if !duration.is_finite() {
            return Err("invalid slideshow duration".into());
        }
        let frames = duration.max(minimum) * f64::from(fps);
        exact_end += frames;
        if !exact_end.is_finite() || exact_end >= usize::MAX as f64 {
            return Err("the slideshow is too long".into());
        }
        let end = if narrated {
            (exact_end.round() as usize).max(
                previous_end
                    .checked_add(1)
                    .ok_or("the slideshow is too long")?,
            )
        } else {
            previous_end
                .checked_add(frames.round() as usize)
                .ok_or("the slideshow is too long")?
        };
        result.push(end - previous_end);
        previous_end = end;
    }
    Ok(result)
}

/// Makes a video from pictures, each shown for its time in `seconds` (the
/// last time repeats for the rest) with a short cross-fade, at the size of
/// the largest (at most 1920 x 1080), with `sound` (16-bit PCM) as its
/// soundtrack.
fn slideshow(
    images: &[PathBuf],
    out: &Path,
    seconds: &[f64],
    sound: Option<(jiffshot_capture::media::AudioSpec, Vec<i16>)>,
    progress: impl Fn(f64),
) -> Result<(), String> {
    let pictures: Vec<Pixmap> = images
        .iter()
        .map(|path| {
            jiffshot_doc::io::load_image(path)
                .map_err(|error| format!("Could not open {}: {error}", path.display()))
        })
        .collect::<Result<_, _>>()?;
    if pictures.is_empty() {
        return Err("none of the files is a picture".into());
    }
    let (w, h) = pictures.iter().fold((0u32, 0u32), |(w, h), p| {
        (w.max(p.width()), h.max(p.height()))
    });
    let fit = (1920.0 / w as f64).min(1080.0 / h as f64).min(1.0);
    let spec = VideoSpec::for_size(
        ((w as f64 * fit) as u32).max(16),
        ((h as f64 * fit) as u32).max(16),
        30,
    );
    let (w, h) = (spec.width as usize, spec.height as usize);
    let slides: Vec<Vec<u8>> = pictures
        .iter()
        .map(|pm| {
            let mut buf = vec![0u8; w * h * 4];
            paste_fitted(pm, &mut buf, w, h);
            buf
        })
        .collect();
    let slide_frames = slide_frame_counts(seconds, slides.len(), spec.fps, sound.is_some())?;
    let mut writer = Writer::create(out, spec, sound.as_ref().map(|s| s.0))?;
    let frame_time = HNS_PER_SECOND / spec.fps as i64;
    let total: usize = slide_frames.iter().sum();
    if let Some((a, pcm)) = &sound {
        // The soundtrack, in tenth-of-a-second blocks, up to the video's end.
        let ch = a.channels.max(1) as usize;
        let block = a.sample_rate as usize / 10 * ch;
        let limit = (total as f64 / spec.fps as f64 * a.sample_rate as f64) as usize * ch;
        for (n, chunk) in pcm[..pcm.len().min(limit)].chunks(block).enumerate() {
            let at = n as i64 * HNS_PER_SECOND / 10;
            writer.write_audio(
                chunk,
                at,
                (chunk.len() / ch) as i64 * HNS_PER_SECOND / a.sample_rate as i64,
            )?;
        }
    }
    let mut mixed = vec![0u8; w * h * 4];
    let (mut i, mut start) = (0usize, 0usize);
    for n in 0..total {
        while n >= start + slide_frames[i] {
            start += slide_frames[i];
            i += 1;
        }
        let (per, k) = (slide_frames[i], n - start);
        let fade = (spec.fps as usize / 2).min(per / 2);
        let frame = match slides.get(i + 1) {
            Some(next) if k >= per - fade => {
                let t = (k + fade + 1 - per) as u32 * 256 / (fade as u32 + 1);
                for ((m, a), b) in mixed.iter_mut().zip(&slides[i]).zip(next) {
                    *m = ((*a as u32 * (256 - t) + *b as u32 * t) >> 8) as u8;
                }
                &mixed
            }
            _ => &slides[i],
        };
        writer.write_frame(frame, n as i64 * frame_time, frame_time)?;
        progress(n as f64 / total as f64);
    }
    writer.finish()
}

/// Writes the trimmed range as an animated GIF.
fn export_gif(
    source: &Path,
    out: &Path,
    (from, to): (f64, f64),
    options: GifOptions,
    progress: impl Fn(f64),
) -> Result<(), String> {
    use image::codecs::gif::{GifEncoder, Repeat};
    let GifOptions { fps, max_width, .. } = options;
    let mut reader = Reader::open(source)?;
    let scale = (max_width as f64 / reader.width as f64).min(1.0);
    let (w, h) = (
        ((reader.width as f64 * scale).round() as u32).max(1),
        ((reader.height as f64 * scale).round() as u32).max(1),
    );
    let file = std::fs::File::create(out).map_err(|e| e.to_string())?;
    let mut encoder = GifEncoder::new_with_speed(std::io::BufWriter::new(file), 10);
    encoder
        .set_repeat(if options.looped {
            Repeat::Infinite
        } else {
            Repeat::Finite(0)
        })
        .map_err(|e| e.to_string())?;
    reader.seek(hns(from))?;
    let step = 1.0 / fps;
    let mut next = from;
    let delay = image::Delay::from_numer_denom_ms((1000.0 / fps).round() as u32, 1);
    while let Some(frame) = reader.next_video()? {
        let t = seconds(frame.time);
        if t > to {
            break;
        }
        if t + 1e-3 < next {
            continue;
        }
        next += step;
        let Some(pm) = to_pixmap(&frame) else {
            continue;
        };
        let small = if scale < 1.0 {
            raster::resample(&pm, w, h).ok_or("resize failed")?
        } else {
            pm
        };
        let mut rgba = jiffshot_doc::io::to_rgba_image(&small);
        // The last half second fades to black.
        let left = to - t;
        if options.fade && left < 0.5 {
            let k = (left / 0.5).clamp(0.0, 1.0);
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
            .encode_frame(image::Frame::from_parts(rgba, 0, 0, delay))
            .map_err(|e| e.to_string())?;
        progress((t - from) / (to - from).max(0.01));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
static PENDING_EXPORTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(target_os = "macos")]
static PENDING_NARRATION_SAVES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(target_os = "macos")]
pub(crate) struct PendingWork(&'static std::sync::atomic::AtomicUsize);

#[cfg(target_os = "macos")]
impl PendingWork {
    fn new(counter: &'static std::sync::atomic::AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self(counter)
    }
}

#[cfg(target_os = "macos")]
impl Drop for PendingWork {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn pending_work() -> bool {
    PENDING_EXPORTS.load(Ordering::Acquire) != 0
        || PENDING_NARRATION_SAVES.load(Ordering::Acquire) != 0
}

#[cfg(target_os = "macos")]
pub(crate) fn with_pending_export_for_selftest(f: impl FnOnce()) {
    let _pending = PendingWork::new(&PENDING_EXPORTS);
    f();
}

#[cfg(target_os = "macos")]
pub(crate) fn hold_pending_export_for_selftest() -> PendingWork {
    PendingWork::new(&PENDING_EXPORTS)
}

/// Runs `work` (given a progress callback) off the UI thread, reporting
/// progress in the status bar; opens the result when `open` is set.
fn in_background(
    out: PathBuf,
    open: bool,
    work: impl FnOnce(&Path, &dyn Fn(f64)) -> Result<(), String> + Send + 'static,
) {
    #[cfg(target_os = "macos")]
    let pending = PendingWork::new(&PENDING_EXPORTS);
    std::thread::spawn(move || {
        let started = Instant::now();
        let last = std::sync::Mutex::new(Instant::now());
        let progress = |f: f64| {
            let mut l = last.lock().unwrap();
            if l.elapsed() > Duration::from_millis(400) {
                *l = Instant::now();
                let pct = (f * 100.0).clamp(0.0, 100.0);
                app::post(move |app| {
                    if app.narration.is_none() && app.narration_request.is_none() {
                        app.set_status(format!("Exporting… {pct:.0}%"));
                    }
                });
            }
        };
        let result = work(&out, &progress);
        let secs = started.elapsed().as_secs_f64();
        app::post(move |app| {
            #[cfg(target_os = "macos")]
            let _pending = pending;
            let narrating = app.narration.is_some() || app.narration_request.is_some();
            match result {
                Ok(()) if narrating => {
                    app.toast("Video exported", &out.display().to_string(), false)
                }
                Err(error) if narrating => app.toast("Video export failed", &error, true),
                Ok(()) => {
                    if open {
                        app.open_path(&out);
                    }
                    app.set_status(format!("Exported {} in {secs:.1} s", out.display()));
                }
                Err(error) => app.set_status(format!("Export failed: {error}")),
            }
        });
    });
}

fn stem_of(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Recording".into())
}

fn run_export(app: &mut App, gif: bool) {
    let Some(v) = app.video.as_mut() else { return };
    v.stop_playing();
    let (source, trim) = (v.path.clone(), v.trim);
    let (kind, ext) = if gif {
        ("Animated GIF", "gif")
    } else {
        ("MP4 video", "mp4")
    };
    let name = format!("{} (selection).{ext}", stem_of(&source));
    let prefs = app.settings.video.clone();
    app.pick_save(
        if gif {
            "Export GIF"
        } else {
            "Save selection as"
        },
        &name,
        &[(kind, ext)],
        move |app, out| {
            if out == source {
                return app.set_status("Choose a different file than the one being edited.");
            }
            app.set_status(format!("Exporting {}…", out.display()));
            let (fps, max_width) = prefs.gif();
            let options = GifOptions {
                fps,
                max_width,
                looped: prefs.gif_loop,
                fade: prefs.gif_fade,
                dither: prefs.gif_dither,
            };
            in_background(out, false, move |out, progress| {
                if gif {
                    export_gif(&source, out, trim, options, progress)
                } else {
                    export_mp4(&source, out, trim, progress)
                }
            });
        },
    );
}

/// Saves the video without the part between the trim marks.
fn cut_selection(app: &mut App) {
    let Some(v) = app.video.as_mut() else { return };
    v.stop_playing();
    let (source, (a, b), duration) = (v.path.clone(), v.trim, v.duration());
    if a <= 0.05 && b >= duration - 0.05 {
        return app.set_status("Set the start and end of the part to cut first.");
    }
    let segments: Vec<Segment> = [(source.clone(), 0.0, a), (source.clone(), b, duration)]
        .into_iter()
        .filter(|(_, x, y)| y - x > 0.05)
        .collect();
    let name = format!("{} (cut).mp4", stem_of(&source));
    app.pick_save(
        "Save the video without the selection",
        &name,
        &[("MP4 video", "mp4")],
        move |app, out| {
            if out == source {
                return app.set_status("Choose a different file than the one being edited.");
            }
            app.set_status(format!("Cutting {a:.1} s to {b:.1} s…"));
            in_background(out, true, move |out, progress| {
                export_segments(&segments, out, true, progress)
            });
        },
    );
}

fn silent_copy(app: &mut App) {
    let Some(v) = app.video.as_mut() else { return };
    v.stop_playing();
    let (source, duration) = (v.path.clone(), v.duration());
    let name = format!("{} (silent).mp4", stem_of(&source));
    app.pick_save(
        "Save a silent copy",
        &name,
        &[("MP4 video", "mp4")],
        move |app, out| {
            if out == source {
                return app.set_status("Choose a different file than the one being edited.");
            }
            in_background(out, false, move |out, progress| {
                export_segments(&[(source, 0.0, duration)], out, false, progress)
            });
        },
    );
}

/// Joins `files` end to end, asking where to save.
pub fn combine_files(app: &mut App, files: Vec<PathBuf>) {
    let mut segments = Vec::new();
    for f in &files {
        match Reader::open(f) {
            Ok(r) => segments.push((f.clone(), 0.0, seconds(r.duration))),
            Err(error) => return app.set_status(format!("{}: {error}", f.display())),
        }
    }
    let name = format!("{} (combined).mp4", stem_of(&files[0]));
    app.pick_save(
        "Save the combined video",
        &name,
        &[("MP4 video", "mp4")],
        move |app, out| {
            app.set_status(format!("Combining {} videos…", segments.len()));
            in_background(out, true, move |out, progress| {
                export_segments(&segments, out, true, progress)
            });
        },
    );
}

/// Joins videos end to end (the open one first).
fn combine(app: &mut App) {
    let current = app.video.as_mut().map(|v| {
        v.stop_playing();
        v.path.clone()
    });
    app.pick_files(
        "Videos to add at the end",
        true,
        "MP4 videos",
        &["mp4"],
        move |app, picked| {
            let mut files: Vec<PathBuf> = current.into_iter().collect();
            files.extend(picked);
            if files.len() < 2 {
                return app.set_status("Pick at least two videos to combine.");
            }
            combine_files(app, files);
        },
    );
}

fn from_images(app: &mut App) {
    let seconds_each = app.settings.video.slide_seconds;
    app.pick_files(
        "Pictures for the video",
        true,
        "Images",
        &["png", "jpg", "jpeg", "bmp", "gif", "webp"],
        move |app, images| {
            if images.is_empty() {
                return;
            }
            let name = format!("{} (slideshow).mp4", stem_of(&images[0]));
            app.pick_save(
                "Save the video",
                &name,
                &[("MP4 video", "mp4")],
                move |app, out| {
                    app.set_status(format!("Making a video of {} pictures…", images.len()));
                    in_background(out, true, move |out, progress| {
                        slideshow(&images, out, &[seconds_each], None, progress)
                    });
                },
            );
        },
    );
}

/// A narration: the pictures shown one at a time in the editor while
/// the microphone records; Next moves on and marks the time.
pub struct Narration {
    id: u64,
    images: Vec<PathBuf>,
    index: usize,
    started: Option<Instant>,
    marks: Vec<f64>,
    spec: Option<AudioSpec>,
    sound: Arc<Mutex<NarrationSound>>,
    stop: Arc<AtomicBool>,
    stop_at: Arc<Mutex<Option<Instant>>>,
    finished_at: Option<Instant>,
    finish_error: Option<String>,
    thread: Option<std::thread::JoinHandle<Result<(), String>>>,
}

impl Narration {
    fn request_stop(&self, at: Instant) {
        *self.stop_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(at);
        self.stop.store(true, Ordering::Release);
    }

    fn join(&mut self) -> Result<(), String> {
        match self.thread.take() {
            Some(thread) => join_narration(thread),
            None => Ok(()),
        }
    }

    pub(crate) fn cancel(&mut self) {
        *self.stop_at.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            if let Err(error) = std::thread::Builder::new()
                .name("narration-cleanup".into())
                .spawn(move || {
                    if let Err(error) = join_narration(thread) {
                        eprintln!("Narration ended with an error: {error}");
                    }
                })
            {
                eprintln!("Could not wait for narration cleanup: {error}");
            }
        }
    }
}

impl Drop for Narration {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = panic.downcast_ref::<&str>() {
        (*message).into()
    } else {
        "unknown panic".into()
    }
}

fn join_narration(thread: std::thread::JoinHandle<Result<(), String>>) -> Result<(), String> {
    thread
        .join()
        .map_err(|panic| format!("narration worker failed: {}", panic_message(panic)))?
}

#[derive(Default)]
struct NarrationSound {
    samples: Vec<i16>,
    #[cfg(target_os = "macos")]
    covered: Vec<(usize, usize)>,
}

#[derive(Default)]
struct FinishedNarration {
    samples: Vec<i16>,
    missing_audio: bool,
}

impl NarrationSound {
    fn insert(
        &mut self,
        started: Instant,
        spec: AudioSpec,
        block: AudioBlock,
    ) -> Result<(), String> {
        #[cfg(windows)]
        {
            let _ = (started, spec);
            self.samples.extend_from_slice(&block.samples);
            Ok(())
        }
        #[cfg(target_os = "macos")]
        {
            let channels = spec.channels as usize;
            if spec.sample_rate == 0 || channels == 0 || block.samples.len() % channels != 0 {
                return Err("the microphone supplied an invalid audio block".into());
            }
            let captured_at = block
                .captured_at
                .ok_or("the microphone supplied audio without a capture timestamp")?;
            if captured_at > Instant::now() + Duration::from_secs(30) {
                return Err("the microphone timestamp is too far ahead of narration".into());
            }
            let count = block.samples.len() / channels;
            let skip = if captured_at < started {
                narration_frames(started - captured_at, spec.sample_rate, true)?.min(count)
            } else {
                0
            };
            if skip == count {
                return Ok(());
            }
            let offset_nanos =
                u64::try_from(skip as u128 * 1_000_000_000 / u128::from(spec.sample_rate))
                    .map_err(|_| "the microphone audio block is too long")?;
            let first_time = captured_at
                .checked_add(Duration::from_nanos(offset_nanos))
                .ok_or("the microphone timestamp is out of range")?;
            let first = narration_frames(
                first_time.saturating_duration_since(started),
                spec.sample_rate,
                false,
            )?;
            let end = first
                .checked_add(count - skip)
                .ok_or("the narration is too long")?;
            let end_samples = end
                .checked_mul(channels)
                .ok_or("the narration is too long")?;
            if end_samples > self.samples.len() {
                self.samples
                    .try_reserve(end_samples - self.samples.len())
                    .map_err(|_| "not enough memory for narration audio")?;
                self.samples.resize(end_samples, 0);
            }
            self.samples[first * channels..end_samples]
                .copy_from_slice(&block.samples[skip * channels..]);
            let begin = self.covered.partition_point(|&(_, end)| end < first);
            let (mut merged_start, mut merged_end) = (first, end);
            let mut last = begin;
            while last < self.covered.len() && self.covered[last].0 <= merged_end {
                merged_start = merged_start.min(self.covered[last].0);
                merged_end = merged_end.max(self.covered[last].1);
                last += 1;
            }
            self.covered
                .splice(begin..last, [(merged_start, merged_end)]);
            Ok(())
        }
    }

    fn finish(
        self,
        started: Instant,
        ended: Instant,
        spec: AudioSpec,
    ) -> Result<FinishedNarration, String> {
        #[cfg(windows)]
        {
            let _ = (started, ended, spec);
            Ok(FinishedNarration {
                samples: self.samples,
                missing_audio: false,
            })
        }
        #[cfg(target_os = "macos")]
        {
            let elapsed = ended
                .checked_duration_since(started)
                .ok_or("the narration ended before it started")?;
            let frames = narration_frames(elapsed, spec.sample_rate, false)?;
            if self
                .covered
                .first()
                .is_none_or(|&(first, _)| first >= frames)
            {
                return Ok(FinishedNarration::default());
            }
            let mut missing = 0usize;
            let mut at = 0usize;
            for &(start, end) in &self.covered {
                if start >= frames {
                    break;
                }
                missing += start.saturating_sub(at);
                at = end.min(frames).max(at);
            }
            missing += frames - at;
            let count = frames
                .checked_mul(spec.channels as usize)
                .ok_or("the narration is too long")?;
            let mut samples = self.samples;
            if count > samples.len() {
                samples
                    .try_reserve(count - samples.len())
                    .map_err(|_| "not enough memory for narration audio")?;
            }
            samples.resize(count, 0);
            Ok(FinishedNarration {
                samples,
                missing_audio: missing >= (spec.sample_rate as usize).div_ceil(20),
            })
        }
    }
}

#[cfg(target_os = "macos")]
fn narration_frames(duration: Duration, rate: u32, round_up: bool) -> Result<usize, String> {
    if rate == 0 {
        return Err("the microphone supplied an invalid sample rate".into());
    }
    let adjustment = if round_up { 999_999_999 } else { 500_000_000 };
    usize::try_from((duration.as_nanos() * u128::from(rate) + adjustment) / 1_000_000_000)
        .map_err(|_| "the narration is too long".into())
}

trait NarrationSource: Send {
    fn spec(&self) -> AudioSpec;
    fn next_block(&self) -> Result<Option<AudioBlock>, String>;
}

impl NarrationSource for AudioCapture {
    fn spec(&self) -> AudioSpec {
        self.spec
    }

    fn next_block(&self) -> Result<Option<AudioBlock>, String> {
        match self.samples.try_recv() {
            Ok(block) => Ok(Some(block)),
            Err(error) => {
                if let Some(error) = self.failure() {
                    return Err(error);
                }
                match error {
                    TryRecvError::Empty => Ok(None),
                    TryRecvError::Disconnected => Err("the microphone sample stream closed".into()),
                }
            }
        }
    }
}

fn narration_worker<S: NarrationSource + 'static>(
    stop: Arc<AtomicBool>,
    stop_at: Arc<Mutex<Option<Instant>>>,
    sound: Arc<Mutex<NarrationSound>>,
    open: impl FnOnce(Arc<AtomicBool>) -> Result<S, String> + Send + 'static,
    ready: impl FnOnce(AudioSpec, Sender<()>) + Send + 'static,
    began: impl FnOnce(Instant, Sender<()>) + Send + 'static,
    finished: impl FnOnce(Result<(), String>) + Send + 'static,
) -> std::thread::JoinHandle<Result<(), String>> {
    std::thread::spawn(move || {
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let capture = match open(stop.clone()) {
                Ok(capture) => capture,
                Err(_) if stop.load(Ordering::Acquire) => return Ok(()),
                Err(error) => return Err(error),
            };
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            let (ack, start) = mpsc::channel();
            ready(capture.spec(), ack);
            if !wait_for_narration_ack(&start, &stop) {
                return Ok(());
            }
            // Samples produced while the first slide was loading precede its timeline.
            loop {
                if stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                match capture.next_block()? {
                    Some(_) => {}
                    None => break,
                }
            }
            let started = Instant::now();
            let (ack, start) = mpsc::channel();
            began(started, ack);
            if !wait_for_narration_ack(&start, &stop) {
                return Ok(());
            }
            let save = |block| {
                sound.lock().unwrap_or_else(|e| e.into_inner()).insert(
                    started,
                    capture.spec(),
                    block,
                )
            };
            while !stop.load(Ordering::Acquire) {
                match capture.next_block()? {
                    Some(block) => save(block)?,
                    None => std::thread::sleep(Duration::from_millis(20)),
                }
            }
            #[cfg(target_os = "macos")]
            let stopping_at = *stop_at.lock().unwrap_or_else(|e| e.into_inner());
            #[cfg(target_os = "macos")]
            if let Some(at) = stopping_at {
                let until = at + Duration::from_millis(500);
                while Instant::now() < until
                    && stop_at.lock().unwrap_or_else(|e| e.into_inner()).is_some()
                {
                    match capture.next_block()? {
                        Some(block) => save(block)?,
                        None => std::thread::sleep(
                            until
                                .saturating_duration_since(Instant::now())
                                .min(Duration::from_millis(20)),
                        ),
                    }
                }
            }
            if stop_at.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
                return Ok(());
            }
            for _ in 0..1024 {
                match capture.next_block()? {
                    Some(block) => save(block)?,
                    None => return Ok(()),
                }
            }
            Err("the microphone sample queue did not drain".into())
        }));
        let result = run.unwrap_or_else(|panic| {
            Err(format!("Narration worker failed: {}", panic_message(panic)))
        });
        let finishing = stop_at.lock().unwrap_or_else(|e| e.into_inner()).is_some();
        if !stop.load(Ordering::Acquire) || finishing {
            finished(result.clone());
        }
        result
    })
}

fn wait_for_narration_ack(receiver: &Receiver<()>, stop: &AtomicBool) -> bool {
    loop {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(()) => return !stop.load(Ordering::Acquire),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

static NEXT_NARRATION: AtomicU64 = AtomicU64::new(1);

fn show_slide(app: &mut App) -> Result<(), String> {
    let Some(n) = &app.narration else {
        return Err("narration is no longer active".into());
    };
    let path = n.images[n.index].clone();
    let pm = jiffshot_doc::io::load_image(&path)
        .map_err(|error| format!("Could not open {}: {error}", path.display()))?;
    app.load_narration_slide(Document::new(pm));
    show_slide_status(app);
    app.publish_narration();
    Ok(())
}

fn show_slide_status(app: &mut App) {
    let Some(n) = &app.narration else { return };
    if n.started.is_none() {
        app.set_status("Starting microphone. Finish cancels narration.");
        return;
    }
    let (i, count) = (n.index, n.images.len());
    let last = i + 1 == count;
    app.set_status(format!(
        "● Narrating picture {} of {count}: talk, then {}",
        i + 1,
        if last { "Finish" } else { "Next picture" }
    ));
}

/// Starts a narrated video: pick pictures, then talk over each.
fn narrate(app: &mut App) {
    app.cancel_narration();
    let id = NEXT_NARRATION.fetch_add(1, Ordering::Relaxed);
    app.narration_request = Some(id);
    let picked = app.pick_files_with_completion(
        "Pictures to narrate",
        true,
        "Images",
        &["png", "jpg", "jpeg", "bmp", "gif", "webp"],
        move |app, outcome| {
            narration_picked(app, id, outcome, |cancel| {
                jiffshot_capture::media::capture_audio_cancellable(true, cancel)
            });
        },
    );
    if !picked && app.narration_request == Some(id) {
        app.narration_request = None;
    }
}

fn narration_picked<S: NarrationSource + 'static>(
    app: &mut App,
    id: u64,
    outcome: Result<Option<Vec<PathBuf>>, String>,
    open: impl FnOnce(Arc<AtomicBool>) -> Result<S, String> + Send + 'static,
) {
    if app.narration_request != Some(id) {
        return;
    }
    let images = match outcome {
        Ok(Some(images)) => images,
        Ok(None) => {
            app.narration_request = None;
            return;
        }
        Err(error) => {
            app.narration_request = None;
            app.set_status(format!("File picker failed: {error}"));
            return;
        }
    };
    if images.is_empty() {
        app.narration_request = None;
        return;
    }
    app.stop_video();
    let sound: Arc<Mutex<NarrationSound>> = Arc::default();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_at = Arc::new(Mutex::new(None));
    let thread = narration_worker(
        stop.clone(),
        stop_at.clone(),
        sound.clone(),
        open,
        move |spec, ack| {
            app::post(move |app| {
                let Some(n) = app
                    .narration
                    .as_mut()
                    .filter(|n| n.id == id && !n.stop.load(Ordering::Acquire))
                else {
                    return;
                };
                n.spec = Some(spec);
                if let Err(error) = show_slide(app) {
                    request_narration_finish(app, Some(error));
                    return;
                }
                if ack.send(()).is_err() {
                    request_narration_finish(
                        app,
                        Some("the microphone worker stopped before narration began".into()),
                    );
                }
            });
        },
        move |started, ack| {
            app::post(move |app| {
                let Some(n) = app
                    .narration
                    .as_mut()
                    .filter(|n| n.id == id && !n.stop.load(Ordering::Acquire))
                else {
                    return;
                };
                n.started = Some(started);
                show_slide_status(app);
                if ack.send(()).is_err() {
                    request_narration_finish(
                        app,
                        Some("the microphone worker stopped as narration began".into()),
                    );
                }
            });
        },
        move |result| {
            app::post(move |app| {
                let Some(n) = app.narration.as_ref().filter(|n| n.id == id) else {
                    return;
                };
                if n.finished_at.is_none() {
                    let error = result
                        .as_ref()
                        .err()
                        .cloned()
                        .unwrap_or_else(|| "the microphone worker stopped unexpectedly".into());
                    request_narration_finish(app, Some(error));
                }
                complete_narration(app, result.err());
            });
        },
    );
    app.narration = Some(Narration {
        id,
        images,
        index: 0,
        started: None,
        marks: Vec::new(),
        spec: None,
        sound,
        stop,
        stop_at,
        finished_at: None,
        finish_error: None,
        thread: Some(thread),
    });
    app.publish_narration();
    app.set_status("Waiting for microphone access. Finish cancels narration.");
}

fn narrate_next(app: &mut App) {
    let Some(n) = app.narration.as_mut() else {
        return;
    };
    if n.finished_at.is_some() {
        app.set_status("Finishing narration audio…");
        return;
    }
    let Some(started) = n.started else {
        app.set_status("Waiting for microphone access. Finish cancels narration.");
        return;
    };
    n.marks
        .push(started.elapsed().as_secs_f64() - n.marks.iter().sum::<f64>());
    if n.index + 1 < n.images.len() {
        n.index += 1;
        if let Err(error) = show_slide(app) {
            if let Some(n) = app.narration.as_mut() {
                n.index -= 1;
            }
            request_narration_finish(app, Some(error));
        }
    } else {
        narrate_finish(app);
    }
}

/// Stops recording and saves the narrated video.
fn narrate_finish(app: &mut App) {
    request_narration_finish(app, None);
}

fn request_narration_finish(app: &mut App, failure: Option<String>) {
    let Some(n) = app.narration.as_mut() else {
        return;
    };
    let Some(started) = n.started else {
        let mut n = app.narration.take().unwrap();
        n.cancel();
        app.publish_narration();
        app.set_status(failure.map_or_else(
            || "Narration cancelled before the microphone started.".into(),
            |error| format!("Narration could not start: {error}"),
        ));
        return;
    };
    if n.finished_at.is_some() {
        return;
    }
    let ended = Instant::now();
    if n.marks.len() <= n.index {
        n.marks
            .push((ended - started).as_secs_f64() - n.marks.iter().sum::<f64>());
    }
    n.finished_at = Some(ended);
    n.finish_error = failure;
    n.request_stop(ended);
    app.set_status("Finishing narration audio…");
}

fn complete_narration(app: &mut App, failure: Option<String>) {
    let Some(mut n) = app.narration.take() else {
        return;
    };
    let Some(started) = n.started else { return };
    let Some(ended) = n.finished_at else { return };
    let stop_error = n.join().err();
    let failure = n.finish_error.take().or(failure).or(stop_error);
    let sound = std::mem::take(&mut *n.sound.lock().unwrap_or_else(|e| e.into_inner()));
    app.publish_narration();
    let Some(spec) = n.spec else {
        app.set_status("The narration's audio format is unavailable; narration was not saved.");
        return;
    };
    let output = match sound.finish(started, ended, spec) {
        Ok(output) => output,
        Err(error) => {
            app.set_status(format!("Narration audio could not be finalized: {error}"));
            return;
        }
    };
    if output.samples.is_empty() {
        app.set_status(failure.map_or_else(
            || "No microphone samples were recorded; narration was not saved.".into(),
            |error| format!("Narration stopped: {error}. No narration audio was recorded."),
        ));
        return;
    }
    // Pictures not reached are left out.
    let images: Vec<PathBuf> = std::mem::take(&mut n.images)
        .into_iter()
        .take(n.marks.len())
        .collect();
    let marks = std::mem::take(&mut n.marks);
    let warning = output
        .missing_audio
        .then_some("Some microphone audio was missing; silence was inserted.");
    if failure.is_some() || warning.is_some() {
        let notes = failure
            .into_iter()
            .chain(warning.map(str::to_owned))
            .collect::<Vec<_>>()
            .join(" ");
        app.set_status(format!(
            "Narration stopped: {notes} Save the narration recorded so far."
        ));
    }
    let pcm = output.samples;
    let name = format!("{} (narrated).mp4", stem_of(&images[0]));
    #[cfg(target_os = "macos")]
    let pending = PendingWork::new(&PENDING_NARRATION_SAVES);
    app.pick_save(
        "Save the narrated video",
        &name,
        &[("MP4 video", "mp4")],
        move |app, out| {
            #[cfg(target_os = "macos")]
            let _pending = pending;
            if app.narration.is_none() && app.narration_request.is_none() {
                app.set_status(format!(
                    "Making a narrated video of {} pictures…",
                    images.len()
                ));
            }
            in_background(out, true, move |out, progress| {
                slideshow(&images, out, &marks, Some((spec, pcm)), progress)
            });
        },
    );
}

/// Moves the playhead by `frames` frames.
fn step(app: &mut App, frames: f64) {
    let Some(v) = app.video.as_ref() else { return };
    let at = v.position + frames / v.reader.fps.max(1.0);
    seek(app, at);
}

/// Applies the saved volume to the open video.
pub fn apply_volume(app: &mut App) {
    let prefs = &app.settings.video;
    let level = if prefs.muted {
        0.0
    } else {
        prefs.volume.clamp(0.0, 1.0)
    };
    if let Some(v) = &app.video {
        v.volume.store((level * 65535.0) as u32, Ordering::Release);
    }
}

pub fn action(app: &mut App, what: &str) {
    match what {
        "record" => match crate::winutil::start_recording() {
            Ok(()) => app.set_status("Select the area to record…"),
            Err(error) => app.set_status(error),
        },
        "play" => play(app),
        "start" | "end" => {
            if let Some(v) = app.video.as_mut() {
                let p = v.position;
                if what == "start" {
                    v.trim = (p.min(v.trim.1 - 0.1).max(0.0), v.trim.1);
                } else {
                    v.trim = (v.trim.0, p.max(v.trim.0 + 0.1));
                }
                let (a, b) = v.trim;
                app.set_status(format!("Keeping {a:.1} s to {b:.1} s"));
                publish(app);
            }
        }
        "export-mp4" => run_export(app, false),
        "export-gif" => run_export(app, true),
        "cut" => cut_selection(app),
        "silent" => silent_copy(app),
        "combine" => combine(app),
        "slideshow" => from_images(app),
        "narrate" => narrate(app),
        "narrate-next" => narrate_next(app),
        "narrate-finish" => narrate_finish(app),
        "prev" => step(app, -1.0),
        "next" => step(app, 1.0),
        "mute" => {
            app.settings.video.muted = !app.settings.video.muted;
            app.settings.save();
            apply_volume(app);
            let state = if app.settings.video.muted {
                "Sound off"
            } else {
                "Sound on"
            };
            app.set_status(state);
        }
        "frame" => {
            let frame = app.video.as_mut().and_then(|v| {
                v.stop_playing();
                v.current.clone().or_else(|| {
                    let p = v.position;
                    v.frame_at(p)
                })
            });
            if let Some(pm) = frame {
                app.stop_video();
                app.load(Document::new(pm), None, None);
                app.set_status("The video frame is now an image; annotate and save it as usual.");
            }
        }
        _ => {}
    }
}

/// A library thumbnail: the frame half a second in.
pub fn thumbnail(path: &Path) -> Option<Pixmap> {
    let mut reader = Reader::open(path).ok()?;
    let at = (reader.duration / 2).min(HNS_PER_SECOND / 2);
    reader
        .frame_at(at)
        .ok()
        .flatten()
        .as_ref()
        .and_then(to_pixmap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn pending_work_clears_on_drop_and_panic() {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let first = PendingWork::new(&COUNTER);
        let second = PendingWork::new(&COUNTER);
        assert_eq!(COUNTER.load(Ordering::Acquire), 2);
        drop(first);
        assert_eq!(COUNTER.load(Ordering::Acquire), 1);
        drop(second);
        let _ = std::panic::catch_unwind(|| {
            let _pending = PendingWork::new(&COUNTER);
            panic!("test worker failure");
        });
        assert_eq!(COUNTER.load(Ordering::Acquire), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn playback_shutdown_signals_without_waiting_for_the_worker() {
        let stop = Arc::new(AtomicBool::new(false));
        let (release, blocked) = mpsc::channel();
        let worker_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            blocked.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(worker_stop.load(Ordering::Acquire));
        });
        let shutdown = Player {
            stop: stop.clone(),
            thread,
            sound: None,
        }
        .request_shutdown();
        assert!(stop.load(Ordering::Acquire));
        let joining = std::thread::spawn(move || shutdown.join());
        release.send(()).unwrap();
        joining.join().unwrap().unwrap();
    }

    struct FakeNarrationSource {
        samples: Receiver<Result<AudioBlock, String>>,
        consumed: Sender<Vec<i16>>,
        emptied: Mutex<Option<Sender<()>>>,
        dropped: Sender<()>,
    }

    impl NarrationSource for FakeNarrationSource {
        fn spec(&self) -> AudioSpec {
            AudioSpec {
                sample_rate: 48_000,
                channels: 2,
            }
        }

        fn next_block(&self) -> Result<Option<AudioBlock>, String> {
            match self.samples.try_recv() {
                Ok(Ok(block)) => {
                    let _ = self.consumed.send(block.samples.clone());
                    Ok(Some(block))
                }
                Ok(Err(error)) => Err(error),
                Err(TryRecvError::Empty) => {
                    if let Some(emptied) = self.emptied.lock().unwrap().take() {
                        let _ = emptied.send(());
                    }
                    Ok(None)
                }
                Err(TryRecvError::Disconnected) => Err("fixture stream closed".into()),
            }
        }
    }

    impl Drop for FakeNarrationSource {
        fn drop(&mut self) {
            let _ = self.dropped.send(());
        }
    }

    struct NarrationFixture {
        source: FakeNarrationSource,
        samples: Sender<Result<AudioBlock, String>>,
        consumed: Receiver<Vec<i16>>,
        emptied: Receiver<()>,
        dropped: Receiver<()>,
    }

    fn narration_fixture() -> NarrationFixture {
        let (samples, rx) = mpsc::channel();
        let (consumed, consumed_rx) = mpsc::channel();
        let (emptied, emptied_rx) = mpsc::channel();
        let (dropped, dropped_rx) = mpsc::channel();
        NarrationFixture {
            source: FakeNarrationSource {
                samples: rx,
                consumed,
                emptied: Mutex::new(Some(emptied)),
                dropped,
            },
            samples,
            consumed: consumed_rx,
            emptied: emptied_rx,
            dropped: dropped_rx,
        }
    }

    #[test]
    fn narration_discards_only_pre_start_samples_and_reports_failure() {
        let NarrationFixture {
            source,
            samples,
            consumed,
            emptied,
            dropped,
        } = narration_fixture();
        samples
            .send(Ok(AudioBlock {
                samples: vec![1, 1],
                captured_at: Some(Instant::now()),
            }))
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let sound = Arc::new(Mutex::new(NarrationSound::default()));
        let (ready, ready_rx) = mpsc::channel();
        let (began, began_rx) = mpsc::channel();
        let (failed, failed_rx) = mpsc::channel();
        let thread = narration_worker(
            stop,
            Arc::new(Mutex::new(None)),
            sound.clone(),
            move |_| Ok(source),
            move |spec, ack| ready.send((spec, ack)).unwrap(),
            move |at, ack| began.send((at, ack)).unwrap(),
            move |result| failed.send(result).unwrap(),
        );
        let (spec, ack) = ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            spec,
            AudioSpec {
                sample_rate: 48_000,
                channels: 2
            }
        );
        ack.send(()).unwrap();
        emptied.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            consumed.recv_timeout(Duration::from_secs(2)).unwrap(),
            vec![1, 1]
        );
        let (started, ack) = began_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        samples
            .send(Ok(AudioBlock {
                samples: vec![42, -42],
                captured_at: Some(started),
            }))
            .unwrap();
        ack.send(()).unwrap();
        assert_eq!(
            consumed.recv_timeout(Duration::from_secs(2)).unwrap(),
            vec![42, -42]
        );
        samples.send(Err("device disconnected".into())).unwrap();
        assert_eq!(
            failed_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Err("device disconnected".into())
        );
        assert_eq!(join_narration(thread), Err("device disconnected".into()));
        dropped.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(sound.lock().unwrap().samples, vec![42, -42]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn narration_places_out_of_order_audio_and_trims_to_its_wall_clock() {
        let started = Instant::now();
        let spec = AudioSpec {
            sample_rate: 1_000,
            channels: 2,
        };
        let stereo = |samples: &[i16]| {
            samples
                .iter()
                .flat_map(|&sample| [sample, sample])
                .collect()
        };
        let mut sound = NarrationSound::default();
        sound
            .insert(
                started,
                spec,
                AudioBlock {
                    captured_at: Some(started - Duration::from_millis(2)),
                    samples: stereo(&[1, 2, 3, 4]),
                },
            )
            .unwrap();
        sound
            .insert(
                started,
                spec,
                AudioBlock {
                    captured_at: Some(started + Duration::from_millis(7)),
                    samples: stereo(&[8, 9, 10, 11, 12]),
                },
            )
            .unwrap();
        sound
            .insert(
                started,
                spec,
                AudioBlock {
                    captured_at: Some(started + Duration::from_millis(4)),
                    samples: stereo(&[5, 6]),
                },
            )
            .unwrap();
        let output = sound
            .finish(started, started + Duration::from_millis(10), spec)
            .unwrap();
        assert_eq!(
            output
                .samples
                .chunks_exact(2)
                .map(|frame| frame[0])
                .collect::<Vec<_>>(),
            [3, 4, 0, 0, 5, 6, 0, 8, 9, 10]
        );
        assert!(!output.missing_audio);

        let mut after_stop = NarrationSound::default();
        after_stop
            .insert(
                started,
                spec,
                AudioBlock {
                    captured_at: Some(started + Duration::from_millis(20)),
                    samples: stereo(&[1]),
                },
            )
            .unwrap();
        assert!(after_stop
            .finish(started, started + Duration::from_millis(10), spec)
            .unwrap()
            .samples
            .is_empty());

        for (missing, warned) in [(49, false), (50, true)] {
            let mut sound = NarrationSound::default();
            sound
                .insert(
                    started,
                    spec,
                    AudioBlock {
                        captured_at: Some(started + Duration::from_millis(missing)),
                        samples: stereo(&[1; 100]),
                    },
                )
                .unwrap();
            let output = sound
                .finish(
                    started,
                    started + Duration::from_millis(missing + 100),
                    spec,
                )
                .unwrap();
            assert_eq!(output.missing_audio, warned);
        }

        let mut invalid = NarrationSound::default();
        assert!(invalid
            .insert(
                started,
                spec,
                AudioBlock {
                    captured_at: None,
                    samples: stereo(&[1])
                }
            )
            .unwrap_err()
            .contains("timestamp"));
        assert!(invalid
            .insert(
                started,
                spec,
                AudioBlock {
                    captured_at: Some(started),
                    samples: vec![1]
                }
            )
            .unwrap_err()
            .contains("invalid audio"));
        assert!(invalid
            .insert(
                started,
                spec,
                AudioBlock {
                    captured_at: Some(Instant::now() + Duration::from_secs(31)),
                    samples: stereo(&[1]),
                }
            )
            .unwrap_err()
            .contains("too far ahead"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn narration_waits_for_late_pre_stop_audio_and_discards_post_stop_audio() {
        let NarrationFixture {
            source,
            samples,
            consumed,
            ..
        } = narration_fixture();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_at = Arc::new(Mutex::new(None));
        let sound = Arc::new(Mutex::new(NarrationSound::default()));
        let (ready, ready_rx) = mpsc::channel();
        let (began, began_rx) = mpsc::channel();
        let (finished, finished_rx) = mpsc::channel();
        let thread = narration_worker(
            stop.clone(),
            stop_at.clone(),
            sound.clone(),
            move |_| Ok(source),
            move |_, ack| ready.send(ack).unwrap(),
            move |at, ack| began.send((at, ack)).unwrap(),
            move |result| finished.send(result).unwrap(),
        );
        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .send(())
            .unwrap();
        let (started, ack) = began_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        ack.send(()).unwrap();
        samples
            .send(Ok(AudioBlock {
                captured_at: Some(started),
                samples: vec![1, 1],
            }))
            .unwrap();
        consumed.recv_timeout(Duration::from_secs(2)).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let ended = Instant::now();
        *stop_at.lock().unwrap() = Some(ended);
        stop.store(true, Ordering::Release);
        std::thread::sleep(Duration::from_millis(30));
        samples
            .send(Ok(AudioBlock {
                captured_at: Some(ended - Duration::from_millis(10)),
                samples: vec![42, -42],
            }))
            .unwrap();
        samples
            .send(Ok(AudioBlock {
                captured_at: Some(ended + Duration::from_millis(5)),
                samples: vec![88, -88],
            }))
            .unwrap();
        assert_eq!(
            finished_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Ok(())
        );
        assert_eq!(join_narration(thread), Ok(()));
        let sound = std::mem::take(&mut *sound.lock().unwrap());
        let pcm = sound
            .finish(
                started,
                ended,
                AudioSpec {
                    sample_rate: 48_000,
                    channels: 2,
                },
            )
            .unwrap()
            .samples;
        assert!(pcm.chunks_exact(2).any(|frame| frame == [42, -42]));
        assert!(!pcm.chunks_exact(2).any(|frame| frame == [88, -88]));
    }

    #[test]
    fn narration_cancel_releases_a_late_source_without_blocking() {
        let NarrationFixture {
            source,
            samples: _samples,
            dropped,
            ..
        } = narration_fixture();
        let stop = Arc::new(AtomicBool::new(false));
        let sound = Arc::new(Mutex::new(NarrationSound::default()));
        let stop_at = Arc::new(Mutex::new(None));
        let (opened, opened_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (events, events_rx) = mpsc::channel::<&str>();
        let (ready_events, began_events, failed_events) = (events.clone(), events.clone(), events);
        let thread = narration_worker(
            stop.clone(),
            stop_at.clone(),
            sound.clone(),
            move |_| {
                opened.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                Ok(source)
            },
            move |_, _| ready_events.send("ready").unwrap(),
            move |_, _| began_events.send("began").unwrap(),
            move |_| failed_events.send("failed").unwrap(),
        );
        let mut narration = Narration {
            id: 1,
            images: Vec::new(),
            index: 0,
            started: None,
            marks: Vec::new(),
            spec: None,
            sound,
            stop,
            stop_at,
            finished_at: None,
            finish_error: None,
            thread: Some(thread),
        };
        opened_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let at = Instant::now();
        narration.cancel();
        assert!(
            at.elapsed() < Duration::from_secs(1),
            "cancellation waited for the pending source"
        );
        release.send(()).unwrap();
        dropped.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(events_rx.try_recv().is_err());
    }

    #[test]
    fn narration_cancel_while_waiting_for_ui_releases_source() {
        let NarrationFixture {
            source,
            samples: _samples,
            dropped,
            ..
        } = narration_fixture();
        let stop = Arc::new(AtomicBool::new(false));
        let (ready, ready_rx) = mpsc::channel();
        let thread = narration_worker(
            stop.clone(),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(NarrationSound::default())),
            move |_| Ok(source),
            move |_, ack| ready.send(ack).unwrap(),
            move |_, _| panic!("cancelled narration must not start"),
            move |result| panic!("cancelled narration unexpectedly completed: {result:?}"),
        );
        let _ack = ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        stop.store(true, Ordering::Release);
        assert_eq!(join_narration(thread), Ok(()));
        dropped.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn narration_reports_startup_and_worker_failures() {
        for panic_on_open in [false, true] {
            let (failed, failed_rx) = mpsc::channel();
            let thread = narration_worker(
                Arc::new(AtomicBool::new(false)),
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(NarrationSound::default())),
                move |_| -> Result<FakeNarrationSource, String> {
                    if panic_on_open {
                        panic!("synthetic worker panic");
                    }
                    Err("permission denied".into())
                },
                |_, _| panic!("failed narration cannot become ready"),
                |_, _| panic!("failed narration cannot start"),
                move |result| failed.send(result).unwrap(),
            );
            let error = failed_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap_err();
            assert!(
                error.contains(if panic_on_open {
                    "synthetic worker panic"
                } else {
                    "permission denied"
                }),
                "{error}"
            );
            assert_eq!(join_narration(thread), Err(error));
        }
    }

    #[test]
    fn narrated_slide_boundaries_do_not_accumulate_rounding_drift() {
        let counts = slide_frame_counts(&[0.051], 100, 30, true).unwrap();
        let mut end = 0usize;
        for (index, count) in counts.iter().enumerate() {
            end += count;
            assert_eq!(end, ((index + 1) as f64 * 0.051 * 30.0).round() as usize);
        }
        assert_eq!(end, 153);
        assert_eq!(
            slide_frame_counts(&[0.051], 2, 30, false).unwrap(),
            [15, 15]
        );
        assert!(slide_frame_counts(&[f64::NAN], 1, 30, true).is_err());
        assert!(slide_frame_counts(&[1.0], 1, 0, true).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn timestamped_narration_round_trips_with_silence_at_the_right_times() {
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(self.0.join("slide.png"));
                let _ = std::fs::remove_file(self.0.join("narrated.mp4"));
                let _ = std::fs::remove_dir(&self.0);
            }
        }

        let name = format!(
            "jiffshot-narration-timeline-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let scratch = Scratch(std::env::temp_dir().join(name));
        std::fs::create_dir(&scratch.0).unwrap();
        let image = scratch.0.join("slide.png");
        let mut picture = Pixmap::new(32, 24).unwrap();
        picture.fill(tiny_skia::Color::from_rgba8(30, 90, 160, 255));
        picture.save_png(&image).unwrap();

        let spec = AudioSpec {
            sample_rate: 48_000,
            channels: 2,
        };
        let started = Instant::now();
        let tone = (0..14_400)
            .flat_map(|frame| {
                let value = ((frame as f64 * 440.0 * std::f64::consts::TAU
                    / f64::from(spec.sample_rate))
                .sin()
                    * 6_000.0) as i16;
                [value, value]
            })
            .collect();
        let mut sound = NarrationSound::default();
        sound
            .insert(
                started,
                spec,
                AudioBlock {
                    samples: tone,
                    captured_at: Some(started + Duration::from_millis(200)),
                },
            )
            .unwrap();
        let output = sound
            .finish(started, started + Duration::from_secs(1), spec)
            .unwrap();
        assert!(output.missing_audio);
        let pcm = output.samples;
        let path = scratch.0.join("narrated.mp4");
        slideshow(&[image], &path, &[1.0], Some((spec, pcm)), |_| {}).unwrap();

        let mut reader = Reader::open(&path).unwrap();
        reader.select_video(false).unwrap();
        reader.select_audio(true).unwrap();
        reader.seek(0).unwrap();
        let mut decoded = vec![0i16; spec.sample_rate as usize * spec.channels as usize];
        while let Some((time, block)) = reader.next_audio().unwrap() {
            let first = ((i128::from(time) * i128::from(spec.sample_rate)
                + i128::from(HNS_PER_SECOND) / 2)
                / i128::from(HNS_PER_SECOND))
            .max(0) as usize
                * spec.channels as usize;
            let count = block.len().min(decoded.len().saturating_sub(first));
            if count > 0 {
                decoded[first..first + count].copy_from_slice(&block[..count]);
            }
        }
        let rms = |from: usize, to: usize| {
            let samples = &decoded[from * 48 * 2..to * 48 * 2];
            (samples
                .iter()
                .map(|sample| f64::from(*sample).powi(2))
                .sum::<f64>()
                / samples.len() as f64)
                .sqrt()
        };
        assert!(rms(50, 150) < 200.0, "the initial gap must remain silent");
        assert!(
            rms(250, 450) > 2_000.0,
            "the captured interval must contain audio"
        );
        assert!(
            rms(650, 900) < 200.0,
            "the interval after capture must remain silent"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn edited_video_preserves_audio_at_trim_and_cut_boundaries() {
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                for name in ["slide.png", "source.mp4", "trim.mp4", "cut.mp4"] {
                    let _ = std::fs::remove_file(self.0.join(name));
                }
                let _ = std::fs::remove_dir(&self.0);
            }
        }

        let name = format!(
            "jiffshot-video-audio-edits-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let scratch = Scratch(std::env::temp_dir().join(name));
        std::fs::create_dir(&scratch.0).unwrap();
        let image = scratch.0.join("slide.png");
        let mut picture = Pixmap::new(32, 24).unwrap();
        picture.fill(tiny_skia::Color::from_rgba8(30, 90, 160, 255));
        picture.save_png(&image).unwrap();
        let spec = AudioSpec {
            sample_rate: 48_000,
            channels: 2,
        };
        let pcm = (0..spec.sample_rate as usize * 3)
            .flat_map(|frame| {
                let frequency = if frame < spec.sample_rate as usize * 3 / 2 {
                    440.0
                } else {
                    880.0
                };
                let value = ((frame as f64 * frequency * std::f64::consts::TAU
                    / f64::from(spec.sample_rate))
                .sin()
                    * 8_000.0) as i16;
                [value, value]
            })
            .collect();
        let source = scratch.0.join("source.mp4");
        slideshow(&[image], &source, &[3.0], Some((spec, pcm)), |_| {}).unwrap();

        let trim = scratch.0.join("trim.mp4");
        export_mp4(&source, &trim, (0.413, 1.713), |_| {}).expect("export trimmed audio");
        let cut = scratch.0.join("cut.mp4");
        export_segments(
            &[(source.clone(), 0.0, 0.91), (source, 1.73, 2.77)],
            &cut,
            true,
            |_| {},
        )
        .expect("export cut audio");

        let samples = |path: &Path, duration: f64| {
            let mut reader = Reader::open(path).unwrap();
            assert_eq!(reader.audio, Some(spec));
            reader.select_video(false).unwrap();
            reader.select_audio(true).unwrap();
            reader.seek(0).unwrap();
            let mut decoded = vec![
                0i16;
                (duration * f64::from(spec.sample_rate)).ceil() as usize
                    * spec.channels as usize
            ];
            while let Some((time, block)) = reader.next_audio().unwrap() {
                let first = ((i128::from(time) * i128::from(spec.sample_rate)
                    + i128::from(HNS_PER_SECOND) / 2)
                    / i128::from(HNS_PER_SECOND))
                .max(0) as usize
                    * spec.channels as usize;
                let count = block.len().min(decoded.len().saturating_sub(first));
                if count > 0 {
                    decoded[first..first + count].copy_from_slice(&block[..count]);
                }
            }
            decoded
        };
        let rms = |samples: &[i16], start: f64, end: f64| {
            let first = (start * f64::from(spec.sample_rate)) as usize * spec.channels as usize;
            let last = (end * f64::from(spec.sample_rate)) as usize * spec.channels as usize;
            let block = &samples[first..last];
            (block
                .iter()
                .map(|sample| f64::from(*sample).powi(2))
                .sum::<f64>()
                / block.len() as f64)
                .sqrt()
        };
        let tone = |samples: &[i16], start: f64, end: f64, frequency: f64| {
            let first = (start * f64::from(spec.sample_rate)) as usize;
            let last = (end * f64::from(spec.sample_rate)) as usize;
            let (mut sin, mut cos) = (0.0, 0.0);
            for frame in first..last {
                let phase =
                    frame as f64 * frequency * std::f64::consts::TAU / f64::from(spec.sample_rate);
                let sample = f64::from(samples[frame * spec.channels as usize]);
                sin += sample * phase.sin();
                cos += sample * phase.cos();
            }
            sin.hypot(cos)
        };
        let trimmed = samples(&trim, 1.30);
        for (start, end) in [(0.02, 0.03), (1.27, 1.28)] {
            let level = rms(&trimmed, start, end);
            assert!(
                level > 2_000.0,
                "trimmed audio is missing at {start:.3} s: RMS {level:.1}"
            );
        }
        assert!(tone(&trimmed, 0.70, 0.80, 440.0) > 3.0 * tone(&trimmed, 0.70, 0.80, 880.0));
        assert!(tone(&trimmed, 1.15, 1.25, 880.0) > 3.0 * tone(&trimmed, 1.15, 1.25, 440.0));
        let cut_audio = samples(&cut, 1.95);
        for (start, end) in [
            (0.88, 0.89),
            (0.90, 0.905),
            (0.915, 0.92),
            (0.93, 0.94),
            (1.92, 1.93),
        ] {
            let level = rms(&cut_audio, start, end);
            assert!(
                level > 2_000.0,
                "cut audio is missing at {start:.3} s: RMS {level:.1}"
            );
        }
        assert!(tone(&cut_audio, 0.70, 0.80, 440.0) > 3.0 * tone(&cut_audio, 0.70, 0.80, 880.0));
        assert!(tone(&cut_audio, 1.00, 1.10, 880.0) > 3.0 * tone(&cut_audio, 1.00, 1.10, 440.0));
    }

    /// Writes a short clip, then exports a trimmed MP4 and a GIF of it.
    #[test]
    fn trims_and_exports() {
        let dir = std::env::temp_dir().join(format!("jiffshot-video-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("clip.mp4");
        let spec = VideoSpec::for_size(160, 120, 10);
        let mut w = Writer::create(&src, spec, None).unwrap();
        for i in 0..30i64 {
            let shade = (i * 8) as u8;
            let frame: Vec<u8> = (0..160 * 120)
                .flat_map(|_| [shade, 128, 255 - shade, 0])
                .collect();
            w.write_frame(&frame, i * HNS_PER_SECOND / 10, HNS_PER_SECOND / 10)
                .unwrap();
        }
        w.finish().unwrap();
        let mp4 = dir.join("trimmed.mp4");
        export_mp4(&src, &mp4, (1.0, 2.0), |_| {}).unwrap();
        let r = Reader::open(&mp4).unwrap();
        assert!(
            (seconds(r.duration) - 1.0).abs() < 0.25,
            "trimmed to about a second: {}",
            seconds(r.duration)
        );
        let gif = dir.join("clip.gif");
        let options = GifOptions {
            fps: 5.0,
            max_width: 80,
            looped: true,
            fade: true,
            dither: true,
        };
        export_gif(&src, &gif, (0.0, 3.0), options, |_| {}).unwrap();
        let decoder = image::codecs::gif::GifDecoder::new(std::io::BufReader::new(
            std::fs::File::open(&gif).unwrap(),
        ))
        .unwrap();
        let frames = image::AnimationDecoder::into_frames(decoder)
            .collect_frames()
            .unwrap();
        assert!((13..=16).contains(&frames.len()), "{} frames", frames.len());
        assert_eq!(frames[0].buffer().width(), 80);

        // Cut 1..2 s out of the 3 s clip, then join the result with the
        // original: about 2 s, then 5 s.
        let cut = dir.join("cut.mp4");
        export_segments(
            &[(src.clone(), 0.0, 1.0), (src.clone(), 2.0, 3.0)],
            &cut,
            true,
            |_| {},
        )
        .unwrap();
        let mut r = Reader::open(&cut).unwrap();
        assert!(
            (seconds(r.duration) - 2.0).abs() < 0.25,
            "cut: {}",
            seconds(r.duration)
        );
        // Source frame 22 (shade 176, blue channel first) follows the join.
        let after = r.frame_at(hns(1.2)).unwrap().unwrap();
        assert!(
            (after.bgra[0] as i32 - 176).abs() < 24,
            "the frame after the cut is from 2.2 s: {}",
            after.bgra[0]
        );
        let joined = dir.join("joined.mp4");
        export_segments(
            &[(cut.clone(), 0.0, 2.0), (src.clone(), 0.0, 3.0)],
            &joined,
            true,
            |_| {},
        )
        .unwrap();
        let d = seconds(Reader::open(&joined).unwrap().duration);
        assert!((d - 5.0).abs() < 0.3, "joined: {d}");

        // Two pictures of different sizes, a second each.
        let (a, b) = (dir.join("a.png"), dir.join("b.png"));
        let mut pa = Pixmap::new(200, 100).unwrap();
        pa.fill(tiny_skia::Color::from_rgba8(255, 0, 0, 255));
        pa.save_png(&a).unwrap();
        let mut pb = Pixmap::new(100, 100).unwrap();
        pb.fill(tiny_skia::Color::from_rgba8(0, 0, 255, 255));
        pb.save_png(&b).unwrap();
        let show = dir.join("show.mp4");
        slideshow(&[a.clone(), b.clone()], &show, &[1.0], None, |_| {}).unwrap();
        let mut r = Reader::open(&show).unwrap();
        assert_eq!((r.width, r.height), (200, 100));
        assert!(
            (seconds(r.duration) - 2.0).abs() < 0.2,
            "{}",
            seconds(r.duration)
        );
        let first = r.frame_at(hns(0.2)).unwrap().unwrap();
        assert!(
            first.bgra[2] > 200 && first.bgra[0] < 60,
            "red first: {:?}",
            &first.bgra[..4]
        );
        let second = r.frame_at(hns(1.6)).unwrap().unwrap();
        let mid = (50 * 200 + 100) * 4;
        assert!(
            second.bgra[mid] > 200 && second.bgra[mid + 2] < 60,
            "blue second: {:?}",
            &second.bgra[mid..mid + 4]
        );
        assert!(
            second.bgra[(50 * 200 + 10) * 4] < 30,
            "black bars beside the square picture"
        );

        // Narrated: the first picture for 0.5 s, the second for 1.5 s,
        // with a soundtrack.
        let narrated = dir.join("narrated.mp4");
        let spec = jiffshot_capture::media::AudioSpec {
            sample_rate: 48_000,
            channels: 2,
        };
        let tone: Vec<i16> = (0..96_000)
            .flat_map(|n| {
                let v =
                    ((n as f32 / 48_000.0 * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
                [v, v]
            })
            .collect();
        slideshow(
            &[a.clone(), b.clone()],
            &narrated,
            &[0.5, 1.5],
            Some((spec, tone.clone())),
            |_| {},
        )
        .unwrap();
        let mut r = Reader::open(&narrated).unwrap();
        assert!(
            (seconds(r.duration) - 2.0).abs() < 0.2,
            "{}",
            seconds(r.duration)
        );
        assert_eq!(r.audio.map(|a| a.sample_rate), Some(48_000));
        let at_one = r.frame_at(hns(1.0)).unwrap().unwrap();
        assert!(
            at_one.bgra[mid] > 200,
            "the second picture shows from 0.5 s"
        );

        let rapid = dir.join("rapid-narration.mp4");
        slideshow(
            &[a.clone(), b.clone()],
            &rapid,
            &[0.1, 0.2],
            Some((spec, tone[..28_800].to_vec())),
            |_| {},
        )
        .unwrap();
        let mut r = Reader::open(&rapid).unwrap();
        assert!(
            (seconds(r.duration) - 0.3).abs() < 0.08,
            "short slide timings were stretched: {}",
            seconds(r.duration)
        );
        let second = r.frame_at(hns(0.15)).unwrap().unwrap();
        assert!(
            second.bgra[mid] > 200,
            "the second short slide starts near 0.1 s"
        );

        let missing = dir.join("missing.png");
        let error = slideshow(
            &[a, missing.clone()],
            &dir.join("invalid.mp4"),
            &[1.0],
            None,
            |_| {},
        )
        .unwrap_err();
        assert!(error.contains(&missing.display().to_string()), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(target_os = "macos")]
#[path = "video/sound_macos.rs"]
mod sound;

#[cfg(target_os = "macos")]
#[path = "video/narration_selftest.rs"]
pub(crate) mod narration_selftest;

/// Playback sound through the wave-out API.
#[cfg(windows)]
mod sound {
    use super::{hns, seconds};
    use jiffshot_capture::media::Reader;
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::Duration;
    use windows_sys::Win32::Media::Audio::{
        waveOutClose, waveOutOpen, waveOutPrepareHeader, waveOutReset, waveOutSetVolume,
        waveOutUnprepareHeader, waveOutWrite, CALLBACK_NULL, HWAVEOUT, WAVEFORMATEX, WAVEHDR,
        WAVE_MAPPER,
    };

    const WHDR_DONE: u32 = 1;

    /// A queued block: the header and the samples it points at, kept in
    /// place until the device is done with them.
    type Block = (Box<WAVEHDR>, Vec<i16>);

    /// Plays `path`'s sound from `from` to `end` seconds until `stop`.
    pub fn play(path: &Path, from: f64, end: f64, stop: &AtomicBool, volume: &AtomicU32) {
        let Ok(mut reader) = Reader::open(path) else {
            return;
        };
        let Some(spec) = reader.audio else { return };
        if reader.select_video(false).is_err()
            || reader.select_audio(true).is_err()
            || reader.seek(hns(from)).is_err()
        {
            return;
        }

        let channels = spec.channels as u16;
        let format = WAVEFORMATEX {
            wFormatTag: 1,
            nChannels: channels,
            nSamplesPerSec: spec.sample_rate,
            nAvgBytesPerSec: spec.sample_rate * channels as u32 * 2,
            nBlockAlign: channels * 2,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let mut out: HWAVEOUT = std::ptr::null_mut();
        // SAFETY: the format outlives the call; no callback.
        if unsafe { waveOutOpen(&mut out, WAVE_MAPPER, &format, 0, 0, CALLBACK_NULL) } != 0 {
            return;
        }
        let header_size = std::mem::size_of::<WAVEHDR>() as u32;
        let mut queued: VecDeque<Block> = VecDeque::new();
        let release = |q: &mut VecDeque<Block>, all: bool| {
            while let Some((h, _)) = q.front_mut() {
                if !all && h.dwFlags & WHDR_DONE == 0 {
                    break;
                }
                // SAFETY: a header prepared on this device.
                unsafe { waveOutUnprepareHeader(out, &mut **h, header_size) };
                q.pop_front();
            }
        };
        let mut level = u32::MAX;
        while !stop.load(Ordering::Acquire) {
            let v = volume.load(Ordering::Acquire).min(0xFFFF);
            if v != level {
                level = v;
                // SAFETY: an open device.
                unsafe { waveOutSetVolume(out, v | (v << 16)) };
            }
            release(&mut queued, false);
            if queued.len() >= 8 {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            let Ok(Some((at, pcm))) = reader.next_audio() else {
                break;
            };
            if seconds(at) < from - 0.05 {
                continue;
            }
            if seconds(at) > end {
                break;
            }
            // SAFETY: a zeroed header is valid before it is filled in.
            let mut header: Box<WAVEHDR> = Box::new(unsafe { std::mem::zeroed() });
            header.lpData = pcm.as_ptr() as *mut u8;
            header.dwBufferLength = (pcm.len() * 2) as u32;
            // SAFETY: the header and its samples stay in `queued` until
            // they are unprepared.
            let ok = unsafe {
                waveOutPrepareHeader(out, &mut *header, header_size) == 0
                    && waveOutWrite(out, &mut *header, header_size) == 0
            };
            queued.push_back((header, pcm));
            if !ok {
                break;
            }
        }
        // Let what is queued finish playing, unless stopped.
        while !stop.load(Ordering::Acquire)
            && queued.iter().any(|(h, _)| h.dwFlags & WHDR_DONE == 0)
        {
            std::thread::sleep(Duration::from_millis(15));
        }
        // SAFETY: stops the device, frees what it held, then closes it.
        unsafe { waveOutReset(out) };
        release(&mut queued, true);
        unsafe { waveOutClose(out) };
    }
}
