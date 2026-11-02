//! Text recognition in a captured image: the Windows OCR engine
//! (`Windows.Media.Ocr`) on Windows, Vision on macOS. Both recognize in the
//! user's preferred languages and report boxes in image pixels.

use crate::Rect;

#[cfg_attr(windows, path = "win/ocr.rs")]
#[cfg_attr(target_os = "macos", path = "mac/ocr.rs")]
mod imp;

pub use imp::recognize;

#[derive(Clone, Debug, PartialEq)]
pub struct Word {
    pub rect: Rect,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub rect: Rect,
    pub text: String,
    pub words: Vec<Word>,
}
