//! GStreamer implementation of the media backend: an MP4 writer (H.264 and
//! AAC), an MP4 reader decoding to BGRA frames and 16-bit PCM, webcam frames
//! and PulseAudio/PipeWire audio capture.

use super::{AudioBlock, AudioSpec, Result, VideoFrame, VideoSpec, HNS_PER_SECOND};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const AAC_ENCODERS: [&str; 3] = ["voaacenc", "avenc_aac", "fdkaacenc"];
const PULL_TIMEOUT: gst::ClockTime = gst::ClockTime::from_seconds(10);

fn e(error: impl std::fmt::Display) -> String {
    error.to_string()
}

pub fn startup() -> Result<()> {
    gst::init().map_err(e)
}

fn hns_to_time(hns: i64) -> gst::ClockTime {
    gst::ClockTime::from_nseconds(hns.max(0) as u64 * 100)
}

fn time_to_hns(time: gst::ClockTime) -> i64 {
    (time.nseconds() / 100) as i64
}

fn aac_encoder() -> Option<&'static str> {
    AAC_ENCODERS
        .into_iter()
        .find(|name| gst::ElementFactory::find(name).is_some())
}

fn app_src(pipeline: &gst::Pipeline, name: &str) -> Result<gst_app::AppSrc> {
    pipeline
        .by_name(name)
        .ok_or_else(|| format!("missing pipeline element {name}"))?
        .downcast::<gst_app::AppSrc>()
        .map_err(|_| format!("{name} is not an appsrc"))
}

fn app_sink(pipeline: &gst::Pipeline, name: &str) -> Result<gst_app::AppSink> {
    pipeline
        .by_name(name)
        .ok_or_else(|| format!("missing pipeline element {name}"))?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| format!("{name} is not an appsink"))
}

fn launch(description: &str) -> Result<gst::Pipeline> {
    gst::parse::launch(description)
        .map_err(e)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "the description is not a pipeline".to_owned())
}

/// Waits for the pipeline to finish changing state, reporting bus errors.
fn settle(pipeline: &gst::Pipeline) -> Result<()> {
    let bus = pipeline.bus().ok_or("the pipeline has no bus")?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let Some(message) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(left.as_millis().min(250) as u64),
            &[gst::MessageType::AsyncDone, gst::MessageType::Error],
        ) else {
            if left.is_zero() {
                return Err("the media pipeline did not start".to_owned());
            }
            continue;
        };
        return match message.view() {
            gst::MessageView::Error(error) => Err(error.error().to_string()),
            _ => Ok(()),
        };
    }
}

/// Writes an MP4 file. Frames are top-down BGRA (the fourth byte unused).
pub struct Writer {
    pipeline: gst::Pipeline,
    video: gst_app::AppSrc,
    audio: Option<gst_app::AppSrc>,
    spec: VideoSpec,
}

impl Writer {
    pub fn create(path: &Path, spec: VideoSpec, audio: Option<AudioSpec>) -> Result<Writer> {
        startup()?;
        let mut description = format!(
            "appsrc name=video format=time caps=video/x-raw,format=BGRA,width={},height={},framerate={}/1 \
             ! videoconvert ! x264enc bitrate={} speed-preset=veryfast bframes=0 key-int-max={} \
             ! video/x-h264,profile=high ! h264parse ! mux. \
             mp4mux name=mux ! filesink name=sink",
            spec.width,
            spec.height,
            spec.fps.max(1),
            (spec.bitrate / 1000).max(1),
            spec.fps.max(1) * 2,
        );
        if let Some(audio) = audio {
            let encoder = aac_encoder().ok_or("no AAC encoder is available")?;
            description.push_str(&format!(
                " appsrc name=audio format=time caps=audio/x-raw,format=S16LE,layout=interleaved,rate={},channels={} \
                 ! audioconvert ! audioresample ! {encoder} ! aacparse ! mux.",
                audio.sample_rate, audio.channels,
            ));
        }
        let pipeline = launch(&description)?;
        pipeline
            .by_name("sink")
            .ok_or("missing file sink")?
            .set_property("location", path.to_string_lossy().as_ref());
        let video = app_src(&pipeline, "video")?;
        let audio_src = if audio.is_some() {
            Some(app_src(&pipeline, "audio")?)
        } else {
            None
        };
        pipeline.set_state(gst::State::Playing).map_err(e)?;
        Ok(Writer {
            pipeline,
            video,
            audio: audio_src,
            spec,
        })
    }

    pub fn spec(&self) -> VideoSpec {
        self.spec
    }

    fn push(source: &gst_app::AppSrc, bytes: &[u8], time: i64, duration: i64) -> Result<()> {
        let mut buffer = gst::Buffer::from_slice(bytes.to_vec());
        {
            let buffer = buffer.get_mut().expect("a new buffer is writable");
            buffer.set_pts(hns_to_time(time));
            buffer.set_duration(hns_to_time(duration));
        }
        source.push_buffer(buffer).map(|_| ()).map_err(e)
    }

    /// Writes a frame of exactly `width * height * 4` bytes.
    pub fn write_frame(&mut self, bgra: &[u8], time: i64, duration: i64) -> Result<()> {
        let len = (self.spec.width * self.spec.height * 4) as usize;
        if bgra.len() < len {
            return Err("frame too small".into());
        }
        Self::push(&self.video, &bgra[..len], time, duration)
    }

    /// Writes interleaved 16-bit samples.
    pub fn write_audio(&mut self, pcm: &[i16], time: i64, duration: i64) -> Result<()> {
        let Some(source) = &self.audio else {
            return Ok(());
        };
        if pcm.is_empty() {
            return Ok(());
        }
        let bytes: Vec<u8> = pcm.iter().flat_map(|sample| sample.to_le_bytes()).collect();
        Self::push(source, &bytes, time, duration)
    }

    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    pub fn finish(self) -> Result<()> {
        let _ = self.video.end_of_stream();
        if let Some(audio) = &self.audio {
            let _ = audio.end_of_stream();
        }
        let bus = self.pipeline.bus().ok_or("the pipeline has no bus")?;
        let result = match bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(60),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) {
            Some(message) => match message.view() {
                gst::MessageView::Error(error) => Err(error.error().to_string()),
                _ => Ok(()),
            },
            None => Err("the video file did not finish in time".to_owned()),
        };
        let _ = self.pipeline.set_state(gst::State::Null);
        result
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn file_uri(path: &Path) -> Result<String> {
    let path = std::fs::canonicalize(path).map_err(e)?;
    gst::glib::filename_to_uri(&path, None)
        .map(|uri| uri.to_string())
        .map_err(e)
}

/// Reads an MP4 (or anything GStreamer can decode).
pub struct Reader {
    video: gst::Pipeline,
    video_sink: gst_app::AppSink,
    audio_pipeline: Option<(gst::Pipeline, gst_app::AppSink)>,
    uri: Option<String>,
    position: i64,
    audio_on: bool,
    video_on: bool,
    pub width: u32,
    pub height: u32,
    pub duration: i64,
    pub fps: f64,
    pub audio: Option<AudioSpec>,
    audio_error: Option<String>,
}

const BGRA_SINK: &str =
    "videoconvert ! video/x-raw,format=BGRA ! appsink name=sink max-buffers=1 sync=false";

impl Reader {
    pub fn open(path: &Path) -> Result<Reader> {
        startup()?;
        let uri = file_uri(path)?;
        let pipeline = launch(&format!("uridecodebin uri={uri:?} ! {BGRA_SINK}"))?;
        let mut reader = Self::start(pipeline, Some(uri.clone()))?;
        let (audio, audio_error) = probe_audio(&uri);
        reader.audio = audio;
        reader.audio_error = audio_error;
        Ok(reader)
    }

    /// The first webcam, decoded to BGRA frames as they arrive (`duration`
    /// is 0; read with `next_video`).
    pub fn webcam() -> Result<Reader> {
        startup()?;
        let pipeline = launch(&format!("v4l2src ! {BGRA_SINK}"))?;
        Self::start(pipeline, None)
    }

    fn start(pipeline: gst::Pipeline, uri: Option<String>) -> Result<Reader> {
        let video_sink = app_sink(&pipeline, "sink")?;
        pipeline.set_state(gst::State::Paused).map_err(e)?;
        settle(&pipeline)?;
        let caps = video_sink
            .static_pad("sink")
            .and_then(|pad| pad.current_caps())
            .ok_or("the video has no frames")?;
        let info = gst_video::VideoInfo::from_caps(&caps).map_err(e)?;
        let duration = pipeline
            .query_duration::<gst::ClockTime>()
            .map(time_to_hns)
            .unwrap_or(0);
        let fps = info.fps();
        let fps = if fps.denom() > 0 && fps.numer() > 0 {
            fps.numer() as f64 / fps.denom() as f64
        } else {
            30.0
        };
        pipeline.set_state(gst::State::Playing).map_err(e)?;
        Ok(Reader {
            video: pipeline,
            video_sink,
            audio_pipeline: None,
            uri,
            position: 0,
            audio_on: false,
            video_on: true,
            width: info.width(),
            height: info.height(),
            duration,
            fps,
            audio: None,
            audio_error: None,
        })
    }

    /// Why a source audio stream could not be decoded; absent for silent sources.
    pub fn audio_error(&self) -> Option<&str> {
        self.audio_error.as_deref()
    }

    /// Whether audio samples are read too (off by default, for frame decoding).
    pub fn select_audio(&mut self, on: bool) -> Result<()> {
        if self.audio.is_none() {
            return Ok(());
        }
        self.audio_on = on;
        if on && self.audio_pipeline.is_none() {
            let uri = self.uri.clone().ok_or("this source has no audio")?;
            let pipeline = launch(&format!(
                "uridecodebin uri={uri:?} ! audioconvert ! audioresample \
                 ! audio/x-raw,format=S16LE,channels=2,layout=interleaved \
                 ! appsink name=sink max-buffers=4 sync=false"
            ))?;
            let sink = app_sink(&pipeline, "sink")?;
            pipeline.set_state(gst::State::Paused).map_err(e)?;
            settle(&pipeline)?;
            pipeline.set_state(gst::State::Playing).map_err(e)?;
            self.audio_pipeline = Some((pipeline, sink));
            if self.position > 0 {
                self.seek(self.position)?;
            }
        }
        Ok(())
    }

    /// Whether video frames are decoded (turn off to read only audio).
    pub fn select_video(&mut self, on: bool) -> Result<()> {
        self.video_on = on;
        Ok(())
    }

    pub fn seek(&mut self, time: i64) -> Result<()> {
        self.position = time.max(0);
        let target = hns_to_time(self.position);
        let flags = gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE;
        self.video.seek_simple(flags, target).map_err(e)?;
        if let Some((pipeline, _)) = &self.audio_pipeline {
            pipeline.seek_simple(flags, target).map_err(e)?;
        }
        Ok(())
    }

    /// The next frame in decode order.
    pub fn next_video(&mut self) -> Result<Option<VideoFrame>> {
        if !self.video_on {
            return Ok(None);
        }
        let Some(sample) = self.video_sink.try_pull_sample(PULL_TIMEOUT) else {
            return Ok(None);
        };
        let buffer = sample.buffer().ok_or("a frame had no data")?;
        let caps = sample.caps().ok_or("a frame had no format")?;
        let info = gst_video::VideoInfo::from_caps(caps).map_err(e)?;
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).map_err(e)?;
        let (width, height) = (info.width() as usize, info.height() as usize);
        let stride = frame.plane_stride()[0] as usize;
        let data = frame.plane_data(0).map_err(e)?;
        let mut bgra = vec![0u8; width * height * 4];
        for y in 0..height {
            let row = &data[y * stride..y * stride + width * 4];
            bgra[y * width * 4..(y + 1) * width * 4].copy_from_slice(row);
        }
        Ok(Some(VideoFrame {
            time: buffer.pts().map(time_to_hns).unwrap_or(0),
            width: info.width(),
            height: info.height(),
            bgra,
        }))
    }

    /// The next block of interleaved 16-bit audio.
    pub fn next_audio(&mut self) -> Result<Option<(i64, Vec<i16>)>> {
        if !self.audio_on {
            return Ok(None);
        }
        let Some((_, sink)) = &self.audio_pipeline else {
            return Ok(None);
        };
        let Some(sample) = sink.try_pull_sample(PULL_TIMEOUT) else {
            return Ok(None);
        };
        let buffer = sample.buffer().ok_or("audio had no data")?;
        let map = buffer.map_readable().map_err(e)?;
        let samples = map
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        Ok(Some((buffer.pts().map(time_to_hns).unwrap_or(0), samples)))
    }

    /// The frame showing at `time`.
    pub fn frame_at(&mut self, time: i64) -> Result<Option<VideoFrame>> {
        self.seek(time)?;
        let half = (HNS_PER_SECOND as f64 / self.fps.max(1.0) / 2.0) as i64;
        let mut last = None;
        while let Some(frame) = self.next_video()? {
            let done = frame.time + half >= time;
            last = Some(frame);
            if done {
                break;
            }
        }
        Ok(last)
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        let _ = self.video.set_state(gst::State::Null);
        if let Some((pipeline, _)) = &self.audio_pipeline {
            let _ = pipeline.set_state(gst::State::Null);
        }
    }
}

/// The audio format of the file's first audio stream, if it has one.
fn probe_audio(uri: &str) -> (Option<AudioSpec>, Option<String>) {
    let probe = || -> Result<Option<AudioSpec>> {
        let discoverer =
            gstreamer_pbutils::Discoverer::new(gst::ClockTime::from_seconds(10)).map_err(e)?;
        let info = discoverer.discover_uri(uri).map_err(e)?;
        Ok(info.audio_streams().first().map(|stream| AudioSpec {
            sample_rate: stream.sample_rate(),
            channels: 2,
        }))
    };
    match probe() {
        Ok(audio) => (audio, None),
        Err(error) => (None, Some(error)),
    }
}

/// Audio being captured: interleaved 16-bit stereo blocks arrive on
/// `samples`; dropping the handle stops capture.
pub struct AudioCapture {
    pub spec: AudioSpec,
    pub samples: Receiver<AudioBlock>,
    pipeline: gst::Pipeline,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    failure: Arc<Mutex<Option<String>>>,
}

impl AudioCapture {
    pub fn failure(&self) -> Option<String> {
        self.failure
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Starts capturing what the speakers play (`microphone` false) or the
/// default microphone, converted to 16-bit stereo.
pub fn capture_audio(microphone: bool) -> Result<AudioCapture> {
    capture_audio_cancellable(microphone, Arc::new(AtomicBool::new(false)))
}

/// Starts an audio source unless `cancel` is set. Dropping the returned
/// capture also stops it.
pub fn capture_audio_cancellable(
    microphone: bool,
    cancel: Arc<AtomicBool>,
) -> Result<AudioCapture> {
    const RATE: u32 = 48_000;
    if cancel.load(Ordering::Acquire) {
        return Err("audio capture was cancelled".into());
    }
    startup()?;
    let device = if microphone {
        ""
    } else {
        "device=@DEFAULT_MONITOR@"
    };
    let pipeline = launch(&format!(
        "pulsesrc {device} do-timestamp=true ! audioconvert ! audioresample \
         ! audio/x-raw,format=S16LE,rate={RATE},channels=2,layout=interleaved \
         ! appsink name=sink max-buffers=64 sync=false"
    ))?;
    let sink = app_sink(&pipeline, "sink")?;
    pipeline.set_state(gst::State::Playing).map_err(e)?;
    let started = settle(&pipeline);
    if cancel.load(Ordering::Acquire) {
        let _ = pipeline.set_state(gst::State::Null);
        return Err("audio capture was cancelled".into());
    }
    if let Err(error) = started {
        let _ = pipeline.set_state(gst::State::Null);
        return Err(error);
    }
    let (sender, samples) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let failure = Arc::new(Mutex::new(None));
    let (flag, worker_failure) = (stop.clone(), failure.clone());
    let thread = std::thread::Builder::new()
        .name("audio-capture".into())
        .spawn(move || {
            while !flag.load(Ordering::Acquire) {
                match sink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
                    Some(sample) => {
                        let Some(buffer) = sample.buffer() else {
                            continue;
                        };
                        let Ok(map) = buffer.map_readable() else {
                            continue;
                        };
                        let samples = map
                            .chunks_exact(2)
                            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                            .collect();
                        let block = AudioBlock {
                            samples,
                            captured_at: Some(Instant::now()),
                        };
                        if sender.send(block).is_err() {
                            break;
                        }
                    }
                    None if sink.is_eos() => {
                        *worker_failure
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()) =
                            Some("the audio source stopped".to_owned());
                        break;
                    }
                    None => {}
                }
            }
        })
        .map_err(e)?;
    Ok(AudioCapture {
        spec: AudioSpec {
            sample_rate: RATE,
            channels: 2,
        },
        samples,
        pipeline,
        stop,
        thread: Some(thread),
        failure,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_reads_back_an_mp4() {
        startup().unwrap();
        if gst::ElementFactory::find("x264enc").is_none() || aac_encoder().is_none() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("jiffshot-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.mp4");
        let spec = VideoSpec::for_size(64, 48, 10);
        let audio = AudioSpec {
            sample_rate: 48_000,
            channels: 2,
        };
        let mut writer = Writer::create(&path, spec, Some(audio)).unwrap();
        let step = HNS_PER_SECOND / 10;
        for index in 0..10i64 {
            let shade = (index * 20) as u8;
            let frame: Vec<u8> = (0..64 * 48)
                .flat_map(|_| [shade, 0, 255 - shade, 255])
                .collect();
            writer.write_frame(&frame, index * step, step).unwrap();
            writer
                .write_audio(&vec![1000; 4800 * 2], index * step, step)
                .unwrap();
        }
        writer.finish().unwrap();

        let mut reader = Reader::open(&path).unwrap();
        assert_eq!((reader.width, reader.height), (64, 48));
        assert!(reader.audio.is_some());
        assert!((reader.duration as f64 / HNS_PER_SECOND as f64 - 1.0).abs() < 0.3);
        let first = reader.next_video().unwrap().unwrap();
        assert_eq!(first.bgra.len(), 64 * 48 * 4);
        let later = reader.frame_at(HNS_PER_SECOND / 2).unwrap().unwrap();
        assert!(later.time >= HNS_PER_SECOND / 4);
        reader.select_audio(true).unwrap();
        reader.seek(0).unwrap();
        assert!(!reader.next_audio().unwrap().unwrap().1.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
