//! Interval capture from one portal-selected screen or window.

use crate::linux::capture::CaptureCursor;
use crate::linux::frame_stream::{document_from_frame, frame_from_sample};
use crate::linux::settings::IntervalPreferences;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use jiffshot_doc::Document;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug)]
pub(super) enum Progress {
    Saved { count: u64, path: PathBuf },
    Skipped { count: u64 },
}

#[derive(Debug, Default)]
pub(super) struct Completed {
    pub(super) saved: u64,
    pub(super) skipped: u64,
    pub(super) last_path: Option<PathBuf>,
}

#[derive(Clone, Copy)]
struct Options {
    interval: Duration,
    skip_identical: bool,
    first_frame_timeout: Duration,
}

pub(super) async fn capture(
    preferences: IntervalPreferences,
    cursor: CaptureCursor,
    stop: async_channel::Receiver<()>,
    progress: impl FnMut(Progress),
) -> Result<Completed, String> {
    gst::init().map_err(|error| format!("could not initialize GStreamer: {error}"))?;
    let stream = crate::linux::capture::open_portal_stream(cursor.portal_mode()).await?;
    let options = Options {
        interval: Duration::from_secs(u64::from(preferences.seconds())),
        skip_identical: preferences.skip_identical(),
        first_frame_timeout: FIRST_FRAME_TIMEOUT,
    };
    let result = crate::linux::capture::pipewire_source(stream.remote.as_raw_fd(), stream.node_id)
        .and_then(|source| {
            capture_from_source(
                source,
                options,
                stop,
                crate::linux::capture::save_capture,
                progress,
            )
        });
    stream.finish(result).await
}

fn capture_from_source(
    source: gst::Element,
    options: Options,
    stop: async_channel::Receiver<()>,
    mut save: impl FnMut(&Document) -> Result<PathBuf, String>,
    mut progress: impl FnMut(Progress),
) -> Result<Completed, String> {
    crate::linux::frame_stream::with_source(source, "interval capture", |sink, bus| {
        capture_frames(sink, bus, options, &stop, &mut save, &mut progress)
    })
}

fn capture_frames(
    sink: &gst_app::AppSink,
    bus: &gst::Bus,
    options: Options,
    stop: &async_channel::Receiver<()>,
    save: &mut impl FnMut(&Document) -> Result<PathBuf, String>,
    progress: &mut impl FnMut(Progress),
) -> Result<Completed, String> {
    let started = Instant::now();
    let mut next_capture = started;
    let mut latest = None;
    let mut previous: Option<Document> = None;
    let mut completed = Completed::default();
    loop {
        match stop.try_recv() {
            Ok(()) | Err(async_channel::TryRecvError::Closed) => return Ok(completed),
            Err(async_channel::TryRecvError::Empty) => {}
        }
        if let Some(message) = bus.timed_pop_filtered(
            gst::ClockTime::ZERO,
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) {
            return match message.view() {
                gst::MessageView::Error(error) => {
                    Err(format!("interval capture failed: {}", error.error()))
                }
                _ => Err("the interval capture stream ended unexpectedly".to_owned()),
            };
        }
        if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
            latest = Some(sample);
        } else {
            if matches!(
                sink.current_state(),
                gst::State::Paused | gst::State::Playing
            ) && sink.is_eos()
            {
                return Err("the interval capture stream ended unexpectedly".to_owned());
            }
            if latest.is_none() && started.elapsed() >= options.first_frame_timeout {
                return Err("timed out waiting for the first interval capture frame".to_owned());
            }
        }
        let Some(sample) = latest.as_ref() else {
            continue;
        };
        if Instant::now() < next_capture {
            continue;
        }
        let document = document_from_frame(frame_from_sample(sample)?)?;
        if options.skip_identical
            && previous.as_ref().is_some_and(|last| {
                document.size() == last.size()
                    && document.background().data() == last.background().data()
            })
        {
            completed.skipped = completed
                .skipped
                .checked_add(1)
                .ok_or_else(|| "too many unchanged interval frames".to_owned())?;
            progress(Progress::Skipped {
                count: completed.skipped,
            });
        } else {
            let path = save(&document)?;
            completed.saved = completed
                .saved
                .checked_add(1)
                .ok_or_else(|| "too many interval captures".to_owned())?;
            completed.last_path = Some(path.clone());
            progress(Progress::Saved {
                count: completed.saved,
                path,
            });
            previous = options.skip_identical.then_some(document);
        }
        next_capture = Instant::now()
            .checked_add(options.interval)
            .ok_or_else(|| "the capture interval is too large".to_owned())?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn source() -> gst_app::AppSrc {
        gst::init().expect("GStreamer should initialize");
        gst_app::AppSrc::builder()
            .caps(
                &gst::Caps::builder("video/x-raw")
                    .field("format", "BGRA")
                    .field("width", 2i32)
                    .field("height", 2i32)
                    .field("framerate", gst::Fraction::new(0, 1))
                    .build(),
            )
            .format(gst::Format::Time)
            .is_live(true)
            .build()
    }

    fn options(skip_identical: bool) -> Options {
        Options {
            interval: Duration::from_millis(120),
            skip_identical,
            first_frame_timeout: Duration::from_secs(3),
        }
    }

    fn push_frame(source: &gst_app::AppSrc, bgra: [u8; 4]) {
        source
            .push_buffer(gst::Buffer::from_mut_slice(bgra.repeat(4)))
            .expect("test frame should be queued");
    }

    #[test]
    fn identical_frames_are_skipped_until_stopped() {
        let source = source();
        push_frame(&source, [3, 2, 1, 255]);
        let (stop, receiver) = async_channel::bounded(1);
        let saved = RefCell::new(Vec::new());
        let result = capture_from_source(
            source.upcast(),
            options(true),
            receiver,
            |document| {
                saved
                    .borrow_mut()
                    .push(document.background().data().to_vec());
                Ok(PathBuf::from("first.png"))
            },
            |event| {
                if matches!(event, Progress::Skipped { count: 2 }) {
                    stop.try_send(()).expect("capture should stop");
                }
            },
        )
        .expect("interval capture should finish");
        assert_eq!((result.saved, result.skipped), (1, 2));
        assert_eq!(result.last_path, Some(PathBuf::from("first.png")));
        assert_eq!(
            saved.into_inner(),
            vec![[1, 2, 3, 255].repeat(4)],
            "the initial frame should be saved as RGBA"
        );
    }

    #[test]
    fn unchanged_frames_are_repeated_when_skipping_is_disabled() {
        let source = source();
        push_frame(&source, [30, 20, 10, 255]);
        let (stop, receiver) = async_channel::bounded(1);
        let saved = RefCell::new(Vec::new());
        let started = Instant::now();
        let result = capture_from_source(
            source.upcast(),
            options(false),
            receiver,
            |document| {
                saved
                    .borrow_mut()
                    .push((Instant::now(), document.background().data().to_vec()));
                Ok(PathBuf::from("repeat.png"))
            },
            |event| {
                if matches!(event, Progress::Saved { count: 2, .. }) {
                    stop.try_send(()).expect("capture should stop");
                }
            },
        )
        .expect("interval capture should finish");
        let saved = saved.into_inner();
        assert_eq!((result.saved, result.skipped), (2, 0));
        assert_eq!(saved[0].1, saved[1].1);
        assert!(
            saved[0].0.duration_since(started) < Duration::from_secs(3),
            "the first frame should be captured without waiting for the interval"
        );
        assert!(
            saved[1].0.duration_since(saved[0].0) >= options(false).interval,
            "repeat captures must respect the configured interval"
        );
    }

    #[test]
    fn changed_frames_are_saved_with_skip_identical_enabled() {
        let source = source();
        push_frame(&source, [3, 2, 1, 255]);
        let producer = source.clone();
        let (stop, receiver) = async_channel::bounded(1);
        let saved = RefCell::new(Vec::new());
        let result = capture_from_source(
            source.upcast(),
            options(true),
            receiver,
            |document| {
                let mut saved = saved.borrow_mut();
                saved.push(document.background().data().to_vec());
                if saved.len() == 1 {
                    push_frame(&producer, [9, 8, 7, 255]);
                }
                Ok(PathBuf::from(format!("capture-{}.png", saved.len())))
            },
            |event| {
                if matches!(event, Progress::Saved { count: 2, .. }) {
                    stop.try_send(()).expect("capture should stop");
                }
            },
        )
        .expect("interval capture should finish");
        let saved = saved.into_inner();
        assert_eq!((result.saved, result.skipped), (2, 0));
        assert_eq!(saved[0], [1, 2, 3, 255].repeat(4));
        assert_eq!(saved[1], [7, 8, 9, 255].repeat(4));
        assert_eq!(result.last_path, Some(PathBuf::from("capture-2.png")));
    }

    #[test]
    fn stopping_before_first_frame_saves_nothing() {
        let source = source();
        let (stop, receiver) = async_channel::bounded(1);
        stop.try_send(()).expect("stop should be queued");
        let result = capture_from_source(
            source.upcast(),
            options(true),
            receiver,
            |_| panic!("a stopped capture must not save a frame"),
            |_| panic!("a stopped capture must not report progress"),
        )
        .expect("stopping should succeed");
        assert_eq!((result.saved, result.skipped), (0, 0));
        assert_eq!(result.last_path, None);
    }

    #[test]
    fn reports_a_save_failure_and_stops_the_stream() {
        let source = source();
        push_frame(&source, [3, 2, 1, 255]);
        let (_stop, receiver) = async_channel::bounded(1);
        let error = capture_from_source(
            source.upcast(),
            options(false),
            receiver,
            |_| Err("disk is full".to_owned()),
            |_| panic!("failed saves must not report success"),
        )
        .expect_err("save failure should be returned");
        assert_eq!(error, "disk is full");
    }

    #[test]
    fn reports_missing_first_frame() {
        let source = source();
        let (_stop, receiver) = async_channel::bounded(1);
        let options = Options {
            first_frame_timeout: Duration::from_millis(150),
            ..options(false)
        };
        let error = capture_from_source(
            source.upcast(),
            options,
            receiver,
            |_| panic!("a missing frame must not be saved"),
            |_| panic!("a missing frame must not report progress"),
        )
        .expect_err("missing frames should time out");
        assert!(
            error.contains("timed out waiting for the first interval capture frame"),
            "unexpected error: {error}"
        );
    }
}
