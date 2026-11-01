//! Exercises the macOS click monitor against another process's synthetic window.

use super::{describe_at, drain_clicks, guide, pause_clicks, platform, reset_clicks, CaptureLayout, ClickPosition, Step, CLICKS};
use block2::RcBlock;
use jiffshot_capture::macos::ffi::{CFRelease, CGEventCreateMouseEvent, CGEventPost, CGWarpMouseCursorPosition, kCGHIDEventTap};
use jiffshot_capture::{screen, targets, Frame, Rect};
use jiffshot_doc::object::Kind;
use objc2::runtime::AnyObject;
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSButton, NSColor, NSEvent, NSEventMask, NSStatusWindowLevel,
    NSView, NSWindow, NSWindowStyleMask,
};
use objc2_core_foundation::CGPoint;
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use std::cell::RefCell;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

thread_local! {
    static STEP_FIXTURE_WINDOW: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
}

struct Fixture {
    child: Child,
    lines: mpsc::Receiver<io::Result<String>>,
    reaped: bool,
}

impl Fixture {
    fn start() -> io::Result<(Self, (i32, i32))> {
        let mut child = Command::new(std::env::current_exe()?)
            .arg("--internal-step-fixture")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().ok_or_else(|| io::Error::other("the step fixture has no output"))?;
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in io::BufReader::new(stdout).lines() {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let fixture = Self { child, lines, reaped: false };
        let ready = fixture.next_line().map_err(|error| io::Error::other(format!("the step fixture did not become ready: {error}")))?;
        let mut words = ready.split_whitespace();
        if words.next() != Some("READY") {
            return Err(io::Error::other("the step fixture did not open"));
        }
        let coordinate = |word: Option<&str>| -> io::Result<i32> {
            word.and_then(|word| word.parse().ok()).ok_or_else(|| io::Error::other("the step fixture returned an invalid point"))
        };
        let point = (coordinate(words.next())?, coordinate(words.next())?);
        Ok((fixture, point))
    }

    fn next_line(&self) -> io::Result<String> {
        self.lines.recv_timeout(Duration::from_secs(5)).map_err(|_| io::Error::other("the step fixture did not respond"))?
    }

    fn clicked(&self) -> io::Result<()> {
        let line = self.next_line().map_err(|error| io::Error::other(format!("the step fixture did not receive the synthetic click: {error}")))?;
        if line == "CLICK" { Ok(()) } else { Err(io::Error::other("the step fixture returned an unexpected event")) }
    }

    fn activate(&mut self) -> io::Result<()> {
        let input = self.child.stdin.as_mut().ok_or_else(|| io::Error::other("the step fixture has no input"))?;
        writeln!(input, "activate")?;
        input.flush()?;
        if self.next_line()? != "ACTIVE" {
            return Err(io::Error::other("the step fixture could not activate"));
        }
        std::thread::sleep(Duration::from_millis(150));
        Ok(())
    }

    fn close(mut self) -> io::Result<()> {
        let input = self.child.stdin.as_mut().ok_or_else(|| io::Error::other("the step fixture has no input"))?;
        writeln!(input, "close")?;
        input.flush()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait()? {
                self.reaped = true;
                return if status.success() { Ok(()) } else { Err(io::Error::other(format!("the step fixture exited with {status}"))) };
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other("the step fixture did not close"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn work_area() -> io::Result<objc2_core_foundation::CGRect> {
    screen::refresh_work_areas();
    screen::monitors()
        .into_iter()
        .find(|monitor| monitor.primary)
        .map(|monitor| screen::rect_to_points(monitor.work))
        .ok_or_else(|| io::Error::other("no primary display"))
}

fn window(mtm: MainThreadMarker, x: f64, y: f64, width: f64, height: f64) -> objc2::rc::Retained<NSWindow> {
    let frame = NSRect::new(NSPoint::new(x, screen::main_height_points() - y - height), NSSize::new(width, height));
    // SAFETY: this window is created and managed on the AppKit main thread.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: the retained window remains owned until it is closed.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setOpaque(true);
    window.setBackgroundColor(Some(&NSColor::blueColor()));
    window.setLevel(NSStatusWindowLevel);
    window.orderFrontRegardless();
    window
}

fn local_monitor(action: impl Fn() + 'static) -> io::Result<objc2::rc::Retained<AnyObject>> {
    let handler = RcBlock::new(move |event: NonNull<NSEvent>| {
        action();
        event.as_ptr()
    });
    // SAFETY: the handler returns the original live event unchanged.
    unsafe {
        NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown, &handler)
    }
    .ok_or_else(|| io::Error::other("could not observe fixture mouse events"))
}

fn click(point: (i32, i32), right: bool) -> io::Result<()> {
    let (x, y) = screen::to_points(point.0 as f64, point.1 as f64);
    let location = CGPoint { x, y };
    let (down, up, button) = if right { (3, 4, 1) } else { (1, 2, 0) };
    // SAFETY: the cursor position and event location use Core Graphics points.
    if unsafe { CGWarpMouseCursorPosition(location) } != 0 {
        return Err(io::Error::other("could not position the test cursor"));
    }
    for kind in [down, up] {
        // SAFETY: each event is posted once and then released.
        unsafe {
            let event = CGEventCreateMouseEvent(std::ptr::null_mut(), kind, location, button);
            if event.is_null() {
                return Err(io::Error::other("could not create the fixture click"));
            }
            CGEventPost(kCGHIDEventTap, event);
            CFRelease(event.cast());
        }
        std::thread::sleep(Duration::from_millis(12));
    }
    Ok(())
}

fn take_clicks() -> io::Result<Vec<(ClickPosition, bool)>> {
    drain_clicks().into_iter().map(|(point, right)| point.map(|point| (point, right))).collect()
}

fn wait_clicks(count: usize) -> io::Result<Vec<(ClickPosition, bool)>> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut clicks = Vec::new();
    while clicks.len() < count && Instant::now() < deadline {
        clicks.extend(take_clicks()?);
        std::thread::sleep(Duration::from_millis(10));
    }
    if clicks.len() == count { Ok(clicks) } else { Err(io::Error::other(format!("the step monitor received {} of {count} clicks", clicks.len()))) }
}

struct RestoreCursor((i32, i32));

impl Drop for RestoreCursor {
    fn drop(&mut self) {
        crate::sys::set_cursor_pos(self.0.0, self.0.1);
    }
}

unsafe extern "system" fn record_message(
    hwnd: crate::sys::HWND,
    message: u32,
    wparam: crate::sys::WPARAM,
    lparam: crate::sys::LPARAM,
) -> crate::sys::LRESULT {
    if message == crate::tray::WM_APP_RUN {
        // SAFETY: on_capture_thread posted this boxed callback to this window.
        let run = unsafe { Box::from_raw(lparam as *mut Box<dyn FnOnce() + Send>) };
        run();
        0
    } else {
        // SAFETY: this is the fixture's own message window.
        unsafe { crate::sys::DefWindowProcW(hwnd, message, wparam, lparam) }
    }
}

struct Recording {
    result: mpsc::Receiver<io::Result<Option<Vec<Step>>>>,
    active: bool,
}

impl Recording {
    fn start() -> Self {
        let (tx, result) = mpsc::channel();
        std::thread::spawn(move || {
            let hwnd = crate::sys::create_message_window(crate::tray::CLASS, "", record_message);
            let outcome = super::record();
            // SAFETY: the fixture owns this message window on this thread.
            unsafe { crate::sys::DefWindowProcW(hwnd, crate::sys::WM_CLOSE, 0, 0) };
            let _ = tx.send(outcome);
        });
        Self { result, active: true }
    }

    fn finish(mut self) -> io::Result<Option<Vec<Step>>> {
        let result = self.result.recv_timeout(Duration::from_secs(20)).map_err(|_| io::Error::other("Step Capture did not finish"))?;
        self.active = false;
        result
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        if self.active {
            crate::on_capture_thread(|| crate::hud::finish_panel(None));
            let _ = self.result.recv_timeout(Duration::from_secs(5));
        }
    }
}

fn wait_panel(rx: &mpsc::Receiver<String>, middle_button: &str, status: Option<&str>) -> io::Result<u64> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let json = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| io::Error::other(format!("Step Capture panel did not reach {middle_button:?} / {status:?}")))?;
        let view: serde_json::Value = serde_json::from_str(&json).map_err(io::Error::other)?;
        if let Some(panels) = view.get("panels").and_then(|panels| panels.as_array()) {
            for panel in panels {
                let button = panel.get("buttons").and_then(|buttons| buttons.get(1)).and_then(|button| button.as_str());
                let text = panel.get("status").and_then(|text| text.as_str()).unwrap_or_default();
                if button == Some(middle_button) && status.is_none_or(|status| text.contains(status)) {
                    return panel.get("id").and_then(|id| id.as_u64()).ok_or_else(|| io::Error::other("Step Capture panel has no ID"));
                }
            }
        }
    }
}

fn record_guide(
    fixture: &mut Fixture,
    point: (i32, i32),
    fixture_rect: Rect,
    hud: &mpsc::Receiver<String>,
    path: &Path,
) -> io::Result<()> {
    fixture.activate()?;
    let recording = Recording::start();
    let panel = wait_panel(hud, "Pause", None)?;
    click(point, false)?;
    fixture.clicked()?;
    wait_panel(hud, "Pause", Some("1 step recorded"))?;

    crate::answer_panel(panel, Some(1));
    let paused = wait_panel(hud, "Resume", None)?;
    click(point, false)?;
    fixture.clicked()?;
    std::thread::sleep(Duration::from_millis(180));
    crate::answer_panel(paused, Some(1));
    let resumed = wait_panel(hud, "Pause", None)?;
    click(point, true)?;
    fixture.clicked()?;
    wait_panel(hud, "Pause", Some("2 steps recorded"))?;
    crate::answer_panel(resumed, Some(0));
    let mut steps = recording.finish()?.ok_or_else(|| io::Error::other("Step Capture did not return a guide"))?;
    if steps.len() != 2 || steps[0].right || !steps[1].right {
        return Err(io::Error::other(format!("Step Capture recorded {} incorrect steps", steps.len())));
    }
    for step in &mut steps {
        if !step.description.contains("Jiffshot synthetic button") || step.area.intersect(fixture_rect) != Some(fixture_rect) {
            return Err(io::Error::other("Step Capture did not preserve the synthetic target"));
        }
        let mapping = jiffshot_capture::CaptureMapping::new(step.area, step.frame.width, step.frame.height)
            .ok_or_else(|| io::Error::other("the step capture has invalid dimensions"))?;
        let local = mapping.image_rect(fixture_rect);
        let cropped = step.frame.crop(local).ok_or_else(|| io::Error::other("the captured fixture was outside the screenshot"))?;
        let color = cropped.rgb(10, cropped.height as i32 / 2).ok_or_else(|| io::Error::other("could not inspect the fixture pixel"))?;
        if color.2 < 150 || color.0 > 100 || color.1 > 100 {
            return Err(io::Error::other(format!("the synthetic fixture was not visible in the captured frame: {color:?}")));
        }
        // Do not save unrelated desktop content around the synthetic fixture.
        step.frame = cropped;
        step.area = fixture_rect;
    }
    let doc = guide(&steps)?;
    jiffshot_doc::io::save_document(&doc, path).map_err(io::Error::other)?;
    let restored = jiffshot_doc::io::load_document(path).map_err(io::Error::other)?;
    let captions = restored.objects.iter().filter(|object| matches!(&object.kind, Kind::Text { text, .. } if text.contains("Jiffshot synthetic button"))).count();
    let images = restored.objects.iter().filter(|object| matches!(&object.kind, Kind::Image { .. })).count();
    if captions != 2 || images != 2 {
        return Err(io::Error::other("the editable guide did not preserve its captions and captured images"));
    }

    let cancelled = Recording::start();
    let panel = wait_panel(hud, "Pause", None)?;
    click(point, false)?;
    fixture.clicked()?;
    crate::answer_panel(panel, Some(2));
    if cancelled.finish()?.is_some() {
        return Err(io::Error::other("cancelled Step Capture returned a guide"));
    }
    Ok(())
}

pub fn run(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    jiffshot_capture::os::require_event_posting()?;
    let mtm = MainThreadMarker::new().ok_or_else(|| io::Error::other("the step self-test must run on the main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    if app.activationPolicy() != NSApplicationActivationPolicy::Regular && !app.setActivationPolicy(NSApplicationActivationPolicy::Regular) {
        return Err(io::Error::other("could not activate the step self-test"));
    }
    let work = work_area()?;
    if work.size.width < 750.0 || work.size.height < 350.0 {
        return Err(io::Error::other("the primary display is too small for the step fixture"));
    }
    let (own_x, own_y) = (work.origin.x + 450.0, work.origin.y + 40.0);
    let own_window = window(mtm, own_x, own_y, 200.0, 200.0);
    let own_point = screen::from_points(own_x + 100.0, own_y + 100.0);
    let local_count = Arc::new(AtomicUsize::new(0));
    let count = local_count.clone();
    let monitor = local_monitor(move || {
        count.fetch_add(1, Ordering::Release);
    })?;
    let (mut fixture, point) = Fixture::start()?;
    let path = dir.join("synthetic-step-guide.jsd");
    let recorded_path = dir.join("recorded-step-guide.jsd");
    let fixture_rect = screen::rect_from_points(jiffshot_capture::macos::ffi::rect(work.origin.x + 40.0, work.origin.y + 40.0, 320.0, 200.0));
    let (hud_tx, hud_rx) = mpsc::channel();
    crate::HOOKS.set(crate::Hooks {
        open_in_editor: Box::new(|_| false),
        show_editor: Box::new(|| {}),
        quit: Box::new(|| {}),
        notify: Box::new(|_, _, _| {}),
        state: Box::new(|_| {}),
        startup_status: Box::new(|_| {}),
        show_capture_window: Box::new(|| {}),
        show_text: Box::new(|_| {}),
        hud: Box::new(move |json| {
            let _ = hud_tx.send(json);
        }),
        overlay: Box::new(|_| {}),
        screen_draw: Box::new(|_| {}),
    }).map_err(|_| io::Error::other("the step self-test cannot replace existing application hooks"))?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(|| -> io::Result<()> {
            let _restore = RestoreCursor(screen::cursor_pos());
            let deadline = Instant::now() + Duration::from_secs(3);
            while targets::window_owner_at(point.0, point.1) != Some(fixture.child.id()) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            if targets::window_owner_at(point.0, point.1) != Some(fixture.child.id()) {
                return Err(io::Error::other(format!(
                    "the synthetic step fixture is not the topmost target at {point:?}: expected {}, got {:?}",
                    fixture.child.id(),
                    targets::window_owner_at(point.0, point.1)
                )));
            }
            reset_clicks();
            let layout = CaptureLayout::current()?;
            let watcher = platform::Watcher::start(layout.desktop_scale())?;
            for right in [false, false, true] {
                click(point, right)?;
            }
            for _ in 0..3 {
                fixture.clicked().map_err(|error| {
                    let queued = CLICKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).pending.len();
                    io::Error::other(format!("{error}; the global monitor queued {queued} clicks"))
                })?;
            }
            let clicks = wait_clicks(3)?;
            if clicks.iter().map(|(_, right)| *right).collect::<Vec<_>>() != [false, false, true]
                || clicks.iter().any(|(actual, _)| actual.grid.0.abs_diff(point.0) > 2 || actual.grid.1.abs_diff(point.1) > 2)
            {
                return Err(io::Error::other("the step monitor reported incorrect click positions or buttons"));
            }
            let description = describe_at(&layout, clicks[0].0, false)?;
            if !description.contains("Jiffshot synthetic button") {
                return Err(io::Error::other(format!("the fixture button was not described: {description}")));
            }
            pause_clicks(true);
            click(point, false)?;
            fixture.clicked().map_err(|error| io::Error::other(format!("paused click: {error}")))?;
            std::thread::sleep(Duration::from_millis(150));
            if !take_clicks()?.is_empty() {
                return Err(io::Error::other("the step monitor recorded a click while paused"));
            }
            pause_clicks(false);
            click(point, false)?;
            fixture.clicked().map_err(|error| io::Error::other(format!("resumed click: {error}")))?;
            wait_clicks(1)?;
            if targets::window_owner_at(own_point.0, own_point.1) != Some(std::process::id()) {
                return Err(io::Error::other("the self-test panel is not the topmost target"));
            }
            click(own_point, false)?;
            let deadline = Instant::now() + Duration::from_secs(3);
            while local_count.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if local_count.load(Ordering::Acquire) == 0 {
                return Err(io::Error::other("the self-test panel did not receive its click"));
            }
            std::thread::sleep(Duration::from_millis(150));
            if !take_clicks()?.is_empty() {
                return Err(io::Error::other("the step monitor recorded its own panel"));
            }
            drop(watcher);
            fixture.activate()?;
            click(point, false)?;
            fixture.clicked().map_err(|error| io::Error::other(format!("stopped click: {error}")))?;
            std::thread::sleep(Duration::from_millis(150));
            if !take_clicks()?.is_empty() {
                return Err(io::Error::other("the stopped step monitor recorded another click"));
            }
            let frame = Frame::new(300, 200);
            let step = Step { frame, area: Rect::new(point.0 - 100, point.1 - 100, 300, 200), mapping: None, click: point, click_precise: None, right: false, description };
            let doc = guide(&[step])?;
            jiffshot_doc::io::save_document(&doc, &path).map_err(io::Error::other)?;
            let restored = jiffshot_doc::io::load_document(&path).map_err(io::Error::other)?;
            if !restored.objects.iter().any(|object| matches!(&object.kind, Kind::Text { text, .. } if text.contains("Jiffshot synthetic button"))) {
                return Err(io::Error::other("the editable guide did not preserve the step caption"));
            }
            record_guide(&mut fixture, point, fixture_rect, &hud_rx, &recorded_path)?;
            fixture.close()?;
            println!("step monitor and recording loop passed pause/resume, own-window exclusion, stop, cancellation, and editable captured guides");
            Ok(())
        })
        .unwrap_or_else(|_| Err(io::Error::other("the step self-test panicked")));
        let _ = tx.send(result);
        dispatch2::DispatchQueue::main().exec_async(crate::selftest_scroll::stop_app);
    });
    app.run();
    // SAFETY: this removes the local monitor installed above.
    unsafe { NSEvent::removeMonitor(&monitor) };
    own_window.close();
    rx.recv_timeout(Duration::from_secs(5)).map_err(|_| io::Error::other("the step self-test did not finish"))?
}

pub fn fixture() -> io::Result<()> {
    let mtm = MainThreadMarker::new().ok_or_else(|| io::Error::other("the step fixture must run on the main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    if app.activationPolicy() != NSApplicationActivationPolicy::Regular && !app.setActivationPolicy(NSApplicationActivationPolicy::Regular) {
        return Err(io::Error::other("could not activate the step fixture"));
    }
    let work = work_area()?;
    let (x, y) = (work.origin.x + 40.0, work.origin.y + 40.0);
    let window = window(mtm, x, y, 320.0, 200.0);
    let content = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(320.0, 200.0)));
    let button = NSButton::initWithFrame(NSButton::alloc(mtm), NSRect::new(NSPoint::new(60.0, 60.0), NSSize::new(200.0, 80.0)));
    button.setTitle(&NSString::from_str("Jiffshot synthetic button"));
    content.addSubview(&button);
    window.setContentView(Some(&content));
    STEP_FIXTURE_WINDOW.with(|fixture| *fixture.borrow_mut() = Some(window.clone()));
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let monitor = local_monitor(|| {
        println!("CLICK");
        if let Err(error) = io::stdout().flush() {
            eprintln!("could not report a fixture click: {error}");
        }
    })?;
    std::thread::spawn(|| {
        for line in io::stdin().lock().lines() {
            match line.as_deref() {
                Ok("activate") => dispatch2::DispatchQueue::main().exec_async(|| {
                    if let Some(mtm) = MainThreadMarker::new() {
                        #[allow(deprecated)]
                        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
                        STEP_FIXTURE_WINDOW.with(|fixture| {
                            if let Some(window) = fixture.borrow().as_ref() {
                                window.makeKeyAndOrderFront(None);
                            }
                        });
                    }
                    println!("ACTIVE");
                    if let Err(error) = io::stdout().flush() {
                        eprintln!("could not report fixture activation: {error}");
                    }
                }),
                _ => break,
            }
        }
        dispatch2::DispatchQueue::main().exec_async(crate::selftest_scroll::stop_app);
    });
    let point = screen::from_points(x + 160.0, y + 100.0);
    println!("READY {} {}", point.0, point.1);
    io::stdout().flush()?;
    app.run();
    // SAFETY: this removes the local monitor installed above.
    unsafe { NSEvent::removeMonitor(&monitor) };
    STEP_FIXTURE_WINDOW.with(|fixture| *fixture.borrow_mut() = None);
    window.close();
    Ok(())
}
