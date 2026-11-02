//! Puts a capture on the macOS pasteboard as PNG (lossless everywhere) and
//! TIFF (what older Cocoa apps read), and reads images, files and text back
//! for pasting. `owner` (a window on Windows) is unused here.

use crate::macos::ffi::CGImageRef;
use crate::macos::sck;
use crate::Frame;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::AllocAnyThread;
use objc2_app_kit::{NSImage, NSPasteboard, NSPasteboardItem, NSPasteboardWriting};
use objc2_foundation::{NSArray, NSData, NSString, NSURL};
use std::io;
use std::path::PathBuf;

const PNG: &str = "public.png";
const TIFF: &str = "public.tiff";
const FILE_URL: &str = "public.file-url";
const TEXT: &str = "public.utf8-plain-text";

/// Something on the clipboard the editor can paste.
pub enum Pasted {
    /// Straight-alpha RGBA, top-down.
    Image { width: u32, height: u32, rgba: Vec<u8> },
    Files(Vec<PathBuf>),
    Text(String),
}

fn image_buffer_size(width: u32, height: u32) -> io::Result<usize> {
    if width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid clipboard image dimensions"));
    }
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .filter(|&bytes| bytes <= isize::MAX as usize)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "clipboard image dimensions are too large"))
}

fn image_buffer(size: usize) -> io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(size).map_err(|error| io::Error::other(format!("could not allocate the clipboard image: {error}")))?;
    buffer.resize(size, 0);
    Ok(buffer)
}

fn decode_png(bytes: &[u8]) -> io::Result<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(bytes);
    decoder.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    image_buffer_size(reader.info().width, reader.info().height)?;
    let mut buf = image_buffer(reader.output_buffer_size())?;
    let info = reader.next_frame(&mut buf).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let size = image_buffer_size(info.width, info.height)?;
    let rgba = match info.color_type {
        png::ColorType::Rgba if info.buffer_size() == size => {
            buf.truncate(size);
            buf
        }
        png::ColorType::GrayscaleAlpha if info.buffer_size() == size / 2 => {
            let mut rgba = image_buffer(size)?;
            for (dst, src) in rgba.chunks_exact_mut(4).zip(buf.chunks_exact(2)) {
                dst.copy_from_slice(&[src[0], src[0], src[0], src[1]]);
            }
            rgba
        }
        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported clipboard PNG format")),
    };
    Ok((info.width, info.height, rgba))
}

fn ty(name: &str) -> Retained<NSString> {
    NSString::from_str(name)
}

thread_local! {
    static TEST_BOARD: Retained<NSPasteboard> = NSPasteboard::pasteboardWithUniqueName();
}

fn pasteboard() -> Retained<NSPasteboard> {
    if std::env::var_os("JIFFSHOT_UITEST").is_some()
        && std::env::var_os("JIFFSHOT_UITEST_LIBRARY_COPY").is_some()
    {
        return TEST_BOARD.with(Clone::clone);
    }
    NSPasteboard::generalPasteboard()
}

/// The pasteboard's change count, used to avoid replacing a newer copy.
pub fn change_count() -> isize {
    pasteboard().changeCount()
}

/// Replaces the clipboard contents with `frame`.
pub fn copy(frame: &Frame, _owner: isize) -> io::Result<()> {
    copy_to(frame, &pasteboard())
}

fn copy_to(frame: &Frame, board: &NSPasteboard) -> io::Result<()> {
    let png = frame.encode_png_vec()?;
    let data = NSData::with_bytes(&png);
    let tiff = NSImage::initWithData(NSImage::alloc(), &data)
        .and_then(|image| image.TIFFRepresentation())
        .ok_or_else(|| io::Error::other("could not prepare the clipboard's TIFF image"))?;
    board.clearContents();
    if !board.setData_forType(Some(&data), &ty(PNG)) {
        return Err(io::Error::other("the clipboard refused the image"));
    }
    if !board.setData_forType(Some(&tiff), &ty(TIFF)) {
        return Err(io::Error::other("the clipboard refused the TIFF image; the PNG may have been copied"));
    }
    Ok(())
}

/// Replaces the clipboard contents with text.
pub fn copy_text(text: &str, _owner: isize) -> io::Result<()> {
    copy_text_to(text, &pasteboard())
}

fn copy_text_to(text: &str, board: &NSPasteboard) -> io::Result<()> {
    board.clearContents();
    if board.setString_forType(&NSString::from_str(text), &ty(TEXT)) {
        Ok(())
    } else {
        Err(io::Error::other("the clipboard refused the text"))
    }
}

/// Replaces the clipboard contents with file URLs.
pub fn copy_files(files: &[PathBuf], _owner: isize) -> io::Result<()> {
    copy_files_to(files, &pasteboard())
}

fn copy_files_to(files: &[PathBuf], board: &NSPasteboard) -> io::Result<()> {
    if files.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "no files to copy"));
    }
    let mut items: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = Vec::with_capacity(files.len());
    for path in files {
        if !path.is_absolute() || !std::fs::metadata(path)?.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("the clipboard source is not an absolute file: {}", path.display())));
        }
        let url = crate::macos::picker::file_url(path, false)?;
        let value = url.absoluteString().ok_or_else(|| io::Error::other(format!("could not prepare a file URL for {}", path.display())))?;
        let item = NSPasteboardItem::new();
        if !item.setString_forType(&value, &ty(FILE_URL)) {
            return Err(io::Error::other(format!("could not prepare the clipboard item for {}", path.display())));
        }
        items.push(ProtocolObject::from_retained(item));
    }
    board.clearContents();
    if board.writeObjects(&NSArray::from_retained_slice(&items)) {
        Ok(())
    } else {
        Err(io::Error::other("the clipboard refused the files; its previous contents may have been cleared"))
    }
}

fn image_pixels(image: &NSImage) -> io::Result<(u32, u32, Vec<u8>)> {
    // SAFETY: a null proposed rectangle asks for the best representation.
    let cg = unsafe { image.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None) }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "AppKit could not render the clipboard image"))?;
    let ptr = Retained::as_ptr(&cg) as CGImageRef;
    // SAFETY: plain size queries on a live image.
    let (w, h) = unsafe { (crate::macos::ffi::CGImageGetWidth(ptr), crate::macos::ffi::CGImageGetHeight(ptr)) };
    let width = u32::try_from(w).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid clipboard image width"))?;
    let height = u32::try_from(h).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid clipboard image height"))?;
    image_buffer_size(width, height)?;
    let mut px = sck::image_bgra(ptr, w, h)?;
    for p in px.chunks_exact_mut(4) {
        let a = p[3] as u32;
        let un = |c: u8| if a > 0 { ((c as u32 * 255 + a / 2) / a).min(255) as u8 } else { 0 };
        let (b, g, r) = (un(p[0]), un(p[1]), un(p[2]));
        p.copy_from_slice(&[r, g, b, a as u8]);
    }
    Ok((width, height, px))
}

/// Any image AppKit can read from the pasteboard (TIFF, JPEG, PDF…), as
/// straight RGBA.
fn image_from(board: &NSPasteboard) -> io::Result<Option<(u32, u32, Vec<u8>)>> {
    let available: Vec<String> = board.types().map(|types| types.iter().map(|name| name.to_string()).collect()).unwrap_or_default();
    let supported: Vec<String> = NSImage::imageUnfilteredTypes().iter().map(|name| name.to_string()).collect();
    let mut candidates = Vec::new();
    if available.iter().any(|name| name == TIFF) {
        candidates.push(TIFF.to_owned());
    }
    candidates.extend(available.iter().filter(|name| name.as_str() != PNG && name.as_str() != TIFF && supported.contains(name)).cloned());
    let mut errors = Vec::new();
    for name in candidates {
        let result = board.dataForType(&ty(&name))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "the image data is unavailable"))
            .and_then(|data| NSImage::initWithData(NSImage::alloc(), &data)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "AppKit could not read the image")))
            .and_then(|image| image_pixels(&image));
        match result {
            Ok(image) => return Ok(Some(image)),
            Err(error) => errors.push(format!("{name}: {error}")),
        }
    }
    let fallback = if NSImage::canInitWithPasteboard(board) {
        NSImage::initWithPasteboard(NSImage::alloc(), board)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "AppKit could not read the clipboard image"))
            .and_then(|image| image_pixels(&image))
            .map(Some)
    } else {
        Ok(None)
    };
    match fallback {
        Ok(Some(image)) => Ok(Some(image)),
        Ok(None) if errors.is_empty() => Ok(None),
        Ok(None) => Err(io::Error::new(io::ErrorKind::InvalidData, errors.join("; "))),
        Err(error) if errors.is_empty() => Err(error),
        Err(error) => Err(io::Error::new(io::ErrorKind::InvalidData, format!("{}; AppKit: {error}", errors.join("; ")))),
    }
}

/// Reads what the editor can paste, preferring files, then images, then
/// text.
pub fn read(_owner: isize) -> io::Result<Option<Pasted>> {
    read_from(&pasteboard())
}

fn read_from(board: &NSPasteboard) -> io::Result<Option<Pasted>> {
    let mut files = Vec::new();
    if let Some(items) = board.pasteboardItems() {
        for item in items.iter() {
            if item.types().iter().any(|name| name.to_string() == FILE_URL) {
                let value = item.stringForType(&ty(FILE_URL))
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "clipboard file URL data is unavailable"))?;
                let url = NSURL::URLWithString(&value)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid clipboard file URL"))?;
                let path = crate::macos::picker::file_path(&url)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, format!("invalid clipboard file URL: {error}")))?;
                files.push(path);
            }
        }
    }
    // Copied files also put their icons on the pasteboard; paste the files.
    if !files.is_empty() {
        return Ok(Some(Pasted::Files(files)));
    }
    let png_error = match board.dataForType(&ty(PNG)) {
        Some(data) => match decode_png(&data.to_vec()) {
            Ok((width, height, rgba)) => return Ok(Some(Pasted::Image { width, height, rgba })),
            Err(error) => Some(error),
        },
        None => None,
    };
    match (png_error, image_from(board)) {
        (_, Ok(Some((width, height, rgba)))) => Ok(Some(Pasted::Image { width, height, rgba })),
        (Some(png), Err(native)) => Err(io::Error::new(io::ErrorKind::InvalidData, format!("could not read clipboard image (PNG: {png}; AppKit: {native})"))),
        (Some(error), Ok(None)) => Err(io::Error::new(io::ErrorKind::InvalidData, format!("could not read clipboard PNG: {error}"))),
        (None, Err(error)) => Err(error),
        (None, Ok(None)) => {
            Ok(board.stringForType(&ty(TEXT)).map(|s| Pasted::Text(s.to_string())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_pasteboards_preserve_png_and_tiff_transparency() {
        let frame = Frame {
            width: 2,
            height: 2,
            alpha: true,
            pixels: vec![
                73, 110, 170, 127, 255, 0, 0, 255,
                0, 0, 0, 0, 127, 205, 12, 64,
            ],
        };
        let expected = [
            170, 110, 73, 127, 0, 0, 255, 255,
            0, 0, 0, 0, 12, 205, 127, 64,
        ];
        let board = NSPasteboard::pasteboardWithUniqueName();
        copy_to(&frame, &board).unwrap();
        let Some(Pasted::Image { width, height, rgba }) = read_from(&board).unwrap() else { panic!("no PNG image") };
        assert_eq!((width, height), (2, 2));
        assert_eq!(rgba, expected);

        let tiff = board.dataForType(&ty(TIFF)).expect("the copied image has a TIFF representation");
        let tiff_board = NSPasteboard::pasteboardWithUniqueName();
        tiff_board.clearContents();
        assert!(tiff_board.setData_forType(Some(&tiff), &ty(TIFF)));
        assert!(tiff_board.dataForType(&ty(PNG)).is_none());
        let Some(Pasted::Image { width, height, rgba }) = read_from(&tiff_board).unwrap() else { panic!("no TIFF image") };
        assert_eq!((width, height), (2, 2));
        for (actual, expected) in rgba.iter().zip(expected) {
            assert!(actual.abs_diff(expected) <= 2, "TIFF pixel differs: {rgba:?}");
        }
    }

    #[test]
    fn private_pasteboard_prefers_file_urls_to_file_icons() {
        let board = NSPasteboard::pasteboardWithUniqueName();
        copy_to(&Frame::new(1, 1), &board).unwrap();
        assert!(board.setString_forType(
            &NSString::from_str("file:///tmp/Jiffshot%20fixture%20%E2%98%83.png"),
            &ty(FILE_URL),
        ));
        let Some(Pasted::Files(files)) = read_from(&board).unwrap() else { panic!("no file URL") };
        assert_eq!(files, [PathBuf::from("/tmp/Jiffshot fixture \u{2603}.png")]);
    }

    #[test]
    fn private_pasteboard_preserves_non_utf8_file_paths() {
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/jiffshot-\xff.png".to_vec()));
        let url = crate::macos::picker::file_url(&path, false).unwrap();
        let board = NSPasteboard::pasteboardWithUniqueName();
        board.clearContents();
        assert!(board.setString_forType(&url.absoluteString().unwrap(), &ty(FILE_URL)));
        let Some(Pasted::Files(files)) = read_from(&board).unwrap() else { panic!("no file URL") };
        assert_eq!(files, [path]);
    }

    #[test]
    fn private_pasteboard_reads_multiple_file_items() {
        let board = NSPasteboard::pasteboardWithUniqueName();
        let items: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = [
            "file:///tmp/first.png", "file:///tmp/second%20image.png",
        ].into_iter().map(|value| {
            let item = NSPasteboardItem::new();
            assert!(item.setString_forType(&NSString::from_str(value), &ty(FILE_URL)));
            ProtocolObject::from_retained(item)
        }).collect();
        board.clearContents();
        assert!(board.writeObjects(&NSArray::from_retained_slice(&items)));
        let Some(Pasted::Files(files)) = read_from(&board).unwrap() else { panic!("no file URLs") };
        assert_eq!(files, [PathBuf::from("/tmp/first.png"), PathBuf::from("/tmp/second image.png")]);
    }

    #[test]
    fn copying_files_preserves_paths_and_rejects_invalid_input_without_clearing() {
        let root = std::env::temp_dir().join(format!("jiffshot-file-pasteboard-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let first = root.join("first image.png");
        let second = root.join("second-雪 100%.mp4");
        std::fs::write(&first, b"image").unwrap();
        std::fs::write(&second, b"video").unwrap();
        let files = vec![std::fs::canonicalize(&first).unwrap(), std::fs::canonicalize(&second).unwrap()];
        let board = NSPasteboard::pasteboardWithUniqueName();
        copy_files_to(&files, &board).unwrap();
        let Some(Pasted::Files(actual)) = read_from(&board).unwrap() else { panic!("no copied file URLs") };
        assert_eq!(actual, files);

        assert!(copy_files_to(&[], &board).is_err());
        assert!(copy_files_to(&[root.join("missing.png")], &board).is_err());
        assert!(copy_files_to(&[PathBuf::from("relative.png")], &board).is_err());
        assert!(copy_files_to(&[root.clone()], &board).is_err());
        let Some(Pasted::Files(actual)) = read_from(&board).unwrap() else { panic!("the previous files were lost") };
        assert_eq!(actual, files);
        std::fs::remove_file(first).unwrap();
        std::fs::remove_file(second).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn private_pasteboard_reads_text() {
        let board = NSPasteboard::pasteboardWithUniqueName();
        copy_text_to("Hello \u{2603}", &board).unwrap();
        assert!(matches!(read_from(&board).unwrap(), Some(Pasted::Text(text)) if text == "Hello \u{2603}"));
    }

    #[test]
    fn invalid_image_does_not_replace_private_pasteboard_contents() {
        let board = NSPasteboard::pasteboardWithUniqueName();
        board.clearContents();
        assert!(board.setString_forType(&NSString::from_str("keep this"), &ty(TEXT)));
        let frame = Frame { width: 2, height: 2, pixels: vec![0; 4], alpha: true };
        assert!(copy_to(&frame, &board).is_err());
        assert!(matches!(read_from(&board).unwrap(), Some(Pasted::Text(text)) if text == "keep this"));
    }

    #[test]
    fn private_pasteboard_reports_invalid_image_and_file_data() {
        let board = NSPasteboard::pasteboardWithUniqueName();
        board.clearContents();
        assert!(board.setData_forType(Some(&NSData::with_bytes(b"not a PNG")), &ty(PNG)));
        assert!(board.setString_forType(&NSString::from_str("unrelated text"), &ty(TEXT)));
        assert!(read_from(&board).is_err());

        board.clearContents();
        assert!(board.setData_forType(Some(&NSData::with_bytes(b"not a PNG")), &ty(PNG)));
        assert!(board.setData_forType(Some(&NSData::with_bytes(b"not a TIFF")), &ty(TIFF)));
        assert!(read_from(&board).is_err());

        board.clearContents();
        assert!(board.setString_forType(&NSString::from_str("https://example.invalid/image.png"), &ty(FILE_URL)));
        assert!(read_from(&board).is_err());
        board.clearContents();
        assert!(board.setString_forType(&NSString::from_str("file://remote.invalid/tmp/image.png"), &ty(FILE_URL)));
        assert!(read_from(&board).err().unwrap().to_string().contains("remote host"));
        board.clearContents();
        assert!(board.setString_forType(&NSString::from_str("file:///tmp/image%00.png"), &ty(FILE_URL)));
        assert!(read_from(&board).err().unwrap().to_string().contains("NUL"));
        board.clearContents();
        assert!(board.setData_forType(Some(&NSData::with_bytes(&[0xff])), &ty(FILE_URL)));
        assert!(read_from(&board).is_err());
    }

    #[test]
    fn private_pasteboard_uses_valid_tiff_when_png_is_invalid() {
        let source = NSPasteboard::pasteboardWithUniqueName();
        let mut frame = Frame::new(1, 1);
        frame.alpha = true;
        frame.pixels.copy_from_slice(&[0, 0, 255, 128]);
        copy_to(&frame, &source).unwrap();
        let tiff = source.dataForType(&ty(TIFF)).unwrap();

        let board = NSPasteboard::pasteboardWithUniqueName();
        board.clearContents();
        assert!(board.setData_forType(Some(&NSData::with_bytes(b"not a PNG")), &ty(PNG)));
        assert!(board.setData_forType(Some(&tiff), &ty(TIFF)));
        let Some(Pasted::Image { width, height, rgba }) = read_from(&board).unwrap() else { panic!("no TIFF image") };
        assert_eq!((width, height), (1, 1));
        assert_eq!(rgba, [255, 0, 0, 128]);
    }

    #[test]
    fn private_pasteboard_reads_other_native_image_formats() {
        use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep};
        use objc2_foundation::NSDictionary;

        let frame = Frame { width: 4, height: 4, pixels: [30, 100, 200, 255].repeat(16), alpha: false };
        let png = NSData::with_bytes(&frame.encode_png_vec().unwrap());
        let bitmap = NSBitmapImageRep::imageRepWithData(&png).unwrap();
        for (format, name, tolerance) in [
            (NSBitmapImageFileType::JPEG, "public.jpeg", 8),
            (NSBitmapImageFileType::BMP, "com.microsoft.bmp", 1),
            (NSBitmapImageFileType::GIF, "com.compuserve.gif", 1),
        ] {
            // SAFETY: an empty property dictionary requires no typed values.
            let data = unsafe { bitmap.representationUsingType_properties(format, &NSDictionary::new()) }.unwrap();
            let board = NSPasteboard::pasteboardWithUniqueName();
            board.clearContents();
            assert!(board.setData_forType(Some(&data), &ty(name)));
            let Some(Pasted::Image { width, height, rgba }) = read_from(&board).unwrap() else { panic!("no {name} image") };
            assert_eq!((width, height), (4, 4));
            for pixel in rgba.chunks_exact(4) {
                for (actual, expected) in pixel.iter().zip([200u8, 100, 30, 255]) {
                    assert!(actual.abs_diff(expected) <= tolerance, "{name} pixel differs: {pixel:?}");
                }
            }
            board.clearContents();
            assert!(board.setData_forType(Some(&NSData::with_bytes(b"not a TIFF")), &ty(TIFF)));
            assert!(board.setData_forType(Some(&data), &ty(name)));
            let Some(Pasted::Image { rgba, .. }) = read_from(&board).unwrap() else { panic!("no fallback {name} image") };
            for pixel in rgba.chunks_exact(4) {
                for (actual, expected) in pixel.iter().zip([200u8, 100, 30, 255]) {
                    assert!(actual.abs_diff(expected) <= tolerance, "fallback {name} pixel differs: {pixel:?}");
                }
            }
        }
    }

    #[test]
    fn grayscale_png_is_converted_to_straight_rgba() {
        let mut encoded = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut encoded, 2, 1);
            encoder.set_color(png::ColorType::GrayscaleAlpha);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[60, 128, 200, 255]).unwrap();
            writer.finish().unwrap();
        }
        let (width, height, rgba) = decode_png(&encoded).unwrap();
        assert_eq!((width, height), (2, 1));
        assert_eq!(rgba, [60, 60, 60, 128, 200, 200, 200, 255]);
        assert!(image_buffer_size(0, 1).is_err());
        assert!(image_buffer_size(u32::MAX, 1).is_err());
    }

    #[test]
    #[ignore = "touches the user's clipboard"]
    fn image_round_trips() {
        let mut frame = Frame::new(3, 2);
        frame.pixels[..4].copy_from_slice(&[1, 2, 3, 255]);
        copy(&frame, 0).unwrap();
        let Some(Pasted::Image { width, height, rgba }) = read(0).unwrap() else { panic!("no image") };
        assert_eq!((width, height), (3, 2));
        assert_eq!(&rgba[..4], &[3, 2, 1, 255]);
        copy_text("hello", 0).unwrap();
        assert!(matches!(read(0).unwrap(), Some(Pasted::Text(t)) if t == "hello"));
    }
}
