//! The CoreGraphics and CoreFoundation C functions the macOS backends call.
//! Declared directly (they are stable C APIs); Objective-C frameworks go
//! through the objc2 crates.

#![allow(non_snake_case, non_upper_case_globals, dead_code)]

use objc2_core_foundation::{CGPoint, CGRect, CGSize, CFUUID};
use std::ffi::c_void;
use std::ptr::NonNull;

pub type CFTypeRef = *const c_void;
pub type CGContextRef = *mut c_void;
pub type CGImageRef = *mut c_void;
pub type CGColorSpaceRef = *mut c_void;
pub type CGEventRef = *mut c_void;
pub type CGEventSourceRef = *mut c_void;
pub type CGDisplayModeRef = *mut c_void;
pub type CGDirectDisplayID = u32;

pub const kCGImageAlphaPremultipliedFirst: u32 = 2;
pub const kCGImageAlphaNoneSkipFirst: u32 = 6;
pub const kCGBitmapByteOrder32Little: u32 = 2 << 12;

pub const kCGEventSourceStateCombinedSessionState: i32 = 0;
pub const kCGEventSourceStateHIDSystemState: i32 = 1;
pub const kCGScrollEventUnitPixel: u32 = 0;
pub const kCGScrollEventUnitLine: u32 = 1;
pub const kCGHIDEventTap: u32 = 0;

pub const kCGWindowListOptionAll: u32 = 0;
pub const kCGWindowListOptionOnScreenOnly: u32 = 1 << 0;
pub const kCGWindowListExcludeDesktopElements: u32 = 1 << 4;
pub const kCGNullWindowID: u32 = 0;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    pub fn CGGetActiveDisplayList(max: u32, displays: *mut CGDirectDisplayID, count: *mut u32) -> i32;
    pub fn CGMainDisplayID() -> CGDirectDisplayID;
    pub fn CGDisplayBounds(display: CGDirectDisplayID) -> CGRect;
    pub fn CGDisplayCopyDisplayMode(display: CGDirectDisplayID) -> CGDisplayModeRef;
    pub fn CGDisplayModeGetPixelWidth(mode: CGDisplayModeRef) -> usize;
    pub fn CGDisplayModeGetPixelHeight(mode: CGDisplayModeRef) -> usize;
    pub fn CGDisplayModeGetWidth(mode: CGDisplayModeRef) -> usize;
    pub fn CGDisplayModeGetHeight(mode: CGDisplayModeRef) -> usize;
    pub fn CGDisplayModeRelease(mode: CGDisplayModeRef);
    pub fn CGDisplayPixelsWide(display: CGDirectDisplayID) -> usize;
    pub fn CGDisplayMirrorsDisplay(display: CGDirectDisplayID) -> CGDirectDisplayID;
    pub fn CGDisplayRotation(display: CGDirectDisplayID) -> f64;

    pub fn CGPreflightScreenCaptureAccess() -> bool;
    pub fn CGRequestScreenCaptureAccess() -> bool;

    pub fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: CGColorSpaceRef,
        bitmap_info: u32,
    ) -> CGContextRef;
    pub fn CGBitmapContextCreateImage(context: CGContextRef) -> CGImageRef;
    pub fn CGContextRelease(context: CGContextRef);
    pub fn CGContextDrawImage(context: CGContextRef, rect: CGRect, image: CGImageRef);
    pub fn CGContextSetInterpolationQuality(context: CGContextRef, quality: i32);
    pub fn CGImageGetWidth(image: CGImageRef) -> usize;
    pub fn CGImageGetHeight(image: CGImageRef) -> usize;
    pub fn CGImageRelease(image: CGImageRef);
    pub fn CGImageRetain(image: CGImageRef) -> CGImageRef;

    pub fn CGEventCreate(source: CGEventSourceRef) -> CGEventRef;
    pub fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    pub fn CGEventPost(tap: u32, event: CGEventRef);
    pub fn CGEventCreateScrollWheelEvent2(
        source: CGEventSourceRef,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> CGEventRef;
    pub fn CGEventCreateMouseEvent(source: CGEventSourceRef, kind: u32, location: CGPoint, button: u32) -> CGEventRef;
    pub fn CGEventCreateKeyboardEvent(source: CGEventSourceRef, key: u16, down: bool) -> CGEventRef;
    pub fn CGEventSetFlags(event: CGEventRef, flags: u64);
    pub fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    pub fn CGEventKeyboardSetUnicodeString(event: CGEventRef, length: usize, unicode: *const u16);
    pub fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
    pub fn CGEventSourceButtonState(state: i32, button: u32) -> bool;
    pub fn CGEventSourceFlagsState(state: i32) -> u64;
    pub fn CGWarpMouseCursorPosition(point: CGPoint) -> i32;
    pub fn CGAssociateMouseAndMouseCursorPosition(connected: bool) -> i32;

    pub fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFTypeRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    pub fn CFRelease(object: CFTypeRef);
    pub fn CFRetain(object: CFTypeRef) -> CFTypeRef;
    pub fn CFEqual(left: CFTypeRef, right: CFTypeRef) -> bool;
}

#[link(name = "ColorSync", kind = "framework")]
extern "C" {
    pub fn CGDisplayCreateUUIDFromDisplayID(display: CGDirectDisplayID) -> Option<NonNull<CFUUID>>;
}

/// Releases a CoreFoundation object when dropped.
pub struct Owned(pub CFTypeRef);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `Owned` holds the single reference it was given.
            unsafe { CFRelease(self.0) }
        }
    }
}

pub fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect { origin: CGPoint { x, y }, size: CGSize { width: w, height: h } }
}
