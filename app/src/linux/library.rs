//! Search saved Linux captures and compatible Jiffshot metadata sidecars.

use serde::Deserialize;
use serde_json::{json, value::RawValue};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureFilter {
    All,
    Images,
    Videos,
    Pinned,
    Tagged,
}

impl CaptureFilter {
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "matching",
            Self::Images => "image",
            Self::Videos => "video",
            Self::Pinned => "pinned",
            Self::Tagged => "tagged",
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct CaptureMetadata {
    #[serde(default)]
    pub(super) window_title: Option<String>,
    #[serde(default)]
    pub(super) process: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    pub(super) captured_at: Option<String>,
    #[serde(default)]
    pub(super) tags: Vec<String>,
    #[serde(default)]
    pub(super) pinned: bool,
}

pub fn search_captures(query: &str, filter: CaptureFilter) -> Result<Vec<PathBuf>, String> {
    search_captures_in(&crate::linux::capture::capture_directory()?, query, filter)
}

fn search_captures_in(
    directory: &Path,
    query: &str,
    filter: CaptureFilter,
) -> Result<Vec<PathBuf>, String> {
    search_captures_in_at(directory, query, filter, SystemTime::now())
}

enum SearchTerm {
    Text(String),
    App(String),
    Site(String),
    Tag(String),
    Age(Duration),
}

fn search_terms(query: &str) -> Result<Vec<SearchTerm>, String> {
    query
        .split_whitespace()
        .map(|word| {
            let word = word.to_lowercase();
            let Some((field, value)) = word.split_once(':') else {
                return Ok(SearchTerm::Text(word));
            };
            match field {
                "app" | "site" | "tag" if value.is_empty() => {
                    Err(format!("the {field}: search qualifier requires a value"))
                }
                "app" => Ok(SearchTerm::App(value.to_owned())),
                "site" => Ok(SearchTerm::Site(value.to_owned())),
                "tag" => Ok(SearchTerm::Tag(value.to_owned())),
                "age" => match value {
                    "day" => Ok(SearchTerm::Age(Duration::from_secs(24 * 60 * 60))),
                    "week" => Ok(SearchTerm::Age(Duration::from_secs(7 * 24 * 60 * 60))),
                    _ => Err("the age: search qualifier must be day or week".to_owned()),
                },
                _ => Ok(SearchTerm::Text(word)),
            }
        })
        .collect()
}

fn search_captures_in_at(
    directory: &Path,
    query: &str,
    filter: CaptureFilter,
    now: SystemTime,
) -> Result<Vec<PathBuf>, String> {
    let terms = search_terms(query)?;
    let mut matches = Vec::new();
    for (path, modified) in capture_files_in(directory)? {
        let is_video = path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("mp4"));
        if (filter == CaptureFilter::Images && is_video)
            || (filter == CaptureFilter::Videos && !is_video)
        {
            continue;
        }
        let metadata = read_metadata(&path)?;
        if (filter == CaptureFilter::Pinned && !metadata.pinned)
            || (filter == CaptureFilter::Tagged && metadata.tags.is_empty())
        {
            continue;
        }
        let mut haystack = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        for value in [
            metadata.window_title.as_deref(),
            metadata.process.as_deref(),
            metadata.url.as_deref(),
            metadata.captured_at.as_deref(),
        ]
        .into_iter()
        .flatten()
        .chain(metadata.tags.iter().map(String::as_str))
        {
            haystack.push(' ');
            haystack.push_str(value);
        }
        let haystack = haystack.to_lowercase();
        if terms.iter().all(|term| match term {
            SearchTerm::Text(text) => haystack.contains(text),
            SearchTerm::App(app) => metadata
                .process
                .as_deref()
                .is_some_and(|process| process.to_lowercase().contains(app)),
            SearchTerm::Site(site) => metadata
                .url
                .as_deref()
                .and_then(site_from_url)
                .is_some_and(|host| host.to_lowercase().contains(site)),
            SearchTerm::Tag(tag) => metadata
                .tags
                .iter()
                .any(|value| value.to_lowercase().contains(tag)),
            SearchTerm::Age(limit) => match now.duration_since(modified) {
                Ok(age) => age < *limit,
                Err(_) => true,
            },
        }) {
            matches.push((path, modified, metadata.pinned));
        }
    }
    matches.sort_by(
        |(left_path, left_time, left_pinned), (right_path, right_time, right_pinned)| {
            right_pinned
                .cmp(left_pinned)
                .then_with(|| right_time.cmp(left_time))
                .then_with(|| right_path.cmp(left_path))
        },
    );
    Ok(matches.into_iter().map(|(path, _, _)| path).collect())
}

fn site_from_url(url: &str) -> Option<&str> {
    let authority = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = authority.split(['/', '?', '#']).next()?;
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    (!host.is_empty()).then_some(host)
}

pub(super) fn read_metadata(capture: &Path) -> Result<CaptureMetadata, String> {
    let Some(contents) = read_metadata_bytes(capture)? else {
        return Ok(CaptureMetadata::default());
    };
    parse_metadata(capture, &contents)
}

fn parse_metadata(capture: &Path, contents: &[u8]) -> Result<CaptureMetadata, String> {
    serde_json::from_slice(contents).map_err(|error| {
        format!(
            "could not parse capture metadata {}: {error}",
            capture.with_extension("json").display()
        )
    })
}

fn read_metadata_bytes(capture: &Path) -> Result<Option<Vec<u8>>, String> {
    let path = capture.with_extension("json");
    match std::fs::read(&path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "could not read capture metadata {}: {error}",
            path.display()
        )),
    }
}

type MetadataDocument = BTreeMap<String, Box<RawValue>>;

fn read_metadata_document(capture: &Path, contents: &[u8]) -> Result<MetadataDocument, String> {
    let path = capture.with_extension("json");
    serde_json::from_slice(contents).map_err(|error| {
        format!(
            "could not parse capture metadata {}: {error}",
            path.display()
        )
    })
}

pub(super) fn set_tags(capture: &Path, tags: &str) -> Result<Vec<String>, String> {
    let tags = crate::library_tags::parse_tags(tags);
    let raw_tags = serde_json::value::to_raw_value(&tags)
        .map_err(|error| format!("could not serialize capture tags: {error}"))?;
    edit_metadata(capture, |document, _| {
        document.insert("tags".to_owned(), raw_tags);
        Ok(())
    })?;
    Ok(tags)
}

pub(super) fn toggle_pin(capture: &Path) -> Result<bool, String> {
    let mut pinned = false;
    edit_metadata(capture, |document, metadata| {
        pinned = !metadata.pinned;
        let value = serde_json::value::to_raw_value(&pinned)
            .map_err(|error| format!("could not serialize capture pin: {error}"))?;
        document.insert("pinned".to_owned(), value);
        Ok(())
    })?;
    Ok(pinned)
}

fn edit_metadata(
    capture: &Path,
    change: impl FnOnce(&mut MetadataDocument, &CaptureMetadata) -> Result<(), String>,
) -> Result<(), String> {
    if !is_capture_file(capture) {
        return Err(format!("{} is not a library capture", capture.display()));
    }
    let capture_info = std::fs::metadata(capture)
        .map_err(|error| format!("could not read capture {}: {error}", capture.display()))?;
    if !capture_info.is_file() {
        return Err(format!("{} is not a capture file", capture.display()));
    }
    reject_metadata_symlink(capture)?;
    let original = read_metadata_bytes(capture)?;
    let mut document = match original.as_deref() {
        Some(contents) => read_metadata_document(capture, contents)?,
        None => new_metadata_document(capture)?,
    };
    let metadata = match original.as_deref() {
        Some(contents) => parse_metadata(capture, contents)?,
        None => CaptureMetadata::default(),
    };
    change(&mut document, &metadata)?;
    write_metadata_document(capture, &document, original.as_deref())
}

fn new_metadata_document(capture: &Path) -> Result<MetadataDocument, String> {
    let is_video = capture
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("mp4"));
    let (width, height) = if is_video {
        (0, 0)
    } else {
        image::image_dimensions(capture)
            .map_err(|error| format!("could not read image {}: {error}", capture.display()))?
    };
    let rect_width = i32::try_from(width).map_err(|_| "capture width is too large".to_owned())?;
    let rect_height =
        i32::try_from(height).map_err(|_| "capture height is too large".to_owned())?;
    let document = json!({
        "kind": if is_video { "video" } else { "region" },
        "rect": {"x": 0, "y": 0, "width": rect_width, "height": rect_height},
        "width": width,
        "height": height,
        "captured_at": "",
        "cursor_included": false
    });
    let bytes = serde_json::to_vec(&document)
        .map_err(|error| format!("could not serialize new capture metadata: {error}"))?;
    read_metadata_document(capture, &bytes)
}

fn reject_metadata_symlink(capture: &Path) -> Result<(), String> {
    let path = capture.with_extension("json");
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "capture metadata {} is a symbolic link; edit its target directly",
            path.display()
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "could not inspect capture metadata {}: {error}",
            path.display()
        )),
    }
}

fn write_metadata_document(
    capture: &Path,
    document: &MetadataDocument,
    original: Option<&[u8]>,
) -> Result<(), String> {
    let path = capture.with_extension("json");
    let parent = path
        .parent()
        .ok_or_else(|| format!("capture metadata path {} has no parent", path.display()))?;
    let bytes = serde_json::to_vec_pretty(document)
        .map_err(|error| format!("could not serialize capture metadata: {error}"))?;
    let mut staging = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("could not stage metadata in {}: {error}", parent.display()))?;
    match std::fs::metadata(&path) {
        Ok(existing) => {
            staging
                .as_file()
                .set_permissions(existing.permissions())
                .map_err(|error| {
                    format!(
                        "could not preserve permissions for {}: {error}",
                        path.display()
                    )
                })?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "could not read permissions for {}: {error}",
                path.display()
            ));
        }
    }
    staging
        .write_all(&bytes)
        .and_then(|_| staging.as_file().sync_all())
        .map_err(|error| {
            format!(
                "could not write capture metadata {}: {error}",
                path.display()
            )
        })?;
    reject_metadata_symlink(capture)?;
    if read_metadata_bytes(capture)?.as_deref() != original {
        return Err(format!(
            "capture metadata {} changed while it was being edited; retry the operation",
            path.display()
        ));
    }
    staging.persist(&path).map_err(|error| {
        format!(
            "could not save capture metadata {}: {}",
            path.display(),
            error.error
        )
    })?;
    Ok(())
}

pub(super) fn capture_files_in(directory: &Path) -> Result<Vec<(PathBuf, SystemTime)>, String> {
    let mut captures = Vec::new();
    collect_capture_files(directory, true, &mut captures)?;
    Ok(captures)
}

fn collect_capture_files(
    directory: &Path,
    include_months: bool,
    captures: &mut Vec<(PathBuf, SystemTime)>,
) -> Result<(), String> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if include_months && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(error) => {
            return Err(format!(
                "could not read captures from {}: {error}",
                directory.display()
            ));
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "could not read a capture entry in {}: {error}",
                directory.display()
            )
        })?;
        let path = entry.path();
        if include_months && is_month_directory(&entry.file_name()) {
            let metadata = std::fs::metadata(&path).map_err(|error| {
                format!("could not read capture folder {}: {error}", path.display())
            })?;
            if metadata.is_dir() {
                collect_capture_files(&path, false, captures)?;
            }
            continue;
        }
        if !is_capture_file(&path) {
            continue;
        }
        let metadata = std::fs::metadata(&path)
            .map_err(|error| format!("could not read capture {}: {error}", path.display()))?;
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().map_err(|error| {
            format!(
                "could not read capture modification time for {}: {error}",
                path.display()
            )
        })?;
        captures.push((path, modified));
    }
    Ok(())
}

fn is_month_directory(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let bytes = name.as_bytes();
    bytes.len() == 7
        && bytes[4] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..].iter().all(u8::is_ascii_digit)
        && matches!(
            (bytes[5], bytes[6]),
            (b'0', b'1'..=b'9') | (b'1', b'0'..=b'2')
        )
}

fn is_capture_file(path: &Path) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    let name = name.to_string_lossy().to_ascii_lowercase();
    !name.ends_with(".cursor.png")
        && !name.ends_with(".thumb.png")
        && matches!(
            path.extension()
                .and_then(|extension| extension.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("png" | "mp4")
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::time::{Duration, UNIX_EPOCH};

    fn write_capture(directory: &Path, name: &str, modified: u64) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, b"capture").expect("capture fixture should be written");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("capture fixture should open")
            .set_times(
                std::fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(modified)),
            )
            .expect("capture modification time should be set");
        path
    }

    #[test]
    fn searches_all_captures_in_the_root_and_month_folders() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        for index in 0..10 {
            write_capture(directory.path(), &format!("Screenshot-{index}.png"), index);
        }
        let month = directory.path().join("2026-03");
        std::fs::create_dir(&month).expect("month directory should be created");
        let recording = write_capture(&month, "Meeting.MP4", 20);
        write_capture(&month, "Meeting.cursor.png", 21);
        write_capture(&month, "Meeting.thumb.png", 22);
        let unrelated = directory.path().join("archive");
        std::fs::create_dir(&unrelated).expect("unrelated directory should be created");
        write_capture(&unrelated, "ignored.png", 23);
        let invalid_month = directory.path().join("2026-13");
        std::fs::create_dir(&invalid_month).expect("invalid month directory should be created");
        write_capture(&invalid_month, "ignored.png", 24);

        let all = search_captures_in(directory.path(), "", CaptureFilter::All)
            .expect("captures should be searchable");
        assert_eq!(
            all.len(),
            11,
            "search should not be limited to the recent strip"
        );
        assert_eq!(all[0], recording, "the newest capture should appear first");
        let videos = search_captures_in(directory.path(), "MEET", CaptureFilter::Videos)
            .expect("video search should succeed");
        assert_eq!(
            videos,
            vec![recording],
            "search and extensions should ignore case"
        );
        let images = search_captures_in(directory.path(), "", CaptureFilter::Images)
            .expect("image search should succeed");
        assert_eq!(
            images.len(),
            10,
            "companion images should not appear as captures"
        );
    }

    #[test]
    fn searches_metadata_and_filters_pinned_and_tagged_captures() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let pinned = write_capture(directory.path(), "older.png", 1);
        let newer = write_capture(directory.path(), "newer.mp4", 2);
        std::fs::write(
            pinned.with_extension("json"),
            r#"{"window_title":"Design Review","process":"Browser","url":"https://example.test/guide","captured_at":"2026-03-01T10:00:00","tags":["Planning","Team"],"pinned":true}"#,
        )
        .expect("metadata should be written");

        let matching = search_captures_in(directory.path(), "BROWSER planning", CaptureFilter::All)
            .expect("metadata should be searchable");
        assert_eq!(matching, vec![pinned.clone()], "all query terms must match");
        for query in ["review", "example.test", "2026-03", "team", "older"] {
            assert_eq!(
                search_captures_in(directory.path(), query, CaptureFilter::All)
                    .expect("metadata search should succeed"),
                vec![pinned.clone()],
                "query {query:?} should find the capture"
            );
        }
        assert_eq!(
            search_captures_in(directory.path(), "", CaptureFilter::All)
                .expect("all captures should be listed"),
            vec![pinned.clone(), newer],
            "pinned captures should precede newer unpinned captures"
        );
        for filter in [CaptureFilter::Pinned, CaptureFilter::Tagged] {
            assert_eq!(
                search_captures_in(directory.path(), "", filter)
                    .expect("metadata filter should succeed"),
                vec![pinned.clone()],
                "the filter should exclude captures without matching metadata"
            );
        }
    }

    #[test]
    fn qualified_search_restricts_matches_to_the_requested_metadata_fields() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let matching = write_capture(directory.path(), "release.png", 10);
        std::fs::write(
            matching.with_extension("json"),
            r#"{"window_title":"Release notes","process":"Firefox","url":"https://account:secret@docs.example.test/guide","tags":["Team Planning"]}"#,
        )
        .expect("matching metadata should be written");
        let unrelated = write_capture(directory.path(), "other.png", 11);
        std::fs::write(
            unrelated.with_extension("json"),
            r#"{"window_title":"Firefox Planning docs.example.test","process":"Editor","url":"https://other.test/example.test","tags":["Reference"]}"#,
        )
        .expect("unrelated metadata should be written");

        for query in [
            "APP:firefox",
            "site:EXAMPLE.test",
            "tag:planning",
            "release app:firefox site:docs.example.test tag:team",
        ] {
            assert_eq!(
                search_captures_in(directory.path(), query, CaptureFilter::All)
                    .expect("qualified search should succeed"),
                vec![matching.clone()],
                "query {query:?} should search the specified fields"
            );
        }
        assert!(
            search_captures_in(directory.path(), "site:guide", CaptureFilter::All)
                .expect("site search should succeed")
                .is_empty(),
            "site searches must not match URL paths"
        );
        assert!(
            search_captures_in(directory.path(), "site:account", CaptureFilter::All)
                .expect("site search should succeed")
                .is_empty(),
            "site searches must not match URL credentials"
        );
        assert_eq!(
            search_captures_in(directory.path(), "Firefox Planning", CaptureFilter::All)
                .expect("ordinary searches should still work"),
            vec![unrelated, matching],
            "unqualified words should continue matching across metadata fields"
        );
    }

    #[test]
    fn age_qualifiers_use_rolling_boundaries_and_keep_future_captures() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let now_seconds = 2_000_000_000;
        let now = UNIX_EPOCH + Duration::from_secs(now_seconds);
        let future = write_capture(directory.path(), "future.png", now_seconds + 60);
        let recent = write_capture(directory.path(), "recent.png", now_seconds - 86_399);
        let day_boundary =
            write_capture(directory.path(), "day-boundary.png", now_seconds - 86_400);
        let week_recent = write_capture(directory.path(), "week.png", now_seconds - 604_799);
        write_capture(directory.path(), "week-boundary.png", now_seconds - 604_800);

        assert_eq!(
            search_captures_in_at(directory.path(), "age:day", CaptureFilter::All, now)
                .expect("daily search should succeed"),
            vec![future.clone(), recent.clone()],
            "the last day excludes the exact 24-hour boundary"
        );
        assert_eq!(
            search_captures_in_at(directory.path(), "age:week", CaptureFilter::All, now)
                .expect("weekly search should succeed"),
            vec![future, recent, day_boundary, week_recent],
            "the last week excludes the exact seven-day boundary"
        );
    }

    #[test]
    fn invalid_qualifiers_are_reported_and_unknown_prefixes_remain_text() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let capture = write_capture(directory.path(), "label:report.png", 10);
        for query in ["app:", "site:", "tag:", "age:", "age:month"] {
            let error = search_captures_in(directory.path(), query, CaptureFilter::All)
                .expect_err("invalid known qualifiers should be reported");
            assert!(
                error
                    .to_lowercase()
                    .contains(query.split(':').next().unwrap()),
                "the error for {query:?} should identify its qualifier: {error}"
            );
        }
        assert_eq!(
            search_captures_in(directory.path(), "label:report", CaptureFilter::All)
                .expect("unknown qualifiers should remain literal search text"),
            vec![capture]
        );
    }

    #[test]
    fn qualified_search_combines_with_existing_capture_filters() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let image = write_capture(directory.path(), "image.png", 1);
        let video = write_capture(directory.path(), "video.mp4", 2);
        for path in [&image, &video] {
            std::fs::write(
                path.with_extension("json"),
                r#"{"process":"Browser","tags":["Shared"],"pinned":true}"#,
            )
            .expect("capture metadata should be written");
        }
        write_capture(directory.path(), "no-metadata.png", 3);

        assert_eq!(
            search_captures_in(directory.path(), "app:browser", CaptureFilter::Images)
                .expect("image filter should succeed"),
            vec![image.clone()]
        );
        assert_eq!(
            search_captures_in(directory.path(), "tag:shared", CaptureFilter::Videos)
                .expect("video filter should succeed"),
            vec![video.clone()]
        );
        for filter in [CaptureFilter::Pinned, CaptureFilter::Tagged] {
            assert_eq!(
                search_captures_in(directory.path(), "app:browser", filter)
                    .expect("metadata filter should succeed"),
                vec![video.clone(), image.clone()],
                "qualified search should compose with {filter:?}"
            );
        }
    }

    #[test]
    fn reports_invalid_metadata_without_changing_the_sidecar() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let capture = write_capture(directory.path(), "capture.png", 1);
        let sidecar = capture.with_extension("json");
        std::fs::write(&sidecar, b"{invalid").expect("invalid metadata should be written");

        let error = search_captures_in(directory.path(), "", CaptureFilter::All)
            .expect_err("invalid metadata should be reported");
        assert!(
            error.contains("capture.json"),
            "the error should identify the sidecar"
        );
        assert_eq!(
            std::fs::read(&sidecar).expect("metadata should remain"),
            b"{invalid",
            "search must not overwrite malformed metadata"
        );
    }

    #[test]
    fn missing_capture_folder_is_an_empty_library() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let missing = directory.path().join("missing");
        assert!(search_captures_in(&missing, "", CaptureFilter::All)
            .expect("a missing capture folder should be empty")
            .is_empty());
    }

    #[test]
    fn tagging_a_new_image_creates_windows_compatible_metadata() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let capture = directory.path().join("new.png");
        image::RgbaImage::from_pixel(3, 2, image::Rgba([1, 2, 3, 255]))
            .save(&capture)
            .expect("capture image should be written");

        assert_eq!(
            set_tags(&capture, " Planning, team; PLANNING ;; ").expect("tags should save"),
            ["Planning", "team"]
        );
        let value: Value = serde_json::from_slice(
            &std::fs::read(capture.with_extension("json")).expect("sidecar should exist"),
        )
        .expect("metadata should be JSON");
        assert_eq!(value["kind"], "region");
        assert_eq!(
            value["rect"],
            json!({"x": 0, "y": 0, "width": 3, "height": 2})
        );
        assert_eq!(value["width"], 3);
        assert_eq!(value["height"], 2);
        assert_eq!(value["captured_at"], "");
        assert_eq!(value["cursor_included"], false);
        assert_eq!(value["tags"], json!(["Planning", "team"]));
        assert_eq!(
            search_captures_in(directory.path(), "planning", CaptureFilter::Tagged)
                .expect("new tags should be searchable"),
            vec![capture]
        );
    }

    #[test]
    fn editing_tags_and_pins_preserves_unrelated_metadata() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let capture = write_capture(directory.path(), "existing.mp4", 1);
        let sidecar = capture.with_extension("json");
        let original = json!({
            "kind": "video",
            "window_title": "Original",
            "tags": ["old"],
            "pinned": false,
            "extra": {"future": [1, 2, 3]}
        });
        std::fs::write(
            &sidecar,
            serde_json::to_vec(&original).expect("fixture should serialize"),
        )
        .expect("metadata fixture should be written");
        assert!(toggle_pin(&capture).expect("pin should save"));
        assert_eq!(
            set_tags(&capture, "first; second, FIRST").expect("tags should save"),
            ["first", "second"]
        );
        let after: Value = serde_json::from_slice(
            &std::fs::read(&sidecar).expect("updated metadata should be readable"),
        )
        .expect("updated metadata should be JSON");
        assert_eq!(
            after["extra"], original["extra"],
            "unknown fields must be retained"
        );
        assert_eq!(after["window_title"], original["window_title"]);
        assert_eq!(after["kind"], original["kind"]);
        assert_eq!(after["pinned"], true);
        assert_eq!(after["tags"], json!(["first", "second"]));
        assert_eq!(
            search_captures_in(directory.path(), "second", CaptureFilter::Pinned)
                .expect("updated pin and tags should be searchable"),
            vec![capture.clone()]
        );
        assert!(!toggle_pin(&capture).expect("unpin should save"));
        assert!(
            search_captures_in(directory.path(), "", CaptureFilter::Pinned)
                .expect("pinned filter should refresh")
                .is_empty()
        );
        assert!(set_tags(&capture, "")
            .expect("tags should clear")
            .is_empty());
        assert!(
            search_captures_in(directory.path(), "", CaptureFilter::Tagged)
                .expect("tagged filter should refresh")
                .is_empty()
        );
    }

    #[test]
    fn pinning_a_video_without_a_sidecar_creates_compatible_defaults() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let capture = write_capture(directory.path(), "recording.mp4", 1);
        assert!(toggle_pin(&capture).expect("pin should save"));
        let value: Value = serde_json::from_slice(
            &std::fs::read(capture.with_extension("json")).expect("sidecar should exist"),
        )
        .expect("metadata should be JSON");
        assert_eq!(value["kind"], "video");
        assert_eq!(
            value["rect"],
            json!({"x": 0, "y": 0, "width": 0, "height": 0})
        );
        assert_eq!(value["width"], 0);
        assert_eq!(value["height"], 0);
        assert_eq!(value["captured_at"], "");
        assert_eq!(value["cursor_included"], false);
        assert_eq!(value["pinned"], true);
    }

    #[test]
    fn metadata_edits_reject_invalid_or_missing_capture_without_overwriting() {
        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let capture = write_capture(directory.path(), "capture.png", 1);
        let sidecar = capture.with_extension("json");
        for contents in [r#"{invalid"#, r#"[]"#, r#"{"tags":"bad","pinned":true}"#] {
            std::fs::write(&sidecar, contents).expect("invalid metadata fixture should be written");
            assert!(
                set_tags(&capture, "new").is_err(),
                "invalid metadata must be reported"
            );
            assert!(
                toggle_pin(&capture).is_err(),
                "invalid metadata must not be overwritten"
            );
            assert_eq!(
                std::fs::read_to_string(&sidecar).expect("metadata should remain"),
                contents,
                "failed edits must preserve the original sidecar"
            );
        }
        std::fs::remove_file(&capture).expect("capture fixture should be removed");
        assert!(
            toggle_pin(&capture).is_err(),
            "a missing capture must not be pinned"
        );
        assert_eq!(
            std::fs::read_to_string(&sidecar).expect("sidecar should remain"),
            r#"{"tags":"bad","pinned":true}"#
        );
    }

    #[test]
    fn metadata_edits_preserve_numeric_precision_and_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let capture = write_capture(directory.path(), "capture.mp4", 1);
        let sidecar = capture.with_extension("json");
        let number = "123456789012345678901234567890.0000000000000000000123";
        let nested =
            r#"{"huge": 1e1000, "numbers": [-0.0000000000000000012300, 9.876543210987654321e-99]}"#;
        std::fs::write(
            &sidecar,
            format!(r#"{{"extra":{number},"nested":{nested}}}"#),
        )
        .expect("metadata fixture should be written");
        std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o640))
            .expect("fixture permissions should be set");
        set_tags(&capture, "docs").expect("tags should save");
        let written = std::fs::read_to_string(&sidecar).expect("metadata should be readable");
        assert!(
            written.contains(number),
            "unrelated numeric data must retain its precision"
        );
        assert!(
            written.contains(nested),
            "unknown nested values must retain their exact representation"
        );
        assert!(toggle_pin(&capture).expect("pin should save with unknown numeric fields"));
        let written = std::fs::read_to_string(&sidecar).expect("metadata should be readable");
        assert!(written.contains(number));
        assert!(written.contains(nested));
        assert_eq!(
            std::fs::metadata(&sidecar)
                .expect("metadata should exist")
                .permissions()
                .mode()
                & 0o777,
            0o640,
            "replacing the sidecar should preserve its permissions"
        );
    }

    #[test]
    fn concurrent_metadata_changes_and_symlinks_are_not_overwritten() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temporary directory should be available");
        let capture = write_capture(directory.path(), "capture.mp4", 1);
        let sidecar = capture.with_extension("json");
        let latest = br#"{"tags":["latest"],"extra":42}"#;
        std::fs::write(&sidecar, latest).expect("concurrent metadata should be written");
        let proposed = read_metadata_document(&capture, br#"{"tags":["stale"]}"#)
            .expect("stale edit fixture should parse");
        let error = write_metadata_document(&capture, &proposed, Some(br#"{"tags":[]}"#))
            .expect_err("a stale edit must be rejected");
        assert!(
            error.contains("changed while"),
            "the conflict should be explained"
        );
        assert_eq!(
            std::fs::read(&sidecar).expect("sidecar should remain"),
            latest
        );
        assert!(
            write_metadata_document(&capture, &proposed, None).is_err(),
            "a newly created sidecar must not be overwritten by a stale first edit"
        );

        let target = directory.path().join("original.json");
        std::fs::rename(&sidecar, &target).expect("metadata target should be created");
        symlink(&target, &sidecar).expect("metadata symlink should be created");
        assert!(
            toggle_pin(&capture).is_err(),
            "metadata symlinks must be rejected"
        );
        assert!(std::fs::symlink_metadata(&sidecar)
            .expect("the metadata link should remain")
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read(&target).expect("target should remain"),
            latest
        );
    }
}
