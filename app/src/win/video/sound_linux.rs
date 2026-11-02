//! PCM playback through GStreamer's default audio sink.

use super::{hns, seconds};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use jiffshot_capture::media::Reader;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

fn play_inner(
    path: &Path,
    from: f64,
    end: f64,
    stop: &AtomicBool,
    volume: &AtomicU32,
) -> Result<(), String> {
    let mut reader = Reader::open(path)?;
    let Some(spec) = reader.audio else {
        return Ok(());
    };
    reader.select_video(false)?;
    reader.select_audio(true)?;
    reader.seek(hns(from))?;
    let pipeline = gst::parse::launch(&format!(
        "appsrc name=src format=time is-live=false block=true max-bytes=65536 \
         caps=audio/x-raw,format=S16LE,layout=interleaved,rate={},channels=2 \
         ! audioconvert ! volume name=volume ! autoaudiosink",
        spec.sample_rate
    ))
    .map_err(|e| e.to_string())?
    .downcast::<gst::Pipeline>()
    .map_err(|_| "audio playback is not a pipeline".to_owned())?;
    let src = pipeline
        .by_name("src")
        .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
        .ok_or("missing audio source")?;
    let gain = pipeline.by_name("volume").ok_or("missing volume control")?;
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| e.to_string())?;
    let mut sent = 0u64;
    let rate = u64::from(spec.sample_rate);
    while !stop.load(Ordering::Acquire) {
        gain.set_property("volume", f64::from(volume.load(Ordering::Relaxed)) / 65535.0);
        let Some((at, pcm)) = reader.next_audio()? else {
            break;
        };
        if seconds(at) >= end {
            break;
        }
        let mut buffer = gst::Buffer::from_slice(
            pcm.iter()
                .flat_map(|s| s.to_le_bytes())
                .collect::<Vec<u8>>(),
        );
        {
            let buffer = buffer.get_mut().expect("a new buffer is writable");
            buffer.set_pts(gst::ClockTime::from_nseconds(sent * 1_000_000_000 / rate));
        }
        sent += pcm.len() as u64 / 2;
        if src.push_buffer(buffer).is_err() {
            break;
        }
    }
    if !stop.load(Ordering::Acquire) {
        let _ = src.end_of_stream();
        if let Some(bus) = pipeline.bus() {
            while !stop.load(Ordering::Acquire) {
                if bus
                    .timed_pop_filtered(
                        gst::ClockTime::from_mseconds(50),
                        &[gst::MessageType::Eos, gst::MessageType::Error],
                    )
                    .is_some()
                {
                    break;
                }
            }
        }
    } else {
        std::thread::sleep(Duration::from_millis(1));
    }
    let _ = pipeline.set_state(gst::State::Null);
    Ok(())
}

pub fn play(path: &Path, from: f64, end: f64, stop: &AtomicBool, volume: &AtomicU32) {
    if let Err(error) = play_inner(path, from, end, stop, volume) {
        eprintln!("video sound playback: {error}");
    }
}
