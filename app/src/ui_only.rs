use std::mem::ManuallyDrop;
use std::thread::ThreadId;

/// Carries a UI-thread value through Rustolonia's `Send` event callbacks.
pub struct UiOnly<T> {
    value: ManuallyDrop<T>,
    thread: ThreadId,
}

// SAFETY: the value is only reached, and only dropped, on the thread that made it.
unsafe impl<T> Send for UiOnly<T> {}
unsafe impl<T> Sync for UiOnly<T> {}

impl<T> UiOnly<T> {
    pub fn new(value: T) -> Self {
        Self {
            value: ManuallyDrop::new(value),
            thread: std::thread::current().id(),
        }
    }

    pub fn get(&self) -> &T {
        assert_eq!(
            self.thread,
            std::thread::current().id(),
            "UI value used off the UI thread"
        );
        &self.value
    }
}

impl<T: Clone> Clone for UiOnly<T> {
    fn clone(&self) -> Self {
        Self::new(self.get().clone())
    }
}

impl<T> Drop for UiOnly<T> {
    fn drop(&mut self) {
        if self.thread == std::thread::current().id() {
            // SAFETY: dropped once, here.
            unsafe { ManuallyDrop::drop(&mut self.value) }
        }
    }
}
