//! One overlay window, over one monitor: its slice of the frozen screenshot
//! on a display surface, the dimming, crosshair, outlines and labels as
//! projected controls on a canvas above it, and on top an input surface
//! that takes the pointer and keys and draws the magnifier. Selection
//! geometry uses the capture side's desktop grid; each screenshot and
//! magnifier use the monitor's own image pixels.

use super::overlay_api::{self as api, OverlayState};
use super::SurfaceViewport;
use crate::ui::hud::native;
use jiffshot_capture::{CaptureMapping, Rect};
use jiffshot_doc::text::{self, TextStyle};
use rustolonia::pixel_surface::{Cursor, Interpolation, PixelFormat, Placement, SurfaceInput};
use rustolonia::{
    AppScope, AsControl, Border, Brush, Canvas, Color, FontWeight, Orientation, Panel, PixelSurface, Polyline, Result, StackPanel, TextBlock, Window,
    WindowDecorations,
};

const ACCENT: Color = Color::rgb(0x2F, 0x8C, 0xFF);
const CROSSHAIR: Color = Color::rgb(0xFF, 0xC1, 0x07);
const LABEL_BACK: Color = Color::rgb(0x1F, 0x20, 0x24);
const LABEL_TEXT: Color = Color::rgb(0xF1, 0xF3, 0xF4);
const LABEL_MUTED: Color = Color::rgb(0xA8, 0xAD, 0xB4);
const DIM: Color = Color::new(140, 2, 6, 16);
const FONT: &str = "Segoe UI";
/// The magnifier shows this many pixels across.
const SAMPLES: i32 = 15;
/// The input surface's background: transparent, but it takes the pointer.
const HIT_TEST_BACKGROUND: u32 = 0x00FF_FFFF;

/// A monitor of the overlay's desktop, in desktop pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlayMonitor {
    pub rect: Rect,
    pub scale: f64,
    pub pixels: (u32, u32),
    pub mapping: CaptureMapping,
}

/// What every window draws: the session's latest state with its texts and
/// points (desktop pixels).
#[derive(Default)]
pub struct Scene {
    pub state: OverlayState,
    pub title: String,
    pub size: String,
    pub hint: String,
    pub help: Vec<String>,
    /// Video guide bounds and their native-pixel dimensions.
    pub guides: Vec<(Rect, (i32, i32))>,
    /// The freehand outline: x, y pairs.
    pub path: Vec<i32>,
    /// Multi-area regions: x, y, w, h.
    pub regions: Vec<i32>,
}

/// A borderless, topmost window over one monitor.
pub struct MonitorWindow {
    pub id: u64,
    /// The monitor, in desktop pixels.
    pub rect: Rect,
    /// The monitor's scale, until the window reports its own.
    scale: f64,
    pixel_size: (u32, u32),
    mapping: CaptureMapping,
    pub(super) viewport: SurfaceViewport,
    pub window: Window,
    snapshot: PixelSurface,
    pub input: PixelSurface,
    decor: Decor,
    /// This monitor's part of the screenshot (BGRA, opaque): kept between
    /// captures, and the magnifier's source.
    pixels: Vec<u8>,
    /// The scaling the window last reported (to notice a change).
    pub reported_scaling: f64,
}

impl MonitorWindow {
    /// Makes the window (not shown yet); `handler` gets the input surface's
    /// input.
    pub fn new(
        scope: &AppScope,
        id: u64,
        monitor: OverlayMonitor,
        viewport: SurfaceViewport,
        handler: impl FnMut(&SurfaceInput) -> Result<bool> + Send + 'static,
    ) -> Result<Self> {
        let snapshot = scope.create_display_surface()?;
        #[cfg(windows)]
        snapshot.set_background(Some(0xFF00_0000))?;
        #[cfg(target_os = "macos")]
        snapshot.set_background(Some(0x0000_0000))?;
        let input = scope.create_pixel_surface(handler)?;
        input.set_background(Some(HIT_TEST_BACKGROUND))?;
        input.set_cursor(Cursor::Cross)?;
        let decor = Decor::new()?;
        let root = Panel::new()?.child(snapshot.control())?.child(&decor.canvas)?.child(input.control())?;
        #[cfg(windows)]
        let background = Color::rgb(0, 0, 0);
        #[cfg(target_os = "macos")]
        let background = Color::new(0, 0, 0, 0);
        let window = Window::new()?
            .title("Jiffshot selection")?
            .window_decorations(WindowDecorations::None)?
            .show_activated(false)?
            .show_in_taskbar(false)?
            .can_resize(false)?
            .background(Brush::solid(background))?
            .content(Some(&root))?;
        #[cfg(target_os = "macos")]
        scope.set_transparent(&window, true)?;
        scope.set_topmost(&window, true)?;
        let mut this = MonitorWindow {
            id,
            rect: monitor.rect,
            scale: monitor.scale,
            pixel_size: monitor.pixels,
            mapping: monitor.mapping,
            viewport,
            window,
            snapshot,
            input,
            decor,
            pixels: Vec::new(),
            reported_scaling: 0.0,
        };
        this.reserve()?;
        Ok(this)
    }

    /// Allocates the screenshot for this monitor ahead of a capture.
    fn reserve(&mut self) -> Result<()> {
        let (w, h) = self.pixel_size;
        self.pixels.resize(w as usize * h as usize * 4, 0);
        self.snapshot.set_layer(0, w, h, PixelFormat::Bgra8)?;
        self.snapshot.place_layer(0, self.viewport.placement(), Interpolation::None, 0.0)
    }

    /// Desktop-coordinate pixels per DIP.
    pub fn scaling(&self) -> f64 {
        desktop_scaling(self.scale, self.input.metrics().ok().map(|m| m.scaling))
    }

    pub fn set_monitor(&mut self, monitor: OverlayMonitor, viewport: SurfaceViewport) {
        if self.mapping != monitor.mapping || self.viewport != viewport {
            self.decor.path_shown.clear();
        }
        self.scale = monitor.scale;
        self.pixel_size = monitor.pixels;
        self.mapping = monitor.mapping;
        self.viewport = viewport;
    }

    pub fn monitor(&self) -> OverlayMonitor {
        OverlayMonitor { rect: self.rect, scale: self.scale, pixels: self.pixel_size, mapping: self.mapping }
    }

    /// A point of the window (DIPs) in desktop pixels.
    pub fn desktop(&self, x: f64, y: f64) -> (f64, f64) {
        surface_to_desktop(self.viewport.frame, self.scaling(), x, y)
    }

    /// Takes overlay `session`'s screenshot of this monitor; false when the
    /// session is over.
    pub fn load(&mut self, session: u64) -> std::result::Result<bool, String> {
        let (w, h) = self.pixel_size;
        let stride = w as usize * 4;
        self.pixels.resize(stride * h as usize, 0);
        if !api::copy(session, self.rect, w, h, &mut self.pixels, stride)? {
            return Ok(false);
        }
        make_opaque(&mut self.pixels);
        self.snapshot.set_layer(0, w, h, PixelFormat::Bgra8).map_err(|error| format!("{error:?}"))?;
        self.snapshot.write_layer(0, 0, 0, w, h, &self.pixels, stride).map_err(|error| format!("{error:?}"))?;
        self.snapshot.place_layer(0, self.viewport.placement(), Interpolation::None, 1.0).map_err(|error| format!("{error:?}"))?;
        Ok(true)
    }

    /// Lets go of the session and hides the allocated screenshot.
    pub fn unload(&mut self) -> Result<()> {
        self.decor.canvas.set_visible(false)?;
        self.input.place_layer(0, Placement::Fill, Interpolation::None, 0.0)?;
        self.snapshot.place_layer(0, self.viewport.placement(), Interpolation::None, 0.0)
    }

    /// Draws `scene` over the screenshot.
    pub fn draw(&mut self, scene: &Scene) -> Result<()> {
        let s = self.scaling();
        let magnifier = self.decor.draw(scene, self.mapping, self.viewport.frame, s)?;
        match magnifier {
            Some((at, bitmap)) => {
                let side = at.width as u32;
                self.input.set_layer(0, side, side, PixelFormat::Bgra8)?;
                // The zoomed pixels come from this monitor's screenshot.
                let bitmap = bitmap.render(&self.pixels, self.mapping, side).map_err(rustolonia::Error::Load)?;
                self.input.write_layer(0, 0, 0, side, side, &bitmap, side as usize * 4)?;
                let d = |v: i32| v as f64 / s;
                let [left, top, _, _] = self.viewport.frame;
                let placement = Placement::Rect {
                    x: (at.x as f64 - left) / s,
                    y: (at.y as f64 - top) / s,
                    width: d(at.width),
                    height: d(at.height),
                };
                self.input.place_layer(0, placement, Interpolation::None, 1.0)
            }
            None => self.input.place_layer(0, Placement::Fill, Interpolation::None, 0.0),
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) fn guide_for_selftest(&self, index: usize) -> Result<Option<super::GuideVisual>> {
        let Some((border, label)) = self.decor.guide_items.get(index) else { return Ok(None) };
        Ok(Some(super::GuideVisual {
            monitor: self.rect,
            scale: self.scaling(),
            bounds: [Canvas::get_left(border)?, Canvas::get_top(border)?, border.get_width()?, border.get_height()?],
            label: label.get_text()?,
            visible: border.get_is_visible()? && label.get_is_visible()?,
        }))
    }

    /// Native handle (0 before the window has one).
    pub fn hwnd(&self, scope: &AppScope) -> isize {
        native::handle(scope, &self.window)
    }

    /// Lets the window go for good.
    pub fn close(&self) {
        let _ = self.input.detach();
        let _ = self.window.close();
    }
}

fn surface_to_desktop(frame: [f64; 4], scale: f64, x: f64, y: f64) -> (f64, f64) {
    let [left, top, _, _] = frame;
    (left + x * scale, top + y * scale)
}

fn desktop_scaling(grid: f64, reported: Option<f64>) -> f64 {
    if cfg!(target_os = "macos") {
        grid
    } else {
        reported.filter(|scale| *scale > 0.0).unwrap_or(grid)
    }
}

fn decoration_scaling(grid: f64, monitor_scale: f64) -> f64 {
    if cfg!(target_os = "macos") { grid } else { monitor_scale }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_coordinates_use_the_capture_grid() {
        let expected = if cfg!(target_os = "macos") { 2.0 } else { 1.0 };
        assert_eq!(desktop_scaling(2.0, Some(1.0)), expected);
        assert_eq!(desktop_scaling(2.0, None), 2.0);
    }

    #[test]
    fn decoration_sizes_are_stable_across_mixed_density_monitors() {
        let low_density = decoration_scaling(2.0, 1.0) / 2.0;
        let high_density = decoration_scaling(2.0, 2.0) / 2.0;
        if cfg!(target_os = "macos") {
            assert_eq!((low_density, high_density), (1.0, 1.0));
            assert_eq!(decoration_scaling(2.0, 1.5) / 2.0, 1.0);
        } else {
            assert_eq!((low_density, high_density), (0.5, 1.0));
        }
    }

    #[test]
    fn magnifier_samples_native_pixels_at_mixed_scales_and_negative_origins() {
        let pixels: Vec<u8> = (0..15).flat_map(|index| [index, 70, 80, 255]).collect();
        let side = (SAMPLES * 4 + 2) as u32;
        for (rect, cursor) in [
            (Rect::new(-10, -6, 10, 6), (-6, -4)),
            (Rect::new(-3, -2, 5, 3), (-1, -1)),
        ] {
            let mapping = CaptureMapping::new(rect, 5, 3).unwrap();
            let image = Magnifier { cursor: (f64::from(cursor.0), f64::from(cursor.1)), zoom: 4 }.render(&pixels, mapping, side).unwrap();
            let sample = |x: usize, y: usize| {
                let offset = ((3 + y * 4) * side as usize + 3 + x * 4) * 4;
                &image[offset..offset + 4]
            };
            assert_eq!(sample(7, 7), &[7, 70, 80, 255]);
            assert_eq!(sample(6, 7), &[6, 70, 80, 255]);
            assert_eq!(sample(8, 7), &[8, 70, 80, 255]);
            assert_eq!(sample(7, 6), &[2, 70, 80, 255]);
            assert_eq!(sample(7, 8), &[12, 70, 80, 255]);
            assert_eq!(sample(4, 7), &[0, 0, 0, 255]);
            assert_eq!(sample(10, 7), &[0, 0, 0, 255]);
        }
    }

    #[test]
    fn magnifier_uses_fractional_monitor_edges() {
        let pixels: Vec<u8> = (0..15).flat_map(|index| [index, 70, 80, 255]).collect();
        let mapping = CaptureMapping::with_desktop_bounds(
            Rect::new(0, 0, 4, 3), 5, 3, [0.49, -0.49, 4.49, 2.51],
        ).unwrap();
        let side = (SAMPLES * 4 + 2) as u32;
        let image = Magnifier { cursor: (1.0, 1.0), zoom: 4 }.render(&pixels, mapping, side).unwrap();
        let sample = |x: usize, y: usize| {
            let offset = ((3 + y * 4) * side as usize + 3 + x * 4) * 4;
            &image[offset..offset + 4]
        };
        assert_eq!(sample(7, 7), &[5, 70, 80, 255]);
        assert_eq!(sample(8, 7), &[6, 70, 80, 255]);
        assert_eq!(sample(6, 7), &[0, 0, 0, 255]);
        let fractional = Magnifier { cursor: (1.3, 1.0), zoom: 4 }.render(&pixels, mapping, side).unwrap();
        let center = ((3 + 7 * 4) * side as usize + 3 + 7 * 4) * 4;
        assert_eq!(&fractional[center..center + 4], &[6, 70, 80, 255]);
        assert!(Magnifier { cursor: (f64::NAN, 1.0), zoom: 4 }.render(&pixels, mapping, side).is_err());
        assert_eq!(surface_to_desktop(mapping.desktop_bounds(), 2.0, 0.25, 0.75), (0.99, 1.01));
        assert_eq!(surface_to_desktop([-2.0, -2.0, 6.0, 4.0], 2.0, 1.25, 1.75), (0.5, 1.5));
    }
}

/// Sets every pixel's alpha (GDI leaves it 0), on all cores: this is
/// between the hotkey and the overlay appearing.
fn make_opaque(pixels: &mut [u8]) {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    let chunk = (pixels.len() / 4).div_ceil(threads) * 4;
    if chunk == 0 {
        return;
    }
    std::thread::scope(|scope| {
        for part in pixels.chunks_mut(chunk) {
            scope.spawn(move || part.chunks_exact_mut(4).for_each(|p| p[3] = 255));
        }
    });
}

// ---- text -------------------------------------------------------------------------

fn style(px: f32, bold: bool) -> TextStyle<'static> {
    TextStyle {
        family: FONT,
        size: px,
        bold,
        italic: false,
    }
}

/// The width of `s` at `px` pixels, rounded up (as the text will measure on
/// screen, near enough to place it).
fn text_width(s: &str, px: f32, bold: bool) -> i32 {
    text::layout(s, &style(px, bold), None).width().ceil() as i32
}

/// The height of a line at `px` pixels, rounded up.
fn line_height(px: f32) -> i32 {
    text::layout("Ag", &style(px, false), None).line_height().ceil() as i32
}

// ---- the decorations -------------------------------------------------------------

fn text_block(color: Color, bold: bool) -> Result<TextBlock> {
    TextBlock::new()?
        .font_family(FONT)?
        .foreground(Brush::solid(color))?
        .font_weight(if bold { FontWeight::DemiBold } else { FontWeight::Normal })
}

fn at(control: &impl AsControl, x: f64, y: f64) -> Result<()> {
    Canvas::set_left(control, x)?;
    Canvas::set_top(control, y)
}

/// Places `b` at `x, y, w, h` (DIPs) and shows it.
fn place(b: &Border, x: f64, y: f64, w: f64, h: f64) -> Result<()> {
    at(b, x, y)?;
    b.set_width(w.max(0.0))?;
    b.set_height(h.max(0.0))?;
    b.set_visible(true)
}

/// An outline `t` thick just outside `r`, in desktop pixels.
fn outline(b: &Border, r: Rect, t: i32, frame: [f64; 4], s: f64) -> Result<()> {
    let [left, top, _, _] = frame;
    let d = |v: i32| v as f64 / s;
    place(b, (r.x as f64 - t as f64 - left) / s, (r.y as f64 - t as f64 - top) / s, d(r.width + 2 * t), d(r.height + 2 * t))?;
    b.set_border_thickness(d(t))
}

fn outline_border(color: Color) -> Result<Border> {
    Border::new()?.border_brush(Brush::solid(color))?.visible(false)
}

fn filled(color: Color) -> Result<Border> {
    Border::new()?.background(Brush::solid(color))?.visible(false)
}

/// A dark box holding text.
fn panel(child: &dyn AsControl) -> Result<Border> {
    Border::new()?.background(Brush::solid(LABEL_BACK))?.child(Some(child))?.visible(false)
}

/// The magnifier to draw: where (desktop pixels, the accent frame
/// included) and around which pointer position.
struct Magnifier {
    cursor: (f64, f64),
    zoom: i32,
}

impl Magnifier {
    /// The `side` × `side` BGRA bitmap: the pixels around the pointer from
    /// `pixels` (the monitor's screenshot), the centre pixel ringed, in an
    /// accent frame.
    fn render(&self, pixels: &[u8], mapping: CaptureMapping, side: u32) -> std::result::Result<Vec<u8>, String> {
        let side = side as i32;
        let mut out = vec![0u8; (side * side * 4) as usize];
        for px in out.chunks_exact_mut(4) {
            px[3] = 255;
        }
        let half = SAMPLES / 2;
        let (cx, cy) = mapping.image_point_precise(self.cursor.0, self.cursor.1)
            .ok_or("the magnifier pointer position is invalid")?;
        let (width, height) = mapping.size();
        let stride = width as usize * 4;
        for sy in 0..SAMPLES {
            for sx in 0..SAMPLES {
                let (x, y) = (cx - half + sx, cy - half + sy);
                if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
                    continue;
                }
                let at = y as usize * stride + x as usize * 4;
                let Some(color) = pixels.get(at..at + 4) else { continue };
                let color = [color[0], color[1], color[2], 255];
                fill(&mut out, side, Rect::new(1 + sx * self.zoom, 1 + sy * self.zoom, self.zoom, self.zoom), color);
            }
        }
        let centre = Rect::new(1 + half * self.zoom, 1 + half * self.zoom, self.zoom, self.zoom);
        ring(&mut out, side, centre, [0, 0, 0, 255]);
        ring(
            &mut out,
            side,
            Rect::new(centre.x + 1, centre.y + 1, centre.width - 2, centre.height - 2),
            [255, 255, 255, 255],
        );
        ring(&mut out, side, Rect::new(1, 1, side - 2, side - 2), [ACCENT.b, ACCENT.g, ACCENT.r, 255]);
        Ok(out)
    }
}

/// Fills `r` (clipped) of a `side` × `side` BGRA bitmap.
fn fill(out: &mut [u8], side: i32, r: Rect, color: [u8; 4]) {
    let Some(r) = r.intersect(Rect::new(0, 0, side, side)) else { return };
    for y in r.y..r.bottom() {
        let row = (y * side) as usize * 4;
        for px in out[row + r.x as usize * 4..row + r.right() as usize * 4].chunks_exact_mut(4) {
            px.copy_from_slice(&color);
        }
    }
}

/// A one-pixel ring just outside `r`.
fn ring(out: &mut [u8], side: i32, r: Rect, color: [u8; 4]) {
    fill(out, side, Rect::new(r.x - 1, r.y - 1, r.width + 2, 1), color);
    fill(out, side, Rect::new(r.x - 1, r.bottom(), r.width + 2, 1), color);
    fill(out, side, Rect::new(r.x - 1, r.y, 1, r.height), color);
    fill(out, side, Rect::new(r.right(), r.y, 1, r.height), color);
}

/// The controls drawn over the screenshot, on one canvas.
struct Decor {
    canvas: Canvas,
    dim: Border,
    cross_h: Border,
    cross_v: Border,
    /// Multi-area outlines, made as needed.
    regions: Canvas,
    region_borders: Vec<Border>,
    path: Polyline,
    /// The points the path shows (to rebuild it only when it grew).
    path_shown: Vec<i32>,
    /// Video guides: an outline and its size, made as needed.
    guides: Canvas,
    guide_items: Vec<(Border, TextBlock)>,
    frame: Border,
    label: Border,
    label_title: TextBlock,
    label_size: TextBlock,
    help: Border,
    help_lines: StackPanel,
    help_shown: Vec<String>,
    mag_panel: Border,
    mag_coords: TextBlock,
    mag_swatch: Border,
    mag_hex: TextBlock,
    hint: Border,
    hint_text: TextBlock,
}

impl Decor {
    fn new() -> Result<Self> {
        let label_title = text_block(LABEL_MUTED, false)?;
        let label_size = text_block(LABEL_TEXT, true)?;
        let label_row = StackPanel::new()?
            .orientation(Orientation::Horizontal)?
            .child(&label_title)?
            .child(&label_size)?;
        let help_lines = StackPanel::new()?;
        let hint_text = text_block(LABEL_MUTED, false)?;
        let this = Decor {
            canvas: Canvas::new()?.visible(false)?,
            dim: Border::new()?.border_brush(Brush::solid(DIM))?,
            cross_h: filled(CROSSHAIR)?,
            cross_v: filled(CROSSHAIR)?,
            regions: Canvas::new()?,
            region_borders: Vec::new(),
            path: Polyline::new()?.stroke(Brush::solid(ACCENT))?.visible(false)?,
            path_shown: Vec::new(),
            guides: Canvas::new()?,
            guide_items: Vec::new(),
            frame: outline_border(ACCENT)?,
            label: panel(&label_row)?,
            label_title,
            label_size,
            help: panel(&help_lines)?,
            help_lines,
            help_shown: Vec::new(),
            mag_panel: filled(LABEL_BACK)?,
            mag_coords: text_block(LABEL_TEXT, false)?.visible(false)?,
            mag_swatch: filled(LABEL_BACK)?,
            mag_hex: text_block(LABEL_TEXT, false)?.visible(false)?,
            hint: panel(&hint_text)?,
            hint_text,
        };
        let children = this.canvas.children()?;
        children.add(&this.dim)?;
        children.add(&this.cross_h)?;
        children.add(&this.cross_v)?;
        children.add(&this.regions)?;
        children.add(&this.path)?;
        children.add(&this.guides)?;
        children.add(&this.frame)?;
        children.add(&this.label)?;
        children.add(&this.help)?;
        children.add(&this.mag_panel)?;
        children.add(&this.mag_coords)?;
        children.add(&this.mag_swatch)?;
        children.add(&this.mag_hex)?;
        children.add(&this.hint)?;
        Ok(this)
    }

    /// Draws `scene` over monitor `m` at `s` desktop pixels per DIP;
    /// returns the magnifier to draw, if any.
    fn draw(&mut self, scene: &Scene, mapping: CaptureMapping, frame: [f64; 4], s: f64) -> Result<Option<(Rect, Magnifier)>> {
        let m = mapping.rect();
        let [left, top, right, bottom] = mapping.desktop_bounds();
        let [frame_left, frame_top, _, _] = frame;
        let d = |v: i32| v as f64 / s;
        let dx = |v: i32| (v as f64 - frame_left) / s;
        let dy = |v: i32| (v as f64 - frame_top) / s;
        let content_left = (left - frame_left) / s;
        let content_top = (top - frame_top) / s;
        let st = &scene.state;
        self.canvas.set_visible(true)?;

        // Dimmed, except the lit area: one border around a hole.
        place(&self.dim, content_left, content_top, (right - left) / s, (bottom - top) / s)?;
        let area = st.lit();
        let (lit_left, lit_top, lit_right, lit_bottom) = if st.lit_kind == 2 && area == m {
            (left, top, right, bottom)
        } else {
            (
                (area.x as f64).max(left),
                (area.y as f64).max(top),
                (area.right() as f64).min(right),
                (area.bottom() as f64).min(bottom),
            )
        };
        if area.is_empty() || lit_left >= lit_right || lit_top >= lit_bottom {
            self.dim.set_background(Brush::solid(DIM))?;
            self.dim.set_border_thickness(0.0)?;
        } else {
            self.dim.set_background(None::<Brush>)?;
            self.dim.set_border_thickness((
                (lit_left - left) / s,
                (lit_top - top) / s,
                (right - lit_right) / s,
                (bottom - lit_bottom) / s,
            ))?;
        }

        let scale = decoration_scaling(s, st.scale as f64);
        let sc = |v: f64| (v * scale).round() as i32;
        let (cx, cy) = (st.cursor[0], st.cursor[1]);

        // The crosshair through the whole desktop.
        place(&self.cross_h, content_left, dy(cy), (right - left) / s, d(1))?;
        place(&self.cross_v, dx(cx), content_top, d(1), (bottom - top) / s)?;

        let thick = sc(2.0).max(2);
        let regions: Vec<Rect> = scene.regions.chunks_exact(4).map(|r| Rect::new(r[0], r[1], r[2], r[3])).collect();
        while self.region_borders.len() < regions.len() {
            let b = outline_border(ACCENT)?;
            self.regions.children()?.add(&b)?;
            self.region_borders.push(b);
        }
        for (i, b) in self.region_borders.iter().enumerate() {
            match regions.get(i) {
                Some(&r) => outline(b, r, thick, frame, s)?,
                None => b.set_visible(false)?,
            }
        }

        if scene.path.len() >= 4 {
            if self.path_shown != scene.path {
                let points: Vec<String> = scene
                    .path
                    .chunks_exact(2)
                    .map(|p| format!("{:.2},{:.2}", dx(p[0]), dy(p[1])))
                    .collect();
                self.path.set_points(points.join(" "))?;
                self.path.set_stroke_thickness(3.0 / s)?;
                self.path_shown.clone_from(&scene.path);
            }
            self.path.set_visible(true)?;
        } else {
            self.path.set_visible(false)?;
        }

        let font = 13.0 * scale;
        let font_dips = font / s;
        let guides = &scene.guides;
        while self.guide_items.len() < guides.len() {
            let b = outline_border(LABEL_MUTED)?;
            let t = text_block(LABEL_MUTED, false)?;
            self.guides.children()?.add(&b)?;
            self.guides.children()?.add(&t)?;
            self.guide_items.push((b, t));
        }
        for (i, (b, t)) in self.guide_items.iter().enumerate() {
            match guides.get(i) {
                Some(&(g, (gw, gh))) => {
                    outline(b, g, 1, frame, s)?;
                    t.set_text(format!("{gw} × {gh}"))?;
                    t.set_font_size(font_dips)?;
                    at(t, dx(g.x + 6), dy(g.y + 4))?;
                    t.set_visible(true)?;
                }
                None => {
                    b.set_visible(false)?;
                    t.set_visible(false)?;
                }
            }
        }

        // The lit area's frame.
        let area = st.lit();
        outline(&self.frame, area, thick, frame, s)?;

        // Labels, help, magnifier and hints only on the monitor under the pointer.
        if st.monitor() != m {
            for b in [&self.label, &self.help, &self.mag_panel, &self.mag_swatch, &self.hint] {
                b.set_visible(false)?;
            }
            self.mag_coords.set_visible(false)?;
            self.mag_hex.set_visible(false)?;
            return Ok(None);
        }
        let mon = m;
        let pad = sc(6.0);
        let px = font as f32;
        let line_h = line_height(px);

        let title_w = if scene.title.is_empty() {
            0
        } else {
            text_width(&scene.title, px, false) + sc(10.0)
        };
        let w = title_w + text_width(&scene.size, px, true) + 2 * pad;
        let h = line_h + 2 * pad;
        let mut x = area.x.max(mon.x);
        let mut y = area.y - h - sc(4.0);
        if y < mon.y {
            y = area.y + sc(4.0);
            x += sc(4.0);
        }
        x = x.min(mon.right() - w);
        let label = Rect::new(x, y, w, h);
        self.label_title.set_text(&scene.title)?;
        self.label_title.set_visible(!scene.title.is_empty())?;
        self.label_title.set_margin((0.0, 0.0, d(sc(10.0)), 0.0))?;
        self.label_title.set_font_size(font_dips)?;
        self.label_size.set_text(&scene.size)?;
        self.label_size.set_font_size(font_dips)?;
        self.label.set_padding(d(pad))?;
        at(&self.label, dx(label.x), dy(label.y))?;
        self.label.set_visible(true)?;

        if st.has(api::FLAG_HELP) && !scene.help.is_empty() {
            if self.help_shown != scene.help {
                let lines = self.help_lines.children()?;
                lines.clear()?;
                for line in &scene.help {
                    lines.add(text_block(LABEL_TEXT, false)?.text(line)?)?;
                }
                self.help_shown.clone_from(&scene.help);
            }
            let lines = self.help_lines.children()?;
            for i in 0..lines.len()? {
                if let Some(t) = lines.get(i)?.cast::<TextBlock>() {
                    t.set_font_size(font_dips)?;
                    t.set_height(d(line_h))?;
                }
            }
            self.help_lines.set_spacing(d(sc(4.0)))?;
            let w = scene.help.iter().map(|l| text_width(l, px, false)).max().unwrap_or(0) + 2 * pad;
            let h = scene.help.len() as i32 * (line_h + sc(4.0)) + 2 * pad;
            let r = Rect::new(mon.x + (mon.width - w) / 2, mon.y + (mon.height - h) / 2, w, h);
            self.help.set_padding(d(pad))?;
            at(&self.help, dx(r.x), dy(r.y))?;
            self.help.set_visible(true)?;
        } else {
            self.help.set_visible(false)?;
        }

        let mut total = Rect::default();
        let mut magnifier = None;
        if st.has(api::FLAG_MAGNIFIER) {
            #[cfg(target_os = "macos")]
            let magnifier_cursor = (st.cursor_precise[0], st.cursor_precise[1]);
            #[cfg(windows)]
            let magnifier_cursor = (f64::from(cx), f64::from(cy));
            let zoom = sc(8.0).max(6);
            let side = SAMPLES * zoom;
            let panel_h = 2 * line_h + 3 * pad;
            let gap = sc(22.0);
            let mut mx = cx + gap;
            let mut my = cy + gap;
            if mx + side > mon.right() {
                mx = cx - gap - side;
            }
            if my + side + panel_h > mon.bottom() {
                my = cy - gap - side - panel_h;
            }
            total = Rect::new(mx, my, side, side + panel_h);
            magnifier = Some((Rect::new(mx - 1, my - 1, side + 2, side + 2), Magnifier { cursor: magnifier_cursor, zoom }));
            let panel = Rect::new(mx - 1, my + side + 1, side + 2, panel_h);
            place(&self.mag_panel, dx(panel.x), dy(panel.y), d(panel.width), d(panel.height))?;
            self.mag_coords.set_text(format!("{cx}, {cy}"))?;
            self.mag_coords.set_font_size(font_dips)?;
            at(&self.mag_coords, dx(panel.x + pad), dy(panel.y + pad))?;
            self.mag_coords.set_visible(true)?;
            if st.has(api::FLAG_COLOR) {
                let [r, g, b] = st.color.map(|c| c.clamp(0, 255) as u8);
                let swatch = Rect::new(panel.x + pad, panel.y + 2 * pad + line_h + 2, line_h - 4, line_h - 4);
                self.mag_swatch.set_background(Brush::solid(Color::rgb(r, g, b)))?;
                place(&self.mag_swatch, dx(swatch.x), dy(swatch.y), d(swatch.width), d(swatch.height))?;
                self.mag_hex.set_text(format!("#{r:02X}{g:02X}{b:02X}"))?;
                self.mag_hex.set_font_size(font_dips)?;
                at(&self.mag_hex, dx(swatch.right() + pad), dy(panel.y + 2 * pad + line_h))?;
                self.mag_hex.set_visible(true)?;
            } else {
                self.mag_swatch.set_visible(false)?;
                self.mag_hex.set_visible(false)?;
            }
        } else {
            self.mag_panel.set_visible(false)?;
            self.mag_coords.set_visible(false)?;
            self.mag_swatch.set_visible(false)?;
            self.mag_hex.set_visible(false)?;
        }

        // Key hints at the top of the monitor, at the bottom when they would
        // cover the pointer, the label or the magnifier.
        let w = text_width(&scene.hint, px, false) + 2 * pad;
        let h = line_h + 2 * pad;
        let top = Rect::new(mon.x + (mon.width - w) / 2, mon.y + sc(12.0), w, h);
        let busy = top.inflate(sc(40.0));
        let covers = busy.contains(cx, cy) || busy.intersect(label).is_some() || (!total.is_empty() && busy.intersect(total).is_some());
        let r = if covers { Rect::new(top.x, mon.bottom() - h - sc(12.0), w, h) } else { top };
        self.hint_text.set_text(&scene.hint)?;
        self.hint_text.set_font_size(font_dips)?;
        self.hint.set_padding(d(pad))?;
        at(&self.hint, dx(r.x), dy(r.y))?;
        self.hint.set_visible(!scene.hint.is_empty())?;
        Ok(magnifier)
    }
}
