//! Displays, the desktop coordinate model, ScreenCaptureKit grabs and the
//! mouse cursor on macOS.
//!
//! Jiffshot currently represents global Quartz points on an integer reference
//! grid using the largest attached display density. That grid is used for
//! input and overlay geometry, not necessarily the captured image size.
//! Captures on one display use its native pixel density; captures spanning
//! displays use the largest intersecting density per axis, resampling pieces
//! as needed. [`Monitor::dpi`] reports each display's own scale.

use crate::macos::ffi::*;
use crate::macos::sck;
use crate::{CaptureMapping, Frame, Rect};
use objc2::rc::Retained;
use objc2_core_foundation::{CFRetained, CGRect, CFUUID};
use objc2_core_media::{CMClock, CMTime, CMTimeFlags};
use objc2_screen_capture_kit::SCShareableContent;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// macOS has no process DPI modes; every API used here reports points that
/// are converted with [`scale_factor`].
pub fn enable_per_monitor_dpi() {}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Display {
    id: CGDirectDisplayID,
    /// Global points, top-left origin.
    bounds: CGRect,
    scale: f64,
    /// Dimensions reported by the active display mode, before any orientation
    /// needed to match the global display bounds.
    mode_pixels: Option<(usize, usize)>,
    mode_points: Option<(f64, f64)>,
    /// AppKit's backing extent already follows the screen's orientation.
    backing_pixels: Option<(f64, f64)>,
    rotation: f64,
    main: bool,
    mirrors: Option<CGDirectDisplayID>,
}

impl Display {
    fn native_size(&self) -> io::Result<(u32, u32)> {
        if !self.bounds.origin.x.is_finite() || !self.bounds.origin.y.is_finite()
            || !self.bounds.size.width.is_finite() || !self.bounds.size.height.is_finite()
            || self.bounds.size.width <= 0.0 || self.bounds.size.height <= 0.0
        {
            return Err(io::Error::other(format!("display {} has invalid native dimensions", self.id)));
        }

        if let Some((width, height)) = self.backing_pixels {
            let dimension = |value: f64| {
                (value.is_finite() && value >= 1.0 && value <= i32::MAX as f64 && (value - value.round()).abs() < 0.001)
                    .then(|| value.round() as u32)
            };
            return dimension(width).zip(dimension(height))
                .ok_or_else(|| io::Error::other(format!("display {} has invalid AppKit backing dimensions", self.id)));
        }

        let rotation = self.rotation.rem_euclid(180.0);
        if !rotation.is_finite() || (rotation > 0.001 && 180.0 - rotation > 0.001) {
            return Err(io::Error::other(format!(
                "display {} requires an AppKit backing transform for its rotation",
                self.id
            )));
        }
        let (width, height) = self.mode_pixels.ok_or_else(|| io::Error::other(format!("display {} has no active pixel mode", self.id)))?;
        let (points_width, points_height) = self.mode_points
            .ok_or_else(|| io::Error::other(format!("display {} has no active logical mode", self.id)))?;
        if width == 0 || height == 0 || width > i32::MAX as usize || height > i32::MAX as usize
            || !points_width.is_finite() || !points_height.is_finite() || points_width <= 0.0 || points_height <= 0.0
        {
            return Err(io::Error::other(format!("display {} has invalid native dimensions", self.id)));
        }
        let matches = |a: f64, b: f64| (a - b).abs() < 0.001;
        let direct = matches(points_width, self.bounds.size.width) && matches(points_height, self.bounds.size.height);
        let swapped = matches(points_height, self.bounds.size.width) && matches(points_width, self.bounds.size.height);
        let (width, height) = if direct && (!swapped || width == height) {
            (width, height)
        } else if swapped && !direct {
            (height, width)
        } else {
            return Err(io::Error::other(format!(
                "display {} native orientation is unavailable without an AppKit backing transform",
                self.id
            )));
        };
        Ok((width as u32, height as u32))
    }

    fn density(&self) -> io::Result<(f64, f64)> {
        let (width, height) = self.native_size()?;
        Ok((width as f64 / self.bounds.size.width, height as f64 / self.bounds.size.height))
    }
}

/// Half-open global Quartz bounds in logical points, with a top-left origin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogicalBounds([f64; 4]);

impl LogicalBounds {
    /// Validates left, top, right and bottom without rounding to a pixel grid.
    pub fn new(edges: [f64; 4]) -> io::Result<Self> {
        let [left, top, right, bottom] = edges;
        if !edges.iter().all(|edge| edge.is_finite())
            || !(right - left).is_finite() || !(bottom - top).is_finite()
            || right <= left || bottom <= top
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid logical capture bounds"));
        }
        Ok(Self(edges))
    }

    /// Returns left, top, right and bottom in global Quartz points.
    pub fn edges(self) -> [f64; 4] {
        self.0
    }

    /// Intersects two half-open logical rectangles.
    pub fn intersection(self, other: Self) -> Option<Self> {
        let [left, top, right, bottom] = self.0;
        let [other_left, other_top, other_right, other_bottom] = other.0;
        Self::new([
            left.max(other_left),
            top.max(other_top),
            right.min(other_right),
            bottom.min(other_bottom),
        ]).ok()
    }

    fn from_rect(bounds: CGRect) -> io::Result<Self> {
        Self::new([
            bounds.origin.x,
            bounds.origin.y,
            bounds.origin.x + bounds.size.width,
            bounds.origin.y + bounds.size.height,
        ])
    }

    fn from_capture(mapping: CaptureMapping, scale: f64) -> io::Result<Self> {
        if !scale.is_finite() || scale <= 0.0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid desktop coordinate scale"));
        }
        Self::new(mapping.desktop_bounds().map(|edge| edge / scale))
    }

    fn intersect(self, display: &Display) -> Option<Self> {
        let [area_left, area_top, area_right, area_bottom] = self.0;
        let bounds = display.bounds;
        let left = area_left.max(bounds.origin.x);
        let top = area_top.max(bounds.origin.y);
        let right = area_right.min(bounds.origin.x + bounds.size.width);
        let bottom = area_bottom.min(bounds.origin.y + bounds.size.height);
        (left.is_finite() && top.is_finite() && right.is_finite() && bottom.is_finite() && right > left && bottom > top)
            .then_some(Self([left, top, right, bottom]))
    }
}

/// Maps logical desktop bounds to a capture's pixel dimensions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogicalCaptureMapping {
    bounds: LogicalBounds,
    size: (u32, u32),
}

impl LogicalCaptureMapping {
    /// Uses explicit output dimensions rather than choosing a display density.
    pub fn new(bounds: LogicalBounds, size: (u32, u32)) -> io::Result<Self> {
        let bytes = (size.0 as usize).checked_mul(size.1 as usize).and_then(|pixels| pixels.checked_mul(4));
        if size.0 == 0 || size.1 == 0 || size.0 > i32::MAX as u32 || size.1 > i32::MAX as u32
            || bytes.is_none_or(|bytes| bytes > isize::MAX as usize)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid capture dimensions"));
        }
        Ok(Self { bounds, size })
    }

    pub fn bounds(self) -> LogicalBounds {
        self.bounds
    }

    pub fn size(self) -> (u32, u32) {
        self.size
    }

    fn from_capture(mapping: CaptureMapping, scale: f64) -> io::Result<Self> {
        Self::new(LogicalBounds::from_capture(mapping, scale)?, mapping.size())
    }

    fn axis_center(start: f64, end: f64, pixel: u32, pixels: u32) -> f64 {
        start + ((f64::from(pixel) + 0.5) / f64::from(pixels)) * (end - start)
    }

    fn pixel_center(self, x: u32, y: u32) -> (f64, f64) {
        let [left, top, right, bottom] = self.bounds.0;
        (
            Self::axis_center(left, right, x, self.size.0),
            Self::axis_center(top, bottom, y, self.size.1),
        )
    }

    fn first_pixel_at_or_after(position: f64, start: f64, end: f64, pixels: u32) -> i32 {
        let (mut low, mut high) = (0, pixels);
        while low < high {
            let middle = low + (high - low) / 2;
            if Self::axis_center(start, end, middle, pixels) < position {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low as i32
    }
}

/// One consistent display configuration for a capture or monitor query.
#[derive(Clone, Debug, PartialEq)]
struct Layout {
    displays: Vec<Display>,
    scale: f64,
    generation: u64,
}

static DISPLAY_GENERATION: AtomicU64 = AtomicU64::new(0);

fn ensure_layout(expected: &Layout, actual: &Layout) -> io::Result<()> {
    if expected == actual {
        Ok(())
    } else {
        Err(io::Error::other("the display configuration changed during capture; retry the capture"))
    }
}

fn check_layout(expected: &Layout) -> io::Result<()> {
    let actual = Layout::current();
    if DISPLAY_GENERATION.load(Ordering::SeqCst) != actual.generation {
        return Err(io::Error::other("the display configuration changed during capture; retry the capture"));
    }
    ensure_layout(expected, &actual)
}

impl Layout {
    fn new(mut displays: Vec<Display>) -> Self {
        let ids: Vec<_> = displays.iter().map(|display| display.id).collect();
        displays.retain(|display| display.mirrors.is_none_or(|master| !ids.contains(&master)));
        displays.sort_by_key(|display| (!display.main, display.id));
        let scale = displays.iter().map(|display| {
            display.density().map_or(display.scale, |(x, y)| x.max(y))
        }).fold(1.0, f64::max);
        Self { displays, scale, generation: 0 }
    }

    fn current() -> Self {
        let generation = DISPLAY_GENERATION.load(Ordering::SeqCst);
        Self { generation, ..Self::new(displays()) }
    }

    fn mapping(&self, rect: Rect) -> io::Result<CaptureMapping> {
        let area = CaptureMapping::identity(rect)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid capture rectangle"))?;
        let (x, y) = self.logical_density(LogicalBounds::from_capture(area, self.scale)?)?;
        let width = ((rect.width as f64 * x / self.scale).round() as u32).max(1);
        let height = ((rect.height as f64 * y / self.scale).round() as u32).max(1);
        CaptureMapping::new(rect, width, height).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid capture dimensions"))
    }

    fn logical_density(&self, area: LogicalBounds) -> io::Result<(f64, f64)> {
        let mut density: Option<(f64, f64)> = None;
        for display in &self.displays {
            if area.intersect(display).is_some() {
                let (x, y) = display.density()?;
                density = Some(density.map_or((x, y), |(max_x, max_y)| (max_x.max(x), max_y.max(y))));
            }
        }
        density.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the capture rectangle does not intersect an available display"))
    }

    fn logical_size(&self, area: LogicalBounds) -> io::Result<(u32, u32)> {
        let (x, y) = self.logical_density(area)?;
        let [left, top, right, bottom] = area.0;
        let dimension = |value: f64| {
            let rounded = value.round().max(1.0);
            if !value.is_finite() || value <= 0.0 || rounded > i32::MAX as f64 {
                Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid capture dimensions"))
            } else {
                Ok(rounded as u32)
            }
        };
        Ok((dimension((right - left) * x)?, dimension((bottom - top) * y)?))
    }

    fn logical_mapping(&self, area: LogicalBounds) -> io::Result<LogicalCaptureMapping> {
        LogicalCaptureMapping::new(area, self.logical_size(area)?)
    }

    fn logical_virtual_bounds(&self) -> io::Result<LogicalBounds> {
        let mut displays = self.displays.iter();
        let first = displays.next().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no active displays are available"))?;
        let mut bounds = LogicalBounds::from_rect(first.bounds)?.edges();
        for display in displays {
            let [left, top, right, bottom] = LogicalBounds::from_rect(display.bounds)?.edges();
            bounds = [bounds[0].min(left), bounds[1].min(top), bounds[2].max(right), bounds[3].max(bottom)];
        }
        LogicalBounds::new(bounds)
    }

    fn native_point(&self, x: i32, y: i32) -> io::Result<(f64, f64)> {
        let point = (f64::from(x) / self.scale, f64::from(y) / self.scale);
        if self.displays.iter().any(|display| {
            let bounds = display.bounds;
            point.0 >= bounds.origin.x && point.0 < bounds.origin.x + bounds.size.width
                && point.1 >= bounds.origin.y && point.1 < bounds.origin.y + bounds.size.height
        }) {
            Ok(point)
        } else {
            Err(io::Error::new(io::ErrorKind::InvalidInput, format!("point ({x}, {y}) is outside the active displays")))
        }
    }

    fn coordinate_space_key(&self, mut identify: impl FnMut(CGDirectDisplayID) -> Option<[u8; 16]>) -> io::Result<CoordinateSpaceKey> {
        let mut key = Vec::with_capacity(2 + self.displays.len() * 11);
        let mut restorable = true;
        key.extend([self.scale.to_bits(), self.displays.len() as u64]);
        for display in &self.displays {
            let (width, height) = display.native_size()?;
            let identity = identify(display.id).map(u128::from_be_bytes).unwrap_or_default();
            if identity == 0 {
                restorable = false;
            }
            key.extend([
                u64::from(display.id),
                (identity >> 64) as u64,
                identity as u64,
                display.bounds.origin.x.to_bits(),
                display.bounds.origin.y.to_bits(),
                display.bounds.size.width.to_bits(),
                display.bounds.size.height.to_bits(),
                u64::from(width),
                u64::from(height),
                display.rotation.to_bits(),
                u64::from(display.main),
            ]);
        }
        Ok(CoordinateSpaceKey { components: key, restorable })
    }
}

fn display_uuid(id: CGDirectDisplayID) -> Option<[u8; 16]> {
    // SAFETY: ColorSync returns an owned CFUUID on success.
    let uuid = unsafe { CGDisplayCreateUUIDFromDisplayID(id).map(|uuid| CFRetained::from_raw(uuid)) }?;
    Some(uuid_bytes(&uuid))
}

fn uuid_bytes(uuid: &CFUUID) -> [u8; 16] {
    let bytes = uuid.uuid_bytes();
    [
        bytes.byte0, bytes.byte1, bytes.byte2, bytes.byte3,
        bytes.byte4, bytes.byte5, bytes.byte6, bytes.byte7,
        bytes.byte8, bytes.byte9, bytes.byte10, bytes.byte11,
        bytes.byte12, bytes.byte13, bytes.byte14, bytes.byte15,
    ]
}

fn displays() -> Vec<Display> {
    let generation = DISPLAY_GENERATION.load(Ordering::SeqCst);
    let needs_refresh = {
        let work = WORK.lock().unwrap();
        !work.initialized || work.generation != generation
    };
    if needs_refresh && objc2::MainThreadMarker::new().is_some() {
        refresh_work_areas();
    }
    let cached = WORK.lock().unwrap().clone();
    let mut ids = [0u32; 32];
    let mut count = 0u32;
    // SAFETY: the buffer holds 32 IDs; display modes are released.
    unsafe {
        if CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) != 0 {
            return Vec::new();
        }
        let main = CGMainDisplayID();
        ids[..count as usize]
            .iter()
            .map(|&id| {
                let bounds = CGDisplayBounds(id);
                let mode = CGDisplayCopyDisplayMode(id);
                let mut scale = 1.0;
                let mut mode_pixels = None;
                let mut mode_points = None;
                if !mode.is_null() {
                    let (px, height, pt, points_height) = (
                        CGDisplayModeGetPixelWidth(mode),
                        CGDisplayModeGetPixelHeight(mode),
                        CGDisplayModeGetWidth(mode),
                        CGDisplayModeGetHeight(mode),
                    );
                    if pt > 0 {
                        scale = (px as f64 / pt as f64).max(1.0);
                    }
                    mode_pixels = Some((px, height));
                    mode_points = Some((pt as f64, points_height as f64));
                    CGDisplayModeRelease(mode);
                }
                let backing_pixels = cached.find(id, bounds, generation).map(|screen| screen.backing_pixels);
                let mirror = CGDisplayMirrorsDisplay(id);
                Display {
                    id,
                    bounds,
                    scale,
                    mode_pixels,
                    mode_points,
                    backing_pixels,
                    rotation: CGDisplayRotation(id),
                    main: id == main,
                    mirrors: (mirror != 0).then_some(mirror),
                }
            })
            .collect()
    }
}

/// Desktop pixels per point: the largest backing scale of any display.
pub fn scale_factor() -> f64 {
    Layout::current().scale
}

/// Converts a desktop-pixel point to global points (top-left origin).
pub fn to_points(x: f64, y: f64) -> (f64, f64) {
    let s = scale_factor();
    (x / s, y / s)
}

/// Converts global points (top-left origin) to desktop pixels.
pub fn from_points(x: f64, y: f64) -> (i32, i32) {
    let s = scale_factor();
    ((x * s).round() as i32, (y * s).round() as i32)
}

/// A rectangle in global points converted to desktop pixels.
pub fn rect_from_points(r: CGRect) -> Rect {
    rect_from_points_at_scale(r, scale_factor())
}

fn rect_from_points_at_scale(r: CGRect, s: f64) -> Rect {
    let l = (r.origin.x * s).round() as i32;
    let t = (r.origin.y * s).round() as i32;
    let rr = ((r.origin.x + r.size.width) * s).round() as i32;
    let b = ((r.origin.y + r.size.height) * s).round() as i32;
    Rect::from_ltrb(l, t, rr, b)
}

/// A desktop-pixel rectangle in global points (top-left origin).
pub fn rect_to_points(r: Rect) -> CGRect {
    let s = scale_factor();
    rect(r.x as f64 / s, r.y as f64 / s, r.width as f64 / s, r.height as f64 / s)
}

/// The height of the main display in points, for flipping between Quartz
/// (y down) and Cocoa (y up) global coordinates.
pub fn main_height_points() -> f64 {
    // SAFETY: plain queries.
    unsafe { CGDisplayBounds(CGMainDisplayID()).size.height }
}

/// The bounding rectangle of all monitors.
pub fn virtual_screen() -> Rect {
    monitors().iter().map(|m| m.rect).reduce(Rect::union).unwrap_or_default()
}

#[derive(Clone, Debug)]
pub struct Monitor {
    pub rect: Rect,
    pub work: Rect,
    pub dpi: u32,
    pub primary: bool,
    pub device: String,
}

/// A saved coordinate-space fingerprint and whether it can be checked after
/// this process exits. ColorSync may not provide a UUID for every display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoordinateSpaceKey {
    pub components: Vec<u64>,
    pub restorable: bool,
}

impl Monitor {
    /// UI scale relative to 96 DPI.
    pub fn scale(&self) -> f32 {
        self.dpi as f32 / 96.0
    }

    /// Desktop-coordinate pixels per AppKit point.
    pub fn desktop_scale(&self) -> f64 {
        scale_factor()
    }
}

/// Display geometry retained for the duration of a capture operation.
#[derive(Clone, Debug)]
pub struct DisplayLayout {
    layout: Layout,
    monitors: Vec<Monitor>,
}

impl DisplayLayout {
    /// Captures a consistent layout and begins observing display changes.
    pub fn current() -> io::Result<Self> {
        observe_display_changes()?;
        let layout = Layout::current();
        for display in &layout.displays {
            display.density()?;
        }
        let monitors: Vec<_> = layout.displays.iter().map(|display| describe(display, layout.scale)).collect();
        if monitors.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no active displays are available"));
        }
        check_layout(&layout)?;
        Ok(Self { layout, monitors })
    }

    pub fn monitors(&self) -> &[Monitor] {
        &self.monitors
    }

    pub fn virtual_screen(&self) -> Rect {
        self.monitors.iter().map(|monitor| monitor.rect).reduce(Rect::union).unwrap_or_default()
    }

    pub fn desktop_scale(&self) -> f64 {
        self.layout.scale
    }

    /// The bounding rectangle of the retained displays, in Quartz points.
    pub fn logical_virtual_bounds(&self) -> io::Result<LogicalBounds> {
        self.validate()?;
        let bounds = self.layout.logical_virtual_bounds()?;
        self.validate()?;
        Ok(bounds)
    }

    /// Maps a reference-grid point onto an actual display in Quartz coordinates.
    pub fn native_point(&self, x: i32, y: i32) -> io::Result<(f64, f64)> {
        self.validate()?;
        self.layout.native_point(x, y)
    }

    /// Reference-grid geometry and available ColorSync UUIDs, without the
    /// process-local generation. UUIDs can collide or change, so this does not
    /// establish a display's physical identity.
    pub fn coordinate_space_key(&self) -> io::Result<CoordinateSpaceKey> {
        self.validate()?;
        let key = self.layout.coordinate_space_key(display_uuid)?;
        self.validate()?;
        Ok(key)
    }

    /// Maps desktop-space coordinates to native-resolution capture pixels.
    /// A selection spanning displays uses the largest density on each axis
    /// among the displays it intersects.
    pub fn mapping(&self, rect: Rect) -> io::Result<CaptureMapping> {
        self.layout.mapping(rect)
    }

    /// Maps logical bounds using the largest intersecting density per axis.
    pub fn logical_mapping(&self, bounds: LogicalBounds) -> io::Result<LogicalCaptureMapping> {
        self.validate()?;
        let mapping = self.layout.logical_mapping(bounds)?;
        self.validate()?;
        Ok(mapping)
    }

    /// Maps one retained monitor to its exact native pixel dimensions.
    pub fn monitor_mapping(&self, monitor: &Monitor) -> io::Result<CaptureMapping> {
        let display = self.layout.displays.iter()
            .find(|display| monitor.device == format!("display-{}", display.id))
            .ok_or_else(|| io::Error::other("the selected display is no longer available"))?;
        if monitor.rect != rect_from_points_at_scale(display.bounds, self.layout.scale) {
            return Err(io::Error::other("the selected display geometry no longer matches the capture"));
        }
        let (width, height) = display.native_size()?;
        let bounds = display.bounds;
        let scale = self.layout.scale;
        CaptureMapping::with_desktop_bounds(monitor.rect, width, height, [
            bounds.origin.x * scale,
            bounds.origin.y * scale,
            (bounds.origin.x + bounds.size.width) * scale,
            (bounds.origin.y + bounds.size.height) * scale,
        ])
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid monitor dimensions"))
    }

    /// Resolves an integer overlay rectangle to exactly one retained monitor.
    pub fn monitor_mapping_for_rect(&self, rect: Rect) -> io::Result<CaptureMapping> {
        let mut monitors = self.monitors.iter().filter(|monitor| monitor.rect == rect);
        let monitor = monitors.next().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the selected display is no longer available"))?;
        if monitors.next().is_some() {
            return Err(io::Error::other("the selected display rectangle is ambiguous"));
        }
        self.monitor_mapping(monitor)
    }

    /// Reference-grid distance of one native pixel on the display at (or
    /// nearest to) the given point.
    pub fn desktop_pixel_step(&self, x: i32, y: i32) -> io::Result<(f64, f64)> {
        let monitor = self.monitor_at(x, y).ok_or_else(|| io::Error::other("no display is available"))?;
        let display = self.layout.displays.iter().find(|display| monitor.device == format!("display-{}", display.id))
            .ok_or_else(|| io::Error::other("the selected display is no longer available"))?;
        let (sx, sy) = display.density()?;
        Ok((self.layout.scale / sx, self.layout.scale / sy))
    }

    fn display_at_precise(&self, x: f64, y: f64) -> io::Result<&Display> {
        if !x.is_finite() || !y.is_finite() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid desktop point"));
        }
        let point = (x / self.layout.scale, y / self.layout.scale);
        let contains = |display: &&Display| {
            let bounds = display.bounds;
            point.0 >= bounds.origin.x && point.0 < bounds.origin.x + bounds.size.width
                && point.1 >= bounds.origin.y && point.1 < bounds.origin.y + bounds.size.height
        };
        let distance = |display: &Display| {
            let bounds = display.bounds;
            let dx = (bounds.origin.x - point.0).max(0.0).max(point.0 - (bounds.origin.x + bounds.size.width));
            let dy = (bounds.origin.y - point.1).max(0.0).max(point.1 - (bounds.origin.y + bounds.size.height));
            dx.hypot(dy)
        };
        self.layout.displays.iter().find(contains)
            .or_else(|| self.layout.displays.iter().min_by(|a, b| distance(a).total_cmp(&distance(b))))
            .ok_or_else(|| io::Error::other("no display is available"))
    }

    /// Reference-grid distance of one native pixel at an unrounded point.
    pub fn desktop_pixel_step_precise(&self, x: f64, y: f64) -> io::Result<(f64, f64)> {
        let display = self.display_at_precise(x, y)?;
        let (sx, sy) = display.density()?;
        Ok((self.layout.scale / sx, self.layout.scale / sy))
    }

    /// The display containing (or nearest to) an unrounded reference-grid point.
    pub fn monitor_at_precise(&self, x: f64, y: f64) -> io::Result<Monitor> {
        let display = self.display_at_precise(x, y)?;
        self.monitors.iter().find(|monitor| monitor.device == format!("display-{}", display.id))
            .cloned()
            .ok_or_else(|| io::Error::other("the selected display is no longer available"))
    }

    fn monitor_at_points(&self, x: f64, y: f64) -> io::Result<Monitor> {
        self.monitor_at_precise(x * self.layout.scale, y * self.layout.scale)
    }

    fn monitor_under_cursor_with(&self, pointer: impl FnOnce() -> io::Result<(f64, f64)>) -> io::Result<Monitor> {
        self.validate()?;
        let (x, y) = pointer()?;
        let monitor = self.monitor_at_points(x, y)?;
        self.validate()?;
        Ok(monitor)
    }

    /// The display under the current pointer, using this retained layout.
    pub fn monitor_under_cursor(&self) -> io::Result<Monitor> {
        self.monitor_under_cursor_with(try_cursor_pos_points)
    }

    /// Desktop-coordinate extent corresponding to native image pixels on
    /// the display at (or nearest to) the given point.
    pub fn desktop_size_for_pixels(&self, x: i32, y: i32, width: u32, height: u32) -> io::Result<(i32, i32)> {
        let (sx, sy) = self.desktop_pixel_step(x, y)?;
        Ok(Self::size_for_pixel_step((sx, sy), width, height))
    }

    /// Desktop-coordinate extent at an unrounded reference-grid point.
    pub fn desktop_size_for_pixels_precise(&self, x: f64, y: f64, width: u32, height: u32) -> io::Result<(i32, i32)> {
        let step = self.desktop_pixel_step_precise(x, y)?;
        Ok(Self::size_for_pixel_step(step, width, height))
    }

    fn size_for_pixel_step((sx, sy): (f64, f64), width: u32, height: u32) -> (i32, i32) {
        let w = (width as f64 * sx).round().clamp(1.0, i32::MAX as f64) as i32;
        let h = (height as f64 * sy).round().clamp(1.0, i32::MAX as f64) as i32;
        (w, h)
    }

    pub fn monitor_at(&self, x: i32, y: i32) -> Option<Monitor> {
        nearest_monitor(&self.monitors, x, y)
    }

    /// Rejects a layout change, including one subsequently reverted.
    pub fn validate(&self) -> io::Result<()> {
        check_layout(&self.layout)
    }

    /// Grabs using this layout, failing if it is no longer current.
    pub fn grab(&self, rect: Rect) -> io::Result<Frame> {
        grab_with_layout(rect, &self.layout)
    }

    /// Grabs with an explicit output mapping, for example a fixed-size capture.
    pub fn grab_mapped(&self, mapping: CaptureMapping) -> io::Result<Frame> {
        grab_mapped(mapping, &self.layout)
    }

    /// Grabs logical bounds without converting them to the reference grid.
    pub fn grab_logical(&self, mapping: LogicalCaptureMapping) -> io::Result<Frame> {
        grab_logical(mapping, &self.layout)
    }

    /// Freezes every display at its own native resolution before an overlay
    /// is shown, allowing a selection to be composed without recapturing it.
    pub fn freeze(&self) -> io::Result<FrozenDesktop> {
        check_layout(&self.layout)?;
        let (capture, snapshot, content) = content_for_capture()?;
        capture.applied(snapshot.revision);
        let sc_displays = unsafe { content.displays() };
        let mut displays = Vec::with_capacity(self.layout.displays.len());
        for display in &self.layout.displays {
            let native = display.native_size()?;
            let sc_display = sc_displays.iter().find(|candidate| unsafe { candidate.displayID() } == display.id)
                .ok_or_else(|| io::Error::other(format!("display {} is not available for screen capture", display.id)))?;
            let filter = sck::display_filter(&content, &sc_display, &snapshot.windows);
            let source = rect(0.0, 0.0, display.bounds.size.width, display.bounds.size.height);
            let config = sck::config(source, native.0 as usize, native.1 as usize, false);
            let pixels = sck::screenshot(&filter, &config, native.0 as usize, native.1 as usize)?;
            displays.push((*display, Frame { width: native.0, height: native.1, pixels, alpha: false }));
        }
        check_layout(&self.layout)?;
        Ok(FrozenDesktop { layout: self.layout.clone(), displays })
    }
}

/// Native-resolution display images retained from before selection begins.
pub struct FrozenDesktop {
    layout: Layout,
    displays: Vec<(Display, Frame)>,
}

impl FrozenDesktop {
    /// Copies one complete monitor without resampling its native pixels.
    pub fn copy_native_monitor(&self, monitor: Rect, width: u32, height: u32, dst: &mut [u8], stride: usize) -> io::Result<()> {
        check_layout(&self.layout)?;
        self.copy_native_monitor_unchecked(monitor, width, height, dst, stride)?;
        check_layout(&self.layout)
    }

    fn copy_native_monitor_unchecked(&self, monitor: Rect, width: u32, height: u32, dst: &mut [u8], stride: usize) -> io::Result<()> {
        let mut displays = self.displays.iter()
            .filter(|(display, _)| rect_from_points_at_scale(display.bounds, self.layout.scale) == monitor);
        let frame = displays.next().map(|(_, frame)| frame)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the frozen monitor is no longer available"))?;
        if displays.next().is_some() {
            return Err(io::Error::other("the frozen monitor rectangle is ambiguous"));
        }
        if (width, height) != (frame.width, frame.height) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "the overlay dimensions do not match the frozen monitor"));
        }
        let row = frame.stride();
        let required = stride.checked_mul(height as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid overlay buffer size"))?;
        if stride < row || dst.len() < required {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "the overlay buffer is too small"));
        }
        for y in 0..height as usize {
            dst[y * stride..y * stride + row].copy_from_slice(&frame.pixels[y * row..(y + 1) * row]);
        }
        Ok(())
    }

    /// The native pixel containing a desktop-grid point, or none in a gap.
    pub fn color_at(&self, x: i32, y: i32) -> Option<(u8, u8, u8)> {
        self.color_at_precise(f64::from(x), f64::from(y))
    }

    /// The native pixel containing an unrounded desktop-grid point, or none in a gap.
    pub fn color_at_precise(&self, x: f64, y: f64) -> Option<(u8, u8, u8)> {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        let (x, y) = (x / self.layout.scale, y / self.layout.scale);
        self.displays.iter().find_map(|(display, frame)| {
            let bounds = display.bounds;
            if x < bounds.origin.x || y < bounds.origin.y
                || x >= bounds.origin.x + bounds.size.width || y >= bounds.origin.y + bounds.size.height
            {
                return None;
            }
            let px = ((x - bounds.origin.x) * frame.width as f64 / bounds.size.width).floor() as i32;
            let py = ((y - bounds.origin.y) * frame.height as f64 / bounds.size.height).floor() as i32;
            frame.rgb(px, py)
        })
    }

    pub fn render(&self, mapping: CaptureMapping) -> io::Result<Frame> {
        check_layout(&self.layout)?;
        let frame = self.render_frozen(mapping)?;
        check_layout(&self.layout)?;
        Ok(frame)
    }

    /// Composes a frozen capture from precise logical bounds.
    pub fn render_logical(&self, mapping: LogicalCaptureMapping) -> io::Result<Frame> {
        check_layout(&self.layout)?;
        let frame = self.render_logical_unchecked(mapping)?;
        check_layout(&self.layout)?;
        Ok(frame)
    }

    fn render_frozen(&self, mapping: CaptureMapping) -> io::Result<Frame> {
        let mapping = LogicalCaptureMapping::from_capture(mapping, self.layout.scale)?;
        self.render_logical_unchecked(mapping)
    }

    fn render_logical_unchecked(&self, mapping: LogicalCaptureMapping) -> io::Result<Frame> {
        let (width, height) = mapping.size;
        let mut out = Frame::new(width, height);
        let mut intersected = false;
        for (display, source) in &self.displays {
            if mapping.bounds.intersect(display).is_none() {
                continue;
            }
            intersected = true;
            let Some(piece) = CapturePiece::new(display, mapping)? else { continue };
            piece.composite(&mut out, &source.pixels, (source.width, source.height), (0, 0))?;
        }
        if !intersected {
            return Err(io::Error::new(io::ErrorKind::NotFound, "the capture rectangle does not intersect an available display"));
        }
        for pixel in out.pixels.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        Ok(out)
    }
}

#[derive(Clone, Copy)]
struct AppKitScreen {
    id: Option<CGDirectDisplayID>,
    frame: CGRect,
    work: CGRect,
    backing_pixels: (f64, f64),
}

#[derive(Clone)]
struct AppKitScreens {
    initialized: bool,
    generation: u64,
    screens: Vec<AppKitScreen>,
}

impl AppKitScreens {
    fn find(&self, id: CGDirectDisplayID, bounds: CGRect, generation: u64) -> Option<&AppKitScreen> {
        (self.initialized && self.generation == generation)
            .then(|| self.screens.iter().find(|screen| screen.id == Some(id) && same_frame(screen.frame, bounds)))
            .flatten()
    }
}

/// AppKit display metrics, refreshed on the main thread.
static WORK: Mutex<AppKitScreens> = Mutex::new(AppKitScreens { initialized: false, generation: 0, screens: Vec::new() });

/// Reads the work areas from AppKit. Call on the main thread at startup and
/// when displays change; other threads use the cached values.
pub fn refresh_work_areas() {
    use objc2_foundation::{NSNumber, NSString};

    let Some(mtm) = objc2::MainThreadMarker::new() else { return };
    let generation = DISPLAY_GENERATION.load(Ordering::SeqCst);
    let flip = main_height_points();
    let to_quartz = |r: CGRect| rect(r.origin.x, flip - r.origin.y - r.size.height, r.size.width, r.size.height);
    let screens = objc2_app_kit::NSScreen::screens(mtm).iter().map(|screen| {
        let frame = screen.frame();
        let backing = screen.convertRectToBacking(frame);
        let id = screen.deviceDescription().objectForKey(&NSString::from_str("NSScreenNumber"))
            .and_then(|value| value.downcast::<NSNumber>().ok())
            .map(|number| number.unsignedIntValue())
            .filter(|&id| id != 0);
        if id.is_none() {
            eprintln!("could not identify an AppKit display for capture geometry");
        }
        AppKitScreen {
            id,
            frame: to_quartz(frame),
            work: to_quartz(screen.visibleFrame()),
            backing_pixels: (backing.size.width, backing.size.height),
        }
    }).collect();
    if DISPLAY_GENERATION.load(Ordering::SeqCst) == generation {
        *WORK.lock().unwrap() = AppKitScreens { initialized: true, generation, screens };
    }
}

/// Keeps cached work areas current when the display arrangement changes.
/// Call once on the AppKit main thread after the application starts.
pub fn watch_displays() -> io::Result<()> {
    if objc2::MainThreadMarker::new().is_none() {
        return Err(io::Error::other("display observation must start on the main thread"));
    }
    observe_display_changes()?;
    refresh_work_areas();
    Ok(())
}

fn observe_display_changes() -> io::Result<()> {
    static OBSERVER: OnceLock<i32> = OnceLock::new();
    let status = *OBSERVER.get_or_init(|| {
        // SAFETY: the callback and its null context remain valid for the process.
        unsafe { CGDisplayRegisterReconfigurationCallback(Some(display_changed), std::ptr::null_mut()) }
    });
    if status == 0 { Ok(()) } else { Err(io::Error::other(format!("could not observe display changes (Core Graphics status {status})"))) }
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGDisplayRegisterReconfigurationCallback(
        callback: Option<unsafe extern "C" fn(u32, u32, *mut std::ffi::c_void)>,
        context: *mut std::ffi::c_void,
    ) -> i32;
}

unsafe extern "C" fn display_changed(_display: u32, flags: u32, _context: *mut std::ffi::c_void) {
    const BEGIN_CONFIGURATION: u32 = 1;
    DISPLAY_GENERATION.fetch_add(1, Ordering::SeqCst);
    if flags & BEGIN_CONFIGURATION == 0 {
        dispatch2::DispatchQueue::main().exec_async(refresh_work_areas);
    }
}

fn same_frame(a: CGRect, b: CGRect) -> bool {
    (a.origin.x - b.origin.x).abs() < 1.0
        && (a.origin.y - b.origin.y).abs() < 1.0
        && (a.size.width - b.size.width).abs() < 1.0
        && (a.size.height - b.size.height).abs() < 1.0
}

fn work_area(d: &Display) -> CGRect {
    let work = WORK.lock().unwrap();
    if let Some(screen) = work.find(d.id, d.bounds, DISPLAY_GENERATION.load(Ordering::SeqCst)) {
        return screen.work;
    }
    drop(work);
    // Before AppKit was asked: leave room for the menu bar on the main
    // display.
    let menu = if d.main { 25.0 } else { 0.0 };
    rect(d.bounds.origin.x, d.bounds.origin.y + menu, d.bounds.size.width, d.bounds.size.height - menu)
}

fn describe(d: &Display, scale: f64) -> Monitor {
    Monitor {
        rect: rect_from_points_at_scale(d.bounds, scale),
        work: rect_from_points_at_scale(work_area(d), scale),
        dpi: (96.0 * d.scale).round() as u32,
        primary: d.main,
        device: format!("display-{}", d.id),
    }
}

pub fn monitors() -> Vec<Monitor> {
    let layout = Layout::current();
    layout.displays.iter().map(|display| describe(display, layout.scale)).collect()
}

/// The monitor containing (or nearest to) a screen point.
pub fn monitor_at(x: i32, y: i32) -> Option<Monitor> {
    nearest_monitor(&monitors(), x, y)
}

fn nearest_monitor(monitors: &[Monitor], x: i32, y: i32) -> Option<Monitor> {
    let distance = |m: &Monitor| {
        let r = m.rect;
        let (x, y) = (i128::from(x), i128::from(y));
        let (left, top) = (i128::from(r.x), i128::from(r.y));
        let dx = (left - x).max(0).max(x - (left + i128::from(r.width) - 1));
        let dy = (top - y).max(0).max(y - (top + i128::from(r.height) - 1));
        dx * dx + dy * dy
    };
    monitors.iter().min_by_key(|monitor| distance(monitor)).cloned()
}

#[derive(Clone, Default)]
struct Exclusions {
    revision: u64,
    windows: Vec<u32>,
    required: Vec<u32>,
}

impl Exclusions {
    fn set(&mut self, number: u32, excluded: bool, required: bool) {
        debug_assert!(!required || excluded);
        if self.windows.contains(&number) == excluded && self.required.contains(&number) == required {
            return;
        }
        if excluded {
            if !self.windows.contains(&number) {
                self.windows.push(number);
            }
        } else {
            self.windows.retain(|&window| window != number);
        }
        if required {
            if !self.required.contains(&number) {
                self.required.push(number);
            }
        } else {
            self.required.retain(|&window| window != number);
        }
        self.revision = self.revision.wrapping_add(1);
    }
}

#[derive(Default)]
struct RetiredExclusion {
    number: u32,
    captures: Vec<u64>,
}

#[derive(Default)]
struct ExclusionRegistry {
    exclusions: Exclusions,
    captures: Vec<(u64, u64)>,
    tickets: Vec<ExclusionTicket>,
    manual: Vec<u32>,
    retired: Vec<RetiredExclusion>,
    next_ticket: u64,
}

impl ExclusionRegistry {
    fn refresh(&mut self, number: u32) {
        let required = self.manual.contains(&number) || self.tickets.iter().any(|ticket| ticket.number == number);
        let retired = self.retired.iter().any(|retired| retired.number == number);
        self.exclusions.set(number, required || retired, required);
    }

    fn register(&mut self, number: u32) -> ExclusionTicket {
        self.next_ticket = self.next_ticket.wrapping_add(1);
        let ticket = ExclusionTicket { number, generation: self.next_ticket };
        self.tickets.push(ticket);
        let revision = self.exclusions.revision;
        self.refresh(number);
        if self.exclusions.revision == revision {
            // A recycled number may now name a different SCWindow.
            self.exclusions.revision = self.exclusions.revision.wrapping_add(1);
        }
        ticket
    }

    fn set(&mut self, number: u32, excluded: bool) {
        if excluded {
            if !self.manual.contains(&number) {
                self.manual.push(number);
            }
        } else {
            self.manual.retain(|&window| window != number);
        }
        self.refresh(number);
    }

    fn retire(&mut self, ticket: ExclusionTicket) -> bool {
        let Some(index) = self.tickets.iter().position(|&current| current == ticket) else {
            return false;
        };
        self.tickets.swap_remove(index);
        if !self.captures.is_empty() {
            let retired = match self.retired.iter_mut().find(|retired| retired.number == ticket.number) {
                Some(retired) => retired,
                None => {
                    self.retired.push(RetiredExclusion { number: ticket.number, captures: Vec::new() });
                    self.retired.last_mut().expect("the retired exclusion was just added")
                }
            };
            for &(id, _) in &self.captures {
                if !retired.captures.contains(&id) {
                    retired.captures.push(id);
                }
            }
        }
        self.refresh(ticket.number);
        true
    }

    fn finish_capture(&mut self, id: u64) {
        self.captures.retain(|&(capture, _)| capture != id);
        let mut released = Vec::new();
        self.retired.retain_mut(|retired| {
            retired.captures.retain(|&capture| capture != id);
            if retired.captures.is_empty() {
                released.push(retired.number);
                false
            } else {
                true
            }
        });
        for number in released {
            self.refresh(number);
        }
    }

    fn ready(&self, number: u32) -> bool {
        self.exclusions.required.contains(&number)
            && self.captures.iter().all(|(_, revision)| *revision == self.exclusions.revision)
    }
}

/// Identifies one registration of a native window number.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExclusionTicket {
    number: u32,
    generation: u64,
}

/// Window numbers left out of captures, and each active capture's applied
/// exclusion revision.
static EXCLUDED: Mutex<ExclusionRegistry> = Mutex::new(ExclusionRegistry {
    exclusions: Exclusions { revision: 0, windows: Vec::new(), required: Vec::new() },
    captures: Vec::new(),
    tickets: Vec::new(),
    manual: Vec::new(),
    retired: Vec::new(),
    next_ticket: 0,
});
static NEXT_CAPTURE_ID: AtomicU64 = AtomicU64::new(1);

/// Includes or leaves out a window (its `NSWindow.windowNumber`) in grabs.
pub fn exclude_window(number: u32, excluded: bool) {
    EXCLUDED.lock().unwrap().set(number, excluded);
}

/// Registers one owner of a native window number. Each owner must retire its
/// own ticket after hiding or closing that window.
pub fn register_excluded_window(number: u32) -> ExclusionTicket {
    EXCLUDED.lock().unwrap().register(number)
}

/// Whether this owner's registration is still active.
pub fn is_exclusion_registered(ticket: ExclusionTicket) -> bool {
    EXCLUDED.lock().unwrap().tickets.contains(&ticket)
}

/// Retires this owner without disturbing other owners of the same number.
/// The number remains conservatively excluded until captures active at
/// retirement finish; a recycled number may temporarily exclude another window.
pub fn retire_exclusion(ticket: ExclusionTicket) -> bool {
    EXCLUDED.lock().unwrap().retire(ticket)
}

pub fn is_excluded_window(number: u32) -> bool {
    EXCLUDED.lock().unwrap().exclusions.windows.contains(&number)
}

/// Whether all active capture operations have applied the current exclusions.
pub fn exclusion_ready(number: u32) -> bool {
    EXCLUDED.lock().unwrap().ready(number)
}

fn exclusion_snapshot() -> Exclusions {
    EXCLUDED.lock().unwrap().exclusions.clone()
}

struct CaptureRegistration {
    id: u64,
}

impl CaptureRegistration {
    fn begin() -> (Self, Exclusions) {
        let id = NEXT_CAPTURE_ID.fetch_add(1, Ordering::Relaxed);
        let mut registry = EXCLUDED.lock().unwrap();
        let snapshot = registry.exclusions.clone();
        registry.captures.push((id, snapshot.revision));
        (Self { id }, snapshot)
    }

    fn applied(&self, revision: u64) {
        if let Some((_, applied)) = EXCLUDED.lock().unwrap().captures.iter_mut().find(|(id, _)| *id == self.id) {
            *applied = revision;
        }
    }
}

impl Drop for CaptureRegistration {
    fn drop(&mut self) {
        EXCLUDED.lock().unwrap().finish_capture(self.id);
    }
}

fn content_for_exclusions(snapshot: &Exclusions) -> io::Result<Option<Retained<SCShareableContent>>> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if exclusion_snapshot().revision != snapshot.revision {
            return Ok(None);
        }
        let content = sck::shareable_content_including_hidden()?;
        if exclusion_snapshot().revision != snapshot.revision {
            return Ok(None);
        }
        // SAFETY: the retained shareable-content snapshot is live.
        let available: Vec<u32> = unsafe { content.windows().iter().map(|window| window.windowID()).collect() };
        let missing: Vec<u32> = snapshot.required.iter().copied().filter(|number| !available.contains(number)).collect();
        if missing.is_empty() {
            return Ok(Some(content));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("could not apply capture exclusions for windows {missing:?}")));
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

fn content_for_capture() -> io::Result<(CaptureRegistration, Exclusions, Retained<SCShareableContent>)> {
    let (capture, mut snapshot) = CaptureRegistration::begin();
    loop {
        if let Some(content) = content_for_exclusions(&snapshot)? {
            return Ok((capture, snapshot, content));
        }
        snapshot = exclusion_snapshot();
    }
}

/// Projects the display's logical intersection to output pixels. A pixel
/// whose center lies on a shared edge belongs to the display starting there.
fn piece(display: &Display, mapping: LogicalCaptureMapping) -> Option<(CGRect, Rect)> {
    let [on_left, on_top, on_right, on_bottom] = mapping.bounds.intersect(display)?.0;
    let [left, top, right, bottom] = mapping.bounds.0;
    let (width, height) = mapping.size;
    let edge = LogicalCaptureMapping::first_pixel_at_or_after;
    let at = Rect::from_ltrb(
        edge(on_left, left, right, width),
        edge(on_top, top, bottom, height),
        edge(on_right, left, right, width),
        edge(on_bottom, top, bottom, height),
    );
    let source = rect(
        on_left - display.bounds.origin.x,
        on_top - display.bounds.origin.y,
        on_right - on_left,
        on_bottom - on_top,
    );
    (!at.is_empty()).then_some((source, at))
}

/// Native pixels needed to sample one display into a capture. Still images,
/// streams and frozen selections use the same pixel-center mapping.
struct CapturePiece {
    at: Rect,
    source: CGRect,
    origin: (usize, usize),
    size: (u32, u32),
    columns: Vec<usize>,
    rows: Vec<usize>,
}

fn aligned_native_crop(first: usize, last: usize, pixels: u32, points: f64) -> (usize, usize, f64, f64) {
    let full = (0, pixels as usize, 0.0, points);
    if points.fract() != 0.0 || points > u32::MAX as f64 {
        return full;
    }
    let mut a = pixels;
    let mut b = points as u32;
    while b != 0 {
        (a, b) = (b, a % b);
    }
    if a == 0 {
        return full;
    }
    let pixel_step = (pixels / a) as usize;
    let point_step = (points as u32 / a) as usize;
    let start = first / pixel_step * pixel_step;
    let end = (last / pixel_step + 1) * pixel_step;
    (
        start,
        end - start,
        (start / pixel_step * point_step) as f64,
        ((end - start) / pixel_step * point_step) as f64,
    )
}

impl CapturePiece {
    fn new(display: &Display, mapping: LogicalCaptureMapping) -> io::Result<Option<Self>> {
        let Some((_, at)) = piece(display, mapping) else { return Ok(None) };
        let (width, height) = display.native_size()?;
        let bounds = display.bounds;
        let columns: Vec<_> = (at.x..at.right()).map(|x| {
            let point = mapping.pixel_center(x as u32, 0).0;
            ((point - bounds.origin.x) * width as f64 / bounds.size.width)
                .floor().clamp(0.0, width.saturating_sub(1) as f64) as usize
        }).collect();
        let rows: Vec<_> = (at.y..at.bottom()).map(|y| {
            let point = mapping.pixel_center(0, y as u32).1;
            ((point - bounds.origin.y) * height as f64 / bounds.size.height)
                .floor().clamp(0.0, height.saturating_sub(1) as f64) as usize
        }).collect();
        // ScreenCaptureKit may resample crops with fractional point edges.
        // Align both point and native-pixel boundaries; fall back to the full
        // axis when a fractional display extent cannot be aligned safely.
        let (x, w, sx, sw) = aligned_native_crop(columns[0], columns[columns.len() - 1], width, bounds.size.width);
        let (y, h, sy, sh) = aligned_native_crop(rows[0], rows[rows.len() - 1], height, bounds.size.height);
        let source = rect(sx, sy, sw, sh);
        Ok(Some(Self { at, source, origin: (x, y), size: (w as u32, h as u32), columns, rows }))
    }

    fn composite(&self, out: &mut Frame, pixels: &[u8], size: (u32, u32), origin: (usize, usize)) -> io::Result<()> {
        let (width, height) = (size.0 as usize, size.1 as usize);
        let required = width.checked_mul(height).and_then(|n| n.checked_mul(4))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid captured display dimensions"))?;
        let right = origin.0.checked_add(width).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid display crop"))?;
        let bottom = origin.1.checked_add(height).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid display crop"))?;
        if pixels.len() != required || self.columns[0] < origin.0 || self.columns[self.columns.len() - 1] >= right
            || self.rows[0] < origin.1 || self.rows[self.rows.len() - 1] >= bottom
            || self.at.intersect(out.bounds()) != Some(self.at)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "captured display pixels do not match their mapping"));
        }
        let contiguous = self.columns.windows(2).all(|pair| pair[1] == pair[0] + 1);
        let output_stride = out.stride();
        for (y, &row) in self.rows.iter().enumerate() {
            let source_row = (row - origin.1) * width * 4;
            let output_row = (self.at.y as usize + y) * output_stride + self.at.x as usize * 4;
            if contiguous {
                let start = source_row + (self.columns[0] - origin.0) * 4;
                let len = self.columns.len() * 4;
                out.pixels[output_row..output_row + len].copy_from_slice(&pixels[start..start + len]);
            } else {
                for (x, &column) in self.columns.iter().enumerate() {
                    let src = source_row + (column - origin.0) * 4;
                    let dst = output_row + x * 4;
                    out.pixels[dst..dst + 4].copy_from_slice(&pixels[src..src + 4]);
                }
            }
        }
        Ok(())
    }
}

/// Copies a rectangle of the screen (desktop pixels) into a frame. Parts
/// outside every display stay black.
pub fn grab(r: Rect) -> io::Result<Frame> {
    observe_display_changes()?;
    grab_with_layout(r, &Layout::current())
}

fn grab_with_layout(r: Rect, layout: &Layout) -> io::Result<Frame> {
    let mapping = layout.mapping(r)?;
    grab_mapped(mapping, layout)
}

fn grab_mapped(mapping: CaptureMapping, layout: &Layout) -> io::Result<Frame> {
    let mapping = LogicalCaptureMapping::from_capture(mapping, layout.scale)?;
    grab_logical(mapping, layout)
}

fn grab_logical(mapping: LogicalCaptureMapping, layout: &Layout) -> io::Result<Frame> {
    check_layout(layout)?;
    let (capture, snapshot, content) = content_for_capture()?;
    capture.applied(snapshot.revision);
    let (width, height) = mapping.size;
    let mut frame = Frame::new(width, height);
    let mut intersected = false;
    // SAFETY: property reads on the shareable content.
    let sc_displays = unsafe { content.displays() };
    for d in &layout.displays {
        if mapping.bounds.intersect(d).is_none() {
            continue;
        }
        intersected = true;
        let Some(piece) = CapturePiece::new(d, mapping)? else { continue };
        // SAFETY: as above.
        let display = sc_displays.iter().find(|s| unsafe { s.displayID() } == d.id)
            .ok_or_else(|| io::Error::other(format!("display {} is not available for screen capture", d.id)))?;
        let filter = sck::display_filter(&content, &display, &snapshot.windows);
        let (width, height) = piece.size;
        let config = sck::config(piece.source, width as usize, height as usize, false);
        let pixels = sck::screenshot(&filter, &config, width as usize, height as usize)?;
        piece.composite(&mut frame, &pixels, piece.size, piece.origin)?;
    }
    if !intersected {
        return Err(io::Error::new(io::ErrorKind::NotFound, "the capture rectangle does not intersect an available display"));
    }
    check_layout(layout)?;
    for p in frame.pixels.chunks_exact_mut(4) {
        p[3] = 255;
    }
    Ok(frame)
}

/// Captures one visible window by its WindowServer number, independent of
/// other windows covering it.
pub fn grab_window(number: u32) -> io::Result<Frame> {
    use objc2::AllocAnyThread;
    use objc2_screen_capture_kit::SCContentFilter;

    observe_display_changes()?;
    let layout = Layout::current();
    let content = sck::shareable_content()?;
    // SAFETY: immutable properties of the shareable content and window.
    let window = unsafe { content.windows() }
        .iter()
        .find(|window| unsafe { window.windowID() } == number)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the window is no longer available for capture"))?;
    let bounds = unsafe { window.frame() };
    let mapping = layout.logical_mapping(LogicalBounds::from_rect(bounds)?)?;
    let (width, height) = mapping.size();
    let (width, height) = (width as usize, height as usize);
    // SAFETY: the window is retained for the filter initializer.
    let filter = unsafe { SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window) };
    let config = sck::config(rect(0.0, 0.0, bounds.size.width, bounds.size.height), width, height, false);
    let mut frame = Frame { width: width as u32, height: height as u32, pixels: sck::screenshot(&filter, &config, width, height)?, alpha: false };
    check_layout(&layout)?;
    for pixel in frame.pixels.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
    Ok(frame)
}

/// Repeated grabs of one rectangle (screen recording): a ScreenCaptureKit
/// stream per display the rectangle covers, each keeping its latest frame.
pub struct Grabber {
    size: (u32, u32),
    frame: Vec<u8>,
    parts: Vec<CapturePart>,
    layout: Layout,
    checked_layout: Instant,
    registration: CaptureRegistration,
    exclusion_revision: u64,
}

struct CapturePart {
    display_id: CGDirectDisplayID,
    piece: CapturePiece,
    latest: Arc<(Mutex<LatestFrame>, Condvar)>,
    stream: sck::Stream,
    seen: bool,
}

#[derive(Default)]
struct LatestFrame {
    pixels: Option<Vec<u8>>,
    failure: Option<String>,
    after: Option<CMTime>,
}

fn frame_after(time: CMTime, cutoff: CMTime) -> io::Result<bool> {
    let valid = |time: CMTime| {
        time.flags.contains(CMTimeFlags::Valid)
            && !time.flags.intersects(CMTimeFlags::ImpliedValueFlagsMask)
            && time.timescale > 0
            && time.epoch == 0
    };
    if !valid(time) || !valid(cutoff) {
        return Err(io::Error::other("screen recording frame has an invalid timestamp"));
    }
    Ok(i128::from(time.value) * i128::from(cutoff.timescale)
        >= i128::from(cutoff.value) * i128::from(time.timescale))
}

fn next_frame(latest: &(Mutex<LatestFrame>, Condvar), seen: bool, timeout: Duration) -> io::Result<Option<Vec<u8>>> {
    let mut slot = latest.0.lock().unwrap();
    if slot.pixels.is_none() && slot.failure.is_none() && !seen {
        slot = latest.1.wait_timeout_while(slot, timeout, |frame| frame.pixels.is_none() && frame.failure.is_none()).unwrap().0;
    }
    if let Some(failure) = &slot.failure {
        return Err(io::Error::other(failure.clone()));
    }
    match slot.pixels.take() {
        Some(pixels) => Ok(Some(pixels)),
        None if seen => Ok(None),
        None => Err(io::Error::new(io::ErrorKind::TimedOut, "no screen recording frame arrived")),
    }
}

fn ensure_fresh_frame(
    latest: &(Mutex<LatestFrame>, Condvar),
    snapshot: impl FnOnce() -> io::Result<Vec<u8>>,
) -> io::Result<()> {
    let slot = latest.0.lock().unwrap();
    let slot = latest.1.wait_timeout_while(slot, Duration::from_millis(150), |frame| {
        frame.pixels.is_none() && frame.failure.is_none()
    }).unwrap().0;
    if let Some(failure) = &slot.failure {
        return Err(io::Error::other(failure.clone()));
    }
    if slot.pixels.is_some() {
        return Ok(());
    }
    drop(slot);
    let pixels = snapshot()?;
    let mut slot = latest.0.lock().unwrap();
    if let Some(failure) = &slot.failure {
        return Err(io::Error::other(failure.clone()));
    }
    if slot.pixels.is_none() {
        slot.pixels = Some(pixels);
        latest.1.notify_all();
    }
    Ok(())
}

impl Grabber {
    pub fn new(rect: Rect) -> io::Result<Grabber> {
        observe_display_changes()?;
        Self::new_in_layout(rect, Layout::current())
    }

    /// Starts recording with the same geometry used for a prior selection.
    pub fn for_layout(rect: Rect, layout: &DisplayLayout) -> io::Result<Grabber> {
        Self::new_in_layout(rect, layout.layout.clone())
    }

    /// Starts recording with explicit image dimensions and retained geometry.
    pub fn for_mapping(mapping: CaptureMapping, layout: &DisplayLayout) -> io::Result<Grabber> {
        Self::new_mapped(mapping, layout.layout.clone())
    }

    /// Starts recording precise logical bounds with explicit output dimensions.
    pub fn for_logical(mapping: LogicalCaptureMapping, layout: &DisplayLayout) -> io::Result<Grabber> {
        Self::new_logical(mapping, layout.layout.clone())
    }

    fn new_in_layout(rect: Rect, layout: Layout) -> io::Result<Grabber> {
        let mapping = layout.mapping(rect)?;
        Self::new_mapped(mapping, layout)
    }

    fn new_mapped(mapping: CaptureMapping, layout: Layout) -> io::Result<Grabber> {
        let mapping = LogicalCaptureMapping::from_capture(mapping, layout.scale)?;
        Self::new_logical(mapping, layout)
    }

    fn new_logical(mapping: LogicalCaptureMapping, layout: Layout) -> io::Result<Grabber> {
        check_layout(&layout)?;
        let (registration, snapshot, content) = content_for_capture()?;
        // SAFETY: property reads on the shareable content.
        let sc_displays = unsafe { content.displays() };
        let mut parts = Vec::new();
        let mut intersected = false;
        for d in &layout.displays {
            if mapping.bounds.intersect(d).is_none() {
                continue;
            }
            intersected = true;
            let Some(piece) = CapturePiece::new(d, mapping)? else { continue };
            // SAFETY: as above.
            let display = sc_displays.iter().find(|s| unsafe { s.displayID() } == d.id)
                .ok_or_else(|| io::Error::other(format!("display {} is not available for screen capture", d.id)))?;
            let filter = sck::display_filter(&content, &display, &snapshot.windows);
            let (w, h) = (piece.size.0 as usize, piece.size.1 as usize);
            let config = sck::config(piece.source, w, h, false);
            // SAFETY: setters on a fresh configuration.
            unsafe {
                config.setMinimumFrameInterval(sck::frame_interval(60));
                config.setQueueDepth(4);
            }
            let latest = Arc::new((Mutex::new(LatestFrame::default()), Condvar::new()));
            let sink = latest.clone();
            let stream = sck::Stream::start(&filter, &config, &[objc2_screen_capture_kit::SCStreamOutputType::Screen], move |_, sample, _| {
                let mut out = Vec::new();
                let result = match sck::screen_frame_bgra(sample, &mut out) {
                    Ok(Some(size)) if size == (w, h) => Some(Ok(out)),
                    Ok(Some((width, height))) => Some(Err(format!("screen recording frame is {width}x{height}, expected {w}x{h}"))),
                    Ok(None) => None,
                    Err(error) => Some(Err(error.to_string())),
                };
                if let Some(result) = result {
                    let mut frame = sink.0.lock().unwrap();
                    match result {
                        Ok(pixels) if frame.failure.is_none() => {
                            if let Some(cutoff) = frame.after {
                                // SAFETY: the sample is live for this callback.
                                let timestamp = unsafe { sample.presentation_time_stamp() };
                                match frame_after(timestamp, cutoff) {
                                    Ok(true) => {}
                                    Ok(false) => return,
                                    Err(error) => {
                                        frame.failure = Some(error.to_string());
                                        sink.1.notify_all();
                                        return;
                                    }
                                }
                            }
                            frame.pixels = Some(pixels);
                        }
                        Ok(_) => {}
                        Err(error) => frame.failure = Some(error),
                    }
                    sink.1.notify_all();
                }
            })?;
            parts.push(CapturePart { display_id: d.id, piece, latest, stream, seen: false });
        }
        if !intersected {
            return Err(io::Error::new(io::ErrorKind::NotFound, "the capture rectangle does not intersect an available display"));
        }
        check_layout(&layout)?;
        let (width, height) = mapping.size;
        let mut frame = vec![0; width as usize * height as usize * 4];
        for pixel in frame.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        registration.applied(snapshot.revision);
        Ok(Grabber {
            size: mapping.size,
            frame,
            parts,
            layout,
            checked_layout: Instant::now(),
            registration,
            exclusion_revision: snapshot.revision,
        })
    }

    /// Updates filters when a capture-excluded window is added or removed.
    pub fn refresh_exclusions(&mut self) -> io::Result<()> {
        loop {
            let snapshot = exclusion_snapshot();
            if snapshot.revision == self.exclusion_revision {
                return Ok(());
            }
            check_layout(&self.layout)?;
            let Some(content) = content_for_exclusions(&snapshot)? else { continue };
            // SAFETY: the retained shareable-content snapshot is live.
            let displays = unsafe { content.displays() };
            for part in &mut self.parts {
                let display = displays.iter().find(|display| unsafe { display.displayID() } == part.display_id)
                    .ok_or_else(|| io::Error::other(format!("display {} is not available for screen capture", part.display_id)))?;
                let filter = sck::display_filter(&content, &display, &snapshot.windows);
                part.stream.update_filter(&filter).map_err(|error| io::Error::other(format!("could not update recording exclusions: {error}")))?;
                // Discard frames captured before the new filter was applied.
                let cutoff = unsafe { CMClock::host_time_clock().time() };
                {
                    let mut latest = part.latest.0.lock().unwrap();
                    latest.pixels = None;
                    latest.after = Some(cutoff);
                }
                part.seen = false;
                let source = part.piece.source;
                let (width, height) = (part.piece.size.0 as usize, part.piece.size.1 as usize);
                // An unchanged desktop may never produce a complete stream frame
                // after the filter update. Capture a fresh image with that same
                // filter rather than reusing a potentially unfiltered frame.
                ensure_fresh_frame(&part.latest, || {
                    let config = sck::config(source, width, height, false);
                    sck::screenshot(&filter, &config, width, height)
                })?;
            }
            check_layout(&self.layout)?;
            self.exclusion_revision = snapshot.revision;
            self.registration.applied(snapshot.revision);
        }
    }

    /// The latest pixels of the rectangle (top-down BGRX). The first call
    /// waits briefly for the streams' first frames.
    pub fn grab(&mut self) -> io::Result<&[u8]> {
        loop {
            if self.checked_layout.elapsed() >= Duration::from_millis(250) {
                check_layout(&self.layout)?;
                self.checked_layout = Instant::now();
            }
            self.refresh_exclusions()?;
            let revision = self.exclusion_revision;
            let (width, height) = self.size;
            let mut frame = Frame { width, height, pixels: std::mem::take(&mut self.frame), alpha: false };
            for part in &mut self.parts {
                if let Some(error) = part.stream.failure() {
                    self.frame = frame.pixels;
                    return Err(io::Error::other(format!("screen recording stopped: {error}")));
                }
                match next_frame(&part.latest, part.seen, Duration::from_secs(2)) {
                    Ok(Some(pixels)) => {
                        if let Err(error) = part.piece.composite(&mut frame, &pixels, part.piece.size, part.piece.origin) {
                            self.frame = frame.pixels;
                            return Err(error);
                        }
                        part.seen = true;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.frame = frame.pixels;
                        return Err(match part.stream.failure() {
                            Some(failure) => io::Error::other(format!("screen recording stopped: {failure}")),
                            None => error,
                        });
                    }
                }
            }
            self.frame = frame.pixels;
            if exclusion_snapshot().revision == revision {
                return Ok(&self.frame);
            }
        }
    }
}

/// The visible mouse cursor at one moment, kept so it can be composited onto
/// a capture taken from a frozen snapshot.
pub struct CursorSnapshot {
    /// Straight-alpha BGRA at desktop-pixel size.
    image: Frame,
    /// Rounded top-left of the cursor image on the compatibility grid.
    pub x: i32,
    pub y: i32,
    offset: (f64, f64),
    extent: (f64, f64),
}

/// The mouse position in global Quartz points, without reference-grid rounding.
pub fn try_cursor_pos_points() -> io::Result<(f64, f64)> {
    // SAFETY: a null-source event only carries the current location.
    unsafe {
        let event = CGEventCreate(std::ptr::null_mut());
        if event.is_null() {
            return Err(io::Error::other("could not query the pointer position"));
        }
        let p = CGEventGetLocation(event);
        CFRelease(event as CFTypeRef);
        if !p.x.is_finite() || !p.y.is_finite() {
            return Err(io::Error::other("the pointer position is invalid"));
        }
        Ok((p.x, p.y))
    }
}

/// The mouse position in desktop pixels, or an error if it cannot be queried.
pub fn try_cursor_pos() -> io::Result<(i32, i32)> {
    let (x, y) = try_cursor_pos_points()?;
    Ok(from_points(x, y))
}

/// The mouse position in desktop pixels.
pub fn cursor_pos() -> (i32, i32) {
    try_cursor_pos().unwrap_or_else(|error| {
        eprintln!("{error}");
        (0, 0)
    })
}

impl CursorSnapshot {
    /// Builds a snapshot from an image and its desktop-pixel top-left.
    pub fn from_image(image: Frame, (x, y): (i32, i32)) -> io::Result<CursorSnapshot> {
        let bounds = [x as f64, y as f64, x as f64 + image.width as f64, y as f64 + image.height as f64];
        Self::from_image_with_bounds(image, bounds)
    }

    /// Builds a snapshot with precise compatibility-grid image bounds.
    pub fn from_image_with_bounds(image: Frame, bounds: [f64; 4]) -> io::Result<CursorSnapshot> {
        image.validate()?;
        let [left, top, right, bottom] = bounds;
        let representable = |value: f64| value.round() >= i32::MIN as f64 && value.round() <= i32::MAX as f64;
        if !bounds.iter().all(|value| value.is_finite())
            || !(right - left).is_finite() || !(bottom - top).is_finite()
            || right <= left || bottom <= top || !representable(left) || !representable(top)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid cursor bounds"));
        }
        let (x, y) = (left.round() as i32, top.round() as i32);
        Ok(CursorSnapshot {
            image,
            x,
            y,
            offset: (left - x as f64, top - y as f64),
            extent: (right - left, bottom - top),
        })
    }

    fn from_system_image(
        image: Frame,
        (x, y): (f64, f64),
        (hot_x, hot_y): (f64, f64),
        (width, height): (f64, f64),
        scale: f64,
    ) -> io::Result<CursorSnapshot> {
        if !scale.is_finite() || scale <= 0.0 || !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid system cursor geometry"));
        }
        let (left, top) = ((x - hot_x) * scale, (y - hot_y) * scale);
        Self::from_image_with_bounds(image, [left, top, left + width * scale, top + height * scale])
    }

    /// The system cursor's image and hot spot. AppKit only exposes the
    /// standard cursor shapes the system reports, not every custom cursor an
    /// app draws (a documented platform gap).
    // The deprecated API is still the only public way to read another app's
    // cursor image.
    pub fn take() -> Option<CursorSnapshot> {
        Self::take_on_scale(scale_factor())
    }

    /// Samples the cursor using the same reference scale as a retained layout.
    /// The caller validates that layout as part of the capture operation.
    pub fn take_in_layout(layout: &DisplayLayout) -> Option<CursorSnapshot> {
        Self::take_on_scale(layout.desktop_scale())
    }

    #[allow(deprecated)]
    fn take_on_scale(scale: f64) -> Option<CursorSnapshot> {
        let cursor = objc2_app_kit::NSCursor::currentSystemCursor()?;
        let image = cursor.image();
        let size = image.size();
        let hot = cursor.hotSpot();
        let (width, height) = (size.width * scale, size.height * scale);
        let dimension = |value: f64| {
            let rounded = value.round().max(1.0);
            (value.is_finite() && value > 0.0 && rounded <= i32::MAX as f64).then_some(rounded as usize)
        };
        let Some((w, h)) = dimension(width).zip(dimension(height)) else {
            eprintln!("could not construct the cursor snapshot: invalid cursor dimensions");
            return None;
        };
        if w.checked_mul(h).and_then(|pixels| pixels.checked_mul(4)).is_none_or(|bytes| bytes > isize::MAX as usize) {
            eprintln!("could not construct the cursor snapshot: cursor image is too large");
            return None;
        }
        // SAFETY: a null proposed rectangle asks for the best representation.
        let cg = unsafe { image.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None) }?;
        let mut pixels = match sck::image_bgra(objc2::rc::Retained::as_ptr(&cg) as CGImageRef, w, h) {
            Ok(pixels) => pixels,
            Err(error) => {
                eprintln!("could not read the cursor image: {error}");
                return None;
            }
        };
        // Premultiplied to straight alpha.
        for p in pixels.chunks_exact_mut(4) {
            let a = p[3] as u32;
            if a > 0 && a < 255 {
                for c in &mut p[..3] {
                    *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
        let (mx, my) = match try_cursor_pos_points() {
            Ok(position) => position,
            Err(error) => {
                eprintln!("could not locate the cursor: {error}");
                return None;
            }
        };
        let image = Frame { width: w as u32, height: h as u32, pixels, alpha: true };
        match CursorSnapshot::from_system_image(image, (mx, my), (hot.x, hot.y), (size.width, size.height), scale) {
            Ok(snapshot) => Some(snapshot),
            Err(error) => {
                eprintln!("could not construct the cursor snapshot: {error}");
                None
            }
        }
    }

    /// The cursor alone, with transparency.
    pub fn image(&self) -> io::Result<Frame> {
        Ok(self.image.clone())
    }

    /// Whether any part of the cursor image overlaps the capture.
    pub fn intersects(&self, rect: Rect) -> bool {
        !rect.is_empty() && self.intersects_bounds([
            rect.x as f64,
            rect.y as f64,
            (i64::from(rect.x) + i64::from(rect.width)) as f64,
            (i64::from(rect.y) + i64::from(rect.height)) as f64,
        ])
    }

    /// Whether any part of the cursor overlaps the precise capture bounds.
    pub fn intersects_mapping(&self, mapping: CaptureMapping) -> bool {
        self.intersects_bounds(mapping.desktop_bounds())
    }

    fn precise_bounds(&self) -> [f64; 4] {
        let left = self.x as f64 + self.offset.0;
        let top = self.y as f64 + self.offset.1;
        [left, top, left + self.extent.0, top + self.extent.1]
    }

    fn intersects_bounds(&self, [left, top, right, bottom]: [f64; 4]) -> bool {
        let [cursor_left, cursor_top, cursor_right, cursor_bottom] = self.precise_bounds();
        cursor_left < right && cursor_top < bottom && cursor_right > left && cursor_bottom > top
    }

    /// Returns the cursor and its position in the capture's image pixels.
    pub fn mapped_image(&self, mapping: CaptureMapping) -> io::Result<(Frame, (i32, i32))> {
        let (output_width, output_height) = mapping.size();
        let [left, top, right, bottom] = mapping.desktop_bounds();
        let [cursor_left, cursor_top, cursor_right, cursor_bottom] = self.precise_bounds();
        // Cover output pixel centers inside the cursor, retaining at least one pixel.
        let axis = |start: f64, end: f64, origin: f64, extent: f64, pixels: u32| -> io::Result<(i32, u32, f64, f64)> {
            let first_edge = (start - origin) * pixels as f64 / extent;
            let last_edge = (end - origin) * pixels as f64 / extent;
            let first = (first_edge - 0.5).ceil();
            let count = ((last_edge - 0.5).ceil() - first).max(1.0);
            if !first_edge.is_finite() || !last_edge.is_finite() || last_edge <= first_edge
                || !first.is_finite() || first < i32::MIN as f64 || first > i32::MAX as f64
                || !count.is_finite() || count > i32::MAX as f64
            {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid mapped cursor geometry"));
            }
            Ok((first as i32, count as u32, first_edge, last_edge - first_edge))
        };
        let (x, width, x_edge, x_extent) = axis(cursor_left, cursor_right, left, right - left, output_width)?;
        let (y, height, y_edge, y_extent) = axis(cursor_top, cursor_bottom, top, bottom - top, output_height)?;
        let bytes = (width as usize).checked_mul(height as usize).and_then(|pixels| pixels.checked_mul(4))
            .filter(|bytes| *bytes <= isize::MAX as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "mapped cursor image is too large"))?;
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(bytes).map_err(|error| io::Error::other(format!("could not allocate the mapped cursor image: {error}")))?;
        pixels.resize(bytes, 0);
        let sample = |first: i32, index: u32, edge: f64, extent: f64, source: u32| {
            let center = first as f64 + index as f64 + 0.5;
            (((center - edge) * source as f64 / extent).floor()).clamp(0.0, (source - 1) as f64) as usize
        };
        for row in 0..height {
            let source_y = sample(y, row, y_edge, y_extent, self.image.height);
            for col in 0..width {
                let source_x = sample(x, col, x_edge, x_extent, self.image.width);
                let source = source_y * self.image.stride() + source_x * 4;
                let target = (row as usize * width as usize + col as usize) * 4;
                pixels[target..target + 4].copy_from_slice(&self.image.pixels[source..source + 4]);
            }
        }
        Ok((Frame { width, height, pixels, alpha: self.image.alpha }, (x, y)))
    }

    /// Draws the cursor using the same desktop-to-image mapping as a capture.
    pub fn draw_onto_mapped(&self, frame: &mut Frame, mapping: CaptureMapping) -> io::Result<()> {
        let (image, (x, y)) = self.mapped_image(mapping)?;
        frame.composite_over(&image, (i64::from(x), i64::from(y)))
    }

    /// Draws the cursor onto `frame`, whose top-left is at `origin` in screen
    /// coordinates.
    pub fn draw_onto(&self, frame: &mut Frame, origin: (i32, i32)) -> io::Result<()> {
        let position = (i64::from(self.x) - i64::from(origin.0), i64::from(self.y) - i64::from(origin.1));
        frame.composite_over(&self.image, position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusion_revisions_change_only_for_real_updates() {
        let mut registry = ExclusionRegistry::default();
        assert!(!registry.ready(42));
        registry.set(42, true);
        assert_eq!(registry.exclusions.revision, 1);
        assert!(registry.ready(42));
        registry.captures.push((7, 0));
        assert!(!registry.ready(42));
        registry.set(42, true);
        registry.set(17, false);
        assert_eq!(registry.exclusions.revision, 1);
        registry.captures[0].1 = 1;
        assert!(registry.ready(42));
        registry.set(17, true);
        assert!(!registry.ready(42));
        registry.set(42, false);
        assert_eq!(registry.exclusions.revision, 3);
        assert!(!registry.ready(42));
        registry.set(42, false);
        assert_eq!(registry.exclusions.revision, 3);
    }

    #[test]
    fn retired_owner_cannot_remove_a_reused_window_number() {
        let mut registry = ExclusionRegistry::default();
        let closed = registry.register(42);
        registry.captures.push((7, registry.exclusions.revision));
        assert!(registry.ready(42));
        let replacement = registry.register(42);
        assert_ne!(closed, replacement);
        assert_eq!(registry.exclusions.revision, 2);
        assert!(!registry.ready(42));
        assert!(registry.retire(closed));
        assert!(!registry.retire(closed));
        assert!(registry.tickets.contains(&replacement));
        assert!(registry.exclusions.required.contains(&42));
        registry.captures[0].1 = registry.exclusions.revision;
        assert!(registry.ready(42));
        assert!(registry.retire(replacement));
        assert!(registry.exclusions.windows.contains(&42));
        assert!(!registry.exclusions.required.contains(&42));
        assert!(!registry.ready(42));
        registry.finish_capture(7);
        assert!(!registry.exclusions.windows.contains(&42));
    }

    #[test]
    fn newer_owner_can_close_before_an_older_owner() {
        let mut registry = ExclusionRegistry::default();
        let older = registry.register(42);
        let newer = registry.register(42);
        assert!(registry.retire(newer));
        assert!(registry.tickets.contains(&older));
        assert!(registry.exclusions.required.contains(&42));
        assert!(registry.ready(42));
        assert!(registry.retire(older));
        assert!(!registry.exclusions.windows.contains(&42));
    }

    #[test]
    fn retired_numbers_outlive_old_captures_but_not_new_ones() {
        let mut registry = ExclusionRegistry::default();
        registry.captures.extend([(1, 0), (2, 0)]);
        let ticket = registry.register(42);
        assert!(registry.retire(ticket));
        assert!(registry.exclusions.windows.contains(&42));
        assert!(!registry.exclusions.required.contains(&42));
        registry.captures.push((3, registry.exclusions.revision));
        registry.finish_capture(1);
        assert!(registry.exclusions.windows.contains(&42));
        registry.finish_capture(2);
        assert!(!registry.exclusions.windows.contains(&42));
        assert!(registry.captures.iter().any(|&(id, _)| id == 3));
    }

    #[test]
    fn reused_retired_number_requires_a_new_filter_without_losing_its_owner() {
        let mut registry = ExclusionRegistry::default();
        registry.captures.push((1, 0));
        let old = registry.register(42);
        assert!(registry.retire(old));
        let before = registry.exclusions.revision;
        let new = registry.register(42);
        assert!(registry.exclusions.revision > before);
        assert!(registry.exclusions.required.contains(&42));
        assert!(!registry.ready(42));
        registry.captures[0].1 = before;
        assert!(!registry.ready(42));
        registry.captures[0].1 = registry.exclusions.revision;
        assert!(registry.ready(42));
        registry.finish_capture(1);
        assert!(registry.tickets.contains(&new));
        assert!(registry.exclusions.required.contains(&42));
        assert!(registry.retire(new));
        assert!(!registry.exclusions.windows.contains(&42));
    }

    #[test]
    fn manual_exclusion_does_not_remove_ticketed_owners() {
        let mut registry = ExclusionRegistry::default();
        let ticket = registry.register(42);
        registry.set(42, true);
        registry.set(42, false);
        assert!(registry.tickets.contains(&ticket));
        assert!(registry.ready(42));
        assert!(registry.retire(ticket));
        assert!(!registry.exclusions.windows.contains(&42));
    }

    #[test]
    fn in_flight_capture_blocks_new_exclusions_until_it_updates_or_finishes() {
        let mut registry = ExclusionRegistry::default();
        registry.captures.push((7, registry.exclusions.revision));
        registry.register(42);
        assert!(!registry.ready(42));
        registry.captures[0].1 = registry.exclusions.revision;
        assert!(registry.ready(42));
        registry.register(17);
        assert!(!registry.ready(17));
        registry.captures.clear();
        assert!(registry.ready(17));
    }

    #[test]
    fn recording_discards_frames_older_than_filter_update() {
        let time = |value, timescale| CMTime {
            value,
            timescale,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        assert!(!frame_after(time(3, 2), time(2, 1)).unwrap());
        assert!(frame_after(time(4, 2), time(2, 1)).unwrap());
        assert!(frame_after(time(5, 2), time(2, 1)).unwrap());
        assert!(frame_after(time(i64::MAX, 1), time(i64::MAX, i32::MAX)).unwrap());
        assert!(frame_after(time(1, 0), time(1, 1)).is_err());
        assert!(frame_after(CMTime { flags: CMTimeFlags::Indefinite, ..time(1, 1) }, time(1, 1)).is_err());
        assert!(frame_after(CMTime { epoch: 1, ..time(1, 1) }, time(1, 1)).is_err());
    }

    fn display(id: u32, bounds: CGRect, scale: f64, main: bool, mirrors: Option<u32>) -> Display {
        Display {
            id,
            bounds,
            scale,
            mode_pixels: Some(((bounds.size.width * scale) as usize, (bounds.size.height * scale) as usize)),
            mode_points: Some((bounds.size.width, bounds.size.height)),
            backing_pixels: None,
            rotation: 0.0,
            main,
            mirrors,
        }
    }

    fn logical(mapping: CaptureMapping, layout: &Layout) -> LogicalCaptureMapping {
        LogicalCaptureMapping::from_capture(mapping, layout.scale).unwrap()
    }

    #[test]
    fn logical_capture_size_ignores_unrelated_display_density() {
        let main = display(1, rect(0.0, 0.0, 10.0, 10.0), 1.0, true, None);
        let distant = display(2, rect(20.0, 0.0, 10.0, 10.0), 2.0, false, None);
        let single = Layout::new(vec![main]);
        let mixed = Layout::new(vec![main, distant]);
        let window = rect(0.26, 0.26, 1.4, 1.4);
        let bounds = LogicalBounds::from_rect(window).unwrap();
        assert_eq!(single.logical_size(bounds).unwrap(), (1, 1));
        assert_eq!(mixed.logical_size(bounds).unwrap(), (1, 1));
        assert_eq!(single.logical_mapping(bounds).unwrap(), mixed.logical_mapping(bounds).unwrap());
        assert_eq!(mixed.logical_mapping(bounds).unwrap().bounds().edges(), [0.26, 0.26, 1.66, 1.66]);
        let old_single = single.mapping(rect_from_points_at_scale(window, single.scale)).unwrap();
        let old_mixed = mixed.mapping(rect_from_points_at_scale(window, mixed.scale)).unwrap();
        assert_eq!(old_single.size(), (2, 2));
        assert_eq!(old_mixed.size(), (1, 1));

        for invalid in [
            rect(f64::NAN, 0.0, 1.0, 1.0),
            rect(0.0, f64::INFINITY, 1.0, 1.0),
            rect(0.0, 0.0, 0.0, 1.0),
            rect(0.0, 0.0, -1.0, 1.0),
        ] {
            assert!(LogicalBounds::from_rect(invalid).is_err());
        }
        assert!(single.logical_size(LogicalBounds::from_rect(rect(30.0, 30.0, 1.0, 1.0)).unwrap()).is_err());
        assert!(single.logical_size(LogicalBounds::from_rect(rect(-1.0, -1.0, 1.0e12, 1.0e12)).unwrap()).is_err());
    }

    #[test]
    fn logical_desktop_preserves_fractional_outer_edges_and_display_gaps() {
        let left = display(2, rect(-2.25, 0.25, 2.0, 2.0), 1.0, false, None);
        let right = display(1, rect(-0.25, 0.25, 2.0, 2.0), 2.0, true, None);
        let separated = display(3, rect(2.25, 0.25, 1.0, 2.0), 1.0, false, None);
        let layout = Layout::new(vec![left, right, separated]);
        let bounds = layout.logical_virtual_bounds().unwrap();
        assert_eq!(bounds.edges(), [-2.25, 0.25, 3.25, 2.25]);
        let mapping = layout.logical_mapping(bounds).unwrap();
        assert_eq!(mapping.size(), (11, 4));
        let grid = layout.displays.iter()
            .map(|display| rect_from_points_at_scale(display.bounds, layout.scale))
            .reduce(Rect::union).unwrap();
        assert_ne!(LogicalBounds::from_capture(layout.mapping(grid).unwrap(), layout.scale).unwrap(), bounds);

        let solid = |width, height, color| Frame {
            width, height, pixels: vec![color, 0, 0, 255].repeat((width * height) as usize), alpha: false,
        };
        let frozen = FrozenDesktop {
            layout,
            displays: vec![(left, solid(2, 2, 10)), (right, solid(4, 4, 30)), (separated, solid(1, 2, 60))],
        };
        let frame = frozen.render_logical_unchecked(mapping).unwrap();
        for row in frame.pixels.chunks_exact(frame.stride()) {
            let colors: Vec<_> = row.chunks_exact(4).map(|pixel| pixel[0]).collect();
            assert_eq!(colors, [10, 10, 10, 10, 30, 30, 30, 30, 0, 60, 60]);
        }
        assert_eq!(Layout::new(Vec::new()).logical_virtual_bounds().unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn logical_capture_mapping_validates_bounds_and_buffer_dimensions() {
        let bounds = LogicalBounds::new([-2.125, -1.75, -1.625, -1.25]).unwrap();
        let mapping = LogicalCaptureMapping::new(bounds, (1, 1)).unwrap();
        assert_eq!(mapping.bounds().edges(), [-2.125, -1.75, -1.625, -1.25]);
        assert_eq!(mapping.size(), (1, 1));

        for edges in [
            [f64::NAN, 0.0, 1.0, 1.0],
            [0.0, 0.0, f64::INFINITY, 1.0],
            [0.0, 0.0, 0.0, 1.0],
            [1.0, 1.0, 0.0, 2.0],
            [-f64::MAX, 0.0, f64::MAX, 1.0],
        ] {
            assert_eq!(LogicalBounds::new(edges).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        }
        for size in [(0, 1), (1, 0), (u32::MAX, 1), (1, u32::MAX), (i32::MAX as u32, i32::MAX as u32)] {
            assert_eq!(LogicalCaptureMapping::new(bounds, size).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        }
        let overlap = LogicalBounds::new([-2.0, -2.0, 0.0, -1.5]).unwrap();
        assert_eq!(bounds.intersection(overlap).unwrap().edges(), [-2.0, -1.75, -1.625, -1.5]);
        let adjacent = LogicalBounds::new([-1.625, -1.75, 0.0, -1.25]).unwrap();
        assert!(bounds.intersection(adjacent).is_none());
    }

    #[test]
    fn logical_frozen_capture_keeps_fractional_pixels_and_mixed_display_seams() {
        let left = display(2, rect(-2.0, -1.0, 2.0, 2.0), 1.0, false, None);
        let right = display(1, rect(0.0, -1.0, 2.0, 2.0), 2.0, true, None);
        let distant = display(3, rect(20.0, 0.0, 2.0, 2.0), 4.0, false, None);
        let solid = |width, height, value| Frame { width, height, pixels: vec![value, 0, 0, 255].repeat((width * height) as usize), alpha: false };
        let bounds = LogicalBounds::from_rect(rect(-0.5, -0.5, 1.0, 1.0)).unwrap();
        let mut results = Vec::new();
        for displays in [vec![left, right], vec![left, right, distant]] {
            let layout = Layout::new(displays.clone());
            let mapping = LogicalCaptureMapping::new(bounds, layout.logical_size(bounds).unwrap()).unwrap();
            assert_eq!(mapping.size, (2, 2));
            let frames = displays.iter().map(|display| {
                let (width, height) = display.native_size().unwrap();
                (*display, solid(width, height, if display.id == 1 { 60 } else { 10 }))
            }).collect();
            let frozen = FrozenDesktop { layout, displays: frames };
            results.push(frozen.render_logical_unchecked(mapping).unwrap().pixels);
        }
        assert_eq!(results[0], results[1]);
        assert_eq!(
            results[0].chunks_exact(4).map(|pixel| pixel[0]).collect::<Vec<_>>(),
            [10, 60, 10, 60]
        );

        let layout = Layout::new(vec![right]);
        let mut source = Frame::new(4, 4);
        for (index, pixel) in source.pixels.chunks_exact_mut(4).enumerate() {
            pixel.copy_from_slice(&[index as u8, 0, 0, 255]);
        }
        let frozen = FrozenDesktop { layout: layout.clone(), displays: vec![(right, source)] };
        let bounds = LogicalBounds::from_rect(rect(0.5, -0.5, 0.5, 0.5)).unwrap();
        let mapping = LogicalCaptureMapping::new(bounds, layout.logical_size(bounds).unwrap()).unwrap();
        assert_eq!(mapping.size, (1, 1));
        assert_eq!(frozen.render_logical_unchecked(mapping).unwrap().pixels, [5, 0, 0, 255]);
    }

    #[test]
    fn logical_piece_edges_match_direct_pixel_centers_at_fractional_seams() {
        for origin in [-1.0e9 + 0.1, -1.3, -0.1, 0.1, 0.3, 1.0e9 + 0.1] {
            let first = display(1, rect(origin - 1.0, 0.0, 1.0, 1.0), 2.0, true, None);
            let seam = first.bounds.origin.x + first.bounds.size.width;
            let second = display(2, rect(seam, 0.0, 1.0, 1.0), 2.0, false, None);
            for width in 1..=16 {
                for extent in [0.2, 0.3, 1.0, 2.0] {
                    for center in 0..width {
                        let start = seam - (f64::from(center) + 0.5) * extent / f64::from(width);
                        let bounds = LogicalBounds::new([start, 0.0, start + extent, 1.0]).unwrap();
                        let mapping = LogicalCaptureMapping::new(bounds, (width, 1)).unwrap();
                        for display in [first, second] {
                            let output = piece(&display, mapping).map(|(_, area)| area);
                            for x in 0..width {
                                let point = mapping.pixel_center(x, 0).0;
                                let expected = point >= display.bounds.origin.x
                                    && point < display.bounds.origin.x + display.bounds.size.width;
                                let actual = output.is_some_and(|area| area.contains(x as i32, 0));
                                assert_eq!(actual, expected, "seam={seam:?} bounds={bounds:?} width={width} x={x}");
                            }
                        }
                    }
                }
            }
        }
        let bounds = LogicalBounds::new([1.0e307, 0.0, 9.0e307, 1.0]).unwrap();
        let mapping = LogicalCaptureMapping::new(bounds, (9, 1)).unwrap();
        let last = mapping.pixel_center(8, 0).0;
        assert!(last.is_finite() && last < bounds.0[2]);
        assert_eq!(LogicalCaptureMapping::first_pixel_at_or_after(last, bounds.0[0], bounds.0[2], 9), 8);
    }

    #[test]
    fn saved_coordinate_space_key_tracks_geometry_and_display_uuid() {
        let first = display(1, rect(0.0, 0.0, 800.0, 600.0), 2.0, true, None);
        let second = display(2, rect(-640.5, 0.0, 640.0, 480.0), 1.0, false, None);
        let base = Layout::new(vec![first, second]);
        let identify = |id| Some([id as u8; 16]);
        let key_for = |layout: &Layout| layout.coordinate_space_key(identify).unwrap();
        let key = key_for(&base);
        assert_eq!(key_for(&Layout::new(vec![second, first])), key);
        let mut restarted = base.clone();
        restarted.generation = 99;
        assert_eq!(key_for(&restarted), key);

        let mut moved = base.clone();
        moved.displays[1].bounds.origin.x += 0.5;
        assert_ne!(key_for(&moved), key);
        let mut changed_pixels = base.clone();
        changed_pixels.displays[1].mode_pixels = Some((641, 480));
        assert_ne!(key_for(&changed_pixels), key);
        let mut rotated = base.clone();
        rotated.displays[1].rotation = 180.0;
        assert_ne!(key_for(&rotated), key);
        let mut replaced = base.clone();
        replaced.displays[1].id = 3;
        assert_ne!(key_for(&replaced), key);
        let reused_id = base.coordinate_space_key(|id| Some([if id == 2 { 3 } else { id as u8 }; 16])).unwrap();
        assert_ne!(reused_id, key, "a different UUID must invalidate a reused display ID");
        let distinctive = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10];
        let identity_key = base.coordinate_space_key(|_| Some(distinctive)).unwrap();
        assert_eq!((identity_key.components[3], identity_key.components[4]), (0x0123456789abcdef, 0xfedcba9876543210));
        assert!(identity_key.restorable);
        let attached = Layout::new(vec![first, second, display(3, rect(900.0, -50.0, 400.0, 300.0), 1.0, false, None)]);
        assert_ne!(key_for(&attached), key);
        let missing = base.coordinate_space_key(|id| (id == 1).then_some([1; 16])).unwrap();
        assert!(!missing.restorable);
        assert!(!base.coordinate_space_key(|_| Some([0; 16])).unwrap().restorable);
        let mut invalid = base;
        invalid.displays[1].mode_pixels = Some((0, 480));
        assert!(invalid.coordinate_space_key(identify).is_err());
    }

    #[test]
    fn core_foundation_uuid_bytes_preserve_all_bits() {
        let uuid = CFUUID::with_bytes(None,
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
            0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10,
        ).unwrap();
        assert_eq!(uuid_bytes(&uuid), [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
            0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10,
        ]);
    }

    #[test]
    fn capture_pieces_use_one_layout_scale_across_mixed_and_negative_displays() {
        let layout = Layout::new(vec![
            display(1, rect(0.0, 0.0, 800.0, 600.0), 2.0, true, None),
            display(2, rect(-640.0, 0.0, 640.0, 480.0), 1.0, false, None),
            display(3, rect(1000.0, -100.0, 480.0, 800.0), 1.0, false, None),
        ]);
        assert_eq!(layout.scale, 2.0);
        let selection = Rect::new(-200, 100, 500, 200);
        let mapping = layout.mapping(selection).unwrap();
        let (left_source, left_output) = piece(&layout.displays[1], logical(mapping, &layout)).unwrap();
        assert_eq!(left_source, rect(540.0, 50.0, 100.0, 100.0));
        assert_eq!(left_output, Rect::new(0, 0, 200, 200));
        let (main_source, main_output) = piece(&layout.displays[0], logical(mapping, &layout)).unwrap();
        assert_eq!(main_source, rect(0.0, 50.0, 150.0, 100.0));
        assert_eq!(main_output, Rect::new(200, 0, 300, 200));

        let gap = Rect::new(1700, 0, 100, 100);
        assert!(layout.mapping(gap).is_err());
        let rotated = Rect::new(2000, -200, 960, 1600);
        let (source, output) = piece(&layout.displays[2], logical(CaptureMapping::identity(rotated).unwrap(), &layout)).unwrap();
        assert_eq!(source, rect(0.0, 0.0, 480.0, 800.0));
        assert_eq!(output, Rect::new(0, 0, 960, 1600));
    }

    #[test]
    fn desktop_pixel_step_retains_fractional_display_density() {
        let layout = Layout::new(vec![
            display(1, rect(0.0, 0.0, 8.0, 8.0), 2.0, true, None),
            display(2, rect(-8.0, 0.0, 8.0, 8.0), 1.5, false, None),
        ]);
        let monitors = layout.displays.iter().map(|display| describe(display, layout.scale)).collect();
        let layout = DisplayLayout { layout, monitors };
        assert_eq!(layout.desktop_pixel_step(2, 2).unwrap(), (1.0, 1.0));
        let (x, y) = layout.desktop_pixel_step(-2, 2).unwrap();
        assert!((x - 4.0 / 3.0).abs() < 1e-10);
        assert!((y - 4.0 / 3.0).abs() < 1e-10);
        assert_eq!(layout.desktop_size_for_pixels(-2, 2, 3, 3).unwrap(), (4, 4));
    }

    #[test]
    fn precise_pixel_step_selects_the_display_on_each_side_of_a_fractional_seam() {
        let layout = Layout::new(vec![
            display(1, rect(-0.125, 0.0, 8.0, 8.0), 2.0, true, None),
            display(2, rect(-8.125, 0.0, 8.0, 8.0), 1.5, false, None),
        ]);
        let monitors = layout.displays.iter().map(|display| describe(display, layout.scale)).collect();
        let layout = DisplayLayout { layout, monitors };
        let left = layout.desktop_pixel_step_precise(-0.26, 2.0).unwrap();
        assert!((left.0 - 4.0 / 3.0).abs() < 1e-10);
        assert!((left.1 - 4.0 / 3.0).abs() < 1e-10);
        assert_eq!(layout.monitor_at_precise(-0.26, 2.0).unwrap().device, "display-2");
        assert_eq!(layout.monitor_at_precise(-0.25, 2.0).unwrap().device, "display-1");
        assert_eq!(layout.monitor_at_precise(-0.24, 2.0).unwrap().device, "display-1");
        assert_eq!(layout.monitor_at_points(-0.13, 1.0).unwrap().device, "display-2");
        assert_eq!(layout.monitor_at_points(-0.125, 1.0).unwrap().device, "display-1");
        assert_eq!(layout.desktop_size_for_pixels_precise(-0.26, 2.0, 3, 3).unwrap(), (4, 4));
        assert_eq!(layout.desktop_size_for_pixels_precise(-0.24, 2.0, 3, 3).unwrap(), (3, 3));
        assert_eq!(layout.desktop_pixel_step_precise(-0.25, 2.0).unwrap(), (1.0, 1.0));
        assert_eq!(layout.desktop_pixel_step_precise(-0.24, 2.0).unwrap(), (1.0, 1.0));
        assert!(layout.desktop_pixel_step_precise(f64::NAN, 2.0).is_err());
        assert!(layout.desktop_pixel_step_precise(0.0, f64::INFINITY).is_err());
        assert!(layout.monitor_at_precise(f64::NAN, 2.0).is_err());
        assert!(layout.monitor_at_points(f64::INFINITY, 1.0).is_err());
        assert!(layout.desktop_size_for_pixels_precise(f64::NAN, 2.0, 3, 3).is_err());
    }

    #[test]
    fn cursor_monitor_lookup_validates_the_layout_and_propagates_pointer_errors() {
        let layout = DisplayLayout::current().expect("an active display");
        let display = layout.layout.displays.first().expect("an active display");
        let point = (
            display.bounds.origin.x + display.bounds.size.width / 2.0,
            display.bounds.origin.y + display.bounds.size.height / 2.0,
        );
        assert_eq!(layout.monitor_under_cursor_with(|| Ok(point)).unwrap().device, format!("display-{}", display.id));
        let error = layout.monitor_under_cursor_with(|| Err(io::Error::other("pointer query failed"))).unwrap_err();
        assert!(error.to_string().contains("pointer query failed"));
        assert!(layout.monitor_under_cursor_with(|| Ok((f64::NAN, point.1))).is_err());

        let mut stale = layout.clone();
        stale.layout.generation = stale.layout.generation.wrapping_sub(1);
        let called = std::cell::Cell::new(false);
        let error = stale.monitor_under_cursor_with(|| {
            called.set(true);
            Ok(point)
        }).unwrap_err();
        assert!(error.to_string().contains("display configuration changed"));
        assert!(!called.get(), "a stale layout must be rejected before reading the pointer");
    }

    #[test]
    fn monitor_selection_uses_the_nearest_display_in_gaps_and_at_extremes() {
        let layout = Layout::new(vec![
            display(1, rect(0.0, 0.0, 100.0, 100.0), 2.0, true, None),
            display(2, rect(-150.0, 0.0, 100.0, 100.0), 1.0, false, None),
        ]);
        let monitors = layout.displays.iter().map(|display| describe(display, layout.scale)).collect();
        let layout = DisplayLayout { layout, monitors };
        assert_eq!(layout.monitor_at(-70, 50).unwrap().device, "display-2");
        assert_eq!(layout.desktop_pixel_step(-70, 50).unwrap(), (2.0, 2.0));
        assert_eq!(layout.desktop_pixel_step_precise(-70.25, 50.0).unwrap(), (2.0, 2.0));
        assert_eq!(layout.monitor_at_precise(-70.25, 50.0).unwrap().device, "display-2");
        assert_eq!(layout.monitor_at(-30, 50).unwrap().device, "display-1");
        assert_eq!(layout.desktop_pixel_step(-30, 50).unwrap(), (1.0, 1.0));
        assert_eq!(layout.desktop_pixel_step_precise(-30.25, 50.0).unwrap(), (1.0, 1.0));
        assert_eq!(layout.monitor_at_precise(-30.25, 50.0).unwrap().device, "display-1");

        let monitor = |x, device: &str| Monitor {
            rect: Rect::new(x, 0, 10, 10),
            work: Rect::new(x, 0, 10, 10),
            dpi: 96,
            primary: false,
            device: device.into(),
        };
        let extremes = [monitor(i32::MIN, "left"), monitor(i32::MAX - 10, "right")];
        assert_eq!(nearest_monitor(&extremes, i32::MAX, i32::MIN).unwrap().device, "right");
        assert!(nearest_monitor(&[], 0, 0).is_none());
    }

    #[test]
    fn frozen_displays_preserve_native_pixels_and_compose_cross_display_seams() {
        let low = display(2, rect(-3.0, 0.0, 3.0, 2.0), 1.0, false, None);
        let high = display(1, rect(0.0, 0.0, 2.0, 2.0), 2.0, true, None);
        let right = display(3, rect(3.0, 0.0, 2.0, 2.0), 1.0, false, None);
        let layout = Layout::new(vec![low, high, right]);
        let frame = |width: u32, height: u32, base: u8| {
            let mut frame = Frame::new(width, height);
            for (index, pixel) in frame.pixels.chunks_exact_mut(4).enumerate() {
                pixel.copy_from_slice(&[base + index as u8, 0, 0, 255]);
            }
            frame
        };
        let frozen = FrozenDesktop {
            layout: layout.clone(),
            displays: vec![(low, frame(3, 2, 10)), (high, frame(4, 4, 30)), (right, frame(2, 2, 60))],
        };

        let low_rect = rect_from_points_at_scale(low.bounds, layout.scale);
        let monitors = layout.displays.iter().map(|display| describe(display, layout.scale)).collect();
        let display_layout = DisplayLayout { layout: layout.clone(), monitors };
        let low_monitor = display_layout.monitors.iter().find(|monitor| monitor.device == "display-2").unwrap();
        assert_eq!(display_layout.monitor_mapping(low_monitor).unwrap().size(), (3, 2));
        let mut stale_monitor = low_monitor.clone();
        stale_monitor.rect.width += 1;
        assert!(display_layout.monitor_mapping(&stale_monitor).is_err());
        stale_monitor.device = "display-missing".into();
        assert!(display_layout.monitor_mapping(&stale_monitor).is_err());
        let low_mapping = layout.mapping(low_rect).unwrap();
        assert_eq!(low_mapping.size(), (3, 2));
        let low_frame = frozen.render_frozen(low_mapping).unwrap();
        assert_eq!(low_frame.pixels, frame(3, 2, 10).pixels);
        let mut copied = vec![0; 32];
        frozen.copy_native_monitor_unchecked(low_rect, 3, 2, &mut copied, 16).unwrap();
        assert_eq!(&copied[..12], &low_frame.pixels[..12]);
        assert_eq!(&copied[16..28], &low_frame.pixels[12..24]);
        assert!(frozen.copy_native_monitor_unchecked(low_rect, 4, 2, &mut copied, 16).is_err());
        assert!(frozen.copy_native_monitor_unchecked(low_rect, 3, 2, &mut copied, 11).is_err());
        assert!(frozen.copy_native_monitor_unchecked(low_rect, 3, 2, &mut copied[..31], 16).is_err());
        assert!(frozen.copy_native_monitor_unchecked(Rect::new(4, 0, 2, 2), 3, 2, &mut copied, 16).is_err());
        assert_eq!(frozen.color_at(-6, 0), Some((0, 0, 10)));
        assert_eq!(frozen.color_at(-5, 0), Some((0, 0, 10)));
        assert_eq!(frozen.color_at(-4, 0), Some((0, 0, 11)));
        assert_eq!(frozen.color_at(-1, 0), Some((0, 0, 12)));
        assert_eq!(frozen.color_at(0, 0), Some((0, 0, 30)));
        assert_eq!(frozen.color_at(3, 0), Some((0, 0, 33)));
        assert_eq!(frozen.color_at(5, 0), None);
        assert_eq!(frozen.color_at(6, 0), Some((0, 0, 60)));

        let cross = Rect::new(-4, 0, 8, 4);
        let cross_mapping = layout.mapping(cross).unwrap();
        assert_eq!(cross_mapping.size(), (8, 4));
        let cross_frame = frozen.render_frozen(cross_mapping).unwrap();
        assert_eq!(cross_frame.pixels[0], 11);
        assert_eq!(cross_frame.pixels[4], 11);
        assert_eq!(cross_frame.pixels[8], 12);
        assert_eq!(cross_frame.pixels[12], 12);
        assert_eq!(cross_frame.pixels[16], 30);
        assert_eq!(cross_frame.pixels[20], 31);

        let gap = Rect::new(4, 0, 6, 4);
        let gap_mapping = layout.mapping(gap).unwrap();
        assert_eq!(gap_mapping.size(), (3, 2));
        let gap_frame = frozen.render_frozen(gap_mapping).unwrap();
        assert_eq!(&gap_frame.pixels[0..4], &[0, 0, 0, 255]);
        assert_eq!(gap_frame.pixels[4], 60);
    }

    #[test]
    fn monitor_mapping_preserves_native_pixels_at_fractional_origins() {
        let display = display(1, rect(0.25, -0.25, 2.0, 2.0), 2.0, true, None);
        let layout = Layout::new(vec![display]);
        let monitors = layout.displays.iter().map(|display| describe(display, layout.scale)).collect();
        let display_layout = DisplayLayout { layout: layout.clone(), monitors };
        let mut source = Frame::new(4, 4);
        for (index, pixel) in source.pixels.chunks_exact_mut(4).enumerate() {
            pixel.copy_from_slice(&[index as u8, 0, 0, 255]);
        }
        let frozen = FrozenDesktop { layout, displays: vec![(display, source.clone())] };
        let mapping = display_layout.monitor_mapping(&display_layout.monitors[0]).unwrap();
        assert_eq!(mapping.desktop_bounds(), [0.5, -0.5, 4.5, 3.5]);
        assert_eq!(display_layout.monitor_mapping_for_rect(mapping.rect()).unwrap(), mapping);
        assert_eq!(frozen.render_frozen(mapping).unwrap().pixels, source.pixels);
    }

    #[test]
    fn precise_monitor_mappings_preserve_negative_anisotropic_and_rotated_pixels() {
        for (bounds, pixels, rotation) in [
            (rect(-2.125, -3.375, 3.0, 2.0), (6, 4), 0.0),
            (rect(1.25, -1.25, 2.0, 4.0), (6, 4), 0.0),
            (rect(-0.25, 1.125, 2.0, 4.0), (4, 8), 90.0),
        ] {
            let display = Display {
                mode_pixels: Some((pixels.0 as usize, pixels.1 as usize)),
                backing_pixels: Some((pixels.0 as f64, pixels.1 as f64)),
                rotation,
                ..display(1, bounds, 1.0, true, None)
            };
            let layout = Layout::new(vec![display]);
            let monitors = layout.displays.iter().map(|display| describe(display, layout.scale)).collect();
            let display_layout = DisplayLayout { layout: layout.clone(), monitors };
            let mut source = Frame::new(pixels.0, pixels.1);
            for (index, pixel) in source.pixels.chunks_exact_mut(4).enumerate() {
                pixel.copy_from_slice(&[index as u8, (index * 3) as u8, 77, 255]);
            }
            let frozen = FrozenDesktop { layout, displays: vec![(display, source.clone())] };
            let mapping = display_layout.monitor_mapping(&display_layout.monitors[0]).unwrap();
            assert_eq!(mapping.size(), pixels);
            assert_eq!(frozen.render_frozen(mapping).unwrap().pixels, source.pixels, "{bounds:?}");
            let piece = CapturePiece::new(&display, logical(mapping, &display_layout.layout)).unwrap().unwrap();
            assert_eq!((piece.origin, piece.size), ((0, 0), pixels));
        }
    }

    #[test]
    fn fractional_seams_assign_shared_edge_centers_once_and_leave_gaps_black() {
        let left = display(2, rect(-2.25, 0.25, 2.0, 2.0), 1.0, false, None);
        let right = display(1, rect(-0.25, 0.25, 2.0, 2.0), 2.0, true, None);
        let separated = display(3, rect(2.25, 0.25, 1.0, 2.0), 1.0, false, None);
        let layout = Layout::new(vec![left, right, separated]);
        let solid = |width, height, color| {
            let mut frame = Frame::new(width, height);
            for pixel in frame.pixels.chunks_exact_mut(4) {
                pixel.copy_from_slice(&[color, 0, 0, 255]);
            }
            frame
        };
        let frozen = FrozenDesktop {
            layout: layout.clone(),
            displays: vec![(left, solid(2, 2, 10)), (right, solid(4, 4, 30)), (separated, solid(1, 2, 60))],
        };
        let mapping = CaptureMapping::identity(Rect::new(-5, 0, 12, 4)).unwrap();
        let frame = frozen.render_frozen(mapping).unwrap();
        for row in frame.pixels.chunks_exact(frame.stride()) {
            let colors: Vec<_> = row.chunks_exact(4).map(|pixel| pixel[0]).collect();
            assert_eq!(colors, [10, 10, 10, 10, 30, 30, 30, 30, 0, 60, 60, 0]);
        }
    }

    #[test]
    fn indistinguishable_monitor_rectangles_fail_instead_of_selecting_another_display() {
        let first = display(1, rect(0.1, 0.1, 2.0, 2.0), 1.0, true, None);
        let second = display(2, rect(0.2, 0.2, 2.0, 2.0), 1.0, false, None);
        let layout = Layout::new(vec![first, second]);
        let monitors = layout.displays.iter().map(|display| describe(display, layout.scale)).collect();
        let display_layout = DisplayLayout { layout: layout.clone(), monitors };
        let monitor = display_layout.monitors[0].rect;
        assert_eq!(display_layout.monitors[1].rect, monitor);
        assert!(display_layout.monitor_mapping_for_rect(monitor).unwrap_err().to_string().contains("ambiguous"));
        let frozen = FrozenDesktop {
            layout,
            displays: vec![(first, Frame::new(2, 2)), (second, Frame::new(2, 2))],
        };
        let mut pixels = vec![0; 16];
        assert!(frozen.copy_native_monitor_unchecked(monitor, 2, 2, &mut pixels, 8).unwrap_err().to_string().contains("ambiguous"));
    }

    #[test]
    fn input_points_use_precise_display_bounds_and_reject_gaps() {
        let left = display(1, rect(-4.0, 0.0, 4.0, 3.0), 2.0, true, None);
        let right = display(2, rect(1.0, 0.0, 3.0, 3.0), 1.0, false, None);
        let layout = Layout::new(vec![left, right]);
        assert_eq!(layout.native_point(-8, 2).unwrap(), (-4.0, 1.0));
        assert_eq!(layout.native_point(2, 2).unwrap(), (1.0, 1.0));
        assert!(layout.native_point(0, 2).is_err());
        assert!(layout.native_point(8, 2).is_err());
        assert!(layout.native_point(2, 6).is_err());

        let fractional = display(3, rect(0.25, 0.0, 2.0, 2.0), 2.0, true, None);
        let layout = Layout::new(vec![fractional]);
        assert!(layout.native_point(0, 0).is_err());
        assert_eq!(layout.native_point(1, 0).unwrap(), (0.5, 0.0));
        assert!(layout.native_point(5, 0).is_err());
    }

    #[test]
    fn optimized_frozen_compositor_matches_pixel_center_sampling() {
        let low = display(2, rect(-4.0, 0.0, 4.0, 3.0), 1.0, false, None);
        let high = display(1, rect(0.0, 0.0, 4.0, 3.0), 1.5, true, None);
        let layout = Layout::new(vec![low, high]);
        let frame = |width: u32, height: u32, base: u8| {
            let mut frame = Frame::new(width, height);
            for (index, pixel) in frame.pixels.chunks_exact_mut(4).enumerate() {
                pixel.copy_from_slice(&[base.wrapping_add(index as u8), 0, 0, 255]);
            }
            frame
        };
        let frozen = FrozenDesktop {
            layout: layout.clone(),
            displays: vec![(low, frame(4, 3, 10)), (high, frame(6, 4, 50))],
        };
        for mapping in [
            CaptureMapping::new(Rect::new(-6, 0, 12, 4), 12, 4).unwrap(),
            CaptureMapping::new(Rect::new(-5, 0, 10, 4), 7, 3).unwrap(),
            CaptureMapping::new(Rect::new(-6, 0, 6, 4), 3, 2).unwrap(),
        ] {
            let (width, height) = mapping.size();
            let mut expected = Frame::new(width, height);
            for y in 0..height {
                for x in 0..width {
                    let (gx, gy) = mapping.pixel_center(x, y);
                    let (gx, gy) = (gx / layout.scale, gy / layout.scale);
                    for (display, source) in &frozen.displays {
                        let bounds = display.bounds;
                        if gx < bounds.origin.x || gx >= bounds.origin.x + bounds.size.width
                            || gy < bounds.origin.y || gy >= bounds.origin.y + bounds.size.height
                        {
                            continue;
                        }
                        let sx = ((gx - bounds.origin.x) * source.width as f64 / bounds.size.width).floor() as usize;
                        let sy = ((gy - bounds.origin.y) * source.height as f64 / bounds.size.height).floor() as usize;
                        let src = sy * source.stride() + sx * 4;
                        let dst = y as usize * expected.stride() + x as usize * 4;
                        expected.pixels[dst..dst + 4].copy_from_slice(&source.pixels[src..src + 4]);
                    }
                }
            }
            for pixel in expected.pixels.chunks_exact_mut(4) {
                pixel[3] = 255;
            }
            assert_eq!(frozen.render_frozen(mapping).unwrap().pixels, expected.pixels);
        }
    }

    #[test]
    fn fractional_display_edges_are_clipped_before_pixel_projection() {
        let left = Display {
            bounds: rect(-1.3, 0.25, 1.3, 2.0),
            mode_pixels: Some((5, 8)),
            ..display(2, rect(-1.3, 0.25, 1.3, 2.0), 1.0, false, None)
        };
        let right = display(1, rect(0.0, 0.25, 2.0, 2.0), 2.0, true, None);
        let layout = Layout::new(vec![left, right]);
        assert_eq!(layout.scale, 4.0);
        let mapping = layout.mapping(Rect::new(-6, 1, 14, 8)).unwrap();
        let (source, at) = piece(&left, logical(mapping, &layout)).unwrap();
        assert_eq!(source.origin.x, 0.0);
        assert!((source.size.width - 1.3).abs() < 1e-10);
        assert_eq!(at, Rect::new(1, 0, 5, 8));

        let make_frame = |width: u32, height: u32, base: u8| {
            let mut frame = Frame::new(width, height);
            for (index, pixel) in frame.pixels.chunks_exact_mut(4).enumerate() {
                pixel.copy_from_slice(&[base + index as u8, 0, 0, 255]);
            }
            frame
        };
        let frozen = FrozenDesktop {
            layout: layout.clone(),
            displays: vec![(left, make_frame(5, 8, 10)), (right, make_frame(4, 4, 60))],
        };
        let output = frozen.render_frozen(mapping).unwrap();
        assert_eq!(output.pixels[0], 0);
        assert_eq!(output.pixels[4], 10);
        assert_eq!(output.pixels[6 * 4], 60);
        for (display, frame) in &frozen.displays {
            let plan = CapturePiece::new(display, logical(mapping, &layout)).unwrap().unwrap();
            let (x, y) = plan.origin;
            let (width, height) = (plan.size.0 as usize, plan.size.1 as usize);
            let mut cropped = Vec::new();
            for row in y..y + height {
                let start = row * frame.stride() + x * 4;
                cropped.extend_from_slice(&frame.pixels[start..start + width * 4]);
            }
            let mut composed = Frame::new(mapping.size().0, mapping.size().1);
            plan.composite(&mut composed, &cropped, plan.size, plan.origin).unwrap();
            let mut native = Frame::new(mapping.size().0, mapping.size().1);
            plan.composite(&mut native, &frame.pixels, (frame.width, frame.height), (0, 0)).unwrap();
            assert_eq!(composed.pixels, native.pixels);
            assert!(plan.composite(&mut composed, &cropped[..cropped.len() - 1], plan.size, plan.origin).is_err());
        }
    }

    #[test]
    fn native_capture_crop_aligns_point_and_pixel_boundaries() {
        assert_eq!(aligned_native_crop(81, 718, 3024, 1512.0), (80, 640, 40.0, 320.0));
        assert_eq!(aligned_native_crop(4, 20, 1200, 800.0), (3, 18, 2.0, 12.0));
        assert_eq!(aligned_native_crop(0, 1199, 1200, 800.0), (0, 1200, 0.0, 800.0));
        assert_eq!(aligned_native_crop(1, 3, 7, 5.0), (0, 7, 0.0, 5.0));
        assert_eq!(aligned_native_crop(1, 3, 8, 3.5), (0, 8, 0.0, 3.5));
    }

    #[test]
    fn native_capture_plans_match_an_independent_logical_pixel_reference() {
        let left = Display {
            mode_pixels: Some((7, 5)),
            ..display(1, rect(-2.2, -1.25, 2.95, 2.5), 1.0, true, None)
        };
        let right = Display {
            mode_pixels: Some((5, 6)),
            ..display(2, rect(0.75, -0.5, 2.2, 2.0), 1.0, false, None)
        };
        let gap = Display {
            mode_pixels: Some((4, 3)),
            ..display(3, rect(3.2, -1.0, 1.3, 1.25), 1.0, false, None)
        };
        let rotated = Display {
            mode_pixels: Some((8, 4)),
            mode_points: Some((4.0, 2.0)),
            backing_pixels: Some((4.0, 8.0)),
            rotation: 90.0,
            ..display(4, rect(0.0, -2.0, 2.0, 4.0), 1.0, true, None)
        };
        let mirror = Display { id: 5, mirrors: Some(4), ..rotated };
        let other = Display {
            mode_pixels: Some((5, 6)),
            backing_pixels: Some((5.0, 6.0)),
            ..display(6, rect(-3.0, -1.5, 3.0, 3.0), 1.0, false, None)
        };
        for layout in [Layout::new(vec![gap, right, left]), Layout::new(vec![mirror, other, rotated])] {
            let displays = layout.displays.iter().map(|display| {
                let (width, height) = display.native_size().unwrap();
                let mut frame = Frame::new(width, height);
                for y in 0..height {
                    for x in 0..width {
                        let at = (y as usize * width as usize + x as usize) * 4;
                        frame.pixels[at..at + 4].copy_from_slice(&[
                            (display.id * 31 + x * 7) as u8,
                            (display.id * 23 + y * 11) as u8,
                            (display.id * 17 + x * 3 + y * 5) as u8,
                            255,
                        ]);
                    }
                }
                (*display, frame)
            }).collect::<Vec<_>>();
            let frozen = FrozenDesktop { layout: layout.clone(), displays };
            for (area, width, height) in [
                (Rect::new(-8, -8, 24, 24), 17, 13),
                (Rect::new(-6, -3, 16, 8), 23, 11),
                (Rect::new(-1, -1, 7, 5), 3, 2),
                (Rect::new(-12, -9, 13, 11), 9, 15),
                (Rect::new(-8, -8, 24, 24), 1, 1),
            ] {
                let mapping = CaptureMapping::new(area, width, height).unwrap();
                let mut expected = Frame::new(width, height);
                let mut cropped = Frame::new(width, height);
                for pixel in expected.pixels.chunks_exact_mut(4) {
                    pixel[3] = 255;
                }
                for pixel in cropped.pixels.chunks_exact_mut(4) {
                    pixel[3] = 255;
                }
                let mut intersects = false;
                for (display, source) in &frozen.displays {
                    let bounds = display.bounds;
                    let (left, top) = (area.x as f64 / layout.scale, area.y as f64 / layout.scale);
                    let (right, bottom) = ((area.x as f64 + area.width as f64) / layout.scale, (area.y as f64 + area.height as f64) / layout.scale);
                    intersects |= left < bounds.origin.x + bounds.size.width && right > bounds.origin.x
                        && top < bounds.origin.y + bounds.size.height && bottom > bounds.origin.y;
                    for y in 0..height {
                        for x in 0..width {
                            let px = (area.x as f64 + (x as f64 + 0.5) * area.width as f64 / width as f64) / layout.scale;
                            let py = (area.y as f64 + (y as f64 + 0.5) * area.height as f64 / height as f64) / layout.scale;
                            if px < bounds.origin.x || px >= bounds.origin.x + bounds.size.width
                                || py < bounds.origin.y || py >= bounds.origin.y + bounds.size.height
                            {
                                continue;
                            }
                            let sx = ((px - bounds.origin.x) * source.width as f64 / bounds.size.width).floor() as usize;
                            let sy = ((py - bounds.origin.y) * source.height as f64 / bounds.size.height).floor() as usize;
                            let from = sy * source.stride() + sx * 4;
                            let to = y as usize * expected.stride() + x as usize * 4;
                            expected.pixels[to..to + 4].copy_from_slice(&source.pixels[from..from + 4]);
                        }
                    }
                    if let Some(piece) = CapturePiece::new(display, logical(mapping, &layout)).unwrap() {
                        let (x, y) = piece.origin;
                        let (w, h) = (piece.size.0 as usize, piece.size.1 as usize);
                        let mut pixels = Vec::with_capacity(w * h * 4);
                        for row in y..y + h {
                            let at = row * source.stride() + x * 4;
                            pixels.extend_from_slice(&source.pixels[at..at + w * 4]);
                        }
                        piece.composite(&mut cropped, &pixels, piece.size, piece.origin).unwrap();
                    }
                }
                if intersects {
                    assert_eq!(frozen.render_frozen(mapping).unwrap().pixels, expected.pixels, "{mapping:?}");
                    assert_eq!(cropped.pixels, expected.pixels, "{mapping:?}");
                } else {
                    assert!(frozen.render_frozen(mapping).is_err());
                }
            }
        }
    }

    #[test]
    fn a_logical_intersection_without_sample_centers_is_an_opaque_black_pixel() {
        let display = display(1, rect(0.0, 0.0, 1.0, 1.0), 1.0, true, None);
        let layout = Layout::new(vec![display]);
        let source = Frame { width: 1, height: 1, pixels: vec![10, 20, 30, 255], alpha: false };
        let frozen = FrozenDesktop { layout, displays: vec![(display, source)] };
        let mapping = CaptureMapping::new(Rect::new(0, 0, 4, 4), 1, 1).unwrap();
        assert_eq!(frozen.render_frozen(mapping).unwrap().pixels, [0, 0, 0, 255]);
    }

    #[test]
    fn non_finite_display_bounds_are_rejected() {
        let base = display(1, rect(0.0, 0.0, 1.0, 1.0), 1.0, true, None);
        for bounds in [rect(f64::NAN, 0.0, 1.0, 1.0), rect(0.0, 0.0, f64::INFINITY, 1.0)] {
            assert!(Display { bounds, ..base }.native_size().is_err());
        }
    }

    #[test]
    fn fractional_seams_assign_a_center_pixel_to_the_display_starting_at_the_edge() {
        let left = Display {
            mode_pixels: Some((2, 2)),
            ..display(2, rect(-1.0, 0.0, 1.25, 1.0), 1.0, false, None)
        };
        let right = display(1, rect(0.25, 0.0, 1.0, 1.0), 2.0, true, None);
        let layout = Layout::new(vec![left, right]);
        let mapping = layout.mapping(Rect::new(-2, 0, 4, 2)).unwrap();
        assert_eq!(piece(&left, logical(mapping, &layout)).unwrap().1, Rect::new(0, 0, 2, 2));
        assert_eq!(piece(&right, logical(mapping, &layout)).unwrap().1, Rect::new(2, 0, 2, 2));
        let fill = |value| Frame { width: 2, height: 2, pixels: vec![value, 0, 0, 255].repeat(4), alpha: false };
        let frozen = FrozenDesktop { layout, displays: vec![(left, fill(10)), (right, fill(60))] };
        let output = frozen.render_frozen(mapping).unwrap();
        assert_eq!(output.pixels.chunks_exact(4).take(4).map(|pixel| pixel[0]).collect::<Vec<_>>(), [10, 10, 60, 60]);
    }

    #[test]
    fn fractional_display_intersection_and_color_use_logical_bounds() {
        let high = display(1, rect(0.3, 0.0, 2.0, 2.0), 2.0, true, None);
        let low = display(2, rect(-1.7, 0.0, 2.0, 2.0), 1.0, false, None);
        let layout = Layout::new(vec![high, low]);
        assert_eq!(layout.mapping(Rect::new(-1, 0, 2, 2)).unwrap().size(), (2, 2));

        let display = display(3, rect(0.1, 0.0, 1.5, 1.0), 2.0, true, None);
        let layout = Layout::new(vec![display]);
        let pixels = [10, 20, 30].into_iter().flat_map(|value| [value, 0, 0, 255]).collect::<Vec<_>>().repeat(2);
        let frozen = FrozenDesktop { layout, displays: vec![(display, Frame { width: 3, height: 2, pixels, alpha: false })] };
        assert_eq!(frozen.color_at(0, 0), None);
        assert_eq!(frozen.color_at(1, 0), Some((0, 0, 10)));
        assert_eq!(frozen.color_at(3, 0), Some((0, 0, 30)));
        assert_eq!(frozen.color_at(4, 0), None);
    }

    #[test]
    fn precise_frozen_color_uses_the_native_pixel_on_the_correct_side_of_a_seam() {
        let left = display(2, rect(-8.125, 0.0, 8.0, 8.0), 1.5, false, None);
        let right = display(1, rect(-0.125, 0.0, 8.0, 8.0), 2.0, true, None);
        let layout = Layout::new(vec![left, right]);
        let fill = |size, value| Frame { width: size, height: size, pixels: vec![value, 0, 0, 255].repeat((size * size) as usize), alpha: false };
        let frozen = FrozenDesktop { layout, displays: vec![(left, fill(12, 10)), (right, fill(16, 60))] };
        assert_eq!(frozen.color_at_precise(-0.26, 2.0), Some((0, 0, 10)));
        assert_eq!(frozen.color_at_precise(-0.25, 2.0), Some((0, 0, 60)));
        assert_eq!(frozen.color_at_precise(-0.24, 2.0), Some((0, 0, 60)));
        assert_eq!(frozen.color_at(0, 2), Some((0, 0, 60)));
        assert_eq!(frozen.color_at_precise(f64::NAN, 2.0), None);
        assert_eq!(frozen.color_at_precise(0.0, f64::INFINITY), None);
    }

    #[test]
    fn native_dimensions_follow_display_rotation() {
        let rotated = Display {
            bounds: rect(0.0, 0.0, 600.0, 800.0),
            mode_pixels: Some((1600, 1200)),
            mode_points: Some((800.0, 600.0)),
            backing_pixels: Some((1200.0, 1600.0)),
            rotation: 90.0,
            ..display(1, rect(0.0, 0.0, 600.0, 800.0), 2.0, true, None)
        };
        assert_eq!(rotated.native_size().unwrap(), (1200, 1600));
        assert_eq!(rotated.density().unwrap(), (2.0, 2.0));
        assert!(Display { backing_pixels: None, ..rotated }.native_size().unwrap_err().to_string().contains("rotation"));

        let already_oriented = Display {
            mode_pixels: Some((1200, 1600)),
            mode_points: Some((600.0, 800.0)),
            ..rotated
        };
        assert_eq!(already_oriented.native_size().unwrap(), (1200, 1600));
    }

    #[test]
    fn native_dimensions_do_not_assume_isotropic_density() {
        let anisotropic = Display {
            mode_pixels: Some((300, 400)),
            ..display(1, rect(0.0, 0.0, 300.0, 200.0), 1.0, true, None)
        };
        assert_eq!(anisotropic.native_size().unwrap(), (300, 400));
        assert_eq!(anisotropic.density().unwrap(), (1.0, 2.0));

        let backing = Display { backing_pixels: Some((300.0, 400.0)), mode_points: Some((200.0, 300.0)), ..anisotropic };
        assert_eq!(backing.native_size().unwrap(), (300, 400));
        assert!(Display { backing_pixels: Some((300.5, 400.0)), ..backing }.native_size().is_err());
        assert!(Display { backing_pixels: Some((f64::NAN, 400.0)), ..backing }.native_size().is_err());
    }

    #[test]
    fn ambiguous_mode_orientation_requires_backing_dimensions() {
        let square = Display {
            mode_pixels: Some((300, 400)),
            ..display(1, rect(0.0, 0.0, 200.0, 200.0), 1.0, true, None)
        };
        assert!(square.native_size().unwrap_err().to_string().contains("orientation"));
        assert_eq!(Display { backing_pixels: Some((300.0, 400.0)), ..square }.native_size().unwrap(), (300, 400));
        let mismatched = Display {
            mode_points: Some((500.0, 500.0)),
            ..display(1, rect(0.0, 0.0, 300.0, 200.0), 1.0, true, None)
        };
        assert!(mismatched.native_size().unwrap_err().to_string().contains("orientation"));
    }

    #[test]
    fn appkit_metrics_match_display_id_frame_and_generation() {
        let bounds = rect(0.0, 0.0, 800.0, 600.0);
        let first = AppKitScreen { id: Some(1), frame: bounds, work: bounds, backing_pixels: (800.0, 600.0) };
        let second = AppKitScreen { id: Some(2), frame: bounds, work: bounds, backing_pixels: (1600.0, 1200.0) };
        let screens = AppKitScreens { initialized: true, generation: 4, screens: vec![first, second] };
        assert_eq!(screens.find(2, bounds, 4).unwrap().backing_pixels, (1600.0, 1200.0));
        assert!(screens.find(3, bounds, 4).is_none());
        assert!(screens.find(1, bounds, 5).is_none());
        assert!(screens.find(1, rect(0.0, 0.0, 900.0, 600.0), 4).is_none());
    }

    #[test]
    fn mirrored_slaves_are_not_composited_twice() {
        let bounds = rect(0.0, 0.0, 800.0, 600.0);
        let layout = Layout::new(vec![
            display(3, rect(800.0, 0.0, 300.0, 400.0), 1.0, false, Some(99)),
            display(2, bounds, 2.0, false, Some(1)),
            display(1, bounds, 1.0, true, None),
        ]);
        assert_eq!(layout.displays.iter().map(|display| display.id).collect::<Vec<_>>(), [1, 3]);
        assert_eq!(layout.scale, 1.0);
    }

    #[test]
    fn changed_display_configuration_fails_instead_of_using_stale_geometry() {
        let display = display(1, rect(0.0, 0.0, 800.0, 600.0), 2.0, true, None);
        let original = Layout::new(vec![display]);
        assert!(ensure_layout(&original, &Layout::new(vec![display])).is_ok());
        let moved = Layout::new(vec![Display { bounds: rect(-100.0, 0.0, 800.0, 600.0), ..display }]);
        assert!(ensure_layout(&original, &moved).unwrap_err().to_string().contains("display configuration changed"));
        let scaled = Layout::new(vec![Display { scale: 1.0, ..display }]);
        assert!(ensure_layout(&original, &scaled).is_err());
        let rotated = Layout::new(vec![Display { rotation: 180.0, ..display }]);
        assert!(ensure_layout(&original, &rotated).is_err());
        let changed_mode = Layout::new(vec![Display { mode_pixels: Some((1600, 1300)), ..display }]);
        assert!(ensure_layout(&original, &changed_mode).is_err());
        let restored_layout = Layout { generation: 1, ..original.clone() };
        assert!(ensure_layout(&original, &restored_layout).is_err());
        assert!(ensure_layout(&original, &Layout::new(Vec::new())).is_err());
    }

    #[test]
    fn observed_reconfiguration_invalidates_a_retained_layout() {
        let mut layout = DisplayLayout::current().expect("an active display");
        assert!(layout.validate().is_ok());
        assert_eq!(
            layout.virtual_screen(),
            layout.monitors().iter().map(|monitor| monitor.rect).reduce(Rect::union).unwrap()
        );
        assert_eq!(layout.desktop_scale(), layout.layout.scale);

        // Simulate a pre-change snapshot without invalidating concurrent tests.
        layout.layout.generation = layout.layout.generation.wrapping_sub(1);
        assert!(layout.validate().unwrap_err().to_string().contains("display configuration changed"));
        let error = layout.grab(Rect::new(0, 0, 1, 1)).err().expect("a stale layout must not capture");
        assert!(error.to_string().contains("display configuration changed"));
        let error = Grabber::for_layout(Rect::new(0, 0, 1, 1), &layout).err().expect("a stale layout must not record");
        assert!(error.to_string().contains("display configuration changed"));
        assert!(DisplayLayout::current().unwrap().validate().is_ok());
    }

    #[test]
    fn logical_capture_entry_points_reject_stale_layouts() {
        let mut layout = DisplayLayout::current().expect("an active display");
        let display = layout.layout.displays.first().expect("an active display");
        let x = display.bounds.origin.x;
        let y = display.bounds.origin.y;
        let bounds = LogicalBounds::new([x + 0.125, y + 0.125, x + 0.625, y + 0.625]).unwrap();
        let mapping = LogicalCaptureMapping::new(bounds, (1, 1)).unwrap();
        layout.layout.generation = layout.layout.generation.wrapping_sub(1);
        let frozen = FrozenDesktop { layout: layout.layout.clone(), displays: Vec::new() };

        for error in [
            layout.logical_virtual_bounds().unwrap_err(),
            layout.logical_mapping(bounds).unwrap_err(),
            layout.grab_logical(mapping).err().expect("a stale layout must not capture"),
            frozen.render_logical(mapping).err().expect("a stale layout must not render"),
            Grabber::for_logical(mapping, &layout).err().expect("a stale layout must not record"),
        ] {
            assert!(error.to_string().contains("display configuration changed"));
        }
    }

    #[test]
    fn work_area_matching_uses_the_entire_display_frame() {
        let display = rect(0.0, 0.0, 800.0, 600.0);
        assert!(same_frame(display, rect(0.5, 0.0, 800.0, 600.0)));
        assert!(!same_frame(display, rect(0.0, 0.0, 1200.0, 600.0)));
        assert!(!same_frame(display, rect(0.0, 0.0, 800.0, 800.0)));
    }

    #[test]
    fn recording_reuses_an_unchanged_frame_without_waiting() {
        let latest = (Mutex::new(LatestFrame::default()), Condvar::new());
        let started = std::time::Instant::now();
        assert!(next_frame(&latest, true, Duration::from_secs(1)).unwrap().is_none());
        assert!(started.elapsed() < Duration::from_millis(250));
        assert_eq!(next_frame(&latest, false, Duration::from_millis(10)).unwrap_err().kind(), io::ErrorKind::TimedOut);
        latest.0.lock().unwrap().pixels = Some(vec![1, 2, 3, 4]);
        assert_eq!(next_frame(&latest, false, Duration::from_secs(1)).unwrap(), Some(vec![1, 2, 3, 4]));
        latest.0.lock().unwrap().failure = Some("screen capture was suspended".into());
        assert!(next_frame(&latest, true, Duration::from_secs(1)).unwrap_err().to_string().contains("suspended"));
    }

    #[test]
    fn filter_refresh_uses_a_fresh_snapshot_only_when_needed() {
        let latest = (Mutex::new(LatestFrame::default()), Condvar::new());
        ensure_fresh_frame(&latest, || Ok(vec![1, 2, 3, 4])).unwrap();
        assert_eq!(next_frame(&latest, false, Duration::ZERO).unwrap(), Some(vec![1, 2, 3, 4]));

        latest.0.lock().unwrap().pixels = Some(vec![5, 6, 7, 8]);
        ensure_fresh_frame(&latest, || panic!("a stream frame is already available")).unwrap();
        assert_eq!(next_frame(&latest, false, Duration::ZERO).unwrap(), Some(vec![5, 6, 7, 8]));

        ensure_fresh_frame(&latest, || {
            latest.0.lock().unwrap().pixels = Some(vec![9, 10, 11, 12]);
            Ok(vec![1, 2, 3, 4])
        }).unwrap();
        assert_eq!(next_frame(&latest, false, Duration::ZERO).unwrap(), Some(vec![9, 10, 11, 12]));

        assert!(ensure_fresh_frame(&latest, || Err(io::Error::other("snapshot failed"))).is_err());
        assert!(next_frame(&latest, false, Duration::ZERO).is_err());
    }

    #[test]
    #[ignore = "requires screen-recording permission and a display"]
    fn recording_reports_a_stopped_stream_instead_of_reusing_a_frame() {
        let display = monitors().into_iter().next().expect("a display");
        let rect = Rect::new(display.rect.x, display.rect.y, 64, 64);
        let mut grabber = Grabber::new(rect).expect("recording stream");
        grabber.grab().expect("first frame");
        *grabber.parts[0].stream.failure_state().lock().unwrap() = Some("test stream failure".into());
        let error = grabber.grab().unwrap_err().to_string();
        assert!(error.contains("screen recording stopped: test stream failure"), "{error}");
    }

    #[test]
    fn monitors_cover_the_virtual_screen() {
        let list = monitors();
        assert!(!list.is_empty());
        assert!(list[0].primary);
        let all = virtual_screen();
        for m in &list {
            assert_eq!(all.intersect(m.rect), Some(m.rect));
            assert!(m.dpi >= 96);
            assert!(m.rect.intersect(m.work) == Some(m.work));
        }
        let c = list[0].rect;
        assert_eq!(monitor_at(c.x + 1, c.y + 1).unwrap().device, list[0].device);
    }

    #[test]
    fn points_and_pixels_round_trip() {
        let r = Rect::new(100, 60, 400, 300);
        assert_eq!(rect_from_points(rect_to_points(r)), r);
    }

    #[test]
    fn cursor_draws_with_alpha() {
        let snap = CursorSnapshot::from_image(
            Frame { width: 2, height: 1, pixels: vec![0, 0, 255, 255, 0, 0, 255, 0], alpha: true },
            (1, 0),
        ).unwrap();
        let mut f = Frame::new(4, 1);
        snap.draw_onto(&mut f, (0, 0)).unwrap();
        assert_eq!(&f.pixels[4..8][..3], &[0, 0, 255]);
        assert_eq!(&f.pixels[8..12][..3], &[0, 0, 0]);
    }

    #[test]
    fn cursor_draw_composites_straight_alpha_and_handles_extreme_origins() {
        let snap = CursorSnapshot::from_image(
            Frame { width: 1, height: 1, pixels: vec![255, 0, 0, 128], alpha: true },
            (i32::MIN, 0),
        ).unwrap();
        let mut frame = Frame { width: 1, height: 1, pixels: vec![0, 0, 255, 128], alpha: true };
        snap.draw_onto(&mut frame, (i32::MIN, 0)).unwrap();
        assert_eq!(frame.pixels, [170, 0, 85, 192]);
        snap.draw_onto(&mut frame, (i32::MAX, 0)).unwrap();
        assert_eq!(frame.pixels, [170, 0, 85, 192]);
    }

    #[test]
    fn cursor_is_scaled_and_positioned_in_capture_pixels() {
        let snap = CursorSnapshot::from_image(
            Frame { width: 2, height: 2, pixels: vec![0, 0, 255, 255].repeat(4), alpha: true },
            (102, 50),
        ).unwrap();
        let mapping = CaptureMapping::new(Rect::new(100, 50, 8, 4), 4, 2).unwrap();
        let (image, position) = snap.mapped_image(mapping).unwrap();
        assert_eq!((image.width, image.height, position), (1, 1, (1, 0)));
        let mut frame = Frame::new(4, 2);
        snap.draw_onto_mapped(&mut frame, mapping).unwrap();
        assert_eq!(frame.rgb(1, 0), Some((255, 0, 0)));
        assert_eq!(frame.rgb(2, 0), Some((0, 0, 0)));
    }

    #[test]
    fn cursor_mapping_uses_precise_capture_edges() {
        let snap = CursorSnapshot::from_image(
            Frame { width: 2, height: 2, pixels: vec![0, 0, 255, 255].repeat(4), alpha: true },
            (1, 0),
        ).unwrap();
        let mapping = CaptureMapping::with_desktop_bounds(Rect::new(1, -1, 4, 5), 4, 4, [0.5, -0.5, 4.5, 3.5]).unwrap();
        let (image, position) = snap.mapped_image(mapping).unwrap();
        assert_eq!((image.width, image.height, position), (2, 2, (0, 0)));
        assert!(snap.intersects_mapping(mapping));
        let mut frame = Frame::new(4, 4);
        snap.draw_onto_mapped(&mut frame, mapping).unwrap();
        assert_eq!(frame.rgb(0, 0), Some((255, 0, 0)));
        assert_eq!(frame.rgb(1, 1), Some((255, 0, 0)));
        assert_eq!(frame.rgb(2, 1), Some((0, 0, 0)));
    }

    #[test]
    fn cursor_at_a_downsampled_edge_remains_visible() {
        let snap = CursorSnapshot::from_image(
            Frame { width: 2, height: 2, pixels: vec![0, 0, 255, 255].repeat(4), alpha: true },
            (3, 3),
        ).unwrap();
        let mapping = CaptureMapping::new(Rect::new(0, 0, 4, 4), 2, 2).unwrap();
        let (image, position) = snap.mapped_image(mapping).unwrap();
        assert_eq!((image.width, image.height, position), (1, 1, (1, 1)));
        let mut frame = Frame::new(2, 2);
        snap.draw_onto_mapped(&mut frame, mapping).unwrap();
        assert_eq!(frame.rgb(1, 1), Some((255, 0, 0)));
        assert!(snap.intersects(Rect::new(4, 4, 1, 1)));
        assert!(!snap.intersects(Rect::new(5, 5, 1, 1)));
    }

    #[test]
    fn fractional_cursor_pixels_are_sampled_at_output_centers() {
        let image = Frame {
            width: 2,
            height: 1,
            pixels: vec![0, 0, 255, 255, 255, 0, 0, 255],
            alpha: true,
        };
        let mapping = CaptureMapping::identity(Rect::new(0, 0, 4, 4)).unwrap();
        let inside = CursorSnapshot::from_image_with_bounds(image.clone(), [0.75, 0.0, 2.75, 1.0]).unwrap();
        let (mapped, position) = inside.mapped_image(mapping).unwrap();
        assert_eq!((mapped.width, mapped.height, position), (2, 1, (1, 0)));
        let mut frame = Frame::new(4, 4);
        inside.draw_onto_mapped(&mut frame, mapping).unwrap();
        assert_eq!(frame.rgb(0, 0), Some((0, 0, 0)));
        assert_eq!(frame.rgb(1, 0), Some((255, 0, 0)));
        assert_eq!(frame.rgb(2, 0), Some((0, 0, 255)));

        let edge = CursorSnapshot::from_image_with_bounds(image.clone(), [-0.6, 0.2, 0.6, 1.2]).unwrap();
        let (mapped, position) = edge.mapped_image(mapping).unwrap();
        assert_eq!((mapped.width, mapped.height, position), (2, 1, (-1, 0)));
        let mut frame = Frame::new(4, 4);
        edge.draw_onto_mapped(&mut frame, mapping).unwrap();
        assert_eq!(frame.rgb(0, 0), Some((0, 0, 255)));

        let tiny = CursorSnapshot::from_image_with_bounds(image, [1.01, 1.01, 1.1, 1.1]).unwrap();
        let (mapped, position) = tiny.mapped_image(mapping).unwrap();
        assert_eq!((mapped.width, mapped.height, position), (1, 1, (1, 1)));
    }

    #[test]
    fn cursor_composition_matches_independent_fractional_pixel_sampling() {
        let image = Frame {
            width: 3,
            height: 2,
            pixels: (0..6).flat_map(|index| [10 + index * 20, 30 + index * 10, 200 - index * 15, 255]).collect(),
            alpha: true,
        };
        let areas = [
            [-4.25, 4.375, -2.875, 6.125],
            [-1.375, 5.75, 1.875, 8.25],
            [15.875, 13.375, 17.125, 15.25],
            [16.875, 7.0, 19.0, 9.0],
        ];
        for scale in [1.0, 1.5, 2.0, 3.0] {
            let bounds = [-3.25_f64, 4.25, 16.75, 14.25].map(|edge| edge * scale);
            let edges = bounds.map(|edge| edge.round() as i32);
            let rect = Rect::from_ltrb(edges[0], edges[1], edges[2], edges[3]);
            let mapping = CaptureMapping::with_desktop_bounds(rect, 40, 20, bounds).unwrap();
            for area in areas {
                let cursor = CursorSnapshot::from_image_with_bounds(image.clone(), area.map(|edge| edge * scale)).unwrap();
                let mut frame = Frame::new(40, 20);
                cursor.draw_onto_mapped(&mut frame, mapping).unwrap();
                for y in 0..frame.height {
                    for x in 0..frame.width {
                        let (px, py) = mapping.pixel_center(x, y);
                        let (px, py) = (px / scale, py / scale);
                        let expected = if px >= area[0] && px < area[2] && py >= area[1] && py < area[3] {
                            let sx = ((px - area[0]) * image.width as f64 / (area[2] - area[0])).floor() as i32;
                            let sy = ((py - area[1]) * image.height as f64 / (area[3] - area[1])).floor() as i32;
                            image.rgb(sx, sy)
                        } else {
                            Some((0, 0, 0))
                        };
                        assert_eq!(frame.rgb(x as i32, y as i32), expected, "scale {scale}, area {area:?}, pixel ({x}, {y})");
                    }
                }
            }
        }
    }

    #[test]
    fn cursor_mapping_retains_fractional_origin_and_extent_across_reference_scales() {
        let image = Frame { width: 6, height: 4, pixels: vec![0, 0, 255, 255].repeat(24), alpha: true };
        let mut expected = None;
        for scale in [1.0, 1.5, 2.0, 3.0] {
            let bounds = [-3.25_f64, 4.25, 16.75, 14.25].map(|edge| edge * scale);
            let edges = bounds.map(|edge| edge.round() as i32);
            let rect = Rect::from_ltrb(edges[0], edges[1], edges[2], edges[3]);
            let mapping = CaptureMapping::with_desktop_bounds(rect, 40, 20, bounds).unwrap();
            let cursor = CursorSnapshot::from_system_image(
                image.clone(),
                (-1.25, 5.75),
                (0.5, 0.5),
                (3.0, 2.0),
                scale,
            ).unwrap();
            let (mapped, position) = cursor.mapped_image(mapping).unwrap();
            assert_eq!((mapped.width, mapped.height, position), (6, 4, (3, 2)));
            let mut frame = Frame::new(40, 20);
            cursor.draw_onto_mapped(&mut frame, mapping).unwrap();
            if let Some(expected) = &expected {
                assert_eq!(&frame.pixels, expected);
            } else {
                expected = Some(frame.pixels);
            }
        }
    }

    #[test]
    fn cursor_intersection_uses_precise_bounds_and_rejects_invalid_snapshots() {
        let image = Frame { width: 2, height: 2, pixels: vec![0, 0, 255, 255].repeat(4), alpha: true };
        let mapping = CaptureMapping::with_desktop_bounds(
            Rect::new(0, 0, 4, 4), 4, 4, [0.25, 0.25, 4.25, 4.25],
        ).unwrap();
        let touching = CursorSnapshot::from_image_with_bounds(image.clone(), [4.1, 1.0, 5.1, 2.0]).unwrap();
        let outside = CursorSnapshot::from_image_with_bounds(image.clone(), [4.26, 1.0, 5.26, 2.0]).unwrap();
        assert!(touching.intersects_mapping(mapping));
        assert!(!outside.intersects_mapping(mapping));
        let mut shifted = outside;
        shifted.x -= 1;
        assert!(shifted.intersects_mapping(mapping));
        for invalid in [
            [f64::NAN, 0.0, 1.0, 1.0],
            [0.0, 0.0, f64::INFINITY, 1.0],
            [0.0, 0.0, 0.0, 1.0],
            [i32::MAX as f64 + 1.0, 0.0, i32::MAX as f64 + 2.0, 1.0],
        ] {
            assert!(CursorSnapshot::from_image_with_bounds(image.clone(), invalid).is_err());
        }
        assert!(CursorSnapshot::from_system_image(image.clone(), (0.0, 0.0), (0.0, 0.0), (1.0, 1.0), f64::NAN).is_err());
        assert!(CursorSnapshot::from_system_image(image, (0.0, 0.0), (f64::INFINITY, 0.0), (1.0, 1.0), 2.0).is_err());
    }

    #[test]
    fn cursor_mapping_rejects_unrepresentable_magnification() {
        let cursor = CursorSnapshot::from_image(Frame::new(2, 2), (0, 0)).unwrap();
        let mapping = CaptureMapping::with_desktop_bounds(
            Rect::new(0, 0, 1, 1), 2, 2, [0.499_999_999_999, 0.0, 0.500_000_000_001, 1.0],
        ).unwrap();
        assert!(cursor.mapped_image(mapping).is_err());
    }
}
