use rustolonia::AppScope;
use std::collections::VecDeque;
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LockResult, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

type Callback = Box<dyn FnOnce() + Send>;
type PendingCallback = Arc<Mutex<Option<Callback>>>;

struct Work {
    callback: PendingCallback,
    delayed: bool,
    description: &'static str,
    failures: AtomicUsize,
}

#[derive(Default)]
struct Queue {
    pending: Mutex<VecDeque<Arc<Work>>>,
    changed: Condvar,
    stopping: AtomicBool,
}

impl Queue {
    fn push(&self, work: Work) -> Result<(), String> {
        let mut pending = lock_state(&self.pending, "shutdown queue");
        if self.stopping.load(Ordering::Acquire) {
            return Err("the shutdown dispatcher is stopping".into());
        }
        pending.push_back(Arc::new(work));
        self.changed.notify_one();
        Ok(())
    }

    fn next(&self) -> Option<Arc<Work>> {
        let mut pending = lock_state(&self.pending, "shutdown queue");
        loop {
            if self.stopping.load(Ordering::Acquire) {
                return None;
            }
            if let Some(work) = pending.front() {
                return Some(work.clone());
            }
            pending = recover_lock(&self.pending, self.changed.wait(pending), "shutdown queue");
        }
    }

    fn finish(&self, work: &Arc<Work>) {
        let mut pending = lock_state(&self.pending, "shutdown queue");
        assert!(pending.front().is_some_and(|front| Arc::ptr_eq(front, work)), "the shutdown queue changed unexpectedly");
        pending.pop_front();
    }

    fn defer(&self, work: &Arc<Work>) {
        let mut pending = lock_state(&self.pending, "shutdown queue");
        assert!(pending.front().is_some_and(|front| Arc::ptr_eq(front, work)), "the shutdown queue changed unexpectedly");
        pending.rotate_left(1);
    }

    fn stop(&self) {
        let _pending = lock_state(&self.pending, "shutdown queue");
        self.stopping.store(true, Ordering::Release);
        self.changed.notify_all();
    }
}

struct Dispatcher {
    queue: Arc<Queue>,
    worker: JoinHandle<()>,
}

static DISPATCHER: Mutex<Option<Dispatcher>> = Mutex::new(None);

pub(crate) fn report(message: &str) {
    if catch_unwind(|| jiffshot_workflow::log(message)).is_err() {
        use std::io::Write;
        let _ = writeln!(std::io::stderr().lock(), "shutdown logging failed: {message}");
    }
}

fn recover_lock<'a, T>(mutex: &Mutex<T>, result: LockResult<MutexGuard<'a, T>>, name: &str) -> MutexGuard<'a, T> {
    match result {
        Ok(guard) => guard,
        Err(poisoned) => {
            // These locks protect only state updates; callbacks run without a guard.
            mutex.clear_poison();
            report(&format!("the {name} was poisoned; recovering its retained state"));
            poisoned.into_inner()
        }
    }
}

fn lock_state<'a, T>(mutex: &'a Mutex<T>, name: &str) -> MutexGuard<'a, T> {
    recover_lock(mutex, mutex.lock(), name)
}

struct InjectedFailures {
    skip: usize,
    remaining: usize,
    target: Option<String>,
    panic: bool,
}

static INJECTED_FAILURES: Mutex<Option<InjectedFailures>> = Mutex::new(None);
static INJECTED_WORKER_PANICS: AtomicUsize = AtomicUsize::new(0);

fn post_to_scope(scope: &AppScope, description: &str, callback: Callback) -> Result<(), String> {
    let injected = {
        let mut injection = lock_state(&INJECTED_FAILURES, "shutdown test state");
        if let Some(injection) = injection.as_mut().filter(|injection| injection.target.as_deref().is_none_or(|target| target == description)) {
            if injection.skip != 0 {
                injection.skip -= 1;
                None
            } else if injection.remaining != 0 {
                injection.remaining -= 1;
                Some(injection.panic)
            } else {
                None
            }
        } else {
            None
        }
    };
    match injected {
        Some(true) => panic!("injected UI dispatcher panic"),
        Some(false) => return Err("injected UI dispatcher failure".into()),
        None => {}
    }
    scope.post(callback).map_err(|error| error.to_string())
}

fn take_once(pending: PendingCallback) -> Callback {
    Box::new(move || {
        let callback = lock_state(&pending, "shutdown callback state").take();
        if let Some(callback) = callback {
            callback();
        }
    })
}

fn post_with_retry(
    description: &'static str,
    pending: &PendingCallback,
    mut post: impl FnMut(Callback) -> Result<(), String>,
    mut pause: impl FnMut(),
    mut stopping: impl FnMut() -> bool,
) -> Result<(), String> {
    let mut failures = 0usize;
    loop {
        if stopping() {
            return Err("the shutdown dispatcher stopped before delivering its callback".into());
        }
        if lock_state(pending, "shutdown callback state").is_none() {
            return Ok(());
        }
        let result = post(take_once(pending.clone()));
        match result {
            Ok(()) => {
                if failures != 0 {
                    report(&format!("queued {description} after retrying the UI dispatcher"));
                }
                return Ok(());
            }
            Err(error) => {
                failures = failures.saturating_add(1);
                if failures == 1 || failures % 300 == 0 {
                    report(&format!("could not post {description}; retrying: {error}"));
                }
                if lock_state(pending, "shutdown callback state").is_none() {
                    return Ok(());
                }
                pause();
            }
        }
    }
}

fn deliver_with_retry(
    description: &'static str,
    pending: PendingCallback,
    mut post: impl FnMut(Callback) -> Result<(), String>,
    mut pause: impl FnMut(),
    mut stopping: impl FnMut() -> bool,
) -> Result<(), String> {
    let mut panics = 0usize;
    loop {
        // Keep the original callback outside the unwind boundary: a failed
        // post may already have queued a wrapper around it.
        let result = catch_unwind(AssertUnwindSafe(|| {
            if panics != 0 {
                pause();
            }
            post_with_retry(description, &pending, &mut post, &mut pause, &mut stopping)
        }));
        match result {
            Ok(result) => return result,
            Err(_) => {
                panics = panics.saturating_add(1);
                if lock_state(&pending, "shutdown callback state").is_none() {
                    return Err(format!("posting {description} panicked after its callback began; it will not be run twice"));
                }
                if stopping() {
                    return Err("the shutdown dispatcher stopped before delivering its callback".into());
                }
                if panics == 1 || panics % 300 == 0 {
                    report(&format!("the shutdown dispatcher panicked while delivering {description}; retrying"));
                }
            }
        }
    }
}

fn supervise(queue: &Queue, mut run: impl FnMut()) {
    let mut panics = 0usize;
    loop {
        match catch_unwind(AssertUnwindSafe(&mut run)) {
            Ok(()) => return,
            Err(_) => {
                panics = panics.saturating_add(1);
                if panics == 1 || panics % 300 == 0 {
                    report("the shutdown dispatcher worker panicked; restarting with its retained queue");
                }
                if queue.stopping.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn dispatch_work(
    queue: &Queue,
    work: &Arc<Work>,
    post: impl FnOnce(Callback) -> Result<(), String>,
    pause: impl FnOnce(),
) {
    if queue.stopping.load(Ordering::Acquire) {
        return;
    }
    if lock_state(&work.callback, "shutdown callback state").is_none() {
        queue.finish(work);
        return;
    }
    let result = catch_unwind(AssertUnwindSafe(|| post(take_once(work.callback.clone()))));
    let callback_started = || lock_state(&work.callback, "shutdown callback state").is_none();
    let failure = match result {
        Ok(Ok(())) => None,
        Ok(Err(_)) if callback_started() => None,
        Ok(Err(error)) => Some(format!("could not post {}; retrying: {error}", work.description)),
        Err(_) if callback_started() => {
            report(&format!("posting {} panicked after its callback began; it will not be run twice", work.description));
            None
        }
        Err(_) => Some(format!("the shutdown dispatcher panicked while delivering {}; retrying", work.description)),
    };
    if let Some(message) = failure {
        let previous = work.failures.fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| Some(value.saturating_add(1)))
            .expect("the failure count update always succeeds");
        let failures = previous.saturating_add(1);
        if failures == 1 || failures % 300 == 0 {
            report(&message);
        }
        queue.defer(work);
        pause();
    } else {
        if work.failures.load(Ordering::Acquire) != 0 {
            report(&format!("queued {} after retrying the UI dispatcher", work.description));
        }
        queue.finish(work);
    }
}

fn dispatch_queued(scope: &AppScope, queue: &Queue) {
    while let Some(work) = queue.next() {
        if INJECTED_WORKER_PANICS.fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| remaining.checked_sub(1)).is_ok() {
            panic!("injected shutdown worker panic");
        }
        if work.delayed {
            std::thread::sleep(Duration::from_millis(25));
        }
        dispatch_work(
            queue,
            &work,
            |callback| post_to_scope(scope, work.description, callback),
            || std::thread::sleep(Duration::from_millis(100)),
        );
    }
}

pub(crate) fn install(scope: &AppScope) -> io::Result<()> {
    let mut dispatcher = lock_state(&DISPATCHER, "shutdown dispatcher state");
    if dispatcher.is_some() {
        return Err(io::Error::other("the shutdown dispatcher is already installed"));
    }
    let injected_failures = if std::env::var_os("JIFFSHOT_UITEST").is_some() {
        match std::env::var("JIFFSHOT_UITEST_SHUTDOWN_POST_FAILURES") {
            Ok(value) => value.parse::<usize>().map_err(|_| io::Error::other("invalid shutdown post failure count"))?,
            Err(std::env::VarError::NotPresent) => 0,
            Err(error) => return Err(io::Error::other(error)),
        }
    } else {
        0
    };
    let injected_worker_panics = if std::env::var_os("JIFFSHOT_UITEST").is_some() {
        match std::env::var("JIFFSHOT_UITEST_SHUTDOWN_WORKER_PANICS") {
            Ok(value) => value.parse::<usize>().map_err(|_| io::Error::other("invalid shutdown worker panic count"))?,
            Err(std::env::VarError::NotPresent) => 0,
            Err(error) => return Err(io::Error::other(error)),
        }
    } else {
        0
    };
    let injected_skip = if injected_failures != 0 {
        match std::env::var("JIFFSHOT_UITEST_SHUTDOWN_POST_SKIP") {
            Ok(value) => value.parse::<usize>().map_err(|_| io::Error::other("invalid shutdown post skip count"))?,
            Err(std::env::VarError::NotPresent) => 0,
            Err(error) => return Err(io::Error::other(error)),
        }
    } else {
        0
    };
    let injected_target = if injected_failures != 0 {
        match std::env::var("JIFFSHOT_UITEST_SHUTDOWN_POST_TARGET") {
            Ok(value) if !value.is_empty() => Some(value),
            Ok(_) => return Err(io::Error::other("the shutdown post target is empty")),
            Err(std::env::VarError::NotPresent) => None,
            Err(error) => return Err(io::Error::other(error)),
        }
    } else {
        None
    };
    let injected_panic = injected_failures != 0 && std::env::var_os("JIFFSHOT_UITEST_SHUTDOWN_POST_PANIC").is_some();
    let queue = Arc::new(Queue::default());
    let worker_queue = queue.clone();
    let scope = scope.clone();
    let worker = std::thread::Builder::new().name("jiffshot-shutdown-ui".into()).spawn(move || {
        supervise(&worker_queue, || dispatch_queued(&scope, &worker_queue));
    })?;
    *lock_state(&INJECTED_FAILURES, "shutdown test state") =
        Some(InjectedFailures { skip: injected_skip, remaining: injected_failures, target: injected_target, panic: injected_panic });
    INJECTED_WORKER_PANICS.store(injected_worker_panics, Ordering::Release);
    *dispatcher = Some(Dispatcher { queue, worker });
    Ok(())
}

fn enqueue(description: &'static str, callback: impl FnOnce() + Send + 'static, delayed: bool) -> Result<(), String> {
    let dispatcher = lock_state(&DISPATCHER, "shutdown dispatcher state");
    let dispatcher = dispatcher.as_ref().ok_or("the shutdown dispatcher is unavailable")?;
    dispatcher.queue.push(Work {
        callback: Arc::new(Mutex::new(Some(Box::new(callback)))),
        delayed,
        description,
        failures: AtomicUsize::new(0),
    })
}

pub(crate) fn post(description: &'static str, callback: impl FnOnce() + Send + 'static) -> Result<(), String> {
    enqueue(description, callback, false)
}

pub(crate) fn retry(callback: impl FnOnce() + Send + 'static) -> Result<(), String> {
    retry_named("a shutdown state retry", callback)
}

pub(crate) fn retry_named(description: &'static str, callback: impl FnOnce() + Send + 'static) -> Result<(), String> {
    enqueue(description, callback, true)
}

pub(crate) fn post_from_worker(scope: &AppScope, callback: impl FnOnce() + Send + 'static) {
    if let Err(error) = deliver_with_retry(
        "the shutdown result",
        Arc::new(Mutex::new(Some(Box::new(callback)))),
        |callback| post_to_scope(scope, "the shutdown result", callback),
        || std::thread::sleep(Duration::from_millis(100)),
        || false,
    ) {
        report(&format!("could not deliver the shutdown result: {error}"));
    }
}

pub(crate) fn stop() -> io::Result<()> {
    let dispatcher = lock_state(&DISPATCHER, "shutdown dispatcher state").take();
    let result = if let Some(dispatcher) = dispatcher {
        dispatcher.queue.stop();
        dispatcher.worker.join().map_err(|_| io::Error::other("the shutdown dispatcher panicked"))
    } else {
        Ok(())
    };
    *lock_state(&INJECTED_FAILURES, "shutdown test state") = None;
    INJECTED_WORKER_PANICS.store(0, Ordering::Release);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_posts_are_retried_without_duplicate_delivery() {
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let observed = delivered.clone();
        let mut attempts = 0;
        let mut pauses = 0;
        let mut stale = None;
        deliver_with_retry(
            "a test callback",
            Arc::new(Mutex::new(Some(Box::new(move || observed.lock().unwrap().push(Err::<(), _>("capture failed".to_owned())))))),
            |callback| {
                attempts += 1;
                match attempts {
                    1 => { stale = Some(callback); Err("dispatcher unavailable".into()) }
                    2 => Err("dispatcher unavailable".into()),
                    _ => { callback(); Ok(()) }
                }
            },
            || pauses += 1,
            || false,
        ).unwrap();
        stale.expect("retain the first attempted callback")();
        assert_eq!(attempts, 3);
        assert_eq!(pauses, 2);
        assert_eq!(*delivered.lock().unwrap(), vec![Err("capture failed".to_owned())]);
    }

    #[test]
    fn stopping_abandons_an_unaccepted_callback_without_running_it() {
        use std::cell::Cell;

        let delivered = Arc::new(AtomicBool::new(false));
        let observed = delivered.clone();
        let attempts = Cell::new(0);
        let pending: PendingCallback = Arc::new(Mutex::new(Some(Box::new(move || observed.store(true, Ordering::Release)))));
        let error = deliver_with_retry(
            "a test callback",
            pending.clone(),
            |_| { attempts.set(attempts.get() + 1); Err("dispatcher unavailable".into()) },
            || {},
            || attempts.get() > 0,
        ).unwrap_err();
        assert!(error.contains("stopped"));
        assert!(!delivered.load(Ordering::Acquire));
        take_once(pending)();
        assert!(delivered.load(Ordering::Acquire));
    }

    #[test]
    fn a_panicking_post_is_retried_without_duplicate_delivery() {
        let delivered = Arc::new(AtomicBool::new(false));
        let observed = delivered.clone();
        let pending: PendingCallback = Arc::new(Mutex::new(Some(Box::new(move || {
            assert!(!observed.swap(true, Ordering::AcqRel), "callback ran twice");
        }))));
        let mut attempts = 0;
        let mut pauses = 0;
        let mut stale = None;
        deliver_with_retry(
            "a test callback",
            pending,
            |callback| {
                attempts += 1;
                if attempts == 1 {
                    stale = Some(callback);
                    panic!("injected post panic");
                }
                callback();
                Ok(())
            },
            || pauses += 1,
            || false,
        ).unwrap();
        stale.expect("retain the first attempted callback")();
        assert_eq!(attempts, 2);
        assert_eq!(pauses, 1);
        assert!(delivered.load(Ordering::Acquire));
    }

    #[test]
    fn a_callback_that_panicked_is_not_run_again() {
        let pending: PendingCallback = Arc::new(Mutex::new(Some(Box::new(|| panic!("injected callback panic")))));
        let mut attempts = 0;
        let error = deliver_with_retry(
            "a test callback",
            pending,
            |callback| { attempts += 1; callback(); Ok(()) },
            || {},
            || false,
        ).unwrap_err();
        assert!(error.contains("will not be run twice"));
        assert_eq!(attempts, 1);
    }

    #[test]
    fn a_poisoned_queue_and_callback_retain_their_work() {
        let queue = Arc::new(Queue::default());
        let delivered = Arc::new(AtomicBool::new(false));
        let observed = delivered.clone();
        queue.push(Work {
            callback: Arc::new(Mutex::new(Some(Box::new(move || observed.store(true, Ordering::Release))))),
            delayed: false,
            description: "a test callback",
            failures: AtomicUsize::new(0),
        }).unwrap();
        let poisoned_queue = queue.clone();
        assert!(std::thread::spawn(move || {
            let _guard = poisoned_queue.pending.lock().unwrap();
            panic!("inject queue poisoning");
        }).join().is_err());
        let work = queue.next().unwrap();
        assert!(!queue.pending.is_poisoned());
        let poisoned_callback = work.callback.clone();
        assert!(std::thread::spawn(move || {
            let _guard = poisoned_callback.lock().unwrap();
            panic!("inject callback poisoning");
        }).join().is_err());
        deliver_with_retry(work.description, work.callback.clone(), |callback| { callback(); Ok(()) }, || {}, || false).unwrap();
        assert!(!work.callback.is_poisoned());
        queue.finish(&work);
        queue.stop();
        assert!(queue.next().is_none());
        assert!(delivered.load(Ordering::Acquire));
    }

    #[test]
    fn a_worker_panic_retains_queued_work_without_duplicate_delivery() {
        let queue = Queue::default();
        let delivered = Arc::new(AtomicBool::new(false));
        let observed = delivered.clone();
        queue.push(Work {
            callback: Arc::new(Mutex::new(Some(Box::new(move || {
                assert!(!observed.swap(true, Ordering::AcqRel), "callback ran twice");
            })))),
            delayed: false,
            description: "a test callback",
            failures: AtomicUsize::new(0),
        }).unwrap();
        let mut attempts = 0;
        let mut stale = None;
        supervise(&queue, || {
            let work = queue.next().expect("work survives the worker panic");
            attempts += 1;
            if attempts == 1 {
                stale = Some(take_once(work.callback.clone()));
                panic!("inject worker panic after posting");
            }
            deliver_with_retry(work.description, work.callback.clone(), |callback| { callback(); Ok(()) }, || {}, || false).unwrap();
            queue.finish(&work);
            queue.stop();
        });
        stale.expect("retain the first attempted callback")();
        assert_eq!(attempts, 2);
        assert!(delivered.load(Ordering::Acquire));
    }

    #[test]
    fn a_failed_post_does_not_starve_later_shutdown_work() {
        let queue = Queue::default();
        let delivered = Arc::new(Mutex::new(Vec::new()));
        for description in ["first", "second"] {
            let observed = delivered.clone();
            queue.push(Work {
                callback: Arc::new(Mutex::new(Some(Box::new(move || observed.lock().unwrap().push(description))))),
                delayed: false,
                description,
                failures: AtomicUsize::new(0),
            }).unwrap();
        }
        let first = queue.next().unwrap();
        let mut stale = None;
        dispatch_work(&queue, &first, |callback| {
            stale = Some(callback);
            Err("unavailable".into())
        }, || {});
        let second = queue.next().unwrap();
        assert_eq!(second.description, "second");
        dispatch_work(&queue, &second, |callback| { callback(); Ok(()) }, || {});
        assert!(Arc::ptr_eq(&queue.next().unwrap(), &first));
        dispatch_work(&queue, &first, |callback| { callback(); Ok(()) }, || {});
        stale.expect("retain the first attempted callback")();
        queue.stop();
        assert_eq!(*delivered.lock().unwrap(), vec!["second", "first"]);
    }
}
