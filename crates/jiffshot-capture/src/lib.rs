//! Screen capture primitives for Jiffshot's capture side and editor.
//!
//! Everything here works in physical screen pixels. Windows callers must make
//! the process per-monitor DPI aware before capturing.

pub mod frame;
pub mod geom;
pub mod stitch;
#[cfg(windows)]
mod win;

pub use frame::Frame;
pub use geom::Rect;

#[cfg(windows)]
pub use win::{
    clipboard, config, drag, gdi, ipc, library, media, ocr, paths, screen, share, targets,
};

#[cfg(windows)]
pub use win::os::{from_wide, wide};
