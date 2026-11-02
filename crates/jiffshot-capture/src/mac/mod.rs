//! macOS plumbing shared by the native modules and the other crates.

pub mod ffi;
pub mod sck;
pub(crate) mod picker;

/// Cancels open file panels and prevents queued panels from opening while quitting.
pub fn suspend_file_panels_for_shutdown() -> std::io::Result<()> {
    picker::suspend_for_shutdown()
}

/// Allows file panels again after a cancelled or failed quit.
pub fn resume_file_panels_after_failed_shutdown() -> std::io::Result<()> {
    picker::resume_after_failed_shutdown()
}

#[doc(hidden)]
pub fn active_file_panels_for_selftest() -> std::io::Result<usize> {
    picker::active_panels_for_selftest()
}
