//! StatusNotifier tray integration with a visible-window fallback.

use ksni::TrayMethods;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

const STARTING: u8 = 0;
const READY: u8 = 1;
const ACTIVE: u8 = 2;
const LOST: u8 = 3;
const STOPPED: u8 = 4;
const HOST_CHECK_INTERVAL: Duration = Duration::from_secs(2);
const HOST_CHECK_TIMEOUT: Duration = Duration::from_secs(3);
const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";

#[derive(Debug)]
pub(super) enum Event {
    Ready,
    Open,
    Capture,
    Quit,
    Unavailable(String),
}

enum Message {
    Open,
    Capture,
    Quit,
    Lost(String),
    Stop,
}

pub(super) struct Session {
    state: Arc<AtomicU8>,
    sender: mpsc::Sender<Message>,
}

impl Session {
    pub(super) fn start(on_event: impl Fn(Event) + Send + 'static) -> Result<Self, String> {
        let icon = tray_icon()?;
        let state = Arc::new(AtomicU8::new(STARTING));
        let (sender, receiver) = mpsc::channel();
        let item = Item {
            icon,
            state: Arc::clone(&state),
            sender: sender.clone(),
        };
        let worker_state = Arc::clone(&state);
        std::thread::Builder::new()
            .name("jiffshot-tray".to_owned())
            .spawn(move || run(item, worker_state, receiver, on_event))
            .map_err(|error| format!("could not start the tray worker: {error}"))?;
        Ok(Self { state, sender })
    }

    pub(super) fn activate(&self) -> bool {
        self.state
            .compare_exchange(READY, ACTIVE, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(super) fn is_active(&self) -> bool {
        self.state.load(Ordering::Acquire) == ACTIVE
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.state.store(STOPPED, Ordering::Release);
        let _ = self.sender.send(Message::Stop);
    }
}

struct Item {
    icon: Vec<u8>,
    state: Arc<AtomicU8>,
    sender: mpsc::Sender<Message>,
}

impl Item {
    fn send(&self, message: Message) {
        if self.sender.send(message).is_err() && self.state.load(Ordering::Acquire) != STOPPED {
            eprintln!("Could not deliver a tray action: the tray worker has stopped");
        }
    }
}

impl ksni::Tray for Item {
    fn id(&self) -> String {
        "jiffshot".to_owned()
    }

    fn title(&self) -> String {
        "Jiffshot".to_owned()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![ksni::Icon {
            width: 32,
            height: 32,
            data: self.icon.clone(),
        }]
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Message::Open);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;

        vec![
            StandardItem {
                label: "Open Jiffshot".to_owned(),
                activate: Box::new(|item: &mut Self| item.send(Message::Open)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Capture screen or window".to_owned(),
                activate: Box::new(|item: &mut Self| item.send(Message::Capture)),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Quit Jiffshot".to_owned(),
                activate: Box::new(|item: &mut Self| item.send(Message::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        if mark_lost(&self.state) {
            self.send(Message::Lost(format!(
                "the desktop tray watcher went away ({reason:?})"
            )));
        }
        false
    }
}

fn tray_icon() -> Result<Vec<u8>, String> {
    let image = image::load_from_memory_with_format(
        include_bytes!("../../assets/jiffshot-tray.png"),
        image::ImageFormat::Png,
    )
    .map_err(|error| format!("could not load the tray icon: {error}"))?
    .into_rgba8();
    if image.dimensions() != (32, 32) {
        return Err("the tray icon must be 32 by 32 pixels".to_owned());
    }
    let mut pixels = image.into_raw();
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.rotate_right(1);
    }
    Ok(pixels)
}

fn mark_lost(state: &AtomicU8) -> bool {
    let mut current = state.load(Ordering::Acquire);
    loop {
        if !matches!(current, STARTING | READY | ACTIVE) {
            return false;
        }
        match state.compare_exchange_weak(current, LOST, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

struct WorkerGuard<'a, F: Fn(Event)> {
    state: &'a AtomicU8,
    on_event: &'a F,
}

impl<F: Fn(Event)> Drop for WorkerGuard<'_, F> {
    fn drop(&mut self) {
        if mark_lost(self.state) {
            (self.on_event)(Event::Unavailable(
                "the system tray worker stopped unexpectedly".to_owned(),
            ));
        }
    }
}

fn run(
    item: Item,
    state: Arc<AtomicU8>,
    receiver: mpsc::Receiver<Message>,
    on_event: impl Fn(Event),
) {
    let _guard = WorkerGuard {
        state: &state,
        on_event: &on_event,
    };
    let handle = match futures_lite::future::block_on(item.spawn()) {
        Ok(handle) => handle,
        Err(error) => {
            if mark_lost(&state) || state.load(Ordering::Acquire) == LOST {
                on_event(Event::Unavailable(error.to_string()));
            }
            return;
        }
    };
    let connection = match futures_lite::future::block_on(zbus::Connection::session()) {
        Ok(connection) => connection,
        Err(error) => {
            if mark_lost(&state) || state.load(Ordering::Acquire) == LOST {
                on_event(Event::Unavailable(format!(
                    "could not monitor the desktop tray: {error}"
                )));
            }
            futures_lite::future::block_on(handle.shutdown());
            return;
        }
    };
    let watcher = match futures_lite::future::block_on(async {
        zbus::proxy::Builder::<zbus::Proxy<'_>>::new(&connection)
            .destination(WATCHER_NAME)?
            .path(WATCHER_PATH)?
            .interface(WATCHER_NAME)?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
    }) {
        Ok(watcher) => watcher,
        Err(error) => {
            if mark_lost(&state) || state.load(Ordering::Acquire) == LOST {
                on_event(Event::Unavailable(format!(
                    "could not monitor the desktop tray: {error}"
                )));
            }
            futures_lite::future::block_on(handle.shutdown());
            return;
        }
    };
    let host_available = || {
        futures_lite::future::block_on(futures_lite::future::or(
            async {
                watcher
                    .get_property::<bool>("IsStatusNotifierHostRegistered")
                    .await
                    .map_err(|error| format!("could not check the desktop tray host: {error}"))
                    .and_then(|available| {
                        available
                            .then_some(())
                            .ok_or_else(|| "the desktop tray host is unavailable".to_owned())
                    })
            },
            async {
                async_io::Timer::after(HOST_CHECK_TIMEOUT).await;
                Err("the desktop tray host did not respond".to_owned())
            },
        ))
    };
    if let Err(error) = host_available() {
        if mark_lost(&state) || state.load(Ordering::Acquire) == LOST {
            on_event(Event::Unavailable(error));
        }
    } else if state
        .compare_exchange(STARTING, READY, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        on_event(Event::Ready);
        loop {
            match receiver.recv_timeout(HOST_CHECK_INTERVAL) {
                Ok(Message::Open) => on_event(Event::Open),
                Ok(Message::Capture) => on_event(Event::Capture),
                Ok(Message::Quit) => on_event(Event::Quit),
                Ok(Message::Lost(reason)) => {
                    if state.load(Ordering::Acquire) != STOPPED {
                        on_event(Event::Unavailable(reason));
                    } else if state.load(Ordering::Acquire) == LOST {
                        on_event(Event::Unavailable(
                            "the desktop tray became unavailable while starting".to_owned(),
                        ));
                    }
                    break;
                }
                Ok(Message::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let result = if handle.is_closed() {
                        Err("the desktop tray service stopped".to_owned())
                    } else {
                        host_available()
                    };
                    if let Err(error) = result {
                        if mark_lost(&state) || state.load(Ordering::Acquire) == LOST {
                            on_event(Event::Unavailable(error));
                        }
                        break;
                    }
                }
            }
        }
    }
    futures_lite::future::block_on(handle.shutdown());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_is_not_active_until_host_ready_and_acknowledged() {
        let (sender, _receiver) = mpsc::channel();
        let session = Session {
            state: Arc::new(AtomicU8::new(STARTING)),
            sender,
        };
        assert!(!session.is_active());
        assert!(!session.activate());
        session.state.store(READY, Ordering::Release);
        assert!(!session.is_active());
        assert!(session.activate());
        assert!(session.is_active());
        assert!(!session.activate());
    }

    #[test]
    fn losing_the_host_disables_close_to_tray_even_before_ui_dispatch() {
        let (sender, _receiver) = mpsc::channel();
        let session = Session {
            state: Arc::new(AtomicU8::new(ACTIVE)),
            sender,
        };
        assert!(mark_lost(&session.state));
        assert!(!session.is_active());
        assert!(!session.activate());
        assert!(!mark_lost(&session.state));
    }

    #[test]
    fn unexpected_worker_exit_reports_failure_and_disables_the_tray() {
        let state = AtomicU8::new(ACTIVE);
        let (sender, receiver) = mpsc::channel();
        {
            let callback = |event| sender.send(event).expect("the receiver should remain open");
            let _guard = WorkerGuard {
                state: &state,
                on_event: &callback,
            };
        }
        assert_eq!(state.load(Ordering::Acquire), LOST);
        assert!(matches!(
            receiver.try_recv().expect("the worker failure should be reported"),
            Event::Unavailable(message) if message.contains("stopped unexpectedly")
        ));
    }

    #[test]
    fn tray_icon_matches_the_embedded_argb_pixels() {
        let pixels = tray_icon().expect("the embedded icon should decode");
        let source = image::load_from_memory_with_format(
            include_bytes!("../../assets/jiffshot-tray.png"),
            image::ImageFormat::Png,
        )
        .expect("the original icon should decode")
        .into_rgba8();
        assert_eq!(pixels.len(), 32 * 32 * 4);
        for (argb, rgba) in pixels
            .as_chunks::<4>()
            .0
            .iter()
            .zip(source.as_raw().as_chunks::<4>().0)
        {
            assert_eq!(*argb, [rgba[3], rgba[0], rgba[1], rgba[2]]);
        }
        assert!(pixels.as_chunks::<4>().0.iter().any(|pixel| pixel[0] != 0));
    }
}
