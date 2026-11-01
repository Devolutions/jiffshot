//! Share saved captures through desktop portals or a chosen local folder.

use ashpd::desktop::ResponseError;
use std::ffi::OsString;
use std::fs::File;
use std::io::{BufReader, Read, Seek, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub(super) async fn open_with(path: &Path) -> Result<bool, String> {
    let file = open_capture(path)?;
    let response = ashpd::desktop::open_uri::OpenFileRequest::default()
        .ask(true)
        .send_file(&file)
        .await
        .and_then(|request| request.response());
    portal_result(response, "open the capture")
}

pub(super) async fn email(path: &Path) -> Result<bool, String> {
    let file = open_capture(path)?;
    let response = ashpd::desktop::email::EmailRequest::default()
        .subject("Jiffshot capture")
        .attach(file.into())
        .send()
        .await
        .and_then(|request| request.response());
    portal_result(response, "compose an email")
}

pub(super) async fn print_image(path: &Path) -> Result<bool, String> {
    let file = printable_image(path)?;
    let proxy = ashpd::desktop::print::PrintProxy::new()
        .await
        .map_err(|error| format!("could not access the print portal: {error}"))?;
    let response = proxy
        .print(None, "Jiffshot saved capture", &file.as_fd(), None, true)
        .await
        .and_then(|request| request.response());
    portal_result(response, "print the saved image")
}

fn printable_image(path: &Path) -> Result<File, String> {
    let pixmap = decode_printable_image(open_capture(path)?)
        .map_err(|error| format!("could not decode saved image {}: {error}", path.display()))?;
    let pdf = jiffshot_doc::io::pdf(&pixmap, 90)
        .map_err(|error| format!("could not prepare saved image for printing: {error}"))?;
    let mut file =
        tempfile::tempfile().map_err(|error| format!("could not create print file: {error}"))?;
    file.write_all(&pdf)
        .and_then(|_| file.rewind())
        .map_err(|error| format!("could not prepare print file: {error}"))?;
    Ok(file)
}

fn decode_printable_image(source: impl Read + Seek) -> Result<tiny_skia::Pixmap, String> {
    let reader = image::ImageReader::new(BufReader::new(source))
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    let image = reader
        .decode()
        .map_err(|error| error.to_string())?
        .into_rgba8();
    jiffshot_doc::io::from_rgba_image(&image).ok_or_else(|| "image has no pixels".to_owned())
}

fn open_capture(path: &Path) -> Result<File, String> {
    let file =
        File::open(path).map_err(|error| format!("could not open {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    Ok(file)
}

fn portal_result(result: Result<(), ashpd::Error>, action: &str) -> Result<bool, String> {
    match result {
        Ok(()) => Ok(true),
        Err(ashpd::Error::Response(ResponseError::Cancelled)) => Ok(false),
        Err(error) => Err(format!("could not {action}: {error}")),
    }
}

pub(super) fn copy_automatically(source: &Path) -> Result<Option<PathBuf>, String> {
    let directory = crate::linux::settings::automatic_copy_directory()?;
    copy_automatically_to(source, directory.as_deref())
}

fn copy_automatically_to(
    source: &Path,
    directory: Option<&Path>,
) -> Result<Option<PathBuf>, String> {
    let Some(directory) = directory else {
        return Ok(None);
    };
    let source_directory = source
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let source_metadata = std::fs::metadata(source_directory)
        .map_err(|error| format!("could not inspect {}: {error}", source_directory.display()))?;
    let destination_metadata = std::fs::metadata(directory)
        .map_err(|error| format!("could not inspect {}: {error}", directory.display()))?;
    if source_metadata.dev() == destination_metadata.dev()
        && source_metadata.ino() == destination_metadata.ino()
    {
        return Err("the automatic copy folder is the same as the captures folder".to_owned());
    }
    copy_to_folder(source, directory).map(Some)
}

pub(super) fn copy_to_folder(source: &Path, directory: &Path) -> Result<PathBuf, String> {
    let name = source
        .file_name()
        .ok_or_else(|| format!("{} has no file name", source.display()))?;
    let stem = source
        .file_stem()
        .ok_or_else(|| format!("{} has no file name", source.display()))?;
    let mut input = open_capture(source)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory).map_err(|error| {
        format!(
            "could not create a file in {}: {error}",
            directory.display()
        )
    })?;
    std::io::copy(&mut input, &mut temporary)
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|error| format!("could not copy {}: {error}", source.display()))?;

    let mut number = 1_u32;
    loop {
        let target = if number == 1 {
            directory.join(name)
        } else {
            let mut candidate = OsString::from(stem);
            candidate.push(format!(" ({number})"));
            if let Some(extension) = source.extension() {
                candidate.push(".");
                candidate.push(extension);
            }
            directory.join(candidate)
        };
        match temporary.persist_noclobber(&target) {
            Ok(_) => return Ok(target),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                temporary = error.file;
                number = number
                    .checked_add(1)
                    .ok_or_else(|| "could not find an unused destination name".to_owned())?;
            }
            Err(error) => {
                return Err(format!(
                    "could not save {}: {}",
                    target.display(),
                    error.error
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;
    use std::sync::{Arc, Barrier};

    #[test]
    fn folder_copies_keep_existing_files_and_support_non_utf8_names() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let source_dir = directory.path().join("source");
        let target_dir = directory.path().join("target");
        std::fs::create_dir(&source_dir).unwrap();
        std::fs::create_dir(&target_dir).unwrap();
        let name = OsString::from_vec(b"capture-\xff.png".to_vec());
        let source = source_dir.join(&name);
        std::fs::write(&source, b"new capture").unwrap();
        std::fs::write(target_dir.join(&name), b"existing capture").unwrap();
        let second = copy_to_folder(&source, &target_dir).expect("copy should succeed");
        assert_eq!(
            second.file_name().unwrap().as_encoded_bytes(),
            b"capture-\xff (2).png"
        );
        assert_eq!(std::fs::read(second).unwrap(), b"new capture");
        assert_eq!(
            std::fs::read(target_dir.join(name)).unwrap(),
            b"existing capture"
        );
    }

    #[test]
    fn concurrent_folder_copies_never_overwrite_each_other() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let source = directory.path().join("capture.png");
        let destination = directory.path().join("destination");
        std::fs::write(&source, b"capture data").unwrap();
        std::fs::create_dir(&destination).unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let mut threads = Vec::new();
        for _ in 0..8 {
            let source = source.clone();
            let destination = destination.clone();
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                copy_to_folder(&source, &destination).expect("concurrent copy should succeed")
            }));
        }
        let mut outputs: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().expect("copy thread should not panic"))
            .collect();
        outputs.sort();
        outputs.dedup();
        assert_eq!(outputs.len(), 8);
        for path in outputs {
            assert_eq!(std::fs::read(path).unwrap(), b"capture data");
        }
        assert_eq!(std::fs::read_dir(destination).unwrap().count(), 8);
    }

    #[test]
    fn folder_copy_rejects_missing_sources_and_destinations() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let source = directory.path().join("source.png");
        assert!(copy_to_folder(&source, directory.path()).is_err());
        std::fs::write(&source, b"image").unwrap();
        assert!(copy_to_folder(&source, &directory.path().join("missing")).is_err());
        assert!(copy_to_folder(directory.path(), directory.path()).is_err());
    }

    #[test]
    fn automatic_copies_preserve_source_and_existing_files() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let source_folder = directory.path().join("captures");
        let target_folder = directory.path().join("copies");
        std::fs::create_dir(&source_folder).expect("create the captures folder");
        std::fs::create_dir(&target_folder).expect("create the copy folder");
        let source = source_folder.join("capture.png");
        std::fs::write(&source, b"new capture").expect("write the original");
        std::fs::write(target_folder.join("capture.png"), b"existing file")
            .expect("write an existing destination");

        assert_eq!(
            copy_automatically_to(&source, None).expect("disabled copying should succeed"),
            None
        );
        let copied = copy_automatically_to(&source, Some(&target_folder))
            .expect("automatic copying should succeed")
            .expect("a destination should be returned");
        assert_eq!(copied, target_folder.join("capture (2).png"));
        assert_eq!(std::fs::read(&copied).unwrap(), b"new capture");
        assert_eq!(std::fs::read(&source).unwrap(), b"new capture");
        assert_eq!(
            std::fs::read(target_folder.join("capture.png")).unwrap(),
            b"existing file"
        );
    }

    #[test]
    fn automatic_copy_rejects_same_folder_and_unavailable_destinations_without_losing_source() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let source_folder = directory.path().join("captures");
        std::fs::create_dir(&source_folder).expect("create the captures folder");
        let source = source_folder.join("capture.png");
        std::fs::write(&source, b"original").expect("write the original");
        let alias = directory.path().join("captures-alias");
        let missing = directory.path().join("missing");
        std::os::unix::fs::symlink(&source_folder, &alias).expect("create a folder alias");
        for destination in [source_folder.as_path(), alias.as_path(), missing.as_path()] {
            assert!(
                copy_automatically_to(&source, Some(destination)).is_err(),
                "the destination {} must be rejected",
                destination.display()
            );
            assert_eq!(std::fs::read(&source).unwrap(), b"original");
        }
        assert_eq!(std::fs::read_dir(source_folder).unwrap().count(), 1);
    }

    #[test]
    fn print_file_contains_the_saved_image_as_a_single_page_pdf() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join("capture.png");
        let mut image = tiny_skia::Pixmap::new(4, 3).expect("fixture dimensions are valid");
        image.fill(tiny_skia::Color::from_rgba8(30, 120, 190, 255));
        std::fs::write(
            &path,
            jiffshot_doc::io::encode(&image, jiffshot_doc::io::Format::Png, 90)
                .expect("fixture PNG should encode"),
        )
        .expect("fixture should be written");
        let mut file = printable_image(&path).expect("the saved image should be printable");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .expect("print file should be readable");
        assert!(bytes.starts_with(b"%PDF-1.4"));
        assert!(bytes.ends_with(b"%%EOF\n"));
        assert!(bytes
            .windows(b"/Width 4 /Height 3".len())
            .any(|part| part == b"/Width 4 /Height 3"));
        let marker = b"stream\n";
        let start = bytes
            .windows(marker.len())
            .position(|part| part == marker)
            .expect("the PDF should contain an image stream")
            + marker.len();
        let end = bytes[start..]
            .windows(b"\nendstream".len())
            .position(|part| part == b"\nendstream")
            .expect("the image stream should terminate")
            + start;
        let decoded = image::load_from_memory(&bytes[start..end])
            .expect("the PDF image should decode")
            .to_rgb8();
        let pixel = decoded.get_pixel(0, 0).0;
        for (actual, expected) in pixel.into_iter().zip([30_u8, 120, 190]) {
            assert!(
                actual.abs_diff(expected) <= 3,
                "unexpected printed pixel: {pixel:?}"
            );
        }
    }

    #[test]
    fn print_file_rejects_missing_and_non_image_captures() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let path = directory.path().join("capture.mp4");
        assert!(printable_image(&path).is_err());
        std::fs::write(&path, b"not an image").expect("fixture should be written");
        let error = printable_image(&path).expect_err("video is not an image");
        assert!(error.contains("could not decode saved image"), "{error}");
    }

    #[test]
    fn print_decode_rejects_non_images_without_reading_the_whole_file() {
        struct CountingReader {
            data: std::io::Cursor<Vec<u8>>,
            bytes_read: usize,
        }

        impl Read for CountingReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                let count = self.data.read(buffer)?;
                self.bytes_read += count;
                Ok(count)
            }
        }

        impl Seek for CountingReader {
            fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
                self.data.seek(position)
            }
        }

        let mut reader = CountingReader {
            data: std::io::Cursor::new(vec![0; 1024 * 1024]),
            bytes_read: 0,
        };
        assert!(decode_printable_image(&mut reader).is_err());
        assert!(
            reader.bytes_read < 64 * 1024,
            "an unsupported file should not be read into memory: {} bytes read",
            reader.bytes_read
        );
    }

    #[test]
    fn portal_results_distinguish_success_cancellation_and_failure() {
        assert_eq!(portal_result(Ok(()), "share"), Ok(true));
        assert_eq!(
            portal_result(
                Err(ashpd::Error::Response(ResponseError::Cancelled)),
                "share"
            ),
            Ok(false)
        );
        let error = portal_result(Err(ashpd::Error::Response(ResponseError::Other)), "share")
            .expect_err("portal failures must remain errors");
        assert!(error.contains("could not share"), "{error}");
    }
}
