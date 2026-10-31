//! Image clipboard access with Wayland data-control and X11 support.

use arboard::{Clipboard, ImageData};
use jiffshot_doc::{Document, Pixmap};
use std::borrow::Cow;

static CLIPBOARD: parking_lot::Mutex<Option<Clipboard>> = parking_lot::Mutex::new(None);

pub fn copy_pixmap(pixmap: Pixmap) -> Result<(), String> {
    let image = image_data_from_pixmap(&pixmap);
    let mut clipboard = clipboard()?;
    let clipboard = clipboard
        .as_mut()
        .ok_or_else(|| "could not initialize the system clipboard".to_owned())?;
    clipboard
        .set_image(image)
        .map_err(|error| format!("could not copy image to the clipboard: {error}"))
}

pub fn copy_text(text: String) -> Result<(), String> {
    let mut clipboard = clipboard()?;
    let clipboard = clipboard
        .as_mut()
        .ok_or_else(|| "could not initialize the system clipboard".to_owned())?;
    clipboard
        .set_text(text)
        .map_err(|error| format!("could not copy text to the clipboard: {error}"))
}

pub fn paste_document() -> Result<Document, String> {
    let mut clipboard = clipboard()?;
    let image = clipboard
        .as_mut()
        .ok_or_else(|| "could not initialize the system clipboard".to_owned())?
        .get_image()
        .map_err(|error| format!("could not read an image from the clipboard: {error}"))?;
    document_from_image(image)
}

fn clipboard() -> Result<parking_lot::MutexGuard<'static, Option<Clipboard>>, String> {
    let mut clipboard = CLIPBOARD.lock();
    if clipboard.is_none() {
        *clipboard = Some(
            Clipboard::new()
                .map_err(|error| format!("could not connect to the system clipboard: {error}"))?,
        );
    }
    Ok(clipboard)
}

fn document_from_image(image: ImageData<'_>) -> Result<Document, String> {
    let width = u32::try_from(image.width)
        .map_err(|error| format!("clipboard image width is out of range: {error}"))?;
    let height = u32::try_from(image.height)
        .map_err(|error| format!("clipboard image height is out of range: {error}"))?;
    let size = tiny_skia::IntSize::from_wh(width, height)
        .ok_or_else(|| "clipboard image is empty".to_owned())?;
    let expected_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "clipboard image is too large".to_owned())?;
    let mut bytes = image.bytes.into_owned();
    if bytes.len() != expected_len {
        return Err("clipboard image has an invalid pixel buffer".to_owned());
    }
    for pixel in bytes.chunks_mut(4) {
        let premultiplied =
            tiny_skia::ColorU8::from_rgba(pixel[0], pixel[1], pixel[2], pixel[3]).premultiply();
        pixel.copy_from_slice(&[
            premultiplied.red(),
            premultiplied.green(),
            premultiplied.blue(),
            premultiplied.alpha(),
        ]);
    }
    let pixmap = Pixmap::from_vec(bytes, size)
        .ok_or_else(|| "clipboard image has an invalid pixel buffer".to_owned())?;
    Ok(Document::new(pixmap))
}

fn image_data_from_pixmap(pixmap: &Pixmap) -> ImageData<'static> {
    let mut bytes = Vec::with_capacity(pixmap.data().len());
    for pixel in pixmap.pixels() {
        let color = pixel.demultiply();
        bytes.extend_from_slice(&[color.red(), color.green(), color.blue(), color.alpha()]);
    }
    ImageData {
        width: pixmap.width() as usize,
        height: pixmap.height() as usize,
        bytes: Cow::Owned(bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_image_round_trips_as_an_editable_document() {
        let document = Document::blank(3, 5, jiffshot_doc::Rgba::new(200, 100, 50, 128));
        let flat = document.flatten();
        let image = image_data_from_pixmap(&flat);

        let pasted = document_from_image(image).expect("clipboard image should become a document");
        assert_eq!(pasted.size(), (3, 5));
        assert_eq!(
            pasted.flatten().data(),
            flat.data(),
            "clipboard round-trip should preserve every pixel"
        );
    }

    #[test]
    fn rejects_clipboard_images_with_inconsistent_pixel_data() {
        let image = ImageData {
            width: 2,
            height: 2,
            bytes: Cow::Owned(vec![0; 8]),
        };
        assert!(
            document_from_image(image).is_err(),
            "truncated clipboard image data must not be accepted"
        );
    }
}
