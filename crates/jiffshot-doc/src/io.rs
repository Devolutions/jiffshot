//! Reading and writing images and editable documents.
//!
//! The native `.jsd` format is a zip archive with `document.json` (canvas
//! size and objects), `background.png`, and one PNG per image object under
//! `images/`. Image exports flatten the annotations.

use crate::color::Rgba;
use crate::document::Document;
use crate::object::{Kind, Object};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::{Cursor, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tiny_skia::Pixmap;

pub type Result<T> = std::result::Result<T, String>;

// Bound allocations before parsing ZIP metadata and prevent oversized source files.
const MAX_DOCUMENT_BYTES: usize = 256 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = 160 * 1024 * 1024;
const MAX_TOTAL_UNCOMPRESSED_BYTES: u64 = 256 * 1024 * 1024;
// 33,177,600 pixels is a 7680x4320 frame; this also caps total document image pixels.
pub const MAX_DOCUMENT_PIXELS: u64 = 33_177_600;
pub(crate) const MAX_DOCUMENT_OBJECTS: usize = 10_000;
const MAX_DOCUMENT_ENTRIES: usize = MAX_DOCUMENT_OBJECTS + 2;
// Bounds geometry and style values before they reach rendering calculations.
const MAX_DOCUMENT_NUMBER: f64 = 1_000_000.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
    Bmp,
    Gif,
    Webp,
    /// A one-page PDF holding the image as JPEG.
    Pdf,
}

impl Format {
    pub fn from_path(path: &Path) -> Option<Format> {
        Some(
            match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
                "png" => Format::Png,
                "jpg" | "jpeg" => Format::Jpeg,
                "bmp" => Format::Bmp,
                "gif" => Format::Gif,
                "webp" => Format::Webp,
                "pdf" => Format::Pdf,
                _ => return None,
            },
        )
    }
}

/// Extensions `open` understands.
pub const OPEN_EXTENSIONS: &[&str] = &["jsd", "png", "jpg", "jpeg", "bmp", "gif", "webp"];

/// Whether `path` is a Jiffshot document (`.jsd`).
pub fn is_document(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("jsd"))
}

pub fn to_rgba_image(pm: &Pixmap) -> image::RgbaImage {
    let mut out = image::RgbaImage::new(pm.width(), pm.height());
    for (dst, src) in out.pixels_mut().zip(pm.pixels()) {
        let c = src.demultiply();
        *dst = image::Rgba([c.red(), c.green(), c.blue(), c.alpha()]);
    }
    out
}

pub fn from_rgba_image(img: &image::RgbaImage) -> Option<Pixmap> {
    let mut pm = Pixmap::new(img.width(), img.height())?;
    for (dst, src) in pm.pixels_mut().iter_mut().zip(img.pixels()) {
        *dst = Rgba::new(src[0], src[1], src[2], src[3]).premultiplied();
    }
    Some(pm)
}

pub fn decode_image(bytes: &[u8]) -> Result<Pixmap> {
    let format = image::guess_format(bytes).map_err(|e| e.to_string())?;
    let dimensions = image::ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|e| e.to_string())?;
    validate_image_dimensions(dimensions)?;
    let img = image::load_from_memory_with_format(bytes, format).map_err(|e| e.to_string())?;
    from_rgba_image(&img.to_rgba8()).ok_or_else(|| "image has no pixels".to_owned())
}

fn validate_image_dimensions((width, height): (u32, u32)) -> Result<()> {
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_DOCUMENT_PIXELS {
        return Err(format!(
            "image dimensions {width}x{height} exceed the supported limits"
        ));
    }
    Ok(())
}

fn validate_object_count(objects: &[Object]) -> Result<()> {
    let mut count = 0;
    for object in objects {
        object.walk(&mut |_| count += 1);
    }
    if count > MAX_DOCUMENT_OBJECTS {
        return Err("document contains too many objects".into());
    }
    Ok(())
}

fn validate_manifest_numbers(value: &serde_json::Value) -> Result<()> {
    match value {
        serde_json::Value::Number(number) => {
            let Some(number) = number.as_f64() else {
                return Err("document contains an out-of-range number".into());
            };
            if !number.is_finite() || number.abs() > MAX_DOCUMENT_NUMBER {
                return Err("document contains an out-of-range number".into());
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                validate_manifest_numbers(value)?;
            }
        }
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                if key != "id" {
                    validate_manifest_numbers(value)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn load_image(path: &Path) -> Result<Pixmap> {
    let bytes = read_limited(path, MAX_IMAGE_BYTES)?;
    decode_image(&bytes)
}

fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() as u64 > limit {
        return Err(format!("{}: file exceeds the size limit", path.display()));
    }
    Ok(bytes)
}

/// Encodes `pm`. Formats without transparency get it flattened onto white.
pub fn encode(pm: &Pixmap, format: Format, jpeg_quality: u8) -> Result<Vec<u8>> {
    let rgba = to_rgba_image(pm);
    let mut out = Cursor::new(Vec::new());
    let flat = || {
        let mut rgb = image::RgbImage::new(rgba.width(), rgba.height());
        for (d, s) in rgb.pixels_mut().zip(rgba.pixels()) {
            let a = s[3] as u32;
            let f = |c: u8| ((c as u32 * a + 255 * (255 - a)) / 255) as u8;
            *d = image::Rgb([f(s[0]), f(s[1]), f(s[2])]);
        }
        rgb
    };
    match format {
        Format::Png => {
            let enc = image::codecs::png::PngEncoder::new_with_quality(
                &mut out,
                image::codecs::png::CompressionType::Fast,
                image::codecs::png::FilterType::Adaptive,
            );
            image::ImageEncoder::write_image(
                enc,
                rgba.as_raw(),
                rgba.width(),
                rgba.height(),
                image::ExtendedColorType::Rgba8,
            )
        }
        Format::Jpeg => {
            let rgb = flat();
            let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(
                &mut out,
                jpeg_quality.clamp(1, 100),
            );
            image::ImageEncoder::write_image(
                enc,
                rgb.as_raw(),
                rgb.width(),
                rgb.height(),
                image::ExtendedColorType::Rgb8,
            )
        }
        Format::Bmp => {
            let rgb = flat();
            image::DynamicImage::ImageRgb8(rgb).write_to(&mut out, image::ImageFormat::Bmp)
        }
        Format::Gif => {
            image::DynamicImage::ImageRgba8(rgba).write_to(&mut out, image::ImageFormat::Gif)
        }
        Format::Webp => {
            image::DynamicImage::ImageRgba8(rgba).write_to(&mut out, image::ImageFormat::WebP)
        }
        Format::Pdf => return pdf(pm, jpeg_quality.max(85)),
    }
    .map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

/// A minimal one-page PDF: the image as a JPEG stream on a page of its
/// size (96 pixels to the inch).
pub fn pdf(pm: &Pixmap, quality: u8) -> Result<Vec<u8>> {
    let jpeg = encode(pm, Format::Jpeg, quality)?;
    let (w, h) = (pm.width(), pm.height());
    let (pw, ph) = (w as f32 * 0.75, h as f32 * 0.75);
    let content = format!("q {pw:.2} 0 0 {ph:.2} 0 0 cm /Im0 Do Q");
    let mut out: Vec<u8> = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = Vec::new();
    let mut object = |out: &mut Vec<u8>, body: &[u8]| {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", offsets.len()).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    };
    object(&mut out, b"<< /Type /Catalog /Pages 2 0 R >>");
    object(&mut out, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>");
    object(&mut out, format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {pw:.2} {ph:.2}] /Resources << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>").as_bytes());
    let mut image = format!("<< /Type /XObject /Subtype /Image /Width {w} /Height {h} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n", jpeg.len()).into_bytes();
    image.extend_from_slice(&jpeg);
    image.extend_from_slice(b"\nendstream");
    object(&mut out, &image);
    object(
        &mut out,
        format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        )
        .as_bytes(),
    );
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes(),
    );
    for o in &offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len() + 1
        )
        .as_bytes(),
    );
    Ok(out)
}

/// Writes the flattened document as an image, the format chosen by the
/// file extension (PNG when unknown).
pub fn export(doc: &Document, path: &Path) -> Result<()> {
    let format = Format::from_path(path).unwrap_or(Format::Png);
    let bytes = encode(&doc.flatten(), format, 90)?;
    write_atomic(path, &bytes)
}

/// Writes through a temporary file so a failed save never truncates the
/// previous version.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    struct StagingFile(std::path::PathBuf);
    impl Drop for StagingFile {
        fn drop(&mut self) {
            if let Err(error) = std::fs::remove_file(&self.0) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("could not remove {}: {error}", self.0.display());
                }
            }
        }
    }

    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let existing_permissions = match std::fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for _ in 0..100 {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let staging = parent.join(format!(".jiffshot-write-{}-{id}.tmp", std::process::id()));
        if staging.file_name() == path.file_name() {
            continue;
        }
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("{}: {error}", staging.display())),
        };
        let staging = StagingFile(staging);
        let written = file
            .write_all(bytes)
            .and_then(|()| {
                if let Some(permissions) = existing_permissions.clone() {
                    file.set_permissions(permissions)?;
                }
                file.sync_all()
            });
        drop(file);
        written.map_err(|e| format!("{}: {e}", staging.0.display()))?;
        return std::fs::rename(&staging.0, path).map_err(|e| format!("{}: {e}", path.display()));
    }
    Err(format!(
        "cannot reserve a staging file in {}",
        parent.display()
    ))
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    format: String,
    version: u32,
    width: u32,
    height: u32,
    objects: Vec<Object>,
}

const FORMAT: &str = "jiffshot-document";

pub fn document_bytes(doc: &Document) -> Result<Vec<u8>> {
    validate_image_dimensions((doc.width(), doc.height()))?;
    validate_object_count(&doc.objects)?;
    let mut manifest = Manifest {
        format: FORMAT.to_owned(),
        version: 1,
        width: doc.width(),
        height: doc.height(),
        objects: doc.objects.clone(),
    };
    let mut reserved = HashSet::new();
    for object in &manifest.objects {
        object.walk(&mut |object| {
            if let Kind::Image { image, .. } = &object.kind {
                if !image.key.is_empty() {
                    reserved.insert(image.key.clone());
                }
            }
        });
    }
    let mut images: BTreeMap<String, Arc<Pixmap>> = BTreeMap::new();
    let mut suffix = 1_u64;
    for object in &mut manifest.objects {
        object.walk_mut(&mut |object| {
            let Kind::Image { image, .. } = &mut object.kind else {
                return;
            };
            let Some(pm) = &image.pixmap else { return };
            let conflict = image.key.is_empty()
                || images.get(&image.key).is_some_and(|existing| {
                    !Arc::ptr_eq(existing, pm)
                        && (existing.width() != pm.width()
                            || existing.height() != pm.height()
                            || existing.data() != pm.data())
                });
            if conflict {
                let base = if image.key.is_empty() {
                    "image"
                } else {
                    &image.key
                };
                loop {
                    let candidate = format!("{base}-{suffix}");
                    suffix += 1;
                    if reserved.insert(candidate.clone()) {
                        image.key = candidate;
                        break;
                    }
                }
            }
            images
                .entry(image.key.clone())
                .or_insert_with(|| pm.clone());
        });
    }
    let mut total_pixels = u64::from(doc.width()) * u64::from(doc.height());
    for pixmap in images.values() {
        validate_image_dimensions((pixmap.width(), pixmap.height()))?;
        total_pixels = total_pixels
            .checked_add(u64::from(pixmap.width()) * u64::from(pixmap.height()))
            .filter(|total| *total <= MAX_DOCUMENT_PIXELS)
            .ok_or("document images exceed the total pixel limit")?;
    }
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let deflated = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let manifest = serde_json::to_value(&manifest).map_err(|e| e.to_string())?;
    validate_manifest_numbers(&manifest)?;
    let json = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
    if json.len() as u64 > MAX_MANIFEST_BYTES {
        return Err("document manifest exceeds the size limit".into());
    }
    zip.start_file("document.json", deflated)
        .map_err(|e| e.to_string())?;
    zip.write_all(&json).map_err(|e| e.to_string())?;
    let mut total_uncompressed_bytes = json.len() as u64;
    let background = encode(doc.background(), Format::Png, 0)?;
    total_uncompressed_bytes += background.len() as u64;
    if background.len() as u64 > MAX_IMAGE_BYTES
        || total_uncompressed_bytes > MAX_TOTAL_UNCOMPRESSED_BYTES
    {
        return Err("document images exceed the uncompressed size limit".into());
    }
    zip.start_file("background.png", stored)
        .map_err(|e| e.to_string())?;
    zip.write_all(&background).map_err(|e| e.to_string())?;
    for (key, pm) in images {
        let image = encode(&pm, Format::Png, 0)?;
        total_uncompressed_bytes += image.len() as u64;
        if image.len() as u64 > MAX_IMAGE_BYTES
            || total_uncompressed_bytes > MAX_TOTAL_UNCOMPRESSED_BYTES
        {
            return Err("document images exceed the uncompressed size limit".into());
        }
        zip.start_file(format!("images/{key}.png"), stored)
            .map_err(|e| e.to_string())?;
        zip.write_all(&image).map_err(|e| e.to_string())?;
    }
    let bytes = zip.finish().map_err(|e| e.to_string())?.into_inner();
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err("document exceeds the compressed size limit".into());
    }
    Ok(bytes)
}

pub fn save_document(doc: &Document, path: &Path) -> Result<()> {
    write_atomic(path, &document_bytes(doc)?)
}

fn entry(
    zip: &mut zip::ZipArchive<Cursor<Vec<u8>>>,
    name: &str,
    max_bytes: u64,
    total_bytes: &mut u64,
) -> Result<Vec<u8>> {
    let f = zip.by_name(name).map_err(|e| format!("{name}: {e}"))?;
    let remaining = MAX_TOTAL_UNCOMPRESSED_BYTES
        .checked_sub(*total_bytes)
        .ok_or("document entries exceed the total uncompressed size limit")?;
    let limit = max_bytes.min(remaining);
    if f.size() > limit {
        return Err(format!("{name} exceeds the uncompressed size limit"));
    }
    let mut buf = Vec::new();
    f.take(limit + 1)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    if buf.len() as u64 > limit {
        return Err(format!("{name} exceeds the uncompressed size limit"));
    }
    *total_bytes += buf.len() as u64;
    Ok(buf)
}

pub fn document_from_bytes(bytes: Vec<u8>) -> Result<Document> {
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err("document exceeds the compressed size limit".into());
    }
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
    if zip.len() > MAX_DOCUMENT_ENTRIES {
        return Err("document contains too many archive entries".into());
    }
    let mut total_uncompressed_bytes = 0;
    let manifest_bytes = entry(
        &mut zip,
        "document.json",
        MAX_MANIFEST_BYTES,
        &mut total_uncompressed_bytes,
    )?;
    let manifest_value: serde_json::Value =
        serde_json::from_slice(&manifest_bytes).map_err(|e| e.to_string())?;
    validate_manifest_numbers(&manifest_value)?;
    let manifest: Manifest = serde_json::from_value(manifest_value).map_err(|e| e.to_string())?;
    if manifest.format != FORMAT {
        return Err(format!("not a Jiffshot document ({})", manifest.format));
    }
    if manifest.version != 1 {
        return Err(format!(
            "unsupported Jiffshot document version {}",
            manifest.version
        ));
    }
    validate_object_count(&manifest.objects)?;
    validate_image_dimensions((manifest.width, manifest.height))?;
    let background_bytes = entry(
        &mut zip,
        "background.png",
        MAX_IMAGE_BYTES,
        &mut total_uncompressed_bytes,
    )?;
    let background = decode_image(&background_bytes)?;
    if (manifest.width, manifest.height) != (background.width(), background.height()) {
        return Err("document canvas dimensions do not match background.png".into());
    }
    let mut total_pixels = u64::from(background.width()) * u64::from(background.height());
    let mut doc = Document::new(background);
    let mut image_keys = BTreeSet::new();
    for object in &manifest.objects {
        object.walk(&mut |object| {
            if let Kind::Image { image, .. } = &object.kind {
                if !image.key.is_empty() {
                    image_keys.insert(image.key.clone());
                }
            }
        });
    }
    let mut cache = std::collections::HashMap::<String, Arc<Pixmap>>::new();
    for key in image_keys {
        let bytes = entry(
            &mut zip,
            &format!("images/{key}.png"),
            MAX_IMAGE_BYTES,
            &mut total_uncompressed_bytes,
        )?;
        let format = image::guess_format(&bytes).map_err(|e| e.to_string())?;
        let dimensions = image::ImageReader::with_format(Cursor::new(&bytes), format)
            .into_dimensions()
            .map_err(|e| e.to_string())?;
        validate_image_dimensions(dimensions)?;
        let pixels = u64::from(dimensions.0) * u64::from(dimensions.1);
        total_pixels = total_pixels
            .checked_add(pixels)
            .filter(|total| *total <= MAX_DOCUMENT_PIXELS)
            .ok_or("document images exceed the total pixel limit")?;
        let pixmap = decode_image(&bytes)?;
        cache.insert(key, Arc::new(pixmap));
    }
    for mut object in manifest.objects {
        object.walk_mut(&mut |object| {
            if let Kind::Image { image, .. } = &mut object.kind {
                image.pixmap = cache.get(&image.key).cloned();
            }
        });
        doc.objects.push(object);
    }
    doc.renumber_from_objects();
    Ok(doc)
}

pub fn load_document(path: &Path) -> Result<Document> {
    document_from_bytes(read_limited(path, MAX_DOCUMENT_BYTES as u64)?)
}

/// Opens any supported file as a document.
pub fn open(path: &Path) -> Result<Document> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("jsd") => load_document(path),
        _ => load_image(path).map(Document::new),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::{Box2, Pt};
    use crate::object::{ImageRef, Style};

    fn archive_with_manifest(manifest: serde_json::Value) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("document.json", options).unwrap();
        zip.write_all(&serde_json::to_vec(&manifest).unwrap())
            .unwrap();
        zip.start_file("background.png", options).unwrap();
        let background = crate::raster::solid(2, 2, Rgba::WHITE).unwrap();
        zip.write_all(&encode(&background, Format::Png, 0).unwrap())
            .unwrap();
        zip.finish().unwrap().into_inner()
    }

    fn document_archive(version: u32, width: u32, height: u32) -> Vec<u8> {
        archive_with_manifest(serde_json::json!({
            "format": FORMAT,
            "version": version,
            "width": width,
            "height": height,
            "objects": []
        }))
    }

    fn archive_with_oversized_manifest() -> Vec<u8> {
        const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
        const CHUNK: [u8; 8192] = [b'x'; 8192];

        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let compressed = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("document.json", compressed).unwrap();
        let prefix = format!(
            r#"{{"format":"{FORMAT}","version":1,"width":2,"height":2,"objects":[],"padding":""#
        );
        let suffix = b"\"}";
        zip.write_all(prefix.as_bytes()).unwrap();

        let padding_len = MAX_MANIFEST_BYTES + 1 - prefix.len() - suffix.len();
        for _ in 0..padding_len / CHUNK.len() {
            zip.write_all(&CHUNK).unwrap();
        }
        zip.write_all(&CHUNK[..padding_len % CHUNK.len()]).unwrap();
        zip.write_all(suffix).unwrap();

        zip.start_file("background.png", zip::write::SimpleFileOptions::default())
            .unwrap();
        let background = crate::raster::solid(2, 2, Rgba::WHITE).unwrap();
        zip.write_all(&encode(&background, Format::Png, 0).unwrap())
            .unwrap();
        zip.finish().unwrap().into_inner()
    }

    fn png_with_dimensions(width: u32, height: u32) -> Vec<u8> {
        let pixmap = crate::raster::solid(2, 2, Rgba::WHITE).unwrap();
        let mut png = encode(&pixmap, Format::Png, 0).unwrap();
        png[16..20].copy_from_slice(&width.to_be_bytes());
        png[20..24].copy_from_slice(&height.to_be_bytes());
        let checksum = png_crc32(&png[12..29]);
        png[29..33].copy_from_slice(&checksum.to_be_bytes());
        png
    }

    fn png_crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0_u32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                let bit_mask = 0_u32.wrapping_sub(crc & 1);
                crc = (crc >> 1) ^ (0xedb8_8320 & bit_mask);
            }
        }
        !crc
    }

    #[test]
    fn rejects_unsupported_document_versions_and_mismatched_canvas_size() {
        assert!(
            document_from_bytes(document_archive(2, 2, 2))
                .unwrap_err()
                .contains("unsupported Jiffshot document version 2")
        );
        assert!(
            document_from_bytes(document_archive(1, 3, 2))
                .unwrap_err()
                .contains("canvas dimensions do not match")
        );
    }

    #[test]
    fn rejects_manifest_entries_over_the_uncompressed_size_limit() {
        let archive = archive_with_oversized_manifest();

        assert!(
            document_from_bytes(archive).is_err(),
            "a compressed manifest larger than 16 MiB must be rejected"
        );
    }

    #[test]
    fn rejects_extreme_manifest_dimensions_before_background_mismatch() {
        let error = document_from_bytes(document_archive(1, u32::MAX, u32::MAX))
            .expect_err("extreme canvas dimensions must be rejected");

        assert!(
            !error.contains("canvas dimensions do not match"),
            "expected the size bound to reject dimensions before comparing the tiny background: {error}"
        );
    }

    #[test]
    fn rejects_out_of_range_manifest_numbers() {
        let archive = archive_with_manifest(serde_json::json!({
            "format": FORMAT,
            "version": 1,
            "width": 2,
            "height": 2,
            "objects": [{
                "id": 1,
                "kind": {"type": "Step", "center": {"x": 1_000_001.0, "y": 1.0}, "radius": 4.0, "label": "1"},
                "style": {}
            }]
        }));
        let error = document_from_bytes(archive)
            .expect_err("out-of-range manifest coordinates must be rejected");

        assert!(error.contains("out-of-range number"), "{error}");
    }

    #[test]
    fn preserves_large_object_ids() {
        let mut document = Document::blank(2, 2, Rgba::WHITE);
        let id = document.add(
            Kind::Step {
                center: Pt::new(1.0, 1.0),
                radius: 1.0,
                label: "1".into(),
                shape: Default::default(),
            },
            Style::default(),
        );
        document.get_mut(id).unwrap().id = 1_000_001;

        let bytes = document_bytes(&document).expect("large object IDs are valid");
        let reopened = document_from_bytes(bytes).expect("saved large object IDs should reopen");
        assert_eq!(reopened.objects[0].id, 1_000_001);
    }

    #[test]
    fn rejects_too_many_nested_objects() {
        let step = serde_json::json!({
            "id": 1,
            "kind": {"type": "Step", "center": {"x": 1.0, "y": 1.0}, "radius": 4.0, "label": "1"},
            "style": {}
        });
        let children = vec![step; MAX_DOCUMENT_OBJECTS];
        let archive = archive_with_manifest(serde_json::json!({
            "format": FORMAT,
            "version": 1,
            "width": 2,
            "height": 2,
            "objects": [{
                "id": 1,
                "kind": {"type": "Group", "objects": children},
                "style": {}
            }]
        }));
        let error = document_from_bytes(archive)
            .expect_err("nested objects must count toward the document limit");

        assert!(error.contains("too many objects"), "{error}");
    }

    #[test]
    fn refuses_to_write_documents_outside_the_open_limits() {
        let mut document = Document::blank(2, 2, Rgba::WHITE);
        document.add(
            Kind::Step {
                center: Pt::new(1_000_001.0, 1.0),
                radius: 4.0,
                label: "1".into(),
                shape: Default::default(),
            },
            Style::default(),
        );

        assert!(
            document_bytes(&document)
                .expect_err("out-of-range geometry must not be written")
                .contains("out-of-range number")
        );
    }

    #[test]
    fn rejects_image_dimensions_that_exceed_decoder_allocation_limits() {
        let image = png_with_dimensions(100_000, 100_000);
        let error = decode_image(&image).expect_err("oversized image dimensions must be rejected");

        assert!(
            error.to_ascii_lowercase().contains("dimension")
                || error.to_ascii_lowercase().contains("limit")
                || error.to_ascii_lowercase().contains("large"),
            "expected rejection for oversized dimensions, not an unrelated decode error: {error}"
        );
    }

    #[test]
    fn documents_round_trip() {
        let mut d = Document::blank(64, 32, Rgba::rgb(10, 20, 30));
        d.add(
            Kind::Line {
                a: Pt::new(1.0, 2.0),
                b: Pt::new(30.0, 20.0),
                control: None,
            },
            Style::default(),
        );
        d.add(
            Kind::Image {
                rect: Box2::new(5.0, 5.0, 15.0, 15.0),
                image: ImageRef::new("a", crate::raster::solid(4, 4, Rgba::RED).unwrap()),
            },
            Style::default(),
        );
        let bytes = document_bytes(&d).unwrap();
        let mut back = document_from_bytes(bytes).unwrap();
        assert_eq!(back.size(), (64, 32));
        assert_eq!(back.objects, d.objects);
        assert_eq!(back.background().data(), d.background().data());
        let Kind::Image { image, .. } = &back.objects[1].kind else {
            panic!()
        };
        assert_eq!(image.pixmap.as_ref().unwrap().width(), 4);
        assert_eq!(
            back.add(
                Kind::Text {
                    rect: Box2::default(),
                    text: String::new()
                },
                Style::default()
            ),
            3
        );
    }

    #[test]
    fn document_round_trip_keeps_distinct_images_with_the_same_key() {
        let mut d = Document::blank(10, 10, Rgba::WHITE);
        for (key, color) in [
            ("shared", Rgba::RED),
            ("shared", Rgba::BLACK),
            ("shared-1", Rgba::YELLOW),
            ("", Rgba::WHITE),
        ] {
            d.add(
                Kind::Image {
                    rect: Box2::new(0.0, 0.0, 1.0, 1.0),
                    image: ImageRef::new(key, crate::raster::solid(1, 1, color).unwrap()),
                },
                Style::default(),
            );
        }
        let back = document_from_bytes(document_bytes(&d).unwrap()).unwrap();
        let colors: Vec<_> = back
            .objects
            .iter()
            .map(|object| {
                let Kind::Image { image, .. } = &object.kind else {
                    panic!("expected image");
                };
                crate::raster::pixel(image.pixmap.as_deref().unwrap(), 0, 0)
            })
            .collect();
        assert_eq!(colors, [Rgba::RED, Rgba::BLACK, Rgba::YELLOW, Rgba::WHITE]);
    }

    #[test]
    fn atomic_write_preserves_unrelated_temporary_files_and_concurrent_writes() {
        use std::sync::{Arc, Barrier};
        use std::time::{SystemTime, UNIX_EPOCH};

        struct TestDir(std::path::PathBuf);
        impl Drop for TestDir {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).unwrap();
            }
        }
        let dir = TestDir(std::env::temp_dir().join(format!(
            "jiffshot-doc-write-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        )));
        std::fs::create_dir(&dir.0).unwrap();
        let target = dir.0.join("drawing.jsd");
        let other = target.with_extension("tmp~");
        std::fs::write(&other, b"unrelated file").unwrap();
        write_atomic(&target, b"first").unwrap();
        assert_eq!(std::fs::read(&other).unwrap(), b"unrelated file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
            write_atomic(&target, b"private").unwrap();
            assert_eq!(
                std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                0o600,
                "replacing a file should preserve its permissions"
            );
        }

        let barrier = Arc::new(Barrier::new(4));
        let writers: Vec<_> = (0_u8..4)
            .map(|value| {
                let target = target.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    write_atomic(&target, &vec![value; 128 * 1024])
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap().unwrap();
        }
        let result = std::fs::read(&target).unwrap();
        assert_eq!(result.len(), 128 * 1024);
        assert!(result.iter().all(|byte| *byte == result[0]));
        assert_eq!(std::fs::read(&other).unwrap(), b"unrelated file");
        let directory = dir.0.join("not-a-file.jsd");
        std::fs::create_dir(&directory).unwrap();
        assert!(write_atomic(&directory, b"cannot replace a directory").is_err());
        assert!(directory.is_dir());
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 3);
    }

    #[test]
    fn encodes_every_format() {
        let pm = crate::raster::solid(8, 8, Rgba::RED.with_alpha(128)).unwrap();
        let pdf = encode(&pm, Format::Pdf, 90).unwrap();
        assert!(pdf.starts_with(b"%PDF-1.4") && pdf.ends_with(b"%%EOF\n"));
        for f in [
            Format::Png,
            Format::Jpeg,
            Format::Bmp,
            Format::Gif,
            Format::Webp,
        ] {
            let bytes = encode(&pm, f, 85).unwrap();
            let back = decode_image(&bytes).unwrap();
            assert_eq!((back.width(), back.height()), (8, 8), "{f:?}");
        }
    }
}
