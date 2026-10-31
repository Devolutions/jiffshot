use std::future::Future;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use ashpd::desktop::file_chooser::{FileFilter, SelectedFiles};
use ashpd::desktop::ResponseError;
use ashpd::{PortalError, WindowIdentifier};
use rustolonia::storage::{
    FileTypeFilter, FolderPickerOptions, OpenFilePickerOptions,
    PickerOutcome as NativePickerOutcome, SaveFilePickerOptions, StorageLocation, WellKnownFolder,
};
use rustolonia::{AppScope, Window};

pub(super) type PickerOperation =
    Pin<Box<dyn Future<Output = Result<PickerOutcome, String>> + Send>>;

pub(super) enum PickerOutcome {
    Selected(Vec<PickedItem>),
    Cancelled,
}

pub(super) struct PickedItem {
    local_path: Option<PathBuf>,
}

impl PickedItem {
    pub(super) fn local_path(&self) -> Option<&Path> {
        self.local_path.as_deref()
    }
}

enum PortalFailure {
    Unavailable(String),
    Failed(String),
}

pub(super) fn open_file_picker(
    scope: &AppScope,
    window: &Window,
    options: &OpenFilePickerOptions,
) -> Result<PickerOperation, String> {
    // Rustolonia's portal picker ignores response codes and can close its
    // D-Bus connection when an unsuccessful response omits "uris".
    let parent = window_identifier(scope, window)?;
    let scope = scope.clone();
    let window = window.clone();
    let options = options.clone();
    Ok(Box::pin(async move {
        let filters = portal_filters(&options.file_types);
        let mut request = SelectedFiles::open_file()
            .identifier(parent)
            .title(options.title.as_deref())
            .modal(true)
            .multiple(options.allow_multiple)
            .current_filter(selected_filter(&filters, options.suggested_file_type)?)
            .filters(filters);
        if let Some(folder) = start_folder(options.suggested_start_location.as_ref()) {
            request = request
                .current_folder(folder)
                .map_err(|error| format!("invalid starting folder: {error}"))?;
        }
        let response = request.send().await.and_then(|request| request.response());
        match portal_response(response, None) {
            Ok(outcome) => Ok(outcome),
            Err(PortalFailure::Failed(error)) => Err(error),
            Err(PortalFailure::Unavailable(error)) => {
                eprintln!("File chooser portal unavailable; using the native picker: {error}");
                let operation = scope
                    .open_file_picker(&window, &options)
                    .map_err(|error| format!("could not open native file picker: {error}"))?;
                operation
                    .await
                    .map(native_outcome)
                    .map_err(|error| format!("native file picker failed: {error}"))
            }
        }
    }))
}

pub(super) fn open_folder_picker(
    scope: &AppScope,
    window: &Window,
    options: &FolderPickerOptions,
) -> Result<PickerOperation, String> {
    let parent = window_identifier(scope, window)?;
    let scope = scope.clone();
    let window = window.clone();
    let options = options.clone();
    Ok(Box::pin(async move {
        let mut request = SelectedFiles::open_file()
            .identifier(parent)
            .title(options.title.as_deref())
            .modal(true)
            .multiple(options.allow_multiple)
            .directory(true);
        if let Some(folder) = start_folder(options.suggested_start_location.as_ref()) {
            request = request
                .current_folder(folder)
                .map_err(|error| format!("invalid starting folder: {error}"))?;
        }
        let response = request.send().await.and_then(|request| request.response());
        match portal_response(response, None) {
            Ok(outcome) => Ok(outcome),
            Err(PortalFailure::Failed(error)) => Err(error),
            Err(PortalFailure::Unavailable(error)) => {
                eprintln!("File chooser portal unavailable; using the native picker: {error}");
                let operation = scope
                    .open_folder_picker(&window, &options)
                    .map_err(|error| format!("could not open native folder picker: {error}"))?;
                operation
                    .await
                    .map(native_outcome)
                    .map_err(|error| format!("native folder picker failed: {error}"))
            }
        }
    }))
}

pub(super) fn save_file_picker(
    scope: &AppScope,
    window: &Window,
    options: &SaveFilePickerOptions,
) -> Result<PickerOperation, String> {
    let parent = window_identifier(scope, window)?;
    let scope = scope.clone();
    let window = window.clone();
    let options = options.clone();
    Ok(Box::pin(async move {
        let filters = portal_filters(&options.file_types);
        let mut request = SelectedFiles::save_file()
            .identifier(parent)
            .title(options.title.as_deref())
            .current_name(options.suggested_file_name.as_deref())
            .modal(true)
            .current_filter(selected_filter(&filters, options.suggested_file_type)?)
            .filters(filters);
        if let Some(folder) = start_folder(options.suggested_start_location.as_ref()) {
            request = request
                .current_folder(folder)
                .map_err(|error| format!("invalid starting folder: {error}"))?;
        }
        let response = request.send().await.and_then(|request| request.response());
        match portal_response(response, options.default_extension.as_deref()) {
            Ok(outcome) => Ok(outcome),
            Err(PortalFailure::Failed(error)) => Err(error),
            Err(PortalFailure::Unavailable(error)) => {
                eprintln!("File chooser portal unavailable; using the native picker: {error}");
                let operation = scope
                    .save_file_picker(&window, &options)
                    .map_err(|error| format!("could not open native save picker: {error}"))?;
                operation
                    .await
                    .map(native_outcome)
                    .map_err(|error| format!("native save picker failed: {error}"))
            }
        }
    }))
}

fn window_identifier(scope: &AppScope, window: &Window) -> Result<WindowIdentifier, String> {
    let handle = scope
        .platform_handle(window)
        .map_err(|error| format!("could not identify the editor window: {error}"))?;
    let xid = std::os::raw::c_ulong::try_from(handle)
        .ok()
        .filter(|xid| *xid != 0)
        .ok_or_else(|| "the editor window has no X11 identifier".to_owned())?;
    Ok(WindowIdentifier::from_xid(xid))
}

fn portal_filters(filters: &[FileTypeFilter]) -> Vec<FileFilter> {
    filters
        .iter()
        .map(|filter| {
            let result = filter
                .patterns
                .iter()
                .fold(FileFilter::new(&filter.name), |result, pattern| {
                    result.glob(pattern)
                });
            filter
                .mime_types
                .iter()
                .fold(result, |result, mime_type| result.mimetype(mime_type))
        })
        .collect()
}

fn selected_filter(
    filters: &[FileFilter],
    selected: Option<usize>,
) -> Result<Option<FileFilter>, String> {
    selected
        .map(|index| {
            filters
                .get(index)
                .cloned()
                .ok_or_else(|| format!("invalid file filter index: {index}"))
        })
        .transpose()
}

fn start_folder(location: Option<&StorageLocation>) -> Option<PathBuf> {
    let path = match location? {
        StorageLocation::Path(path) => Some(path.clone()),
        StorageLocation::Uri(uri) => ashpd::url::Url::parse(uri).ok()?.to_file_path().ok(),
        StorageLocation::WellKnown(folder) => match folder {
            WellKnownFolder::Desktop => dirs::desktop_dir(),
            WellKnownFolder::Documents => dirs::document_dir(),
            WellKnownFolder::Downloads => dirs::download_dir(),
            WellKnownFolder::Music => dirs::audio_dir(),
            WellKnownFolder::Pictures => dirs::picture_dir(),
            WellKnownFolder::Videos => dirs::video_dir(),
        },
    }?;
    if path.is_file() {
        path.parent().map(Path::to_path_buf)
    } else {
        Some(path)
    }
}

fn portal_response(
    response: Result<SelectedFiles, ashpd::Error>,
    default_extension: Option<&str>,
) -> Result<PickerOutcome, PortalFailure> {
    match response {
        Ok(files) => {
            if files.uris().is_empty() {
                return Err(PortalFailure::Failed(
                    "the file chooser returned no selected files".to_owned(),
                ));
            }
            let mut items = files
                .uris()
                .iter()
                .map(|uri| PickedItem {
                    local_path: uri.to_file_path().ok(),
                })
                .collect::<Vec<_>>();
            if let Some(extension) = default_extension {
                for item in &mut items {
                    if let Some(path) = &mut item.local_path {
                        apply_default_extension(path, extension).map_err(PortalFailure::Failed)?;
                    }
                }
            }
            Ok(PickerOutcome::Selected(items))
        }
        Err(ashpd::Error::Response(ResponseError::Cancelled)) => Ok(PickerOutcome::Cancelled),
        Err(error) if portal_unavailable(&error) => {
            Err(PortalFailure::Unavailable(error.to_string()))
        }
        Err(error) => Err(PortalFailure::Failed(format!(
            "desktop file chooser failed: {error}"
        ))),
    }
}

fn apply_default_extension(path: &mut PathBuf, extension: &str) -> Result<(), String> {
    let name = path
        .file_name()
        .ok_or_else(|| "the selected destination has no file name".to_owned())?
        .as_bytes();
    let has_extension = name
        .iter()
        .rposition(|byte| *byte == b'.')
        .is_some_and(|index| index + 1 < name.len());
    if !has_extension && !extension.is_empty() {
        path.set_extension(extension.trim_start_matches('.'));
    }
    Ok(())
}

fn native_outcome(outcome: NativePickerOutcome) -> PickerOutcome {
    match outcome {
        NativePickerOutcome::Selected(items) => PickerOutcome::Selected(
            items
                .iter()
                .map(|item| PickedItem {
                    local_path: item.local_path().map(Path::to_path_buf),
                })
                .collect(),
        ),
        NativePickerOutcome::Cancelled => PickerOutcome::Cancelled,
    }
}

fn portal_unavailable(error: &ashpd::Error) -> bool {
    match error {
        ashpd::Error::PortalNotFound(_) => true,
        ashpd::Error::Zbus(error) | ashpd::Error::Portal(PortalError::ZBus(error)) => {
            bus_unavailable(error)
        }
        _ => false,
    }
}

fn bus_unavailable(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::InterfaceNotFound | zbus::Error::Address(_) => true,
        zbus::Error::InputOutput(error) | zbus::Error::Connection(error, _) => matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        ),
        zbus::Error::MethodError(name, _, _) => matches!(
            name.as_str(),
            "org.freedesktop.DBus.Error.ServiceUnknown"
                | "org.freedesktop.DBus.Error.NameHasNoOwner"
                | "org.freedesktop.DBus.Error.UnknownMethod"
                | "org.freedesktop.DBus.Error.UnknownObject"
                | "org.freedesktop.DBus.Error.UnknownInterface"
        ),
        zbus::Error::FDO(error) => match error.as_ref() {
            zbus::fdo::Error::ServiceUnknown(_)
            | zbus::fdo::Error::NameHasNoOwner(_)
            | zbus::fdo::Error::UnknownMethod(_)
            | zbus::fdo::Error::UnknownObject(_)
            | zbus::fdo::Error::UnknownInterface(_)
            | zbus::fdo::Error::NoServer(_) => true,
            zbus::fdo::Error::ZBus(error) => bus_unavailable(error),
            _ => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    use super::*;

    #[test]
    fn response_status_distinguishes_cancellation_from_other_failures() {
        assert!(matches!(
            portal_response(Err(ashpd::Error::Response(ResponseError::Cancelled)), None),
            Ok(PickerOutcome::Cancelled)
        ));
        match portal_response(Err(ashpd::Error::Response(ResponseError::Other)), None) {
            Err(PortalFailure::Failed(message)) => {
                assert!(message.contains("desktop file chooser failed"), "{message}");
            }
            _ => panic!("a non-cancellation response must be reported as a failure"),
        }
        assert!(matches!(
            native_outcome(NativePickerOutcome::Cancelled),
            PickerOutcome::Cancelled
        ));
    }

    #[test]
    fn native_fallback_is_limited_to_an_unavailable_portal() {
        let absent = ashpd::Error::Zbus(zbus::Error::FDO(Box::new(
            zbus::fdo::Error::ServiceUnknown("not installed".to_owned()),
        )));
        let absent_method = ashpd::Error::Portal(PortalError::ZBus(zbus::Error::FDO(Box::new(
            zbus::fdo::Error::UnknownMethod("not supported".to_owned()),
        ))));
        let denied = ashpd::Error::Zbus(zbus::Error::FDO(Box::new(
            zbus::fdo::Error::AccessDenied("access denied".to_owned()),
        )));
        assert!(portal_unavailable(&absent));
        assert!(portal_unavailable(&absent_method));
        assert!(!portal_unavailable(&denied));
        assert!(!portal_unavailable(&ashpd::Error::Response(
            ResponseError::Other
        )));
        assert!(!portal_unavailable(&ashpd::Error::NoResponse));
    }

    #[test]
    fn portal_filters_preserve_patterns_mime_types_and_selection() {
        let filters = portal_filters(&[
            FileTypeFilter::new("Images")
                .with_extension("png")
                .with_extension("jpg")
                .with_mime_type("image/png"),
            FileTypeFilter::new("Documents").with_extension("pdf"),
        ]);
        assert_eq!(filters[0].label(), "Images");
        assert_eq!(filters[0].pattern_filters(), ["*.png", "*.jpg"]);
        assert_eq!(filters[0].mimetype_filters(), ["image/png"]);
        assert_eq!(selected_filter(&filters, None).unwrap(), None);
        assert_eq!(
            selected_filter(&filters, Some(1)).unwrap(),
            Some(filters[1].clone())
        );
        assert!(selected_filter(&filters, Some(2))
            .expect_err("an out-of-range filter must be rejected")
            .contains('2'));
    }

    #[test]
    fn save_extension_preserves_explicit_and_hidden_file_names() {
        for (input, extension, expected) in [
            ("/tmp/image", "png", "/tmp/image.png"),
            ("/tmp/image.", "png", "/tmp/image.png"),
            ("/tmp/image.jpg", "png", "/tmp/image.jpg"),
            ("/tmp/.hidden", "png", "/tmp/.hidden"),
            (
                "/tmp/folder.name/image",
                ".pdf",
                "/tmp/folder.name/image.pdf",
            ),
            ("/tmp/image", "", "/tmp/image"),
        ] {
            let mut path = PathBuf::from(input);
            apply_default_extension(&mut path, extension).expect("a file name should be accepted");
            assert_eq!(path, PathBuf::from(expected), "input: {input}");
        }
        let mut path = PathBuf::from(OsString::from_vec(b"/tmp/image\xff".to_vec()));
        apply_default_extension(&mut path, "png").expect("non-UTF-8 paths should be accepted");
        assert_eq!(path.as_os_str().as_bytes(), b"/tmp/image\xff.png");
        assert!(apply_default_extension(&mut PathBuf::from("/"), "png").is_err());
    }

    #[test]
    fn local_start_locations_use_a_files_parent_and_ignore_remote_uris() {
        let directory = tempfile::tempdir().expect("a temporary folder should be available");
        let image = directory.path().join("image.png");
        std::fs::write(&image, b"image").expect("the fixture file should be writable");
        assert_eq!(
            start_folder(Some(&StorageLocation::Path(image.clone()))),
            Some(directory.path().to_path_buf())
        );
        let uri = ashpd::url::Url::from_file_path(&image)
            .expect("an absolute path should form a file URI")
            .to_string();
        assert_eq!(
            start_folder(Some(&StorageLocation::Uri(uri))),
            Some(directory.path().to_path_buf())
        );
        assert_eq!(
            start_folder(Some(&StorageLocation::Uri(
                "https://example.test/image.png".to_owned()
            ))),
            None
        );
    }
}
