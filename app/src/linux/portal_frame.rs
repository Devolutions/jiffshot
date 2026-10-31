//! Validation of video buffers received from desktop capture portals.

use gst::prelude::*;
use gstreamer as gst;

pub(super) fn guard_screen_frames(source: &gst::Element) -> Result<(), String> {
    let output = source
        .static_pad("src")
        .ok_or_else(|| "screen capture source has no output pad".to_owned())?;
    let weak_source = source.downgrade();
    output
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(buffer) = info.buffer() {
                if buffer.size() == 0 || buffer.flags().contains(gst::BufferFlags::CORRUPTED) {
                    if let Some(source) = weak_source.upgrade() {
                        gst::element_error!(
                            source,
                            gst::StreamError::Format,
                            ("desktop returned an empty or corrupted screen frame")
                        );
                    }
                    return gst::PadProbeReturn::Drop;
                }
            }
            gst::PadProbeReturn::Ok
        })
        .ok_or_else(|| "could not validate screen capture frames".to_owned())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_source(buffer: gst::Buffer) -> gst::Message {
        gst::init().expect("GStreamer should initialize");
        let source = gstreamer_app::AppSrc::builder()
            .format(gst::Format::Time)
            .build();
        let sink = gst::ElementFactory::make("fakesink")
            .build()
            .expect("test sink should be available");
        let pipeline = gst::Pipeline::new();
        pipeline
            .add_many([source.upcast_ref(), &sink])
            .expect("test pipeline elements should be added");
        source
            .link(&sink)
            .expect("test source should link to the sink");
        guard_screen_frames(source.upcast_ref()).expect("source guard should be installed");
        pipeline
            .set_state(gst::State::Playing)
            .expect("test pipeline should start");
        source
            .push_buffer(buffer)
            .expect("test buffer should be queued");
        source.end_of_stream().expect("test source should finish");
        let message = pipeline
            .bus()
            .expect("test pipeline should have a bus")
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(5),
                &[gst::MessageType::Error, gst::MessageType::Eos],
            )
            .expect("test pipeline should report an error or finish");
        pipeline
            .set_state(gst::State::Null)
            .expect("test pipeline should stop");
        message
    }

    #[test]
    fn empty_and_corrupted_screen_frames_report_errors() {
        gst::init().expect("GStreamer should initialize");
        let mut corrupted = gst::Buffer::from_mut_slice(vec![0; 64]);
        corrupted
            .get_mut()
            .expect("test buffer should be uniquely owned")
            .set_flags(gst::BufferFlags::CORRUPTED);
        for buffer in [gst::Buffer::new(), corrupted] {
            let message = run_source(buffer);
            let gst::MessageView::Error(error) = message.view() else {
                panic!("an invalid screen frame must produce an error");
            };
            assert!(
                error.error().to_string().contains("empty or corrupted"),
                "error must identify the invalid screen frame: {error:?}"
            );
        }
    }

    #[test]
    fn nonempty_black_screen_frames_remain_valid() {
        gst::init().expect("GStreamer should initialize");
        let message = run_source(gst::Buffer::from_mut_slice(vec![0; 64]));
        assert!(
            matches!(message.view(), gst::MessageView::Eos(_)),
            "a legitimate black frame must not be discarded: {message:?}"
        );
    }
}
