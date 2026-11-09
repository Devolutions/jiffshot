//! Shared video-control commands and recording metadata.

pub mod annotation;
mod args;
pub mod guide;
pub mod recording_marks;
mod video;

use args::{internal, invalid, not_found, Args, Reply};
use serde_json::Value;

/// Dispatches a video command, using `cancelled` to stop an in-progress export.
///
/// # Errors
///
/// Returns a control error when the command, arguments, or media operation fail.
pub fn dispatch(
    command: &str,
    args: Value,
    cancelled: &dyn Fn() -> bool,
) -> Result<Value, jiffshot_control::Error> {
    video::dispatch(command, &Args::new(args)?, cancelled)
}
