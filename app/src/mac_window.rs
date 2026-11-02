//! AppKit access for live Rustolonia top-level windows.

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSWindow, NSWindowCollectionBehavior, NSWindowWillCloseNotification, NSStatusWindowLevel};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObjectProtocol};
use rustolonia::AppScope;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr::NonNull;

thread_local! {
    static CLOSE_OBSERVER: RefCell<Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>> = RefCell::new(None);
    static PROTECTED_WINDOWS: RefCell<HashMap<isize, jiffshot_capture::screen::ExclusionTicket>> = RefCell::new(HashMap::new());
}

/// Resolves an opaque handle freshly obtained from a live Rustolonia window
/// on the UI thread. The handle is compared, never dereferenced. It must not
/// be reused after native closure or recreation, since addresses can be reused.
pub fn window(handle: isize) -> Option<Retained<NSWindow>> {
    let marker = MainThreadMarker::new()?;
    if handle == 0 {
        return None;
    }
    NSApplication::sharedApplication(marker)
        .windows()
        .iter()
        .find(|window| (&**window as *const NSWindow as isize) == handle)
}

pub fn number(handle: isize) -> Option<u32> {
    u32::try_from(window(handle)?.windowNumber()).ok().filter(|number| *number != 0)
}

/// Tracks this native top-level until its close notification. The numeric
/// handle is used only as an identity on the UI thread, never dereferenced.
pub fn protect(handle: isize) -> Result<(u32, jiffshot_capture::screen::ExclusionTicket), String> {
    if CLOSE_OBSERVER.with(|observer| observer.borrow().is_none()) {
        return Err("the native window lifetime observer is unavailable".into());
    }
    let number = number(handle).ok_or("the capture window has no live native window number")?;
    let ticket = jiffshot_capture::screen::register_excluded_window(number);
    let previous = PROTECTED_WINDOWS.with(|windows| windows.borrow_mut().insert(handle, ticket));
    if let Some(previous) = previous {
        if !jiffshot_capture::screen::retire_exclusion(previous) {
            eprintln!("capture window: its previous exclusion was already retired");
        }
    }
    Ok((number, ticket))
}

pub fn protection(handle: isize) -> Option<jiffshot_capture::screen::ExclusionTicket> {
    PROTECTED_WINDOWS.with(|windows| windows.borrow().get(&handle).copied())
}

/// Makes a utility window visible above normal windows and full-screen
/// applications without activating it. Click-through is used for borders.
pub fn prepare(handle: isize, click_through: bool) -> Result<(), String> {
    let window = window(handle).ok_or("the native window is no longer available")?;
    window.setIgnoresMouseEvents(click_through);
    window.setHidesOnDeactivate(false);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    window.setLevel(NSStatusWindowLevel);
    Ok(())
}

pub fn activate(handle: isize) -> Result<(), String> {
    let marker = MainThreadMarker::new().ok_or("window activation must run on the main thread")?;
    let window = window(handle).ok_or("the native window is no longer available")?;
    if window.isMiniaturized() {
        window.deminiaturize(None);
    }
    // macOS 13 does not provide the newer cooperative activation API.
    #[allow(deprecated)]
    NSApplication::sharedApplication(marker).activateIgnoringOtherApps(true);
    window.makeKeyAndOrderFront(None);
    Ok(())
}

/// Hides protected top-levels and retires their own exclusion tickets after
/// the close notification has returned.
pub fn watch_window_lifetimes(scope: &AppScope) -> Result<(), String> {
    if MainThreadMarker::new().is_none() {
        return Err("window observation must start on the main thread".into());
    }
    CLOSE_OBSERVER.with(|slot| {
        if slot.borrow().is_some() {
            return Ok(());
        }
        let dispatcher = scope.clone();
        let callback = RcBlock::new(move |notification: NonNull<NSNotification>| {
            // SAFETY: Foundation invokes the copied block with a live notification.
            let object = unsafe { notification.as_ref() }.object();
            if let Some(window) = object.as_deref().and_then(AnyObject::downcast_ref::<NSWindow>) {
                let handle = (window as *const NSWindow) as isize;
                crate::ui::forget_native_window(handle);
                let ticket = PROTECTED_WINDOWS.with(|windows| windows.borrow_mut().remove(&handle));
                if let Some(ticket) = ticket {
                    window.setIgnoresMouseEvents(true);
                    window.setAlphaValue(0.0);
                    window.orderOut(None);
                    if let Err(error) = dispatcher.post(move || {
                        if !jiffshot_capture::screen::retire_exclusion(ticket) {
                            eprintln!("capture window: its exclusion was already retired");
                        }
                    }) {
                        eprintln!("capture window: could not schedule exclusion retirement; keeping it registered: {error:?}");
                    }
                }
            }
        });
        // SAFETY: the block captures no thread-bound values, and AppKit posts
        // this window notification on the UI thread.
        let observer = unsafe {
            NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                Some(NSWindowWillCloseNotification),
                None,
                None,
                &callback,
            )
        };
        *slot.borrow_mut() = Some(observer);
        Ok(())
    })
}
