//! The on-disk capture library: one PNG per capture plus a JSON sidecar with
//! where it came from, grouped into monthly folders under
//! `Pictures\Jiffshot`.

use crate::{Frame, Rect};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureKind {
    Region,
    Window,
    Monitor,
    Desktop,
    Scrolling,
    Panoramic,
    Menu,
    Video,
    Steps,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureMeta {
    pub kind: CaptureKind,
    /// Screen rectangle that was captured.
    pub rect: Rect,
    pub width: u32,
    pub height: u32,
    /// Local time, `YYYY-MM-DDTHH:MM:SS`.
    pub captured_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
    pub cursor_included: bool,
    /// The address of the web page, for captures of a browser window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Kept at the front of the tray.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
}

/// The sidecar of a capture (or recording): `<stem>.json`.
pub fn sidecar(capture: &Path) -> PathBuf {
    capture.with_extension("json")
}

/// A capture's metadata; recordings and imported files may have none.
pub fn read_meta(capture: &Path) -> Option<CaptureMeta> {
    read_meta_checked(capture).ok().flatten()
}

/// Reads metadata without treating an unreadable or invalid sidecar as absent.
pub fn read_meta_checked(capture: &Path) -> io::Result<Option<CaptureMeta>> {
    let path = sidecar(capture);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid capture metadata {}: {error}", path.display()),
        )
    })
}

pub fn write_meta(capture: &Path, meta: &CaptureMeta) -> io::Result<()> {
    std::fs::write(
        sidecar(capture),
        serde_json::to_vec_pretty(meta).map_err(io::Error::other)?,
    )
}

/// The extension of a capture's saved edits (a Jiffshot document).
pub const EDITS_EXT: &str = "jsd";

/// A capture's saved edits, if any: the `.jsd` beside it.
pub fn edits_of(capture: &Path) -> Option<PathBuf> {
    Some(capture.with_extension(EDITS_EXT)).filter(|p| p.exists())
}

/// Files that belong with a capture: its sidecar, edits and pointer.
pub fn companions(capture: &Path) -> Vec<PathBuf> {
    let (cursor_image, cursor_metadata) = cursor_paths(capture);
    vec![
        capture.with_extension("json"),
        capture.with_extension(EDITS_EXT),
        cursor_image,
        cursor_metadata,
    ]
}

/// Paths for an editable pointer image and its position metadata.
pub fn cursor_paths(capture: &Path) -> (PathBuf, PathBuf) {
    (
        capture.with_extension("cursor.png"),
        capture.with_extension("cursor.json"),
    )
}

/// Position and optional transformed size of an editable pointer.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CursorPlacement {
    pub x: f64,
    pub y: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<f64>,
}

impl CursorPlacement {
    pub fn pixels((x, y): (i32, i32)) -> Self {
        Self {
            x: f64::from(x),
            y: f64::from(y),
            width: None,
            height: None,
        }
    }

    /// Validated image bounds; legacy sidecars omit the size.
    pub fn geometry(self, (image_width, image_height): (u32, u32)) -> io::Result<[f32; 4]> {
        let (width, height) = match (self.width, self.height) {
            (None, None) => (f64::from(image_width), f64::from(image_height)),
            (Some(width), Some(height)) => (width, height),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "incomplete pointer dimensions",
                ))
            }
        };
        let (x, y, width, height) = (self.x as f32, self.y as f32, width as f32, height as f32);
        if !x.is_finite()
            || !y.is_finite()
            || !width.is_finite()
            || !height.is_finite()
            || width <= 0.0
            || height <= 0.0
            || !(x + width).is_finite()
            || !(y + height).is_finite()
            || x + width <= x
            || y + height <= y
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid pointer bounds",
            ));
        }
        Ok([x, y, width, height])
    }
}

/// Saves an editable cursor without replacing either existing companion.
pub fn write_cursor(capture: &Path, image: &Frame, position: (i32, i32)) -> io::Result<()> {
    write_cursor_at(capture, image, CursorPlacement::pixels(position))
}

fn write_cursor_at(capture: &Path, image: &Frame, position: CursorPlacement) -> io::Result<()> {
    position.geometry((image.width, image.height))?;
    let json = serde_json::to_vec(&position).map_err(io::Error::other)?;
    let (image_path, metadata_path) = cursor_paths(capture);
    let image_file = std::fs::File::options()
        .write(true)
        .create_new(true)
        .open(&image_path)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("could not create {}: {error}", image_path.display()),
            )
        })?;
    let mut metadata_file = match std::fs::File::options()
        .write(true)
        .create_new(true)
        .open(&metadata_path)
    {
        Ok(file) => file,
        Err(error) => {
            drop(image_file);
            let error = io::Error::new(
                error.kind(),
                format!("could not create {}: {error}", metadata_path.display()),
            );
            return Err(cleanup_created_files(error, &[&image_path]));
        }
    };
    let mut image_file = io::BufWriter::with_capacity(1 << 20, image_file);
    let result = image
        .encode_png(&mut image_file)
        .and_then(|()| image_file.flush())
        .and_then(|()| metadata_file.write_all(&json));
    drop(image_file);
    drop(metadata_file);
    result.map_err(|error| cleanup_created_files(error, &[&image_path, &metadata_path]))
}

/// Renames a capture and its companions to `name` (no extension) in the
/// same folder; returns the new path.
pub fn rename(capture: &Path, name: &str) -> io::Result<PathBuf> {
    let name = name.trim();
    if name.is_empty() || name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a file name cannot contain \\ / : * ? \" < > |",
        ));
    }
    let mut target_name = std::ffi::OsString::from(name);
    target_name.push(".");
    if let Some(ext) = capture.extension() {
        target_name.push(ext);
    }
    let target = capture.with_file_name(target_name);
    let target_companions = companions(&target);
    std::fs::symlink_metadata(capture)?;
    for destination in std::iter::once(&target).chain(target_companions.iter()) {
        if path_exists(destination)? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists", destination.display()),
            ));
        }
    }
    let mut files = Vec::new();
    for (source, destination) in companions(capture).into_iter().zip(target_companions) {
        if path_exists(&source)? {
            files.push((source, destination));
        }
    }
    files.push((capture.to_path_buf(), target.clone()));
    move_files_with_rollback(&files, rename_library_exclusively)?;
    Ok(target)
}

fn path_exists(path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn move_files_with_rollback(
    files: &[(PathBuf, PathBuf)],
    mut move_file: impl FnMut(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let mut moved: Vec<(&Path, &Path)> = Vec::new();
    for (source, destination) in files {
        if let Err(error) = move_file(source, destination) {
            let failures: Vec<String> = moved
                .into_iter()
                .rev()
                .filter_map(|(old, new)| {
                    move_file(new, old)
                        .err()
                        .map(|failure| format!("{} -> {}: {failure}", new.display(), old.display()))
                })
                .collect();
            let rollback = if failures.is_empty() {
                String::new()
            } else {
                format!("; could not restore {}", failures.join("; "))
            };
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "could not rename {} to {}: {error}{rollback}",
                    source.display(),
                    destination.display()
                ),
            ));
        }
        moved.push((source, destination));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn rename_library_exclusively(source: &Path, destination: &Path) -> io::Result<()> {
    match renamex_exclusively(source, destination) {
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::ENOSYS)) => {
            std::fs::hard_link(source, destination)?;
            if let Err(error) = std::fs::remove_file(source) {
                return match std::fs::remove_file(destination) {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(io::Error::new(
                        error.kind(),
                        format!(
                            "{error}; could not remove {}: {cleanup}",
                            destination.display()
                        ),
                    )),
                };
            }
            Ok(())
        }
        result => result,
    }
}

#[cfg(windows)]
fn rename_library_exclusively(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileW;

    fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let name = path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "a file name is required")
        })?;
        let mut absolute = std::fs::canonicalize(parent)?;
        absolute.push(name);
        let mut wide: Vec<u16> = absolute.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the file name contains a NUL byte",
            ));
        }
        wide.push(0);
        Ok(wide)
    }

    let source = wide_path(source)?;
    let destination = wide_path(destination)?;
    // SAFETY: both paths are NUL-terminated UTF-16 buffers and stay alive for the call.
    if unsafe { MoveFileW(source.as_ptr(), destination.as_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Local wall-clock time as `(year, month, day, hour, minute, second)`.
pub fn local_time() -> (u16, u16, u16, u16, u16, u16) {
    crate::os::local_time()
}

pub fn timestamp() -> String {
    let (y, mo, d, h, mi, s) = local_time();
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}")
}

pub struct Library {
    pub root: PathBuf,
}

/// A saved image, including a failure to save its optional editable pointer.
#[derive(Debug)]
pub enum SavedCapture {
    Complete(PathBuf),
    CursorFailed { path: PathBuf, error: io::Error },
}

#[cfg(target_os = "macos")]
pub struct StagedRecording {
    path: PathBuf,
    folder: PathBuf,
    stem: String,
    preserve: bool,
}

#[cfg(target_os = "macos")]
impl StagedRecording {
    /// Creates a private staging location beside an exact output destination.
    pub fn for_destination(destination: &Path) -> io::Result<Self> {
        let folder = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the recording destination has no parent",
                )
            })?;
        let stem = destination
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the recording destination has no valid name",
                )
            })?;
        create_recording_stage(folder.to_path_buf(), stem.to_owned())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn remove_staging(&self) -> io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        std::fs::remove_dir(self.path.parent().expect("a staged recording has a parent"))
    }

    /// Discards an explicitly cancelled or restarted recording.
    pub fn discard(mut self) -> io::Result<()> {
        let result = self.remove_staging();
        self.preserve = true;
        result.map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "could not discard temporary recording {}: {error}",
                    self.path.display()
                ),
            )
        })
    }

    /// Publishes a completed recording without replacing an existing file.
    /// On failure, the staged recording is kept for recovery.
    pub fn publish(mut self) -> io::Result<PathBuf> {
        self.preserve = true;
        let publish = || -> io::Result<PathBuf> {
            'candidate: for n in 1u64.. {
                let path = if n == 1 {
                    self.folder.join(format!("{}.mp4", self.stem))
                } else {
                    self.folder.join(format!("{} ({n}).mp4", self.stem))
                };
                for extension in ["json", EDITS_EXT, "cursor.png", "cursor.json"] {
                    match std::fs::symlink_metadata(path.with_extension(extension)) {
                        Ok(_) => continue 'candidate,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                }
                match rename_recording_exclusively(&self.path, &path) {
                    Ok(()) => return Ok(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            unreachable!("recording filename suffixes are exhausted")
        };
        match publish() {
            Ok(path) => {
                let directory = self.path.parent().expect("a staged recording has a parent");
                if let Err(error) = std::fs::remove_dir(directory) {
                    eprintln!("could not remove the recording staging directory {}: {error}", directory.display());
                }
                Ok(path)
            }
            Err(error) => Err(io::Error::new(
                error.kind(),
                format!("could not save the recording; temporary location {} may be recoverable: {error}", self.path.display()),
            )),
        }
    }

    /// Publishes to the requested path without replacing an existing entry.
    /// On failure, the staged recording is kept for recovery.
    pub fn publish_to(mut self, destination: &Path) -> io::Result<PathBuf> {
        self.preserve = true;
        match rename_recording_exclusively(&self.path, destination) {
            Ok(()) => {
                let directory = self.path.parent().expect("a staged recording has a parent");
                if let Err(error) = std::fs::remove_dir(directory) {
                    eprintln!("could not remove the recording staging directory {}: {error}", directory.display());
                }
                Ok(destination.to_path_buf())
            }
            Err(error) => Err(io::Error::new(
                error.kind(),
                format!("could not save the recording to {}; temporary location {} may be recoverable: {error}",
                    destination.display(), self.path.display()),
            )),
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for StagedRecording {
    fn drop(&mut self) {
        if !self.preserve {
            if let Err(error) = self.remove_staging() {
                eprintln!(
                    "could not remove the incomplete recording {}: {error}",
                    self.path.display()
                );
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn rename_recording_exclusively(source: &Path, destination: &Path) -> io::Result<()> {
    match renamex_exclusively(source, destination) {
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::ENOSYS)) => {
            link_recording_exclusively(source, destination)
        }
        result => result,
    }
}

#[cfg(target_os = "macos")]
fn renamex_exclusively(source: &Path, destination: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = |path: &Path| {
        CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "the path contains a NUL byte")
        })
    };
    let from = c_path(source)?;
    let to = c_path(destination)?;
    // SAFETY: both paths are NUL-terminated and remain alive for the call.
    if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) } == 0 {
        return Ok(());
    }
    Err(io::Error::last_os_error())
}

#[cfg(target_os = "macos")]
fn link_recording_exclusively(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::hard_link(source, destination)?;
    if let Err(error) = std::fs::remove_file(source) {
        eprintln!(
            "recording was saved, but its staging file {} could not be removed: {error}",
            source.display()
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn create_recording_stage(folder: PathBuf, stem: String) -> io::Result<StagedRecording> {
    use std::os::unix::fs::DirBuilderExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_STAGE: AtomicU64 = AtomicU64::new(1);
    std::fs::create_dir_all(&folder)?;
    loop {
        let id = NEXT_STAGE
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| io::Error::other("too many recording staging directories"))?;
        let directory = folder.join(format!(".jiffshot-recording-{}-{id}", std::process::id()));
        match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {
                return Ok(StagedRecording {
                    path: directory.join("recording.mp4"),
                    folder,
                    stem,
                    preserve: false,
                })
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

fn cleanup_created_files(error: io::Error, paths: &[&Path]) -> io::Error {
    let failures = paths
        .iter()
        .filter_map(|path| {
            std::fs::remove_file(path)
                .err()
                .filter(|failure| failure.kind() != io::ErrorKind::NotFound)
                .map(|failure| format!("{}: {failure}", path.display()))
        })
        .collect::<Vec<_>>();
    if failures.is_empty() {
        error
    } else {
        io::Error::new(
            error.kind(),
            format!(
                "{error}; could not remove incomplete files: {}",
                failures.join(", ")
            ),
        )
    }
}

impl Library {
    pub fn new(root: PathBuf) -> Library {
        Library { root }
    }

    /// `Pictures\Jiffshot` (following OneDrive or other folder redirection
    /// on Windows), `~/Pictures/Jiffshot` on macOS.
    pub fn default_root() -> PathBuf {
        crate::os::pictures_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(crate::paths::NAME)
    }

    /// Saves a capture and its sidecar; returns the PNG path.
    pub fn save(&self, frame: &Frame, meta: &CaptureMeta) -> io::Result<PathBuf> {
        self.save_at(frame, meta, local_time())
    }

    /// Saves the image even if its optional pointer cannot be saved. In that
    /// case `CursorFailed` describes the failure and metadata excludes it.
    pub fn save_with_cursor(
        &self,
        frame: &Frame,
        meta: &CaptureMeta,
        cursor: Option<(&Frame, CursorPlacement)>,
    ) -> io::Result<SavedCapture> {
        self.save_at_with_cursor(frame, meta, cursor, local_time())
    }

    /// Creates a private staging location for a recording in this library.
    #[cfg(target_os = "macos")]
    pub fn stage_recording(&self) -> io::Result<StagedRecording> {
        self.stage_recording_at(local_time())
    }

    #[cfg(target_os = "macos")]
    fn stage_recording_at(
        &self,
        (y, mo, d, h, mi, s): (u16, u16, u16, u16, u16, u16),
    ) -> io::Result<StagedRecording> {
        let folder = self.root.join(format!("{y:04}-{mo:02}"));
        let stem = format!("Recording {y:04}-{mo:02}-{d:02} {h:02}{mi:02}{s:02}");
        create_recording_stage(folder, stem)
    }

    fn save_at(
        &self,
        frame: &Frame,
        meta: &CaptureMeta,
        (y, mo, d, h, mi, s): (u16, u16, u16, u16, u16, u16),
    ) -> io::Result<PathBuf> {
        match self.save_at_with_cursor(frame, meta, None, (y, mo, d, h, mi, s))? {
            SavedCapture::Complete(path) => Ok(path),
            SavedCapture::CursorFailed { path, error } => Err(io::Error::new(
                error.kind(),
                format!(
                    "capture saved to {}, but its pointer could not be saved: {error}",
                    path.display()
                ),
            )),
        }
    }

    fn save_at_with_cursor(
        &self,
        frame: &Frame,
        meta: &CaptureMeta,
        cursor: Option<(&Frame, CursorPlacement)>,
        (y, mo, d, h, mi, s): (u16, u16, u16, u16, u16, u16),
    ) -> io::Result<SavedCapture> {
        let folder = self.root.join(format!("{y:04}-{mo:02}"));
        std::fs::create_dir_all(&folder)?;
        let stem = format!("Capture {y:04}-{mo:02}-{d:02} {h:02}{mi:02}{s:02}");
        let mut metadata_with_cursor = meta.clone();
        if cursor.is_some() {
            metadata_with_cursor.cursor_included = true;
        }
        let json = serde_json::to_vec_pretty(&metadata_with_cursor).map_err(io::Error::other)?;
        let json_without_cursor = if cursor.is_some() {
            metadata_with_cursor.cursor_included = false;
            serde_json::to_vec_pretty(&metadata_with_cursor).map_err(io::Error::other)?
        } else {
            json.clone()
        };
        'candidate: for n in 1u64.. {
            let path = if n == 1 {
                folder.join(format!("{stem}.png"))
            } else {
                folder.join(format!("{stem} ({n}).png"))
            };
            for extension in [EDITS_EXT, "cursor.png", "cursor.json"] {
                if path_exists(&path.with_extension(extension))? {
                    continue 'candidate;
                }
            }
            let image = match std::fs::File::options()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
            let sidecar = path.with_extension("json");
            let mut metadata = match std::fs::File::options()
                .write(true)
                .create_new(true)
                .open(&sidecar)
            {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    drop(image);
                    std::fs::remove_file(&path)?;
                    continue;
                }
                Err(error) => {
                    drop(image);
                    return Err(cleanup_created_files(error, &[&path]));
                }
            };
            let mut image = io::BufWriter::with_capacity(1 << 20, image);
            let outcome = frame.encode_png(&mut image).and_then(|()| image.flush());
            drop(image);
            if let Err(error) = outcome {
                drop(metadata);
                return Err(cleanup_created_files(error, &[&path, &sidecar]));
            }
            let cursor_error =
                cursor.and_then(|(image, position)| write_cursor_at(&path, image, position).err());
            let saved_json = if cursor_error.is_some() {
                &json_without_cursor
            } else {
                &json
            };
            let metadata_result = metadata.write_all(saved_json);
            drop(metadata);
            if let Err(error) = metadata_result {
                let cursor_paths =
                    (cursor.is_some() && cursor_error.is_none()).then(|| cursor_paths(&path));
                let mut created = vec![path.as_path(), sidecar.as_path()];
                if let Some((image, metadata)) = &cursor_paths {
                    created.extend([image.as_path(), metadata.as_path()]);
                }
                return Err(cleanup_created_files(error, &created));
            }
            return Ok(match cursor_error {
                Some(error) => SavedCapture::CursorFailed { path, error },
                None => SavedCapture::Complete(path),
            });
        }
        unreachable!("capture filename suffixes are exhausted")
    }

    /// A new, unused path for a screen recording.
    pub fn recording_path(&self) -> io::Result<PathBuf> {
        self.recording_path_at(local_time())
    }

    fn recording_path_at(
        &self,
        (y, mo, d, h, mi, s): (u16, u16, u16, u16, u16, u16),
    ) -> io::Result<PathBuf> {
        let folder = self.root.join(format!("{y:04}-{mo:02}"));
        std::fs::create_dir_all(&folder)?;
        let stem = format!("Recording {y:04}-{mo:02}-{d:02} {h:02}{mi:02}{s:02}");
        let mut path = folder.join(format!("{stem}.mp4"));
        for n in 2.. {
            #[cfg(target_os = "macos")]
            let available = {
                let mut available = !path_exists(&path)?;
                for companion in companions(&path) {
                    if path_exists(&companion)? {
                        available = false;
                        break;
                    }
                }
                available
            };
            #[cfg(not(target_os = "macos"))]
            let available = !path.exists();
            if available {
                break;
            }
            path = folder.join(format!("{stem} ({n}).mp4"));
        }
        Ok(path)
    }

    /// Captures in the library (images and recordings), newest first.
    pub fn recent(&self, limit: usize) -> Vec<PathBuf> {
        let mut items: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|month| month.path().is_dir())
            .flat_map(|month| {
                std::fs::read_dir(month.path())
                    .into_iter()
                    .flatten()
                    .flatten()
            })
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("png") || e.eq_ignore_ascii_case("mp4"))
            })
            .filter(|entry| {
                !entry.path().to_string_lossy().ends_with(".thumb.png")
                    && !entry.path().to_string_lossy().ends_with(".cursor.png")
            })
            .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
            .collect();
        items.sort_by(|a, b| b.0.cmp(&a.0));
        items
            .into_iter()
            .take(limit)
            .map(|(_, path)| path)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    fn recording_test_root(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("jiffshot-{name}-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        root
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn staged_recording_preserves_existing_files_and_companions() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let root = recording_test_root("recording-collision");
        let library = Library::new(root.clone());
        let staging = library.stage_recording_at((2000, 1, 2, 3, 4, 5)).unwrap();
        let directory = staging.path().parent().unwrap().to_path_buf();
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o077,
            0
        );
        std::fs::write(staging.path(), b"new recording").unwrap();
        let folder = root.join("2000-01");
        let path = |n: u32| {
            let suffix = if n == 1 {
                String::new()
            } else {
                format!(" ({n})")
            };
            folder.join(format!("Recording 2000-01-02 030405{suffix}.mp4"))
        };
        let existing = path(1);
        let orphan_meta = path(2).with_extension("json");
        let orphan_edits = path(3).with_extension(EDITS_EXT);
        let orphan_cursor_image = path(4).with_extension("cursor.png");
        let orphan_cursor_meta = path(5).with_extension("cursor.json");
        for item in [
            &existing,
            &orphan_meta,
            &orphan_edits,
            &orphan_cursor_image,
            &orphan_cursor_meta,
        ] {
            std::fs::write(item, b"existing").unwrap();
        }
        std::fs::create_dir(path(6)).unwrap();
        symlink("missing-target", path(7)).unwrap();

        let published = staging.publish().unwrap();
        assert_eq!(published, path(8));
        assert_eq!(std::fs::read(&published).unwrap(), b"new recording");
        for item in [
            &existing,
            &orphan_meta,
            &orphan_edits,
            &orphan_cursor_image,
            &orphan_cursor_meta,
        ] {
            assert_eq!(std::fs::read(item).unwrap(), b"existing");
        }
        assert!(path(6).is_dir());
        assert!(std::fs::symlink_metadata(path(7))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!directory.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn concurrent_recordings_publish_without_collisions() {
        let root = recording_test_root("recording-concurrent");
        let library = Library::new(root.clone());
        let barrier = std::sync::Barrier::new(24);
        let recordings = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..24u8)
                .map(|id| {
                    let library = &library;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        let staging = library.stage_recording_at((2000, 1, 2, 3, 4, 5)).unwrap();
                        std::fs::write(staging.path(), [id]).unwrap();
                        barrier.wait();
                        (id, staging.publish().unwrap())
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        let unique: std::collections::HashSet<_> =
            recordings.iter().map(|(_, path)| path).collect();
        assert_eq!(unique.len(), recordings.len());
        for (id, path) in recordings {
            assert_eq!(std::fs::read(path).unwrap(), [id]);
        }
        let entries = std::fs::read_dir(root.join("2000-01"))
            .unwrap()
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            entries.len(),
            24,
            "published recordings must not leave staging directories"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn staged_recordings_clean_up_or_remain_recoverable() {
        let root = recording_test_root("recording-staging");
        let library = Library::new(root.clone());
        let staging = library.stage_recording_at((2000, 1, 2, 3, 4, 5)).unwrap();
        let directory = staging.path().parent().unwrap().to_path_buf();
        std::fs::write(staging.path(), b"cancelled").unwrap();
        staging.discard().unwrap();
        assert!(!directory.exists());

        let staging = library.stage_recording_at((2000, 1, 2, 3, 4, 5)).unwrap();
        let directory = staging.path().parent().unwrap().to_path_buf();
        drop(staging);
        assert!(!directory.exists());

        let mut staging = library.stage_recording_at((2000, 1, 2, 3, 4, 5)).unwrap();
        let recoverable = staging.path().to_path_buf();
        std::fs::write(&recoverable, b"recoverable").unwrap();
        staging.stem.push('\0');
        let error = staging.publish().unwrap_err();
        assert!(error.to_string().contains("temporary location"));
        assert_eq!(std::fs::read(&recoverable).unwrap(), b"recoverable");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn exact_recording_destination_is_private_and_never_replaces_files_or_links() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let root = recording_test_root("recording-exact");
        let destination = root.join("output.mp4");
        let staging = StagedRecording::for_destination(&destination).unwrap();
        let directory = staging.path().parent().unwrap().to_path_buf();
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o077,
            0
        );
        std::fs::write(staging.path(), b"first").unwrap();
        assert_eq!(staging.publish_to(&destination).unwrap(), destination);
        assert!(!directory.exists());
        assert_eq!(std::fs::read(&destination).unwrap(), b"first");

        let staging = StagedRecording::for_destination(&destination).unwrap();
        let recoverable = staging.path().to_path_buf();
        std::fs::write(&recoverable, b"second").unwrap();
        let error = staging.publish_to(&destination).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(error
            .to_string()
            .contains(&recoverable.display().to_string()));
        assert_eq!(std::fs::read(&destination).unwrap(), b"first");
        assert_eq!(std::fs::read(&recoverable).unwrap(), b"second");

        let link = root.join("link.mp4");
        symlink("missing", &link).unwrap();
        let staging = StagedRecording::for_destination(&link).unwrap();
        let recoverable_link = staging.path().to_path_buf();
        std::fs::write(&recoverable_link, b"third").unwrap();
        assert_eq!(
            staging.publish_to(&link).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&recoverable_link).unwrap(), b"third");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn suggested_recording_path_avoids_existing_companions_and_dangling_links() {
        use std::os::unix::fs::symlink;

        let root = recording_test_root("recording-suggested");
        let library = Library::new(root.clone());
        let time = (2000, 1, 2, 3, 4, 5);
        let first = library.recording_path_at(time).unwrap();
        symlink("missing", &first).unwrap();
        let second = library.recording_path_at(time).unwrap();
        std::fs::write(second.with_extension("json"), b"orphaned metadata").unwrap();
        let third = library.recording_path_at(time).unwrap();
        assert!(third.file_name().unwrap().to_string_lossy().contains("(3)"));
        assert!(!third.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn recording_link_fallback_never_replaces_a_destination() {
        let root = recording_test_root("recording-link");
        let source = root.join("source");
        let destination = root.join("destination");
        std::fs::write(&source, b"new").unwrap();
        std::fs::write(&destination, b"existing").unwrap();
        assert_eq!(
            link_recording_exclusively(&source, &destination)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&source).unwrap(), b"new");
        assert_eq!(std::fs::read(&destination).unwrap(), b"existing");
        std::fs::remove_file(&destination).unwrap();
        link_recording_exclusively(&source, &destination).unwrap();
        assert!(!source.exists());
        assert_eq!(std::fs::read(&destination).unwrap(), b"new");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn save_writes_png_and_sidecar_without_overwriting() {
        let root =
            std::env::temp_dir().join(format!("jiffshot-library-test-{}", std::process::id()));
        let library = Library::new(root.clone());
        let frame = Frame::new(4, 3);
        let meta = CaptureMeta {
            kind: CaptureKind::Region,
            rect: Rect::new(1, 2, 4, 3),
            width: 4,
            height: 3,
            captured_at: timestamp(),
            window_title: None,
            process: None,
            cursor_included: false,
            url: None,
            tags: vec!["docs".into()],
            pinned: false,
        };
        let first = library.save(&frame, &meta).unwrap();
        let second = library.save(&frame, &meta).unwrap();
        assert_ne!(first, second);
        assert!(first.with_extension("json").exists());
        let recent = library.recent(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(read_meta(&first).unwrap().tags, ["docs"]);
        std::fs::write(first.with_extension("jsd"), b"{}").unwrap();
        let renamed = rename(&first, "Settings page").unwrap();
        assert!(renamed.ends_with("Settings page.png") && renamed.exists());
        assert!(renamed.with_extension("json").exists() && renamed.with_extension("jsd").exists());
        assert!(!first.exists() && !first.with_extension("jsd").exists());
        assert_eq!(edits_of(&renamed), Some(renamed.with_extension("jsd")));
        assert!(rename(&renamed, "a/b").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checked_metadata_read_distinguishes_missing_and_invalid_sidecars() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "jiffshot-library-meta-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let capture = root.join("capture.png");
        std::fs::write(&capture, b"image").unwrap();
        assert!(read_meta_checked(&capture).unwrap().is_none());
        std::fs::write(sidecar(&capture), b"{invalid").unwrap();
        let error = read_meta_checked(&capture).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("invalid capture metadata"));
        assert_eq!(std::fs::read(sidecar(&capture)).unwrap(), b"{invalid");
        assert!(read_meta(&capture).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rename_rejects_existing_companions_without_overwriting() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "jiffshot-library-rename-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let source = root.join("original.png");
        let target = root.join("renamed.png");
        std::fs::write(&source, b"source image").unwrap();
        for extension in ["json", "jsd", "cursor.png", "cursor.json"] {
            std::fs::write(source.with_extension(extension), extension.as_bytes()).unwrap();
        }
        for extension in ["json", "jsd", "cursor.png", "cursor.json"] {
            let conflict = target.with_extension(extension);
            std::fs::write(&conflict, b"unrelated file").unwrap();
            assert_eq!(
                rename(&source, "renamed").unwrap_err().kind(),
                io::ErrorKind::AlreadyExists
            );
            assert_eq!(std::fs::read(&source).unwrap(), b"source image");
            for original_extension in ["json", "jsd", "cursor.png", "cursor.json"] {
                assert_eq!(
                    std::fs::read(source.with_extension(original_extension)).unwrap(),
                    original_extension.as_bytes()
                );
            }
            assert_eq!(std::fs::read(&conflict).unwrap(), b"unrelated file");
            assert!(!target.exists());
            std::fs::remove_file(conflict).unwrap();
        }
        std::fs::remove_file(source.with_extension("cursor.json")).unwrap();
        std::fs::write(target.with_extension("cursor.json"), b"unrelated file").unwrap();
        assert_eq!(
            rename(&source, "renamed").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            std::fs::read(target.with_extension("cursor.json")).unwrap(),
            b"unrelated file"
        );
        assert!(source.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cursor_and_companion_paths_preserve_non_utf8_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let source = PathBuf::from(OsString::from_vec(b"folder-\xff/capture-\xfe.png".to_vec()));
        assert_eq!(
            companions(&source),
            ["json", "jsd", "cursor.png", "cursor.json"]
                .into_iter()
                .map(|extension| source.with_extension(extension))
                .collect::<Vec<_>>()
        );
        let (image, metadata) = cursor_paths(&source);
        assert_eq!(
            image.as_os_str().as_bytes(),
            b"folder-\xff/capture-\xfe.cursor.png"
        );
        assert_eq!(
            metadata.as_os_str().as_bytes(),
            b"folder-\xff/capture-\xfe.cursor.json"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rename_rejects_dangling_companion_symlinks() {
        use std::os::unix::fs::symlink;

        let root = recording_test_root("rename-symlink");
        let source = root.join("original.png");
        let target = root.join("renamed.png");
        std::fs::write(&source, b"source").unwrap();
        symlink("missing", target.with_extension("jsd")).unwrap();
        assert_eq!(
            rename(&source, "renamed").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&source).unwrap(), b"source");
        assert!(std::fs::symlink_metadata(target.with_extension("jsd"))
            .unwrap()
            .file_type()
            .is_symlink());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rename_rolls_back_companions_after_failure() {
        let root = recording_test_root("rename-rollback");
        let first = root.join("first.json");
        let second = root.join("second.jsd");
        let new_first = root.join("new.json");
        let new_second = root.join("new.jsd");
        std::fs::write(&first, b"metadata").unwrap();
        std::fs::write(&second, b"edits").unwrap();
        let files = vec![
            (first.clone(), new_first.clone()),
            (second.clone(), new_second.clone()),
        ];
        let error = move_files_with_rollback(&files, |from, to| {
            if from == second {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "simulated failure",
                ))
            } else {
                rename_library_exclusively(from, to)
            }
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read(&first).unwrap(), b"metadata");
        assert_eq!(std::fs::read(&second).unwrap(), b"edits");
        assert!(!new_first.exists() && !new_second.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rename_reports_an_unrestorable_companion_without_replacing_it() {
        let root = recording_test_root("rename-rollback-conflict");
        let first = root.join("first.json");
        let second = root.join("second.jsd");
        let new_first = root.join("new.json");
        let new_second = root.join("new.jsd");
        std::fs::write(&first, b"metadata").unwrap();
        std::fs::write(&second, b"edits").unwrap();
        let files = vec![
            (first.clone(), new_first.clone()),
            (second.clone(), new_second),
        ];
        let error = move_files_with_rollback(&files, |from, to| {
            if from == second {
                std::fs::write(&first, b"concurrent file").unwrap();
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "simulated failure",
                ))
            } else {
                rename_library_exclusively(from, to)
            }
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("could not restore"));
        assert!(error
            .to_string()
            .contains(&new_first.to_string_lossy().to_string()));
        assert_eq!(std::fs::read(&first).unwrap(), b"concurrent file");
        assert_eq!(std::fs::read(&new_first).unwrap(), b"metadata");
        assert_eq!(std::fs::read(&second).unwrap(), b"edits");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn concurrent_renames_do_not_replace_other_captures() {
        let root = recording_test_root("rename-concurrent");
        let sources = [root.join("first.png"), root.join("second.png")];
        for (index, source) in sources.iter().enumerate() {
            std::fs::write(source, [index as u8]).unwrap();
            std::fs::write(source.with_extension("json"), [index as u8]).unwrap();
        }
        let barrier = std::sync::Barrier::new(sources.len());
        let results = std::thread::scope(|scope| {
            let workers: Vec<_> = sources
                .iter()
                .enumerate()
                .map(|(index, source)| {
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        (index, rename(source, "shared"))
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        let winner = results
            .iter()
            .find(|(_, result)| result.is_ok())
            .expect("one rename must succeed")
            .0;
        let loser = results
            .iter()
            .find(|(_, result)| result.is_err())
            .expect("one rename must fail");
        assert_eq!(
            loser.1.as_ref().unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            std::fs::read(root.join("shared.png")).unwrap(),
            [winner as u8]
        );
        assert_eq!(
            std::fs::read(root.join("shared.json")).unwrap(),
            [winner as u8]
        );
        assert_eq!(std::fs::read(&sources[loser.0]).unwrap(), [loser.0 as u8]);
        assert_eq!(
            std::fs::read(sources[loser.0].with_extension("json")).unwrap(),
            [loser.0 as u8]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn simultaneous_saves_preserve_each_image_and_sidecar() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "jiffshot-library-concurrent-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let library = Library::new(root.clone());
        let barrier = std::sync::Barrier::new(24);
        let paths = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..24u8)
                .map(|id| {
                    let library = &library;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        let mut frame = Frame::new(2, 2);
                        for pixel in frame.pixels.chunks_exact_mut(4) {
                            pixel[2] = id;
                        }
                        let meta = CaptureMeta {
                            kind: CaptureKind::Region,
                            rect: frame.bounds(),
                            width: frame.width,
                            height: frame.height,
                            captured_at: timestamp(),
                            window_title: None,
                            process: None,
                            cursor_included: false,
                            url: None,
                            tags: vec![id.to_string()],
                            pinned: false,
                        };
                        barrier.wait();
                        (id, library.save(&frame, &meta).unwrap())
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        let unique: std::collections::HashSet<_> = paths.iter().map(|(_, path)| path).collect();
        assert_eq!(
            unique.len(),
            paths.len(),
            "concurrent captures overwrote one another"
        );
        for (id, path) in paths {
            assert_eq!(Frame::load_png(&path).unwrap().rgb(0, 0), Some((id, 0, 0)));
            assert_eq!(read_meta(&path).unwrap().tags, [id.to_string()]);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn save_preserves_existing_sidecars_and_removes_incomplete_files() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "jiffshot-library-reservation-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let folder = root.join("2000-01");
        std::fs::create_dir(&folder).unwrap();
        let sidecar = folder.join("Capture 2000-01-02 030405.json");
        std::fs::write(&sidecar, b"existing metadata").unwrap();
        let library = Library::new(root.clone());
        let frame = Frame::new(2, 2);
        let meta = CaptureMeta {
            kind: CaptureKind::Region,
            rect: frame.bounds(),
            width: frame.width,
            height: frame.height,
            captured_at: timestamp(),
            window_title: None,
            process: None,
            cursor_included: false,
            url: None,
            tags: Vec::new(),
            pinned: false,
        };
        let path = library
            .save_at(&frame, &meta, (2000, 1, 2, 3, 4, 5))
            .unwrap();
        assert!(path.ends_with("Capture 2000-01-02 030405 (2).png"));
        assert_eq!(std::fs::read(&sidecar).unwrap(), b"existing metadata");
        assert!(!folder.join("Capture 2000-01-02 030405.png").exists());
        assert!(library
            .save_at(&Frame::new(0, 0), &meta, (2000, 1, 2, 3, 4, 6))
            .is_err());
        assert!(!folder.join("Capture 2000-01-02 030406.png").exists());
        assert!(!folder.join("Capture 2000-01-02 030406.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn save_skips_orphaned_edits_and_cursor_companions() {
        use std::os::unix::fs::symlink;

        let root = recording_test_root("capture-companion-collision");
        let folder = root.join("2000-01");
        std::fs::create_dir(&folder).unwrap();
        let orphan_edits = folder.join("Capture 2000-01-02 030405.jsd");
        let orphan_cursor = folder.join("Capture 2000-01-02 030405 (2).cursor.png");
        let orphan_link = folder.join("Capture 2000-01-02 030405 (3).cursor.json");
        std::fs::write(&orphan_edits, b"existing edits").unwrap();
        std::fs::write(&orphan_cursor, b"existing pointer").unwrap();
        symlink("missing.json", &orphan_link).unwrap();

        let frame = Frame::new(2, 2);
        let meta = CaptureMeta {
            kind: CaptureKind::Region,
            rect: frame.bounds(),
            width: frame.width,
            height: frame.height,
            captured_at: timestamp(),
            window_title: None,
            process: None,
            cursor_included: false,
            url: None,
            tags: Vec::new(),
            pinned: false,
        };
        let path = Library::new(root.clone())
            .save_at(&frame, &meta, (2000, 1, 2, 3, 4, 5))
            .unwrap();
        assert!(
            path.ends_with("Capture 2000-01-02 030405 (4).png"),
            "{}",
            path.display()
        );
        assert_eq!(std::fs::read(&orphan_edits).unwrap(), b"existing edits");
        assert_eq!(std::fs::read(&orphan_cursor).unwrap(), b"existing pointer");
        assert!(std::fs::symlink_metadata(&orphan_link)
            .unwrap()
            .file_type()
            .is_symlink());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cursor_companions_do_not_overwrite_or_leave_partial_files() {
        use std::os::unix::fs::symlink;

        let root = recording_test_root("cursor-companions");
        let frame = Frame::new(2, 2);
        let image_conflict = root.join("image-conflict.png");
        std::fs::write(&image_conflict, b"saved capture").unwrap();
        std::fs::write(
            image_conflict.with_extension("cursor.png"),
            b"existing pointer",
        )
        .unwrap();
        assert_eq!(
            write_cursor(&image_conflict, &frame, (1, 2))
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            std::fs::read(image_conflict.with_extension("cursor.png")).unwrap(),
            b"existing pointer"
        );
        assert!(!image_conflict.with_extension("cursor.json").exists());
        assert_eq!(std::fs::read(&image_conflict).unwrap(), b"saved capture");

        let metadata_conflict = root.join("metadata-conflict.png");
        std::fs::write(&metadata_conflict, b"saved capture").unwrap();
        symlink(
            "missing.json",
            metadata_conflict.with_extension("cursor.json"),
        )
        .unwrap();
        assert_eq!(
            write_cursor(&metadata_conflict, &frame, (1, 2))
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(!metadata_conflict.with_extension("cursor.png").exists());
        assert!(
            std::fs::symlink_metadata(metadata_conflict.with_extension("cursor.json"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&metadata_conflict).unwrap(), b"saved capture");

        let invalid = root.join("invalid.png");
        std::fs::write(&invalid, b"saved capture").unwrap();
        assert!(write_cursor(&invalid, &Frame::new(0, 0), (1, 2)).is_err());
        assert!(!invalid.with_extension("cursor.png").exists());
        assert!(!invalid.with_extension("cursor.json").exists());
        assert_eq!(std::fs::read(&invalid).unwrap(), b"saved capture");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cursor_save_records_whether_the_pointer_was_saved() {
        let root = recording_test_root("cursor-metadata");
        let library = Library::new(root.clone());
        let frame = Frame::new(4, 3);
        let cursor = Frame::new(2, 2);
        let meta = CaptureMeta {
            kind: CaptureKind::Region,
            rect: frame.bounds(),
            width: frame.width,
            height: frame.height,
            captured_at: timestamp(),
            window_title: None,
            process: None,
            cursor_included: true,
            url: None,
            tags: Vec::new(),
            pinned: false,
        };
        let SavedCapture::Complete(saved) = library
            .save_at_with_cursor(
                &frame,
                &meta,
                Some((&cursor, CursorPlacement::pixels((1, 2)))),
                (2000, 1, 2, 3, 4, 5),
            )
            .unwrap()
        else {
            panic!("valid pointer could not be saved");
        };
        assert!(read_meta(&saved).unwrap().cursor_included);
        assert!(saved.with_extension("cursor.png").exists());
        let position: CursorPlacement =
            serde_json::from_slice(&std::fs::read(saved.with_extension("cursor.json")).unwrap())
                .unwrap();
        assert_eq!(position, CursorPlacement::pixels((1, 2)));

        let SavedCapture::CursorFailed { path: failed, .. } = library
            .save_at_with_cursor(
                &frame,
                &meta,
                Some((&Frame::new(0, 0), CursorPlacement::pixels((1, 2)))),
                (2000, 1, 2, 3, 4, 5),
            )
            .unwrap()
        else {
            panic!("invalid pointer was reported as saved");
        };
        assert!(!read_meta(&failed).unwrap().cursor_included);
        assert!(failed.exists());
        assert!(!failed.with_extension("cursor.png").exists());
        assert!(!failed.with_extension("cursor.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cursor_geometry_accepts_legacy_sidecars_and_validates_bounds() {
        let legacy: CursorPlacement = serde_json::from_str("{\"x\":-1,\"y\":2}").unwrap();
        assert_eq!(legacy.geometry((4, 3)).unwrap(), [-1.0, 2.0, 4.0, 3.0]);
        let scaled: CursorPlacement =
            serde_json::from_str("{\"x\":1.5,\"y\":2.25,\"width\":2.5,\"height\":1.5}").unwrap();
        assert_eq!(scaled.geometry((4, 3)).unwrap(), [1.5, 2.25, 2.5, 1.5]);
        assert!(CursorPlacement {
            width: Some(2.0),
            ..legacy
        }
        .geometry((4, 3))
        .is_err());
        assert!(CursorPlacement {
            width: Some(0.0),
            height: Some(1.0),
            ..legacy
        }
        .geometry((4, 3))
        .is_err());
        assert!(CursorPlacement {
            x: f64::INFINITY,
            ..legacy
        }
        .geometry((4, 3))
        .is_err());
        assert!(legacy.geometry((0, 3)).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn concurrent_cursor_writes_keep_one_complete_pair() {
        let root = recording_test_root("cursor-concurrent");
        let capture = root.join("capture.png");
        let barrier = std::sync::Barrier::new(2);
        let results = std::thread::scope(|scope| {
            let workers: Vec<_> = (1..=2)
                .map(|id| {
                    let capture = &capture;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        let mut frame = Frame::new(1, 1);
                        frame.pixels[2] = id as u8;
                        barrier.wait();
                        (id, write_cursor(capture, &frame, (id, -id)))
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        let winner = results
            .iter()
            .find(|(_, result)| result.is_ok())
            .expect("one cursor write must succeed")
            .0;
        let loser = results
            .iter()
            .find(|(_, result)| result.is_err())
            .expect("one cursor write must fail");
        assert_eq!(
            loser.1.as_ref().unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            Frame::load_png(&capture.with_extension("cursor.png"))
                .unwrap()
                .rgb(0, 0),
            Some((winner as u8, 0, 0))
        );
        let position: CursorPlacement =
            serde_json::from_slice(&std::fs::read(capture.with_extension("cursor.json")).unwrap())
                .unwrap();
        assert_eq!(position, CursorPlacement::pixels((winner, -winner)));
        std::fs::remove_dir_all(root).unwrap();
    }
}
