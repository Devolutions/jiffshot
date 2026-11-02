//! Pointer effects for screen recordings: a translucent halo that follows the pointer and a
//! ring while a mouse button is held. The pointer is only observable in native X11 sessions.

use super::x11;
use gstreamer as gst;
use gstreamer_video::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

const HALO_RADIUS: f32 = 28.0;
const POLL_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Default)]
struct Pointer {
    x: AtomicI32,
    y: AtomicI32,
    pressed: AtomicBool,
    known: AtomicBool,
}

/// Blends a halo (and a click ring when `pressed`) centered at `(x, y)` into RGBA pixels.
pub(super) fn draw_effect(
    plane: &mut [u8],
    stride: usize,
    width: usize,
    height: usize,
    (x, y): (f32, f32),
    pressed: bool,
) {
    if width == 0 || height == 0 || stride < width * 4 || plane.len() < (height - 1) * stride + width * 4
    {
        return;
    }
    let reach = HALO_RADIUS + 2.0;
    let top = (y - reach).floor().max(0.0) as usize;
    let bottom = ((y + reach).ceil().max(0.0) as usize).min(height);
    let left = (x - reach).floor().max(0.0) as usize;
    let right = ((x + reach).ceil().max(0.0) as usize).min(width);
    let (color, fill) = if pressed {
        ([255.0, 80.0, 60.0], 0.45)
    } else {
        ([255.0, 220.0, 40.0], 0.3)
    };
    for row in top..bottom {
        let dy = row as f32 + 0.5 - y;
        for column in left..right {
            let dx = column as f32 + 0.5 - x;
            let distance = (dx * dx + dy * dy).sqrt();
            let edge = (HALO_RADIUS + 0.5 - distance).clamp(0.0, 1.0);
            if edge <= 0.0 {
                continue;
            }
            let ring = pressed && distance > HALO_RADIUS - 4.0;
            let alpha = edge * if ring { 0.9 } else { fill };
            let offset = row * stride + column * 4;
            for (channel, source) in plane[offset..offset + 3].iter_mut().zip(color) {
                *channel = (f32::from(*channel) * (1.0 - alpha) + source * alpha).round() as u8;
            }
        }
    }
}

/// Starts drawing the pointer effects on frames leaving `pad`, which must carry RGBA video.
/// Returns false when the session cannot report the pointer.
pub(super) fn attach(pad: &gst::Pad) -> bool {
    if !x11::is_x11_session() || x11::pointer().is_none() {
        return false;
    }
    let state = Arc::new(Pointer::default());
    let poller = Arc::downgrade(&state);
    let spawned = std::thread::Builder::new()
        .name("jiffshot-pointer".to_owned())
        .spawn(move || {
            // The probe owns the state, so polling ends when the pipeline is dropped.
            while let Some(state) = poller.upgrade() {
                if let Some(((x, y), pressed)) = x11::pointer() {
                    state.x.store(x, Ordering::Relaxed);
                    state.y.store(y, Ordering::Relaxed);
                    state.pressed.store(pressed, Ordering::Relaxed);
                    state.known.store(true, Ordering::Release);
                }
                drop(state);
                std::thread::sleep(POLL_INTERVAL);
            }
        });
    if spawned.is_err() {
        return false;
    }
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, probe| {
        if state.known.load(Ordering::Acquire) {
            draw_probe(pad, probe, &state);
        }
        gst::PadProbeReturn::Ok
    })
    .is_some()
}

fn draw_probe(pad: &gst::Pad, probe: &mut gst::PadProbeInfo<'_>, state: &Pointer) {
    let Some(caps) = pad.current_caps() else {
        return;
    };
    let Ok(info) = gstreamer_video::VideoInfo::from_caps(&caps) else {
        return;
    };
    if info.format() != gstreamer_video::VideoFormat::Rgba {
        return;
    }
    let Some(buffer) = probe.buffer_mut() else {
        return;
    };
    let Ok(mut frame) =
        gstreamer_video::VideoFrameRef::from_buffer_ref_writable(buffer.make_mut(), &info)
    else {
        return;
    };
    let Some(stride) = frame
        .plane_stride()
        .first()
        .and_then(|stride| usize::try_from(*stride).ok())
    else {
        return;
    };
    let Ok(plane) = frame.plane_data_mut(0) else {
        return;
    };
    draw_effect(
        plane,
        stride,
        info.width() as usize,
        info.height() as usize,
        (
            state.x.load(Ordering::Relaxed) as f32,
            state.y.load(Ordering::Relaxed) as f32,
        ),
        state.pressed.load(Ordering::Relaxed),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: usize, height: usize) -> Vec<u8> {
        [0, 0, 0, 255].repeat(width * height)
    }

    #[test]
    fn halo_tints_only_pixels_near_the_pointer() {
        let (width, height) = (200, 120);
        let mut plane = frame(width, height);
        draw_effect(&mut plane, width * 4, width, height, (100.0, 60.0), false);
        let at = |x: usize, y: usize| plane[(y * width + x) * 4..(y * width + x) * 4 + 3].to_vec();
        assert_ne!(at(100, 60), [0, 0, 0]);
        assert_eq!(at(10, 10), [0, 0, 0]);
        assert_eq!(plane[(60 * width + 100) * 4 + 3], 255);
    }

    #[test]
    fn pressed_pointer_draws_a_stronger_red_ring() {
        let (width, height) = (200, 120);
        let mut idle = frame(width, height);
        let mut held = frame(width, height);
        draw_effect(&mut idle, width * 4, width, height, (100.0, 60.0), false);
        draw_effect(&mut held, width * 4, width, height, (100.0, 60.0), true);
        let ring = (60 * width + 100 + 26) * 4;
        assert!(held[ring] > idle[ring]);
        assert!(held[ring + 1] > 0 && held[ring + 1] < held[ring]);
    }

    #[test]
    fn halo_near_edges_and_invalid_buffers_are_safe() {
        let mut plane = frame(40, 30);
        draw_effect(&mut plane, 160, 40, 30, (-10.0, 500.0), true);
        draw_effect(&mut plane, 160, 40, 30, (39.0, 29.0), false);
        let before = plane.clone();
        draw_effect(&mut plane, 8, 40, 30, (20.0, 15.0), false);
        draw_effect(&mut plane[..100], 160, 40, 30, (20.0, 15.0), false);
        assert_eq!(plane, before);
    }
}
