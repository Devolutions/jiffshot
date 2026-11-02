//! Video and audio: an MP4 writer (H.264 video, AAC audio), an MP4 reader
//! that decodes to top-down BGRA frames and 16-bit PCM, webcam frames, and
//! capture of system audio or the microphone. Media Foundation and WASAPI
//! do the work on Windows; AVFoundation and ScreenCaptureKit on macOS.
//!
//! Times are in 100-nanosecond units ("hns") on every platform.

#[cfg_attr(windows, path = "win/media.rs")]
#[cfg_attr(target_os = "macos", path = "mac/media.rs")]
#[cfg_attr(target_os = "linux", path = "linux/media.rs")]
mod imp;

pub use imp::{capture_audio, capture_audio_cancellable, startup, AudioCapture, Reader, Writer};

#[path = "media/post.rs"]
pub mod post;

pub type Result<T> = std::result::Result<T, String>;

pub const HNS_PER_SECOND: i64 = 10_000_000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VideoSpec {
    /// Even width and height (H.264 needs them).
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
}

impl VideoSpec {
    /// A spec for `width` x `height` (rounded down to even) with a bitrate
    /// that keeps screen text sharp.
    pub fn for_size(width: u32, height: u32, fps: u32) -> VideoSpec {
        let (w, h) = (width & !1, height & !1);
        let bitrate = ((w as u64 * h as u64 * fps as u64) / 6).clamp(1_500_000, 40_000_000) as u32;
        VideoSpec {
            width: w,
            height: h,
            fps,
            bitrate,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioSpec {
    pub sample_rate: u32,
    pub channels: u32,
}

/// Captured interleaved stereo PCM and the monotonic time of its first frame.
/// Windows does not currently expose a capture timestamp.
#[derive(Debug)]
pub struct AudioBlock {
    pub samples: Vec<i16>,
    pub captured_at: Option<std::time::Instant>,
}

/// A decoded video frame.
pub struct VideoFrame {
    pub time: i64,
    pub width: u32,
    pub height: u32,
    /// Top-down BGRA, `width * 4` bytes per row.
    pub bgra: Vec<u8>,
}

/// Silence for `frames` stereo frames.
pub fn silence(frames: usize) -> Vec<i16> {
    vec![0; frames * 2]
}

#[cfg(any(windows, test))]
fn wait_audio_start<T>(
    receiver: &std::sync::mpsc::Receiver<Result<T>>,
    cancel: &std::sync::atomic::AtomicBool,
    stop: &std::sync::atomic::AtomicBool,
    timeout: std::time::Duration,
) -> Result<T> {
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + timeout;
    loop {
        if cancel.load(Ordering::Acquire) {
            stop.store(true, Ordering::Release);
            return Err("audio capture was cancelled".into());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            stop.store(true, Ordering::Release);
            return Err("audio device did not start".into());
        }
        match receiver.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(result) => {
                if cancel.load(Ordering::Acquire) {
                    stop.store(true, Ordering::Release);
                    return Err("audio capture was cancelled".into());
                }
                if result.is_err() {
                    stop.store(true, Ordering::Release);
                }
                return result;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                stop.store(true, Ordering::Release);
                return Err("audio device stopped while starting".into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_start_timeout_cancels_a_late_worker() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{mpsc, Arc};
        use std::time::Duration;

        let cancel = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let activated = Arc::new(AtomicBool::new(false));
        let (ready, ready_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (worker_stop, worker_activated) = (stop.clone(), activated.clone());
        let worker = std::thread::spawn(move || {
            release_rx.recv().unwrap();
            if !worker_stop.load(Ordering::Acquire) {
                worker_activated.store(true, Ordering::Release);
            }
            let _ = ready.send(Ok(()));
        });
        assert_eq!(
            wait_audio_start(&ready_rx, &cancel, &stop, Duration::from_millis(10)),
            Err("audio device did not start".into())
        );
        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(!cancel.load(Ordering::Acquire));
        assert!(!activated.load(Ordering::Acquire));
    }

    #[test]
    fn audio_start_reports_cancellation_failure_and_success() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use std::time::Duration;

        let cancel = AtomicBool::new(false);
        let stop = AtomicBool::new(false);
        let (ready, receiver) = mpsc::channel();
        ready.send(Ok(7)).unwrap();
        assert_eq!(
            wait_audio_start(&receiver, &cancel, &stop, Duration::from_secs(1)),
            Ok(7)
        );
        assert!(!stop.load(Ordering::Acquire));

        ready.send(Err("permission denied".into())).unwrap();
        assert_eq!(
            wait_audio_start(&receiver, &cancel, &stop, Duration::from_secs(1)),
            Err("permission denied".into())
        );
        assert!(stop.load(Ordering::Acquire));
        assert!(!cancel.load(Ordering::Acquire));

        cancel.store(true, Ordering::Release);
        assert_eq!(
            wait_audio_start(&receiver, &cancel, &stop, Duration::from_secs(1)),
            Err("audio capture was cancelled".into())
        );
    }

}

#[cfg(test)]
mod probe {
    /// `JIFFSHOT_PROBE=<file.mp4> cargo test probe -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn probe_file() {
        let path = std::env::var("JIFFSHOT_PROBE").unwrap();
        let mut r = super::Reader::open(std::path::Path::new(&path)).unwrap();
        println!(
            "{}x{} {:.2}s fps {} audio {:?}",
            r.width,
            r.height,
            r.duration as f64 / 1e7,
            r.fps,
            r.audio
        );
        let mut n = 0;
        loop {
            match r.next_video() {
                Ok(Some(f)) => {
                    n += 1;
                    if n % 20 == 0 {
                        println!("  frame {n} at {}", f.time);
                    }
                }
                Ok(None) => {
                    println!("end after {n} frames");
                    break;
                }
                Err(err) => {
                    println!("error after {n} frames: {err}");
                    break;
                }
            }
        }
        for t in [0i64, 1, 5_000_000, 25_000_000] {
            let mut r = super::Reader::open(std::path::Path::new(&path)).unwrap();
            println!("seek {t}: {:?}", r.seek(t));
            match r.next_video() {
                Ok(Some(f)) => println!("  frame at {}", f.time),
                other => println!("  {:?}", other.map(|f| f.map(|f| f.time))),
            }
        }
    }
}
