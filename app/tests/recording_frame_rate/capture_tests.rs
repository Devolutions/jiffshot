use super::*;
use gstreamer_app as gst_app;

const NANOS_PER_SECOND: u64 = 1_000_000_000;

#[derive(Debug)]
struct DecodedFrame {
    timestamp: u64,
    color: [u8; 3],
}

#[derive(Debug)]
struct DecodedVideo {
    frame_rate: gst::Fraction,
    dimensions: (u32, u32),
    frames: Vec<DecodedFrame>,
    duration: u64,
}

#[derive(Debug)]
struct DecodedAudio {
    last_timestamp: u64,
    samples: Vec<f32>,
}

fn presentation_timestamp(sample: &gst::Sample, buffer: &gst::BufferRef) -> u64 {
    let timestamp = buffer
        .pts()
        .expect("decoded buffer should have a timestamp");
    let segment = sample
        .segment()
        .and_then(|segment| segment.downcast_ref::<gst::ClockTime>())
        .expect("decoded sample should have a time segment");
    segment
        .to_running_time(timestamp)
        .expect("decoded timestamp should fall within its segment")
        .nseconds()
}

fn decode_video(path: &Path) -> DecodedVideo {
    let pipeline = gst::parse::launch(
        "filesrc name=input ! qtdemux name=demux \
         demux.video_0 ! queue ! decodebin ! videoconvert \
         ! video/x-raw,format=RGB ! appsink name=video sync=false",
    )
    .expect("recorded video decoder should parse")
    .downcast::<gst::Pipeline>()
    .expect("recorded video decoder should be a pipeline");
    pipeline
        .by_name("input")
        .expect("video decoder should have a file source")
        .set_property("location", path.to_string_lossy().as_ref());
    let sink = pipeline
        .by_name("video")
        .expect("video decoder should have a sink")
        .downcast::<gst_app::AppSink>()
        .expect("video sink should be an appsink");
    pipeline
        .set_state(gst::State::Playing)
        .expect("recorded video should start decoding");

    let mut frames = Vec::new();
    let mut frame_rate = None;
    let mut dimensions = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "video decoding timed out"
        );
        if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(250)) {
            let caps = sample.caps().expect("decoded video should have caps");
            let info = gstreamer_video::VideoInfo::from_caps(caps)
                .expect("decoded video caps should be valid");
            assert_eq!(info.format(), gstreamer_video::VideoFormat::Rgb);
            let size = (info.width(), info.height());
            if let Some(previous) = dimensions {
                assert_eq!(size, previous, "decoded dimensions changed");
            } else {
                dimensions = Some(size);
            }
            if let Some(previous) = frame_rate {
                assert_eq!(info.fps(), previous, "decoded frame rate changed");
            } else {
                frame_rate = Some(info.fps());
            }
            let buffer = sample.buffer().expect("decoded frame should have a buffer");
            let timestamp = presentation_timestamp(&sample, buffer);
            let mapped = buffer
                .map_readable()
                .expect("decoded frame should be readable");
            let stride = usize::try_from(info.stride()[0]).expect("RGB stride should be positive");
            let offset = stride * info.height() as usize / 2 + info.width() as usize / 2 * 3;
            let pixel = mapped
                .as_slice()
                .get(offset..offset + 3)
                .expect("decoded frame should contain its center pixel");
            frames.push(DecodedFrame {
                timestamp,
                color: [pixel[0], pixel[1], pixel[2]],
            });
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

    let uri = gst::glib::filename_to_uri(path, None).expect("recording should have a URI");
    let discoverer = gstreamer_pbutils::Discoverer::new(gst::ClockTime::from_seconds(10))
        .expect("media discoverer should be available");
    let duration = discoverer
        .discover_uri(uri.as_str())
        .expect("recording should be discoverable")
        .duration()
        .expect("recording should report its duration")
        .nseconds();
    DecodedVideo {
        frame_rate: frame_rate.expect("recording should contain decoded frames"),
        dimensions: dimensions.expect("recording should have dimensions"),
        frames,
        duration,
    }
}

fn decode_audio(path: &Path) -> DecodedAudio {
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
        .expect("audio decoder should have a file source")
        .set_property("location", path.to_string_lossy().as_ref());
    let sink = pipeline
        .by_name("audio")
        .expect("audio decoder should have a sink")
        .downcast::<gst_app::AppSink>()
        .expect("audio sink should be an appsink");
    pipeline
        .set_state(gst::State::Playing)
        .expect("recorded audio should start decoding");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut last_timestamp = None;
    let mut samples = Vec::new();
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "audio decoding timed out"
        );
        if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(250)) {
            let buffer = sample.buffer().expect("decoded audio should have a buffer");
            let timestamp = presentation_timestamp(&sample, buffer);
            last_timestamp = Some(timestamp);
            let mapped = buffer
                .map_readable()
                .expect("decoded audio should be readable");
            samples.extend(
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
    DecodedAudio {
        last_timestamp: last_timestamp.expect("recording should contain decoded audio"),
        samples,
    }
}

fn audio_amplitude(samples: &[f32], frequency: f64, start: usize, length: usize) -> f64 {
    let window = samples
        .get(start..start + length)
        .expect("decoded recording should contain the requested audio window");
    let (sine, cosine) =
        window
            .iter()
            .enumerate()
            .fold((0.0, 0.0), |(sine, cosine), (index, sample)| {
                let phase = std::f64::consts::TAU * frequency * index as f64 / 48_000.0;
                let sample = f64::from(*sample);
                (sine + sample * phase.sin(), cosine + sample * phase.cos())
            });
    2.0 * sine.hypot(cosine) / window.len() as f64
}

fn assert_frame_pixels(
    path: &Path,
    seconds: f64,
    screen_xy: (u32, u32),
    expected_screen: [u8; 3],
    webcam_xy: (u32, u32),
) {
    let frame = crate::linux::video::frame_at(path, seconds)
        .expect("recording should contain a decodable frame at the requested time");
    let image = frame.flatten();
    for (point, expected) in [(screen_xy, expected_screen), (webcam_xy, [0, 0, 255])] {
        let pixel = image
            .pixel(point.0, point.1)
            .expect("the selected point should be inside the recording")
            .demultiply();
        let actual = [pixel.red(), pixel.green(), pixel.blue()];
        assert!(
            actual
                .into_iter()
                .zip(expected)
                .all(|(actual, expected)| actual.abs_diff(expected) <= 40),
            "frame at {seconds}s, pixel {point:?}: expected {expected:?}, got {actual:?}"
        );
    }
}

fn assert_audio_video_tail_synchronized(video: &DecodedVideo, audio: &DecodedAudio) {
    let video_end = video
        .frames
        .last()
        .expect("recording should have a last video frame")
        .timestamp;
    assert!(
        video_end.abs_diff(audio.last_timestamp) <= 200_000_000,
        "audio and video tails should remain synchronized: video {video_end} ns, \
         audio {} ns",
        audio.last_timestamp
    );
}

#[derive(Clone, Copy)]
enum PortalRecordingCase {
    Pause,
    Restart,
}

fn record_virtual_portal(case: PortalRecordingCase) -> PathBuf {
    gst::init().expect("GStreamer should initialize");
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .expect("real-portal test should use a private runtime directory");
    crate::linux::settings::set_capture_directory(&runtime.join("frame-rate-captures"))
        .expect("real-portal captures should use the private runtime directory");
    crate::linux::settings::set_recording_frame_rate(RecordingFrameRate::Fps24)
        .expect("the selected rate should be saved");
    let selected = crate::linux::settings::recording_frame_rate()
        .expect("the selected rate should load at recording start");
    assert_eq!(selected, RecordingFrameRate::Fps24);
    let (commands, receiver) = async_channel::bounded(8);
    let (states, observed) = std::sync::mpsc::channel();
    let controller = std::thread::spawn(move || {
        let next_state = || {
            observed
                .recv_timeout(Duration::from_secs(25))
                .expect("portal recording should report its state")
        };
        assert_eq!(next_state(), RecordingState::Recording);
        std::thread::sleep(Duration::from_millis(1200));
        match case {
            PortalRecordingCase::Pause => {
                commands
                    .send_blocking(RecordingCommand::TogglePause)
                    .expect("portal pause should be accepted");
                assert_eq!(next_state(), RecordingState::Paused);
                std::thread::sleep(Duration::from_millis(1400));
                commands
                    .send_blocking(RecordingCommand::TogglePause)
                    .expect("portal resume should be accepted");
                assert_eq!(next_state(), RecordingState::Recording);
            }
            PortalRecordingCase::Restart => {
                crate::linux::settings::set_recording_frame_rate(RecordingFrameRate::Fps60)
                    .expect("a new preference should save while recording");
                commands
                    .send_blocking(RecordingCommand::Restart)
                    .expect("portal restart should be accepted");
                assert_eq!(next_state(), RecordingState::Restarting);
                assert_eq!(next_state(), RecordingState::Recording);
            }
        }
        std::thread::sleep(Duration::from_millis(1200));
        commands
            .send_blocking(RecordingCommand::Stop)
            .expect("portal stop should be accepted");
        assert_eq!(next_state(), RecordingState::Stopping);
    });
    let result = futures_lite::future::block_on(record_portal_sources(
        receiver,
        RecordingAudio::SystemAndMicrophone,
        CaptureCursor::Hidden,
        selected,
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
                .expect("portal test should receive states")
        },
    ));
    controller
        .join()
        .expect("portal recording controller should finish");
    result
        .expect("real portal recording should finalize")
        .expect("real portal recording should be saved")
}

fn assert_virtual_portal_media(path: &Path, duration_range: std::ops::RangeInclusive<u64>) {
    let video = decode_video(path);
    let audio = decode_audio(path);
    assert_eq!(
        video.dimensions,
        (1280, 720),
        "Sway output dimensions changed"
    );
    assert_eq!(video.frame_rate, gst::Fraction::new(24, 1));
    assert!(
        duration_range.contains(&video.duration),
        "unexpected portal recording duration: {} ns",
        video.duration
    );
    assert!(
        video.frames.len() >= 15,
        "portal recording should contain actual decoded frames, got {}",
        video.frames.len()
    );
    for pair in video.frames.windows(2) {
        let spacing = pair[1].timestamp - pair[0].timestamp;
        assert!(
            spacing.abs_diff(NANOS_PER_SECOND / 24) <= 2_000_000,
            "portal frames should have 24 fps spacing, got {spacing} ns"
        );
    }
    assert_frame_pixels(path, 0.4, (320, 180), [0x20, 0x40, 0x60], (1104, 614));
    let start_440 = audio_amplitude(&audio.samples, 440.0, 9_600, 9_600);
    let start_880 = audio_amplitude(&audio.samples, 880.0, 9_600, 9_600);
    assert!(
        start_440 > 0.03,
        "virtual system tone is missing: {start_440}"
    );
    assert!(
        start_880 > 0.03,
        "virtual microphone tone is missing: {start_880}"
    );
    assert_audio_video_tail_synchronized(&video, &audio);
}

#[test]
#[ignore = "requires a ScreenCast portal and virtual system/microphone audio"]
fn portal_fixed_rate_pause_preserves_video_webcam_and_virtual_audio() {
    let path = record_virtual_portal(PortalRecordingCase::Pause);
    let video = decode_video(&path);
    let audio = decode_audio(&path);
    assert_virtual_portal_media(&path, 1_700_000_000..=3_500_000_000);
    assert_frame_pixels(&path, 1.5, (320, 180), [0x20, 0x40, 0x60], (1104, 614));
    for frequency in [440.0, 880.0] {
        let resumed = audio_amplitude(&audio.samples, frequency, 72_000, 9_600);
        assert!(
            resumed > 0.03,
            "{frequency} Hz should survive resume: {resumed}"
        );
    }
    assert_audio_video_tail_synchronized(&video, &audio);
    std::fs::remove_file(path).expect("portal test recording should be removed");
}

#[test]
#[ignore = "requires a ScreenCast portal and virtual system/microphone audio"]
fn portal_fixed_rate_restart_keeps_original_choice_and_discards_old_segment() {
    let path = record_virtual_portal(PortalRecordingCase::Restart);
    assert_eq!(
        crate::linux::settings::recording_frame_rate()
            .expect("the changed preference should remain saved"),
        RecordingFrameRate::Fps60
    );
    assert_virtual_portal_media(&path, 700_000_000..=2_200_000_000);
    std::fs::remove_file(path).expect("portal test recording should be removed");
}

#[test]
#[ignore = "requires a ScreenCast portal and real GUI test fixture"]
fn portal_fixed_rate_gui_restart_and_command_line_use_saved_rate() {
    gst::init().expect("GStreamer should initialize");
    let directory = tempfile::tempdir().expect("temporary directory should be available");
    let app = std::env::var_os("JIFFSHOT_FRAME_RATE_GUI_APP")
        .expect("real GUI portal test should provide the app binary");
    let runtime = std::env::var_os("JIFFSHOT_FRAME_RATE_GUI_RUNTIME")
        .expect("real GUI portal test should provide the app runtime");
    let script = std::env::var_os("JIFFSHOT_FRAME_RATE_GUI_SCRIPT")
        .expect("real GUI portal test should provide its driver script");
    let output = std::process::Command::new("pwsh")
        .arg("-NoProfile")
        .arg("-File")
        .arg(script)
        .arg("-Binary")
        .arg(app)
        .arg("-RuntimeDir")
        .arg(runtime)
        .arg("-RealPortal")
        .arg("-CaptureDir")
        .arg(directory.path())
        .output()
        .expect("the real GUI driver should start");
    let stdout = String::from_utf8_lossy(&output.stdout);
    eprintln!("{stdout}");
    eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "real GUI driver failed: {}",
        output.status
    );
    assert!(
        stdout.contains("PASS test_real_frame_rate_gui_and_command_line_recording_after_save")
            && stdout.contains("GUI test result: ok. 1 passed"),
        "real GUI driver must report its one named, completed test"
    );

    let recordings: Vec<_> = std::fs::read_dir(directory.path())
        .expect("capture directory should be readable")
        .map(|entry| {
            entry
                .expect("capture directory entry should be readable")
                .path()
        })
        .filter(|path| path.extension().is_some_and(|extension| extension == "mp4"))
        .collect();
    assert_eq!(
        recordings.len(),
        2,
        "the GUI and command-line starts must each save an MP4"
    );
    for recording in &recordings {
        let video = decode_video(recording);
        assert_eq!(video.dimensions, (1280, 720));
        assert_eq!(
            video.frame_rate,
            gst::Fraction::new(30, 1),
            "both real recording entry points must use the GUI-saved choice"
        );
        assert!(
            video.frames.len() >= 15,
            "the real recording must contain decoded video"
        );
        assert!(
            (800_000_000..=15_000_000_000).contains(&video.duration),
            "unexpected real GUI recording duration: {} ns",
            video.duration
        );
        for pair in video.frames.windows(2) {
            let spacing = pair[1]
                .timestamp
                .checked_sub(pair[0].timestamp)
                .expect("real GUI video timestamps must be monotonic");
            assert!(
                spacing.abs_diff(NANOS_PER_SECOND / 30) <= 2_000_000,
                "real GUI video must have 30 fps spacing, got {spacing} ns"
            );
        }
        for index in [0, video.frames.len() / 2, video.frames.len() - 1] {
            let actual = video.frames[index].color;
            assert!(
                actual
                    .into_iter()
                    .zip([0x20, 0x40, 0x60])
                    .all(|(value, expected): (u8, u8)| value.abs_diff(expected) <= 40),
                "real portal frame {index} must contain the selected desktop: {actual:?}"
            );
        }
    }
}
