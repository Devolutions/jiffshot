//! The Linux clipboard (Wayland data-control and X11) through `arboard`.

use crate::Frame;
use arboard::{Clipboard, ImageData};
use std::borrow::Cow;
use std::io;
use std::path::PathBuf;

/// Something on the clipboard the editor can paste.
pub enum Pasted {
    /// Straight-alpha RGBA, top-down.
    Image { width: u32, height: u32, rgba: Vec<u8> },
    Files(Vec<PathBuf>),
    Text(String),
}

fn clipboard() -> io::Result<Clipboard> {
    Clipboard::new().map_err(|error| io::Error::other(format!("clipboard unavailable: {error}")))
}

/// The clipboard has no change counter here.
pub fn change_count() -> isize {
    0
}

/// Replaces the clipboard contents with `frame`.
pub fn copy(frame: &Frame, _owner: isize) -> io::Result<()> {
    frame.validate()?;
    let mut rgba = Vec::with_capacity(frame.pixels.len());
    for pixel in frame.pixels.chunks_exact(4) {
        let alpha = if frame.alpha { pixel[3] } else { 255 };
        rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], alpha]);
    }
    clipboard()?
        .set_image(ImageData {
            width: frame.width as usize,
            height: frame.height as usize,
            bytes: Cow::Owned(rgba),
        })
        .map_err(|error| io::Error::other(format!("could not copy the image: {error}")))
}

/// Replaces the clipboard contents with text.
pub fn copy_text(text: &str, _owner: isize) -> io::Result<()> {
    clipboard()?
        .set_text(text.to_owned())
        .map_err(|error| io::Error::other(format!("could not copy the text: {error}")))
}

/// Replaces the clipboard contents with a list of file paths.
pub fn copy_files(files: &[PathBuf], _owner: isize) -> io::Result<()> {
    let text = files
        .iter()
        .map(|file| file.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n");
    copy_text(&text, 0)
}

/// What is on the clipboard: an image, files, or text.
pub fn read(_owner: isize) -> io::Result<Option<Pasted>> {
    let mut clipboard = clipboard()?;
    if let Ok(files) = clipboard.get().file_list() {
        if !files.is_empty() {
            return Ok(Some(Pasted::Files(files)));
        }
    }
    if let Ok(image) = clipboard.get_image() {
        return Ok(Some(Pasted::Image {
            width: image.width as u32,
            height: image.height as u32,
            rgba: image.bytes.into_owned(),
        }));
    }
    match clipboard.get_text() {
        Ok(text) if !text.is_empty() => Ok(Some(Pasted::Text(text))),
        _ => Ok(None),
    }
}
