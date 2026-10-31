//! Offline video inspection, stills and transactional post-production.

use super::{internal, invalid, not_found, Args, Reply};
use jiffshot_capture::media::post::{self, Audio, GifOptions, Mp4Options, Overlay, Segment};
use serde_json::{json, Value};
use std::path::PathBuf;

pub(super) fn dispatch(command: &str, a: &Args) -> Reply {
    match command {
        "video.info" => {
            a.only(&["path"])?;
            let path = source(a)?;
            serde_json::to_value(post::inspect(&path).map_err(internal)?).map_err(internal)
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
        "video.export" => export(a),
        "video.join" => join(a),
        _ => Err(invalid(format!("unknown video command {command:?}"))),
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
    Ok((path, from, to))
}

fn options(a: &Args) -> Result<Mp4Options, jiffshot_control::Error> {
    Ok(Mp4Options {
        fps: a.int("fps", 1, 120)?.map(|n| n as u32),
        width: a.int("width", 2, 8192)?.map(|n| n as u32),
        height: a.int("height", 2, 8192)?.map(|n| n as u32),
        audio: if a.bool("mute")? {
            Audio::Mute
        } else {
            Audio::RequireCompatible
        },
        overlays: overlays(a)?,
    })
}

fn export(a: &Args) -> Reply {
    a.only(&[
        "path",
        "out",
        "from",
        "to",
        "fps",
        "width",
        "height",
        "mute",
        "max_width",
        "loop",
        "fade",
        "dither",
        "overlays",
    ])?;
    let segment = segment(a)?;
    let out = a
        .out("out", &["mp4", "gif"])?
        .ok_or_else(|| invalid("out is required"))?;
    let result = if out
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("gif"))
    {
        if a.has("height") || a.has("width") {
            return Err(invalid("GIF uses max_width, not width or height"));
        }
        if a.has("mute") {
            a.bool("mute")?;
        }
        let options = GifOptions {
            fps: finite(a, "fps", 10.0)?,
            max_width: a.int("max_width", 1, 8192)?.unwrap_or(960) as u32,
            looped: a.flag("loop")?.unwrap_or(true),
            fade: a.bool("fade")?,
            dither: a.flag("dither")?.unwrap_or(true),
            overlays: overlays(a)?,
        };
        if !(1.0..=50.0).contains(&options.fps) {
            return Err(invalid("GIF fps must be 1..50"));
        }
        post::export_gif(&segment.0, &out, (segment.1, segment.2), options, |_| {})
    } else {
        for key in ["max_width", "loop", "fade", "dither"] {
            if a.has(key) {
                return Err(invalid(format!("{key} is only supported for GIF exports")));
            }
        }
        post::export_mp4(&[segment], &out, options(a)?, |_| {})
    };
    serde_json::to_value(result.map_err(internal)?).map_err(internal)
}

fn join(a: &Args) -> Reply {
    a.only(&[
        "segments", "out", "fps", "width", "height", "mute", "overlays",
    ])?;
    let value = match a.0.get("segments") {
        Some(Value::String(s)) => {
            serde_json::from_str(s).map_err(|e| invalid(format!("invalid segments JSON: {e}")))?
        }
        Some(v) => v.clone(),
        None => return Err(invalid("segments is required")),
    };
    let entries = value
        .as_array()
        .ok_or_else(|| invalid("segments must be a JSON array of {path, from?, to?} objects"))?;
    if entries.is_empty() || entries.len() > 256 {
        return Err(invalid("provide 1..256 segments"));
    }
    let segments = entries
        .iter()
        .enumerate()
        .map(|(i, value)| {
            let args = Args::new(value.clone())?;
            args.only(&["path", "from", "to"])?;
            segment(&args).map_err(|e| invalid(format!("segment {i}: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let out = a
        .out("out", &["mp4"])?
        .ok_or_else(|| invalid("out is required"))?;
    serde_json::to_value(post::export_mp4(&segments, &out, options(a)?, |_| {}).map_err(internal)?)
        .map_err(internal)
}

fn overlays(a: &Args) -> Result<Vec<Overlay>, jiffshot_control::Error> {
    let value = match a.0.get("overlays") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::String(s)) => {
            serde_json::from_str(s).map_err(|e| invalid(format!("invalid overlays JSON: {e}")))?
        }
        Some(v) => v.clone(),
    };
    let entries = value
        .as_array()
        .ok_or_else(|| invalid("overlays must be an array of {path, x?, y?, from, to} objects"))?;
    if entries.len() > 256 {
        return Err(invalid("provide at most 256 overlays"));
    }
    entries
        .iter()
        .map(|v| {
            let a = Args::new(v.clone())?;
            a.only(&["path", "x", "y", "from", "to"])?;
            let path = source(&a)?;
            if !a.has("from") || !a.has("to") {
                return Err(invalid(
                    "overlay from and to are required on the output timeline",
                ));
            }
            let (from, to) = (finite(&a, "from", 0.0)?, finite(&a, "to", 0.0)?);
            if from < 0.0 || to <= from {
                return Err(invalid("overlay range must satisfy 0 <= from < to"));
            }
            Ok(Overlay {
                path,
                x: a.int("x", -8192, 8192)?.unwrap_or(0) as i32,
                y: a.int("y", -8192, 8192)?.unwrap_or(0) as i32,
                from,
                to,
            })
        })
        .collect()
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
            dispatch(command, &Args::new(value).expect("object args"))
        }
    }

    impl Drop for Fixtures {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn rejects_unexpected_video_arguments_and_empty_join() {
        let args = Args::new(json!({"paths":[]})).expect("object");
        assert!(dispatch("video.info", &args)
            .expect_err("typo")
            .message
            .contains("unexpected argument"));
        let args = Args::new(json!({"segments":[]})).expect("object");
        assert!(dispatch("video.join", &args)
            .expect_err("empty")
            .message
            .contains("1..256"));
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
        assert_eq!(animation["duration"], 0.5);
        let gif_info = fixtures
            .run("video.info", json!({"path":gif}))
            .expect("GIF info");
        assert_video_metadata(&gif_info, &gif, "gif", Some(10));
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
