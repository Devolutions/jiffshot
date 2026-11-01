//! macOS media backend: AVFoundation writes MP4 (H.264 video, AAC audio),
//! decodes MP4 to top-down BGRA frames and 16-bit PCM, and reads the webcam
//! and the microphone; ScreenCaptureKit captures system audio.
//!
//! Times are in 100-nanosecond units ("hns"), as on Windows.

use super::{AudioBlock, AudioSpec, Result, VideoFrame, VideoSpec, HNS_PER_SECOND};
use crate::macos::ffi::rect;
use crate::macos::sck::{self, SendBox};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_av_foundation::{
    AVAssetReader, AVAssetReaderStatus, AVAssetReaderTrackOutput, AVAssetTrack, AVAssetWriter, AVAssetWriterInput,
    AVAssetWriterInputPixelBufferAdaptor, AVAssetWriterStatus, AVAuthorizationStatus, AVCaptureAudioDataOutput,
    AVCaptureAudioDataOutputSampleBufferDelegate, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceInput,
    AVCaptureDeviceWasDisconnectedNotification, AVCaptureOutput, AVCaptureSession, AVCaptureSessionDidStopRunningNotification,
    AVCaptureSessionErrorKey, AVCaptureSessionRuntimeErrorNotification, AVCaptureSessionWasInterruptedNotification,
    AVCaptureVideoDataOutput, AVCaptureVideoDataOutputSampleBufferDelegate, AVFileTypeMPEG4, AVMediaType,
    AVMediaTypeAudio, AVMediaTypeVideo, AVURLAsset, AVVideoAllowFrameReorderingKey, AVVideoAverageBitRateKey,
    AVVideoCodecKey, AVVideoCodecTypeH264, AVVideoCompressionPropertiesKey, AVVideoExpectedSourceFrameRateKey,
    AVVideoColorPrimariesKey, AVVideoColorPrimaries_ITU_R_709_2, AVVideoColorPropertiesKey, AVVideoHeightKey,
    AVVideoMaxKeyFrameIntervalKey, AVVideoTransferFunctionKey, AVVideoTransferFunction_ITU_R_709_2, AVVideoWidthKey,
    AVVideoYCbCrMatrixKey, AVVideoYCbCrMatrix_ITU_R_709_2,
};
use objc2_core_foundation::{CFRetained, CFString};
use objc2_core_media::{
    kCMBlockBufferAssureMemoryNowFlag, kCMTimePositiveInfinity, CMAudioSampleBufferCreateReadyWithPacketDescriptions,
    CMBlockBuffer, CMClock, CMFormatDescription, CMSampleBuffer, CMSyncConvertTime, CMTime, CMTimeFlags, CMTimeRange,
};
use objc2_core_video::{
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA, CVPixelBuffer, CVPixelBufferCreate,
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferPool, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{
    ns_string, NSDictionary, NSError, NSNotification, NSNotificationCenter, NSNumber, NSObject, NSObjectProtocol, NSString,
};
use objc2_screen_capture_kit::SCStreamOutputType;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::path::Path;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The rate both audio sources deliver, so they can share a soundtrack.
const CAPTURE_RATE: u32 = 48_000;

/// `kAudioFormatLinearPCM` and `kAudioFormatMPEG4AAC`.
const FORMAT_LPCM: u32 = u32::from_be_bytes(*b"lpcm");
const FORMAT_AAC: u32 = u32::from_be_bytes(*b"aac ");
/// `kAudioFormatFlag*` bits used to interpret PCM.
const FLAG_FLOAT: u32 = 1 << 0;
const FLAG_BIG_ENDIAN: u32 = 1 << 1;
const FLAG_SIGNED: u32 = 1 << 2;
const FLAG_PACKED: u32 = 1 << 3;
const FLAG_NON_INTERLEAVED: u32 = 1 << 5;

/// CoreAudio's `AudioStreamBasicDescription`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct Asbd {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

/// CoreAudio's `AudioBuffer`; an `AudioBufferList` is a `u32` count
/// followed by these (8-byte aligned).
#[repr(C)]
struct AudioBuffer {
    channels: u32,
    size: u32,
    data: *mut c_void,
}

// Declared here with local types: the CoreAudio types crate is not a
// dependency of this crate.
#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CMAudioFormatDescriptionCreate(
        allocator: *const c_void,
        asbd: *const Asbd,
        layout_size: usize,
        layout: *const c_void,
        cookie_size: usize,
        cookie: *const c_void,
        extensions: *const c_void,
        out: *mut *const CMFormatDescription,
    ) -> i32;
    fn CMAudioFormatDescriptionGetStreamBasicDescription(desc: &CMFormatDescription) -> *const Asbd;
    fn CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
        sample: &CMSampleBuffer,
        size_needed: *mut usize,
        list: *mut c_void,
        list_size: usize,
        structure_allocator: *const c_void,
        block_allocator: *const c_void,
        flags: u32,
        block_out: *mut *mut CMBlockBuffer,
    ) -> i32;
}

fn ns_err(e: &NSError) -> String {
    e.localizedDescription().to_string()
}

/// A framework string constant, or its documented value when the symbol is
/// missing.
fn key(constant: Option<&'static NSString>, value: &'static NSString) -> &'static NSString {
    constant.unwrap_or(value)
}

fn video_type() -> &'static AVMediaType {
    // SAFETY: reading a framework constant.
    key(unsafe { AVMediaTypeVideo }, ns_string!("vide"))
}

fn audio_type() -> &'static AVMediaType {
    // SAFETY: reading a framework constant.
    key(unsafe { AVMediaTypeAudio }, ns_string!("soun"))
}

fn cf_key(k: &CFString) -> &NSString {
    // SAFETY: CFString and NSString are toll-free bridged.
    unsafe { &*(k as *const CFString as *const NSString) }
}

fn dict(pairs: &[(&NSString, &AnyObject)]) -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys: Vec<&NSString> = pairs.iter().map(|p| p.0).collect();
    let values: Vec<&AnyObject> = pairs.iter().map(|p| p.1).collect();
    NSDictionary::from_slices(&keys, &values)
}

fn num(n: i64) -> Retained<NSNumber> {
    NSNumber::new_i64(n)
}

fn cm_time(hns: i64) -> CMTime {
    CMTime { value: hns, timescale: HNS_PER_SECOND as i32, flags: CMTimeFlags::Valid, epoch: 0 }
}

fn hns(t: CMTime) -> Option<i64> {
    if !t.flags.contains(CMTimeFlags::Valid) || t.flags.intersects(CMTimeFlags::ImpliedValueFlagsMask) || t.timescale <= 0 {
        return None;
    }
    Some((t.value as i128 * HNS_PER_SECOND as i128 / t.timescale as i128) as i64)
}

fn audio_frame_position(time: i64, rate: u32) -> Result<i64> {
    if time < 0 || rate == 0 || rate > i32::MAX as u32 {
        return Err("invalid audio presentation time or sample rate".into());
    }
    let frames = (i128::from(time) * i128::from(rate) + i128::from(HNS_PER_SECOND) / 2) / i128::from(HNS_PER_SECOND);
    i64::try_from(frames).map_err(|_| "audio presentation time is out of range".into())
}

fn stream_description(desc: &CMFormatDescription) -> Option<Asbd> {
    // SAFETY: returns null for non-audio descriptions, else a pointer valid
    // while `desc` lives; copied out at once.
    unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(desc).as_ref().copied() }
}

pub fn startup() -> Result<()> {
    Ok(())
}

/// Waits until `input` takes more data (AVAssetWriter interleaves tracks,
/// so one input may wait for the other to catch up).
fn writer_error(writer: &AVAssetWriter) -> String {
    // SAFETY: plain property read.
    match unsafe { writer.error() } {
        Some(e) => ns_err(&e),
        None => "writing the video failed".into(),
    }
}

struct AudioInput {
    input: Retained<AVAssetWriterInput>,
    format: CFRetained<CMFormatDescription>,
    spec: AudioSpec,
}

/// Writes an MP4 file. Frames are top-down BGRA (the fourth byte unused).
pub struct Writer {
    writer: Retained<AVAssetWriter>,
    video: Retained<AVAssetWriterInput>,
    adaptor: Retained<AVAssetWriterInputPixelBufferAdaptor>,
    audio: Option<AudioInput>,
    spec: VideoSpec,
    /// Samples the inputs weren't ready for yet. AVAssetWriter interleaves
    /// the tracks, so one input can stall until the other gets more data
    /// (AAC's encoder delay alone causes that); queueing instead of
    /// blocking lets callers write the streams in any order.
    pending_video: VecDeque<(CFRetained<CVPixelBuffer>, i64)>,
    pending_audio: VecDeque<CFRetained<CMSampleBuffer>>,
    /// The end of the latest sample, where the file ends.
    end: i64,
    finished: bool,
}

/// How much may be queued before writes block: generous, since an input
/// stalls by up to the writer's interleaving period (about a second).
const MAX_PENDING_VIDEO: usize = 120;
const MAX_PENDING_AUDIO: usize = 400;

// SAFETY: AVAssetWriter and its inputs may be used from any thread, one at
// a time, which `&mut self` guarantees.
unsafe impl Send for Writer {}

impl Writer {
    pub fn create(path: &Path, spec: VideoSpec, audio: Option<AudioSpec>) -> Result<Writer> {
        let url = crate::macos::picker::file_url(path, false).map_err(|error| error.to_string())?;
        // AVAssetWriter refuses to replace a file; Media Foundation does.
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("could not replace {}: {error}", path.display())),
        }
        // SAFETY: AVFoundation calls with valid arguments; the dictionaries
        // hold the documented key and value types.
        unsafe {
            let file_type = key(AVFileTypeMPEG4, ns_string!("public.mpeg-4"));
            let writer = AVAssetWriter::assetWriterWithURL_fileType_error(&url, file_type).map_err(|e| ns_err(&e))?;

            let fps = spec.fps.max(1) as i64;
            let compression = dict(&[
                (key(AVVideoAverageBitRateKey, ns_string!("AverageBitRate")), &num(spec.bitrate as i64)),
                // A keyframe every two seconds, so the editor can seek quickly.
                (key(AVVideoMaxKeyFrameIntervalKey, ns_string!("MaxKeyFrameInterval")), &num(fps * 2)),
                (key(AVVideoExpectedSourceFrameRateKey, ns_string!("ExpectedFrameRate")), &num(fps)),
                // Presentation order equals decode order: simpler seeking.
                (key(AVVideoAllowFrameReorderingKey, ns_string!("AllowFrameReordering")), &NSNumber::new_bool(false)),
            ]);
            // Explicit metadata prevents SD frames from being decoded with a different YCbCr matrix.
            let colors = dict(&[
                (key(AVVideoColorPrimariesKey, ns_string!("AVVideoColorPrimariesKey")), key(AVVideoColorPrimaries_ITU_R_709_2, ns_string!("ITU_R_709_2"))),
                (key(AVVideoTransferFunctionKey, ns_string!("AVVideoTransferFunctionKey")), key(AVVideoTransferFunction_ITU_R_709_2, ns_string!("ITU_R_709_2"))),
                (key(AVVideoYCbCrMatrixKey, ns_string!("AVVideoYCbCrMatrixKey")), key(AVVideoYCbCrMatrix_ITU_R_709_2, ns_string!("ITU_R_709_2"))),
            ]);
            let settings = dict(&[
                (key(AVVideoCodecKey, ns_string!("AVVideoCodecKey")), key(AVVideoCodecTypeH264, ns_string!("avc1"))),
                (key(AVVideoWidthKey, ns_string!("AVVideoWidthKey")), &num(spec.width as i64)),
                (key(AVVideoHeightKey, ns_string!("AVVideoHeightKey")), &num(spec.height as i64)),
                (key(AVVideoCompressionPropertiesKey, ns_string!("AVVideoCompressionPropertiesKey")), &compression),
                (key(AVVideoColorPropertiesKey, ns_string!("AVVideoColorPropertiesKey")), &colors),
            ]);
            let video = AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(video_type(), Some(&settings));
            video.setExpectsMediaDataInRealTime(false);
            let attributes = dict(&[
                (cf_key(kCVPixelBufferPixelFormatTypeKey), &num(kCVPixelFormatType_32BGRA as i64)),
                (cf_key(objc2_core_video::kCVPixelBufferWidthKey), &num(spec.width as i64)),
                (cf_key(objc2_core_video::kCVPixelBufferHeightKey), &num(spec.height as i64)),
            ]);
            let adaptor = AVAssetWriterInputPixelBufferAdaptor::assetWriterInputPixelBufferAdaptorWithAssetWriterInput_sourcePixelBufferAttributes(
                &video,
                Some(&attributes),
            );
            if !writer.canAddInput(&video) {
                return Err("cannot add an H.264 track".into());
            }
            writer.addInput(&video);

            let audio = match audio {
                Some(a) => {
                    let settings = dict(&[
                        (ns_string!("AVFormatIDKey"), &num(FORMAT_AAC as i64)),
                        (ns_string!("AVSampleRateKey"), &num(a.sample_rate as i64)),
                        (ns_string!("AVNumberOfChannelsKey"), &num(a.channels as i64)),
                        (ns_string!("AVEncoderBitRateKey"), &num(96_000 * a.channels.min(2) as i64)),
                    ]);
                    let input = AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(audio_type(), Some(&settings));
                    input.setExpectsMediaDataInRealTime(false);
                    if !writer.canAddInput(&input) {
                        return Err("cannot add an AAC track".into());
                    }
                    writer.addInput(&input);
                    let asbd = Asbd {
                        sample_rate: a.sample_rate as f64,
                        format_id: FORMAT_LPCM,
                        format_flags: FLAG_SIGNED | FLAG_PACKED,
                        bytes_per_packet: a.channels * 2,
                        frames_per_packet: 1,
                        bytes_per_frame: a.channels * 2,
                        channels_per_frame: a.channels,
                        bits_per_channel: 16,
                        reserved: 0,
                    };
                    let mut format = ptr::null();
                    let status = CMAudioFormatDescriptionCreate(
                        ptr::null(),
                        &asbd,
                        0,
                        ptr::null(),
                        0,
                        ptr::null(),
                        ptr::null(),
                        &mut format,
                    );
                    let format = NonNull::new(format as *mut CMFormatDescription)
                        .filter(|_| status == 0)
                        .ok_or_else(|| format!("unsupported audio format (error {status})"))?;
                    Some(AudioInput { input, format: CFRetained::from_raw(format), spec: a })
                }
                None => None,
            };
            if !writer.startWriting() {
                return Err(writer_error(&writer));
            }
            let () = msg_send![&*writer, startSessionAtSourceTime: cm_time(0)];
            Ok(Writer {
                writer,
                video,
                adaptor,
                audio,
                spec,
                pending_video: VecDeque::new(),
                pending_audio: VecDeque::new(),
                end: 0,
                finished: false,
            })
        }
    }

    pub fn spec(&self) -> VideoSpec {
        self.spec
    }

    fn pixel_buffer(&self) -> Result<CFRetained<CVPixelBuffer>> {
        let (w, h) = (self.spec.width as usize, self.spec.height as usize);
        let mut out: *mut CVPixelBuffer = ptr::null_mut();
        // SAFETY: `out` receives a +1 pixel buffer on success.
        let status = unsafe {
            match self.adaptor.pixelBufferPool() {
                Some(pool) => CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut out)),
                None => CVPixelBufferCreate(None, w, h, kCVPixelFormatType_32BGRA, None, NonNull::from(&mut out)),
            }
        };
        match NonNull::new(out) {
            // SAFETY: a created (+1) buffer.
            Some(p) if status == 0 => Ok(unsafe { CFRetained::from_raw(p) }),
            _ => Err(format!("could not create a pixel buffer (error {status})")),
        }
    }

    /// Writes a frame of exactly `width * height * 4` bytes.
    pub fn write_frame(&mut self, bgra: &[u8], time: i64, duration: i64) -> Result<()> {
        let (w, h) = (self.spec.width as usize, self.spec.height as usize);
        if bgra.len() < w * h * 4 {
            return Err("frame too small".into());
        }
        let buffer = self.pixel_buffer()?;
        // SAFETY: the buffer is locked while its rows are written, each row
        // within its bytes-per-row (which may include padding) and size.
        unsafe {
            if CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags::empty()) != 0 {
                return Err("could not lock a pixel buffer".into());
            }
            let base = CVPixelBufferGetBaseAddress(&buffer) as *mut u8;
            let stride = CVPixelBufferGetBytesPerRow(&buffer);
            let rows = CVPixelBufferGetHeight(&buffer).min(h);
            let row = (CVPixelBufferGetWidth(&buffer).min(w) * 4).min(stride);
            if !base.is_null() {
                for y in 0..rows {
                    ptr::copy_nonoverlapping(bgra.as_ptr().add(y * w * 4), base.add(y * stride), row);
                }
            }
            CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags::empty());
        }
        self.pending_video.push_back((buffer, time));
        self.end = self.end.max(time + duration);
        self.drain(false)
    }

    /// Writes interleaved 16-bit samples.
    pub fn write_audio(&mut self, pcm: &[i16], time: i64, duration: i64) -> Result<()> {
        let Some(audio) = &self.audio else { return Ok(()) };
        let channels = audio.spec.channels.max(1) as usize;
        let frames = pcm.len() / channels;
        if frames == 0 {
            return Ok(());
        }
        let bytes = frames * channels * 2;
        let rate = audio.spec.sample_rate as i64;
        // SAFETY: the block buffer owns `bytes` bytes, filled from `pcm`
        // before the sample buffer is created over it.
        let sample = unsafe {
            let mut block: *mut CMBlockBuffer = ptr::null_mut();
            let status = CMBlockBuffer::create_with_memory_block(
                None,
                ptr::null_mut(),
                bytes,
                None,
                ptr::null(),
                0,
                bytes,
                kCMBlockBufferAssureMemoryNowFlag,
                NonNull::from(&mut block),
            );
            let block = match NonNull::new(block) {
                Some(b) if status == 0 => CFRetained::from_raw(b),
                _ => return Err(format!("could not allocate audio (error {status})")),
            };
            let status = CMBlockBuffer::replace_data_bytes(NonNull::new_unchecked(pcm.as_ptr() as *mut c_void), &block, 0, bytes);
            if status != 0 {
                return Err(format!("could not copy audio (error {status})"));
            }
            let mut sample: *mut CMSampleBuffer = ptr::null_mut();
            let pts = CMTime {
                value: audio_frame_position(time, audio.spec.sample_rate)?,
                timescale: rate as i32,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            };
            let status = CMAudioSampleBufferCreateReadyWithPacketDescriptions(
                None,
                &block,
                &audio.format,
                frames as isize,
                pts,
                ptr::null(),
                NonNull::from(&mut sample),
            );
            match NonNull::new(sample) {
                Some(s) if status == 0 => CFRetained::from_raw(s),
                _ => return Err(format!("could not create an audio sample (error {status})")),
            }
        };
        self.pending_audio.push_back(sample);
        self.end = self.end.max(time + duration);
        self.drain(false)
    }

    /// Appends queued samples while their inputs take them. Waits until the
    /// queues are short again, or, if `all` (finishing), until they are
    /// empty, marking each input finished once its queue is, since the
    /// writer may hold one track back until the other one ends.
    fn drain(&mut self, all: bool) -> Result<()> {
        let mut progress = Instant::now();
        let (mut video_done, mut audio_done) = (false, self.audio.is_none());
        loop {
            let mut appended = false;
            // SAFETY: appending filled buffers of each input's format while
            // it is ready for them.
            unsafe {
                while let Some((buffer, time)) = self.pending_video.front() {
                    if !self.video.isReadyForMoreMediaData() {
                        break;
                    }
                    let ok: bool = msg_send![&*self.adaptor, appendPixelBuffer: &**buffer, withPresentationTime: cm_time(*time)];
                    if !ok {
                        return Err(writer_error(&self.writer));
                    }
                    self.pending_video.pop_front();
                    appended = true;
                }
                if let Some(audio) = &self.audio {
                    while let Some(sample) = self.pending_audio.front() {
                        if !audio.input.isReadyForMoreMediaData() {
                            break;
                        }
                        let ok: bool = msg_send![&*audio.input, appendSampleBuffer: &**sample];
                        if !ok {
                            return Err(writer_error(&self.writer));
                        }
                        self.pending_audio.pop_front();
                        appended = true;
                    }
                }
            }
            let done = if all {
                // SAFETY: no more samples follow for an emptied queue.
                unsafe {
                    if !video_done && self.pending_video.is_empty() {
                        self.video.markAsFinished();
                        (video_done, appended) = (true, true);
                    }
                    if let Some(audio) = self.audio.as_ref().filter(|_| !audio_done && self.pending_audio.is_empty()) {
                        audio.input.markAsFinished();
                        (audio_done, appended) = (true, true);
                    }
                }
                video_done && audio_done
            } else {
                self.pending_video.len() <= MAX_PENDING_VIDEO && self.pending_audio.len() <= MAX_PENDING_AUDIO
            };
            if done {
                return Ok(());
            }
            // SAFETY: plain property read.
            if unsafe { self.writer.status() } != AVAssetWriterStatus::Writing {
                return Err(writer_error(&self.writer));
            }
            if appended {
                progress = Instant::now();
            } else if progress.elapsed() > Duration::from_secs(10) {
                return Err("the video encoder stopped taking data".into());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    pub fn finish(mut self) -> Result<()> {
        self.drain(true)?;
        self.finished = true;
        let (tx, rx) = mpsc::channel();
        let done = RcBlock::new(move || {
            let _ = tx.send(());
        });
        // SAFETY: finishing a writing writer; the completion block is copied
        // by AVFoundation.
        unsafe {
            if self.end > 0 {
                let () = msg_send![&*self.writer, endSessionAtSourceTime: cm_time(self.end)];
            }
            self.writer.finishWritingWithCompletionHandler(&done);
        }
        rx.recv_timeout(Duration::from_secs(120)).map_err(|_| "finishing the video timed out".to_owned())?;
        // SAFETY: plain property read.
        match unsafe { self.writer.status() } {
            AVAssetWriterStatus::Completed => Ok(()),
            _ => Err(writer_error(&self.writer)),
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if !self.finished {
            // SAFETY: abandons an unfinished file.
            unsafe { self.writer.cancelWriting() };
        }
    }
}

/// One track being decoded from some position on.
struct TrackReader {
    reader: Retained<AVAssetReader>,
    output: Retained<AVAssetReaderTrackOutput>,
}

impl TrackReader {
    fn new(asset: &AVURLAsset, track: &AVAssetTrack, settings: &NSDictionary<NSString, AnyObject>, start: i64) -> Result<TrackReader> {
        // SAFETY: AVFoundation calls with valid arguments; the settings hold
        // the documented keys.
        unsafe {
            let reader = AVAssetReader::assetReaderWithAsset_error(asset).map_err(|e| ns_err(&e))?;
            let output = AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(track, Some(settings));
            output.setAlwaysCopiesSampleData(false);
            if !reader.canAddOutput(&output) {
                return Err("cannot decode this track".into());
            }
            reader.addOutput(&output);
            if start > 0 {
                let range = CMTimeRange { start: cm_time(start), duration: kCMTimePositiveInfinity };
                let () = msg_send![&*reader, setTimeRange: range];
            }
            if !reader.startReading() {
                return Err(reader_error(&reader));
            }
            Ok(TrackReader { reader, output })
        }
    }

    /// The next sample buffer, or `None` at the end.
    fn next(&self) -> Result<Option<CFRetained<CMSampleBuffer>>> {
        // SAFETY: reading a started reader; the copied buffer is +1.
        unsafe {
            let sample: *mut CMSampleBuffer = msg_send![&*self.output, copyNextSampleBuffer];
            match NonNull::new(sample) {
                Some(s) => Ok(Some(CFRetained::from_raw(s))),
                None if self.reader.status() == AVAssetReaderStatus::Failed => Err(reader_error(&self.reader)),
                None => Ok(None),
            }
        }
    }
}

fn reader_error(reader: &AVAssetReader) -> String {
    // SAFETY: plain property read.
    match unsafe { reader.error() } {
        Some(e) => ns_err(&e),
        None => "reading the video failed".into(),
    }
}

/// A decoded file.
struct File {
    asset: Retained<AVURLAsset>,
    video_track: Retained<AVAssetTrack>,
    audio_track: Option<Retained<AVAssetTrack>>,
    /// The track's display transform, a rotation or flip by quarter turns.
    turn: [i32; 4],
    /// The decoded picture size, before `turn`.
    coded: (usize, usize),
    video: Option<TrackReader>,
    audio: Option<TrackReader>,
    want_video: bool,
    want_audio: bool,
    /// Where readers created next start: the last seek or sample.
    position: i64,
}

/// The webcam: frames arrive from the capture session's queue.
struct Camera {
    session: Session,
    frames: Receiver<VideoFrame>,
    first: Option<VideoFrame>,
    cancel: Option<Arc<AtomicBool>>,
}

enum Source {
    File(Box<File>),
    Camera(Camera),
}

/// Reads an MP4 (or anything AVFoundation can open), or the webcam.
pub struct Reader {
    source: Source,
    pub width: u32,
    pub height: u32,
    pub duration: i64,
    pub fps: f64,
    pub audio: Option<AudioSpec>,
    audio_error: Option<String>,
}

// SAFETY: AVFoundation assets, readers and capture sessions may be used from
// any thread, one at a time, which `&mut self` guarantees.
unsafe impl Send for Reader {}

/// A transform's linear part when it maps pixels onto pixels (quarter
/// turns and flips), else identity.
fn quarter_turn(a: f64, b: f64, c: f64, d: f64) -> [i32; 4] {
    let m = [a, b, c, d].map(|v| v.round() as i32);
    let unit = [a, b, c, d].iter().zip(m).all(|(v, r)| (v - r as f64).abs() < 1e-3 && r.abs() <= 1);
    let [a, b, c, d] = m;
    if unit && ((b == 0 && c == 0 && a != 0 && d != 0) || (a == 0 && d == 0 && b != 0 && c != 0)) {
        m
    } else {
        [1, 0, 0, 1]
    }
}

/// Applies `turn` to a top-down BGRA picture of `w` x `h`.
fn apply_turn(src: Vec<u8>, w: usize, h: usize, turn: [i32; 4]) -> (Vec<u8>, usize, usize) {
    if turn == [1, 0, 0, 1] {
        return (src, w, h);
    }
    let [a, b, c, d] = turn.map(|v| v as i64);
    let (ow, oh) = if a == 0 { (h, w) } else { (w, h) };
    let (wm, hm) = (w as i64 - 1, h as i64 - 1);
    let ox = -((a * wm).min(0) + (c * hm).min(0));
    let oy = -((b * wm).min(0) + (d * hm).min(0));
    let mut out = vec![0u8; ow * oh * 4];
    for y in 0..h as i64 {
        for x in 0..w as i64 {
            let (dx, dy) = ((a * x + c * y + ox) as usize, (b * x + d * y + oy) as usize);
            let s = (y as usize * w + x as usize) * 4;
            let o = (dy * ow + dx) * 4;
            out[o..o + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    (out, ow, oh)
}

fn video_settings() -> Retained<NSDictionary<NSString, AnyObject>> {
    // SAFETY: reading a framework constant.
    let format_key = cf_key(unsafe { kCVPixelBufferPixelFormatTypeKey });
    dict(&[(format_key, &num(kCVPixelFormatType_32BGRA as i64))])
}

fn pcm_settings() -> Retained<NSDictionary<NSString, AnyObject>> {
    dict(&[
        (ns_string!("AVFormatIDKey"), &num(FORMAT_LPCM as i64)),
        (ns_string!("AVLinearPCMBitDepthKey"), &num(16)),
        (ns_string!("AVLinearPCMIsFloatKey"), &NSNumber::new_bool(false)),
        (ns_string!("AVLinearPCMIsBigEndianKey"), &NSNumber::new_bool(false)),
        (ns_string!("AVLinearPCMIsNonInterleaved"), &NSNumber::new_bool(false)),
    ])
}

impl Reader {
    pub fn open(path: &Path) -> Result<Reader> {
        let url = crate::macos::picker::file_url(path, false).map_err(|error| error.to_string())?;
        // SAFETY: AVFoundation property reads on a valid asset; the format
        // description is a CMFormatDescription (as documented) kept alive by
        // the track while it is read.
        unsafe {
            let asset = AVURLAsset::URLAssetWithURL_options(&url, None);
            let tracks = asset.tracks();
            let of_type = |kind: &AVMediaType| tracks.iter().find(|t| &*t.mediaType() == kind);
            let video_track = of_type(video_type()).ok_or_else(|| format!("{} has no video", path.display()))?;
            let audio_track = of_type(audio_type());
            let size = video_track.naturalSize();
            let coded = (size.width.round().max(0.0) as usize, size.height.round().max(0.0) as usize);
            let t = video_track.preferredTransform();
            let turn = quarter_turn(t.a, t.b, t.c, t.d);
            let (width, height) = if turn[0] == 0 { (coded.1, coded.0) } else { coded };
            let fps = match video_track.nominalFrameRate() as f64 {
                f if f > 0.0 => f,
                _ => 30.0,
            };
            let duration: CMTime = msg_send![&*asset, duration];
            let duration = hns(duration).unwrap_or(0).max(0);
            let audio = audio_track.as_ref().and_then(|track| {
                let first = track.formatDescriptions().firstObject()?;
                let desc = &*(Retained::as_ptr(&first) as *const CMFormatDescription);
                let asbd = stream_description(desc)?;
                if !asbd.sample_rate.is_finite() || !(1.0..=u32::MAX as f64).contains(&asbd.sample_rate) || asbd.channels_per_frame == 0 {
                    return None;
                }
                Some(AudioSpec { sample_rate: asbd.sample_rate.round() as u32, channels: asbd.channels_per_frame })
            });
            let audio_error = (audio_track.is_some() && audio.is_none()).then(|| "the audio track has no usable format description".to_owned());
            let file = File {
                asset,
                video_track,
                audio_track,
                turn,
                coded,
                video: None,
                audio: None,
                want_video: true,
                want_audio: false,
                position: 0,
            };
            Ok(Reader { source: Source::File(Box::new(file)), width: width as u32, height: height as u32, duration, fps, audio, audio_error })
        }
    }

    /// The default webcam, decoded to BGRA frames as they arrive (`duration`
    /// is 0; read with `next_video`).
    pub fn webcam() -> Result<Reader> {
        Self::open_webcam(None)
    }

    /// Starts the webcam unless `cancel` is set, including while permission
    /// or the first frame is pending. Subsequent reads also observe it.
    pub fn webcam_cancellable(cancel: Arc<AtomicBool>) -> Result<Reader> {
        Self::open_webcam(Some(cancel))
    }

    fn open_webcam(cancel: Option<Arc<AtomicBool>>) -> Result<Reader> {
        let (tx, rx) = mpsc::sync_channel::<VideoFrame>(2);
        let origin = Mutex::new(None::<i64>);
        let delegate = CaptureDelegate::new(Box::new(move |sample| {
            let mut bgra = Vec::new();
            let Some((w, h)) = sck::sample_bgra(sample, &mut bgra) else { return };
            // SAFETY: a plain property read.
            let pts = hns(unsafe { sample.presentation_time_stamp() }).unwrap_or(0);
            let origin = *origin.lock().unwrap().get_or_insert(pts);
            let frame = VideoFrame { time: pts - origin, width: w as u32, height: h as u32, bgra };
            // Late frames are dropped rather than queued.
            let _ = tx.try_send(frame);
        }));
        // SAFETY: a fresh output configured with documented settings.
        let output = unsafe {
            let output = AVCaptureVideoDataOutput::new();
            output.setVideoSettings(Some(&video_settings()));
            output.setAlwaysDiscardsLateVideoFrames(true);
            output
        };
        let session = Session::start(video_type(), "Camera", &output, |_| delegate, cancel.as_deref(), |d, q| {
            // SAFETY: a serial queue, as required; the delegate outlives the session.
            unsafe { output.setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(d)), Some(q)) }
        })?;
        let first = webcam_frame(&rx, &session, cancel.as_deref(), "the webcam sent no picture")?;
        Ok(Reader {
            width: first.width,
            height: first.height,
            source: Source::Camera(Camera { session, frames: rx, first: Some(first), cancel }),
            duration: 0,
            fps: 30.0,
            audio: None,
            audio_error: None,
        })
    }

    /// Why an existing audio track cannot be inspected; absent for silent sources.
    pub fn audio_error(&self) -> Option<&str> {
        self.audio_error.as_deref()
    }

    /// Whether audio samples are read too (off by default, for frame
    /// decoding).
    pub fn select_audio(&mut self, on: bool) -> Result<()> {
        if let Source::File(f) = &mut self.source {
            if f.want_audio != on {
                f.want_audio = on;
                f.audio = None;
            }
        }
        Ok(())
    }

    /// Whether video frames are decoded (turn off to read only audio).
    pub fn select_video(&mut self, on: bool) -> Result<()> {
        if let Source::File(f) = &mut self.source {
            if f.want_video != on {
                f.want_video = on;
                f.video = None;
            }
        }
        Ok(())
    }

    pub fn seek(&mut self, time: i64) -> Result<()> {
        match &mut self.source {
            Source::File(f) => {
                f.position = time.max(0);
                f.video = None;
                f.audio = None;
                Ok(())
            }
            Source::Camera(_) => Err("a webcam cannot seek".into()),
        }
    }

    /// The next frame.
    pub fn next_video(&mut self) -> Result<Option<VideoFrame>> {
        let f = match &mut self.source {
            Source::Camera(c) => {
                check_cancel(c.cancel.as_deref())?;
                if let Some(error) = c.session.failure() {
                    return Err(error);
                }
                if let Some(first) = c.first.take() {
                    return Ok(Some(first));
                }
                return webcam_frame(&c.frames, &c.session, c.cancel.as_deref(), "the webcam stopped sending pictures").map(Some);
            }
            Source::File(f) => f,
        };
        if !f.want_video {
            return Err("video is not selected".into());
        }
        if f.video.is_none() {
            f.video = Some(TrackReader::new(&f.asset, &f.video_track, &video_settings(), f.position)?);
        }
        let reader = f.video.as_ref().unwrap();
        loop {
            let Some(sample) = reader.next()? else { return Ok(None) };
            let mut raw = Vec::new();
            let Some((bw, bh)) = sck::sample_bgra(&sample, &mut raw) else { continue };
            // SAFETY: a plain property read.
            let time = hns(unsafe { sample.presentation_time_stamp() }).unwrap_or(f.position);
            f.position = time;
            // The decoder may pad the picture; it stays at the top left.
            let (w, h) = f.coded;
            let mut bgra = vec![0u8; w * h * 4];
            let row = w.min(bw) * 4;
            for y in 0..h.min(bh) {
                bgra[y * w * 4..y * w * 4 + row].copy_from_slice(&raw[y * bw * 4..y * bw * 4 + row]);
            }
            let (bgra, w, h) = apply_turn(bgra, w, h, f.turn);
            return Ok(Some(VideoFrame { time, width: w as u32, height: h as u32, bgra }));
        }
    }

    /// The next block of interleaved 16-bit audio.
    pub fn next_audio(&mut self) -> Result<Option<(i64, Vec<i16>)>> {
        let Source::File(f) = &mut self.source else { return Ok(None) };
        let Some(track) = &f.audio_track else { return Ok(None) };
        if !f.want_audio {
            return Err("audio is not selected".into());
        }
        if f.audio.is_none() {
            f.audio = Some(TrackReader::new(&f.asset, track, &pcm_settings(), f.position)?);
        }
        let reader = f.audio.as_ref().unwrap();
        loop {
            let Some(sample) = reader.next()? else { return Ok(None) };
            // SAFETY: copies the sample's bytes into a buffer of their length.
            let pcm = unsafe {
                let Some(block) = sample.data_buffer() else { continue };
                let len = block.data_length();
                let mut pcm = vec![0i16; len / 2];
                if len < 2 || block.copy_data_bytes(0, pcm.len() * 2, NonNull::new_unchecked(pcm.as_mut_ptr() as *mut c_void)) != 0 {
                    continue;
                }
                pcm
            };
            // SAFETY: a plain property read.
            let time = hns(unsafe { sample.presentation_time_stamp() }).unwrap_or(f.position);
            f.position = time;
            return Ok(Some((time, pcm)));
        }
    }

    /// The frame showing at `time`.
    pub fn frame_at(&mut self, time: i64) -> Result<Option<VideoFrame>> {
        self.seek(time)?;
        let half = (HNS_PER_SECOND as f64 / self.fps.max(1.0) / 2.0) as i64;
        let mut last = None;
        while let Some(frame) = self.next_video()? {
            let done = frame.time + half >= time;
            last = Some(frame);
            if done {
                break;
            }
        }
        Ok(last)
    }
}

type SampleHandler = Box<dyn Fn(&CMSampleBuffer) + Send + Sync>;

pub struct DelegateIvars {
    handler: SampleHandler,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[name = "JiffshotCaptureDelegate"]
    #[ivars = DelegateIvars]
    struct CaptureDelegate;

    unsafe impl NSObjectProtocol for CaptureDelegate {}

    // The audio protocol has the same callback selector.
    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for CaptureDelegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(&self, _output: &AVCaptureOutput, sample: &CMSampleBuffer, _connection: &AVCaptureConnection) {
            (self.ivars().handler)(sample);
        }
    }

    unsafe impl AVCaptureAudioDataOutputSampleBufferDelegate for CaptureDelegate {}
);

impl CaptureDelegate {
    fn new(handler: SampleHandler) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars { handler });
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum WaitError {
    Cancelled,
    TimedOut,
    Disconnected,
    Failed(String),
}

fn check_cancel(cancel: Option<&AtomicBool>) -> Result<()> {
    if cancel.is_some_and(|cancel| cancel.load(Ordering::Acquire)) {
        Err("capture was cancelled".into())
    } else {
        Ok(())
    }
}

fn wait_for<T>(
    receiver: &Receiver<T>,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
    failure: impl Fn() -> Option<String>,
) -> std::result::Result<T, WaitError> {
    let deadline = Instant::now() + timeout;
    loop {
        if check_cancel(cancel).is_err() {
            return Err(WaitError::Cancelled);
        }
        if let Some(error) = failure() {
            return Err(WaitError::Failed(error));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(WaitError::TimedOut);
        }
        match receiver.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(value) => {
                if check_cancel(cancel).is_err() {
                    return Err(WaitError::Cancelled);
                }
                if let Some(error) = failure() {
                    return Err(WaitError::Failed(error));
                }
                return Ok(value);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if check_cancel(cancel).is_err() {
                    return Err(WaitError::Cancelled);
                }
                return Err(failure().map_or(WaitError::Disconnected, WaitError::Failed));
            }
        }
    }
}

fn webcam_frame(
    frames: &Receiver<VideoFrame>,
    session: &Session,
    cancel: Option<&AtomicBool>,
    timeout_message: &str,
) -> Result<VideoFrame> {
    wait_for(frames, Duration::from_secs(5), cancel, || session.failure()).map_err(|error| match error {
        WaitError::Cancelled => "webcam capture was cancelled".into(),
        WaitError::TimedOut => timeout_message.into(),
        WaitError::Disconnected => "the webcam frame stream closed".into(),
        WaitError::Failed(error) => error,
    })
}

/// Asks for camera or microphone access (`what` names it in System
/// Settings), waiting for the user's answer the first time.
fn authorize(media_type: &AVMediaType, what: &str, cancel: Option<&AtomicBool>) -> Result<()> {
    check_cancel(cancel)?;
    let denied = || format!("Jiffshot needs {what} permission (System Settings > Privacy & Security > {what})");
    // SAFETY: plain class method calls; the block is copied.
    unsafe {
        match AVCaptureDevice::authorizationStatusForMediaType(media_type) {
            AVAuthorizationStatus::Authorized => Ok(()),
            AVAuthorizationStatus::NotDetermined => {
                let (tx, rx) = mpsc::channel();
                let handler = RcBlock::new(move |granted: Bool| {
                    let _ = tx.send(granted.as_bool());
                });
                AVCaptureDevice::requestAccessForMediaType_completionHandler(media_type, &handler);
                match wait_for(&rx, Duration::from_secs(120), cancel, || None) {
                    Ok(true) => Ok(()),
                    Ok(false) => Err(denied()),
                    Err(WaitError::Cancelled) => Err("capture was cancelled".into()),
                    Err(WaitError::TimedOut) => Err(format!("the {what} permission request timed out")),
                    Err(WaitError::Disconnected) => Err(format!("the {what} permission request ended unexpectedly")),
                    Err(WaitError::Failed(error)) => Err(error),
                }
            }
            _ => Err(denied()),
        }
    }
}

fn report_failure(failure: &Mutex<Option<String>>, error: String) {
    let mut failure = failure.lock().unwrap_or_else(|e| e.into_inner());
    if failure.is_none() {
        *failure = Some(error);
    }
}

#[derive(Clone, Copy)]
enum SessionEvent {
    RuntimeError,
    Interrupted,
    Stopped,
    DeviceDisconnected,
}

struct SessionEvents {
    failure: Arc<Mutex<Option<String>>>,
    observers: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl SessionEvents {
    fn observe(session: &AVCaptureSession, device: &AnyObject) -> SessionEvents {
        let failure = Arc::new(Mutex::new(None));
        let center = NSNotificationCenter::defaultCenter();
        // SAFETY: these are framework constants available on supported macOS versions.
        let notifications = unsafe {
            [
                (AVCaptureSessionRuntimeErrorNotification, session as &AnyObject, SessionEvent::RuntimeError),
                (AVCaptureSessionWasInterruptedNotification, session as &AnyObject, SessionEvent::Interrupted),
                (AVCaptureSessionDidStopRunningNotification, session as &AnyObject, SessionEvent::Stopped),
                (AVCaptureDeviceWasDisconnectedNotification, device, SessionEvent::DeviceDisconnected),
            ]
        };
        let mut observers = Vec::with_capacity(notifications.len());
        for (name, object, event) in notifications {
            let failure = failure.clone();
            let callback = RcBlock::new(move |notification: NonNull<NSNotification>| {
                let message = match event {
                    SessionEvent::RuntimeError => {
                        // SAFETY: the notification is live while the copied block runs.
                        let info = unsafe { notification.as_ref() }.userInfo();
                        // SAFETY: reading the documented error dictionary key.
                        let detail = info
                            .and_then(|info| info.objectForKey(unsafe { AVCaptureSessionErrorKey }))
                            .and_then(|error| error.downcast_ref::<NSError>().map(ns_err));
                        detail.map_or_else(|| "capture session failed".into(), |detail| format!("capture session failed: {detail}"))
                    }
                    SessionEvent::Interrupted => "capture session was interrupted".into(),
                    SessionEvent::Stopped => "capture session stopped unexpectedly".into(),
                    SessionEvent::DeviceDisconnected => "capture device disconnected".into(),
                };
                report_failure(&failure, message);
            });
            // SAFETY: the block captures only thread-safe state; the center
            // retains the observer and delivers matching notifications.
            let observer = unsafe {
                center.addObserverForName_object_queue_usingBlock(Some(name), Some(object), None, &callback)
            };
            observers.push(observer);
        }
        SessionEvents { failure, observers }
    }

    fn failure(&self) -> Option<String> {
        self.failure.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn remove(&mut self) {
        let center = NSNotificationCenter::defaultCenter();
        for observer in self.observers.drain(..) {
            // SAFETY: each token came from this notification center.
            unsafe { center.removeObserver((*observer).as_ref()) };
        }
    }
}

impl Drop for SessionEvents {
    fn drop(&mut self) {
        self.remove();
    }
}

/// A running capture session reading the default device of a media type;
/// stops when dropped.
struct Session {
    session: Retained<AVCaptureSession>,
    queue: DispatchRetained<DispatchQueue>,
    _delegate: Retained<CaptureDelegate>,
    events: SessionEvents,
}

// SAFETY: AVCaptureSession may be started and stopped from any thread; the
// delegate is only called on the session's queue.
unsafe impl Send for Session {}

struct WeakCaptureSession(Weak<AVCaptureSession>);

// SAFETY: the Objective-C weak reference can be loaded on the sample queue,
// where only the session's read-only synchronization clock is queried.
unsafe impl Send for WeakCaptureSession {}
unsafe impl Sync for WeakCaptureSession {}

impl WeakCaptureSession {
    fn clock(&self) -> Option<Retained<CMClock>> {
        let session = self.0.load()?;
        // SAFETY: the retained session is alive for this property read.
        unsafe { session.synchronizationClock() }
    }
}

impl Session {
    /// Starts reading the default `media_type` device into `output`;
    /// `attach` sets `delegate` as the output's delegate on the queue.
    fn start(
        media_type: &AVMediaType,
        what: &str,
        output: &AVCaptureOutput,
        make_delegate: impl FnOnce(&AVCaptureSession) -> Retained<CaptureDelegate>,
        cancel: Option<&AtomicBool>,
        attach: impl FnOnce(&CaptureDelegate, &DispatchQueue),
    ) -> Result<Session> {
        authorize(media_type, what, cancel)?;
        check_cancel(cancel)?;
        let queue = DispatchQueue::new("com.devolutions.jiffshot.media", None);
        // SAFETY: AVFoundation calls with valid arguments; the session is
        // configured before it starts.
        unsafe {
            let device = AVCaptureDevice::defaultDeviceWithMediaType(media_type).ok_or_else(|| format!("no {} found", what.to_lowercase()))?;
            let input = AVCaptureDeviceInput::deviceInputWithDevice_error(&device).map_err(|e| ns_err(&e))?;
            let session = AVCaptureSession::new();
            session.beginConfiguration();
            let ok = session.canAddInput(&input) && session.canAddOutput(output);
            if ok {
                session.addInput(&input);
                session.addOutput(output);
            }
            session.commitConfiguration();
            if !ok {
                return Err(format!("cannot read the {}", what.to_lowercase()));
            }
            let delegate = make_delegate(&session);
            attach(&delegate, &queue);
            let events = SessionEvents::observe(&session, &device);
            check_cancel(cancel)?;
            session.startRunning();
            let running = Session { session, queue, _delegate: delegate, events };
            check_cancel(cancel)?;
            if let Some(error) = running.failure() {
                return Err(error);
            }
            if !running.session.isRunning() {
                return Err(format!("the {} did not start", what.to_lowercase()));
            }
            Ok(running)
        }
    }

    fn failure(&self) -> Option<String> {
        self.events.failure()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.events.remove();
        // SAFETY: stopping a running session (synchronous).
        unsafe { self.session.stopRunning() };
        // Let callbacks already queued finish before the delegate goes.
        self.queue.exec_sync(|| {});
    }
}

/// Linear resampling of interleaved stereo, continuous across blocks.
struct Resampler {
    /// Input frames per output frame.
    step: f64,
    /// The next output's position in input frames; 0 is `prev`, 1 the
    /// block's first frame.
    pos: f64,
    prev: [f32; 2],
}

impl Resampler {
    fn new(from: f64, to: f64) -> Resampler {
        Resampler { step: from / to, pos: 1.0, prev: [0.0; 2] }
    }

    fn process(&mut self, input: &[f32]) -> (Vec<f32>, f64) {
        let n = input.len() / 2;
        let first = self.pos - 1.0;
        let prev = self.prev;
        let frame = |i: usize| if i == 0 { prev } else { [input[(i - 1) * 2], input[(i - 1) * 2 + 1]] };
        let mut out = Vec::with_capacity(((n as f64 / self.step) as usize + 1) * 2);
        while self.pos <= n as f64 {
            let i = self.pos.floor() as usize;
            let t = (self.pos - i as f64) as f32;
            let (a, b) = (frame(i), if i < n { frame(i + 1) } else { frame(i) });
            out.extend([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]);
            self.pos += self.step;
        }
        self.pos -= n as f64;
        if n > 0 {
            self.prev = frame(n);
        }
        (out, first)
    }
}

/// Converts captured PCM of any layout to interleaved 16-bit stereo at
/// `rate`.
struct Converter {
    rate: u32,
    resampler: Option<(u32, Resampler)>,
    expected_input: Option<(u32, Instant)>,
}

fn valid_sample_rate(rate: f64) -> Option<u32> {
    (rate.is_finite() && rate >= 1.0 && rate <= u32::MAX as f64).then(|| rate.round() as u32)
}

impl Converter {
    fn new(rate: u32) -> Converter {
        Converter { rate, resampler: None, expected_input: None }
    }

    fn convert(&mut self, sample: &CMSampleBuffer, captured_at: Instant) -> Option<AudioBlock> {
        // SAFETY: plain property reads.
        let desc = unsafe { sample.format_description() }?;
        let asbd = stream_description(&desc)?;
        let from = valid_sample_rate(asbd.sample_rate)?;
        let stereo = read_stereo(sample, &asbd)?;
        if self.expected_input.is_some_and(|(previous_rate, expected)| {
            previous_rate != from
                || if captured_at >= expected { captured_at - expected } else { expected - captured_at } > Duration::from_millis(2)
        }) {
            self.resampler = None;
        }
        let input_nanos = (stereo.len() as u128 / 2).checked_mul(1_000_000_000)?.checked_div(from as u128)?;
        let input_duration = Duration::from_nanos(u64::try_from(input_nanos).ok()?);
        self.expected_input = Some((from, captured_at.checked_add(input_duration)?));
        let (stereo, offset) = if from == self.rate {
            self.resampler = None;
            (stereo, 0.0)
        } else {
            if self.resampler.as_ref().map(|r| r.0) != Some(from) {
                self.resampler = Some((from, Resampler::new(from as f64, self.rate as f64)));
            }
            self.resampler.as_mut().unwrap().1.process(&stereo)
        };
        let offset = offset / from as f64;
        let delta = Duration::from_secs_f64(offset.abs());
        let captured_at = if offset < 0.0 { captured_at.checked_sub(delta)? } else { captured_at.checked_add(delta)? };
        Some(AudioBlock {
            samples: stereo.iter().map(|v| (v.clamp(-1.0, 1.0) * 32767.0) as i16).collect(),
            captured_at: Some(captured_at),
        })
    }
}

/// A sample buffer's linear PCM as interleaved stereo floats (mono is
/// duplicated, extra channels dropped).
fn read_stereo(sample: &CMSampleBuffer, asbd: &Asbd) -> Option<Vec<f32>> {
    let channels = asbd.channels_per_frame as usize;
    let bytes = (asbd.bits_per_channel / 8) as usize;
    let float = asbd.format_flags & FLAG_FLOAT != 0;
    if asbd.format_id != FORMAT_LPCM || channels == 0 || asbd.format_flags & FLAG_BIG_ENDIAN != 0 || !matches!((float, bytes), (true, 4) | (false, 2) | (false, 4)) {
        return None;
    }
    let planar = asbd.format_flags & FLAG_NON_INTERLEAVED != 0;
    // SAFETY: the list is sized as CoreMedia asks and 8-byte aligned; its
    // buffers stay valid while `block` (the retained backing) lives; every
    // read is within a buffer's byte size.
    unsafe {
        let mut needed = 0usize;
        CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(sample, &mut needed, ptr::null_mut(), 0, ptr::null(), ptr::null(), 0, ptr::null_mut());
        let mut list = vec![0u64; needed.max(24).div_ceil(8)];
        let mut block: *mut CMBlockBuffer = ptr::null_mut();
        let status = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sample,
            ptr::null_mut(),
            list.as_mut_ptr() as *mut c_void,
            list.len() * 8,
            ptr::null(),
            ptr::null(),
            0,
            &mut block,
        );
        let _block = CFRetained::from_raw(NonNull::new(block)?);
        if status != 0 {
            return None;
        }
        let count = *(list.as_ptr() as *const u32) as usize;
        let buffers = std::slice::from_raw_parts((list.as_ptr() as *const u8).add(8) as *const AudioBuffer, count);
        let read = |buffer: &AudioBuffer, index: usize| -> f32 {
            match (float, bytes) {
                (true, _) => *(buffer.data as *const f32).add(index),
                (false, 2) => *(buffer.data as *const i16).add(index) as f32 / 32768.0,
                _ => *(buffer.data as *const i32).add(index) as f32 / 2_147_483_648.0,
            }
        };
        let valid = |b: &&AudioBuffer| !b.data.is_null();
        let frames = if planar {
            if buffers.len() < channels || !buffers.iter().take(channels).all(|b| valid(&b)) {
                return None;
            }
            buffers.iter().take(channels).map(|b| b.size as usize / bytes).min()?
        } else {
            let b = buffers.first().filter(valid)?;
            b.size as usize / (bytes * channels)
        };
        let mut out = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            for c in 0..2 {
                let c = c.min(channels - 1);
                out.push(if planar { read(&buffers[c], i) } else { read(&buffers[0], i * channels + c) });
            }
        }
        Some(out)
    }
}

fn clock_nanos(time: CMTime) -> Result<i128> {
    if !time.flags.contains(CMTimeFlags::Valid)
        || time.flags.intersects(CMTimeFlags::ImpliedValueFlagsMask)
        || time.timescale <= 0
        || time.epoch != 0
    {
        return Err("invalid audio capture timestamp".into());
    }
    Ok(time.value as i128 * 1_000_000_000 / time.timescale as i128)
}

fn instant_for_clock_time(time: CMTime, clock: &CMClock) -> Result<Instant> {
    clock_nanos(time)?;
    // SAFETY: both arguments are live CoreMedia clocks, as CMSyncConvertTime requires.
    let host = unsafe { CMClock::host_time_clock() };
    let timestamp = unsafe { CMSyncConvertTime(time, clock.as_ref(), host.as_ref()) };
    // Bracket the host-time read instead of assuming it shares Instant's epoch.
    let before = Instant::now();
    // SAFETY: host is the live host-time clock.
    let host_now = unsafe { host.time() };
    let after = Instant::now();
    let midpoint = before + (after - before) / 2;
    let offset = clock_nanos(timestamp)? - clock_nanos(host_now)?;
    let magnitude = u64::try_from(offset.unsigned_abs()).map_err(|_| "audio capture timestamp is out of range")?;
    let delta = Duration::from_nanos(magnitude);
    if offset < 0 {
        midpoint.checked_sub(delta)
    } else {
        midpoint.checked_add(delta)
    }
    .ok_or_else(|| "audio capture timestamp is out of range".into())
}

fn deliver_audio(
    sample: &CMSampleBuffer,
    clock: Option<&CMClock>,
    converter: &Mutex<Converter>,
    output: &Sender<AudioBlock>,
    failure: &Mutex<Option<String>>,
    source: &str,
) {
    let result = (|| {
        let clock = clock.ok_or("audio synchronization clock is unavailable")?;
        // SAFETY: the sample is alive for this property read.
        let captured_at = instant_for_clock_time(unsafe { sample.presentation_time_stamp() }, clock)?;
        converter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .convert(sample, captured_at)
            .ok_or_else(|| "unsupported or unreadable audio samples".to_owned())
    })();
    match result {
        Ok(block) => {
            let _ = output.send(block);
        }
        Err(error) => report_failure(failure, format!("{source}: {error}")),
    }
}

/// Audio being captured: interleaved 16-bit stereo blocks arrive on
/// `samples`; dropping the handle stops capture.
pub struct AudioCapture {
    pub spec: AudioSpec,
    pub samples: Receiver<AudioBlock>,
    _source: Box<dyn Send>,
    failures: Vec<Arc<Mutex<Option<String>>>>,
}

impl AudioCapture {
    pub fn failure(&self) -> Option<String> {
        self.failures.iter().find_map(|failure| failure.lock().unwrap_or_else(|e| e.into_inner()).clone())
    }
}

/// Starts capturing what the speakers play (`microphone` false) or the
/// default microphone, converted to 16-bit stereo at 48 kHz.
pub fn capture_audio(microphone: bool) -> Result<AudioCapture> {
    capture_audio_with_cancel(microphone, None)
}

/// Starts an audio source unless `cancel` is set, including while microphone
/// permission is pending.
pub fn capture_audio_cancellable(microphone: bool, cancel: Arc<AtomicBool>) -> Result<AudioCapture> {
    capture_audio_with_cancel(microphone, Some(&cancel))
}

fn capture_audio_with_cancel(microphone: bool, cancel: Option<&AtomicBool>) -> Result<AudioCapture> {
    check_cancel(cancel)?;
    let spec = AudioSpec { sample_rate: CAPTURE_RATE, channels: 2 };
    let (tx, rx) = mpsc::channel();
    let converter = Mutex::new(Converter::new(CAPTURE_RATE));
    let conversion_failure = Arc::new(Mutex::new(None));
    let mut failures = vec![conversion_failure.clone()];
    let source: Box<dyn Send> = if microphone {
        // SAFETY: a fresh output; it delivers the device's own PCM format,
        // which the converter reads.
        let output = unsafe { AVCaptureAudioDataOutput::new() };
        let session = Session::start(audio_type(), "Microphone", &output, |session| {
            let session = WeakCaptureSession(Weak::new(session));
            CaptureDelegate::new(Box::new(move |sample| {
                let clock = session.clock();
                deliver_audio(sample, clock.as_deref(), &converter, &tx, &conversion_failure, "microphone");
            }))
        }, cancel, |d, q| {
            // SAFETY: a serial queue, as required; the delegate outlives the session.
            unsafe { output.setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(d)), Some(q)) }
        })?;
        failures.push(session.events.failure.clone());
        Box::new(session)
    } else {
        let content = sck::shareable_content().map_err(|e| e.to_string())?;
        check_cancel(cancel)?;
        // SAFETY: plain property reads and setters on a fresh configuration.
        let (filter, config) = unsafe {
            let display = content.displays().firstObject().ok_or("no display to capture audio from")?;
            let filter = sck::display_filter(&content, &display, &[]);
            // The stream needs a picture too; a tiny, rare one.
            let config = sck::config(rect(0.0, 0.0, 2.0, 2.0), 2, 2, false);
            config.setMinimumFrameInterval(sck::frame_interval(1));
            config.setCapturesAudio(true);
            // System audio includes Jiffshot's own playback, as on Windows.
            config.setExcludesCurrentProcessAudio(false);
            config.setSampleRate(CAPTURE_RATE as isize);
            config.setChannelCount(2);
            (filter, config)
        };
        let kinds = [SCStreamOutputType::Screen, SCStreamOutputType::Audio];
        check_cancel(cancel)?;
        let stream = sck::Stream::start(&filter, &config, &kinds, move |stream, sample, kind| {
            if kind == SCStreamOutputType::Audio {
                // SAFETY: the live stream owns this read-only clock.
                let clock = unsafe { stream.synchronizationClock() };
                deliver_audio(sample, clock.as_deref(), &converter, &tx, &conversion_failure, "system audio");
            }
        })
        .map_err(|e| e.to_string())?;
        failures.push(stream.failure_state());
        Box::new(SendBox(stream))
    };
    check_cancel(cancel)?;
    Ok(AudioCapture { spec, samples: rx, _source: source, failures })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_paths_with_nul_bytes_are_rejected() {
        use std::os::unix::ffi::OsStringExt;

        let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(b"invalid\0recording.mp4".to_vec()));
        let spec = VideoSpec::for_size(64, 64, 15);
        assert!(Writer::create(&path, spec, None).err().unwrap().contains("NUL byte"));
        assert!(Reader::open(&path).err().unwrap().contains("NUL byte"));
    }

    fn synthetic_audio_sample(rate: u32, frames: usize) -> CFRetained<CMSampleBuffer> {
        let asbd = Asbd {
            sample_rate: f64::from(rate),
            format_id: FORMAT_LPCM,
            format_flags: FLAG_SIGNED | FLAG_PACKED,
            bytes_per_packet: 4,
            frames_per_packet: 1,
            bytes_per_frame: 4,
            channels_per_frame: 2,
            bits_per_channel: 16,
            reserved: 0,
        };
        let pcm = vec![1_000i16; frames * 2];
        // SAFETY: the format and block are retained until the sample buffer
        // owns them, and all byte counts match the source PCM.
        unsafe {
            let mut format = ptr::null();
            assert_eq!(CMAudioFormatDescriptionCreate(ptr::null(), &asbd, 0, ptr::null(), 0, ptr::null(), ptr::null(), &mut format), 0);
            let format = CFRetained::from_raw(NonNull::new(format as *mut CMFormatDescription).unwrap());
            let bytes = pcm.len() * 2;
            let mut block: *mut CMBlockBuffer = ptr::null_mut();
            assert_eq!(
                CMBlockBuffer::create_with_memory_block(
                    None,
                    ptr::null_mut(),
                    bytes,
                    None,
                    ptr::null(),
                    0,
                    bytes,
                    kCMBlockBufferAssureMemoryNowFlag,
                    NonNull::from(&mut block),
                ),
                0
            );
            let block = CFRetained::from_raw(NonNull::new(block).unwrap());
            assert_eq!(CMBlockBuffer::replace_data_bytes(NonNull::new(pcm.as_ptr() as *mut c_void).unwrap(), &block, 0, bytes), 0);
            let mut sample: *mut CMSampleBuffer = ptr::null_mut();
            assert_eq!(
                CMAudioSampleBufferCreateReadyWithPacketDescriptions(
                    None,
                    &block,
                    &format,
                    frames as isize,
                    cm_time(0),
                    ptr::null(),
                    NonNull::from(&mut sample),
                ),
                0
            );
            CFRetained::from_raw(NonNull::new(sample).unwrap())
        }
    }

    #[test]
    fn waits_distinguish_cancellation_timeout_failure_and_disconnection() {
        let cancel = AtomicBool::new(true);
        let (tx, rx) = mpsc::channel();
        tx.send(7).unwrap();
        assert_eq!(wait_for(&rx, Duration::from_secs(1), Some(&cancel), || None), Err(WaitError::Cancelled));
        cancel.store(false, Ordering::Release);
        assert_eq!(wait_for(&rx, Duration::from_secs(1), Some(&cancel), || None), Ok(7));
        assert_eq!(
            wait_for(&rx, Duration::from_secs(1), None, || Some("device lost".into())),
            Err(WaitError::Failed("device lost".into()))
        );
        assert_eq!(wait_for(&rx, Duration::from_millis(10), None, || None), Err(WaitError::TimedOut));
        drop(tx);
        assert_eq!(wait_for(&rx, Duration::from_secs(1), None, || None), Err(WaitError::Disconnected));
    }

    #[test]
    fn cancelled_permission_wait_does_not_accept_a_late_response() {
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let worker_cancel = cancel.clone();
        let worker = std::thread::spawn(move || wait_for(&rx, Duration::from_secs(5), Some(&worker_cancel), || None));
        cancel.store(true, Ordering::Release);
        assert_eq!(worker.join().unwrap(), Err(WaitError::Cancelled));
        assert!(tx.send(true).is_err());
    }

    #[test]
    fn session_notifications_report_only_the_selected_session_and_device() {
        // SAFETY: fresh, unconfigured sessions do not open a device.
        let session = unsafe { AVCaptureSession::new() };
        let other_session = unsafe { AVCaptureSession::new() };
        let device = NSObject::new();
        let other_device = NSObject::new();
        let center = NSNotificationCenter::defaultCenter();
        let mut events = SessionEvents::observe(&session, &device);
        // SAFETY: observers only compare the synthetic object identities.
        unsafe {
            center.postNotificationName_object(AVCaptureSessionWasInterruptedNotification, Some(&other_session));
            center.postNotificationName_object(AVCaptureDeviceWasDisconnectedNotification, Some(&other_device));
        }
        assert_eq!(events.failure(), None);

        // SAFETY: the dictionary has the documented error key and NSError value.
        let error = unsafe { NSError::errorWithDomain_code_userInfo(ns_string!("JiffshotTest"), 42, None) };
        let description = ns_err(&error);
        let info = dict(&[(unsafe { AVCaptureSessionErrorKey }, &error)]);
        unsafe {
            let _: () = msg_send![
                &*center,
                postNotificationName: AVCaptureSessionRuntimeErrorNotification,
                object: &*session,
                userInfo: &*info
            ];
            center.postNotificationName_object(AVCaptureDeviceWasDisconnectedNotification, Some(&device));
        }
        assert_eq!(events.failure(), Some(format!("capture session failed: {description}")));

        events.remove();
        *events.failure.lock().unwrap() = None;
        unsafe { center.postNotificationName_object(AVCaptureSessionWasInterruptedNotification, Some(&session)) };
        assert_eq!(events.failure(), None);
    }

    #[test]
    fn session_interruptions_stops_and_device_disconnects_are_reported() {
        // SAFETY: a fresh, unconfigured session does not open a device.
        let session = unsafe { AVCaptureSession::new() };
        let device = NSObject::new();
        let center = NSNotificationCenter::defaultCenter();
        // SAFETY: reading framework notification names; the synthetic
        // objects are scoped to this test and open no capture device.
        let cases = unsafe {
            [
                (AVCaptureSessionWasInterruptedNotification, &*session as &AnyObject, "capture session was interrupted"),
                (AVCaptureSessionDidStopRunningNotification, &*session as &AnyObject, "capture session stopped unexpectedly"),
                (AVCaptureDeviceWasDisconnectedNotification, &*device as &AnyObject, "capture device disconnected"),
            ]
        };
        for (name, object, expected) in cases {
            let events = SessionEvents::observe(&session, &device);
            unsafe { center.postNotificationName_object(name, Some(object)) };
            assert_eq!(events.failure().as_deref(), Some(expected));
        }
    }

    #[test]
    fn audio_capture_reports_stream_and_conversion_failures() {
        let conversion = Arc::new(Mutex::new(None));
        let stream = Arc::new(Mutex::new(None));
        let (_tx, samples) = mpsc::channel();
        let capture = AudioCapture {
            spec: AudioSpec { sample_rate: CAPTURE_RATE, channels: 2 },
            samples,
            _source: Box::new(()),
            failures: vec![conversion.clone(), stream.clone()],
        };
        assert_eq!(capture.failure(), None);
        report_failure(&stream, "device stopped".into());
        assert_eq!(capture.failure().as_deref(), Some("device stopped"));
        report_failure(&conversion, "unreadable audio".into());
        assert_eq!(capture.failure().as_deref(), Some("unreadable audio"));
    }

    #[test]
    fn resampler_keeps_pace_across_blocks() {
        let mut r = Resampler::new(44_100.0, 48_000.0);
        let block: Vec<f32> = (0..441 * 2).map(|i| (i / 2) as f32 / 441.0).collect();
        let total: usize = (0..100).map(|_| r.process(&block).0.len() / 2).sum();
        assert!((total as i64 - 48_000).abs() <= 1, "{total} frames for a second");
        let mut same = Resampler::new(48_000.0, 48_000.0);
        assert_eq!(same.process(&[0.5, -0.5, 0.25, -0.25]).0, vec![0.5, -0.5, 0.25, -0.25]);
    }

    #[test]
    fn resampler_reports_the_first_output_position_across_blocks() {
        let mut r = Resampler::new(44_100.0, 48_000.0);
        assert_eq!(r.process(&[0.0; 8]).1, 0.0);
        let (_, offset) = r.process(&[0.0; 8]);
        assert!(offset < 0.0 && offset > -1.0, "offset was {offset}");
    }

    #[test]
    fn converter_timestamps_resampled_blocks_and_resets_after_a_gap() {
        let sample = synthetic_audio_sample(44_100, 4);
        let start = Instant::now();
        let mut converter = Converter::new(48_000);
        let first = converter.convert(&sample, start).unwrap();
        assert_eq!(first.captured_at, Some(start));
        assert_eq!(first.samples.len() / 2, 4);

        let next_input = start + Duration::from_nanos(4 * 1_000_000_000 / 44_100);
        let second = converter.convert(&sample, next_input).unwrap();
        let second_at = second.captured_at.unwrap().duration_since(start);
        assert!(second_at.abs_diff(Duration::from_nanos(4 * 1_000_000_000 / 48_000)) <= Duration::from_nanos(2));

        let after_gap = start + Duration::from_secs(1);
        let third = converter.convert(&sample, after_gap).unwrap();
        assert_eq!(third.captured_at, Some(after_gap));
    }

    #[test]
    fn converter_keeps_resampled_capture_on_the_input_timeline() {
        let sample = synthetic_audio_sample(44_100, 441);
        let start = Instant::now();
        let mut converter = Converter::new(48_000);
        let mut total = 0usize;
        for i in 0..100 {
            let block = converter.convert(&sample, start + Duration::from_millis(i * 10)).unwrap();
            let expected = Duration::from_secs_f64(total as f64 / 48_000.0);
            let actual = block.captured_at.unwrap().duration_since(start);
            assert!(actual.abs_diff(expected) <= Duration::from_nanos(100), "block {i}: {actual:?} vs {expected:?}");
            total += block.samples.len() / 2;
        }
        assert!((total as i64 - 48_000).abs() <= 1);
    }

    #[test]
    fn host_clock_timestamps_map_to_monotonic_instants() {
        // SAFETY: the host-time clock is a process-wide CoreMedia singleton.
        let host = unsafe { CMClock::host_time_clock() };
        let mut time = unsafe { host.time() };
        time.value -= i64::from(time.timescale);
        let now = Instant::now();
        let mapped = instant_for_clock_time(time, &host).unwrap();
        let elapsed = now.duration_since(mapped);
        assert!((Duration::from_millis(990)..Duration::from_millis(1010)).contains(&elapsed), "{elapsed:?}");

        time.flags = CMTimeFlags::empty();
        assert!(instant_for_clock_time(time, &host).is_err());
        time.flags = CMTimeFlags::Valid;
        time.timescale = 0;
        assert!(instant_for_clock_time(time, &host).is_err());
        time.timescale = 1;
        time.epoch = 1;
        assert!(instant_for_clock_time(time, &host).is_err());
    }

    #[test]
    fn host_clock_and_monotonic_clock_advance_together() {
        // SAFETY: the host-time clock is a process-wide CoreMedia singleton.
        let host = unsafe { CMClock::host_time_clock() };
        let before_first = Instant::now();
        let first = clock_nanos(unsafe { host.time() }).unwrap();
        let after_first = Instant::now();
        std::thread::sleep(Duration::from_millis(20));
        let before_second = Instant::now();
        let second = clock_nanos(unsafe { host.time() }).unwrap();
        let after_second = Instant::now();
        let host_elapsed = Duration::from_nanos(u64::try_from(second - first).unwrap());
        let tolerance = Duration::from_millis(1);
        assert!(host_elapsed + tolerance >= before_second - after_first);
        assert!(host_elapsed <= after_second - before_first + tolerance);
    }

    #[test]
    fn weak_session_clock_does_not_outlive_the_capture_session() {
        // SAFETY: an unconfigured session does not open a device.
        let session = unsafe { AVCaptureSession::new() };
        let weak = WeakCaptureSession(Weak::new(&session));
        assert!(weak.clock().is_none());
        drop(session);
        assert!(weak.clock().is_none());
    }

    #[test]
    fn invalid_audio_sample_rates_are_rejected() {
        for rate in [0.0, -1.0, f64::NAN, f64::INFINITY, u32::MAX as f64 + 1.0] {
            assert_eq!(valid_sample_rate(rate), None);
        }
        assert_eq!(valid_sample_rate(44_100.0), Some(44_100));
        assert_eq!(valid_sample_rate(48_000.0), Some(48_000));
    }

    #[test]
    fn audio_frame_timestamps_survive_hns_round_trips() {
        for rate in [44_100, 48_000] {
            for frame in 0..10_000 {
                let time = frame * HNS_PER_SECOND / i64::from(rate);
                assert_eq!(audio_frame_position(time, rate).unwrap(), frame);
            }
        }
        assert!(audio_frame_position(-1, 48_000).is_err());
        assert!(audio_frame_position(0, 0).is_err());
    }

    #[test]
    fn quarter_turns_map_pixels() {
        // 2 x 1 picture: pixels 1 and 2.
        let src = vec![1, 1, 1, 1, 2, 2, 2, 2];
        // A 90 degree clockwise turn (as phones record portrait video).
        let (out, w, h) = apply_turn(src.clone(), 2, 1, quarter_turn(0.0, 1.0, -1.0, 0.0));
        assert_eq!((w, h, out[0], out[4]), (1, 2, 1, 2));
        let (out, ..) = apply_turn(src.clone(), 2, 1, quarter_turn(-1.0, 0.0, 0.0, -1.0));
        assert_eq!((out[0], out[4]), (2, 1));
        assert_eq!(quarter_turn(0.7, 0.7, -0.7, 0.7), [1, 0, 0, 1]);
    }

    fn capture_a_second(microphone: bool) {
        let c = capture_audio(microphone).expect("audio capture must start");
        assert_eq!(c.spec, AudioSpec { sample_rate: 48_000, channels: 2 });
        let start = Instant::now();
        let mut frames = 0;
        let mut peak = 0i32;
        while start.elapsed() < Duration::from_secs(1) {
            if let Ok(block) = c.samples.recv_timeout(Duration::from_millis(200)) {
                assert!(block.captured_at.is_some(), "audio block has no capture timestamp");
                assert_eq!(block.samples.len() % 2, 0);
                frames += block.samples.len() / 2;
                peak = peak.max(block.samples.iter().map(|&sample| (sample as i32).abs()).max().unwrap_or(0));
            }
        }
        assert!(frames > 0, "no audio samples arrived; check permissions and provide an active audio source");
        if std::env::var_os("JIFFSHOT_TEST_REQUIRE_AUDIO_SIGNAL").is_some() {
            assert!(peak > 0, "audio samples were all silent");
        }
        println!("captured {frames} frames, peak {peak} (microphone: {microphone})");
    }

    /// Needs Screen & System Audio Recording permission.
    #[test]
    #[ignore]
    fn captures_system_audio() {
        capture_a_second(false);
    }

    /// Needs Microphone permission.
    #[test]
    #[ignore]
    fn captures_the_microphone() {
        capture_a_second(true);
    }
}
