//! Preparing library captures for copying or dragging without changing them.

use jiffshot_capture::Frame;
use jiffshot_doc::text::FontSubstitution;
use jiffshot_doc::{io, Document, Pixmap};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub(crate) struct Source {
    pub path: PathBuf,
    pub open_document: Option<Document>,
}

enum Content {
    Image(Frame),
    Files(Vec<PathBuf>),
}

#[derive(Default)]
struct Exports {
    paths: Vec<PathBuf>,
    keep: bool,
}

impl Drop for Exports {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        for path in &self.paths {
            for file in [path.clone(), path.with_extension("tmp~")] {
                if let Err(error) = std::fs::remove_file(&file) {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        eprintln!("could not remove unused transfer export {}: {error}", file.display());
                    }
                }
            }
            if let Some(dir) = path.parent() {
                if let Err(error) = std::fs::remove_dir(dir) {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        eprintln!("could not remove unused transfer folder {}: {error}", dir.display());
                    }
                }
            }
        }
    }
}

pub(crate) struct Prepared {
    content: Content,
    exports: Exports,
    font_substitutions: Vec<FontSubstitution>,
}

impl Prepared {
    pub fn copy(mut self, owner: isize) -> Result<(String, Vec<FontSubstitution>), String> {
        let message = match &self.content {
            Content::Image(frame) => {
                jiffshot_capture::clipboard::copy(frame, owner).map_err(|error| error.to_string())?;
                format!("Copied the image ({} × {}) to the clipboard", frame.width, frame.height)
            }
            Content::Files(files) => {
                jiffshot_capture::clipboard::copy_files(files, owner).map_err(|error| error.to_string())?;
                format!("Copied {} capture{} to the clipboard", files.len(), if files.len() == 1 { "" } else { "s" })
            }
        };
        // Pasteboard consumers may read file URLs after this call returns.
        self.exports.keep = true;
        Ok((message, std::mem::take(&mut self.font_substitutions)))
    }

    pub fn font_substitutions(&self) -> &[FontSubstitution] {
        &self.font_substitutions
    }

    pub fn file_urls_json(&self) -> Result<Vec<u16>, String> {
        let Content::Files(files) = &self.content else {
            return Err("the prepared transfer does not contain files".into());
        };
        let urls = files.iter().map(|path| {
            let url = crate::winutil::file_url(path)?;
            url.absoluteString().map(|value| value.to_string())
                .ok_or_else(|| format!("could not prepare a file URL for {}", path.display()))
        }).collect::<Result<Vec<_>, _>>()?;
        let json = serde_json::to_string(&urls).map_err(|error| error.to_string())?;
        Ok(json.encode_utf16().chain(std::iter::once(0)).collect())
    }

    pub fn retain_for_destination(&mut self) {
        self.exports.keep = true;
    }

    pub fn discard_after_completion(&mut self) {
        self.exports.keep = false;
    }

    pub fn files_for_selftest(&self) -> Vec<PathBuf> {
        match &self.content {
            Content::Files(files) => files.clone(),
            Content::Image(_) => Vec::new(),
        }
    }
}

fn video(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("mp4"))
}

fn checked_file(path: &Path) -> Result<PathBuf, String> {
    let metadata = std::fs::metadata(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("the capture is not a file: {}", path.display()));
    }
    std::fs::canonicalize(path).map_err(|error| format!("could not resolve {}: {error}", path.display()))
}

fn render(doc: &Document) -> (Pixmap, Vec<FontSubstitution>) {
    let substitutions = doc.font_substitutions();
    (doc.flatten(), substitutions)
}

fn image(source: &Source) -> Result<(Pixmap, Vec<FontSubstitution>), String> {
    if let Some(doc) = &source.open_document {
        return Ok(render(doc));
    }
    let edits = source.path.with_extension(jiffshot_capture::library::EDITS_EXT);
    match std::fs::symlink_metadata(&edits) {
        Ok(_) => return io::load_document(&edits).map(|doc| render(&doc)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("could not inspect {}: {error}", edits.display())),
    }
    let mut doc = Document::new(io::load_image(&source.path)?);
    let added = crate::app::try_add_cursor_object(&mut doc, &source.path)?;
    if !added {
        let (pointer, placement) = jiffshot_capture::library::cursor_paths(&source.path);
        match (std::fs::symlink_metadata(&pointer), std::fs::symlink_metadata(&placement)) {
            (Ok(_), Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!("the editable pointer is missing its placement: {}", placement.display()));
            }
            (Err(error), _) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(format!("could not inspect {}: {error}", pointer.display()));
            }
            (_, Err(error)) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(format!("could not inspect {}: {error}", placement.display()));
            }
            _ => {}
        }
    }
    Ok(render(&doc))
}

/// Builds a snapshot before changing the pasteboard. Temporary images remain
/// private and are kept only after a successful copy.
pub(crate) fn prepare(sources: Vec<Source>) -> Result<Prepared, String> {
    if sources.is_empty() {
        return Err("no captures to copy".into());
    }
    if let [source] = sources.as_slice() {
        checked_file(&source.path)?;
        if !video(&source.path) {
            let (image, font_substitutions) = image(source)?;
            let frame = crate::winutil::frame_from_pixmap(&image);
            return Ok(Prepared { content: Content::Image(frame), exports: Exports::default(), font_substitutions });
        }
    }
    prepare_files(&sources)
}

pub(crate) fn prepare_drag(sources: Vec<Source>) -> Result<Prepared, String> {
    prepare_files(&sources)
}

fn prepare_files(sources: &[Source]) -> Result<Prepared, String> {
    if sources.is_empty() {
        return Err("no captures to export".into());
    }
    let mut exports = Exports::default();
    let mut files = Vec::with_capacity(sources.len());
    let mut names = HashSet::new();
    let mut font_substitutions = Vec::new();
    let mut seen_fonts = HashSet::new();
    for source in sources {
        let canonical = checked_file(&source.path)?;
        if video(&source.path) {
            files.push(canonical);
            continue;
        }
        let stem = source.path.file_stem().map(|name| name.to_string_lossy().into_owned()).filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Capture".to_owned());
        let mut name = stem.clone();
        let mut suffix = 2u64;
        while !names.insert(name.to_lowercase()) {
            name = format!("{stem} ({suffix})");
            suffix = suffix.checked_add(1).ok_or("too many captures with the same name")?;
        }
        let path = crate::share::temporary_export_path(&name, "png")?;
        exports.paths.push(path.clone());
        let (image, substitutions) = image(source)?;
        for substitution in substitutions {
            let key = (substitution.requested.to_ascii_lowercase(), substitution.bold, substitution.italic);
            if seen_fonts.insert(key) {
                font_substitutions.push(substitution);
            }
        }
        let bytes = io::encode(&image, io::Format::Png, 0)?;
        std::fs::write(&path, bytes).map_err(|error| format!("could not prepare {}: {error}", path.display()))?;
        files.push(path);
    }
    Ok(Prepared { content: Content::Files(files), exports, font_substitutions })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiffshot_doc::Rgba;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "jiffshot-library-copy-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn source(path: &Path) -> Source {
        Source { path: path.to_path_buf(), open_document: None }
    }

    fn save_image(path: &Path, width: u32, height: u32) {
        let mut frame = Frame::new(width, height);
        for pixel in frame.pixels.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[30, 40, 50, 255]);
        }
        frame.save_png(path).unwrap();
    }

    #[test]
    fn one_image_uses_saved_edits_or_its_editable_pointer() {
        let root = Scratch::new();
        let path = root.0.join("capture.png");
        save_image(&path, 4, 3);
        let mut pointer = Frame::new(1, 1);
        pointer.pixels.copy_from_slice(&[0, 0, 255, 255]);
        jiffshot_capture::library::write_cursor(&path, &pointer, (1, 1)).unwrap();
        let prepared = prepare(vec![source(&path)]).unwrap();
        let Content::Image(frame) = &prepared.content else { panic!("expected image pixels") };
        assert_eq!((frame.width, frame.height), (4, 3));
        assert_eq!(frame.rgb(0, 0), Some((50, 40, 30)));
        assert_eq!(frame.rgb(1, 1), Some((255, 0, 0)));

        let edits = path.with_extension("jsd");
        io::save_document(&Document::blank(7, 5, Rgba::WHITE), &edits).unwrap();
        let prepared = prepare(vec![source(&path)]).unwrap();
        let Content::Image(frame) = &prepared.content else { panic!("expected saved edits") };
        assert_eq!((frame.width, frame.height), (7, 5));

        std::fs::write(&edits, b"not a document").unwrap();
        assert!(prepare(vec![source(&path)]).is_err(), "corrupt edits must not be silently omitted");
        let open = Source { path: path.clone(), open_document: Some(Document::blank(9, 6, Rgba::WHITE)) };
        let prepared = prepare(vec![open]).unwrap();
        let Content::Image(frame) = &prepared.content else { panic!("expected the open document") };
        assert_eq!((frame.width, frame.height), (9, 6));
    }

    #[test]
    fn copy_and_drag_snapshots_keep_font_substitution_notices() {
        use jiffshot_doc::object::{Kind, ShapeKind, Style};
        use jiffshot_doc::{Box2, Pt};

        let root = Scratch::new();
        let first = root.0.join("saved.png");
        let second = root.0.join("open.png");
        save_image(&first, 120, 80);
        save_image(&second, 120, 80);
        let missing = "Jiffshot deliberately unavailable transfer face f3a664";
        let style = Style { font: missing.into(), italic: true, ..Style::default() };
        let mut saved = Document::blank(120, 80, Rgba::WHITE);
        saved.add(Kind::Text { rect: Box2::new(0.0, 0.0, 100.0, 35.0), text: "Saved".into() }, style.clone());
        io::save_document(&saved, &first.with_extension("jsd")).unwrap();

        let single = prepare(vec![source(&first)]).unwrap();
        assert_eq!(single.font_substitutions().len(), 1);
        assert_eq!(single.font_substitutions()[0].requested, missing);
        assert_eq!(single.font_substitutions()[0].replacement.as_ref().map(|face| face.family.as_str()), Some("Helvetica Neue"));

        let mut open = saved.clone();
        open.add(Kind::Step { center: Pt::new(80.0, 55.0), radius: 15.0, label: "1".into(), shape: ShapeKind::Ellipse }, style);
        let sources = || vec![source(&first), Source { path: second.clone(), open_document: Some(open.clone()) }];
        for prepared in [prepare(sources()).unwrap(), prepare_drag(sources()).unwrap()] {
            let substitutions = prepared.font_substitutions();
            assert_eq!(substitutions.len(), 2, "duplicate faces from multiple sources are reported once");
            assert_eq!((substitutions[0].bold, substitutions[0].italic), (false, true));
            assert_eq!((substitutions[1].bold, substitutions[1].italic), (true, false));
            assert!(substitutions.iter().all(|font| font.requested == missing
                && font.replacement.as_ref().is_some_and(|face| face.family == "Helvetica Neue")));
        }
        assert_eq!(io::load_document(&first.with_extension("jsd")).unwrap().objects[0].style.font, missing);
    }

    #[test]
    fn malformed_pointer_or_missing_capture_fails() {
        let root = Scratch::new();
        let path = root.0.join("capture.png");
        save_image(&path, 3, 3);
        std::fs::write(path.with_extension("cursor.png"), b"orphan").unwrap();
        assert!(prepare(vec![source(&path)]).err().unwrap().contains("missing its placement"));
        std::fs::write(path.with_extension("cursor.json"), b"invalid").unwrap();
        assert!(prepare(vec![source(&path)]).err().unwrap().contains("invalid editable pointer"));
        std::fs::remove_file(&path).unwrap();
        assert!(prepare(vec![source(&path)]).is_err());
    }

    #[test]
    fn multiple_captures_are_snapshots_and_failed_or_abandoned_exports_are_removed() {
        let root = Scratch::new();
        let first = root.0.join("one");
        let second = root.0.join("two");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let first = first.join("same.png");
        let second = second.join("same.png");
        let movie = root.0.join("clip.mp4");
        save_image(&first, 4, 3);
        save_image(&second, 5, 2);
        std::fs::write(&movie, b"original video").unwrap();
        io::save_document(&Document::blank(8, 6, Rgba::WHITE), &second.with_extension("jsd")).unwrap();
        let prepared = prepare(vec![source(&first), source(&second), source(&movie)]).unwrap();
        let Content::Files(files) = &prepared.content else { panic!("expected file URLs") };
        assert_eq!(files.len(), 3);
        assert_ne!(files[0].file_name(), files[1].file_name());
        assert_eq!((io::load_image(&files[0]).unwrap().width(), io::load_image(&files[1]).unwrap().width()), (4, 8));
        assert_eq!(files[2], std::fs::canonicalize(&movie).unwrap());
        let staged = files[..2].to_vec();
        drop(prepared);
        assert!(staged.iter().all(|path| !path.exists() && !path.parent().unwrap().exists()));
        assert_eq!(std::fs::read(&movie).unwrap(), b"original video");
        assert!(first.exists() && second.exists() && second.with_extension("jsd").exists());
    }

    #[test]
    fn a_single_file_export_includes_unsaved_edits_without_changing_the_capture() {
        let root = Scratch::new();
        let path = root.0.join("capture.png");
        save_image(&path, 4, 3);
        let sources = [Source {
            path: path.clone(),
            open_document: Some(Document::blank(9, 6, Rgba::WHITE)),
        }];
        let prepared = prepare_files(&sources).unwrap();
        let Content::Files(files) = &prepared.content else { panic!("expected a file URL") };
        assert_eq!(files.len(), 1);
        assert_ne!(files[0], path);
        assert_eq!(io::load_image(&files[0]).unwrap().width(), 9);
        assert_eq!(io::load_image(&path).unwrap().width(), 4);
        let exported = files[0].clone();
        drop(prepared);
        assert!(!exported.exists() && !exported.parent().unwrap().exists());
    }

    #[test]
    fn drag_exports_a_single_edited_image_as_a_file_url_and_retains_it_on_copy() {
        let root = Scratch::new();
        let path = root.0.join("capture #1.png");
        save_image(&path, 4, 3);
        let mut prepared = prepare_drag(vec![Source {
            path: path.clone(),
            open_document: Some(Document::blank(9, 6, Rgba::WHITE)),
        }]).unwrap();
        let Content::Files(files) = &prepared.content else { panic!("expected file URLs") };
        let exported = files[0].clone();
        let json = prepared.file_urls_json().unwrap();
        assert_eq!(json.last(), Some(&0));
        let urls: Vec<String> = serde_json::from_str(&String::from_utf16(&json[..json.len() - 1]).unwrap()).unwrap();
        assert_eq!(urls.len(), 1);
        assert!(urls[0].starts_with("file:///") && urls[0].contains("%20") && urls[0].contains("%23"));
        assert_eq!((io::load_image(&exported).unwrap().width(), io::load_image(&path).unwrap().width()), (9, 4));
        prepared.retain_for_destination();
        drop(prepared);
        assert!(exported.exists());
        std::fs::remove_file(&exported).unwrap();
        std::fs::remove_dir(exported.parent().unwrap()).unwrap();
    }

    #[test]
    fn drag_file_urls_preserve_non_utf8_paths() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let root = Scratch::new();
        let video = root.0.join(OsString::from_vec(b"clip-\xff.mp4".to_vec()));
        let prepared = Prepared { content: Content::Files(vec![video]), exports: Exports::default(), font_substitutions: Vec::new() };
        let json = prepared.file_urls_json().unwrap();
        let urls: Vec<String> = serde_json::from_str(&String::from_utf16(&json[..json.len() - 1]).unwrap()).unwrap();
        assert_eq!(urls.len(), 1);
        assert!(urls[0].ends_with("clip-%FF.mp4"), "{:?}", urls[0]);
    }

    #[test]
    fn a_later_failure_removes_earlier_file_exports() {
        let root = Scratch::new();
        let name = format!("failed-copy-{}", root.0.file_name().unwrap().to_string_lossy());
        let path = root.0.join(format!("{name}.png"));
        save_image(&path, 4, 3);
        let missing = root.0.join("missing.png");
        let result = prepare_files(&[source(&path), source(&missing)]);
        assert!(result.is_err());
        let leftover = std::fs::read_dir(std::env::temp_dir()).unwrap().any(|entry| {
            let entry = entry.unwrap();
            entry.file_name().to_string_lossy().starts_with(&format!("jiffshot-share-{}-", std::process::id()))
                && entry.path().join(format!("{name}.png")).exists()
        });
        assert!(!leftover, "failed preparation retained an earlier export");
        assert!(path.exists());
    }
}
