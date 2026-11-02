//! Monitors, the virtual desktop, GDI screen grabs and the mouse cursor.
//!
//! GDI `BitBlt` is used for now: it is fast enough to freeze the whole desktop
//! before the overlay opens. Content that GDI cannot read (some
//! hardware-overlay video) needs Windows.Graphics.Capture, planned later.

use crate::gdi::Dib;
use crate::{from_wide, Frame, Rect};
use std::io;
use windows_sys::Win32::Foundation::{BOOL, LPARAM, POINT, RECT, TRUE};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, DeleteObject, EnumDisplayMonitors, GetDC, GetMonitorInfoW, GetObjectW,
    MonitorFromPoint, ReleaseDC, BITMAP, CAPTUREBLT, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
    MONITOR_DEFAULTTONEAREST, SRCCOPY,
};
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForMonitor, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    MDT_EFFECTIVE_DPI,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CopyIcon, CopyImage, DestroyIcon, DrawIconEx, GetCursorInfo, GetIconInfo, GetSystemMetrics,
    SystemParametersInfoW, CURSORINFO, CURSOR_SHOWING, DI_NORMAL, HICON, ICONINFO, IMAGE_CURSOR,
    LR_COPYFROMRESOURCE, MONITORINFOF_PRIMARY, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// Opts the process into per-monitor-v2 DPI awareness so every coordinate is
/// in physical pixels. Must run before any window is created.
pub fn enable_per_monitor_dpi() {
    // SAFETY: a process-wide setting with no pointer arguments; failure
    // (already set, e.g. by a manifest) is harmless.
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// The bounding rectangle of all monitors.
pub fn virtual_screen() -> Rect {
    // SAFETY: no arguments beyond a metric index.
    unsafe {
        Rect::new(
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

#[derive(Clone, Debug)]
pub struct Monitor {
    pub rect: Rect,
    pub work: Rect,
    pub dpi: u32,
    pub primary: bool,
    pub device: String,
}

impl Monitor {
    /// UI scale relative to 96 DPI.
    pub fn scale(&self) -> f32 {
        self.dpi as f32 / 96.0
    }

    /// Desktop-coordinate pixels per window DIP.
    pub fn desktop_scale(&self) -> f64 {
        self.scale() as f64
    }
}

fn rect(r: RECT) -> Rect {
    Rect::from_ltrb(r.left, r.top, r.right, r.bottom)
}

fn describe_monitor(handle: HMONITOR) -> Option<Monitor> {
    // SAFETY: `info` is a correctly sized MONITORINFOEXW; the DPI outputs are
    // plain integers.
    unsafe {
        let mut info: MONITORINFOEXW = std::mem::zeroed();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(handle, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) == 0 {
            return None;
        }
        let (mut dpi_x, mut dpi_y) = (96, 96);
        GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        Some(Monitor {
            rect: rect(info.monitorInfo.rcMonitor),
            work: rect(info.monitorInfo.rcWork),
            dpi: dpi_x,
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            device: from_wide(&info.szDevice),
        })
    }
}

pub fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn collect(
        handle: HMONITOR,
        _: HDC,
        _: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        // SAFETY: `data` is the `Vec` passed below, alive for the whole call.
        let list = &mut *(data as *mut Vec<Monitor>);
        list.extend(describe_monitor(handle));
        TRUE
    }
    let mut list: Vec<Monitor> = Vec::new();
    // SAFETY: the callback only touches `list` through the pointer it is given.
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(collect),
            &mut list as *mut _ as LPARAM,
        );
    }
    list
}

/// The monitor containing (or nearest to) a screen point.
pub fn monitor_at(x: i32, y: i32) -> Option<Monitor> {
    // SAFETY: MonitorFromPoint with DEFAULTTONEAREST always returns a monitor.
    describe_monitor(unsafe { MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST) })
}

/// Copies a rectangle of the screen (screen coordinates) into a frame.
pub fn grab(r: Rect) -> io::Result<Frame> {
    if r.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty capture rectangle",
        ));
    }
    let dib = Dib::new(r.width, r.height)?;
    // SAFETY: the screen DC is released before returning; `dib` owns its DC.
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        let copied = BitBlt(
            dib.dc(),
            0,
            0,
            r.width,
            r.height,
            screen,
            r.x,
            r.y,
            SRCCOPY | CAPTUREBLT,
        );
        ReleaseDC(std::ptr::null_mut(), screen);
        if copied == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(dib.to_frame())
}

/// Repeated grabs of one rectangle into a reusable bitmap (screen
/// recording).
pub struct Grabber {
    rect: Rect,
    dib: Dib,
}

impl Grabber {
    pub fn new(rect: Rect) -> io::Result<Grabber> {
        Ok(Grabber {
            rect,
            dib: Dib::new(rect.width, rect.height)?,
        })
    }

    /// Grabs the rectangle; returns its pixels (top-down BGRX).
    pub fn grab(&mut self) -> io::Result<&[u8]> {
        let r = self.rect;
        // SAFETY: as in `grab`.
        unsafe {
            let screen = GetDC(std::ptr::null_mut());
            let copied = BitBlt(
                self.dib.dc(),
                0,
                0,
                r.width,
                r.height,
                screen,
                r.x,
                r.y,
                SRCCOPY | CAPTUREBLT,
            );
            ReleaseDC(std::ptr::null_mut(), screen);
            if copied == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(self.dib.pixels())
    }
}

/// The visible mouse cursor at one moment, kept so it can be composited onto
/// a capture taken from a frozen snapshot.
pub struct CursorSnapshot {
    icon: HICON,
    /// Top-left of the cursor image in screen coordinates.
    pub x: i32,
    pub y: i32,
    width: i32,
    height: i32,
}

/// The default cursor size Windows designs cursor images for.
const CURSOR_DEFAULT_SIZE: i32 = 32;
/// Undocumented `SPI_GETCURSORBASESIZE`: the pointer size chosen in
/// Settings > Accessibility > Mouse pointer (32 is the default).
const SPI_GETCURSORBASESIZE: u32 = 0x2028;

/// The user's pointer size. Cursor handles keep their nominal (32 px)
/// bitmaps and the system enlarges them only when drawing the pointer, so a
/// capture must scale them by the same factor.
fn cursor_base_size() -> i32 {
    let mut size: u32 = 0;
    // SAFETY: the out-parameter is a plain u32.
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCURSORBASESIZE,
            0,
            &mut size as *mut u32 as *mut core::ffi::c_void,
            0,
        )
    };
    if ok == 0 || !(CURSOR_DEFAULT_SIZE as u32..=1024).contains(&size) {
        CURSOR_DEFAULT_SIZE
    } else {
        size as i32
    }
}

/// Hotspot and drawn size of an icon or cursor.
///
/// # Safety
/// `icon` must be a valid icon or cursor handle.
unsafe fn icon_metrics(icon: HICON) -> Option<(i32, i32, i32, i32)> {
    let mut icon_info: ICONINFO = std::mem::zeroed();
    if GetIconInfo(icon, &mut icon_info) == 0 {
        return None;
    }
    let mut bitmap: BITMAP = std::mem::zeroed();
    let has_size = !icon_info.hbmMask.is_null()
        && GetObjectW(
            icon_info.hbmMask,
            std::mem::size_of::<BITMAP>() as i32,
            &mut bitmap as *mut BITMAP as *mut core::ffi::c_void,
        ) != 0;
    // Monochrome cursors stack the AND and XOR masks in one bitmap.
    let monochrome = icon_info.hbmColor.is_null();
    if !icon_info.hbmMask.is_null() {
        DeleteObject(icon_info.hbmMask);
    }
    if !icon_info.hbmColor.is_null() {
        DeleteObject(icon_info.hbmColor);
    }
    let (width, height) = if has_size {
        let height = if monochrome {
            bitmap.bmHeight / 2
        } else {
            bitmap.bmHeight
        };
        (bitmap.bmWidth, height)
    } else {
        (CURSOR_DEFAULT_SIZE, CURSOR_DEFAULT_SIZE)
    };
    Some((
        icon_info.xHotspot as i32,
        icon_info.yHotspot as i32,
        width,
        height,
    ))
}

/// A copy of `cursor` enlarged for the user's pointer size, preferring the
/// cursor file's own larger image over stretching the nominal one. `None`
/// at the default size or when no copy could be made.
///
/// # Safety
/// `cursor` must be a valid cursor handle.
unsafe fn scaled_copy(cursor: HICON, base_size: i32) -> Option<HICON> {
    if base_size <= CURSOR_DEFAULT_SIZE {
        return None;
    }
    let (_, _, width, height) = icon_metrics(cursor)?;
    let scale = |v: i32| (v * base_size + CURSOR_DEFAULT_SIZE / 2) / CURSOR_DEFAULT_SIZE;
    let (width, height) = (scale(width), scale(height));
    [LR_COPYFROMRESOURCE, 0].into_iter().find_map(|flags| {
        let copy = CopyImage(cursor, IMAGE_CURSOR, width, height, flags);
        (!copy.is_null()).then_some(copy as HICON)
    })
}

// SAFETY: the icon is an owned copy, only drawn and destroyed.
unsafe impl Send for CursorSnapshot {}

impl CursorSnapshot {
    /// Returns `None` when the cursor is hidden.
    pub fn take() -> Option<CursorSnapshot> {
        // SAFETY: sized out-structs; the icon is copied so it outlives the
        // system's shared cursor, and the ICONINFO bitmaps are released.
        unsafe {
            let mut info: CURSORINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<CURSORINFO>() as u32;
            if GetCursorInfo(&mut info) == 0
                || info.flags & CURSOR_SHOWING == 0
                || info.hCursor.is_null()
            {
                return None;
            }
            let icon = scaled_copy(info.hCursor, cursor_base_size())
                .unwrap_or_else(|| CopyIcon(info.hCursor));
            if icon.is_null() {
                return None;
            }
            let (hot_x, hot_y, width, height) = icon_metrics(icon).unwrap_or((0, 0, 32, 32));
            Some(CursorSnapshot {
                icon,
                x: info.ptScreenPos.x - hot_x,
                y: info.ptScreenPos.y - hot_y,
                width,
                height,
            })
        }
    }

    /// The cursor alone, with transparency (drawn once on black and once on
    /// white; the difference gives the alpha).
    pub fn image(&self) -> io::Result<Frame> {
        let (width, height) = (self.width.max(1), self.height.max(1));
        let on = |v: u8| -> io::Result<Frame> {
            let mut f = Frame::new(width as u32, height as u32);
            f.pixels.iter_mut().for_each(|b| *b = v);
            let probe = CursorSnapshot {
                icon: self.icon,
                x: 0,
                y: 0,
                width,
                height,
            };
            let r = probe.draw_onto(&mut f, (0, 0));
            std::mem::forget(probe);
            r.map(|_| f)
        };
        let (black, white) = (on(0)?, on(255)?);
        let mut out = Frame::new(width as u32, height as u32);
        out.alpha = true;
        for i in 0..(width * height) as usize {
            let a = 255
                - (white.pixels[i * 4 + 1] as i32 - black.pixels[i * 4 + 1] as i32).clamp(0, 255);
            let un = |c: u8| {
                if a > 0 {
                    (c as i32 * 255 / a).min(255) as u8
                } else {
                    0
                }
            };
            out.pixels[i * 4..i * 4 + 4].copy_from_slice(&[
                un(black.pixels[i * 4]),
                un(black.pixels[i * 4 + 1]),
                un(black.pixels[i * 4 + 2]),
                a as u8,
            ]);
        }
        Ok(out)
    }

    /// Draws the cursor onto `frame`, whose top-left is at `origin` in screen
    /// coordinates.
    pub fn draw_onto(&self, frame: &mut Frame, origin: (i32, i32)) -> io::Result<()> {
        let mut dib = Dib::from_frame(frame)?;
        // SAFETY: draws an owned icon into an owned DC.
        unsafe {
            DrawIconEx(
                dib.dc(),
                self.x - origin.0,
                self.y - origin.1,
                self.icon,
                self.width,
                self.height,
                0,
                std::ptr::null_mut(),
                DI_NORMAL,
            );
        }
        frame.pixels.copy_from_slice(dib.pixels_mut());
        Ok(())
    }
}

impl Drop for CursorSnapshot {
    fn drop(&mut self) {
        // SAFETY: destroys the copy made in `take`.
        unsafe {
            DestroyIcon(self.icon);
        }
    }
}

#[cfg(test)]
mod cursor_scale {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{LoadCursorW, IDC_ARROW, IDC_IBEAM};

    #[test]
    fn default_size_keeps_the_nominal_cursor() {
        // SAFETY: a shared system cursor.
        let arrow = unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) };
        assert!(unsafe { scaled_copy(arrow, CURSOR_DEFAULT_SIZE) }.is_none());
    }

    #[test]
    fn enlarged_pointer_scales_image_and_hotspot() {
        // SAFETY: a shared system cursor; the copies are destroyed.
        unsafe {
            let ibeam = LoadCursorW(std::ptr::null_mut(), IDC_IBEAM);
            let (hot_x, hot_y, w, h) = icon_metrics(ibeam).unwrap();
            let copy = scaled_copy(ibeam, CURSOR_DEFAULT_SIZE * 3).unwrap();
            let (big_hot_x, big_hot_y, big_w, big_h) = icon_metrics(copy).unwrap();
            DestroyIcon(copy);
            assert_eq!((big_w, big_h), (w * 3, h * 3));
            assert!((big_hot_x - hot_x * 3).abs() <= 3, "{big_hot_x} vs {hot_x}");
            assert!((big_hot_y - hot_y * 3).abs() <= 3, "{big_hot_y} vs {hot_y}");
        }
    }

    #[test]
    fn image_covers_the_whole_cursor() {
        // SAFETY: a shared system cursor, copied and owned by the snapshot.
        let snapshot = unsafe {
            let arrow = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
            let icon = scaled_copy(arrow, CURSOR_DEFAULT_SIZE * 4).unwrap();
            let (_, _, width, height) = icon_metrics(icon).unwrap();
            CursorSnapshot {
                icon,
                x: 0,
                y: 0,
                width,
                height,
            }
        };
        let image = snapshot.image().unwrap();
        assert_eq!((image.width, image.height), (128, 128));
        let opaque_right = (0..image.height)
            .flat_map(|y| (0..image.width).map(move |x| (x, y)))
            .filter(|&(x, y)| image.pixels[((y * image.width + x) * 4 + 3) as usize] > 0)
            .map(|(x, _)| x)
            .max()
            .unwrap();
        assert!(opaque_right > 32, "arrow drawn at nominal size");
    }
}

#[cfg(test)]
mod grab_timing {
    use super::*;

    /// Prints how long whole-desktop grabs take with and without CAPTUREBLT
    /// and split per monitor across threads (run with --ignored --nocapture).
    #[test]
    #[ignore]
    fn grab_timing() {
        let desktop = virtual_screen();
        let blt = |r: Rect, flags: u32| {
            let dib = Dib::new(r.width, r.height).unwrap();
            // SAFETY: as in `grab`.
            unsafe {
                let screen = GetDC(std::ptr::null_mut());
                BitBlt(dib.dc(), 0, 0, r.width, r.height, screen, r.x, r.y, flags);
                ReleaseDC(std::ptr::null_mut(), screen);
            }
            dib.to_frame()
        };
        for round in 0..3 {
            let t = std::time::Instant::now();
            drop(blt(desktop, SRCCOPY | CAPTUREBLT));
            let with = t.elapsed();
            let t = std::time::Instant::now();
            drop(blt(desktop, SRCCOPY));
            let without = t.elapsed();
            let t = std::time::Instant::now();
            std::thread::scope(|s| {
                for m in monitors() {
                    s.spawn(move || drop(blt(m.rect, SRCCOPY | CAPTUREBLT)));
                }
            });
            let split = t.elapsed();
            println!("round {round}: CAPTUREBLT {with:?}, plain {without:?}, per monitor in threads {split:?}");
        }
    }
}
