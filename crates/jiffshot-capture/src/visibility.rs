//! A screenshot-scoped host hook, absent in standalone command-line captures.

use std::io;
use std::sync::OnceLock;
use std::time::Instant;

type Restore = Box<dyn FnOnce() + Send>;
type Hide = Box<dyn Fn() -> io::Result<(Restore, Option<Instant>)> + Send + Sync>;

static HIDE: OnceLock<Hide> = OnceLock::new();

/// Installs the application's synchronous window-hiding hook once.
pub fn install(
    hide: impl Fn() -> io::Result<(Restore, Option<Instant>)> + Send + Sync + 'static,
) -> io::Result<()> {
    HIDE.set(Box::new(hide))
        .map_err(|_| io::Error::other("screenshot visibility is already installed"))
}

/// Restores the windows even when screenshot preparation fails or is cancelled.
#[must_use]
pub struct HiddenWindows {
    restore: Option<Restore>,
    hidden_since: Option<Instant>,
}

impl HiddenWindows {
    /// Earliest safe frame timestamp, after window hiding and compositor settling.
    /// Streaming screenshot backends must discard buffered frames older than this.
    pub fn hidden_since(&self) -> Option<Instant> {
        self.hidden_since
    }
}

impl Drop for HiddenWindows {
    fn drop(&mut self) {
        if let Some(restore) = self.restore.take() {
            restore();
        }
    }
}

/// Waits until the host's windows are hidden before allowing a screenshot.
pub fn hide() -> io::Result<HiddenWindows> {
    HIDE.get()
        .map(|hide| {
            hide().map(|(restore, hidden_since)| HiddenWindows {
                restore: Some(restore),
                hidden_since,
            })
        })
        .unwrap_or_else(|| {
            Ok(HiddenWindows {
                restore: None,
                hidden_since: None,
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[test]
    fn restores_once_on_success_error_and_unwind() {
        let restored = Arc::new(AtomicUsize::new(0));
        for outcome in 0..3 {
            let restored = restored.clone();
            let result = std::panic::catch_unwind(|| -> io::Result<()> {
                let _hidden = HiddenWindows {
                    restore: Some(Box::new(move || {
                        restored.fetch_add(1, Ordering::SeqCst);
                    })),
                    hidden_since: Some(Instant::now()),
                };
                match outcome {
                    0 => Ok(()),
                    1 => Err(io::Error::other("capture failed")),
                    _ => panic!("capture panicked"),
                }
            });
            assert_eq!(result.is_err(), outcome == 2);
        }
        assert_eq!(restored.load(Ordering::SeqCst), 3);
    }
}
