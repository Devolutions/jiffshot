use jiffshot_capture::{macos::sck, screen, CaptureMapping, Rect};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, ProtocolObject, Sel};
use objc2::{ClassType, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSApplicationWillTerminateNotification, NSBackingStoreType, NSColor, NSEvent, NSEventMask, NSEventModifierFlags, NSEventType, NSFont, NSPasteboard, NSStatusWindowLevel, NSTextField, NSWindow, NSWindowStyleMask, NSWindowWillCloseNotification};
use objc2_core_foundation::CGRect;
use objc2_foundation::{NSAppleEventDescriptor, NSAppleEventSendOptions, NSDate, NSError, NSNotification, NSNotificationCenter, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use std::cell::RefCell;
use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

thread_local! {
    static EXCLUSION_PROBE: RefCell<Option<ExclusionProbe>> = const { RefCell::new(None) };
    static RECORDING_SHUTDOWN_FIXTURE: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
    static CONTROL_FOCUS_FIXTURE: RefCell<Vec<Retained<NSWindow>>> = const { RefCell::new(Vec::new()) };
    static CLOSE_STALL: RefCell<Option<(Retained<ProtocolObject<dyn NSObjectProtocol>>, Arc<AtomicBool>)>> = const { RefCell::new(None) };
    static LIFETIME_OTHER_OWNER: RefCell<Option<(u32, screen::ExclusionTicket, screen::ExclusionTicket)>> = const { RefCell::new(None) };
    static TERMINATION_OBSERVER: RefCell<Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>> = const { RefCell::new(None) };
    static DRAG_OBSERVATIONS: RefCell<Vec<Result<DragObservation, String>>> = const { RefCell::new(Vec::new()) };
}

static ORIGINAL_BEGIN_DRAG: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static CANCEL_OBSERVED_DRAG: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug)]
pub(super) struct DragObservation {
    pub files: Vec<PathBuf>,
    pub image_sizes: Vec<Option<(u32, u32)>>,
    pub retained_mouse_down: bool,
    pub copy_only: bool,
}

unsafe extern "C" {
    fn class_getInstanceMethod(class: *const AnyClass, selector: Sel) -> *mut c_void;
    fn method_setImplementation(method: *mut c_void, implementation: *mut c_void) -> *mut c_void;
    fn CGEventPostToPid(pid: i32, event: *mut c_void);
}

fn drag_method() -> Result<*mut c_void, String> {
    let class = AnyClass::get(c"NSView").ok_or("NSView is unavailable")?;
    let selector = Sel::register(c"beginDraggingSessionWithItems:event:source:");
    // SAFETY: the class and selector are registered Objective-C runtime objects.
    let method = unsafe { class_getInstanceMethod(class, selector) };
    if method.is_null() { Err("NSView does not expose beginDraggingSession".into()) } else { Ok(method) }
}

pub(super) fn observe_native_file_drag() -> Result<(), String> {
    if !ORIGINAL_BEGIN_DRAG.load(Ordering::Acquire).is_null() {
        return Err("the native drag observer is already installed".into());
    }
    let method = drag_method()?;
    // SAFETY: this test process forwards the same method signature to the original.
    let original = unsafe { method_setImplementation(method, observe_begin_drag as *mut c_void) };
    if original.is_null() {
        return Err("could not install the native drag observer".into());
    }
    ORIGINAL_BEGIN_DRAG.store(original, Ordering::Release);
    DRAG_OBSERVATIONS.with(|observations| observations.borrow_mut().clear());
    Ok(())
}

pub(super) fn stop_observing_native_file_drag() -> Result<(), String> {
    let original = ORIGINAL_BEGIN_DRAG.swap(std::ptr::null_mut(), Ordering::AcqRel);
    if !original.is_null() {
        let method = drag_method()?;
        // SAFETY: restore the implementation saved by observe_native_file_drag.
        unsafe { method_setImplementation(method, original); }
    }
    Ok(())
}

pub(super) fn native_file_drags() -> Vec<Result<DragObservation, String>> {
    DRAG_OBSERVATIONS.with(|observations| observations.borrow().clone())
}

pub(super) fn cancel_next_native_file_drag() {
    CANCEL_OBSERVED_DRAG.store(true, Ordering::Release);
}

unsafe extern "C" fn observe_begin_drag(
    view: *mut AnyObject,
    selector: Sel,
    items: *mut AnyObject,
    event: *mut AnyObject,
    source: *mut AnyObject,
) -> *mut AnyObject {
    let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: AppKit supplies live objects for this method invocation.
        let observation = unsafe { inspect_native_drag(items, event, source) }?;
        if CANCEL_OBSERVED_DRAG.swap(false, Ordering::AcqRel) {
            // SAFETY: the live event belongs to the fixture window.
            let number: isize = unsafe { objc2::msg_send![event, windowNumber] };
            post_drag_escape(number)?;
        }
        Ok(observation)
    })).unwrap_or_else(|_| Err("the native drag observer panicked".into()));
    DRAG_OBSERVATIONS.with(|observations| observations.borrow_mut().push(observed));
    let original = ORIGINAL_BEGIN_DRAG.load(Ordering::Acquire);
    // SAFETY: the original implementation has the exact signature above.
    let original: unsafe extern "C" fn(*mut AnyObject, Sel, *mut AnyObject, *mut AnyObject, *mut AnyObject) -> *mut AnyObject =
        unsafe { std::mem::transmute(original) };
    unsafe { original(view, selector, items, event, source) }
}

unsafe fn inspect_native_drag(items: *mut AnyObject, event: *mut AnyObject, source: *mut AnyObject) -> Result<DragObservation, String> {
    if items.is_null() || event.is_null() || source.is_null() {
        return Err("AppKit began a drag with missing objects".into());
    }
    // SAFETY: these are AppKit's live dragging items, event, and source.
    unsafe {
        let count: usize = objc2::msg_send![items, count];
        let event_type: usize = objc2::msg_send![event, type];
        let external: usize = objc2::msg_send![source, draggingSession: std::ptr::null_mut::<AnyObject>(), sourceOperationMaskForDraggingContext: 0usize];
        let local: usize = objc2::msg_send![source, draggingSession: std::ptr::null_mut::<AnyObject>(), sourceOperationMaskForDraggingContext: 1usize];
        let writers: *mut AnyObject = objc2::msg_send![objc2::class!(NSMutableArray), arrayWithCapacity: count];
        if writers.is_null() {
            return Err("could not create the drag receiver's writer list".into());
        }
        for index in 0..count {
            let item: *mut AnyObject = objc2::msg_send![items, objectAtIndex: index];
            let writer: *mut AnyObject = objc2::msg_send![item, item];
            if writer.is_null() {
                return Err("a dragging item has no pasteboard writer".into());
            }
            let _: () = objc2::msg_send![writers, addObject: writer];
        }
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();
        let written: bool = objc2::msg_send![&*pasteboard, writeObjects: writers];
        let classes: *mut AnyObject = objc2::msg_send![objc2::class!(NSArray), arrayWithObject: objc2::class!(NSURL)];
        let received: *mut AnyObject = objc2::msg_send![&*pasteboard, readObjectsForClasses: classes, options: std::ptr::null_mut::<AnyObject>()];
        let mut files = Vec::new();
        if written && !received.is_null() {
            let received_count: usize = objc2::msg_send![received, count];
            for index in 0..received_count {
                let url: *mut AnyObject = objc2::msg_send![received, objectAtIndex: index];
                let is_file: bool = objc2::msg_send![url, isFileURL];
                let path: *mut NSString = objc2::msg_send![url, path];
                if !is_file || path.is_null() {
                    let _: () = objc2::msg_send![&*pasteboard, releaseGlobally];
                    return Err("the drag receiver did not get a local file URL".into());
                }
                files.push(PathBuf::from((&*path).to_string()));
            }
        }
        let _: () = objc2::msg_send![&*pasteboard, releaseGlobally];
        if !written || files.len() != count {
            return Err("the isolated drag receiver could not read every file URL".into());
        }
        let image_sizes = files.iter().map(|path| image::image_dimensions(path).ok()).collect();
        Ok(DragObservation { files, image_sizes, retained_mouse_down: event_type == 1, copy_only: external == 1 && local == 1 })
    }
}

pub(super) fn watch_application_termination() -> Result<(), String> {
    if TERMINATION_OBSERVER.with(|slot| slot.borrow().is_some()) {
        return Err("the application termination observer is already installed".into());
    }
    let callback = RcBlock::new(|_: NonNull<NSNotification>| super::native_will_terminate());
    // SAFETY: AppKit delivers this notification on the main thread.
    let observer = unsafe {
        NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
            Some(NSApplicationWillTerminateNotification), None, None, &callback,
        )
    };
    TERMINATION_OBSERVER.with(|slot| *slot.borrow_mut() = Some(observer));
    Ok(())
}

pub(super) fn request_application_termination() -> Result<(), String> {
    let marker = MainThreadMarker::new().ok_or("AppKit termination must run on the main thread")?;
    if std::env::var_os("JIFFSHOT_UITEST_TERMINATION_OS_REASON").is_none() {
        NSApplication::sharedApplication(marker).terminate(None);
        return Ok(());
    }
    let target = NSAppleEventDescriptor::descriptorWithProcessIdentifier(std::process::id() as i32);
    let reason = NSAppleEventDescriptor::descriptorWithEnumCode(u32::from_be_bytes(*b"logo"));
    // SAFETY: this test event targets only this process, uses the documented
    // AppleEvent argument types, and does not wait for a reply.
    unsafe {
        let event: Retained<NSAppleEventDescriptor> = objc2::msg_send![
            NSAppleEventDescriptor::class(),
            appleEventWithEventClass: u32::from_be_bytes(*b"aevt"),
            eventID: u32::from_be_bytes(*b"quit"),
            targetDescriptor: &*target,
            returnID: -1i16,
            transactionID: 0i32
        ];
        let _: () = objc2::msg_send![&*event, setParamDescriptor: &*reason, forKeyword: u32::from_be_bytes(*b"why?")];
        let mut error: *mut NSError = std::ptr::null_mut();
        let _: *mut NSAppleEventDescriptor = objc2::msg_send![
            &*event, sendEventWithOptions: NSAppleEventSendOptions::NoReply,
            timeout: 1.0f64, error: &mut error
        ];
        if !error.is_null() {
            return Err(format!("could not send the self-directed logout-reason event: {:?}", &*error));
        }
    }
    Ok(())
}

pub(super) fn start_recording_shutdown_fixture() -> Result<Rect, String> {
    let (marker, _) = app().ok_or("the recording fixture must run on the main thread")?;
    let layout = screen::DisplayLayout::current().map_err(|error| error.to_string())?;
    let work = layout.monitors().iter().find(|monitor| monitor.primary).ok_or("the recording fixture has no primary display")?.work;
    if work.width < 520 || work.height < 360 {
        return Err("the display is too small for the recording fixture".into());
    }
    let area = Rect::new(work.x + (work.width - 420) / 2, work.y + (work.height - 260) / 2, 420, 260);
    let points = screen::rect_to_points(area);
    let frame = NSRect::new(
        NSPoint::new(points.origin.x, screen::main_height_points() - points.origin.y - points.size.height),
        NSSize::new(points.size.width, points.size.height),
    );
    // SAFETY: this fixture is created, owned and closed on the AppKit main thread.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(marker), frame, NSWindowStyleMask::Borderless, NSBackingStoreType::Buffered, false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setOpaque(true);
    window.setBackgroundColor(Some(&NSColor::redColor()));
    window.setLevel(NSStatusWindowLevel);
    window.orderFrontRegardless();
    RECORDING_SHUTDOWN_FIXTURE.with(|slot| *slot.borrow_mut() = Some(window));
    Ok(Rect::new(area.x + 20, area.y + 20, area.width - 40, area.height - 40))
}

pub(super) fn create_control_video_fixture(path: &std::path::Path) -> Result<(), String> {
    use jiffshot_capture::media::{AudioSpec, VideoSpec, Writer, HNS_PER_SECOND};
    let audio = AudioSpec { sample_rate: 48_000, channels: 2 };
    let mut writer = Writer::create(path, VideoSpec::for_size(64, 48, 12), Some(audio))?;
    for frame in 0..12 {
        let at = frame * HNS_PER_SECOND / 12;
        let end = (frame + 1) * HNS_PER_SECOND / 12;
        let color = if frame < 6 { [0, 0, 240, 0] } else { [0, 240, 0, 0] };
        writer.write_frame(&color.repeat(64 * 48), at, end - at)?;
        let amplitude = if frame < 6 { 6000 } else { -6000 };
        writer.write_audio(&vec![amplitude; 48_000 / 12 * 2], at, end - at)?;
    }
    writer.finish()
}

pub(super) fn start_control_capture_fixture() -> Result<(Rect, u32), String> {
    let area = start_recording_shutdown_fixture()?;
    let marker = MainThreadMarker::new().ok_or("the control capture fixture must run on the main thread")?;
    RECORDING_SHUTDOWN_FIXTURE.with(|slot| {
        let fixture = slot.borrow();
        let window = fixture.as_ref().ok_or("the control capture window is unavailable")?;
        window.setTitle(&NSString::from_str("Jiffshot Control Fixture"));
        window.orderFrontRegardless();
        let view = window.contentView().ok_or("the control capture window has no view")?;
        let label = NSTextField::labelWithString(&NSString::from_str("Jiffshot Fixture"), marker);
        let width = view.frame().size.width;
        label.setFrame(NSRect::new(NSPoint::new(12.0, 15.0), NSSize::new(width - 24.0, 30.0)));
        label.setFont(Some(&NSFont::boldSystemFontOfSize(18.0)));
        label.setTextColor(Some(&NSColor::whiteColor()));
        view.addSubview(&label);
        for index in 0..64 {
            let node = NSTextField::labelWithString(&NSString::from_str(&format!("Fixture node {index}")), marker);
            node.setFrame(NSRect::new(NSPoint::new(1.0, 1.0), NSSize::new(1.0, 1.0)));
            view.addSubview(&node);
        }
        window.display();
        let id = u32::try_from(window.windowNumber()).map_err(|_| "the control capture window has no number")?;
        Ok((area, id))
    })
}

pub(super) fn start_control_focus_fixture() -> Result<u32, String> {
    let (marker, app) = app().ok_or("the control focus fixture must run on the main thread")?;
    let layout = screen::DisplayLayout::current().map_err(|error| error.to_string())?;
    let work = layout.monitors().iter().find(|monitor| monitor.primary).ok_or("the control fixture has no primary display")?.work;
    let area = Rect::new(work.x + 80, work.y + 120, 300, 180);
    let points = screen::rect_to_points(area);
    let content = NSRect::new(
        NSPoint::new(points.origin.x, screen::main_height_points() - points.origin.y - points.size.height),
        NSSize::new(points.size.width, points.size.height),
    );
    // SAFETY: both fixture windows are created and retained on the AppKit main thread.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(marker), content, NSWindowStyleMask::Titled, NSBackingStoreType::Buffered, false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("Jiffshot Focus Fixture"));
    window.setOpaque(true);
    window.setBackgroundColor(Some(&NSColor::blueColor()));
    let frame = window.frame();
    let backing_frame = NSRect::new(
        NSPoint::new(frame.origin.x - 12.0, frame.origin.y - 12.0),
        NSSize::new(frame.size.width + 24.0, frame.size.height + 24.0),
    );
    let backing = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(marker), backing_frame, NSWindowStyleMask::Borderless, NSBackingStoreType::Buffered, false,
        )
    };
    unsafe { backing.setReleasedWhenClosed(false) };
    backing.setOpaque(true);
    backing.setBackgroundColor(Some(&NSColor::blueColor()));
    backing.orderFrontRegardless();
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    window.makeKeyAndOrderFront(None);
    window.display();
    let id = u32::try_from(window.windowNumber()).map_err(|_| "the control focus window has no number")?;
    CONTROL_FOCUS_FIXTURE.with(|slot| *slot.borrow_mut() = vec![backing, window]);
    Ok(id)
}

pub(super) fn close_recording_shutdown_fixture() {
    RECORDING_SHUTDOWN_FIXTURE.with(|slot| {
        if let Some(window) = slot.borrow_mut().take() {
            window.close();
        }
    });
}

pub(super) enum ProbeCommand {
    Raw,
    Refresh,
    StartSecond,
    RefreshSecond,
    DualFlow,
    Flow,
    Filtered,
    Still,
    Frozen,
    Fresh,
    WatchClose(u32),
    WatchPopupClose(u32, Vec<usize>),
    StopWatching,
    Stop,
}

#[derive(Debug)]
pub(super) enum ProbeReply {
    Ready,
    Refreshed,
    SecondReady,
    DualFrames { first: Vec<Vec<u8>>, second: Vec<Vec<u8>>, raw: Vec<u8> },
    Pixel([u8; 3]),
    PopupFrames { frames: Vec<Vec<u8>>, visible_frames: Vec<Vec<u8>>, raw: Vec<u8>, still: Vec<u8>, frozen: Vec<u8> },
    Fresh,
    Watching,
    CloseFrames { frames: u32, leaked: u32, first: Option<[u8; 3]>, registered: u32 },
}

struct ExclusionProbe {
    background: Retained<NSWindow>,
    commands: Sender<ProbeCommand>,
    replies: Receiver<Result<ProbeReply, String>>,
    cached: Option<Result<ProbeReply, String>>,
    worker: JoinHandle<()>,
}

fn probe_pixel(pixels: &[u8]) -> Result<[u8; 3], String> {
    let at = (32 * 210 + 105) * 4;
    let pixel = pixels.get(at..at + 3).ok_or("the exclusion probe frame is too small")?;
    Ok([pixel[0], pixel[1], pixel[2]])
}

fn raw_frame(layout: &screen::DisplayLayout, area: Rect, width: usize, height: usize) -> Result<Vec<u8>, String> {
    layout.validate().map_err(|error| error.to_string())?;
    let monitor = layout.monitor_at(area.center().0, area.center().1).ok_or("the exclusion probe has no display")?;
    let id: u32 = monitor.device.strip_prefix("display-").ok_or("the exclusion probe display has no ID")?
        .parse().map_err(|_| "the exclusion probe display ID is invalid")?;
    let content = sck::shareable_content().map_err(|error| error.to_string())?;
    // SAFETY: the retained shareable-content snapshot is live.
    let display = unsafe { content.displays() }.iter().find(|display| unsafe { display.displayID() } == id)
        .ok_or("the exclusion probe display is unavailable")?;
    let points = screen::rect_to_points(area);
    let display_points = screen::rect_to_points(monitor.rect);
    let source = CGRect::new(
        objc2_core_foundation::CGPoint::new(points.origin.x - display_points.origin.x, points.origin.y - display_points.origin.y),
        points.size,
    );
    let filter = sck::display_filter(&content, &display, &[]);
    let config = sck::config(source, width, height, false);
    sck::screenshot(&filter, &config, width, height).map_err(|error| error.to_string())
}

fn raw_probe_frame(layout: &screen::DisplayLayout, area: Rect) -> Result<Vec<u8>, String> {
    raw_frame(layout, area, 210, 130)
}

pub(super) fn raw_desktop_pixels(area: Rect) -> Result<Vec<u8>, String> {
    if area.is_empty() {
        return Err("the desktop probe area is empty".into());
    }
    let layout = screen::DisplayLayout::current().map_err(|error| error.to_string())?;
    raw_frame(&layout, area, area.width as usize, area.height as usize)
}

fn watch_close(
    grabber: &mut screen::Grabber,
    requests: &Receiver<ProbeCommand>,
    responses: &Sender<Result<ProbeReply, String>>,
    number: u32,
    samples: &[usize],
) -> Result<ProbeReply, String> {
    if samples.is_empty() || samples.iter().any(|offset| offset.checked_add(3).is_none_or(|end| end > 210 * 130 * 4)) {
        return Err("the close probe has invalid samples".into());
    }
    responses.send(Ok(ProbeReply::Watching)).map_err(|error| error.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut frames = 0;
    let mut leaked = 0;
    let mut first = None;
    let mut registered = 0;
    loop {
        match requests.try_recv() {
            Ok(ProbeCommand::StopWatching) => break,
            Ok(_) => return Err("unexpected command during the close probe".into()),
            Err(mpsc::TryRecvError::Disconnected) => return Err("the close probe lost its command channel".into()),
            Err(mpsc::TryRecvError::Empty) if Instant::now() >= deadline => return Err("the close probe timed out".into()),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        let pixels = grabber.grab().map_err(|error| error.to_string())?;
        if pixels.len() != 210 * 130 * 4 {
            return Err("the close probe frame has an invalid size".into());
        }
        frames += 1;
        if let Some(pixel) = samples.iter().find_map(|&offset| {
            let pixel = &pixels[offset..offset + 3];
            (!(pixel[1] > 160 && u16::from(pixel[1]) > u16::from(pixel[0].max(pixel[2])) + 100))
                .then_some([pixel[0], pixel[1], pixel[2]])
        }) {
            leaked += 1;
            first.get_or_insert(pixel);
            if screen::is_excluded_window(number) {
                registered += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(ProbeReply::CloseFrames { frames, leaked, first, registered })
}

pub(super) fn start_exclusion_probe() -> Result<Rect, String> {
    let (marker, _) = app().ok_or("the exclusion probe must run on the main thread")?;
    let layout = screen::DisplayLayout::current().map_err(|error| error.to_string())?;
    let work = layout.monitors().iter().find(|monitor| monitor.primary).ok_or("the exclusion probe has no primary display")?.work;
    if work.width < 520 || work.height < 360 {
        return Err("the display is too small for the exclusion probe".into());
    }
    let area = Rect::new(work.x + (work.width - 420) / 2, work.y + (work.height - 260) / 2, 420, 260);
    let points = screen::rect_to_points(area);
    let frame = NSRect::new(
        NSPoint::new(points.origin.x, screen::main_height_points() - points.origin.y - points.size.height),
        NSSize::new(points.size.width, points.size.height),
    );
    // SAFETY: this fixture is created, owned and closed on the AppKit main thread.
    let background = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(marker), frame, NSWindowStyleMask::Borderless, NSBackingStoreType::Buffered, false,
        )
    };
    unsafe { background.setReleasedWhenClosed(false) };
    background.setOpaque(true);
    background.setBackgroundColor(Some(&NSColor::greenColor()));
    background.setLevel(NSStatusWindowLevel);
    background.orderFrontRegardless();
    let (commands, requests) = mpsc::channel();
    let (responses, replies) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let run = || -> Result<(), String> {
            let mapping = CaptureMapping::new(area, 210, 130).ok_or("invalid exclusion probe mapping")?;
            let mut grabber = screen::Grabber::for_mapping(mapping, &layout).map_err(|error| error.to_string())?;
            let mut second: Option<screen::Grabber> = None;
            grabber.grab().map_err(|error| error.to_string())?;
            responses.send(Ok(ProbeReply::Ready)).map_err(|error| error.to_string())?;
            while let Ok(command) = requests.recv() {
                let result = match command {
                    ProbeCommand::Raw => raw_probe_frame(&layout, area).and_then(|pixels| probe_pixel(&pixels)).map(ProbeReply::Pixel),
                    ProbeCommand::Refresh => grabber.refresh_exclusions().map(|()| ProbeReply::Refreshed).map_err(|error| error.to_string()),
                    ProbeCommand::StartSecond => {
                        if second.is_some() {
                            Err("the second recording probe is already running".into())
                        } else {
                            let mut next = screen::Grabber::for_mapping(mapping, &layout).map_err(|error| error.to_string())?;
                            next.grab().map_err(|error| error.to_string())?;
                            second = Some(next);
                            Ok(ProbeReply::SecondReady)
                        }
                    }
                    ProbeCommand::RefreshSecond => second.as_mut().ok_or("the second recording probe is unavailable".to_owned())?
                        .refresh_exclusions().map(|()| ProbeReply::Refreshed).map_err(|error| error.to_string()),
                    ProbeCommand::DualFlow => {
                        let second = second.as_mut().ok_or("the second recording probe is unavailable")?;
                        let deadline = Instant::now() + Duration::from_secs(1);
                        let mut first_frames = Vec::new();
                        let mut second_frames = Vec::new();
                        while Instant::now() < deadline {
                            first_frames.push(grabber.grab().map_err(|error| error.to_string())?.to_vec());
                            second_frames.push(second.grab().map_err(|error| error.to_string())?.to_vec());
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        let raw = raw_probe_frame(&layout, area)?;
                        Ok(ProbeReply::DualFrames { first: first_frames, second: second_frames, raw })
                    }
                    ProbeCommand::Flow => {
                        let deadline = Instant::now() + Duration::from_secs(1);
                        let mut frames = Vec::new();
                        while Instant::now() < deadline {
                            frames.push(grabber.grab().map_err(|error| error.to_string())?.to_vec());
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        let raw = raw_probe_frame(&layout, area)?;
                        let still = layout.grab_mapped(mapping).map_err(|error| error.to_string())?.pixels;
                        let frozen = layout.freeze().and_then(|desktop| desktop.render(mapping)).map_err(|error| error.to_string())?.pixels;
                        let deadline = Instant::now() + Duration::from_millis(400);
                        let mut visible_frames = Vec::new();
                        while Instant::now() < deadline {
                            visible_frames.push(grabber.grab().map_err(|error| error.to_string())?.to_vec());
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        Ok(ProbeReply::PopupFrames { frames, visible_frames, raw, still, frozen })
                    }
                    ProbeCommand::Filtered => grabber.grab().map_err(|error| error.to_string())
                        .and_then(probe_pixel).map(ProbeReply::Pixel),
                    ProbeCommand::Still => layout.grab_mapped(mapping).map_err(|error| error.to_string())
                        .and_then(|frame| probe_pixel(&frame.pixels)).map(ProbeReply::Pixel),
                    ProbeCommand::Frozen => layout.freeze().and_then(|frozen| frozen.render(mapping)).map_err(|error| error.to_string())
                        .and_then(|frame| probe_pixel(&frame.pixels)).map(ProbeReply::Pixel),
                    ProbeCommand::Fresh => {
                        let mut fresh = screen::Grabber::for_mapping(mapping, &layout).map_err(|error| error.to_string())?;
                        fresh.grab().map_err(|error| error.to_string())?;
                        Ok(ProbeReply::Fresh)
                    }
                    ProbeCommand::WatchClose(number) =>
                        watch_close(&mut grabber, &requests, &responses, number, &[(32 * 210 + 105) * 4]),
                    ProbeCommand::WatchPopupClose(number, samples) =>
                        watch_close(&mut grabber, &requests, &responses, number, &samples),
                    ProbeCommand::StopWatching => Err("the close probe is not running".into()),
                    ProbeCommand::Stop => break,
                };
                responses.send(result).map_err(|error| error.to_string())?;
            }
            Ok(())
        };
        if let Err(error) = run() {
            let _ = responses.send(Err(error));
        }
    });
    EXCLUSION_PROBE.with(|probe| {
        *probe.borrow_mut() = Some(ExclusionProbe { background, commands, replies, cached: None, worker });
    });
    Ok(area)
}

pub(super) fn exclusion_command(command: ProbeCommand) -> Result<(), String> {
    EXCLUSION_PROBE.with(|probe| {
        probe.borrow().as_ref().ok_or("the exclusion probe is unavailable")?.commands.send(command).map_err(|error| error.to_string())
    })
}

pub(super) fn exclusion_reply() -> Result<ProbeReply, String> {
    EXCLUSION_PROBE.with(|probe| {
        let mut probe = probe.borrow_mut();
        let probe = probe.as_mut().ok_or("the exclusion probe is unavailable")?;
        if let Some(reply) = probe.cached.take() {
            return reply;
        }
        probe.replies.try_recv().map_err(|error| format!("the exclusion probe did not answer: {error}"))?
    })
}

pub(super) fn poll_exclusion_reply() -> Result<bool, String> {
    EXCLUSION_PROBE.with(|probe| {
        let mut probe = probe.borrow_mut();
        let probe = probe.as_mut().ok_or("the exclusion probe is unavailable")?;
        if probe.cached.is_some() {
            return Ok(true);
        }
        match probe.replies.try_recv() {
            Ok(reply) => {
                probe.cached = Some(reply);
                Ok(true)
            }
            Err(mpsc::TryRecvError::Empty) => Ok(false),
            Err(mpsc::TryRecvError::Disconnected) => Err("the exclusion probe disconnected".into()),
        }
    })
}

pub(super) fn exclusion_window_alpha(number: isize) -> Option<f64> {
    window(number).map(|window| window.alphaValue())
}

pub(super) fn exclusion_window_ignores_input(number: isize) -> Option<bool> {
    window(number).map(|window| window.ignoresMouseEvents())
}

pub(super) fn exclusion_window_visible(number: u32) -> Option<bool> {
    window(number as isize).map(|window| window.isVisible())
}

pub(super) fn place_for_exclusion_probe(number: isize, x: i32, y: i32) -> Result<(), String> {
    let window = window(number).ok_or("the test window is unavailable")?;
    let (x, y) = screen::to_points(x as f64, y as f64);
    window.setFrameTopLeftPoint(NSPoint::new(x, screen::main_height_points() - y));
    Ok(())
}

pub(super) fn hide_exclusion_background() -> Result<(), String> {
    EXCLUSION_PROBE.with(|probe| {
        let probe = probe.borrow();
        let probe = probe.as_ref().ok_or("the exclusion probe is unavailable")?;
        probe.background.orderOut(None);
        Ok(())
    })
}

pub(super) fn front_for_exclusion_probe(number: isize) -> Result<(), String> {
    let window = window(number).ok_or("the test window is unavailable")?;
    window.setLevel(NSStatusWindowLevel + 1);
    window.orderFrontRegardless();
    Ok(())
}

pub(super) fn window_at(x: i32, y: i32) -> Option<isize> {
    let (marker, _) = app()?;
    let (x, y) = screen::to_points(x as f64, y as f64);
    let point = NSPoint::new(x, screen::main_height_points() - y);
    Some(NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(point, 0, marker))
}

pub(super) fn describe_window(number: isize) -> String {
    match window(number) {
        Some(window) => format!(
            "{number}: {:?}, {:?}, alpha {}, excluded {}, parent {:?}, level {}, class {:?}",
            window.title().to_string(), window_rect(number), window.alphaValue(), screen::is_excluded_window(number as u32),
            window.parentWindow().map(|parent| parent.windowNumber()), window.level(), window.class().name(),
        ),
        None => format!("{number}: unavailable"),
    }
}

pub(super) fn stall_window_close(number: isize) -> Result<(), String> {
    if window(number).is_none() {
        return Err("the close probe window is unavailable".into());
    }
    let called = Arc::new(AtomicBool::new(false));
    let seen = called.clone();
    let callback = RcBlock::new(move |notification: NonNull<NSNotification>| {
        // SAFETY: Foundation invokes the copied block with a live notification.
        let object = unsafe { notification.as_ref() }.object();
        if object.as_deref().and_then(AnyObject::downcast_ref::<NSWindow>).is_some_and(|window| window.windowNumber() == number) {
            seen.store(true, Ordering::Release);
            std::thread::sleep(Duration::from_millis(750));
        }
    });
    // SAFETY: the observer is called on the AppKit thread and owns its block.
    let observer = unsafe {
        NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
            Some(NSWindowWillCloseNotification), None, None, &callback,
        )
    };
    CLOSE_STALL.with(|slot| *slot.borrow_mut() = Some((observer, called)));
    Ok(())
}

pub(super) fn finish_window_close_stall() -> Result<bool, String> {
    let (observer, called) = CLOSE_STALL.with(|slot| slot.borrow_mut().take()).ok_or("the close probe observer is unavailable")?;
    // SAFETY: this is the observer returned by the notification center.
    unsafe { NSNotificationCenter::defaultCenter().removeObserver((&*observer).as_ref()) };
    Ok(called.load(Ordering::Acquire))
}

pub(super) fn finish_exclusion_probe() -> Result<(), String> {
    EXCLUSION_PROBE.with(|probe| {
        let probe = probe.borrow_mut();
        let probe = probe.as_ref().ok_or("the exclusion probe is unavailable")?;
        probe.commands.send(ProbeCommand::Stop).map_err(|error| error.to_string())?;
        probe.background.close();
        Ok(())
    })
}

pub(super) fn join_exclusion_probe() -> Result<(), String> {
    EXCLUSION_PROBE.with(|probe| {
        let mut slot = probe.borrow_mut();
        if !slot.as_ref().ok_or("the exclusion probe is unavailable")?.worker.is_finished() {
            return Err("the exclusion probe did not stop".into());
        }
        slot.take().expect("checked above").worker.join().map_err(|_| "the exclusion probe panicked".into())
    })
}

fn app() -> Option<(MainThreadMarker, Retained<NSApplication>)> {
    let marker = MainThreadMarker::new()?;
    Some((marker, NSApplication::sharedApplication(marker)))
}

pub(super) fn main_window() -> Option<isize> {
    let (_, app) = app()?;
    app.mainWindow().or_else(|| app.keyWindow()).map(|window| window.windowNumber())
}

pub(super) fn process_windows() -> Vec<isize> {
    let Some((_, app)) = app() else { return Vec::new() };
    app.windows()
        .iter()
        .filter(|window| window.isVisible())
        .map(|window| window.windowNumber())
        .collect()
}

fn window(number: isize) -> Option<Retained<NSWindow>> {
    app()?.1.windowWithWindowNumber(number)
}

pub(super) fn window_lifetime() -> Result<u32, String> {
    let (marker, _) = app().ok_or("window lifetime test must run on the main thread")?;
    let frame = NSRect::new(NSPoint::new(-5000.0, -5000.0), NSSize::new(32.0, 32.0));
    // SAFETY: the fixture is created, used, and closed on the AppKit main thread.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(marker),
            frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: this retained fixture remains owned through the close call.
    unsafe { window.setReleasedWhenClosed(false) };
    let handle = (&*window as *const NSWindow) as isize;
    let number = u32::try_from(window.windowNumber()).map_err(|_| "fixture has no WindowServer number")?;
    if number == 0 {
        return Err("fixture has no WindowServer number".into());
    }
    let (tracked, first) = crate::mac_window::protect(handle)?;
    if tracked != number {
        return Err("the fixture's native window number changed".into());
    }
    let other = screen::register_excluded_window(number);
    LIFETIME_OTHER_OWNER.with(|slot| *slot.borrow_mut() = Some((number, first, other)));
    super::super::hud::native::cloak(handle, true);
    let was_cloaked = super::super::hud::native::is_cloaked(handle) && window.alphaValue() == 0.0;
    window.close();
    if !was_cloaked {
        return Err("fixture window could not be cloaked".into());
    }
    if crate::mac_window::protection(handle).is_some() {
        return Err("the native close notification did not remove the protected window".into());
    }
    if !screen::is_excluded_window(number) || super::super::hud::native::is_cloaked(handle) {
        return Err("the fixture did not retain its exclusion through native closure or clear its cloak".into());
    }
    Ok(number)
}

pub(super) fn finish_window_lifetime(number: u32) -> Result<(), String> {
    let (tracked, first, other) = LIFETIME_OTHER_OWNER.with(|slot| slot.borrow_mut().take())
        .ok_or("the fixture's other exclusion owner is unavailable")?;
    if tracked != number || !screen::is_exclusion_registered(other) || !screen::is_excluded_window(number) {
        return Err("closing the fixture retired another owner's exclusion".into());
    }
    if !screen::retire_exclusion(other) {
        return Err("the fixture's second exclusion owner could not be retired".into());
    }
    if screen::is_exclusion_registered(first) {
        return Err("the native close notification did not retire the first exclusion owner".into());
    }
    if screen::is_excluded_window(number) {
        return Err("the fixture's exclusion is still retained by a capture or another owner".into());
    }
    Ok(())
}

pub(super) fn window_rect(number: isize) -> Rect {
    let Some(window) = window(number) else { return Rect::default() };
    let mut bounds = window.frame();
    bounds.origin.y = screen::main_height_points() - bounds.origin.y - bounds.size.height;
    screen::rect_from_points(bounds)
}

pub(super) fn window_bounds(number: isize) -> Result<[f64; 4], String> {
    let window = window(number).ok_or("the test window is unavailable")?;
    let frame = window.frame();
    let scale = screen::scale_factor();
    let top = screen::main_height_points() - frame.origin.y - frame.size.height;
    Ok([
        frame.origin.x * scale,
        top * scale,
        (frame.origin.x + frame.size.width) * scale,
        (top + frame.size.height) * scale,
    ])
}

pub(super) fn window_hits(number: isize, x: f64, y: f64) -> bool {
    let Some((marker, _)) = app() else { return false };
    let scale = screen::scale_factor();
    let point = NSPoint::new(x / scale, screen::main_height_points() - y / scale);
    NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(point, 0, marker) == number
}

pub(super) fn affinity(number: isize) -> u32 {
    if screen::is_excluded_window(number as u32) { 0x11 } else { 0 }
}

pub(super) fn probe(x: i32, y: i32, number: isize) -> String {
    let Some((marker, _)) = app() else { return "not on main thread".into() };
    let (point_x, point_y) = screen::to_points(x as f64, y as f64);
    let point = NSPoint::new(point_x, screen::main_height_points() - point_y);
    let hit = NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(point, 0, marker) == number;
    let frame = screen::DisplayLayout::current().map_err(|error| error.to_string())
        .and_then(|layout| raw_frame(&layout, Rect::new(x, y, 1, 1), 1, 1));
    match frame {
        Ok(pixels) => format!(
            "#{:02X}{:02X}{:02X}, hits: {}",
            pixels[2], pixels[1], pixels[0],
            if hit { "yes" } else { "no" }
        ),
        Err(error) => format!("pixel unavailable ({error}), hits: {}", if hit { "yes" } else { "no" }),
    }
}

pub(super) fn close_window(number: isize) {
    if let Some(window) = window(number) {
        window.performClose(None);
    }
}

pub(super) fn close_owned_window(number: isize) -> bool {
    let Some(window) = window(number) else { return false };
    window.close();
    !window.isVisible()
}

pub(super) fn drag_window(number: isize, from: (f64, f64), to: (f64, f64)) -> Result<(), String> {
    let window = window(number).ok_or("the test window is unavailable")?;
    let view = window.contentView().ok_or("the test window has no content view")?;
    let bounds = view.bounds();
    if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
        return Err("the test window has no usable content area".into());
    }
    for (kind, (x, y)) in [
        (NSEventType::LeftMouseDown, from),
        (NSEventType::LeftMouseDragged, to),
        (NSEventType::LeftMouseUp, to),
    ] {
        let location = NSPoint::new(bounds.origin.x + x * bounds.size.width, bounds.origin.y + (1.0 - y) * bounds.size.height);
        let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            kind, location, NSEventModifierFlags::empty(), 0.0, number, None, 1, 1, 1.0,
        ).ok_or("could not create a test mouse event")?;
        window.sendEvent(&event);
    }
    Ok(())
}

pub(super) fn post_drag_mouse(number: isize, kind: NSEventType, x: f64, y: f64) -> Result<(), String> {
    let (_, app) = app().ok_or("the drag fixture must run on the main thread")?;
    let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
        kind, NSPoint::new(x, y), NSEventModifierFlags::empty(), 1.0, number, None, 1, 1, 1.0,
    ).ok_or("could not create a drag fixture mouse event")?;
    app.postEvent_atStart(&event, true);
    let deadline = NSDate::dateWithTimeIntervalSinceNow(0.1);
    let mode = NSString::from_str("kCFRunLoopDefaultMode");
    let queued = app.nextEventMatchingMask_untilDate_inMode_dequeue(NSEventMask(1u64 << kind.0), Some(&deadline), &mode, true)
        .ok_or("the drag fixture mouse event was not queued")?;
    if queued.windowNumber() != number {
        app.postEvent_atStart(&queued, true);
        return Err("the queued mouse event belonged to another window".into());
    }
    app.sendEvent(&queued);
    Ok(())
}

pub(super) fn post_drag_escape(number: isize) -> Result<(), String> {
    let (_, app) = app().ok_or("the drag fixture must run on the main thread")?;
    let escape = NSString::from_str("\u{1b}");
    let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
        NSEventType::KeyDown, NSPoint::new(0.0, 0.0), NSEventModifierFlags::empty(), 2.0, number, None, &escape, &escape, false, 53,
    ).ok_or("could not create the drag fixture Escape event")?;
    app.postEvent_atStart(&event, false);
    Ok(())
}

pub(super) struct GlobalDrag {
    number: isize,
    original: objc2_core_foundation::CGPoint,
    current: objc2_core_foundation::CGPoint,
    guard: PathBuf,
    active: bool,
    held: bool,
}

impl GlobalDrag {
    pub(super) fn begin(number: isize) -> Result<Self, String> {
        use jiffshot_capture::macos::ffi::CGWarpMouseCursorPosition;

        if !jiffshot_capture::os::event_posting_allowed() {
            return Err("accessibility permission to post fixture input is unavailable".into());
        }
        if any_fixture_input() {
            return Err("release held mouse buttons and modifiers before the drop fixture".into());
        }
        let original = fixture_pointer()?;
        let current = fixture_drag_point(number, 50.0, 50.0)?;
        fixture_drag_point(number, 70.0, 50.0)?;
        let path = std::env::var_os("JIFFSHOT_UITEST_DRAG_ORIGINAL_POINTER_FILE")
            .ok_or("the fixture's original pointer path is unavailable")?;
        let guard = std::env::var_os("JIFFSHOT_UITEST_DRAG_ACTIVE_FILE")
            .map(PathBuf::from)
            .ok_or("the fixture's drag recovery path is unavailable")?;
        let data = serde_json::to_vec(&(original.x, original.y))
            .map_err(|error| format!("could not encode the fixture's original pointer: {error}"))?;
        std::fs::write(path, data)
            .map_err(|error| format!("could not record the fixture's original pointer: {error}"))?;
        let mut drag = Self { number, original, current: original, guard, active: true, held: false };
        drag.record_state(current, false)?;
        if !same_fixture_point(fixture_pointer()?, original) || any_fixture_input() {
            return Err("input changed while the drag fixture was starting".into());
        }
        // SAFETY: the target has just been verified to belong to the fixture window.
        let error = unsafe { CGWarpMouseCursorPosition(current) };
        if error != 0 {
            return Err(format!("could not position the fixture pointer: {error}"));
        }
        drag.current = current;
        drag.record_state(current, false)?;
        if any_fixture_input() {
            return Err("input became active before the fixture press".into());
        }
        drag.record_state(current, true)?;
        drag.held = true;
        post_fixture_mouse(1, current, false)?;
        Ok(drag)
    }

    fn record_state(&self, planned: objc2_core_foundation::CGPoint, armed: bool) -> Result<(), String> {
        let data = serde_json::to_vec(&(
            self.original.x, self.original.y, self.current.x, self.current.y, planned.x, planned.y, armed,
        ))
            .map_err(|error| format!("could not encode the fixture drag state: {error}"))?;
        let temporary = self.guard.with_extension("tmp");
        std::fs::write(&temporary, data)
            .and_then(|_| std::fs::rename(&temporary, &self.guard))
            .map_err(|error| format!("could not record the fixture drag state: {error}"))
    }

    fn clear_state(&self) -> Result<(), String> {
        match std::fs::remove_file(&self.guard) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("could not clear the fixture drag state: {error}")),
        }
    }

    pub(super) fn raise_global_drag_window(number: isize) -> Result<(), String> {
        let (_, application) = app().ok_or("the drag fixture must run on the main thread")?;
        let window = window(number).ok_or("the fixture drag window is unavailable")?;
        #[allow(deprecated)]
        application.activateIgnoringOtherApps(true);
        window.setLevel(NSStatusWindowLevel);
        window.makeKeyAndOrderFront(None);
        Ok(())
    }

    pub(super) fn move_to_threshold(&mut self) -> Result<(), String> {
        use jiffshot_capture::macos::ffi::CGWarpMouseCursorPosition;

        if !same_fixture_point(fixture_pointer()?, self.current) || other_fixture_input() {
            return Err("input changed while the drag fixture was starting".into());
        }
        let target = fixture_drag_point(self.number, 70.0, 50.0)?;
        self.record_state(target, true)?;
        if !same_fixture_point(fixture_pointer()?, self.current) || other_fixture_input() {
            return Err("input changed while the drag fixture was starting".into());
        }
        // SAFETY: the target has just been verified to belong to the fixture window.
        let error = unsafe { CGWarpMouseCursorPosition(target) };
        if error != 0 {
            return Err(format!("could not move the fixture pointer: {error}"));
        }
        self.current = target;
        self.record_state(target, true)?;
        post_fixture_mouse(6, target, false)
    }

    pub(super) fn finish(&mut self) -> Result<(), String> {
        use jiffshot_capture::macos::ffi::{CGEventSourceButtonState, CGWarpMouseCursorPosition, kCGEventSourceStateCombinedSessionState};

        if !self.active {
            return Ok(());
        }
        // SAFETY: this is a query of the current session's input state.
        if other_fixture_input()
            || (!self.held && unsafe { CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, 0) })
        {
            return Err("other input is active; refusing fixture cleanup".into());
        }
        let current = fixture_pointer()?;
        let mut restore = self.original;
        let mut interruption = None;
        if !same_fixture_point(current, self.current) {
            match receiver_fixture_point(self.current, current) {
                Ok(true) => {}
                Ok(false) => {
                    restore = current;
                    interruption = Some("the pointer moved during the drop fixture; preserving its newer position".to_owned());
                }
                Err(error) => {
                    restore = current;
                    interruption = Some(error);
                }
            }
        }
        // SAFETY: this is a query of the current session's input state.
        if self.held && unsafe { CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, 0) } {
            let latest = fixture_pointer()?;
            if !same_fixture_point(latest, current) {
                restore = latest;
                interruption = Some("the pointer moved during the drop fixture; preserving its newer position".to_owned());
            }
            let owner = if std::env::var_os("JIFFSHOT_UITEST_DRAG_FORCE_OWNER_FAILURE").is_some() {
                Err("injected fixture source-owner failure".into())
            } else {
                require_fixture_owner(self.number, self.current)
            };
            if let Err(error) = owner {
                post_fixture_mouse(2, self.current, true)?;
                return Err(format!("{error}; could not safely release the synthetic mouse button"));
            }
            post_fixture_mouse(2, self.current, false)?;
            for _ in 0..50 {
                // SAFETY: this is a query of the current session's input state.
                if !unsafe { CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, 0) } {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            // SAFETY: this is a query of the current session's input state.
            if unsafe { CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, 0) } {
                return Err("the synthetic fixture mouse button is still pressed".into());
            }
            self.held = false;
            self.active = false;
            self.clear_state()?;
            if !same_fixture_point(fixture_pointer()?, self.current) {
                return Err("the pointer moved during fixture cleanup; leaving it at the newer position".into());
            }
        } else {
            self.held = false;
            self.active = false;
            self.clear_state()?;
            if !same_fixture_point(fixture_pointer()?, current) {
                return Err("the pointer moved during fixture cleanup; leaving it at the newer position".into());
            }
        }
        if any_fixture_input() {
            return Err("other input became active; leaving the pointer at its newer position".into());
        }
        // SAFETY: the pointer still matches the fixture's last observed position.
        let error = unsafe { CGWarpMouseCursorPosition(restore) };
        if error != 0 {
            return Err(format!("could not restore the pointer after the drop fixture: {error}"));
        }
        interruption.map_or(Ok(()), Err)
    }
}

fn any_fixture_input() -> bool {
    use jiffshot_capture::macos::ffi::{CGEventSourceButtonState, kCGEventSourceStateCombinedSessionState};
    // SAFETY: this is a query of the current session's input state.
    (unsafe { CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, 0) }) || other_fixture_input()
}

fn other_fixture_input() -> bool {
    use jiffshot_capture::macos::ffi::{CGEventSourceButtonState, CGEventSourceFlagsState, kCGEventSourceStateCombinedSessionState};
    // SAFETY: these functions query the current session's input state.
    (1..32).any(|button| unsafe { CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, button) })
        || unsafe { CGEventSourceFlagsState(kCGEventSourceStateCombinedSessionState) } & (0xf << 17) != 0
}

impl Drop for GlobalDrag {
    fn drop(&mut self) {
        if self.active {
            if let Err(error) = self.finish() {
                eprintln!("ui self-test: {error}");
            }
        }
    }
}

fn fixture_pointer() -> Result<objc2_core_foundation::CGPoint, String> {
    use jiffshot_capture::macos::ffi::{CGEventCreate, CGEventGetLocation, Owned};
    // SAFETY: the new event is released by Owned after its location is read.
    let event = Owned(unsafe { CGEventCreate(std::ptr::null_mut()) }.cast_const());
    if event.0.is_null() {
        return Err("could not read the fixture pointer".into());
    }
    Ok(unsafe { CGEventGetLocation(event.0.cast_mut()) })
}

fn same_fixture_point(a: objc2_core_foundation::CGPoint, b: objc2_core_foundation::CGPoint) -> bool {
    (a.x - b.x).abs() < 1.0 && (a.y - b.y).abs() < 1.0
}

fn fixture_drag_point(number: isize, x: f64, y: f64) -> Result<objc2_core_foundation::CGPoint, String> {
    let window = window(number).ok_or("the fixture drag window is unavailable")?;
    // SAFETY: the window is live on the AppKit thread and the point is in its content.
    let point: NSPoint = unsafe { objc2::msg_send![&*window, convertPointToScreen: NSPoint::new(x, y)] };
    let point = objc2_core_foundation::CGPoint { x: point.x, y: screen::main_height_points() - point.y };
    require_fixture_owner(number, point)?;
    Ok(point)
}

fn require_fixture_owner(number: isize, point: objc2_core_foundation::CGPoint) -> Result<(), String> {
    let window = window(number).ok_or("the fixture drag window is unavailable")?;
    let marker = MainThreadMarker::new().ok_or("the drag fixture must run on the main thread")?;
    let screen_point = NSPoint::new(point.x, screen::main_height_points() - point.y);
    let owner = NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(screen_point, 0, marker);
    if owner != number {
        return Err(format!(
            "another window ({owner}) covers fixture window {number} at {screen_point:?}; frame {:?}, level {}, visible {}, ignores input {}",
            window.frame(), window.level(), window.isVisible(), window.ignoresMouseEvents(),
        ));
    }
    Ok(())
}

fn receiver_fixture_point(source: objc2_core_foundation::CGPoint, current: objc2_core_foundation::CGPoint) -> Result<bool, String> {
    let Some(path) = std::env::var_os("JIFFSHOT_UITEST_DRAG_POINTER_FILE") else { return Ok(false) };
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("could not read the drop fixture pointer state: {error}")),
    };
    let (start_x, start_y, target_x, target_y, moved): (f64, f64, f64, f64, bool) = serde_json::from_slice(&data)
        .map_err(|error| format!("invalid drop fixture pointer state: {error}"))?;
    let start = objc2_core_foundation::CGPoint { x: start_x, y: start_y };
    let target = objc2_core_foundation::CGPoint { x: target_x, y: target_y };
    Ok(moved && [start_x, start_y, target_x, target_y].iter().all(|value| value.is_finite())
        && same_fixture_point(start, source) && same_fixture_point(target, current))
}

fn post_fixture_mouse(kind: u32, point: objc2_core_foundation::CGPoint, target_self: bool) -> Result<(), String> {
    use jiffshot_capture::macos::ffi::{CGEventCreateMouseEvent, CGEventPost, CGEventSetIntegerValueField, Owned, kCGHIDEventTap};
    // SAFETY: the event is owned until it has been posted.
    let event = Owned(unsafe { CGEventCreateMouseEvent(std::ptr::null_mut(), kind, point, 0) }.cast_const());
    if event.0.is_null() {
        return Err("could not create fixture mouse input".into());
    }
    unsafe {
        CGEventSetIntegerValueField(event.0.cast_mut(), 1, 1);
        if target_self {
            CGEventPostToPid(std::process::id() as i32, event.0.cast_mut());
        } else {
            CGEventPost(kCGHIDEventTap, event.0.cast_mut());
        }
    }
    Ok(())
}

pub(super) fn move_in_window(number: isize, at: (f64, f64)) -> Result<(), String> {
    let window = window(number).ok_or("the test window is unavailable")?;
    let view = window.contentView().ok_or("the test window has no content view")?;
    let bounds = view.bounds();
    window.setAcceptsMouseMovedEvents(true);
    let location = NSPoint::new(bounds.origin.x + at.0 * bounds.size.width, bounds.origin.y + (1.0 - at.1) * bounds.size.height);
    let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
        NSEventType::MouseMoved, location, NSEventModifierFlags::empty(), 0.0, number, None, 1, 0, 0.0,
    ).ok_or("could not create a test mouse event")?;
    window.sendEvent(&event);
    Ok(())
}
