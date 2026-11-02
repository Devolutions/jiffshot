use std::sync::{Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

enum State {
    Empty,
    Running(JoinHandle<Result<i32, String>>),
    Stopping,
    Stopped { result: Result<(), String>, failure_acknowledged: bool },
}

struct Tracker {
    state: Mutex<State>,
    changed: Condvar,
}

impl Tracker {
    const fn new() -> Self {
        Self { state: Mutex::new(State::Empty), changed: Condvar::new() }
    }

    fn start(&self, spawn: impl FnOnce() -> std::io::Result<JoinHandle<Result<i32, String>>>) -> std::io::Result<()> {
        let mut state = self.state.lock().map_err(|_| std::io::Error::other("the capture engine state is poisoned"))?;
        if !matches!(*state, State::Empty) {
            return Err(std::io::Error::other("the capture engine is already registered"));
        }
        *state = State::Running(spawn()?);
        Ok(())
    }

    fn running(&self) -> Result<bool, String> {
        let state = self.state.lock().map_err(|_| "the capture engine state is poisoned".to_owned())?;
        Ok(match &*state {
            State::Running(engine) => !engine.is_finished(),
            State::Stopping => true,
            State::Empty | State::Stopped { .. } => false,
        })
    }

    fn result(&self) -> Result<Option<Result<(), String>>, String> {
        let state = self.state.lock().map_err(|_| "the capture engine state is poisoned".to_owned())?;
        Ok(match &*state {
            State::Stopped { result, .. } => Some(result.clone()),
            _ => None,
        })
    }

    fn acknowledge_failure(&self) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|_| "the capture engine state is poisoned".to_owned())?;
        if let State::Stopped { result: Err(_), failure_acknowledged } = &mut *state {
            *failure_acknowledged = true;
        }
        Ok(())
    }

    fn stop(&self, timeout: Duration, request_stop: impl Fn()) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|_| "the capture engine state is poisoned".to_owned())?;
        let engine = loop {
            match &*state {
                State::Empty => return Ok(()),
                State::Stopped { result, failure_acknowledged } => {
                    return if *failure_acknowledged { Ok(()) } else { result.clone() };
                }
                State::Stopping => {
                    state = self.changed.wait(state).map_err(|_| "the capture engine state is poisoned".to_owned())?;
                }
                State::Running(_) => {
                    let State::Running(engine) = std::mem::replace(&mut *state, State::Stopping) else { unreachable!() };
                    break engine;
                }
            }
        };
        drop(state);

        let deadline = Instant::now() + timeout;
        while !engine.is_finished() {
            request_stop();
            if Instant::now() >= deadline {
                let mut state = self.state.lock().map_err(|_| "the capture engine state is poisoned".to_owned())?;
                *state = State::Running(engine);
                self.changed.notify_all();
                return Err("the capture engine did not stop in time".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let result = match engine.join() {
            Ok(Ok(0)) => Ok(()),
            Ok(Ok(code)) => Err(format!("the capture engine stopped with code {code}")),
            Ok(Err(error)) => Err(error),
            Err(_) => Err("the capture engine panicked".into()),
        };
        let mut state = self.state.lock().map_err(|_| "the capture engine state is poisoned".to_owned())?;
        *state = State::Stopped { result: result.clone(), failure_acknowledged: false };
        self.changed.notify_all();
        result
    }
}

static ENGINE: Tracker = Tracker::new();

pub(crate) fn start(spawn: impl FnOnce() -> std::io::Result<JoinHandle<Result<i32, String>>>) -> std::io::Result<()> {
    ENGINE.start(spawn)
}

pub(crate) fn running() -> Result<bool, String> {
    ENGINE.running()
}

pub(crate) fn result() -> Result<Option<Result<(), String>>, String> {
    ENGINE.result()
}

pub(crate) fn acknowledge_failure() -> Result<(), String> {
    ENGINE.acknowledge_failure()
}

pub(crate) fn stop() -> Result<(), String> {
    // AVFoundation permits up to 120 seconds to finish a recording.
    ENGINE.stop(Duration::from_secs(130), jiffshot_workflow::stop_engine)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn stop_joins_once_and_remembers_the_result() {
        let tracker = Tracker::new();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        tracker.start(|| Ok(std::thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(0)
        }))).unwrap();
        assert!(tracker.running().unwrap());
        tracker.stop(Duration::from_secs(1), || stop.store(true, Ordering::Release)).unwrap();
        assert!(!tracker.running().unwrap());
        assert_eq!(tracker.result().unwrap(), Some(Ok(())));
        tracker.stop(Duration::from_secs(1), || panic!("already stopped")).unwrap();
    }

    #[test]
    fn timeout_retains_the_running_engine_for_retry() {
        let tracker = Tracker::new();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        tracker.start(|| Ok(std::thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(0)
        }))).unwrap();
        assert!(tracker.stop(Duration::from_millis(30), || {}).is_err());
        assert!(tracker.running().unwrap());
        assert_eq!(tracker.result().unwrap(), None);
        tracker.stop(Duration::from_secs(1), || stop.store(true, Ordering::Release)).unwrap();
        assert_eq!(tracker.result().unwrap(), Some(Ok(())));
    }

    #[test]
    fn stop_retries_until_the_engine_is_ready() {
        let tracker = Tracker::new();
        let ready = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_ready = ready.clone();
        let worker_stop = stop.clone();
        tracker.start(|| Ok(std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            worker_ready.store(true, Ordering::Release);
            while !worker_stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(0)
        }))).unwrap();
        let attempts = AtomicUsize::new(0);
        tracker.stop(Duration::from_secs(1), || {
            attempts.fetch_add(1, Ordering::Relaxed);
            if ready.load(Ordering::Acquire) {
                stop.store(true, Ordering::Release);
            }
        }).unwrap();
        assert!(attempts.load(Ordering::Relaxed) > 1);
    }

    #[test]
    fn nonzero_engine_exit_is_reported() {
        let tracker = Tracker::new();
        tracker.start(|| Ok(std::thread::spawn(|| Ok(7)))).unwrap();
        let error = tracker.stop(Duration::from_secs(1), || {}).unwrap_err();
        assert!(error.contains("code 7"));
        assert_eq!(tracker.result().unwrap(), Some(Err(error.clone())));
        assert_eq!(tracker.stop(Duration::from_secs(1), || {}).unwrap_err(), error);
        tracker.acknowledge_failure().unwrap();
        tracker.stop(Duration::from_secs(1), || {}).unwrap();
        assert_eq!(tracker.result().unwrap(), Some(Err(error)));
    }

    #[test]
    fn concurrent_stops_wait_for_the_same_engine() {
        let tracker = Arc::new(Tracker::new());
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        tracker.start(|| Ok(std::thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            std::thread::sleep(Duration::from_millis(30));
            Ok(0)
        }))).unwrap();
        let other_tracker = tracker.clone();
        let other_stop = stop.clone();
        let other = std::thread::spawn(move || {
            other_tracker.stop(Duration::from_secs(1), || other_stop.store(true, Ordering::Release))
        });
        tracker.stop(Duration::from_secs(1), || stop.store(true, Ordering::Release)).unwrap();
        other.join().unwrap().unwrap();
        assert_eq!(tracker.result().unwrap(), Some(Ok(())));
    }
}
