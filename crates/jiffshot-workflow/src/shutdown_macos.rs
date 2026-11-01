use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread::JoinHandle;

struct Shutdown {
    requested: AtomicBool,
    failures: Mutex<Vec<String>>,
    jobs: Mutex<Vec<Job>>,
}

struct Job {
    handle: JoinHandle<Option<String>>,
    pending_at_shutdown: bool,
}

impl Shutdown {
    const fn new() -> Self {
        Self { requested: AtomicBool::new(false), failures: Mutex::new(Vec::new()), jobs: Mutex::new(Vec::new()) }
    }

    fn begin(&self) -> io::Result<()> {
        if !self.jobs.lock().map_err(|_| io::Error::other("the capture finish state is poisoned"))?.is_empty() {
            return Err(io::Error::other("capture finish jobs remain from a previous capture engine"));
        }
        self.failures.lock().map_err(|_| io::Error::other("the capture shutdown state is poisoned"))?.clear();
        self.requested.store(false, Ordering::Release);
        Ok(())
    }

    fn request(&self) {
        match self.jobs.lock() {
            Ok(mut jobs) => {
                for job in jobs.iter_mut() {
                    job.pending_at_shutdown |= !job.handle.is_finished();
                }
                self.requested.store(true, Ordering::Release);
            }
            Err(_) => {
                self.requested.store(true, Ordering::Release);
                crate::log("could not inspect capture finish jobs: the shutdown state is poisoned");
            }
        }
    }

    fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    fn work_failed(&self, error: &str) {
        if self.requested() {
            match self.failures.lock() {
                Ok(mut failures) => failures.push(error.to_owned()),
                Err(_) => crate::log("could not record a shutdown failure: the capture state is poisoned"),
            }
        }
    }

    fn spawn<F>(&self, job: F) -> Result<(), (io::Error, F)>
    where
        F: FnOnce() -> Option<String> + Send + 'static,
    {
        let mut jobs = match self.jobs.lock() {
            Ok(jobs) => jobs,
            Err(_) => return Err((io::Error::other("the capture finish state is poisoned"), job)),
        };
        if !self.requested() {
            let mut active = Vec::new();
            for job in jobs.drain(..) {
                if job.handle.is_finished() {
                    if job.handle.join().is_err() {
                        crate::log("a capture finish worker panicked");
                    }
                } else {
                    active.push(job);
                }
            }
            *jobs = active;
        }
        let pending_at_shutdown = self.requested();
        let (sender, receiver) = std::sync::mpsc::sync_channel::<F>(1);
        let handle = match std::thread::Builder::new().name("jiffshot-capture-finish".into()).spawn(move || {
            match receiver.recv() {
                Ok(job) => job(),
                Err(_) => Some("the capture finish worker did not receive its work".into()),
            }
        }) {
            Ok(handle) => handle,
            Err(error) => return Err((error, job)),
        };
        if let Err(error) = sender.send(job) {
            let _ = handle.join();
            return Err((io::Error::other("the capture finish worker stopped before receiving its work"), error.0));
        }
        jobs.push(Job { handle, pending_at_shutdown });
        Ok(())
    }

    fn spawn_or_run(&self, job: impl FnOnce() -> Option<String> + Send + 'static) {
        if let Err((error, job)) = self.spawn(job) {
            crate::log(&format!("could not start a capture finish worker; finishing on the capture thread: {error}"));
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)) {
                Ok(Some(error)) => self.work_failed(&error),
                Ok(None) => {}
                Err(_) => {
                    let error = "a capture finish worker panicked";
                    crate::log(error);
                    if let Some(hooks) = crate::hooks() {
                        (hooks.notify)("Capture finalization failed", error, true);
                    }
                    self.work_failed(error);
                }
            }
        }
    }

    fn finish(&self) -> Result<(), String> {
        // The capture thread registers all jobs before its message loop returns;
        // each job completes its own work rather than enqueueing another job.
        let jobs = std::mem::take(&mut *self.jobs.lock().map_err(|_| "the capture finish state is poisoned".to_owned())?);
        let mut errors = Vec::new();
        for job in jobs {
            match job.handle.join() {
                Ok(Some(error)) if job.pending_at_shutdown => errors.push(error),
                Ok(Some(_)) => {}
                Ok(None) => {}
                Err(_) => errors.push("a capture finish worker panicked".into()),
            }
        }
        errors.append(&mut *self.failures.lock().map_err(|_| "the capture shutdown state is poisoned".to_owned())?);
        if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) }
    }
}

static SHUTDOWN: Shutdown = Shutdown::new();

pub(crate) fn begin() -> io::Result<()> { SHUTDOWN.begin() }
pub(crate) fn request() { SHUTDOWN.request(); }
pub(crate) fn requested() -> bool { SHUTDOWN.requested() }
pub(crate) fn work_failed(error: &str) { SHUTDOWN.work_failed(error); }
pub(crate) fn spawn(job: impl FnOnce() -> Option<String> + Send + 'static) { SHUTDOWN.spawn_or_run(job); }
pub(crate) fn finish() -> Result<(), String> { SHUTDOWN.finish() }

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn shutdown_waits_for_video_finishing_and_reports_failures() {
        let shutdown = Shutdown::new();
        shutdown.begin().unwrap();
        let (release, wait) = mpsc::channel();
        assert!(shutdown.spawn(move || {
            wait.recv().unwrap();
            Some("sharing failed".into())
        }).is_ok());
        shutdown.request();
        shutdown.work_failed("recording could not be published; staged file preserved");
        release.send(()).unwrap();
        let error = shutdown.finish().unwrap_err();
        assert!(error.contains("sharing failed"));
        assert!(error.contains("staged file preserved"));
        shutdown.begin().unwrap();
    }

    #[test]
    fn earlier_recording_errors_do_not_block_shutdown() {
        let shutdown = Shutdown::new();
        shutdown.begin().unwrap();
        shutdown.work_failed("an earlier recording failed");
        shutdown.request();
        shutdown.finish().unwrap();
    }

    #[test]
    fn finished_share_errors_do_not_block_a_later_quit() {
        let shutdown = Shutdown::new();
        shutdown.begin().unwrap();
        let (done, completed) = mpsc::channel();
        assert!(shutdown.spawn(move || {
            done.send(()).unwrap();
            Some("an earlier share failed".into())
        }).is_ok());
        completed.recv().unwrap();
        while !shutdown.jobs.lock().unwrap()[0].handle.is_finished() {
            std::thread::yield_now();
        }
        shutdown.request();
        shutdown.finish().unwrap();
    }

    #[test]
    fn video_follow_up_started_during_shutdown_is_joined() {
        let shutdown = Shutdown::new();
        shutdown.begin().unwrap();
        shutdown.request();
        assert!(shutdown.spawn(|| Some("sharing failed while quitting".into())).is_ok());
        assert!(shutdown.finish().unwrap_err().contains("sharing failed while quitting"));
    }

    #[test]
    fn shutdown_waits_for_an_in_flight_capture_to_finish() {
        let shutdown = std::sync::Arc::new(Shutdown::new());
        shutdown.begin().unwrap();
        let (release, wait) = mpsc::channel();
        assert!(shutdown.spawn(move || {
            wait.recv().unwrap();
            None
        }).is_ok());
        shutdown.request();
        let (done, completed) = mpsc::channel();
        let waiter = shutdown.clone();
        let thread = std::thread::spawn(move || done.send(waiter.finish()).unwrap());
        assert!(completed.recv_timeout(std::time::Duration::from_millis(50)).is_err());
        release.send(()).unwrap();
        completed.recv_timeout(std::time::Duration::from_secs(2)).unwrap().unwrap();
        thread.join().unwrap();
    }

    #[test]
    fn unavailable_worker_state_falls_back_to_finishing_inline() {
        let shutdown = std::sync::Arc::new(Shutdown::new());
        let poison = shutdown.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poison.jobs.lock().unwrap();
            panic!("poison the worker state");
        }).join();
        let finished = std::sync::Arc::new(AtomicBool::new(false));
        let done = finished.clone();
        shutdown.spawn_or_run(move || {
            done.store(true, Ordering::Release);
            None
        });
        assert!(finished.load(Ordering::Acquire));
        assert!(shutdown.finish().is_err());
    }
}
