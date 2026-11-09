//! Versioned recording markers, captions, and input activity.

use jiffshot_doc::Rgba;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const MAX_RECORDED_ACTIVITIES: usize = 16_384;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Marker {
    Label { at_ms: u64, label: String },
    Discard { at_ms: u64 },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RecordedCaption {
    pub at_ms: u64,
    pub duration_ms: Option<u64>,
    /// The annotation was already rendered into the recorded video frames.
    #[serde(default)]
    pub burned_in: bool,
    pub text: String,
    pub x: i32,
    pub y: i32,
    pub font: String,
    pub size: f32,
    pub color: Rgba,
    pub background: Rgba,
    pub padding: u32,
}

/// Builds the export caption corresponding to a recorded guide step.
///
/// # Errors
/// Returns an error when the description is empty or too long.
pub fn guide_caption(
    at_ms: u64,
    frame_height: Option<u32>,
    text: String,
) -> Result<RecordedCaption, String> {
    if text.trim().is_empty() || text.chars().count() > 4096 {
        return Err("recorded caption text must contain 1..4096 characters".into());
    }
    Ok(RecordedCaption {
        at_ms,
        duration_ms: None,
        burned_in: false,
        text,
        x: 24,
        y: frame_height.map_or(24, |height| height.saturating_sub(88) as i32),
        font: jiffshot_doc::text::DEFAULT_FAMILY.to_owned(),
        size: 28.0,
        color: Rgba::WHITE,
        background: Rgba::new(0, 0, 0, 176),
        padding: 8,
    })
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Activity {
    pub at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RecordingMarks {
    version: u32,
    duration_ms: u64,
    fps: u32,
    width: u32,
    height: u32,
    markers: Vec<Marker>,
    #[serde(default)]
    captions: Vec<RecordedCaption>,
    #[serde(default)]
    activities: Vec<Activity>,
    #[serde(default)]
    activities_complete: bool,
}

impl RecordingMarks {
    pub fn new(duration_ms: u64, fps: u32, width: u32, height: u32, markers: Vec<Marker>) -> Self {
        Self {
            version: 1,
            duration_ms,
            fps,
            width,
            height,
            markers,
            captions: Vec::new(),
            activities: Vec::new(),
            activities_complete: false,
        }
    }

    pub fn with_captions(mut self, mut captions: Vec<RecordedCaption>) -> Self {
        for caption in &mut captions {
            caption.duration_ms = caption
                .duration_ms
                .map(|duration| duration.min(self.duration_ms.saturating_sub(caption.at_ms)));
        }
        self.captions = captions;
        self
    }

    pub fn markers(&self) -> &[Marker] {
        &self.markers
    }

    pub fn captions(&self) -> &[RecordedCaption] {
        &self.captions
    }

    pub fn with_activities(mut self, activities: Vec<Activity>) -> Self {
        self.activities = activities;
        self
    }

    pub fn with_activity_tracking_complete(mut self, complete: bool) -> Self {
        self.activities_complete = complete;
        self
    }

    pub fn activities(&self) -> &[Activity] {
        &self.activities
    }

    pub fn activities_complete(&self) -> bool {
        self.activities_complete
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn duration_seconds(&self) -> f64 {
        self.duration_ms as f64 / 1000.0
    }
}

pub fn sidecar_path(video: &Path) -> PathBuf {
    let mut name = video.as_os_str().to_owned();
    name.push(".jiffshot.json");
    PathBuf::from(name)
}

pub fn write(video: &Path, marks: &RecordingMarks) -> Result<(), String> {
    let path = sidecar_path(video);
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let bytes = serde_json::to_vec(marks).map_err(|error| error.to_string())?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".jiffshot-marks-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .map_err(|error| format!("cannot prepare {}: {error}", path.display()))?;
    temporary
        .write_all(&bytes)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    temporary.persist_noclobber(&path).map_err(|error| {
        if error.error.kind() == std::io::ErrorKind::AlreadyExists {
            format!("recording marker file already exists: {}", path.display())
        } else {
            format!("cannot publish {}: {}", path.display(), error.error)
        }
    })?;
    Ok(())
}

pub fn read(video: &Path) -> Result<Option<RecordingMarks>, String> {
    let path = sidecar_path(video);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let marks: RecordingMarks = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid recording marker file {}: {error}", path.display()))?;
    let valid_markers = marks.markers.len() <= 256
        && marks.markers.iter().all(|marker| match marker {
            Marker::Label { at_ms, label } => {
                *at_ms <= marks.duration_ms
                    && !label.trim().is_empty()
                    && label.chars().count() <= 128
            }
            Marker::Discard { at_ms } => *at_ms <= marks.duration_ms,
        });
    let valid_captions = marks.captions.len() <= 256
        && marks.captions.iter().all(|caption| {
            caption.at_ms <= marks.duration_ms
                && caption.duration_ms.is_none_or(|duration| {
                    duration <= marks.duration_ms.saturating_sub(caption.at_ms)
                })
                && !caption.text.trim().is_empty()
                && caption.text.chars().count() <= 4096
                && caption.size.is_finite()
                && (1.0..=512.0).contains(&caption.size)
                && caption.padding <= 1024
        });
    let valid_activities = marks.activities.len() <= MAX_RECORDED_ACTIVITIES
        && marks
            .activities
            .iter()
            .all(|activity| activity.at_ms <= marks.duration_ms);
    if marks.version != 1
        || marks.fps == 0
        || marks.width == 0
        || marks.height == 0
        || !valid_markers
        || !valid_captions
        || !valid_activities
    {
        return Err(format!(
            "unsupported recording marker file: {}",
            path.display()
        ));
    }
    Ok(Some(marks))
}

pub fn select_ranges(
    marks: &RecordingMarks,
    from: f64,
    to: f64,
    keep_marked: bool,
    drop_discarded: bool,
) -> Result<Vec<(f64, f64)>, String> {
    if !from.is_finite() || !to.is_finite() || from < 0.0 || to <= from {
        return Err("selection range must satisfy 0 <= from < to".into());
    }
    let duration = marks.duration_ms as f64 / 1000.0;
    let to = to.min(duration);
    if from >= to {
        return Err("selection does not overlap the recording".into());
    }

    let mut ranges = if keep_marked {
        marker_ranges(marks, true, duration)?
    } else {
        vec![(0.0, duration)]
    };
    ranges = intersect(&ranges, from, to);
    if drop_discarded {
        for (start, end) in marker_ranges(marks, false, duration)? {
            ranges = subtract(&ranges, start, end);
        }
    }
    if ranges.is_empty() {
        return Err("recording markers select no video frames".into());
    }
    Ok(ranges)
}

fn marker_ranges(
    marks: &RecordingMarks,
    labeled: bool,
    duration: f64,
) -> Result<Vec<(f64, f64)>, String> {
    let mut labels = std::collections::BTreeMap::<String, Option<f64>>::new();
    let mut discard_start = None;
    let mut ranges = Vec::new();
    for marker in marks.markers() {
        let (at_ms, label, is_discard) = match marker {
            Marker::Label { at_ms, label } => (*at_ms, Some(label), false),
            Marker::Discard { at_ms } => (*at_ms, None, true),
        };
        let at = at_ms as f64 / 1000.0;
        if at > duration {
            return Err("recording marker lies beyond the saved recording".into());
        }
        if labeled {
            if is_discard {
                continue;
            }
        } else if !is_discard {
            continue;
        }
        if let Some(label) = label {
            let open = labels.entry(label.clone()).or_default();
            if let Some(start) = open.take() {
                if at > start {
                    ranges.push((start, at));
                }
            } else {
                *open = Some(at);
            }
        } else if let Some(start) = discard_start.take() {
            if at > start {
                ranges.push((start, at));
            }
        } else {
            discard_start = Some(at);
        }
    }
    if labeled {
        for start in labels.into_values().flatten() {
            if duration > start {
                ranges.push((start, duration));
            }
        }
    } else if let Some(start) = discard_start {
        if duration > start {
            ranges.push((start, duration));
        }
    }
    ranges.sort_by(|left, right| left.0.total_cmp(&right.0));
    Ok(merge(ranges))
}

fn intersect(ranges: &[(f64, f64)], from: f64, to: f64) -> Vec<(f64, f64)> {
    ranges
        .iter()
        .filter_map(|(start, end)| {
            let start = start.max(from);
            let end = end.min(to);
            (start < end).then_some((start, end))
        })
        .collect()
}

fn subtract(ranges: &[(f64, f64)], from: f64, to: f64) -> Vec<(f64, f64)> {
    ranges
        .iter()
        .flat_map(|(start, end)| {
            if to <= *start || from >= *end {
                vec![(*start, *end)]
            } else {
                let mut remaining = Vec::with_capacity(2);
                if *start < from {
                    remaining.push((*start, from));
                }
                if to < *end {
                    remaining.push((to, *end));
                }
                remaining
            }
        })
        .collect()
}

fn merge(mut ranges: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    ranges.sort_by(|left, right| left.0.total_cmp(&right.0));
    let mut merged: Vec<(f64, f64)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        if let Some(last) = merged.last_mut() {
            if start <= last.1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        merged.push((start, end));
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_sidecar_round_trips_and_rejects_bad_versions() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let video = directory.path().join("recording.mp4");
        let sidecar = sidecar_path(&video);
        let marks = RecordingMarks::new(
            1_000,
            30,
            640,
            480,
            vec![Marker::Label {
                at_ms: 100,
                label: "opening".into(),
            }],
        )
        .with_activities(vec![Activity { at_ms: 250 }])
        .with_activity_tracking_complete(false);
        write(&video, &marks).expect("write sidecar");
        let decoded = read(&video).expect("read sidecar").expect("sidecar exists");
        assert_eq!(decoded.markers(), marks.markers());
        assert_eq!(decoded.activities(), marks.activities());
        assert_eq!(decoded.activities_complete(), marks.activities_complete());
        assert!(
            write(&video, &marks).is_err(),
            "never overwrite marker metadata"
        );

        fs::write(
            &sidecar,
            br#"{"version":1,"duration_ms":1000,"fps":30,"width":640,"height":480,"markers":[]}"#,
        )
        .expect("write sidecar without optional metadata");
        let legacy = read(&video)
            .expect("read sidecar without optional metadata")
            .expect("sidecar exists");
        assert!(legacy.captions().is_empty());
        assert!(legacy.activities().is_empty());
        assert!(
            !legacy.activities_complete(),
            "older sidecars cannot prove that physical input was tracked"
        );

        fs::write(
            &sidecar,
            br#"{"version":2,"duration_ms":1000,"fps":30,"markers":[]}"#,
        )
        .expect("replace test sidecar");
        assert!(read(&video).is_err(), "reject unknown sidecar versions");
        fs::remove_file(sidecar).expect("remove test sidecar");
    }

    #[test]
    fn concurrent_sidecar_writers_never_replace_the_winner() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let video = directory.path().join("recording.mp4");
        let participants = 12;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(participants));
        let writers: Vec<_> = (0..participants)
            .map(|index| {
                let video = video.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let label = format!("writer-{index}");
                    let marks = RecordingMarks::new(
                        1_000,
                        30,
                        640,
                        480,
                        vec![Marker::Label {
                            at_ms: 100,
                            label: label.clone(),
                        }],
                    );
                    barrier.wait();
                    (label, write(&video, &marks))
                })
            })
            .collect();
        let results: Vec<_> = writers
            .into_iter()
            .map(|writer| writer.join().expect("writer thread"))
            .collect();
        let winners: Vec<_> = results
            .iter()
            .filter(|(_, result)| result.is_ok())
            .collect();
        assert_eq!(winners.len(), 1, "only one writer may publish the sidecar");
        for (_, result) in &results {
            if let Err(error) = result {
                assert!(error.contains("already exists"), "{error}");
            }
        }
        let saved = read(&video)
            .expect("sidecar is valid")
            .expect("sidecar exists");
        assert_eq!(
            saved.markers(),
            &[Marker::Label {
                at_ms: 100,
                label: winners[0].0.clone(),
            }]
        );
        assert_eq!(
            fs::read_dir(directory.path())
                .expect("directory listing")
                .count(),
            1,
            "losing writers remove their temporary files"
        );
    }

    #[test]
    fn keep_and_discard_markers_select_and_subtract_half_open_ranges() {
        let marks = RecordingMarks::new(
            10_000,
            30,
            640,
            480,
            vec![
                Marker::Label {
                    at_ms: 1_000,
                    label: "intro".into(),
                },
                Marker::Label {
                    at_ms: 5_000,
                    label: "intro".into(),
                },
                Marker::Label {
                    at_ms: 6_000,
                    label: "result".into(),
                },
                Marker::Discard { at_ms: 2_000 },
                Marker::Discard { at_ms: 3_000 },
            ],
        );

        assert_eq!(
            select_ranges(&marks, 0.0, 10.0, true, true).unwrap(),
            [(1.0, 2.0), (3.0, 5.0), (6.0, 10.0)]
        );
        assert_eq!(
            select_ranges(&marks, 4.0, 7.0, true, false).unwrap(),
            [(4.0, 5.0), (6.0, 7.0)]
        );
    }

    #[test]
    fn open_ranges_extend_to_end_and_invalid_marker_times_are_rejected() {
        let marks = RecordingMarks::new(
            5_000,
            30,
            640,
            480,
            vec![Marker::Label {
                at_ms: 2_000,
                label: "keep".into(),
            }],
        );
        assert_eq!(
            select_ranges(&marks, 0.0, 5.0, true, false).unwrap(),
            [(2.0, 5.0)]
        );

        let invalid = RecordingMarks::new(
            5_000,
            30,
            640,
            480,
            vec![Marker::Label {
                at_ms: 6_000,
                label: "bad".into(),
            }],
        );
        assert!(select_ranges(&invalid, 0.0, 5.0, true, false).is_err());
    }
}
