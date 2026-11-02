use super::{narrate_finish, narration_picked, AudioBlock, AudioSpec, NarrationSource, NEXT_NARRATION};
use crate::app;
use jiffshot_doc::Pixmap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct FakeSource {
    samples: Receiver<Result<Vec<i16>, String>>,
    dropped: Sender<()>,
}

impl NarrationSource for FakeSource {
    fn spec(&self) -> AudioSpec {
        AudioSpec { sample_rate: 48_000, channels: 2 }
    }

    fn next_block(&self) -> Result<Option<AudioBlock>, String> {
        match self.samples.try_recv() {
            Ok(result) => result.map(|samples| Some(AudioBlock { samples, captured_at: Some(Instant::now()) })),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err("synthetic microphone stream closed".into()),
        }
    }
}

impl Drop for FakeSource {
    fn drop(&mut self) {
        let _ = self.dropped.send(());
    }
}

fn source() -> (FakeSource, Sender<Result<Vec<i16>, String>>, Receiver<()>) {
    let (samples, rx) = mpsc::channel();
    let (dropped, drop_rx) = mpsc::channel();
    (FakeSource { samples: rx, dropped }, samples, drop_rx)
}

pub(crate) struct Harness {
    image: PathBuf,
    stale_opened: Arc<AtomicBool>,
    old_id: u64,
    old_opened: Receiver<()>,
    old_release: Option<Sender<()>>,
    old_dropped: Receiver<()>,
    _old_samples: Sender<Result<Vec<i16>, String>>,
    new_id: u64,
    new_source: Option<FakeSource>,
    new_samples: Sender<Result<Vec<i16>, String>>,
    new_dropped: Receiver<()>,
}

impl Harness {
    pub(crate) fn start(dir: &Path) -> Result<Self, String> {
        let image = dir.join("narration-fixture.png");
        let mut picture = Pixmap::new(32, 24).ok_or("could not create fixture image")?;
        picture.fill(tiny_skia::Color::from_rgba8(30, 90, 160, 255));
        picture.save_png(&image).map_err(|error| error.to_string())?;
        let (stale_source, _stale_samples, _) = source();
        let stale_opened = Arc::new(AtomicBool::new(false));
        let stale_flag = stale_opened.clone();
        let (old_source, old_samples, old_dropped) = source();
        let (opened, old_opened) = mpsc::channel();
        let (old_release, release) = mpsc::channel();
        let (new_source, new_samples, new_dropped) = source();
        let stale_id = NEXT_NARRATION.fetch_add(1, Ordering::Relaxed);
        let old_id = NEXT_NARRATION.fetch_add(1, Ordering::Relaxed);
        let new_id = NEXT_NARRATION.fetch_add(1, Ordering::Relaxed);
        let old_image = image.clone();

        app::with(|app| -> Result<(), String> {
            app.narration_request = Some(stale_id);
            app.hidden();
            narration_picked(app, stale_id, Ok(Some(vec![image.clone()])), move |_| {
                stale_flag.store(true, Ordering::Release);
                Ok(stale_source)
            });
            if app.narration.is_some() || app.narration_request.is_some() {
                return Err("a stale file picker restarted narration after the editor closed".into());
            }
            app.narration_request = Some(old_id);
            narration_picked(app, old_id, Ok(Some(vec![old_image])), move |_| {
                opened.send(()).map_err(|error| error.to_string())?;
                release.recv_timeout(Duration::from_secs(5)).map_err(|error| error.to_string())?;
                Ok(old_source)
            });
            Ok(())
        })
        .ok_or("the editor is unavailable")??;
        app::schedule_sync();
        Ok(Self {
            image,
            stale_opened,
            old_id,
            old_opened,
            old_release: Some(old_release),
            old_dropped,
            _old_samples: old_samples,
            new_id,
            new_source: Some(new_source),
            new_samples,
            new_dropped,
        })
    }

    pub(crate) fn cancel_and_replace(&mut self) -> Result<(), String> {
        self.old_opened.try_recv().map_err(|_| "the pending source did not begin opening")?;
        let source = self.new_source.take().ok_or("the replacement source was already used")?;
        let image = self.image.clone();
        let new_id = self.new_id;
        let old_id = self.old_id;
        app::with(|app| -> Result<(), String> {
            let pending = app.narration.as_ref().is_some_and(|n| n.id == old_id && n.started.is_none());
            if !pending {
                return Err("the old narration was not pending".into());
            }
            app.hidden();
            if app.narration.is_some() {
                return Err("closing the editor did not cancel narration".into());
            }
            app.narration_request = Some(new_id);
            narration_picked(app, new_id, Ok(Some(vec![image])), move |_| Ok(source));
            Ok(())
        })
        .ok_or("the editor is unavailable")??;
        self.old_release.take().ok_or("the old source was already released")?.send(()).map_err(|error| error.to_string())?;
        app::schedule_sync();
        Ok(())
    }

    pub(crate) fn interrupt(&mut self) -> Result<(), String> {
        self.old_dropped.try_recv().map_err(|_| "the cancelled source was not released")?;
        if self.stale_opened.load(Ordering::Acquire) {
            return Err("a stale file picker opened its microphone".into());
        }
        let running = app::with(|app| app.narration.as_ref().is_some_and(|n| n.id == self.new_id && n.started.is_some()))
            .ok_or("the editor is unavailable")?;
        if !running {
            return Err("the replacement narration did not start".into());
        }
        self.new_samples.send(Err("synthetic microphone interruption".into())).map_err(|error| error.to_string())
    }

    pub(crate) fn check_stopped(&mut self) -> Result<(), String> {
        self.new_dropped.try_recv().map_err(|_| "the interrupted source was not released")?;
        if app::with(|app| app.narration.is_some()).ok_or("the editor is unavailable")? {
            return Err("the interrupted narration is still active".into());
        }
        Ok(())
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        app::with(|app| app.cancel_narration());
        self.old_release.take();
    }
}

pub(crate) struct FinishHarness {
    image: PathBuf,
    old_dropped: Receiver<()>,
    _old_samples: Sender<Result<Vec<i16>, String>>,
    new_id: u64,
    new_source: Option<FakeSource>,
    _new_samples: Sender<Result<Vec<i16>, String>>,
    new_dropped: Receiver<()>,
}

impl FinishHarness {
    pub(crate) fn start(dir: &Path) -> Result<Self, String> {
        let image = dir.join("narration-finish-fixture.png");
        let mut picture = Pixmap::new(32, 24).ok_or("could not create fixture image")?;
        picture.fill(tiny_skia::Color::from_rgba8(30, 90, 160, 255));
        picture.save_png(&image).map_err(|error| error.to_string())?;
        let (old_source, old_samples, old_dropped) = source();
        let (new_source, new_samples, new_dropped) = source();
        let old_id = NEXT_NARRATION.fetch_add(1, Ordering::Relaxed);
        let new_id = NEXT_NARRATION.fetch_add(1, Ordering::Relaxed);
        app::with(|app| {
            app.narration_request = Some(old_id);
            narration_picked(app, old_id, Ok(Some(vec![image.clone()])), move |_| Ok(old_source));
        }).ok_or("the editor is unavailable")?;
        app::schedule_sync();
        Ok(Self {
            image,
            old_dropped,
            _old_samples: old_samples,
            new_id,
            new_source: Some(new_source),
            _new_samples: new_samples,
            new_dropped,
        })
    }

    pub(crate) fn finish_and_replace(&mut self) -> Result<(), String> {
        let source = self.new_source.take().ok_or("the replacement source was already used")?;
        let image = self.image.clone();
        let new_id = self.new_id;
        app::with(|app| -> Result<(), String> {
            if !app.narration.as_ref().is_some_and(|n| n.started.is_some()) {
                return Err("the finishing narration did not start".into());
            }
            let before = Instant::now();
            narrate_finish(app);
            if before.elapsed() >= Duration::from_millis(250) {
                return Err("finishing narration blocked the UI".into());
            }
            if !app.narration.as_ref().is_some_and(|n| n.finished_at.is_some()) {
                return Err("the narration did not enter its finishing state".into());
            }
            app.cancel_narration();
            app.narration_request = Some(new_id);
            narration_picked(app, new_id, Ok(Some(vec![image])), move |_| Ok(source));
            Ok(())
        }).ok_or("the editor is unavailable")??;
        app::schedule_sync();
        Ok(())
    }

    pub(crate) fn finish_replacement(&mut self) -> Result<(), String> {
        self.old_dropped.try_recv().map_err(|_| "the cancelled finishing source was not released")?;
        app::with(|app| -> Result<(), String> {
            if !app.narration.as_ref().is_some_and(|n| n.id == self.new_id && n.started.is_some() && n.finished_at.is_none()) {
                return Err("a stale completion affected the replacement narration".into());
            }
            narrate_finish(app);
            if !app.narration.as_ref().is_some_and(|n| n.id == self.new_id && n.finished_at.is_some()) {
                return Err("the replacement narration did not enter its finishing state".into());
            }
            Ok(())
        }).ok_or("the editor is unavailable")??;
        app::schedule_sync();
        Ok(())
    }

    pub(crate) fn check_finished(&self) -> Result<(), String> {
        self.new_dropped.try_recv().map_err(|_| "the finished source was not released")?;
        if app::with(|app| app.narration.is_some()).ok_or("the editor is unavailable")? {
            return Err("the completed narration is still active".into());
        }
        Ok(())
    }
}

impl Drop for FinishHarness {
    fn drop(&mut self) {
        app::with(|app| app.cancel_narration());
    }
}
