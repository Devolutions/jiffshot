//! Offline video inspection, stills and transactional post-production.

use crate::recording_marks;
use crate::{internal, invalid, not_found, Args, Reply};
use jiffshot_capture::media::post::{
    self, Audio, Caption, CaptionTimeline, GifDither, GifOptions, Mp4Options, Overlay, Segment,
};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub(super) fn dispatch(command: &str, a: &Args, should_cancel: &dyn Fn() -> bool) -> Reply {
    match command {
        "video.info" => {
            a.only(&["path", "exact"])?;
            let exact = a.bool("exact")?;
            let path = source(a)?;
            let metadata = if exact {
                post::inspect_with_frame_timing(&path)
            } else {
                post::inspect(&path)
            };
            serde_json::to_value(metadata.map_err(internal)?).map_err(internal)
        }
        "video.verify" => {
            a.only(&["path"])?;
            let path = source(a)?;
            serde_json::to_value(post::verify(&path).map_err(internal)?).map_err(internal)
        }
        "video.frame" => {
            a.only(&["path", "out", "at"])?;
            let path = source(a)?;
            let at = finite(a, "at", 0.0)?;
            let metadata = post::inspect(&path).map_err(internal)?;
            if at < 0.0 || at >= metadata.duration {
                return Err(invalid(format!(
                    "at must satisfy 0 <= at < duration ({})",
                    metadata.duration
                )));
            }
            let out = a
                .out("out", &["png"])?
                .ok_or_else(|| invalid("out is required"))?;
            let (width, height, time, bytes) =
                post::save_frame(&path, &out, at).map_err(internal)?;
            Ok(
                json!({"path":out, "format":"png", "width":width, "height":height, "at":time, "requested_at":at, "bytes":bytes}),
            )
        }
        "video.export" => export(a, should_cancel),
        "video.join" => join(a, should_cancel),
        "video.edit" => edit(a, should_cancel),
        _ => Err(invalid(format!("unknown video command {command:?}"))),
    }
}

fn edit(a: &Args, should_cancel: &dyn Fn() -> bool) -> Reply {
    a.only(&["plan"])?;
    let plan_path = path(
        &a.string("plan")?
            .ok_or_else(|| invalid("plan is required"))?,
    )?;
    let mut plan: Value = serde_json::from_slice(
        &std::fs::read(&plan_path)
            .map_err(|error| internal(format!("cannot read {}: {error}", plan_path.display())))?,
    )
    .map_err(|error| invalid(format!("invalid edit plan JSON: {error}")))?;
    let base = plan_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    resolve_plan_paths(&mut plan, base);
    let plan = Args::new(plan)?;
    plan.only(&[
        "segments",
        "out",
        "fps",
        "width",
        "height",
        "duration",
        "mute",
        "overlays",
        "captions_from_recording",
        "idle_after",
        "idle_mode",
        "idle_speed",
        "min_pause",
        "transition",
        "outputs",
    ])?;
    let outputs = plan
        .0
        .get("outputs")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("edit plan outputs must be a non-empty array"))?;
    if outputs.is_empty() || outputs.len() > 8 {
        return Err(invalid("edit plan must provide 1..8 outputs"));
    }

    let output_specs = outputs
        .iter()
        .map(|value| {
            let output = Args::new(value.clone())?;
            output.only(&[
                "format",
                "path",
                "fps",
                "max_width",
                "loop",
                "fade",
                "dither",
                "dither_mode",
                "palette_size",
                "optimize_frames",
                "max_bytes",
            ])?;
            let format = output
                .string("format")?
                .ok_or_else(|| invalid("each edit output requires a format"))?;
            let path = output
                .string("path")?
                .ok_or_else(|| invalid("each edit output requires a path"))?;
            let expected = match format.as_str() {
                "mp4" => "mp4",
                "gif" => "gif",
                _ => return Err(invalid("edit output format must be mp4 or gif")),
            };
            if format == "mp4" && output.0.keys().any(|key| key != "format" && key != "path") {
                return Err(invalid(
                    "MP4 settings belong at the edit-plan level; output settings are for GIF",
                ));
            }
            if PathBuf::from(&path)
                .extension()
                .is_none_or(|extension| !extension.eq_ignore_ascii_case(expected))
            {
                return Err(invalid(format!(
                    "edit output path must use the .{expected} extension"
                )));
            }
            Ok((format, path, output))
        })
        .collect::<Result<Vec<_>, jiffshot_control::Error>>()?;
    let mp4_outputs: Vec<_> = output_specs
        .iter()
        .filter(|(format, _, _)| format == "mp4")
        .collect();
    if mp4_outputs.len() > 1 {
        return Err(invalid("an edit plan may produce at most one MP4"));
    }
    let mut destinations = std::collections::HashSet::new();
    for (_, path, _) in &output_specs {
        let destination = PathBuf::from(path);
        let parent = destination
            .parent()
            .ok_or_else(|| invalid("edit output has no parent directory"))?;
        std::fs::create_dir_all(parent).map_err(internal)?;
        let identity = if destination.exists() {
            std::fs::canonicalize(&destination).map_err(internal)?
        } else {
            std::fs::canonicalize(parent).map_err(internal)?.join(
                destination
                    .file_name()
                    .ok_or_else(|| invalid("edit output has no file name"))?,
            )
        };
        let identity = identity.to_string_lossy();
        let identity = if cfg!(any(windows, target_os = "macos")) {
            identity.to_lowercase()
        } else {
            identity.into_owned()
        };
        if !destinations.insert(identity) {
            return Err(invalid("edit output paths must be distinct"));
        }
    }

    let input_segments = join_segments(&plan)?;
    let has_gif_output = output_specs.iter().any(|(format, _, _)| format == "gif");
    let source_has_audio = if has_gif_output {
        input_segments.iter().try_fold(false, |found, segment| {
            let metadata = post::inspect(&segment.path).map_err(internal)?;
            Ok::<_, jiffshot_control::Error>(
                found || metadata.audio.is_some() || metadata.audio_note.is_some(),
            )
        })?
    } else {
        false
    };
    let (input_overlays, _) = overlays(&plan)?;
    let protected_inputs: Vec<&Path> = std::iter::once(plan_path.as_path())
        .chain(input_segments.iter().map(|segment| segment.path.as_path()))
        .chain(input_overlays.iter().map(|overlay| overlay.path.as_path()))
        .collect();
    let temporary_mp4 = TempOutput::reserve("jiffshot-edit", "mp4").map_err(internal)?;
    let mp4_path = temporary_mp4.0.clone();
    let mut join_value = plan.0.clone();
    join_value.remove("outputs");
    join_value.insert("out".into(), json!(mp4_path));
    if mp4_outputs.is_empty() {
        plan.bool("mute")?;
        join_value.insert("mute".into(), json!(true));
    }
    let join_result = join(&Args::new(Value::Object(join_value))?, should_cancel)?;

    let mut produced = Vec::with_capacity(output_specs.len());
    let mut temporary_gifs = Vec::new();
    for (format, path, output) in &output_specs {
        let destination = PathBuf::from(path);
        if format == "mp4" {
            produced.push((mp4_path.clone(), destination, format, join_result.clone()));
            continue;
        }
        let temporary_gif = TempOutput::reserve("jiffshot-edit", "gif").map_err(internal)?;
        let mut gif_args = Map::new();
        gif_args.insert("path".into(), json!(mp4_path));
        gif_args.insert("out".into(), json!(temporary_gif.0));
        for key in [
            "fps",
            "max_width",
            "loop",
            "fade",
            "dither",
            "dither_mode",
            "palette_size",
            "optimize_frames",
            "max_bytes",
        ] {
            if let Some(value) = output.0.get(key) {
                gif_args.insert(key.to_owned(), value.clone());
            }
        }
        let mut result = export(&Args::new(Value::Object(gif_args))?, should_cancel)?;
        if source_has_audio {
            result
                .as_object_mut()
                .ok_or_else(|| internal("video export returned invalid metadata"))?
                .insert("audio_note".into(), json!("GIF cannot retain source audio"));
        }
        produced.push((temporary_gif.0.clone(), destination, format, result));
        temporary_gifs.push(temporary_gif);
    }
    for (_, destination, _, result) in &mut produced {
        let metadata = result
            .as_object_mut()
            .ok_or_else(|| internal("video export returned invalid metadata"))?;
        metadata.insert("path".into(), json!(destination));
    }
    let staged = produced
        .iter()
        .map(|(source, destination, format, _)| {
            post::prepare_output(source, destination, &protected_inputs, format).map_err(internal)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut results = Vec::with_capacity(produced.len());
    for (staged, (_, _, _, result)) in staged.into_iter().zip(produced) {
        staged.publish().map_err(internal)?;
        results.push(result);
    }
    Ok(Value::Array(results))
}

fn resolve_plan_paths(value: &mut Value, base: &std::path::Path) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if matches!(key.as_str(), "path" | "out") {
                    if let Some(path) = value.as_str() {
                        let path = PathBuf::from(path);
                        if !path.is_absolute() {
                            *value = json!(base.join(path));
                        }
                    }
                } else {
                    resolve_plan_paths(value, base);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                resolve_plan_paths(value, base);
            }
        }
        _ => {}
    }
}

struct TempOutput(PathBuf, Option<tempfile::TempDir>);

impl TempOutput {
    fn reserve(prefix: &str, extension: &str) -> std::io::Result<Self> {
        let mut builder = tempfile::Builder::new();
        builder.prefix(prefix);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let directory = builder.tempdir()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::metadata(directory.path())?.permissions().mode() & 0o077 != 0 {
                eprintln!(
                    "jiffshot: filesystem did not enforce owner-only permissions for {}; access depends on its ACLs",
                    directory.path().display()
                );
            }
        }
        let path = directory.path().join(format!("output.{extension}"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&path)?;
        Ok(Self(path, Some(directory)))
    }
}

impl Drop for TempOutput {
    fn drop(&mut self) {
        if let Some(directory) = self.1.take() {
            if let Err(error) = directory.close() {
                eprintln!(
                    "could not remove temporary video directory for {}: {error}",
                    self.0.display()
                );
            }
        }
    }
}

fn path(text: &str) -> Result<PathBuf, jiffshot_control::Error> {
    let path = PathBuf::from(text);
    if !path.is_absolute() {
        return Err(invalid("source path must be absolute"));
    }
    if !path.is_file() {
        return Err(not_found(format!("video not found: {}", path.display())));
    }
    Ok(path)
}

fn source(a: &Args) -> Result<PathBuf, jiffshot_control::Error> {
    path(
        &a.string("path")?
            .ok_or_else(|| invalid("path is required"))?,
    )
}

fn finite(a: &Args, name: &str, default: f64) -> Result<f64, jiffshot_control::Error> {
    let n = a.number(name)?.unwrap_or(default);
    if !n.is_finite() {
        return Err(invalid(format!("{name} must be finite")));
    }
    Ok(n)
}

fn segment(a: &Args) -> Result<Segment, jiffshot_control::Error> {
    let path = source(a)?;
    let metadata = post::inspect(&path).map_err(internal)?;
    let (from, to) = (finite(a, "from", 0.0)?, finite(a, "to", metadata.duration)?);
    if from < 0.0 || to <= from || to > metadata.duration {
        return Err(invalid(format!(
            "range must satisfy 0 <= from < to <= duration ({})",
            metadata.duration
        )));
    }
    let speed = finite(a, "speed", 1.0)?;
    if !(0.01..=100.0).contains(&speed) {
        return Err(invalid("speed must be in 0.01..=100"));
    }
    Ok(Segment {
        path,
        from,
        to,
        speed,
    })
}

fn options(a: &Args) -> Result<Mp4Options, jiffshot_control::Error> {
    let (overlays, captions) = overlays(a)?;
    Ok(Mp4Options {
        fps: a.int("fps", 1, 120)?.map(|n| n as u32),
        width: a.int("width", 2, 8192)?.map(|n| n as u32),
        height: a.int("height", 2, 8192)?.map(|n| n as u32),
        duration: a.number("duration")?,
        transition_seconds: a.number("transition")?,
        transition_boundaries: None,
        audio: if a.bool("mute")? {
            Audio::Mute
        } else {
            Audio::RequireCompatible
        },
        overlays,
        captions,
    })
}

fn run_export<T>(
    should_cancel: &dyn Fn() -> bool,
    work: impl FnOnce(&dyn Fn() -> bool) -> Result<T, post::ExportError>,
) -> Result<T, jiffshot_control::Error> {
    let observed = std::cell::Cell::new(false);
    let cancelled = || {
        let stopping = should_cancel();
        observed.set(observed.get() || stopping);
        stopping
    };
    work(&cancelled).map_err(|error| {
        if observed.get() {
            jiffshot_control::Error::new(
                jiffshot_control::ErrorCode::Conflict,
                "Jiffshot is shutting down; video export cancelled",
            )
        } else {
            internal(error)
        }
    })
}

fn export(a: &Args, should_cancel: &dyn Fn() -> bool) -> Reply {
    a.only(&[
        "path",
        "out",
        "from",
        "to",
        "fps",
        "width",
        "height",
        "duration",
        "mute",
        "max_width",
        "loop",
        "fade",
        "dither",
        "dither_mode",
        "palette_size",
        "optimize_frames",
        "max_bytes",
        "overlays",
        "keep_marked",
        "drop_discarded",
        "captions_from_recording",
        "idle_after",
        "idle_mode",
        "idle_speed",
        "min_pause",
    ])?;
    let segment = segment(a)?;
    let keep_marked = a.bool("keep_marked")?;
    let drop_discarded = a.bool("drop_discarded")?;
    let captions_from_recording = a.bool("captions_from_recording")?;
    let idle = idle_options(a)?;
    let marks = if keep_marked || drop_discarded || captions_from_recording || idle.is_some() {
        Some(
            recording_marks::read(&segment.path)
                .map_err(invalid)?
                .ok_or_else(|| invalid("recording markers are unavailable for this video"))?,
        )
    } else {
        None
    };
    let mut segments = if keep_marked || drop_discarded {
        let ranges = recording_marks::select_ranges(
            marks
                .as_ref()
                .expect("marker metadata required for selection"),
            segment.from,
            segment.to,
            keep_marked,
            drop_discarded,
        )
        .map_err(invalid)?;
        ranges
            .into_iter()
            .map(|(from, to)| Segment {
                path: segment.path.clone(),
                from,
                to,
                speed: segment.speed,
            })
            .collect::<Vec<_>>()
    } else {
        vec![segment.clone()]
    };
    if let Some(idle) = idle {
        segments = edit_idle_segments(
            segments,
            idle,
            marks
                .as_ref()
                .expect("recording metadata required for idle editing"),
        )?;
    }
    let origins = vec![0; segments.len()];
    let stage_gif = keep_marked || drop_discarded || idle.is_some();
    let out = a
        .out("out", &["mp4", "gif"])?
        .ok_or_else(|| invalid("out is required"))?;
    let result = if out
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("gif"))
    {
        let (overlays, captions) = overlays(a)?;
        if a.has("height") || a.has("width") {
            return Err(invalid("GIF uses max_width, not width or height"));
        }
        if a.has("mute") {
            a.bool("mute")?;
        }
        let mut gif_options = GifOptions {
            fps: finite(a, "fps", 10.0)?,
            max_width: a.int("max_width", 1, 8192)?.unwrap_or(960) as u32,
            duration: a.number("duration")?,
            looped: a.flag("loop")?.unwrap_or(true),
            fade: a.bool("fade")?,
            dither: a.flag("dither")?.unwrap_or(true),
            dither_mode: a
                .string("dither_mode")?
                .map(|mode| match mode.as_str() {
                    "none" => Ok(GifDither::None),
                    "ordered" => Ok(GifDither::Ordered),
                    "floyd-steinberg" => Ok(GifDither::FloydSteinberg),
                    _ => Err(invalid(
                        "dither_mode must be none, ordered, or floyd-steinberg",
                    )),
                })
                .transpose()?,
            palette_size: a.int("palette_size", 2, 256)?.unwrap_or(256) as u16,
            optimize_frames: a.flag("optimize_frames")?.unwrap_or(false),
            max_bytes: a.int("max_bytes", 1, i64::MAX)?.map(|value| value as u64),
            overlays,
            captions,
        };
        if !(1.0..=50.0).contains(&gif_options.fps) {
            return Err(invalid("GIF fps must be 1..50"));
        }
        if captions_from_recording && !stage_gif {
            let source_metadata = post::inspect(&segment.path).map_err(internal)?;
            add_recorded_captions(
                &mut gif_options.captions,
                marks
                    .as_ref()
                    .expect("marker metadata required for captions"),
                &segments,
                source_metadata.width,
                source_metadata.height,
                0,
                CaptionFit::Gif,
            )?;
        }
        if stage_gif {
            let temporary = TempOutput::reserve("jiffshot-select", "mp4").map_err(internal)?;
            let mp4_path = temporary.0.clone();
            let mut mp4_args = a.0.clone();
            mp4_args.remove("fps");
            let mut mp4_options = options(&Args::new(Value::Object(mp4_args))?)?;
            mp4_options.audio = Audio::Mute;
            mp4_options.overlays.clear();
            mp4_options.captions.clear();
            let source_metadata = post::inspect(&segment.path).map_err(internal)?;
            let (width, height) =
                output_dimensions(source_metadata.width, source_metadata.height, &mp4_options);
            let fps = mp4_options
                .fps
                .unwrap_or(source_metadata.fps.round().clamp(1.0, 120.0) as u32);
            let (mut output_captions, mut source_captions): (Vec<_>, Vec<_>) =
                std::mem::take(&mut gif_options.captions)
                    .into_iter()
                    .partition(|caption| caption.timeline == CaptionTimeline::Output);
            remap_source_captions(&mut source_captions, &segments, &origins, 1)?;
            if captions_from_recording {
                add_recorded_captions(
                    &mut source_captions,
                    marks
                        .as_ref()
                        .expect("marker metadata required for captions"),
                    &segments,
                    width,
                    height,
                    0,
                    CaptionFit::StagedGif,
                )?;
            }
            let metadata = run_export(should_cancel, |cancelled| {
                post::export_mp4_cancellable(&segments, &mp4_path, mp4_options, |_| {}, cancelled)
            })?;
            let frames = metadata
                .frames
                .ok_or_else(|| internal("edited GIF intermediate has no frame count"))?;
            output_captions.extend(
                post::map_captions_for_gif(&source_captions, &segments, frames, fps)
                    .map_err(internal)?,
            );
            gif_options.captions = output_captions;
            run_export(should_cancel, |cancelled| {
                post::export_gif_cancellable(
                    &mp4_path,
                    &out,
                    (0.0, metadata.duration),
                    gif_options,
                    |_| {},
                    cancelled,
                )
            })
        } else {
            run_export(should_cancel, |cancelled| {
                post::export_gif_cancellable(
                    &segment.path,
                    &out,
                    (segment.from, segment.to),
                    gif_options,
                    |_| {},
                    cancelled,
                )
            })
        }
    } else {
        for key in [
            "max_width",
            "loop",
            "fade",
            "dither",
            "dither_mode",
            "palette_size",
            "optimize_frames",
            "max_bytes",
        ] {
            if a.has(key) {
                return Err(invalid(format!("{key} is only supported for GIF exports")));
            }
        }
        let mut options = options(a)?;
        remap_source_captions(&mut options.captions, &segments, &origins, 1)?;
        if captions_from_recording {
            let source_metadata = post::inspect(&segment.path).map_err(internal)?;
            let (width, height) =
                output_dimensions(source_metadata.width, source_metadata.height, &options);
            add_recorded_captions(
                &mut options.captions,
                marks
                    .as_ref()
                    .expect("marker metadata required for captions"),
                &segments,
                width,
                height,
                0,
                CaptionFit::Video,
            )?;
        }
        run_export(should_cancel, |cancelled| {
            post::export_mp4_cancellable(&segments, &out, options, |_| {}, cancelled)
        })
    };
    serde_json::to_value(result?).map_err(internal)
}

fn join_segments(a: &Args) -> Result<Vec<Segment>, jiffshot_control::Error> {
    let value = match a.0.get("segments") {
        Some(Value::String(s)) => {
            serde_json::from_str(s).map_err(|e| invalid(format!("invalid segments JSON: {e}")))?
        }
        Some(v) => v.clone(),
        None => return Err(invalid("segments is required")),
    };
    let entries = value.as_array().ok_or_else(|| {
        invalid("segments must be a JSON array of {path, from?, to?, speed?} objects")
    })?;
    if entries.is_empty() || entries.len() > 256 {
        return Err(invalid("provide 1..256 segments"));
    }
    entries
        .iter()
        .enumerate()
        .map(|(i, value)| {
            let args = Args::new(value.clone())?;
            args.only(&["path", "from", "to", "speed"])?;
            segment(&args).map_err(|e| invalid(format!("segment {i}: {e}")))
        })
        .collect()
}

fn join(a: &Args, should_cancel: &dyn Fn() -> bool) -> Reply {
    a.only(&[
        "segments",
        "out",
        "fps",
        "width",
        "height",
        "duration",
        "mute",
        "overlays",
        "captions_from_recording",
        "transition",
        "idle_after",
        "idle_mode",
        "idle_speed",
        "min_pause",
    ])?;
    let mut segments = join_segments(a)?;
    let original_count = segments.len();
    let mut origins: Vec<usize> = (0..original_count).collect();
    let idle = idle_options(a)?;
    if let Some(idle) = idle {
        let mut edited = Vec::new();
        let mut edited_origins = Vec::new();
        for (index, segment) in segments.into_iter().enumerate() {
            let marks = recording_marks::read(&segment.path)
                .map_err(invalid)?
                .ok_or_else(|| invalid("recording markers are unavailable for idle editing"))?;
            let selected = edit_idle_segments(vec![segment], idle, &marks)?;
            edited_origins.extend(std::iter::repeat_n(index, selected.len()));
            edited.extend(selected);
        }
        segments = edited;
        origins = edited_origins;
    }
    let out = a
        .out("out", &["mp4"])?
        .ok_or_else(|| invalid("out is required"))?;
    let mut options = options(a)?;
    if options.transition_seconds.is_some() && idle.is_some() {
        if original_count < 2 {
            return Err(invalid(
                "transitions require at least two joined source segments",
            ));
        }
        options.transition_boundaries =
            Some(origins.windows(2).map(|pair| pair[0] != pair[1]).collect());
    }
    remap_source_captions(&mut options.captions, &segments, &origins, original_count)?;
    if a.bool("captions_from_recording")? {
        let first = post::inspect(&segments[0].path).map_err(internal)?;
        let (width, height) = output_dimensions(first.width, first.height, &options);
        for (index, segment) in segments.iter().enumerate() {
            let marks = recording_marks::read(&segment.path)
                .map_err(invalid)?
                .ok_or_else(|| invalid("recording markers are unavailable for this video"))?;
            add_recorded_captions(
                &mut options.captions,
                &marks,
                std::slice::from_ref(segment),
                width,
                height,
                index,
                CaptionFit::Video,
            )?;
        }
    }
    serde_json::to_value(run_export(should_cancel, |cancelled| {
        post::export_mp4_cancellable(&segments, &out, options, |_| {}, cancelled)
    })?)
    .map_err(internal)
}

fn remap_source_captions(
    captions: &mut Vec<Caption>,
    segments: &[Segment],
    origins: &[usize],
    original_count: usize,
) -> Result<(), jiffshot_control::Error> {
    if segments.len() != origins.len() {
        return Err(internal("edited segment origins are inconsistent"));
    }
    let mut mapped = Vec::with_capacity(captions.len());
    for caption in std::mem::take(captions) {
        if caption.timeline == CaptionTimeline::Output {
            mapped.push(caption);
            continue;
        }
        let origin = caption
            .segment
            .filter(|index| *index < original_count)
            .ok_or_else(|| invalid("source-timeline captions require a valid segment index"))?;
        for (index, (segment, source)) in segments.iter().zip(origins).enumerate() {
            if *source == origin && caption.from < segment.to && caption.to > segment.from {
                let mut selected = caption.clone();
                selected.segment = Some(index);
                mapped.push(selected);
            }
        }
    }
    *captions = mapped;
    Ok(())
}

#[derive(Clone, Copy)]
enum IdleMode {
    Speed,
    Cut,
}

#[derive(Clone, Copy)]
struct IdleOptions {
    after: f64,
    mode: IdleMode,
    speed: f64,
    min_pause: f64,
}

fn idle_options(a: &Args) -> Result<Option<IdleOptions>, jiffshot_control::Error> {
    let after = a.number("idle_after")?;
    let mode = a.string("idle_mode")?;
    let speed = a.number("idle_speed")?;
    let min_pause = a.number("min_pause")?;
    if after.is_none() && mode.is_none() {
        if speed.is_some() || min_pause.is_some() {
            return Err(invalid(
                "idle_speed and min_pause require idle_after and idle_mode",
            ));
        }
        return Ok(None);
    }
    let after = after.ok_or_else(|| invalid("idle_after is required with idle_mode"))?;
    let mode = match mode.as_deref() {
        Some("speed") => IdleMode::Speed,
        Some("cut") => IdleMode::Cut,
        _ => return Err(invalid("idle_mode must be speed or cut")),
    };
    if !after.is_finite() || after <= 0.0 {
        return Err(invalid("idle_after must be a positive number of seconds"));
    }
    let speed = speed.unwrap_or(4.0);
    if !speed.is_finite() || !(1.0..=100.0).contains(&speed) {
        return Err(invalid("idle_speed must be finite and in 1..=100"));
    }
    let min_pause = min_pause.unwrap_or(0.5);
    if !min_pause.is_finite() || min_pause < 0.0 {
        return Err(invalid(
            "min_pause must be a finite non-negative number of seconds",
        ));
    }
    Ok(Some(IdleOptions {
        after,
        mode,
        speed,
        min_pause,
    }))
}

fn edit_idle_segments(
    segments: Vec<Segment>,
    options: IdleOptions,
    marks: &recording_marks::RecordingMarks,
) -> Result<Vec<Segment>, jiffshot_control::Error> {
    if !marks.activities_complete() {
        return Err(invalid(
            "idle editing is unavailable because activity history is incomplete",
        ));
    }
    let activities: Vec<_> = marks
        .activities()
        .iter()
        .map(|activity| activity.at_ms)
        .collect();
    let mut edited = Vec::new();
    for segment in segments {
        let idle = post::idle_ranges(
            &segment.path,
            segment.from,
            segment.to,
            options.after,
            &activities,
        )
        .map_err(internal)?;
        edited.extend(segments_for_idle_ranges(segment, &idle, options));
    }
    if edited.is_empty() || edited.len() > 256 {
        return Err(invalid(
            "idle editing must produce between 1 and 256 video segments",
        ));
    }
    Ok(edited)
}

fn segments_for_idle_ranges(
    segment: Segment,
    idle: &[(f64, f64)],
    options: IdleOptions,
) -> Vec<Segment> {
    let mut edited = Vec::new();
    let mut cursor = segment.from;
    for (start, end) in idle {
        if cursor < *start {
            edited.push(Segment {
                path: segment.path.clone(),
                from: cursor,
                to: *start,
                speed: segment.speed,
            });
        }
        let retained_end = (start + options.min_pause).min(*end);
        if *start < retained_end {
            edited.push(Segment {
                path: segment.path.clone(),
                from: *start,
                to: retained_end,
                speed: segment.speed,
            });
        }
        if retained_end < *end {
            if matches!(options.mode, IdleMode::Speed) {
                edited.push(Segment {
                    path: segment.path.clone(),
                    from: retained_end,
                    to: *end,
                    speed: segment.speed * options.speed,
                });
            }
            cursor = *end;
        } else {
            cursor = retained_end;
        }
    }
    if cursor < segment.to {
        edited.push(Segment {
            path: segment.path,
            from: cursor,
            to: segment.to,
            speed: segment.speed,
        });
    }
    edited
}

fn output_dimensions(width: u32, height: u32, options: &Mp4Options) -> (u32, u32) {
    let (source_width, source_height) = (width, height);
    let (width, height) = match (options.width, options.height) {
        (None, None) => (width, height),
        (Some(width), Some(height)) => (width, height),
        (Some(width), None) => (
            width,
            (f64::from(source_height) * f64::from(width) / f64::from(source_width)).round() as u32,
        ),
        (None, Some(height)) => (
            (f64::from(source_width) * f64::from(height) / f64::from(source_height)).round() as u32,
            height,
        ),
    };
    ((width & !1).max(1), (height & !1).max(1))
}

#[derive(Clone, Copy)]
enum CaptionFit {
    Video,
    Gif,
    StagedGif,
}

fn add_recorded_captions(
    captions: &mut Vec<Caption>,
    marks: &recording_marks::RecordingMarks,
    segments: &[Segment],
    output_width: u32,
    output_height: u32,
    segment_index_offset: usize,
    fit: CaptionFit,
) -> Result<(), jiffshot_control::Error> {
    let (source_width, source_height) = marks.dimensions();
    let placement = match fit {
        CaptionFit::Video | CaptionFit::StagedGif => {
            post::frame_placement((source_width, source_height), (output_width, output_height))
        }
        CaptionFit::Gif
            if source_width > 0 && source_height > 0 && output_width > 0 && output_height > 0 =>
        {
            Some(post::FramePlacement {
                x: 0,
                y: 0,
                width: output_width,
                height: output_height,
                unscaled: false,
            })
        }
        CaptionFit::Gif => None,
    }
    .ok_or_else(|| invalid("recording caption dimensions must be non-zero"))?;
    let scale_x = f64::from(placement.width) / f64::from(source_width);
    let scale_y = f64::from(placement.height) / f64::from(source_height);
    let font_scale = (scale_x + scale_y) / 2.0;
    for (segment_index, segment) in segments.iter().enumerate() {
        for (caption_index, recorded) in marks.captions().iter().enumerate() {
            if recorded.burned_in {
                continue;
            }
            let from = recorded.at_ms as f64 / 1000.0;
            let to = recorded
                .duration_ms
                .map(|duration| from + duration as f64 / 1000.0)
                .or_else(|| {
                    marks
                        .captions()
                        .iter()
                        .skip(caption_index + 1)
                        .map(|next| next.at_ms as f64 / 1000.0)
                        .find(|next| *next >= from)
                })
                .unwrap_or_else(|| marks.duration_seconds());
            if to <= from || from >= segment.to || to <= segment.from {
                continue;
            }
            captions.push(Caption {
                text: recorded.text.clone(),
                x: (f64::from(recorded.x) * scale_x + f64::from(placement.x)).round() as i32,
                y: (f64::from(recorded.y) * scale_y + f64::from(placement.y)).round() as i32,
                from,
                to,
                timeline: CaptionTimeline::Source,
                segment: Some(segment_index + segment_index_offset),
                font: recorded.font.clone(),
                size: (recorded.size as f64 * font_scale) as f32,
                color: recorded.color,
                background: recorded.background,
                padding: (f64::from(recorded.padding) * font_scale).round() as u32,
                fade: 0.0,
                fade_window: None,
                reference_canvas: match fit {
                    CaptionFit::Video => None,
                    CaptionFit::Gif | CaptionFit::StagedGif => Some((output_width, output_height)),
                },
            });
        }
    }
    Ok(())
}

fn overlays(a: &Args) -> Result<(Vec<Overlay>, Vec<Caption>), jiffshot_control::Error> {
    let value = match a.0.get("overlays") {
        None | Some(Value::Null) => return Ok((Vec::new(), Vec::new())),
        Some(Value::String(s)) => {
            serde_json::from_str(s).map_err(|e| invalid(format!("invalid overlays JSON: {e}")))?
        }
        Some(v) => v.clone(),
    };
    let entries = value.as_array().ok_or_else(|| {
        invalid("overlays must be an array of timed image or text overlay objects")
    })?;
    if entries.len() > 256 {
        return Err(invalid("provide at most 256 overlays"));
    }
    let mut images = Vec::new();
    let mut captions = Vec::new();
    for v in entries {
        let a = Args::new(v.clone())?;
        if !a.has("from") || !a.has("to") {
            return Err(invalid(
                "overlay from and to are required on the output timeline",
            ));
        }
        let (from, to) = (finite(&a, "from", 0.0)?, finite(&a, "to", 0.0)?);
        if from < 0.0 || to <= from {
            return Err(invalid("overlay range must satisfy 0 <= from < to"));
        }
        let x = a.int("x", -8192, 8192)?.unwrap_or(0) as i32;
        let y = a.int("y", -8192, 8192)?.unwrap_or(0) as i32;
        if a.has("text") {
            if a.has("path") {
                return Err(invalid("an overlay cannot have both path and text"));
            }
            a.only(&[
                "text",
                "x",
                "y",
                "from",
                "to",
                "timeline",
                "segment",
                "font",
                "size",
                "color",
                "background",
                "padding",
                "fade",
            ])?;
            let text = a
                .string("text")?
                .ok_or_else(|| invalid("caption text is required"))?;
            let font = a
                .string("font")?
                .unwrap_or_else(|| jiffshot_doc::text::DEFAULT_FAMILY.to_owned());
            if font.trim().is_empty() {
                return Err(invalid("caption font must not be empty"));
            }
            let size = finite(&a, "size", 28.0)? as f32;
            let timeline = a
                .string("timeline")?
                .map(|timeline| match timeline.as_str() {
                    "output" => Ok(CaptionTimeline::Output),
                    "source" => Ok(CaptionTimeline::Source),
                    _ => Err(invalid("caption timeline must be output or source")),
                })
                .transpose()?
                .unwrap_or_default();
            let segment = a.int("segment", 0, 255)?.map(|segment| segment as usize);
            if timeline == CaptionTimeline::Output && segment.is_some() {
                return Err(invalid(
                    "caption segment is only valid for source timelines",
                ));
            }
            if timeline == CaptionTimeline::Source && segment.is_none() {
                return Err(invalid("source-timeline captions require a segment index"));
            }
            let color = a
                .string("color")?
                .map(|value| {
                    jiffshot_doc::Rgba::parse(&value)
                        .ok_or_else(|| invalid(format!("invalid caption color {value:?}")))
                })
                .transpose()?
                .unwrap_or(jiffshot_doc::Rgba::WHITE);
            let background = a
                .string("background")?
                .map(|value| {
                    jiffshot_doc::Rgba::parse(&value)
                        .ok_or_else(|| invalid(format!("invalid caption background {value:?}")))
                })
                .transpose()?
                .unwrap_or(jiffshot_doc::Rgba::new(0, 0, 0, 176));
            captions.push(Caption {
                text,
                x,
                y,
                from,
                to,
                timeline,
                segment,
                font,
                size,
                color,
                background,
                padding: a.int("padding", 0, 1024)?.unwrap_or(8) as u32,
                fade: finite(&a, "fade", 0.0)?,
                fade_window: None,
                reference_canvas: None,
            });
        } else {
            a.only(&["path", "x", "y", "from", "to"])?;
            images.push(Overlay {
                path: source(&a)?,
                x,
                y,
                from,
                to,
            });
        }
    }
    Ok((images, captions))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiffshot_capture::media::{AudioSpec, VideoSpec, Writer, HNS_PER_SECOND};
    use jiffshot_control::ErrorCode;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixtures(PathBuf);

    impl Fixtures {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::current_dir()
                .expect("current directory")
                .join(format!(
                    ".video-handler-tests-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir(&path).expect("unique fixture directory");
            Self(path)
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
        fn video(&self, name: &str, audio: Option<AudioSpec>) -> PathBuf {
            let path = self.path(name);
            let spec = VideoSpec::for_size(64, 48, 12);
            let mut writer = Writer::create(&path, spec, audio).expect("fixture encoder");
            let pixels = [0_u8, 0, 240, 0].repeat(64 * 48);
            for n in 0..12 {
                let at = n * HNS_PER_SECOND / 12;
                let duration = (n + 1) * HNS_PER_SECOND / 12 - at;
                writer
                    .write_frame(&pixels, at, duration)
                    .expect("fixture frame");
                if let Some(audio) = audio {
                    let frames = audio.sample_rate as usize / 12;
                    writer
                        .write_audio(&vec![1000; frames * audio.channels as usize], at, duration)
                        .expect("fixture audio");
                }
            }
            writer.finish().expect("finalized fixture");
            path
        }
        fn run(&self, command: &str, value: Value) -> Reply {
            dispatch(command, &Args::new(value).expect("object args"), &|| false)
        }
    }

    impl Drop for Fixtures {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn temporary_outputs_reserve_distinct_files_and_clean_up() {
        let first = TempOutput::reserve("jiffshot-test-temp", "tmp").expect("first reservation");
        let second = TempOutput::reserve("jiffshot-test-temp", "tmp").expect("second reservation");
        let first_path = first.0.clone();
        let second_path = second.0.clone();
        assert_ne!(first_path, second_path);

        fs::write(&first_path, b"first").expect("write first temporary");
        fs::write(&second_path, b"second").expect("write second temporary");
        drop(first);
        drop(second);

        assert!(!first_path.exists());
        assert!(!second_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn temporary_video_outputs_remain_private_after_replacement() {
        use std::os::unix::fs::PermissionsExt;

        let temporary =
            TempOutput::reserve("jiffshot-test-private", "mp4").expect("temporary video output");
        let directory = temporary.0.parent().unwrap().to_path_buf();
        assert_eq!(
            fs::metadata(&temporary.0)
                .expect("temporary file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let replacement = directory.join("replacement.mp4");
        fs::write(&replacement, b"rendered").unwrap();
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o644)).unwrap();
        fs::rename(&replacement, &temporary.0).unwrap();
        assert_eq!(
            fs::metadata(&temporary.0).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        drop(temporary);
        assert!(!directory.exists());
    }

    #[test]
    fn idle_edit_retains_minimum_pause_then_speeds_or_cuts_the_remainder() {
        let segment = Segment {
            path: PathBuf::from("source.mp4"),
            from: 0.0,
            to: 10.0,
            speed: 2.0,
        };
        let idle = [(2.0, 6.0), (7.0, 9.0)];
        let options = IdleOptions {
            after: 1.0,
            mode: IdleMode::Speed,
            speed: 4.0,
            min_pause: 0.5,
        };
        let speed_ranges: Vec<_> = segments_for_idle_ranges(segment.clone(), &idle, options)
            .into_iter()
            .map(|part| (part.from, part.to, part.speed))
            .collect();
        assert_eq!(
            speed_ranges,
            [
                (0.0, 2.0, 2.0),
                (2.0, 2.5, 2.0),
                (2.5, 6.0, 8.0),
                (6.0, 7.0, 2.0),
                (7.0, 7.5, 2.0),
                (7.5, 9.0, 8.0),
                (9.0, 10.0, 2.0)
            ]
        );

        let cut_ranges: Vec<_> = segments_for_idle_ranges(
            segment,
            &idle,
            IdleOptions {
                mode: IdleMode::Cut,
                ..options
            },
        )
        .into_iter()
        .map(|part| (part.from, part.to))
        .collect();
        assert_eq!(
            cut_ranges,
            [(0.0, 2.0), (2.0, 2.5), (6.0, 7.0), (7.0, 7.5), (9.0, 10.0)]
        );
    }

    #[test]
    fn rejects_unexpected_video_arguments_and_empty_join() {
        let args = Args::new(json!({"paths":[]})).expect("object");
        assert!(dispatch("video.info", &args, &|| false)
            .expect_err("typo")
            .message
            .contains("unexpected argument"));
        let args = Args::new(json!({"segments":[]})).expect("object");
        assert!(dispatch("video.join", &args, &|| false)
            .expect_err("empty")
            .message
            .contains("1..256"));
    }

    #[test]
    fn exact_video_info_reports_decoded_frames_and_preserves_fast_inspection() {
        let fixtures = Fixtures::new();
        let source = fixtures.video("source.mp4", None);
        for args in [
            json!({"path": source}),
            json!({"path": source, "exact": false}),
        ] {
            let info = fixtures.run("video.info", args).expect("fast video info");
            assert!(info["frames"].is_null());
            assert!(info.get("constant_frame_rate").is_none());
        }
        let exact = fixtures
            .run("video.info", json!({"path": source, "exact": true}))
            .expect("exact video info");
        assert_eq!(exact["frames"], 12);
        assert_eq!(exact["constant_frame_rate"], true);
        let error = fixtures
            .run("video.info", json!({"path": source, "exact": "maybe"}))
            .expect_err("invalid exact option");
        assert_eq!(error.code, jiffshot_control::ErrorCode::InvalidRequest);
    }

    #[test]
    fn cancellation_from_the_host_preserves_the_existing_output() {
        let fixtures = Fixtures::new();
        let source = fixtures.video("source.mp4", None);
        let destination = fixtures.path("destination.mp4");
        fs::write(&destination, b"existing output").expect("existing output");

        let error = crate::dispatch(
            "video.export",
            json!({"path": source, "out": destination, "mute": true}),
            &|| true,
        )
        .expect_err("the host cancelled the export");
        assert_eq!(error.code, ErrorCode::Conflict);
        assert!(error.message.contains("cancelled"));
        assert_eq!(fs::read(destination).unwrap(), b"existing output");
    }

    #[test]
    fn rejects_relative_video_sources_and_nonfinite_times() {
        assert!(path("relative.mp4")
            .expect_err("relative path")
            .message
            .contains("absolute"));
        let args = Args::new(json!({"at":"NaN"})).expect("object");
        assert!(finite(&args, "at", 0.0)
            .expect_err("NaN")
            .message
            .contains("finite"));
        let args = Args::new(json!({"fps":0})).expect("object");
        assert!(options(&args).is_err());
    }

    #[test]
    fn segment_speed_defaults_to_one_and_validates_finite_positive_bounds() {
        let fixtures = Fixtures::new();
        let source = fixtures.video("speed-source.mp4", None);
        let default =
            segment(&Args::new(json!({"path":source})).expect("default-speed segment arguments"))
                .expect("default-speed segment");
        assert_eq!(default.speed, 1.0);

        for speed in [0.01, 100.0] {
            let parsed = segment(
                &Args::new(json!({"path":source,"speed":speed}))
                    .expect("boundary-speed segment arguments"),
            )
            .expect("inclusive speed boundary");
            assert_eq!(parsed.speed, speed);
        }

        for speed in ["0", "-1", "0.009", "100.01", "NaN", "Infinity"] {
            let error = segment(
                &Args::new(json!({"path":source,"speed":speed}))
                    .expect("invalid-speed segment arguments"),
            )
            .expect_err("invalid speed");
            assert_eq!(error.code, ErrorCode::InvalidRequest, "speed {speed}");
            assert!(
                error.message.contains("speed"),
                "speed {speed} should identify the invalid field: {error}"
            );
        }
    }

    #[test]
    fn caption_overlay_parses_timing_position_and_text_style_options() {
        let args = Args::new(json!({
            "overlays": [{
                "text": "Release notes",
                "x": 17,
                "y": 23,
                "from": 0.4,
                "to": 2.5,
                "font": "Arial",
                "size": 32,
                "color": "#FFFF0000",
                "background": "#8000FF00",
                "padding": 9,
                "fade": 0.2
            }]
        }))
        .expect("caption overlay arguments");

        let (images, captions) = overlays(&args).expect("caption options");

        assert!(images.is_empty());
        assert_eq!(captions.len(), 1);
        let caption = &captions[0];
        assert_eq!(caption.text, "Release notes");
        assert_eq!((caption.x, caption.y), (17, 23));
        assert_eq!((caption.from, caption.to), (0.4, 2.5));
        assert_eq!(caption.timeline, CaptionTimeline::Output);
        assert_eq!(caption.segment, None);
        assert_eq!(caption.font, "Arial");
        assert_eq!(caption.size, 32.0);
        assert_eq!(caption.color, jiffshot_doc::Rgba::rgb(255, 0, 0));
        assert_eq!(caption.background, jiffshot_doc::Rgba::new(0, 255, 0, 128));
        assert_eq!(caption.padding, 9);
        assert_eq!(caption.fade, 0.2);
    }

    #[test]
    fn caption_overlay_requires_a_segment_for_source_timeline() {
        let (_, captions) = overlays(
            &Args::new(json!({
                "overlays": [{
                    "text": "Source caption",
                    "from": 1.0,
                    "to": 2.0,
                    "timeline": "source",
                    "segment": 2
                }]
            }))
            .expect("source caption arguments"),
        )
        .expect("source caption");
        assert_eq!(captions[0].timeline, CaptionTimeline::Source);
        assert_eq!(captions[0].segment, Some(2));

        for invalid_caption in [
            json!({"text":"Missing segment","from":0,"to":1,"timeline":"source"}),
            json!({"text":"Unexpected segment","from":0,"to":1,"segment":0}),
            json!({"text":"Unknown timeline","from":0,"to":1,"timeline":"recording"}),
        ] {
            assert!(
                overlays(&Args::new(json!({"overlays":[invalid_caption]})).unwrap()).is_err(),
                "invalid source timeline caption should be rejected"
            );
        }
    }

    #[test]
    fn recording_text_and_step_captions_follow_source_time_and_scale_to_output() {
        let marks = crate::recording_marks::RecordingMarks::new(1_000, 30, 32, 24, Vec::new())
            .with_captions(vec![
                crate::recording_marks::RecordedCaption {
                    at_ms: 100,
                    duration_ms: None,
                    burned_in: false,
                    text: "First".into(),
                    x: 4,
                    y: 5,
                    font: "Arial".into(),
                    size: 12.0,
                    color: jiffshot_doc::Rgba::WHITE,
                    background: jiffshot_doc::Rgba::new(0, 0, 0, 128),
                    padding: 2,
                },
                crate::recording_marks::RecordedCaption {
                    at_ms: 600,
                    duration_ms: Some(200),
                    burned_in: false,
                    text: "Second".into(),
                    x: 6,
                    y: 7,
                    font: "Arial".into(),
                    size: 12.0,
                    color: jiffshot_doc::Rgba::WHITE,
                    background: jiffshot_doc::Rgba::new(0, 0, 0, 128),
                    padding: 2,
                },
            ]);
        let segments = [Segment {
            path: PathBuf::new(),
            from: 0.2,
            to: 0.9,
            speed: 1.0,
        }];
        let mut captions = Vec::new();

        add_recorded_captions(
            &mut captions,
            &marks,
            &segments,
            64,
            48,
            0,
            CaptionFit::Video,
        )
        .expect("recorded captions");

        assert_eq!(captions.len(), 2);
        assert_eq!((captions[0].from, captions[0].to), (0.1, 0.6));
        assert_eq!((captions[0].x, captions[0].y), (8, 10));
        assert_eq!(captions[0].size, 24.0);
        assert_eq!((captions[1].from, captions[1].to), (0.6, 0.8));
        assert_eq!((captions[1].x, captions[1].y), (12, 14));

        let mut letterboxed = Vec::new();
        add_recorded_captions(
            &mut letterboxed,
            &marks,
            &segments,
            64,
            64,
            0,
            CaptionFit::Video,
        )
        .expect("letterboxed captions");
        assert_eq!((letterboxed[0].x, letterboxed[0].y), (8, 18));
        assert_eq!(letterboxed[0].size, 24.0);
    }

    #[test]
    fn burned_in_annotations_are_not_rendered_twice_with_guide_captions() {
        let marks: recording_marks::RecordingMarks = serde_json::from_value(json!({
            "version": 1,
            "duration_ms": 1000,
            "fps": 30,
            "width": 32,
            "height": 24,
            "markers": [],
            "captions": [
                {
                    "at_ms": 100, "duration_ms": null, "text": "guide",
                    "x": 4, "y": 5, "font": "Arial", "size": 12,
                    "color": "#FFFFFFFF", "background": "#80000000", "padding": 2
                },
                {
                    "at_ms": 600, "duration_ms": 200, "text": "annotation",
                    "x": 6, "y": 7, "font": "Arial", "size": 12,
                    "color": "#FFFFFFFF", "background": "#80000000", "padding": 2,
                    "burned_in": true
                }
            ]
        }))
        .expect("recorded captions");
        let segments = [Segment {
            path: PathBuf::new(),
            from: 0.0,
            to: 1.0,
            speed: 1.0,
        }];
        let mut captions = Vec::new();

        add_recorded_captions(
            &mut captions,
            &marks,
            &segments,
            32,
            24,
            0,
            CaptionFit::Video,
        )
        .expect("recorded captions");

        assert_eq!(captions.len(), 1);
        assert_eq!(captions[0].text, "guide");
        assert_eq!((captions[0].from, captions[0].to), (0.1, 0.6));
    }

    #[test]
    fn simultaneous_guide_steps_keep_only_the_last_caption_at_that_time() {
        let marks = recording_marks::RecordingMarks::new(1_000, 30, 32, 24, Vec::new())
            .with_captions(vec![
                recording_marks::guide_caption(100, Some(24), "first".into()).unwrap(),
                recording_marks::guide_caption(100, Some(24), "second".into()).unwrap(),
                recording_marks::guide_caption(600, Some(24), "third".into()).unwrap(),
            ]);
        let segments = [Segment {
            path: PathBuf::new(),
            from: 0.0,
            to: 1.0,
            speed: 1.0,
        }];
        let mut captions = Vec::new();

        add_recorded_captions(
            &mut captions,
            &marks,
            &segments,
            32,
            24,
            0,
            CaptionFit::Video,
        )
        .unwrap();

        assert_eq!(
            captions
                .iter()
                .map(|caption| (caption.text.as_str(), caption.from, caption.to))
                .collect::<Vec<_>>(),
            [("second", 0.1, 0.6), ("third", 0.6, 1.0)]
        );
    }

    #[test]
    fn exports_preserve_burned_annotations_and_render_guide_captions() {
        let fixtures = Fixtures::new();
        let source = fixtures.video("mixed-recording.mp4", None);
        let caption = |text: &str, x, y, burned_in| recording_marks::RecordedCaption {
            at_ms: 0,
            duration_ms: Some(1000),
            burned_in,
            text: text.into(),
            x,
            y,
            font: jiffshot_doc::text::DEFAULT_FAMILY.into(),
            size: 12.0,
            color: jiffshot_doc::Rgba::WHITE,
            background: jiffshot_doc::Rgba::BLACK,
            padding: 2,
        };
        let markers = vec![
            recording_marks::Marker::Label {
                at_ms: 0,
                label: "selection".into(),
            },
            recording_marks::Marker::Label {
                at_ms: 1000,
                label: "selection".into(),
            },
        ];
        let marks =
            recording_marks::RecordingMarks::new(1000, 12, 64, 48, markers).with_captions(vec![
                caption("annotation", 2, 2, true),
                caption("guide", 32, 24, false),
            ]);
        recording_marks::write(&source, &marks).expect("write recording sidecar");

        for (command, name, args) in [
            (
                "video.export",
                "mixed.mp4",
                json!({"path":source,"fps":12,"captions_from_recording":true}),
            ),
            (
                "video.export",
                "mixed.gif",
                json!({"path":source,"fps":10,"captions_from_recording":true}),
            ),
            (
                "video.export",
                "mixed-marked.gif",
                json!({"path":source,"fps":10,"captions_from_recording":true,"keep_marked":true}),
            ),
            (
                "video.join",
                "mixed-joined.mp4",
                json!({"segments":[{"path":source}],"fps":12,"captions_from_recording":true}),
            ),
        ] {
            let output = fixtures.path(name);
            let mut args = args;
            args["out"] = json!(output);
            fixtures
                .run(command, args)
                .expect("export recorded captions");
            let (frame, _) = post::frame(&output, 0.2).expect("decode exported frame");
            let annotation_pixel = frame.pixels()[3 * 64 + 3];
            let guide_pixel = frame.pixels()[25 * 64 + 33];
            assert!(
                annotation_pixel.red() > 150,
                "the burned annotation must not be redrawn: {name}: {annotation_pixel:?}"
            );
            assert!(
                guide_pixel.red() < 100,
                "the guide caption must be rendered: {name}: {guide_pixel:?}"
            );
        }
    }

    #[test]
    fn zero_duration_recorded_caption_is_not_exported() {
        let marks = recording_marks::RecordingMarks::new(1_000, 30, 32, 24, Vec::new())
            .with_captions(vec![recording_marks::RecordedCaption {
                at_ms: 500,
                duration_ms: Some(0),
                burned_in: false,
                text: "cleared while paused".into(),
                x: 2,
                y: 3,
                font: jiffshot_doc::text::DEFAULT_FAMILY.into(),
                size: 12.0,
                color: jiffshot_doc::Rgba::WHITE,
                background: jiffshot_doc::Rgba::TRANSPARENT,
                padding: 0,
            }]);
        let segments = [Segment {
            path: PathBuf::new(),
            from: 0.0,
            to: 1.0,
            speed: 1.0,
        }];
        let mut captions = Vec::new();
        add_recorded_captions(
            &mut captions,
            &marks,
            &segments,
            32,
            24,
            0,
            CaptionFit::Video,
        )
        .unwrap();
        assert!(captions.is_empty());
    }

    #[test]
    fn recorded_caption_uses_uncropped_coordinates_for_odd_sized_video() {
        let marks = recording_marks::RecordingMarks::new(1000, 30, 65, 49, Vec::new())
            .with_captions(vec![recording_marks::RecordedCaption {
                at_ms: 0,
                duration_ms: Some(1000),
                burned_in: false,
                text: "A".into(),
                x: 32,
                y: 24,
                font: jiffshot_doc::text::DEFAULT_FAMILY.into(),
                size: 12.0,
                color: jiffshot_doc::Rgba::WHITE,
                background: jiffshot_doc::Rgba::rgb(0, 0, 0),
                padding: 2,
            }]);
        let segments = [Segment {
            path: PathBuf::new(),
            from: 0.0,
            to: 1.0,
            speed: 1.0,
        }];
        let mut captions = Vec::new();
        add_recorded_captions(
            &mut captions,
            &marks,
            &segments,
            64,
            48,
            0,
            CaptionFit::Video,
        )
        .expect("odd-sized recorded caption");
        assert_eq!((captions[0].x, captions[0].y), (32, 24));
        assert_eq!(captions[0].size, 12.0);
    }

    #[test]
    fn invalid_video_arguments_do_not_create_output() {
        let fixtures = Fixtures::new();
        let source = fixtures.video("source.mp4", None);
        let output = fixtures.path("output.mp4");
        let cases = [
            (
                "video.frame",
                json!({"path":source,"out":fixtures.path("frame.png"),"at":1.0}),
            ),
            (
                "video.frame",
                json!({"path":source,"out":fixtures.path("frame.png"),"at":"Infinity"}),
            ),
            (
                "video.export",
                json!({"path":source,"out":output,"from":0.5,"to":0.5}),
            ),
            (
                "video.export",
                json!({"path":source,"out":output,"from":-0.1}),
            ),
            ("video.export", json!({"path":source,"out":output,"to":1.1})),
            ("video.export", json!({"path":source,"out":output,"fps":0})),
            (
                "video.export",
                json!({"path":source,"out":output,"idle_after":2}),
            ),
            (
                "video.export",
                json!({"path":source,"out":output,"idle_mode":"unknown","idle_after":2}),
            ),
            (
                "video.export",
                json!({"path":source,"out":output,"idle_mode":"speed","idle_after":2,"idle_speed":0}),
            ),
            (
                "video.export",
                json!({"path":source,"out":output,"idle_mode":"cut","idle_after":2,"min_pause":-1}),
            ),
            (
                "video.export",
                json!({"path":source,"out":output,"width":1}),
            ),
            (
                "video.export",
                json!({"path":source,"out":output,"loop":true}),
            ),
            (
                "video.export",
                json!({"path":source,"out":fixtures.path("output.gif"),"width":64}),
            ),
            (
                "video.export",
                json!({"path":source,"out":fixtures.path("output.gif"),"fps":51}),
            ),
            (
                "video.export",
                json!({"path":source,"out":output,"overlays":[{"path":source,"from":0.5,"to":0.25}]}),
            ),
            (
                "video.export",
                json!({"path":source,"out":output,"overlays":[{"path":source,"from":0.0}]}),
            ),
            ("video.join", json!({"out":output,"segments":"not JSON"})),
            (
                "video.join",
                json!({"out":output,"segments":[{"path":source,"typo":1}]}),
            ),
            (
                "video.join",
                json!({"out":fixtures.path("output.gif"),"segments":[{"path":source}]}),
            ),
        ];
        for (command, args) in cases {
            let error = fixtures
                .run(command, args.clone())
                .expect_err("invalid arguments");
            assert_eq!(
                error.code,
                ErrorCode::InvalidRequest,
                "{command}: {args}: {error}"
            );
        }
        for name in ["output.mp4", "output.gif", "frame.png"] {
            assert!(
                !fixtures.path(name).exists(),
                "invalid request created {name}"
            );
        }
    }

    fn assert_video_metadata(
        value: &Value,
        path: &std::path::Path,
        format: &str,
        frames: Option<u64>,
    ) {
        assert_eq!(value["path"], json!(path));
        assert_eq!(value["format"], format);
        assert_eq!(
            (value["width"].as_u64(), value["height"].as_u64()),
            (Some(64), Some(48))
        );
        assert!(value["fps"].as_f64().is_some_and(|fps| fps > 0.0));
        assert!(value["duration"]
            .as_f64()
            .is_some_and(|duration| duration > 0.0));
        assert_eq!(
            value["bytes"].as_u64(),
            Some(fs::metadata(path).expect("published output").len())
        );
        assert_eq!(value["frames"], json!(frames));
        assert!(value["audio"].is_null());
        assert!(value.get("audio_note").is_none());
    }

    #[test]
    fn handlers_return_finalized_info_still_mp4_gif_and_join_metadata() {
        let fixtures = Fixtures::new();
        let source = fixtures.video("source.mp4", None);
        let info = fixtures
            .run("video.info", json!({"path":source}))
            .expect("video info");
        assert_video_metadata(&info, &source, "mp4", None);
        let png = fixtures.path("frame.png");
        let still = fixtures
            .run("video.frame", json!({"path":source,"out":png,"at":0.25}))
            .expect("still");
        assert_eq!(still["path"], json!(png));
        assert_eq!(still["format"], "png");
        assert_eq!(still["requested_at"], 0.25);
        assert_eq!(
            (still["width"].as_u64(), still["height"].as_u64()),
            (Some(64), Some(48))
        );
        assert!((still["at"].as_f64().expect("frame time") - 0.25).abs() < 0.05);
        assert_eq!(
            still["bytes"].as_u64(),
            Some(fs::metadata(&png).expect("still file").len())
        );
        let mp4 = fixtures.path("trim.mp4");
        let trimmed = fixtures
            .run(
                "video.export",
                json!({"path":source,"out":mp4,"from":0.25,"to":0.75,"fps":20}),
            )
            .expect("trimmed MP4");
        assert_video_metadata(&trimmed, &mp4, "mp4", Some(10));
        assert!((trimmed["duration"].as_f64().expect("duration") - 0.5).abs() < 0.02);
        let gif = fixtures.path("animation.gif");
        let animation = fixtures
            .run(
                "video.export",
                json!({"path":source,"out":gif,"from":0.25,"to":0.75,"fps":20,"max_width":64}),
            )
            .expect("GIF export");
        assert_video_metadata(&animation, &gif, "gif", Some(10));
        assert!((animation["duration"].as_f64().expect("GIF duration") - 0.5).abs() < 0.001);
        let gif_info = fixtures
            .run("video.info", json!({"path":gif}))
            .expect("GIF info");
        assert_video_metadata(&gif_info, &gif, "gif", Some(10));
        assert_eq!(gif_info["codec"], "GIF");
        assert_eq!(gif_info["constant_frame_rate"], true);
        let joined = fixtures.path("join.mp4");
        let segments =
            json!([{"path":source,"from":0.0,"to":0.5},{"path":source,"from":0.5,"to":1.0}]);
        let joined_metadata = fixtures
            .run(
                "video.join",
                json!({"out":joined,"segments":segments.to_string(),"fps":20}),
            )
            .expect("joined MP4");
        assert_video_metadata(&joined_metadata, &joined, "mp4", Some(20));
        assert!((joined_metadata["duration"].as_f64().expect("duration") - 1.0).abs() < 0.02);
    }

    #[test]
    fn marker_selection_exports_kept_ranges_and_drops_discarded_ranges() {
        let fixtures = Fixtures::new();
        let source = fixtures.video("marked-source.mp4", None);
        let duration_ms =
            (post::inspect(&source).expect("source metadata").duration * 1000.0).round() as u64;
        crate::recording_marks::write(
            &source,
            &crate::recording_marks::RecordingMarks::new(
                duration_ms,
                20,
                64,
                48,
                vec![
                    crate::recording_marks::Marker::Label {
                        at_ms: 200,
                        label: "demo".into(),
                    },
                    crate::recording_marks::Marker::Label {
                        at_ms: 800,
                        label: "demo".into(),
                    },
                    crate::recording_marks::Marker::Discard { at_ms: 400 },
                    crate::recording_marks::Marker::Discard { at_ms: 500 },
                ],
            )
            .with_captions(vec![crate::recording_marks::RecordedCaption {
                at_ms: 550,
                duration_ms: Some(100),
                burned_in: false,
                text: "Recorded action".into(),
                x: 2,
                y: 2,
                font: jiffshot_doc::text::DEFAULT_FAMILY.into(),
                size: 12.0,
                color: jiffshot_doc::Rgba::WHITE,
                background: jiffshot_doc::Rgba::new(0, 0, 0, 176),
                padding: 2,
            }]),
        )
        .expect("recording marker sidecar");

        let mp4 = fixtures.path("selected.mp4");
        let metadata = fixtures
            .run(
                "video.export",
                json!({
                    "path": source,
                    "out": mp4,
                    "fps": 20,
                    "keep_marked": true,
                    "drop_discarded": true,
                    "captions_from_recording": true
                }),
            )
            .expect("selected MP4");
        assert_eq!(metadata["frames"], 10);
        assert!((metadata["duration"].as_f64().expect("duration") - 0.5).abs() < 0.03);

        let gif = fixtures.path("selected.gif");
        let gif_metadata = fixtures
            .run(
                "video.export",
                json!({
                    "path": source,
                    "out": gif,
                    "fps": 20,
                    "keep_marked": true,
                    "drop_discarded": true,
                    "captions_from_recording": true
                }),
            )
            .expect("selected GIF");
        assert_eq!(gif_metadata["frames"], 10);
        assert!((gif_metadata["duration"].as_f64().expect("GIF duration") - 0.5).abs() < 0.06);
    }

    #[test]
    fn source_captions_follow_later_marked_ranges_for_mp4_and_gif() {
        let fixtures = Fixtures::new();
        let source = fixtures.video("caption-ranges-source.mp4", None);
        let markers = [
            (0, "first"),
            (250, "first"),
            (750, "second"),
            (1000, "second"),
        ]
        .into_iter()
        .map(|(at_ms, label)| recording_marks::Marker::Label {
            at_ms,
            label: label.into(),
        })
        .collect();
        recording_marks::write(
            &source,
            &recording_marks::RecordingMarks::new(1000, 12, 64, 48, markers),
        )
        .expect("recording sidecar");

        for extension in ["mp4", "gif"] {
            let output = fixtures.path(&format!("caption-ranges.{extension}"));
            fixtures
                .run(
                    "video.export",
                    json!({
                        "path":source,
                        "out":output,
                        "fps":12,
                        "keep_marked":true,
                        "overlays":[{
                            "text":"Later",
                            "x":2,
                            "y":2,
                            "from":0.75,
                            "to":1.0,
                            "timeline":"source",
                            "segment":0,
                            "size":12,
                            "padding":2,
                            "background":"#000000"
                        }]
                    }),
                )
                .expect("export selected ranges");
            let (before, _) = post::frame(&output, 0.1).expect("frame before caption");
            let (during, _) = post::frame(&output, 0.35).expect("frame during caption");
            let index = 2 * before.width() as usize + 2;
            assert!(before.pixels()[index].red() > 150, "{extension}");
            assert!(during.pixels()[index].red() < 100, "{extension}");
        }
    }

    #[test]
    fn marked_gif_overlays_use_final_canvas_coordinates() {
        let fixtures = Fixtures::new();
        let source = fixtures.video("gif-overlay-source.mp4", None);
        let markers = [(0, "selection"), (1000, "selection")]
            .into_iter()
            .map(|(at_ms, label)| recording_marks::Marker::Label {
                at_ms,
                label: label.into(),
            })
            .collect();
        recording_marks::write(
            &source,
            &recording_marks::RecordingMarks::new(1000, 12, 64, 48, markers),
        )
        .expect("recording sidecar");

        for keep_marked in [false, true] {
            let output = fixtures.path(if keep_marked {
                "marked-overlay.gif"
            } else {
                "direct-overlay.gif"
            });
            fixtures
                .run(
                    "video.export",
                    json!({
                        "path":source,
                        "out":output,
                        "fps":10,
                        "max_width":32,
                        "keep_marked":keep_marked,
                        "dither_mode":"none",
                        "overlays":[{
                            "text":"A",
                            "x":8,
                            "y":6,
                            "from":0.0,
                            "to":0.5,
                            "size":12,
                            "padding":2,
                            "background":"#000000"
                        }]
                    }),
                )
                .expect("GIF overlay export");
            let (frame, _) = post::frame(&output, 0.1).expect("GIF frame");
            assert_eq!((frame.width(), frame.height()), (32, 24));
            assert!(
                frame.pixels()[3 * 32 + 5].red() > 150,
                "background before the caption should remain red, marked={keep_marked}"
            );
            assert!(
                frame.pixels()[6 * 32 + 8].red() < 100,
                "caption should begin at its output-canvas coordinates, marked={keep_marked}"
            );
        }
    }

    #[test]
    fn recorded_captions_scale_with_direct_and_marked_gif_canvas() {
        let fixtures = Fixtures::new();
        let source = fixtures.video("recorded-gif-source.mp4", None);
        let markers = [(0, "selection"), (1000, "selection")]
            .into_iter()
            .map(|(at_ms, label)| recording_marks::Marker::Label {
                at_ms,
                label: label.into(),
            })
            .collect();
        recording_marks::write(
            &source,
            &recording_marks::RecordingMarks::new(1000, 12, 64, 48, markers).with_captions(vec![
                recording_marks::RecordedCaption {
                    at_ms: 0,
                    duration_ms: Some(1000),
                    burned_in: false,
                    text: "A".into(),
                    x: 16,
                    y: 12,
                    font: jiffshot_doc::text::DEFAULT_FAMILY.into(),
                    size: 12.0,
                    color: jiffshot_doc::Rgba::WHITE,
                    background: jiffshot_doc::Rgba::rgb(0, 0, 0),
                    padding: 4,
                },
            ]),
        )
        .expect("recording sidecar");

        for keep_marked in [false, true] {
            for width in [32, 48] {
                let output = fixtures.path(&format!("recorded-{keep_marked}-{width}.gif"));
                fixtures
                    .run(
                        "video.export",
                        json!({
                            "path":source,
                            "out":output,
                            "fps":10,
                            "max_width":width,
                            "keep_marked":keep_marked,
                            "captions_from_recording":true,
                            "dither_mode":"none"
                        }),
                    )
                    .expect("GIF with recorded caption");
                let (frame, _) = post::frame(&output, 0.1).expect("GIF frame");
                let x = width as usize / 4;
                let y = width as usize * 3 / 16;
                assert!(
                    frame.pixels()[(y - 2) * width as usize + x - 2].red() > 150,
                    "background before caption, marked={keep_marked}, width={width}"
                );
                assert!(
                    frame.pixels()[y * width as usize + x].red() < 100,
                    "scaled caption origin, marked={keep_marked}, width={width}"
                );
            }
        }
    }

    #[test]
    fn source_captions_follow_the_original_join_segment_after_idle_edits() {
        let fixtures = Fixtures::new();
        let first = fixtures.video("idle-caption-first.mp4", None);
        let second = fixtures.video("idle-caption-second.mp4", None);
        for source in [&first, &second] {
            recording_marks::write(
                source,
                &recording_marks::RecordingMarks::new(1000, 12, 64, 48, Vec::new())
                    .with_activity_tracking_complete(true),
            )
            .expect("recording sidecar");
        }
        let output = fixtures.path("idle-caption-join.mp4");
        let result = fixtures
            .run(
                "video.join",
                json!({
                    "segments":[{"path":first}, {"path":second}],
                    "out":output,
                    "fps":12,
                    "idle_after":0.2,
                    "idle_mode":"speed",
                    "idle_speed":2,
                    "min_pause":0.25,
                    "overlays":[{
                        "text":"Second",
                        "x":2,
                        "y":2,
                        "from":0.0,
                        "to":1.0,
                        "timeline":"source",
                        "segment":1,
                        "size":12,
                        "padding":2,
                        "background":"#000000"
                    }]
                }),
            )
            .expect("join edited segments");
        let duration = result["duration"].as_f64().expect("output duration");
        let (before, _) = post::frame(&output, duration * 0.25).expect("first segment frame");
        let (during, _) = post::frame(&output, duration * 0.75).expect("second segment frame");
        let index = 2 * before.width() as usize + 2;
        assert!(before.pixels()[index].red() > 150);
        assert!(
            during.pixels()[index].red() < 100,
            "caption pixel at {} of {duration}s was {:?}",
            duration * 0.75,
            during.pixels()[index]
        );
    }

    #[test]
    fn idle_join_crossfades_only_between_original_segments() {
        let fixtures = Fixtures::new();
        let first = fixtures.video("idle-transition-first.mp4", None);
        let second = fixtures.video("idle-transition-second.mp4", None);
        for source in [&first, &second] {
            recording_marks::write(
                source,
                &recording_marks::RecordingMarks::new(1000, 12, 64, 48, Vec::new())
                    .with_activity_tracking_complete(true),
            )
            .expect("recording sidecar");
        }
        let arguments = json!({
            "segments":[{"path":first}, {"path":second}],
            "out":fixtures.path("idle-no-transition.mp4"),
            "fps":12,
            "idle_after":0.2,
            "idle_mode":"speed",
            "idle_speed":2,
            "min_pause":0.25
        });
        let baseline = fixtures
            .run("video.join", arguments.clone())
            .expect("join without transition");
        let mut crossfade = arguments.as_object().expect("join arguments").clone();
        crossfade.insert("out".into(), json!(fixtures.path("idle-transition.mp4")));
        crossfade.insert("transition".into(), json!(1.0 / 12.0));
        let faded = fixtures
            .run("video.join", Value::Object(crossfade))
            .expect("join with transition");
        assert_eq!(
            faded["frames"].as_u64().expect("crossfaded frames") + 1,
            baseline["frames"].as_u64().expect("baseline frames"),
            "only the boundary between the two original segments should overlap"
        );

        let error = fixtures
            .run(
                "video.join",
                json!({
                    "segments":[{"path":first}],
                    "out":fixtures.path("idle-single-transition.mp4"),
                    "fps":12,
                    "idle_after":0.2,
                    "idle_mode":"speed",
                    "min_pause":0.25,
                    "transition":1.0 / 12.0
                }),
            )
            .expect_err("one original segment has no join boundary");
        assert!(error.message.contains("two"));
    }

    #[test]
    fn source_captions_crossfade_with_their_own_segments() {
        let fixtures = Fixtures::new();
        let first = fixtures.video("crossfade-caption-first.mp4", None);
        let second = fixtures.video("crossfade-caption-second.mp4", None);
        let output = fixtures.path("crossfade-captions.mp4");
        fixtures
            .run(
                "video.join",
                json!({
                    "segments":[{"path":first}, {"path":second}],
                    "out":output,
                    "fps":12,
                    "transition":0.5,
                    "overlays":[
                        {
                            "text":"First",
                            "x":2,
                            "y":2,
                            "from":0.0,
                            "to":1.0,
                            "timeline":"source",
                            "segment":0,
                            "size":12,
                            "padding":4,
                            "background":"#00FF00"
                        },
                        {
                            "text":"Second",
                            "x":2,
                            "y":2,
                            "from":0.0,
                            "to":1.0,
                            "timeline":"source",
                            "segment":1,
                            "size":12,
                            "padding":4,
                            "background":"#0000FF"
                        }
                    ]
                }),
            )
            .expect("crossfaded captions");

        let (frame, _) = post::frame(&output, 0.625).expect("frame during overlap");
        let pixel = frame.pixels()[3 * 64 + 3];
        assert!(
            pixel.green() > 40 && pixel.blue() > 40,
            "both source captions should contribute to the crossfade: {pixel:?}"
        );
    }

    #[test]
    fn recording_captions_align_with_letterboxed_join_sources() {
        let fixtures = Fixtures::new();
        let first = fixtures.video("caption-canvas-first.mp4", None);
        let second = fixtures.path("caption-canvas-second.mp4");
        let mut writer =
            Writer::create(&second, VideoSpec::for_size(64, 64, 12), None).expect("encoder");
        for frame in 0..12 {
            let at = frame * HNS_PER_SECOND / 12;
            let end = (frame + 1) * HNS_PER_SECOND / 12;
            writer
                .write_frame(&[0_u8, 0, 240, 0].repeat(64 * 64), at, end - at)
                .expect("source frame");
        }
        writer.finish().expect("finalize source");
        recording_marks::write(
            &first,
            &recording_marks::RecordingMarks::new(1000, 12, 64, 48, Vec::new()),
        )
        .expect("first sidecar");
        recording_marks::write(
            &second,
            &recording_marks::RecordingMarks::new(1000, 12, 64, 64, Vec::new()).with_captions(
                vec![recording_marks::RecordedCaption {
                    at_ms: 0,
                    duration_ms: Some(1000),
                    burned_in: false,
                    text: "Caption".into(),
                    x: 2,
                    y: 2,
                    font: jiffshot_doc::text::DEFAULT_FAMILY.into(),
                    size: 12.0,
                    color: jiffshot_doc::Rgba::WHITE,
                    background: jiffshot_doc::Rgba::new(0, 0, 0, 255),
                    padding: 2,
                }],
            ),
        )
        .expect("second sidecar");
        let output = fixtures.path("caption-canvas-join.mp4");
        fixtures
            .run(
                "video.join",
                json!({
                    "segments":[{"path":first}, {"path":second}],
                    "out":output,
                    "fps":12,
                    "captions_from_recording":true
                }),
            )
            .expect("join recording captions");
        let (frame, _) = post::frame(&output, 1.25).expect("second segment frame");
        assert!(frame.pixels()[2 * 64 + 9].red() > 150);
        assert!(frame.pixels()[4 * 64 + 12].red() < 100);
    }

    #[test]
    fn idle_editing_applies_to_mp4_and_gif_exports() {
        let fixtures = Fixtures::new();
        let source = fixtures.video("still-source.mp4", None);
        let duration_ms =
            (post::inspect(&source).expect("source metadata").duration * 1000.0).round() as u64;
        crate::recording_marks::write(
            &source,
            &crate::recording_marks::RecordingMarks::new(duration_ms, 12, 64, 48, Vec::new())
                .with_activity_tracking_complete(true),
        )
        .expect("recording sidecar");
        let options = json!({
            "path": source,
            "fps": 12,
            "idle_after": 0.5,
            "idle_mode": "speed",
            "idle_speed": 4,
            "min_pause": 0.25
        });

        let mp4 = fixtures.path("idle-edited.mp4");
        let mut mp4_args = options.as_object().expect("options object").clone();
        mp4_args.insert("out".into(), json!(mp4));
        let mp4_metadata = fixtures
            .run("video.export", Value::Object(mp4_args))
            .expect("idle-edited MP4");
        let mp4_duration = mp4_metadata["duration"].as_f64().expect("MP4 duration");
        assert!(
            mp4_duration > 0.3 && mp4_duration < 0.6,
            "{mp4_duration}; idle ranges {:?}",
            post::idle_ranges(&source, 0.0, 1.0, 0.5, &[])
        );

        let gif = fixtures.path("idle-edited.gif");
        let mut gif_args = options.as_object().expect("options object").clone();
        gif_args.insert("out".into(), json!(gif));
        gif_args.insert("fps".into(), json!(10));
        let gif_metadata = fixtures
            .run("video.export", Value::Object(gif_args))
            .expect("idle-edited GIF");
        let gif_duration = gif_metadata["duration"].as_f64().expect("GIF duration");
        assert!(gif_duration > 0.3 && gif_duration < 0.7, "{gif_duration}");
    }

    #[test]
    fn idle_editing_rejects_incomplete_or_legacy_activity_history() {
        let incomplete = recording_marks::RecordingMarks::new(1000, 12, 64, 48, Vec::new());
        let legacy: recording_marks::RecordingMarks = serde_json::from_value(json!({
            "version": 1, "duration_ms": 1000, "fps": 12,
            "width": 64, "height": 48, "markers": []
        }))
        .expect("legacy sidecar");
        for marks in [&incomplete, &legacy] {
            let error = edit_idle_segments(
                Vec::new(),
                IdleOptions {
                    after: 1.0,
                    mode: IdleMode::Cut,
                    speed: 4.0,
                    min_pause: 0.5,
                },
                marks,
            )
            .expect_err("incomplete activity history");

            assert_eq!(error.code, ErrorCode::InvalidRequest);
            assert!(error.message.contains("activity history"));
        }
    }

    #[test]
    fn verify_handler_decodes_video_and_reports_frame_count() {
        let fixtures = Fixtures::new();
        let source = fixtures.video("source.mp4", None);

        let verified = fixtures
            .run("video.verify", json!({"path":source}))
            .expect("verified video");

        assert_eq!(verified["path"], json!(source));
        assert_eq!(verified["format"], "mp4");
        assert_eq!(verified["frames"], 12);
        assert_eq!(verified["pixel_format"], "BGRA");
        assert_eq!(verified["constant_frame_rate"], true);
    }

    #[test]
    fn edit_plan_exports_mp4_and_derived_gif_outputs() {
        let fixtures = Fixtures::new();
        fixtures.video("source.mp4", None);
        let mp4 = fixtures.path("edited.mp4");
        let gif = fixtures.path("preview.gif");
        let plan_path = fixtures.path("edit.json");
        fs::write(
            &plan_path,
            serde_json::to_vec(&json!({
                "fps": 12,
                "segments": [{"path": "source.mp4", "from": 0.0, "to": 1.0, "speed": 2.0}],
                "outputs": [
                    {"format": "mp4", "path": "edited.mp4"},
                    {"format": "gif", "path": "preview.gif", "fps": 6, "max_width": 32}
                ]
            }))
            .expect("encode plan"),
        )
        .expect("write plan");

        let results = fixtures
            .run("video.edit", json!({"plan":plan_path}))
            .expect("edit plan");

        assert_eq!(results.as_array().expect("output results").len(), 2);
        assert_eq!(results[0]["format"], "mp4");
        assert_eq!(results[0]["path"], json!(mp4));
        assert_eq!(results[0]["frames"], 6);
        assert_eq!(results[1]["format"], "gif");
        assert_eq!(results[1]["path"], json!(gif));
        assert!(fs::metadata(mp4).expect("MP4 output").len() > 0);
        assert!(fs::metadata(gif).expect("GIF output").len() > 0);
    }

    #[test]
    fn edit_plan_render_failure_preserves_every_existing_destination() {
        let fixtures = Fixtures::new();
        fixtures.video("source.mp4", None);
        let mp4 = fixtures.path("existing.mp4");
        let gif = fixtures.path("existing.gif");
        fs::write(&mp4, b"old mp4").expect("existing MP4");
        fs::write(&gif, b"old gif").expect("existing GIF");
        let plan_path = fixtures.path("failure-plan.json");
        fs::write(
            &plan_path,
            serde_json::to_vec(&json!({
                "segments":[{"path":"source.mp4"}],
                "outputs":[
                    {"format":"mp4","path":"existing.mp4"},
                    {"format":"gif","path":"existing.gif","max_bytes":1}
                ]
            }))
            .expect("encode plan"),
        )
        .expect("write plan");

        assert!(
            fixtures
                .run("video.edit", json!({"plan":plan_path}))
                .is_err(),
            "an impossible GIF size must fail the edit"
        );
        assert_eq!(fs::read(mp4).expect("existing MP4"), b"old mp4");
        assert_eq!(fs::read(gif).expect("existing GIF"), b"old gif");
    }

    #[test]
    fn edit_plan_rejects_source_overwrites_and_duplicate_destinations() {
        let fixtures = Fixtures::new();
        let source = fixtures.video("protected-source.mp4", None);
        let source_bytes = fs::read(&source).expect("source bytes");
        let plan_path = fixtures.path("protected-plan.json");
        fs::write(
            &plan_path,
            serde_json::to_vec(&json!({
                "segments":[{"path":"protected-source.mp4"}],
                "outputs":[{"format":"mp4","path":"protected-source.mp4"}]
            }))
            .expect("encode plan"),
        )
        .expect("write plan");
        assert!(fixtures
            .run("video.edit", json!({"plan":plan_path}))
            .is_err());
        assert!(
            fs::read(&source).expect("protected source") == source_bytes,
            "the recording must not be replaced"
        );

        let output = fixtures.path("duplicate.gif");
        fs::write(&output, b"old gif").expect("existing GIF");
        fs::write(
            &plan_path,
            serde_json::to_vec(&json!({
                "segments":[{"path":"protected-source.mp4"}],
                "outputs":[
                    {"format":"gif","path":"duplicate.gif"},
                    {"format":"gif","path":PathBuf::from(".").join("duplicate.gif")}
                ]
            }))
            .expect("encode duplicate plan"),
        )
        .expect("write duplicate plan");
        let error = fixtures
            .run("video.edit", json!({"plan":plan_path}))
            .expect_err("duplicate outputs");
        assert!(error.message.contains("distinct"));
        assert_eq!(fs::read(output).expect("unchanged GIF"), b"old gif");

        let plan_with_mp4_extension = fixtures.path("input-plan.mp4");
        let plan = serde_json::to_vec(&json!({
            "segments":[{"path":"protected-source.mp4"}],
            "outputs":[{"format":"mp4","path":"input-plan.mp4"}]
        }))
        .expect("encode plan-as-output");
        fs::write(&plan_with_mp4_extension, &plan).expect("write plan-as-output");
        assert!(fixtures
            .run("video.edit", json!({"plan":plan_with_mp4_extension}))
            .is_err());
        assert_eq!(
            fs::read(plan_with_mp4_extension).expect("protected plan"),
            plan
        );
    }

    #[test]
    fn gif_only_edit_plan_produces_its_output() {
        let fixtures = Fixtures::new();
        fixtures.video("source.mp4", None);
        let gif = fixtures.path("preview.gif");
        let plan_path = fixtures.path("edit.json");
        fs::write(
            &plan_path,
            serde_json::to_vec(&json!({
                "segments": [{"path": "source.mp4", "from": 0.0, "to": 1.0}],
                "outputs": [{"format": "gif", "path": "preview.gif", "fps": 6}]
            }))
            .expect("encode plan"),
        )
        .expect("write plan");
        let result = fixtures
            .run("video.edit", json!({"plan":plan_path}))
            .expect("GIF-only edit plan");

        assert_eq!(result[0]["format"], "gif");
        assert!(fs::metadata(gif).expect("GIF output").len() > 0);
    }

    #[test]
    fn gif_only_edit_discards_incompatible_audio_and_reports_it() {
        let fixtures = Fixtures::new();
        let audio = fixtures.video(
            "with-audio.mp4",
            Some(AudioSpec {
                sample_rate: 48_000,
                channels: 2,
            }),
        );
        let silent = fixtures.video("without-audio.mp4", None);
        let gif = fixtures.path("gif-only-audio.gif");
        let plan_path = fixtures.path("gif-only-audio.json");
        fs::write(
            &plan_path,
            serde_json::to_vec(&json!({
                "segments":[{"path":audio},{"path":silent}],
                "outputs":[{"format":"gif","path":gif,"fps":5}]
            }))
            .expect("encode plan"),
        )
        .expect("write plan");

        let result = fixtures
            .run("video.edit", json!({"plan":plan_path}))
            .expect("GIF-only edit should discard incompatible audio");

        assert_eq!(result[0]["audio_note"], "GIF cannot retain source audio");
        assert!(fs::metadata(gif).expect("GIF output").len() > 0);
    }

    #[test]
    fn gif_only_edit_still_validates_the_mute_option() {
        let fixtures = Fixtures::new();
        let source = fixtures.video("source.mp4", None);
        let plan_path = fixtures.path("invalid-mute.json");
        fs::write(
            &plan_path,
            serde_json::to_vec(&json!({
                "segments":[{"path":source}],
                "mute":[],
                "outputs":[{"format":"gif","path":"preview.gif"}]
            }))
            .expect("encode plan"),
        )
        .expect("write plan");

        let error = fixtures
            .run("video.edit", json!({"plan":plan_path}))
            .expect_err("invalid mute option");

        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains("mute"));
        assert!(!fixtures.path("preview.gif").exists());
    }

    #[test]
    fn edit_plan_reports_omitted_gif_audio_when_mp4_is_muted() {
        let fixtures = Fixtures::new();
        let source = fixtures.video(
            "with-audio.mp4",
            Some(AudioSpec {
                sample_rate: 48_000,
                channels: 2,
            }),
        );
        let plan_path = fixtures.path("muted-edit.json");
        fs::write(
            &plan_path,
            serde_json::to_vec(&json!({
                "segments":[{"path":source}],
                "mute":true,
                "outputs":[
                    {"format":"mp4","path":"muted.mp4"},
                    {"format":"gif","path":"preview.gif","fps":5}
                ]
            }))
            .expect("encode plan"),
        )
        .expect("write plan");

        let result = fixtures
            .run("video.edit", json!({"plan":plan_path}))
            .expect("muted edit plan");

        assert!(result[0]["audio"].is_null());
        assert_eq!(result[1]["audio_note"], "GIF cannot retain source audio");
    }

    #[test]
    fn gif_quality_controls_reduce_duplicate_frames_and_preserve_existing_output_on_size_failure() {
        let fixtures = Fixtures::new();
        let source = fixtures.video("source.mp4", None);
        let gif = fixtures.path("quality.gif");
        fs::write(&gif, b"preserve me").expect("existing output");

        let error = fixtures
            .run(
                "video.export",
                json!({
                    "path": source,
                    "out": gif,
                    "fps": 12,
                    "max_width": 64,
                    "palette_size": 16,
                    "dither_mode": "floyd-steinberg",
                    "optimize_frames": true,
                    "max_bytes": 1
                }),
            )
            .expect_err("unattainable size");
        assert!(error.message.contains("max_bytes"));
        assert_eq!(
            fs::read(&gif).expect("unchanged destination"),
            b"preserve me"
        );

        let metadata = fixtures
            .run(
                "video.export",
                json!({
                    "path": source,
                    "out": gif,
                    "fps": 12,
                    "max_width": 64,
                    "palette_size": 16,
                    "dither_mode": "floyd-steinberg",
                    "optimize_frames": true,
                    "max_bytes": 100_000
                }),
            )
            .expect("GIF quality controls");
        assert_eq!(metadata["format"], "gif");
        assert_eq!(metadata["frames"], 1);
        assert!(metadata["bytes"].as_u64().expect("byte count") <= 100_000);
        let inspected = fixtures
            .run("video.info", json!({"path": gif}))
            .expect("inspect optimized GIF");
        assert_eq!(metadata["fps"], inspected["fps"]);
        assert_eq!(
            metadata["constant_frame_rate"],
            inspected["constant_frame_rate"]
        );
    }

    #[test]
    fn join_handler_forwards_segment_speeds_and_exact_target_duration() {
        let fixtures = Fixtures::new();
        let source = fixtures.video("source.mp4", None);
        let output = fixtures.path("target-duration.mp4");
        let joined = fixtures
            .run(
                "video.join",
                json!({
                    "out": output,
                    "fps": 12,
                    "duration": 0.5,
                    "segments": [
                        {"path": source, "from": 0.0, "to": 0.5, "speed": 2.0},
                        {"path": source, "from": 0.5, "to": 1.0}
                    ]
                }),
            )
            .expect("speed-adjusted target-duration join");

        assert_eq!(joined["frames"], 6);
        assert!((joined["fps"].as_f64().expect("encoded fps") - 12.0).abs() < 0.01);
        assert!((joined["duration"].as_f64().expect("encoded duration") - 0.5).abs() < 0.02);
    }

    #[test]
    fn cli_requires_explicit_mute_for_audio_downgrade_and_preserves_compatible_audio() {
        let fixtures = Fixtures::new();
        let sound = fixtures.video(
            "sound.mp4",
            Some(AudioSpec {
                sample_rate: 48_000,
                channels: 2,
            }),
        );
        let silent = fixtures.video("silent.mp4", None);
        let destination = fixtures.path("join.mp4");
        let segments =
            json!([{"path":sound,"from":0.0,"to":0.5},{"path":silent,"from":0.0,"to":0.5}]);
        let error = fixtures
            .run("video.join", json!({"out":destination,"segments":segments}))
            .expect_err("audio loss must be explicit");
        assert!(
            error.message.contains("--mute") && error.message.contains("matching"),
            "{error}"
        );
        assert!(!destination.exists());
        let muted = fixtures
            .run(
                "video.join",
                json!({"out":destination,"segments":segments,"mute":true}),
            )
            .expect("explicit mute");
        assert!(muted["audio"].is_null());
        assert!(muted.get("audio_note").is_none());
        let kept = fixtures
            .run(
                "video.export",
                json!({"path":sound,"out":fixtures.path("kept.mp4"),"from":0.25,"to":0.75}),
            )
            .expect("preserved sound");
        assert_eq!(kept["audio"], json!({"sample_rate":48000,"channels":2}));
        assert!(kept.get("audio_note").is_none());
        let compatible = fixtures.run("video.join", json!({"out":fixtures.path("compatible.mp4"),"segments":[{"path":sound,"from":0.0,"to":0.5},{"path":sound,"from":0.5,"to":1.0}]})).expect("compatible sound");
        assert_eq!(
            compatible["audio"],
            json!({"sample_rate":48000,"channels":2})
        );
        let gif = fixtures
            .run(
                "video.export",
                json!({"path":sound,"out":fixtures.path("sound.gif")}),
            )
            .expect("GIF without sound");
        assert_eq!(gif["audio_note"], "GIF cannot retain source audio");
        let default = options(&Args::new(json!({})).expect("object")).expect("options");
        assert_eq!(default.audio, Audio::RequireCompatible);
    }
}
