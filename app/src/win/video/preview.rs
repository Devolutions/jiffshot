//! Latest-request-only frame decoding, off the UI thread.

use super::{app, show, to_pixmap, Pixmap, Reader, HNS_PER_SECOND};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

#[derive(Clone, Copy)]
struct Request {
    time: i64,
    revision: u64,
}

#[derive(Default)]
struct Pending {
    request: Option<Request>,
    closed: bool,
}

#[derive(Default)]
struct State {
    pending: Mutex<Pending>,
    wake: Condvar,
    revision: AtomicU64,
}

pub(super) struct Preview {
    state: Arc<State>,
}

impl Preview {
    pub fn new(path: &Path) -> Result<Self, String> {
        let path = path.to_path_buf();
        let mut reader = None;
        Self::spawn(
            move |time, current| {
                if reader.is_none() {
                    reader = Some(Reader::open(&path)?);
                }
                let reader = reader.as_mut().expect("the preview reader was opened");
                reader.seek(time)?;
                let half = (HNS_PER_SECOND as f64 / reader.fps.max(1.0) / 2.0) as i64;
                let mut last = None;
                while current() {
                    let Some(frame) = reader.next_video()? else {
                        break;
                    };
                    let done = frame.time + half >= time;
                    last = Some(frame);
                    if done {
                        break;
                    }
                }
                if !current() {
                    return Ok(None);
                }
                last.map(|frame| {
                    to_pixmap(&frame).ok_or("Could not display the video frame.".into())
                })
                .transpose()
            },
            |state, revision, result| {
                app::post(move |app| {
                    if state.revision.load(Ordering::Acquire) != revision {
                        return;
                    }
                    let Some(video) = app
                        .video
                        .as_mut()
                        .filter(|video| Arc::ptr_eq(&video.preview.state, &state))
                    else {
                        return;
                    };
                    match result {
                        Ok(Some(frame)) => {
                            video.current = Some(frame.clone());
                            show(app, frame);
                        }
                        Ok(None) => app.set_status("No video frame is available at this position."),
                        Err(error) => app.set_status(format!("Video preview failed: {error}")),
                    }
                });
            },
        )
    }

    fn spawn(
        mut decode: impl FnMut(i64, &dyn Fn() -> bool) -> Result<Option<Pixmap>, String>
            + Send
            + 'static,
        publish: impl Fn(Arc<State>, u64, Result<Option<Pixmap>, String>) + Send + 'static,
    ) -> Result<Self, String> {
        let state = Arc::<State>::default();
        let worker = state.clone();
        std::thread::Builder::new()
            .name("video-preview".into())
            .spawn(move || loop {
                let request = {
                    let pending = worker
                        .pending
                        .lock()
                        .expect("preview request lock poisoned");
                    let mut pending = worker
                        .wake
                        .wait_while(pending, |pending| {
                            pending.request.is_none() && !pending.closed
                        })
                        .expect("preview request lock poisoned");
                    if pending.closed {
                        break;
                    }
                    pending
                        .request
                        .take()
                        .expect("the preview request is ready")
                };
                let current = || worker.revision.load(Ordering::Acquire) == request.revision;
                if !current() {
                    continue;
                }
                let result = decode(request.time, &current);
                if current() {
                    publish(worker.clone(), request.revision, result);
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(Self { state })
    }

    pub fn request(&self, time: i64) {
        let mut pending = self
            .state
            .pending
            .lock()
            .expect("preview request lock poisoned");
        let revision = self.state.revision.fetch_add(1, Ordering::AcqRel) + 1;
        pending.request = Some(Request { time, revision });
        self.state.wake.notify_one();
    }

    pub fn cancel(&self) {
        let mut pending = self
            .state
            .pending
            .lock()
            .expect("preview request lock poisoned");
        self.state.revision.fetch_add(1, Ordering::AcqRel);
        pending.request = None;
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        let mut pending = self
            .state
            .pending
            .lock()
            .expect("preview request lock poisoned");
        self.state.revision.fetch_add(1, Ordering::AcqRel);
        pending.closed = true;
        self.state.wake.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn dragging_does_not_wait_for_decoding_and_only_the_latest_frame_is_published() {
        let (started, decoding) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let (published, result) = mpsc::channel();
        let preview = Preview::spawn(
            move |time, current| {
                started.send(time).unwrap();
                if time == 0 {
                    blocked.recv().unwrap();
                    assert!(!current(), "a superseded decode must be cancelled");
                }
                Ok(Pixmap::new(u32::try_from(time).unwrap() + 1, 1))
            },
            move |_, _, frame| published.send(frame.unwrap().unwrap()).unwrap(),
        )
        .unwrap();
        preview.request(0);
        assert_eq!(decoding.recv_timeout(Duration::from_secs(2)).unwrap(), 0);
        for time in 1..=10_000 {
            preview.request(time);
        }
        release.send(()).unwrap();
        assert_eq!(
            decoding.recv_timeout(Duration::from_secs(2)).unwrap(),
            10_000,
            "intermediate pointer positions must not be queued for decoding"
        );
        assert_eq!(
            result.recv_timeout(Duration::from_secs(2)).unwrap().width(),
            10_001,
            "the displayed frame must belong to the final pointer position"
        );
        assert!(
            result.try_recv().is_err(),
            "the superseded frame must not be published"
        );
    }

    #[test]
    fn cancelled_or_closed_previews_do_not_publish_in_flight_frames() {
        for close in [false, true] {
            let (started, decoding) = mpsc::channel();
            let (release, blocked) = mpsc::channel();
            let (finished, done) = mpsc::channel();
            let (published, result) = mpsc::channel();
            let preview = Preview::spawn(
                move |_, current| {
                    started.send(()).unwrap();
                    blocked.recv().unwrap();
                    assert!(!current(), "a stopped preview must cancel its decoder");
                    finished.send(()).unwrap();
                    Ok(Pixmap::new(1, 1))
                },
                move |_, _, _| published.send(()).unwrap(),
            )
            .unwrap();
            preview.request(1);
            decoding.recv_timeout(Duration::from_secs(2)).unwrap();
            if close {
                drop(preview);
            } else {
                preview.cancel();
            }
            release.send(()).unwrap();
            done.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(
                result.recv_timeout(Duration::from_millis(50)).is_err(),
                "cancelled frames must not replace the current editor frame"
            );
        }
    }
}
