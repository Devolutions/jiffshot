//! AAC encoder selection shared by recording and video editing.

use gst::prelude::*;
use gstreamer as gst;

mod mp4;

// Keep Ubuntu's previous encoder and bitrate when it is installed.
const FACTORIES: [&str; 2] = ["voaacenc", "fdkaacenc"];
const BITRATE: i32 = 128_000;
// AACENC_InfoStruct.nDelay for FDK's AAC-LC mode is two 1024-sample frames.
// gstfdkaacenc (through 1.26.11) does not signal this delay to downstream muxers.
const FDK_LC_DELAY: u64 = 2_048;
// VisualOn's published encoder delay (also used by FFmpeg's libvo-aacenc wrapper).
const VO_LC_DELAY: u64 = 1_600;
const AAC_FRAME_SAMPLES: u64 = 1_024;
const AUDIO_RATE: u32 = 48_000;

/// Source sample count needed to distinguish codec padding from audible media.
pub(super) struct Timing {
    frames: u64,
    delay: u64,
}

/// Snapshots negotiated source audio before stopping the pipeline clears its caps.
pub(super) fn mp4_timing(pipeline: &gst::Pipeline) -> Result<Option<Timing>, String> {
    let Some(encoder) = pipeline.by_name("aac-encoder") else {
        return Ok(None);
    };
    let factory = encoder.factory().map(|factory| factory.name().to_string());
    let delay = match factory.as_deref() {
        Some("fdkaacenc") => FDK_LC_DELAY,
        Some("voaacenc") => VO_LC_DELAY,
        _ => return Err("AAC stage has an unsupported encoder".to_owned()),
    };
    let counter = pipeline
        .by_name("aac-input-counter")
        .ok_or_else(|| "AAC stage has no source sample counter".to_owned())?;
    let bytes = counter
        .property::<gst::Structure>("stats")
        .get::<u64>("num-bytes")
        .map_err(|error| format!("could not read AAC source sample count: {error}"))?;
    Ok(Some(Timing {
        frames: source_frames(bytes)?,
        delay,
    }))
}

fn source_frames(bytes: u64) -> Result<u64, String> {
    if bytes == 0 || !bytes.is_multiple_of(4) {
        return Err("AAC source must contain complete stereo S16LE frames".to_owned());
    }
    Ok(bytes / 4)
}

/// Finalizes AAC priming/padding as an MP4 presentation edit, retaining codec packets.
pub(super) fn finalize_mp4(path: &std::path::Path, timing: Option<Timing>) -> Result<(), String> {
    if let Some(timing) = timing {
        mp4::finalize(path, timing.frames, timing.delay, AUDIO_RATE)?;
    }
    Ok(())
}

fn select_with(mut available: impl FnMut(&str) -> bool) -> Result<&'static str, String> {
    FACTORIES
        .into_iter()
        .find(|name| available(name))
        .ok_or_else(|| {
            "no supported AAC audio encoder is available (requires voaacenc or fdkaacenc)"
                .to_owned()
        })
}

fn create_with<T>(
    available: impl FnMut(&str) -> bool,
    create: impl FnOnce(&str) -> Result<T, String>,
) -> Result<T, String> {
    let factory = select_with(available)?;
    // A present but broken preferred encoder is an error, not a reason to switch codecs.
    create(factory)
        .map_err(|error| format!("could not create AAC audio encoder {factory}: {error}"))
}

/// Creates a 128 kbit/s AAC-LC stage with raw output suitable for aacparse and MP4.
///
/// Both supported encoders negotiate interleaved S16LE input; callers provide
/// audioconvert/audioresample upstream. Missing factories and creation/link errors
/// are returned rather than silently choosing another encoder.
///
/// # Errors
///
/// Returns the selected factory's creation error, or an assembly/negotiation error.
pub(super) fn encoder() -> Result<gst::Element, String> {
    let encoder = create_with(
        |name| gst::ElementFactory::find(name).is_some(),
        |name| {
            let builder = gst::ElementFactory::make(name)
                .name("aac-encoder")
                .property("bitrate", BITRATE)
                // Pause boundaries can overlap by a fraction of a mixer buffer.
                // Keep codec timestamps sample-based instead of shortening MP4 media.
                .property("perfect-timestamp", true);
            let builder = if name == "fdkaacenc" {
                builder.property_from_str("rate-control", "cbr")
            } else {
                builder
            };
            builder.build().map_err(|error| error.to_string())
        },
    )?;
    let input_format = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("format", "S16LE")
                .field("layout", "interleaved")
                .field("rate", AUDIO_RATE as i32)
                .field("channels", 2i32)
                .build(),
        )
        .build()
        .map_err(|error| format!("could not create AAC input format: {error}"))?;
    let counter = gst::ElementFactory::make("identity")
        .name("aac-input-counter")
        // concat's per-input segments must not reset/drain the codec mid-stream:
        // FDK otherwise loses delayed tail samples and emits zero-duration packets.
        .property("single-segment", true)
        .build()
        .map_err(|error| format!("could not count AAC input samples: {error}"))?;
    if encoder
        .factory()
        .is_some_and(|factory| factory.name() == "voaacenc")
    {
        finish_visualon_tail(&counter)?;
    }
    // FDK also supports HE-AAC/LD. Pin the original AAC-LC contract, not those modes.
    let filter = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("audio/mpeg")
                .field("mpegversion", 4i32)
                .field("stream-format", "raw")
                .field("profile", "lc")
                .field("base-profile", "lc")
                .build(),
        )
        .build()
        .map_err(|error| format!("could not create AAC-LC format filter: {error}"))?;
    let bin = gst::Bin::new();
    bin.add_many([&input_format, &counter, &encoder, &filter])
        .map_err(|error| format!("could not assemble AAC encoder: {error}"))?;
    gst::Element::link_many([&input_format, &counter, &encoder, &filter])
        .map_err(|error| format!("could not connect AAC-LC format filter: {error}"))?;
    for (name, element) in [("sink", &input_format), ("src", &filter)] {
        let target = element
            .static_pad(name)
            .ok_or_else(|| format!("AAC encoder has no {name} pad"))?;
        let pad = gst::GhostPad::builder_with_target(&target)
            .map_err(|error| format!("could not expose AAC {name} pad: {error}"))?
            .name(name)
            .build();
        bin.add_pad(&pad)
            .map_err(|error| format!("could not add AAC {name} pad: {error}"))?;
    }
    Ok(bin.upcast())
}

fn visualon_padding_frames(frames: u64) -> Result<u64, String> {
    frames
        .checked_add(VO_LC_DELAY)
        .and_then(|count| count.checked_next_multiple_of(AAC_FRAME_SAMPLES))
        .map(|total| total - frames)
        .ok_or_else(|| "AAC source sample count overflow".to_owned())
}

fn finish_visualon_tail(counter: &gst::Element) -> Result<(), String> {
    // voaacenc discards partial input frames and does not drain its 1600-sample
    // lookahead. Feed complete zero-padded codec frames at EOS, then exclude
    // that padding in the presentation edit. Original source counting stays
    // upstream of these pushes, so silence never becomes part of the timeline.
    let pad = counter
        .static_pad("src")
        .ok_or("AAC counter has no output pad")?;
    let weak = counter.downgrade();
    let last_end = std::sync::Arc::new(std::sync::Mutex::new(None::<gst::ClockTime>));
    let observed_end = last_end.clone();
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(buffer) = info.buffer() {
            *observed_end.lock().expect("AAC timestamp lock") = buffer
                .pts()
                .zip(buffer.duration())
                .map(|(pts, duration)| pts + duration);
        }
        gst::PadProbeReturn::Ok
    })
    .ok_or("could not observe AAC source timestamps")?;
    // Intercept before identity marks its source pad EOS. Pushing from an
    // EOS source-pad probe itself would fail with FlowError::Eos.
    counter
        .static_pad("sink")
        .ok_or("AAC counter has no input pad")?
        .add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
            if info
                .event()
                .is_some_and(|event| event.type_() == gst::EventType::Eos)
            {
                let Some(counter) = weak.upgrade() else {
                    return gst::PadProbeReturn::Drop;
                };
                let result = (|| -> Result<(), String> {
                    let bytes = counter
                        .property::<gst::Structure>("stats")
                        .get::<u64>("num-bytes")
                        .map_err(|error| error.to_string())?;
                    let frames = visualon_padding_frames(source_frames(bytes)?)?;
                    let pts = last_end
                        .lock()
                        .expect("AAC timestamp lock")
                        .ok_or("AAC source ended without a timestamped buffer")?;
                    let mut buffer = gst::Buffer::with_size(frames as usize * 4)
                        .map_err(|error| error.to_string())?;
                    let buffer_mut = buffer
                        .get_mut()
                        .ok_or("AAC padding buffer is not writable")?;
                    buffer_mut
                        .map_writable()
                        .map_err(|error| error.to_string())?
                        .as_mut_slice()
                        .fill(0);
                    buffer_mut.set_pts(pts);
                    buffer_mut.set_duration(gst::ClockTime::from_nseconds(
                        frames * 1_000_000_000 / u64::from(AUDIO_RATE),
                    ));
                    counter
                        .static_pad("src")
                        .ok_or("AAC counter has no output pad")?
                        .push(buffer)
                        .map_err(|error| format!("could not drain VisualOn AAC tail: {error}"))?;
                    Ok(())
                })();
                if let Err(error) = result {
                    gst::element_error!(counter, gst::StreamError::Encode, ["{error}"]);
                    return gst::PadProbeReturn::Drop;
                }
            }
            gst::PadProbeReturn::Ok
        })
        .ok_or("could not install AAC tail finalization")?;
    Ok(())
}
/// Connects the shared encoder between named endpoints of a parsed pipeline.
///
/// # Errors
///
/// Returns an error for missing endpoints, unavailable encoders or failed links.
pub(super) fn add_to_pipeline(
    pipeline: &gst::Pipeline,
    input: &str,
    output: &str,
) -> Result<(), String> {
    let input = pipeline
        .by_name(input)
        .ok_or_else(|| format!("AAC pipeline has no input {input}"))?;
    let output = pipeline
        .by_name(output)
        .ok_or_else(|| format!("AAC pipeline has no output {output}"))?;
    let encoder = encoder()?;
    pipeline
        .add(&encoder)
        .map_err(|error| format!("could not add AAC encoder: {error}"))?;
    gst::Element::link_many([&input, &encoder, &output])
        .map_err(|error| format!("could not connect AAC encoder: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_accounting_rejects_empty_and_incomplete_stereo_frames() {
        for bytes in [0, 1, 2, 3, 5, u64::MAX] {
            assert_eq!(
                source_frames(bytes).expect_err("invalid stereo byte count"),
                "AAC source must contain complete stereo S16LE frames"
            );
        }
        for (bytes, frames) in [
            (4, 1),
            (8, 2),
            (191_996, 47_999),
            (u64::MAX - 3, u64::MAX / 4),
        ] {
            assert_eq!(
                source_frames(bytes).expect("complete stereo frames"),
                frames
            );
        }
    }

    #[test]
    fn visualon_tail_padding_completes_partial_frames_and_codec_lookahead_without_overflow() {
        for (source, padding) in [
            (1, 2047),
            (1023, 2049),
            (1024, 2048),
            (47999, 2177),
            (48000, 2176),
        ] {
            assert_eq!(
                visualon_padding_frames(source).expect("tail padding"),
                padding
            );
            assert!((source + padding).is_multiple_of(1024));
            assert!(source + padding >= source + VO_LC_DELAY);
        }
        assert!(visualon_padding_frames(u64::MAX)
            .expect_err("overflow must fail")
            .contains("overflow"));
    }

    #[test]
    fn selects_ubuntu_encoder_when_only_voaacenc_is_available() {
        assert_eq!(select_with(|name| name == "voaacenc"), Ok("voaacenc"));
    }

    #[test]
    fn selects_fedora_encoder_when_only_fdkaacenc_is_available() {
        assert_eq!(select_with(|name| name == "fdkaacenc"), Ok("fdkaacenc"));
    }

    #[test]
    fn preserves_ubuntu_priority_when_both_encoders_are_available() {
        let mut queried = Vec::new();
        assert_eq!(
            select_with(|name| {
                queried.push(name.to_owned());
                true
            }),
            Ok("voaacenc")
        );
        assert_eq!(queried, ["voaacenc"], "do not switch to Fedora's encoder");
    }

    #[test]
    fn reports_neither_encoder_available_explicitly() {
        let mut queried = Vec::new();
        assert_eq!(
            select_with(|name| {
                queried.push(name.to_owned());
                false
            }),
            Err(
                "no supported AAC audio encoder is available (requires voaacenc or fdkaacenc)"
                    .to_owned()
            )
        );
        assert_eq!(queried, ["voaacenc", "fdkaacenc"]);
    }

    #[test]
    fn creation_errors_name_the_selected_encoder_without_fallback() {
        for factory in FACTORIES {
            let mut created = Vec::new();
            let result = create_with::<()>(
                |name| factory == "voaacenc" || name == "fdkaacenc",
                |name| {
                    created.push(name.to_owned());
                    Err("plugin could not load".to_owned())
                },
            );
            assert_eq!(
                result,
                Err(format!(
                    "could not create AAC audio encoder {factory}: plugin could not load"
                ))
            );
            assert_eq!(
                created,
                [factory],
                "creation failure must not try a fallback"
            );
        }
    }

    #[test]
    fn missing_encoders_do_not_attempt_creation() {
        let mut created = Vec::new();
        let result = create_with(
            |_| false,
            |name| {
                created.push(name.to_owned());
                Ok(())
            },
        );
        assert_eq!(
            result,
            Err(
                "no supported AAC audio encoder is available (requires voaacenc or fdkaacenc)"
                    .to_owned()
            )
        );
        assert!(
            created.is_empty(),
            "an unavailable encoder must never be created"
        );
    }

}
