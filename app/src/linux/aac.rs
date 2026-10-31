//! AAC encoder selection shared by recording and video editing.

use gst::prelude::*;
use gstreamer as gst;

// Keep Ubuntu's previous encoder and bitrate when it is installed.
const FACTORIES: [&str; 2] = ["voaacenc", "fdkaacenc"];
const BITRATE: i32 = 128_000;

fn select_with(mut available: impl FnMut(&str) -> bool) -> Result<&'static str, String> {
    FACTORIES
        .into_iter()
        .find(|name| available(name))
        .ok_or_else(|| {
            "no supported AAC audio encoder is available (requires voaacenc or fdkaacenc)"
                .to_owned()
        })
}

/// Finds a supported factory without replacing or registering GStreamer plugins.
///
/// # Errors
///
/// Returns an error naming both candidates when neither is installed.
#[cfg(test)]
pub(super) fn select_factory() -> Result<&'static str, String> {
    select_with(|name| gst::ElementFactory::find(name).is_some())
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
                .property("bitrate", BITRATE);
            let builder = if name == "fdkaacenc" {
                builder.property_from_str("rate-control", "cbr")
            } else {
                builder
            };
            builder.build().map_err(|error| error.to_string())
        },
    )?;
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
    bin.add_many([&encoder, &filter])
        .map_err(|error| format!("could not assemble AAC encoder: {error}"))?;
    encoder
        .link(&filter)
        .map_err(|error| format!("could not connect AAC-LC format filter: {error}"))?;
    for (name, element) in [("sink", &encoder), ("src", &filter)] {
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

    #[test]
    fn selected_encoder_negotiates_lc_stereo_and_decodes_a_resampled_tone() {
        gst::init().expect("GStreamer should initialize");
        let selected = select_factory().expect("a real AAC encoder must be installed");
        let pipeline = gst::parse::launch(
            "audiotestsrc freq=440 num-buffers=100 samplesperbuffer=441 \
             ! audio/x-raw,format=F32LE,rate=44100,channels=2,layout=interleaved \
             ! audioconvert ! audioresample \
             ! audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved \
             ! identity name=input \
             aacparse name=parser ! avdec_aac ! audioconvert \
             ! audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved \
             ! appsink name=decoded sync=false",
        )
        .expect("headless audio pipeline should parse")
        .downcast::<gst::Pipeline>()
        .expect("headless audio test should have a pipeline");
        add_to_pipeline(&pipeline, "input", "parser")
            .expect("the selected real encoder should connect");
        let encoder = pipeline
            .by_name("aac-encoder")
            .expect("shared stage should have an encoder");
        assert_eq!(
            encoder
                .factory()
                .expect("encoder should have a factory")
                .name(),
            selected
        );
        assert_eq!(encoder.property::<i32>("bitrate"), 128_000);
        let sink = pipeline
            .by_name("decoded")
            .expect("pipeline should have a decoded sink")
            .downcast::<gstreamer_app::AppSink>()
            .expect("decoded sink should be an appsink");
        pipeline
            .set_state(gst::State::Playing)
            .expect("AAC encoding and decoding should start");
        let mut audio = Vec::new();
        loop {
            let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_seconds(10)) else {
                assert!(sink.is_eos(), "AAC decoding must finish, not stall");
                break;
            };
            let caps = sample.caps().expect("decoded audio should have caps");
            let format = caps.structure(0).expect("audio format should be present");
            assert_eq!(format.get::<String>("format").expect("PCM format"), "S16LE");
            assert_eq!(format.get::<i32>("rate").expect("PCM rate"), 48_000);
            assert_eq!(format.get::<i32>("channels").expect("PCM channels"), 2);
            assert_eq!(
                format.get::<String>("layout").expect("PCM layout"),
                "interleaved"
            );
            let map = sample
                .buffer()
                .expect("decoded sample should have a buffer")
                .map_readable()
                .expect("decoded PCM should be readable");
            let (frames, remainder) = map.as_slice().as_chunks::<4>();
            assert!(remainder.is_empty(), "stereo PCM frames must be complete");
            audio.extend(
                frames
                    .iter()
                    .map(|frame| i16::from_le_bytes([frame[0], frame[1]])),
            );
        }
        let parser_caps = pipeline
            .by_name("parser")
            .expect("pipeline should have an AAC parser")
            .static_pad("src")
            .expect("parser should have an output pad")
            .current_caps()
            .expect("AAC output should negotiate");
        let encoded = parser_caps.structure(0).expect("encoded audio format");
        assert_eq!(encoded.name(), "audio/mpeg");
        assert_eq!(encoded.get::<i32>("mpegversion").expect("MPEG version"), 4);
        assert_eq!(
            encoded.get::<String>("stream-format").expect("AAC framing"),
            "raw"
        );
        assert_eq!(
            encoded.get::<String>("base-profile").expect("AAC profile"),
            "lc"
        );
        assert_eq!(encoded.get::<i32>("rate").expect("AAC rate"), 48_000);
        assert_eq!(encoded.get::<i32>("channels").expect("AAC channels"), 2);
        let input_caps = encoder
            .static_pad("sink")
            .expect("encoder should have an input pad")
            .current_caps()
            .expect("encoder input should negotiate");
        let input = input_caps.structure(0).expect("encoder input format");
        assert_eq!(
            input.get::<String>("format").expect("encoder PCM format"),
            "S16LE"
        );
        assert_eq!(
            input.get::<String>("layout").expect("encoder PCM layout"),
            "interleaved"
        );
        assert_eq!(input.get::<i32>("rate").expect("encoder PCM rate"), 48_000);
        assert_eq!(
            input.get::<i32>("channels").expect("encoder PCM channels"),
            2
        );
        pipeline
            .set_state(gst::State::Null)
            .expect("headless audio pipeline should stop");
        assert!(
            (47_000..=50_000).contains(&audio.len()),
            "one second of resampled AAC should decode to about 48000 frames, got {}",
            audio.len()
        );
        let window = &audio[4_800..28_800];
        let crossings = window
            .windows(2)
            .filter(|pair| pair[0] <= 0 && pair[1] > 0)
            .count();
        let frequency = crossings as f64 / 0.5;
        assert!(
            (430.0..=450.0).contains(&frequency),
            "decoded tone: {frequency} Hz"
        );
        assert!(
            window.iter().any(|sample| sample.unsigned_abs() > 5_000),
            "decoded audio must contain the tone, not silence"
        );
    }
}
