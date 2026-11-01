pub(crate) mod app;
pub(crate) mod capture_window;
pub(crate) mod drag_out;
pub(crate) mod library;
mod main;
pub(crate) mod ocr;
pub(crate) mod prefs;
pub(crate) mod settings;
pub(crate) mod share;
pub(crate) mod tray;
pub(crate) mod video;
pub(crate) mod vm;
pub(crate) mod vm_support;
pub(crate) mod winutil;

pub(super) use main::run;
