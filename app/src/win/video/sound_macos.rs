//! PCM playback through Core Audio's output queue.

use super::{hns, seconds};
use jiffshot_capture::media::Reader;
use std::ffi::c_void;
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

#[repr(C)]
struct AudioFormat {
    sample_rate: f64,
    format_id: u32,
    flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioBuffer {
    capacity: u32,
    data: *mut c_void,
    size: u32,
    user_data: *mut c_void,
    packet_capacity: u32,
    packet_descriptions: *mut c_void,
    packet_count: u32,
}

type AudioQueue = *mut c_void;

#[link(name = "AudioToolbox", kind = "framework")]
extern "C" {
    fn AudioQueueNewOutput(
        format: *const AudioFormat,
        callback: unsafe extern "C" fn(*mut c_void, AudioQueue, *mut AudioBuffer),
        user_data: *mut c_void,
        run_loop: *const c_void,
        mode: *const c_void,
        flags: u32,
        out: *mut AudioQueue,
    ) -> i32;
    fn AudioQueueAllocateBuffer(queue: AudioQueue, size: u32, buffer: *mut *mut AudioBuffer) -> i32;
    fn AudioQueueEnqueueBuffer(queue: AudioQueue, buffer: *mut AudioBuffer, count: u32, packets: *const c_void) -> i32;
    fn AudioQueueSetParameter(queue: AudioQueue, parameter: u32, value: f32) -> i32;
    fn AudioQueueStart(queue: AudioQueue, time: *const c_void) -> i32;
    fn AudioQueueStop(queue: AudioQueue, immediate: u8) -> i32;
    fn AudioQueueGetProperty(queue: AudioQueue, property: u32, data: *mut c_void, size: *mut u32) -> i32;
    fn AudioQueueDispose(queue: AudioQueue, immediate: u8) -> i32;
}

const BUFFER_COUNT: usize = 4;
const BUFFER_BYTES: u32 = 32 * 1024;
const VOLUME: u32 = 1;

#[derive(Default)]
struct Shared {
    available: Mutex<Vec<usize>>,
    wake: Condvar,
}

unsafe extern "C" fn completed(context: *mut c_void, _queue: AudioQueue, buffer: *mut AudioBuffer) {
    let shared = &*(context as *const Shared);
    shared.available.lock().unwrap_or_else(|e| e.into_inner()).push(buffer as usize);
    shared.wake.notify_one();
}

fn check(status: i32, operation: &str) -> Result<(), String> {
    if status == 0 { Ok(()) } else { Err(format!("{operation} failed (Core Audio status {status})")) }
}

struct Output {
    queue: AudioQueue,
    shared: Box<Shared>,
    started: bool,
    level: u32,
}

impl Output {
    fn new(rate: u32, channels: u32) -> Result<Self, String> {
        if rate == 0 || channels == 0 || channels > 32 {
            return Err("unsupported audio format".into());
        }
        let frame_bytes = channels * 2;
        let format = AudioFormat {
            sample_rate: f64::from(rate),
            format_id: u32::from_be_bytes(*b"lpcm"),
            flags: (1 << 2) | (1 << 3),
            bytes_per_packet: frame_bytes,
            frames_per_packet: 1,
            bytes_per_frame: frame_bytes,
            channels,
            bits_per_channel: 16,
            reserved: 0,
        };
        let mut shared = Box::<Shared>::default();
        let mut queue = ptr::null_mut();
        // SAFETY: the format is copied; `shared` stays at this address until
        // the queue has been synchronously disposed.
        check(
            unsafe {
                AudioQueueNewOutput(
                    &format,
                    completed,
                    (&mut *shared as *mut Shared).cast(),
                    ptr::null(),
                    ptr::null(),
                    0,
                    &mut queue,
                )
            },
            "opening the audio output",
        )?;
        let output = Self { queue, shared, started: false, level: u32::MAX };
        for _ in 0..BUFFER_COUNT {
            let mut buffer = ptr::null_mut();
            // SAFETY: the queue is live and buffer is an out-parameter.
            check(unsafe { AudioQueueAllocateBuffer(output.queue, BUFFER_BYTES, &mut buffer) }, "allocating an audio buffer")?;
            if buffer.is_null() {
                return Err("Core Audio returned an empty audio buffer".into());
            }
            output.shared.available.lock().unwrap_or_else(|e| e.into_inner()).push(buffer as usize);
        }
        Ok(output)
    }

    fn update_volume(&mut self, volume: &AtomicU32) -> Result<(), String> {
        let level = volume.load(Ordering::Acquire).min(0xFFFF);
        if self.level != level {
            // SAFETY: the queue is live and the value is between 0 and 1.
            check(unsafe { AudioQueueSetParameter(self.queue, VOLUME, level as f32 / 65535.0) }, "setting playback volume")?;
            self.level = level;
        }
        Ok(())
    }

    fn take_buffer(&mut self, stop: &AtomicBool, volume: &AtomicU32) -> Result<Option<*mut AudioBuffer>, String> {
        while !stop.load(Ordering::Acquire) {
            self.update_volume(volume)?;
            let available = self.shared.available.lock().unwrap_or_else(|e| e.into_inner());
            let (mut available, _) = self
                .shared
                .wake
                .wait_timeout_while(available, Duration::from_millis(20), |available| available.is_empty())
                .unwrap_or_else(|e| e.into_inner());
            if let Some(buffer) = available.pop() {
                return Ok(Some(buffer as *mut AudioBuffer));
            }
        }
        Ok(None)
    }

    fn write(&mut self, samples: &[i16], channels: usize, stop: &AtomicBool, volume: &AtomicU32) -> Result<(), String> {
        let capacity = (BUFFER_BYTES as usize / 2 / channels) * channels;
        for chunk in samples.chunks(capacity) {
            let Some(buffer) = self.take_buffer(stop, volume)? else { break };
            // SAFETY: Core Audio owns this available buffer, which has
            // sufficient capacity and is not enqueued until after the copy.
            unsafe {
                ptr::copy_nonoverlapping(chunk.as_ptr().cast::<u8>(), (*buffer).data.cast::<u8>(), chunk.len() * 2);
                (*buffer).size = (chunk.len() * 2) as u32;
                check(AudioQueueEnqueueBuffer(self.queue, buffer, 0, ptr::null()), "queueing audio")?;
                if !self.started {
                    check(AudioQueueStart(self.queue, ptr::null()), "starting audio playback")?;
                    self.started = true;
                }
            }
        }
        Ok(())
    }

    fn drain(&mut self, stop: &AtomicBool, volume: &AtomicU32) -> Result<(), String> {
        if !self.started || stop.load(Ordering::Acquire) {
            return Ok(());
        }
        // SAFETY: the live queue finishes its queued buffers asynchronously.
        check(unsafe { AudioQueueStop(self.queue, 0) }, "draining the audio output")?;
        let started = Instant::now();
        while !stop.load(Ordering::Acquire) {
            self.update_volume(volume)?;
            let mut running = 0u32;
            let mut size = std::mem::size_of_val(&running) as u32;
            // SAFETY: `running` is an appropriately sized out-parameter.
            check(
                unsafe { AudioQueueGetProperty(self.queue, u32::from_be_bytes(*b"aqrn"), (&mut running as *mut u32).cast(), &mut size) },
                "checking audio playback",
            )?;
            if running == 0 {
                break;
            }
            if started.elapsed() > Duration::from_secs(5) {
                return Err("audio output did not finish playing".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        // SAFETY: synchronous disposal ends callbacks before `shared` drops.
        let status = unsafe { AudioQueueDispose(self.queue, 1) };
        if status != 0 {
            eprintln!("closing the audio output failed (Core Audio status {status})");
        }
    }
}

fn sample_range(at: f64, from: f64, end: f64, rate: u32, channels: usize, samples: usize) -> std::ops::Range<usize> {
    let frames = samples / channels;
    let first = (((from - at) * f64::from(rate)).ceil().max(0.0) as usize).min(frames);
    let last = (((end - at) * f64::from(rate)).ceil().max(0.0) as usize).min(frames);
    first * channels..last.max(first) * channels
}

fn play_inner(path: &Path, from: f64, end: f64, stop: &AtomicBool, volume: &AtomicU32) -> Result<(), String> {
    let mut reader = Reader::open(path)?;
    let Some(spec) = reader.audio else { return Ok(()) };
    reader.select_video(false)?;
    reader.select_audio(true)?;
    reader.seek(hns(from))?;
    let mut output = Output::new(spec.sample_rate, spec.channels)?;
    while !stop.load(Ordering::Acquire) {
        let Some((at, pcm)) = reader.next_audio()? else { break };
        let at = seconds(at);
        if at >= end {
            break;
        }
        let range = sample_range(at, from, end, spec.sample_rate, spec.channels as usize, pcm.len());
        output.write(&pcm[range], spec.channels as usize, stop, volume)?;
    }
    output.drain(stop, volume)
}

pub fn play(path: &Path, from: f64, end: f64, stop: &AtomicBool, volume: &AtomicU32) {
    if let Err(error) = play_inner(path, from, end, stop, volume) {
        eprintln!("video sound playback: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_pcm_to_the_requested_time_range() {
        assert_eq!(sample_range(1.0, 1.25, 1.75, 4, 2, 8), 2..6);
        assert_eq!(sample_range(1.0, 0.0, 1.5, 4, 2, 8), 0..4);
        assert_eq!(sample_range(1.0, 3.0, 4.0, 4, 2, 8), 8..8);
    }
}
