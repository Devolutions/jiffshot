//! ScreenCaptureKit plumbing shared by screen grabs, recording and system
//! audio: blocking access to the shareable content, still screenshots,
//! continuous streams, and pixel-buffer and image copies.

use super::ffi::*;
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_foundation::CGRect;
use objc2_core_graphics::{kCGColorSpaceSRGB, CGColorSpace, CGImage};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    kCVPixelFormatType_32BGRA, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType, SCWindow,
};
use std::io;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub fn err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

fn ns_err(error: *mut NSError) -> io::Error {
    // SAFETY: a non-null NSError handed to a completion handler.
    match unsafe { error.as_ref() } {
        Some(e) => err(e.localizedDescription()),
        None => err("ScreenCaptureKit failed"),
    }
}

/// Whether the user allowed this app to record the screen. Asking shows
/// the system prompt once; afterwards the user must go to System Settings.
pub fn screen_access(ask: bool) -> bool {
    // SAFETY: plain queries.
    unsafe { CGPreflightScreenCaptureAccess() || (ask && CGRequestScreenCaptureAccess()) }
}

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Jiffshot needs Screen Recording permission (System Settings > Privacy & Security > Screen & System Audio Recording)",
    )
}

/// Displays and windows that can be captured, fetched synchronously.
pub fn shareable_content() -> io::Result<Retained<SCShareableContent>> {
    shareable_content_with_visibility(true)
}

/// Includes hidden windows so an existing filter can exclude them if shown.
pub fn shareable_content_including_hidden() -> io::Result<Retained<SCShareableContent>> {
    shareable_content_with_visibility(false)
}

fn shareable_content_with_visibility(on_screen_only: bool) -> io::Result<Retained<SCShareableContent>> {
    if !screen_access(true) {
        return Err(denied());
    }
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        // SAFETY: the handler's content is valid for its duration; retained
        // to keep it.
        let result = match unsafe { Retained::retain(content) } {
            Some(c) => Ok(c),
            None => Err(ns_err(error)),
        };
        let _ = tx.send(SendBox(result));
    });
    // SAFETY: the block lives until called (ScreenCaptureKit copies it).
    unsafe { SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(false, on_screen_only, &handler) };
    rx.recv_timeout(Duration::from_secs(10)).map_err(|_| err("ScreenCaptureKit did not answer"))?.0
}

/// Moves an Objective-C value between threads where the framework allows it.
pub struct SendBox<T>(pub T);
// SAFETY: used only for immutable framework results that are thread-safe
// to read (ScreenCaptureKit objects, CGImages and sample buffers).
unsafe impl<T> Send for SendBox<T> {}

/// A filter for `display` without the windows in `excluded` (window numbers).
pub fn display_filter(content: &SCShareableContent, display: &SCDisplay, excluded: &[u32]) -> Retained<SCContentFilter> {
    // SAFETY: plain property reads and an initializer with valid arguments.
    unsafe {
        let windows: Vec<Retained<SCWindow>> =
            content.windows().iter().filter(|w| excluded.contains(&w.windowID())).collect();
        let windows = NSArray::from_retained_slice(&windows);
        SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), display, &windows)
    }
}

/// A BGRA configuration producing `width` x `height` pixels from
/// `source` (points, relative to the display).
pub fn config(source: CGRect, width: usize, height: usize, cursor: bool) -> Retained<SCStreamConfiguration> {
    // SAFETY: setters on a fresh configuration.
    unsafe {
        let c = SCStreamConfiguration::new();
        c.setSourceRect(source);
        c.setWidth(width.max(1));
        c.setHeight(height.max(1));
        c.setPixelFormat(kCVPixelFormatType_32BGRA);
        c.setColorSpaceName(kCGColorSpaceSRGB);
        c.setShowsCursor(cursor);
        c.setScalesToFit(false);
        c
    }
}

/// Copies a CGImage into top-down BGRA bytes of `width` x `height`
/// (scaling if the image differs).
pub fn image_bgra(image: CGImageRef, width: usize, height: usize) -> io::Result<Vec<u8>> {
    let mut pixels = vec![0u8; width * height * 4];
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
        .ok_or_else(|| err("could not create the sRGB color space"))?;
    // SAFETY: the context draws into `pixels`, which is exactly
    // `height * width * 4` bytes and outlives it.
    unsafe {
        let ctx = CGBitmapContextCreate(
            pixels.as_mut_ptr() as *mut _,
            width,
            height,
            8,
            width * 4,
            (&*space as *const CGColorSpace).cast_mut().cast(),
            kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little,
        );
        if ctx.is_null() {
            return Err(err("could not create a bitmap context"));
        }
        CGContextSetInterpolationQuality(ctx, 3);
        CGContextDrawImage(ctx, rect(0.0, 0.0, width as f64, height as f64), image);
        CGContextRelease(ctx);
    }
    Ok(pixels)
}

/// A CGImage holding a copy of top-down BGRA pixels (alpha ignored).
pub fn bgra_image(width: usize, height: usize, bgra: &[u8]) -> io::Result<Owned> {
    let mut pixels = bgra[..width * height * 4].to_vec();
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
        .ok_or_else(|| err("could not create the sRGB color space"))?;
    // SAFETY: the context wraps `pixels` only until the image is copied out
    // of it; CGBitmapContextCreateImage copies the bytes.
    unsafe {
        let ctx = CGBitmapContextCreate(
            pixels.as_mut_ptr() as *mut _,
            width,
            height,
            8,
            width * 4,
            (&*space as *const CGColorSpace).cast_mut().cast(),
            kCGImageAlphaNoneSkipFirst | kCGBitmapByteOrder32Little,
        );
        if ctx.is_null() {
            return Err(err("could not create a bitmap context"));
        }
        let image = CGBitmapContextCreateImage(ctx);
        CGContextRelease(ctx);
        if image.is_null() {
            return Err(err("could not create an image"));
        }
        Ok(Owned(image as _))
    }
}

/// Copies the BGRA pixels of a sample buffer's image into `out`
/// (`width * 4` bytes per row). Returns the image size, or `None` for
/// buffers without an image (ScreenCaptureKit's idle frames).
pub fn sample_bgra(sample: &CMSampleBuffer, out: &mut Vec<u8>) -> Option<(usize, usize)> {
    // SAFETY: the pixel buffer is locked read-only while its rows are copied.
    unsafe {
        let buffer = sample.image_buffer()?;
        let (w, h) = (CVPixelBufferGetWidth(&buffer), CVPixelBufferGetHeight(&buffer));
        if CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags::ReadOnly) != 0 {
            return None;
        }
        let base = CVPixelBufferGetBaseAddress(&buffer) as *const u8;
        let stride = CVPixelBufferGetBytesPerRow(&buffer);
        if base.is_null() || stride < w * 4 {
            CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags::ReadOnly);
            return None;
        }
        out.resize(w * h * 4, 0);
        for y in 0..h {
            std::ptr::copy_nonoverlapping(base.add(y * stride), out.as_mut_ptr().add(y * w * 4), w * 4);
        }
        CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags::ReadOnly);
        Some((w, h))
    }
}

fn screen_frame_status(sample: &CMSampleBuffer) -> Option<SCFrameStatus> {
    // SAFETY: Core Media returns a retained CFArray, toll-free bridged to
    // NSArray. The callback keeps the sample and its attachments alive.
    let attachments = unsafe { sample.sample_attachments_array(false)? };
    let array = unsafe { &*((&*attachments as *const objc2_core_foundation::CFArray).cast::<NSArray<AnyObject>>()) };
    let first = array.firstObject()?.downcast::<NSDictionary>().ok()?;
    // SAFETY: ScreenCaptureKit's sample attachment dictionary uses NSString keys.
    let first: Retained<NSDictionary<NSString, AnyObject>> = unsafe { Retained::cast_unchecked(first) };
    let value = first.objectForKey(unsafe { SCStreamFrameInfoStatus })?.downcast::<NSNumber>().ok()?;
    Some(SCFrameStatus(value.integerValue()))
}

fn complete_screen_frame(status: Option<SCFrameStatus>) -> io::Result<bool> {
    match status {
        Some(SCFrameStatus::Complete) => Ok(true),
        Some(SCFrameStatus::Idle | SCFrameStatus::Started) => Ok(false),
        Some(SCFrameStatus::Blank) => Err(err("screen capture became blank")),
        Some(SCFrameStatus::Suspended) => Err(err("screen capture was suspended")),
        Some(SCFrameStatus::Stopped) => Err(err("screen capture stopped")),
        Some(other) => Err(err(format!("unknown screen frame status {}", other.0))),
        None => Err(err("screen capture frame has no valid status")),
    }
}

/// Copies only complete ScreenCaptureKit frames; idle frames need no update.
pub fn screen_frame_bgra(sample: &CMSampleBuffer, out: &mut Vec<u8>) -> io::Result<Option<(usize, usize)>> {
    if !complete_screen_frame(screen_frame_status(sample))? {
        return Ok(None);
    }
    sample_bgra(sample, out).map(Some).ok_or_else(|| err("a complete screen frame has no readable pixels"))
}

type Handler = Box<dyn Fn(&SCStream, &CMSampleBuffer, SCStreamOutputType) + Send + Sync>;

pub struct OutputIvars {
    handler: Handler,
    failure: Arc<Mutex<Option<String>>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[ivars = OutputIvars]
    pub struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            (self.ivars().handler)(stream, sample, kind);
        }
    }

    unsafe impl SCStreamDelegate for StreamOutput {
        #[allow(non_snake_case)]
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn stream_didStopWithError(&self, _stream: &SCStream, error: &NSError) {
            let message = error.localizedDescription().to_string();
            *self.ivars().failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(message);
        }
    }
);

impl StreamOutput {
    fn new(handler: Handler, failure: Arc<Mutex<Option<String>>>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { handler, failure });
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

/// A running capture stream; stops when dropped.
pub struct Stream {
    stream: Retained<SCStream>,
    _output: Retained<StreamOutput>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
    failure: Arc<Mutex<Option<String>>>,
}

// SAFETY: SCStream is thread-safe for start/stop; the output object is only
// called by the stream on its queue.
unsafe impl Send for Stream {}

fn wait_done(action: &str, start: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>)) -> io::Result<()> {
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |error: *mut NSError| {
        let _ = tx.send(if error.is_null() { Ok(()) } else { Err(ns_err(error).to_string()) });
    });
    start(&block);
    rx.recv_timeout(Duration::from_secs(10)).map_err(|_| err(format!("ScreenCaptureKit did not {action}")))?.map_err(err)
}

impl Stream {
    /// Starts capturing `filter` with `config`; `handler` receives every
    /// sample (screen frames, and audio when the configuration asks).
    pub fn start(
        filter: &SCContentFilter,
        config: &SCStreamConfiguration,
        kinds: &[SCStreamOutputType],
        handler: impl Fn(&SCStream, &CMSampleBuffer, SCStreamOutputType) + Send + Sync + 'static,
    ) -> io::Result<Stream> {
        let queue = DispatchQueue::new("com.devolutions.jiffshot.capture", None);
        let failure = Arc::new(Mutex::new(None));
        let output = StreamOutput::new(Box::new(handler), failure.clone());
        // SAFETY: valid filter, configuration and output; the queue and the
        // output are kept alive with the stream.
        unsafe {
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                filter,
                config,
                Some(ProtocolObject::from_ref(&*output)),
            );
            for kind in kinds {
                stream
                    .addStreamOutput_type_sampleHandlerQueue_error(ProtocolObject::from_ref(&*output), *kind, Some(&queue))
                    .map_err(|e| err(e.localizedDescription()))?;
            }
            wait_done("start", |block| stream.startCaptureWithCompletionHandler(Some(block)))?;
            Ok(Stream { stream, _output: output, _queue: queue, failure })
        }
    }

    /// Replaces the content filter of a running stream.
    pub fn update_filter(&self, filter: &SCContentFilter) -> io::Result<()> {
        // SAFETY: the live stream and filter remain retained until the
        // completion callback reports that the update has finished.
        wait_done("update its content filter", |block| unsafe {
            self.stream.updateContentFilter_completionHandler(filter, Some(block))
        })
    }

    pub fn failure(&self) -> Option<String> {
        self.failure.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn failure_state(&self) -> Arc<Mutex<Option<String>>> {
        self.failure.clone()
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: stopping a started stream; waits so no sample arrives after
        // the handler is gone.
        let stream = &self.stream;
        let _ = wait_done("stop", |block| unsafe { stream.stopCaptureWithCompletionHandler(Some(block)) });
    }
}

/// One still image of `filter`, as top-down BGRA of the configured size.
pub fn screenshot(filter: &SCContentFilter, config: &SCStreamConfiguration, width: usize, height: usize) -> io::Result<Vec<u8>> {
    if AnyClass::get(c"SCScreenshotManager").is_some() {
        // macOS 14 and later.
        let (tx, rx) = mpsc::channel();
        let handler = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
            let result = if image.is_null() {
                Err(ns_err(error).to_string())
            } else {
                image_bgra(image as CGImageRef, width, height).map_err(|e| e.to_string())
            };
            let _ = tx.send(result);
        });
        // SAFETY: valid filter and configuration; the block is copied.
        unsafe {
            objc2_screen_capture_kit::SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
                filter,
                config,
                Some(&handler),
            )
        };
        return rx.recv_timeout(Duration::from_secs(10)).map_err(|_| err("the screenshot timed out"))?.map_err(err);
    }
    screenshot_from_stream(filter, config, width, height)
}

/// macOS 13 still capture: wait for the first complete frame of a stream.
fn screenshot_from_stream(filter: &SCContentFilter, config: &SCStreamConfiguration, width: usize, height: usize) -> io::Result<Vec<u8>> {
    let (tx, rx) = mpsc::sync_channel(1);
    let stream = Stream::start(filter, config, &[SCStreamOutputType::Screen], move |_, sample, _| {
        let mut out = Vec::new();
        match screen_frame_bgra(sample, &mut out) {
            Ok(Some(size)) if size == (width, height) => { let _ = tx.try_send(Ok(out)); }
            Ok(Some((w, h))) => { let _ = tx.try_send(Err(format!("the screenshot frame is {w}x{h}, expected {width}x{height}"))); }
            Ok(None) => {}
            Err(error) => { let _ = tx.try_send(Err(error.to_string())); }
        }
    })?;
    let pixels = rx.recv_timeout(Duration::from_secs(5))
        .map_err(|_| stream.failure().map_or_else(|| err("no complete screen frame arrived"), |failure| err(format!("screen capture stopped: {failure}"))))?
        .map_err(err)?;
    drop(stream);
    if pixels.len() != width * height * 4 {
        return Err(err("the frame has an unexpected size"));
    }
    Ok(pixels)
}

/// `1 / fps` as a CMTime.
pub fn frame_interval(fps: u32) -> CMTime {
    // SAFETY: plain value constructor.
    unsafe { CMTime::new(1, fps.max(1) as i32) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_foundation::NSString;

    #[test]
    fn screen_capture_configuration_uses_srgb() {
        let config = config(rect(0.0, 0.0, 16.0, 16.0), 32, 32, false);
        let name = unsafe { config.colorSpaceName() };
        assert_eq!(&*name, unsafe { kCGColorSpaceSRGB });
    }

    #[test]
    fn only_complete_screen_frames_are_consumed() {
        assert!(complete_screen_frame(Some(SCFrameStatus::Complete)).unwrap());
        assert!(!complete_screen_frame(Some(SCFrameStatus::Idle)).unwrap());
        assert!(!complete_screen_frame(Some(SCFrameStatus::Started)).unwrap());
        for status in [None, Some(SCFrameStatus::Blank), Some(SCFrameStatus::Suspended), Some(SCFrameStatus::Stopped), Some(SCFrameStatus(99))] {
            assert!(complete_screen_frame(status).is_err());
        }
    }

    #[test]
    #[ignore = "requires screen-recording permission and a display"]
    fn stream_screenshot_waits_for_a_complete_frame() {
        let content = shareable_content().expect("shareable content");
        let display = unsafe { content.displays() }.firstObject().expect("a display");
        let filter = display_filter(&content, &display, &[]);
        let config = config(rect(0.0, 0.0, 16.0, 16.0), 16, 16, false);
        let pixels = screenshot_from_stream(&filter, &config, 16, 16).expect("a complete screenshot frame");
        assert_eq!(pixels.len(), 16 * 16 * 4);
        let error = screenshot_from_stream(&filter, &config, 8, 32).unwrap_err();
        assert!(error.to_string().contains("expected 8x32"));
    }

    #[test]
    #[ignore = "requires screen-recording permission and a display"]
    fn stream_delegate_reports_an_unexpected_stop() {
        let content = shareable_content().expect("shareable content");
        let display = unsafe { content.displays() }.firstObject().expect("a display");
        let filter = display_filter(&content, &display, &[]);
        let config = config(rect(0.0, 0.0, 16.0, 16.0), 16, 16, false);
        let stream = Stream::start(&filter, &config, &[SCStreamOutputType::Screen], |_, _, _| {}).expect("capture stream");
        assert!(stream.failure().is_none());
        let error = unsafe { NSError::errorWithDomain_code_userInfo(&NSString::from_str("JiffshotStreamTest"), 42, None) };
        let expected = error.localizedDescription().to_string();
        // SAFETY: the live stream and its delegate receive a valid NSError.
        unsafe {
            let _: () = msg_send![&*stream._output, stream: &*stream.stream, didStopWithError: &*error];
        }
        assert_eq!(stream.failure(), Some(expected));
    }
}
