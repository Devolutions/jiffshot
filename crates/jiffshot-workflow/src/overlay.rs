//! The selection overlay: a frozen, dimmed screenshot of the whole desktop
//! with the window or region under the cursor lit, plus a crosshair, a
//! magnifier and size labels.
//!
//! This module is the overlay's logic: what is lit, what a click, drag,
//! wheel or key does, and what to show ([`Overlay::view`]). The app draws
//! it, one window per monitor, and feeds the input back through the C ABI
//! in `ui_api.rs`; [`run`] hands an overlay to the app and waits for the
//! selection.

#[cfg(target_os = "macos")]
use crate::geometry::CaptureLayout;
use crate::sys::{
    DispatchMessageW, MsgWaitForMultipleObjects, PeekMessageW, PostQuitMessage, RegisterHotKey,
    TranslateMessage, UnregisterHotKey, MOD_NOREPEAT, MSG, PM_REMOVE, QS_ALLINPUT, VK_BACK,
    VK_DOWN, VK_ESCAPE, VK_F1, VK_LEFT, VK_RETURN, VK_RIGHT, VK_SPACE, VK_UP, WM_HOTKEY, WM_QUIT,
};
use jiffshot_capture::screen::Monitor;
use jiffshot_capture::targets::Targets;
use jiffshot_capture::{Frame, Rect};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// How the overlay selects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Windows, regions, screens.
    #[default]
    Normal,
    /// A freehand outline.
    Freehand,
    /// Several regions, finished with Enter.
    MultiArea,
    /// A rectangle of this size, placed by clicking.
    Fixed(i32, i32),
    /// For recording: G shows standard-size guides.
    Video,
    /// For recording: a standard-size rectangle, placed by clicking.
    VideoFixed(i32, i32),
}

/// What the user picked, in screen coordinates.
#[derive(Clone, Debug)]
pub enum Selection {
    Region(Rect),
    /// A freehand outline (screen points) inside `rect`.
    Freehand {
        rect: Rect,
        points: Vec<(i32, i32)>,
    },
    /// Several regions, all inside `rect`.
    Multi {
        rect: Rect,
        regions: Vec<Rect>,
    },
    /// A window or child window; `target` indexes [`Overlay::targets`].
    Window {
        rect: Rect,
        target: usize,
    },
    /// A fixed-size selection with explicit output dimensions.
    #[cfg(target_os = "macos")]
    Fixed {
        rect: Rect,
        size: (u32, u32),
    },
    Monitor(Rect),
    Desktop(Rect),
}

impl Selection {
    pub fn rect(&self) -> Rect {
        match self {
            Selection::Region(r)
            | Selection::Window { rect: r, .. }
            | Selection::Monitor(r)
            | Selection::Desktop(r)
            | Selection::Freehand { rect: r, .. }
            | Selection::Multi { rect: r, .. } => *r,
            #[cfg(target_os = "macos")]
            Selection::Fixed { rect, .. } => *rect,
        }
    }
}

/// The lit area while hovering or dragging, in client coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Lit {
    Region(Rect),
    Window(usize, Rect),
    Monitor(Rect),
}

impl Lit {
    fn rect(self) -> Rect {
        match self {
            Lit::Region(r) | Lit::Window(_, r) | Lit::Monitor(r) => r,
        }
    }
}

const DRAG_THRESHOLD: i32 = 3;

#[cfg(target_os = "macos")]
#[derive(Default)]
struct Nudge {
    position: Option<(f64, f64)>,
    pending: Option<(f64, f64)>,
    expected: Option<(i32, i32)>,
}

#[cfg(target_os = "macos")]
pub(crate) fn rounded_desktop_point(point: (f64, f64)) -> io::Result<(i32, i32)> {
    let coordinate = |value: f64| {
        let value = value.round();
        if !value.is_finite() || value < i32::MIN as f64 || value > i32::MAX as f64 {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "selection cursor is outside the desktop coordinate range",
            ))
        } else {
            Ok(value as i32)
        }
    };
    Ok((coordinate(point.0)?, coordinate(point.1)?))
}

#[cfg(target_os = "macos")]
pub(crate) fn client_point(point: (f64, f64), origin: (i32, i32)) -> io::Result<(i32, i32)> {
    let (x, y) = rounded_desktop_point(point)?;
    x.checked_sub(origin.0)
        .zip(y.checked_sub(origin.1))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "selection cursor is outside the overlay coordinate range",
            )
        })
}

#[cfg(target_os = "macos")]
impl Nudge {
    fn position(&self, current: (i32, i32), origin: (i32, i32)) -> (f64, f64) {
        self.position.unwrap_or((
            f64::from(current.0) + f64::from(origin.0),
            f64::from(current.1) + f64::from(origin.1),
        ))
    }

    fn target(
        &mut self,
        current: (i32, i32),
        origin: (i32, i32),
        direction: (i32, i32),
        unit: (f64, f64),
        pixels: i32,
    ) -> io::Result<(i32, i32)> {
        let position = self.position(current, origin);
        let point = (
            position.0 + f64::from(direction.0) * unit.0 * f64::from(pixels),
            position.1 + f64::from(direction.1) * unit.1 * f64::from(pixels),
        );
        let target = client_point(point, origin)?;
        self.pending = Some(point);
        self.expected = Some(target);
        Ok(target)
    }

    fn moved(&mut self, current: (i32, i32), origin: (i32, i32)) {
        self.position = if self.expected == Some(current) {
            self.pending
        } else {
            Some((
                f64::from(current.0) + f64::from(origin.0),
                f64::from(current.1) + f64::from(origin.1),
            ))
        };
        self.pending = None;
        self.expected = None;
    }

    fn remember(&mut self, position: (f64, f64)) {
        self.position = Some(position);
        self.pending = None;
        self.expected = None;
    }

    fn failed(&mut self) {
        *self = Self::default();
    }
}

/// Standard recording sizes the G key shows.
pub const GUIDES: &[(i32, i32)] = &[(1920, 1080), (1280, 720), (640, 480), (320, 240)];

fn guide_rectangles_at(
    cursor: (i32, i32),
    mut desktop_size: impl FnMut(i32, i32) -> io::Result<(i32, i32)>,
) -> io::Result<Vec<(Rect, (i32, i32))>> {
    GUIDES
        .iter()
        .map(|&(width, height)| {
            let (w, h) = desktop_size(width, height)?;
            if w <= 0 || h <= 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid video guide dimensions",
                ));
            }
            let x = cursor.0.checked_sub(w / 2).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "video guide is outside the desktop coordinate range",
                )
            })?;
            let y = cursor.1.checked_sub(h / 2).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "video guide is outside the desktop coordinate range",
                )
            })?;
            x.checked_add(w)
                .and_then(|_| y.checked_add(h))
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "video guide is outside the desktop coordinate range",
                    )
                })?;
            Ok((Rect::new(x, y, w, h), (width, height)))
        })
        .collect()
}

/// The F1 help, one line each.
pub const HELP: &[&str] = &[
    "Click: window (wheel: parent / child)   ·   Drag: region",
    #[cfg(windows)]
    "Shift: square   ·   Ctrl: 16:9   ·   Ctrl+Shift: 4:3",
    #[cfg(target_os = "macos")]
    "Shift: square   ·   Cmd: 16:9   ·   Cmd+Shift: 4:3",
    "Arrows: move 1 px (Shift: 10)   ·   Shift+Enter: start a region at the pointer",
    "Video: 1–4 select fixed sizes (1920 × 1080, 1280 × 720, 640 × 480, 320 × 240); press again for freeform",
    "Enter / Space: capture   ·   F: this screen   ·   A: all screens",
    "M: magnifier   ·   G: video size guides   ·   F1: this help   ·   Esc: cancel",
];

/// The key hints shown at the top of the screen.
fn hint(mode: Mode) -> &'static str {
    match mode {
        Mode::Freehand => "Draw around the area to capture  ·  Esc: cancel  ·  F1: help",
        Mode::MultiArea => "Drag each area to capture, then press Enter  ·  Backspace: remove the last  ·  Esc: cancel",
        Mode::Fixed(..) => "Click to place the fixed-size area  ·  Arrows: nudge  ·  Esc: cancel",
        Mode::Video => {
            if cfg!(target_os = "macos") {
                "Click a window or drag the area to record  ·  1–4: fixed sizes  ·  G: guides  ·  Shift: square, Cmd: 16:9, Cmd+Shift: 4:3  ·  Esc: cancel"
            } else {
                "Click a window or drag the area to record  ·  1–4: fixed sizes  ·  G: guides  ·  Shift: square, Ctrl: 16:9, Ctrl+Shift: 4:3  ·  Esc: cancel"
            }
        }
        Mode::VideoFixed(..) => "Move pointer or use arrows; click or press Enter to record  ·  1–4: change size; same key: freeform  ·  Esc: cancel",
        Mode::Normal => "Click a window or drag a region  ·  Wheel: parent / child  ·  F: screen  ·  A: all screens  ·  F1: help  ·  Esc: cancel",
    }
}

/// The drag rectangle, constrained by modifiers and the desktop bounds.
fn constrained(
    a: (i32, i32),
    b: (i32, i32),
    shift: bool,
    ctrl: bool,
    bounds: Rect,
) -> Option<Rect> {
    let ratio = match (ctrl, shift) {
        (true, true) => Some(4.0 / 3.0),
        (true, false) => Some(16.0 / 9.0),
        (false, true) => Some(1.0),
        _ => None,
    };
    let Some(ratio) = ratio else {
        return Rect::from_points(a, b).intersect(bounds);
    };
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let w = (dx.abs() as f32).max(dy.abs() as f32 * ratio);
    let h = w / ratio;
    let sx = if dx < 0 { -1.0 } else { 1.0 };
    let sy = if dy < 0 { -1.0 } else { 1.0 };
    let requested = Rect::from_points(a, (a.0 + (w * sx) as i32, a.1 + (h * sy) as i32));
    let clipped = requested.intersect(bounds)?;
    if requested == clipped || !bounds.contains(a.0, a.1) {
        return Some(clipped);
    }
    let max_x = if dx < 0 {
        a.0 - bounds.x
    } else {
        bounds.right() - 1 - a.0
    };
    let max_y = if dy < 0 {
        a.1 - bounds.y
    } else {
        bounds.bottom() - 1 - a.1
    };
    let width = w.min(max_x as f32).min(max_y as f32 * ratio);
    let height = width / ratio;
    Some(Rect::from_points(
        a,
        (a.0 + (width * sx) as i32, a.1 + (height * sy) as i32),
    ))
}

fn place_fixed(bounds: Rect, cursor: (i32, i32), width: i32, height: i32) -> Rect {
    let x = if bounds.width >= width {
        (cursor.0 - width / 2).clamp(bounds.x, bounds.right() - width)
    } else {
        bounds.x
    };
    let y = if bounds.height >= height {
        (cursor.1 - height / 2).clamp(bounds.y, bounds.bottom() - height)
    } else {
        bounds.y
    };
    Rect::new(x, y, width, height)
}

/// What the overlay shows right now, in desktop pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    pub cursor: (i32, i32),
    #[cfg(target_os = "macos")]
    pub cursor_precise: (f64, f64),
    pub lit: Rect,
    /// 0 a region, 1 a window, 2 a screen.
    pub lit_kind: i32,
    /// The window's name (and child level), or "Screen".
    pub title: Option<String>,
    /// "W × H".
    pub size: String,
    /// The monitor under the cursor, and its UI scale.
    pub monitor: Rect,
    pub scale: f32,
    /// The snapshot's color under the cursor.
    pub color: Option<(u8, u8, u8)>,
    pub hint: &'static str,
    pub magnifier: bool,
    pub help: bool,
    pub guides: bool,
    pub dragging: bool,
}

pub struct Overlay {
    /// Screen position of the desktop bounds.
    pub origin: (i32, i32),
    pub snapshot: Frame,
    bounds: Rect,
    pub targets: Targets,
    pub monitors: Vec<Monitor>,
    /// The macOS geometry used to create the frozen desktop.
    #[cfg(target_os = "macos")]
    pub(crate) display_layout: Option<CaptureLayout>,
    #[cfg(target_os = "macos")]
    pub(crate) frozen: Option<jiffshot_capture::screen::FrozenDesktop>,
    #[cfg(target_os = "macos")]
    nudge: Nudge,
    /// When the capture started (the hotkey), for the timing log.
    pub started: Option<Instant>,
    cursor: (i32, i32),
    press: Option<(i32, i32)>,
    dragging: bool,
    shift: bool,
    ctrl: bool,
    levels_up: u32,
    deepest: Option<usize>,
    pub outcome: Option<Option<Selection>>,
    pub mode: Mode,
    show_magnifier: bool,
    show_help: bool,
    show_guides: bool,
    /// Freehand outline so far (client coordinates).
    path: Vec<(i32, i32)>,
    /// Regions already added in multi-area mode (client coordinates).
    regions: Vec<Rect>,
}

impl Overlay {
    /// `snapshot` covers the virtual screen starting at `origin`, unless a
    /// native frozen desktop is supplied separately. `cursor` is the
    /// initial pointer position in screen pixels.
    pub fn new(
        snapshot: Frame,
        origin: (i32, i32),
        targets: Targets,
        monitors: Vec<Monitor>,
        cursor: (i32, i32),
    ) -> io::Result<Overlay> {
        let bounds = snapshot.bounds();
        Ok(Overlay {
            cursor: (cursor.0 - origin.0, cursor.1 - origin.1),
            origin,
            snapshot,
            bounds,
            targets,
            monitors,
            #[cfg(target_os = "macos")]
            display_layout: None,
            #[cfg(target_os = "macos")]
            frozen: None,
            #[cfg(target_os = "macos")]
            nudge: Nudge::default(),
            started: None,
            press: None,
            dragging: false,
            shift: false,
            ctrl: false,
            levels_up: 0,
            deepest: None,
            outcome: None,
            mode: Mode::Normal,
            show_magnifier: true,
            show_help: false,
            show_guides: false,
            path: Vec::new(),
            regions: Vec::new(),
        })
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn with_display_layout(mut self, layout: CaptureLayout) -> Self {
        self.display_layout = Some(layout);
        self
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn with_frozen(
        mut self,
        frozen: jiffshot_capture::screen::FrozenDesktop,
        desktop: Rect,
    ) -> Self {
        self.bounds = Rect::new(0, 0, desktop.width, desktop.height);
        self.frozen = Some(frozen);
        self
    }

    fn bounds(&self) -> Rect {
        self.bounds
    }

    fn to_screen(&self, r: Rect) -> Rect {
        r.offset(self.origin.0, self.origin.1)
    }

    fn screen_cursor(&self) -> (i32, i32) {
        (self.cursor.0 + self.origin.0, self.cursor.1 + self.origin.1)
    }

    fn target_at_cursor(&self, levels_up: u32) -> io::Result<Option<usize>> {
        #[cfg(target_os = "macos")]
        if let Some(layout) = &self.display_layout {
            layout.validate()?;
            let (x, y) = self.nudge.position(self.cursor, self.origin);
            let target = self
                .targets
                .hit_precise(x, y, layout.desktop_scale(), levels_up)?;
            layout.validate()?;
            return Ok(target);
        }
        let (x, y) = self.screen_cursor();
        Ok(self.targets.hit(x, y, levels_up))
    }

    fn update_deepest(&mut self) -> io::Result<Option<usize>> {
        let deepest = self.target_at_cursor(0)?;
        if deepest != self.deepest {
            self.deepest = deepest;
            self.levels_up = 0;
        }
        Ok(deepest)
    }

    #[cfg(target_os = "macos")]
    fn cursor_desktop_size_for_pixels(
        &self,
        width: u32,
        height: u32,
    ) -> Option<io::Result<(i32, i32)>> {
        self.display_layout.as_ref().map(|layout| {
            let (x, y) = self.nudge.position(self.cursor, self.origin);
            layout.desktop_size_for_pixels_precise(x, y, width, height)
        })
    }

    /// Video guide bounds in desktop coordinates and their native-pixel labels.
    pub(crate) fn guide_rectangles(&self) -> io::Result<Vec<(Rect, (i32, i32))>> {
        let cursor = self.screen_cursor();
        let guides = guide_rectangles_at(cursor, |width, height| {
            #[cfg(target_os = "macos")]
            if let Some(size) = self.cursor_desktop_size_for_pixels(width as u32, height as u32) {
                return size;
            }
            Ok((width, height))
        })?;
        #[cfg(target_os = "macos")]
        if self.display_layout.is_some() {
            return Ok(guides
                .into_iter()
                .map(|(rect, label)| {
                    let placed = place_fixed(self.bounds(), self.cursor, rect.width, rect.height);
                    (self.to_screen(placed), label)
                })
                .collect());
        }
        Ok(guides)
    }

    /// The monitor under the cursor, in client coordinates.
    fn monitor(&self) -> io::Result<(Rect, f32)> {
        #[cfg(windows)]
        let (x, y) = self.screen_cursor();
        #[cfg(target_os = "macos")]
        let precise = if let Some(layout) = &self.display_layout {
            let (x, y) = self.nudge.position(self.cursor, self.origin);
            Some(layout.monitor_at_precise(x, y)?)
        } else {
            None
        };
        #[cfg(target_os = "macos")]
        let monitor = precise.as_ref().or_else(|| {
            let (x, y) = self.nudge.position(self.cursor, self.origin);
            self.monitors.iter().find(|m| {
                x >= f64::from(m.rect.x)
                    && x < f64::from(m.rect.right())
                    && y >= f64::from(m.rect.y)
                    && y < f64::from(m.rect.bottom())
            })
        });
        #[cfg(windows)]
        let monitor = self.monitors.iter().find(|m| m.rect.contains(x, y));
        let monitor = monitor
            .or_else(|| self.monitors.iter().find(|m| m.primary))
            .or(self.monitors.first());
        Ok(match monitor {
            Some(m) => (m.rect.offset(-self.origin.0, -self.origin.1), m.scale()),
            None => (self.bounds(), 1.0),
        })
    }

    fn lit_now(&mut self) -> io::Result<Lit> {
        if self.mode == Mode::Freehand && !self.path.is_empty() {
            let r = self
                .path
                .iter()
                .fold(Rect::new(self.path[0].0, self.path[0].1, 1, 1), |r, p| {
                    r.union(Rect::new(p.0, p.1, 1, 1))
                });
            return Ok(Lit::Region(r));
        }
        if let (true, Some(press)) = (self.dragging, self.press) {
            return Ok(Lit::Region(
                constrained(press, self.cursor, self.shift, self.ctrl, self.bounds())
                    .unwrap_or_default(),
            ));
        }
        let fixed_size = match self.mode {
            Mode::Fixed(w, h) => Some((w, h)),
            Mode::VideoFixed(w, h) => Some((w, h)),
            _ => None,
        };
        if let Some((w, h)) = fixed_size {
            #[cfg(target_os = "macos")]
            let (w, h) = if let Some(size) =
                self.cursor_desktop_size_for_pixels(w.max(1) as u32, h.max(1) as u32)
            {
                size?
            } else {
                (w, h)
            };
            let b = self.bounds();
            let r = place_fixed(b, self.cursor, w, h);
            return Ok(Lit::Region(if matches!(self.mode, Mode::VideoFixed(..)) {
                r
            } else {
                r.intersect(b).unwrap_or_default()
            }));
        }
        self.update_deepest()?;
        Ok(match self.target_at_cursor(self.levels_up)? {
            Some(i) => Lit::Window(
                i,
                self.targets.list[i]
                    .rect
                    .offset(-self.origin.0, -self.origin.1),
            ),
            None => Lit::Monitor(self.monitor()?.0),
        })
    }

    /// What to draw now.
    pub fn view(&mut self) -> io::Result<View> {
        let lit = self.lit_now()?;
        let area = lit.rect().intersect(self.bounds()).unwrap_or_default();
        let title = match lit {
            Lit::Window(i, _) => {
                let t = &self.targets.list[i];
                let name = if t.title.is_empty() {
                    t.class.clone()
                } else {
                    t.title.clone()
                };
                let name: String = if name.chars().count() > 60 {
                    name.chars().take(59).chain(['…']).collect()
                } else {
                    name
                };
                let level = self.targets.depth_of(i);
                Some(if level > 0 {
                    format!("{name}  ·  child {level}")
                } else {
                    name
                })
            }
            Lit::Monitor(_) => Some("Screen".to_owned()),
            Lit::Region(_) => None,
        };
        let (monitor, scale) = self.monitor()?;
        #[cfg(target_os = "macos")]
        let size = match self.fixed_output_size(area)? {
            Some((w, h)) => format!("{w} × {h}"),
            None if area.is_empty() && self.display_layout.is_some() => {
                "Outside displays".to_owned()
            }
            None => match self.display_layout.as_ref().map(|layout| {
                let rect = self.to_screen(area);
                match lit {
                    Lit::Monitor(_) if area == lit.rect() => layout.monitor_mapping_for_rect(rect),
                    Lit::Monitor(_) => layout.preview_mapping(rect),
                    Lit::Window(target, _) => {
                        layout.preview_target_mapping(&self.targets.list[target])
                    }
                    Lit::Region(_) => layout.preview_mapping(rect),
                }
            }) {
                Some(Ok(mapping)) => {
                    let (w, h) = mapping.size();
                    format!("{w} × {h}")
                }
                Some(Err(error))
                    if matches!(lit, Lit::Region(_)) && error.kind() == io::ErrorKind::NotFound =>
                {
                    "Outside displays".to_owned()
                }
                Some(Err(error)) => return Err(error),
                None => format!("{} × {}", lit.rect().width, lit.rect().height),
            },
        };
        #[cfg(windows)]
        let size = format!("{} × {}", lit.rect().width, lit.rect().height);
        #[cfg(target_os = "macos")]
        let cursor_precise = self.nudge.position(self.cursor, self.origin);
        #[cfg(target_os = "macos")]
        let color = match &self.frozen {
            Some(frozen) => frozen.color_at_precise(cursor_precise.0, cursor_precise.1),
            None => self.snapshot.rgb(self.cursor.0, self.cursor.1),
        };
        #[cfg(windows)]
        let color = self.snapshot.rgb(self.cursor.0, self.cursor.1);
        Ok(View {
            cursor: self.screen_cursor(),
            #[cfg(target_os = "macos")]
            cursor_precise,
            lit: self.to_screen(area),
            lit_kind: match lit {
                Lit::Region(_) => 0,
                Lit::Window(..) => 1,
                Lit::Monitor(_) => 2,
            },
            title,
            size,
            monitor: self.to_screen(monitor),
            scale,
            color,
            hint: hint(self.mode),
            magnifier: self.show_magnifier,
            help: self.show_help,
            guides: self.show_guides && matches!(self.mode, Mode::Video | Mode::VideoFixed(..)),
            dragging: self.dragging,
        })
    }

    #[cfg(target_os = "macos")]
    fn fixed_output_size(&self, area: Rect) -> io::Result<Option<(u32, u32)>> {
        if self.dragging {
            return Ok(None);
        }
        let (width, height) = match self.mode {
            Mode::Fixed(w, h) | Mode::VideoFixed(w, h) => (w.max(1) as u32, h.max(1) as u32),
            _ => return Ok(None),
        };
        if self.display_layout.is_none() {
            return Ok(None);
        }
        if matches!(self.mode, Mode::VideoFixed(..)) {
            return Ok(Some((width, height)));
        }
        let Some(size) = self.cursor_desktop_size_for_pixels(width, height) else {
            return Ok(None);
        };
        let (desktop_width, desktop_height) = size?;
        let width = ((width as f64 * area.width.max(0) as f64 / desktop_width as f64).round()
            as u32)
            .max(1);
        let height = ((height as f64 * area.height.max(0) as f64 / desktop_height as f64).round()
            as u32)
            .max(1);
        Ok(Some((width, height)))
    }

    /// The freehand outline so far, in screen pixels.
    pub fn path(&self) -> impl Iterator<Item = (i32, i32)> + '_ {
        self.path
            .iter()
            .map(|&(x, y)| (x + self.origin.0, y + self.origin.1))
    }

    /// The areas added so far in multi-area mode, in screen pixels.
    pub fn regions(&self) -> impl Iterator<Item = Rect> + '_ {
        self.regions.iter().map(|r| self.to_screen(*r))
    }

    /// The modifier keys held, which constrain a drag.
    pub fn set_modifiers(&mut self, shift: bool, ctrl: bool) {
        self.shift = shift;
        self.ctrl = ctrl;
    }

    // ---- input (client coordinates) -----------------------------------

    pub fn pointer_moved(&mut self, x: i32, y: i32) {
        #[cfg(target_os = "macos")]
        self.nudge.moved((x, y), self.origin);
        self.cursor = (x, y);
        if !matches!(self.mode, Mode::VideoFixed(..)) {
            if let (false, Some((px, py))) = (self.dragging, self.press) {
                if (x - px).abs() > DRAG_THRESHOLD || (y - py).abs() > DRAG_THRESHOLD {
                    self.dragging = true;
                }
            }
        }
        if self.mode == Mode::Freehand && self.press.is_some() && self.path.last() != Some(&(x, y))
        {
            self.path.push((x, y));
        }
    }

    pub fn pointer_pressed(&mut self, x: i32, y: i32) {
        #[cfg(target_os = "macos")]
        self.nudge.failed();
        self.cursor = (x, y);
        self.press = Some((x, y));
    }

    pub fn pointer_released(&mut self, x: i32, y: i32) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        self.nudge.failed();
        self.finish_pointer_release(x, y)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn pointer_released_precise(
        &mut self,
        x: i32,
        y: i32,
        position: (f64, f64),
    ) -> io::Result<()> {
        self.nudge.remember(position);
        self.finish_pointer_release(x, y)
    }

    fn finish_pointer_release(&mut self, x: i32, y: i32) -> io::Result<()> {
        self.cursor = (x, y);
        if self.mode == Mode::Freehand {
            if self.path.len() >= 3 {
                let rect = self
                    .lit_now()?
                    .rect()
                    .intersect(self.bounds())
                    .unwrap_or_default();
                let points = self
                    .path
                    .iter()
                    .map(|&(px, py)| (px + self.origin.0, py + self.origin.1))
                    .collect();
                self.outcome = Some(Some(Selection::Freehand {
                    rect: self.to_screen(rect),
                    points,
                }));
            }
            self.path.clear();
            self.press = None;
            self.dragging = false;
            return Ok(());
        }
        if self.mode == Mode::MultiArea {
            if self.dragging {
                if let Some(r) = self
                    .press
                    .and_then(|p| constrained(p, self.cursor, self.shift, self.ctrl, self.bounds()))
                {
                    self.regions.push(r);
                }
            }
            self.press = None;
            self.dragging = false;
            return Ok(());
        }
        if matches!(self.mode, Mode::VideoFixed(..)) {
            if self.press.is_some() {
                self.select_lit()?;
            }
            self.press = None;
            self.dragging = false;
            return Ok(());
        }
        if self.dragging {
            if let Some(press) = self.press {
                let region = constrained(press, self.cursor, self.shift, self.ctrl, self.bounds());
                self.outcome = Some(region.map(|r| Selection::Region(self.to_screen(r))));
            }
        } else if self.press.is_some() {
            self.select_lit()?;
        }
        self.press = None;
        self.dragging = false;
        Ok(())
    }

    fn select_lit(&mut self) -> io::Result<()> {
        let selection = match self.lit_now()? {
            Lit::Window(target, r) => Selection::Window {
                rect: self.to_screen(r),
                target,
            },
            Lit::Monitor(r) => Selection::Monitor(self.to_screen(r)),
            Lit::Region(r) => {
                #[cfg(target_os = "macos")]
                if let Some(size) = self.fixed_output_size(r)? {
                    Selection::Fixed {
                        rect: self.to_screen(r),
                        size,
                    }
                } else {
                    Selection::Region(self.to_screen(r))
                }
                #[cfg(windows)]
                Selection::Region(self.to_screen(r))
            }
        };
        self.outcome = Some(Some(selection));
        Ok(())
    }

    /// Mouse wheel: up selects the parent of the lit window, down goes back
    /// towards the deepest child under the cursor.
    pub fn wheel(&mut self, delta: i32) -> io::Result<()> {
        if self.dragging {
            return Ok(());
        }
        let max = self
            .update_deepest()?
            .map_or(0, |i| self.targets.depth_of(i));
        self.levels_up = if delta > 0 {
            (self.levels_up + 1).min(max)
        } else {
            self.levels_up.saturating_sub(1)
        };
        Ok(())
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn wheel_precise(
        &mut self,
        x: i32,
        y: i32,
        position: (f64, f64),
        delta: i32,
    ) -> io::Result<()> {
        if !self.dragging {
            self.cursor = (x, y);
            self.nudge.remember(position);
        }
        self.wheel(delta)
    }

    pub fn cancel(&mut self) {
        self.outcome = Some(None);
    }

    /// Handles a key; returns a new cursor position (client coordinates)
    /// when the key nudges the pointer.
    pub fn key(&mut self, vk: u16, shift: bool) -> io::Result<Option<(i32, i32)>> {
        let step = if shift { 10 } else { 1 };
        let (x, y) = self.cursor;
        let direction = match vk {
            VK_LEFT => Some((-1, 0)),
            VK_RIGHT => Some((1, 0)),
            VK_UP => Some((0, -1)),
            VK_DOWN => Some((0, 1)),
            _ => None,
        };
        if let Some((dx, dy)) = direction {
            #[cfg(target_os = "macos")]
            {
                let unit = if let Some(layout) = &self.display_layout {
                    let (screen_x, screen_y) = self.nudge.position(self.cursor, self.origin);
                    layout.desktop_pixel_step_precise(screen_x, screen_y)?
                } else {
                    (1.0, 1.0)
                };
                return self
                    .nudge
                    .target((x, y), self.origin, (dx, dy), unit, step)
                    .map(Some);
            }
            #[cfg(windows)]
            return Ok(Some((x + dx * step, y + dy * step)));
        }
        match vk {
            VK_ESCAPE => self.cancel(),
            VK_RETURN | VK_SPACE if matches!(self.mode, Mode::VideoFixed(..)) => {
                self.select_lit()?
            }
            VK_RETURN if shift && !self.dragging => {
                // A region started from the keyboard: arrows size it, Enter
                // finishes.
                self.press = Some((x, y));
                self.dragging = true;
            }
            VK_RETURN | VK_SPACE if self.mode == Mode::MultiArea => {
                if self.dragging {
                    self.pointer_released(x, y)?;
                }
                if let Some(first) = self.regions.first().copied() {
                    let all = self.regions.iter().fold(first, |a, r| a.union(*r));
                    let regions = self.regions.iter().map(|r| self.to_screen(*r)).collect();
                    self.outcome = Some(Some(Selection::Multi {
                        rect: self.to_screen(all),
                        regions,
                    }));
                }
            }
            VK_BACK if self.mode == Mode::MultiArea => {
                self.regions.pop();
            }
            VK_RETURN | VK_SPACE => {
                if self.dragging {
                    self.pointer_released(x, y)?;
                } else {
                    self.select_lit()?;
                }
            }
            VK_F1 => self.show_help = !self.show_help,
            _ if vk == b'M' as u16 => self.show_magnifier = !self.show_magnifier,
            _ if vk == b'G' as u16 => self.show_guides = !self.show_guides,
            _ if (b'1' as u16..=b'4' as u16).contains(&vk)
                && matches!(self.mode, Mode::Video | Mode::VideoFixed(..)) =>
            {
                let (w, h) = GUIDES[(vk - b'1' as u16) as usize];
                self.mode = match self.mode {
                    Mode::VideoFixed(current_w, current_h) if (current_w, current_h) == (w, h) => {
                        Mode::Video
                    }
                    _ => Mode::VideoFixed(w, h),
                };
            }
            _ if vk == b'F' as u16 => {
                self.outcome = Some(Some(Selection::Monitor(self.to_screen(self.monitor()?.0))))
            }
            _ if vk == b'A' as u16 => {
                self.outcome = Some(Some(Selection::Desktop(self.to_screen(self.bounds()))))
            }
            _ => {}
        }
        Ok(None)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn nudge_failed(&mut self) {
        self.nudge.failed();
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn remember_pointer_position(&mut self, position: (f64, f64)) {
        self.nudge.remember(position);
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn nudge_position(&self) -> Option<(f64, f64)> {
        self.nudge.pending
    }

    /// Describes what is lit right now, for self-tests and logs.
    pub fn describe(&mut self) -> io::Result<String> {
        Ok(match self.lit_now()? {
            Lit::Window(i, r) => {
                let t = &self.targets.list[i];
                format!(
                    "window #{i} (depth {}) {:?} class={} title={:?}",
                    t.depth,
                    self.to_screen(r),
                    t.class,
                    t.title
                )
            }
            Lit::Monitor(r) => format!("screen {:?}", self.to_screen(r)),
            Lit::Region(r) => format!("region {:?}", self.to_screen(r)),
        })
    }
}

// ---- the overlay on screen -----------------------------------------------------

/// The overlay the app is showing, driven by its input (see `ui_api.rs`).
pub(crate) struct Session {
    pub id: u64,
    pub overlay: Overlay,
    failure: Option<String>,
}

impl Session {
    pub(crate) fn new(id: u64, overlay: Overlay) -> Self {
        Self {
            id,
            overlay,
            failure: None,
        }
    }

    pub(crate) fn fail(&mut self, message: String) -> bool {
        if self.overlay.outcome.is_some() {
            return false;
        }
        self.fail_input(message);
        true
    }

    pub(crate) fn fail_input(&mut self, message: String) {
        self.failure = Some(message);
        self.overlay.cancel();
    }

    fn finish(mut self) -> io::Result<(Overlay, Option<Selection>)> {
        if let Some(message) = self.failure {
            return Err(io::Error::other(message));
        }
        let selection = self.overlay.outcome.take().flatten();
        Ok((self.overlay, selection))
    }
}

pub(crate) static SESSION: Mutex<Option<Session>> = Mutex::new(None);
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

/// Esc on the capture thread: cancels even when the overlay could not
/// take the focus.
const HOTKEY_ESC: i32 = 0x5340;

pub(crate) fn session() -> std::sync::MutexGuard<'static, Option<Session>> {
    SESSION.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the app needs to put the overlay up: the session, the desktop and
/// its monitors (desktop pixels).
fn announcement(id: u64, overlay: &Overlay) -> io::Result<String> {
    let monitors: Vec<_> = overlay
        .monitors
        .iter()
        .map(|m| -> io::Result<_> {
            #[cfg(target_os = "macos")]
            let scale = overlay
                .display_layout
                .as_ref()
                .map_or_else(|| m.desktop_scale(), |layout| layout.desktop_scale());
            #[cfg(windows)]
            let scale = m.desktop_scale();
            #[cfg(target_os = "macos")]
            let mapping = match &overlay.display_layout {
                Some(layout) => layout.monitor_mapping(m)?,
                None => jiffshot_capture::CaptureMapping::identity(m.rect).ok_or_else(|| {
                    io::Error::other("the selection monitor has invalid dimensions")
                })?,
            };
            #[cfg(windows)]
            let mapping = jiffshot_capture::CaptureMapping::identity(m.rect)
                .ok_or_else(|| io::Error::other("the selection monitor has invalid dimensions"))?;
            let (width, height) = mapping.size();
            Ok(serde_json::json!({
                "rect": [m.rect.x, m.rect.y, m.rect.width, m.rect.height],
                "pixels": [width, height],
                "bounds": mapping.desktop_bounds(),
                "scale": scale,
                "primary": m.primary,
            }))
        })
        .collect::<io::Result<_>>()?;
    let (x, y) = overlay.screen_cursor();
    Ok(serde_json::json!({
        "id": id,
        "origin": [overlay.origin.0, overlay.origin.1],
        "size": [overlay.bounds.width, overlay.bounds.height],
        "cursor": [x, y],
        "monitors": monitors,
    })
    .to_string())
}

/// Has the app show the overlay and waits, with this thread's messages
/// flowing, until the user selects or cancels. Returns the overlay (for
/// cropping its snapshot) and the selection; `None` at once without the
/// app. A display change cancels a macOS selection with an error.
pub fn run(overlay: Overlay) -> io::Result<(Overlay, Option<Selection>)> {
    #[cfg(target_os = "macos")]
    let layout = overlay.display_layout.clone();
    #[cfg(target_os = "macos")]
    if let Some(layout) = &layout {
        layout.validate()?;
    }
    let Some(hooks) = crate::hooks() else {
        return Ok((overlay, None));
    };
    let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let announce = announcement(id, &overlay)?;
    *session() = Some(Session::new(id, overlay));
    let mut quit = false;
    #[cfg(target_os = "macos")]
    let mut display_error = None;
    // SAFETY: a thread hotkey and a plain message pump; the hotkey is
    // released below.
    unsafe {
        RegisterHotKey(
            std::ptr::null_mut(),
            HOTKEY_ESC,
            MOD_NOREPEAT,
            VK_ESCAPE as u32,
        );
        (hooks.overlay)(announce);
        let mut msg: MSG = std::mem::zeroed();
        while !session()
            .as_ref()
            .is_some_and(|s| s.overlay.outcome.is_some())
            && !quit
        {
            #[cfg(target_os = "macos")]
            if let Some(layout) = &layout {
                if let Err(error) = layout.validate() {
                    display_error = Some(error);
                    break;
                }
            }
            MsgWaitForMultipleObjects(0, std::ptr::null(), 0, 100, QS_ALLINPUT);
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                if msg.message == WM_QUIT {
                    quit = true;
                    break;
                }
                if msg.message == WM_HOTKEY && msg.hwnd.is_null() && msg.wParam as i32 == HOTKEY_ESC
                {
                    if let Some(s) = session().as_mut() {
                        s.overlay.cancel();
                    }
                    continue;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        UnregisterHotKey(std::ptr::null_mut(), HOTKEY_ESC);
        (hooks.overlay)(String::new());
        if quit {
            PostQuitMessage(0);
        }
    }
    let completed = session().take().expect("the session ends here");
    #[cfg(target_os = "macos")]
    {
        if let Some(error) = display_error {
            return Err(error);
        }
        if let Some(layout) = &layout {
            layout.validate()?;
        }
    }
    completed.finish()
}

/// Starts an overlay session over a generated desktop (two screens, the
/// second at 150%, with two windows) without the capture thread, for the
/// UI self-test; returns its announcement. `mode`: 0 normal, 1 freehand,
/// 2 multi-area, 3 video.
pub fn demo(mode: i32) -> String {
    use jiffshot_capture::targets::Target;
    let (w, h) = (2560, 720);
    let mut snapshot = Frame::new(w, h);
    let stride = snapshot.stride();
    for y in 0..h as usize {
        for x in 0..w as usize {
            let p = &mut snapshot.pixels[y * stride + x * 4..][..4];
            // A soft diagonal gradient with a grid, so dimming and the
            // magnifier show.
            let grid = x % 64 == 0 || y % 64 == 0;
            let v = |a: usize| if grid { 0x40 } else { (a % 256) as u8 };
            p.copy_from_slice(&[v(120 + x / 12), v(150 + y / 6), v(200 + (x + y) / 30), 255]);
        }
    }
    let window = |rect: Rect, title: &str, fill: [u8; 3], snapshot: &mut Frame| {
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                let at = y as usize * stride + x as usize * 4;
                let bar = y < rect.y + 32;
                snapshot.pixels[at..at + 3].copy_from_slice(if bar {
                    &[0x30, 0x30, 0x30]
                } else {
                    &fill
                });
            }
        }
        Target {
            hwnd: 0,
            rect,
            title: title.to_owned(),
            class: "Demo".into(),
            process_id: 0,
            depth: 0,
            parent: None,
            #[cfg(target_os = "macos")]
            logical_bounds: None,
        }
    };
    let targets = Targets {
        list: vec![
            window(
                Rect::new(140, 120, 640, 400),
                "Quarterly report.xlsx - Spreadsheet",
                [0xF4, 0xF6, 0xF8],
                &mut snapshot,
            ),
            window(
                Rect::new(1500, 160, 820, 460),
                "Inbox - Mail",
                [0xFA, 0xEE, 0xE0],
                &mut snapshot,
            ),
        ],
    };
    let monitors = vec![
        Monitor {
            rect: Rect::new(0, 0, 1280, 720),
            work: Rect::new(0, 0, 1280, 680),
            dpi: 96,
            primary: true,
            device: "DEMO1".into(),
        },
        Monitor {
            rect: Rect::new(1280, 0, 1280, 720),
            work: Rect::new(1280, 0, 1280, 720),
            dpi: 144,
            primary: false,
            device: "DEMO2".into(),
        },
    ];
    let mut overlay =
        Overlay::new(snapshot, (0, 0), targets, monitors, (400, 300)).expect("a demo overlay");
    overlay.mode = match mode {
        1 => Mode::Freehand,
        2 => Mode::MultiArea,
        3 => Mode::Video,
        _ => Mode::Normal,
    };
    let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let announce = announcement(id, &overlay).expect("the demo monitors have valid dimensions");
    *session() = Some(Session::new(id, overlay));
    announce
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_guides_use_the_cursor_display_pixel_step() {
        let cursor = (-2400, 600);
        for (x_step, y_step, expected) in [
            (1.0, 1.0, (640, 480)),
            (2.0, 2.0, (1280, 960)),
            (4.0 / 3.0, 1.5, (853, 720)),
        ] {
            let guides = guide_rectangles_at(cursor, |width, height| {
                Ok((
                    (width as f64 * x_step).round() as i32,
                    (height as f64 * y_step).round() as i32,
                ))
            })
            .unwrap();
            let (rect, label) = guides[2];
            assert_eq!(label, (640, 480));
            assert_eq!((rect.width, rect.height), expected);
            assert_eq!(rect.center(), cursor);
        }
        assert!(guide_rectangles_at((0, 0), |_, _| Ok((0, 1))).is_err());
        assert!(guide_rectangles_at((i32::MIN, 0), |w, h| Ok((w, h))).is_err());
        assert!(guide_rectangles_at((0, 0), |_, _| Err(io::Error::other("no display"))).is_err());
    }

    fn overlay() -> Overlay {
        let monitors = vec![
            Monitor {
                rect: Rect::new(0, 0, 1000, 600),
                work: Rect::new(0, 0, 1000, 560),
                dpi: 96,
                primary: true,
                device: "A".into(),
            },
            Monitor {
                rect: Rect::new(1000, 0, 800, 600),
                work: Rect::new(1000, 0, 800, 600),
                dpi: 144,
                primary: false,
                device: "B".into(),
            },
        ];
        Overlay::new(
            Frame::new(1800, 600),
            (0, 0),
            Targets::default(),
            monitors,
            (100, 100),
        )
        .unwrap()
    }

    #[test]
    fn wheel_uses_the_current_target_before_a_preview_refresh() {
        use jiffshot_capture::targets::Target;
        let target = |rect, title: &str, depth, parent| Target {
            hwnd: 0,
            rect,
            title: title.into(),
            class: String::new(),
            process_id: 0,
            depth,
            parent,
            #[cfg(target_os = "macos")]
            logical_bounds: None,
        };
        let mut overlay = overlay();
        overlay.targets = Targets {
            list: vec![
                target(Rect::new(0, 0, 100, 100), "Left", 0, None),
                target(Rect::new(10, 10, 80, 80), "Left child", 1, Some(0)),
                target(Rect::new(200, 0, 100, 100), "Right", 0, None),
                target(Rect::new(210, 10, 80, 80), "Right child", 1, Some(2)),
            ],
        };
        overlay.pointer_moved(50, 50);
        assert!(overlay
            .view()
            .unwrap()
            .title
            .unwrap()
            .starts_with("Left child"));
        overlay.pointer_moved(250, 50);
        overlay.wheel(120).unwrap();
        assert_eq!(overlay.view().unwrap().title.as_deref(), Some("Right"));
        overlay.wheel(-120).unwrap();
        assert!(overlay
            .view()
            .unwrap()
            .title
            .unwrap()
            .starts_with("Right child"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn wheel_during_drag_does_not_change_the_selection_pointer() {
        let mut overlay = overlay();
        overlay.pointer_pressed(100, 100);
        overlay.pointer_moved(120, 100);
        overlay.remember_pointer_position((120.25, 100.25));
        overlay
            .wheel_precise(200, 200, (200.25, 200.25), 120)
            .unwrap();
        assert_eq!(overlay.screen_cursor(), (120, 100));
        assert_eq!(
            overlay.nudge.position(overlay.cursor, overlay.origin),
            (120.25, 100.25)
        );
        assert!(overlay.dragging);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn releasing_on_a_monitor_seam_uses_the_unrounded_release_position() {
        let mut overlay = overlay();
        overlay.pointer_pressed(1000, 100);
        overlay.remember_pointer_position((1000.4, 100.0));
        overlay
            .pointer_released_precise(1000, 100, (999.6, 100.0))
            .unwrap();
        assert!(matches!(
            overlay.outcome,
            Some(Some(Selection::Monitor(rect))) if rect == Rect::new(0, 0, 1000, 600)
        ));
        assert_eq!(
            overlay.nudge.position(overlay.cursor, overlay.origin),
            (999.6, 100.0)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn fractional_pointer_selects_the_logical_window_and_child_before_rounding() {
        use jiffshot_capture::screen::LogicalBounds;
        use jiffshot_capture::targets::Target;

        let layout = CaptureLayout::current().unwrap();
        let desktop = layout.virtual_screen();
        let monitor = layout
            .monitors()
            .into_iter()
            .find(|monitor| monitor.primary)
            .unwrap();
        let (x, y) = monitor.rect.center();
        let scale = layout.desktop_scale();
        let target = |left: f64, right: f64, title: &str, depth, parent| {
            let edges = [left, f64::from(y - 20), right, f64::from(y + 20)];
            let rect = Rect::from_ltrb(
                edges[0].round() as i32,
                edges[1] as i32,
                edges[2].round() as i32,
                edges[3] as i32,
            );
            Target {
                hwnd: 0,
                rect,
                title: title.into(),
                class: String::new(),
                process_id: 0,
                depth,
                parent,
                logical_bounds: Some(LogicalBounds::new(edges.map(|edge| edge / scale)).unwrap()),
            }
        };
        let seam = f64::from(x) + 0.125;
        let targets = Targets {
            list: vec![
                target(f64::from(x - 30), seam, "Left", 0, None),
                target(f64::from(x - 10), seam, "Child", 1, Some(0)),
                target(seam, f64::from(x + 30), "Right", 0, None),
            ],
        };
        let mut overlay = Overlay::new(
            Frame::new(0, 0),
            (desktop.x, desktop.y),
            targets,
            layout.monitors(),
            (x, y),
        )
        .unwrap()
        .with_display_layout(layout);
        overlay.bounds = Rect::new(0, 0, desktop.width, desktop.height);
        overlay.remember_pointer_position((f64::from(x) + 0.1, f64::from(y)));
        assert!(overlay.view().unwrap().title.unwrap().starts_with("Child"));
        overlay.wheel(120).unwrap();
        assert_eq!(overlay.view().unwrap().title.as_deref(), Some("Left"));
        overlay.remember_pointer_position((f64::from(x) + 0.15, f64::from(y)));
        assert_eq!(overlay.view().unwrap().title.as_deref(), Some("Right"));
        let client = (x - desktop.x, y - desktop.y);
        overlay.pointer_pressed(client.0, client.1);
        overlay
            .pointer_released_precise(client.0, client.1, (f64::from(x) + 0.1, f64::from(y)))
            .unwrap();
        assert!(matches!(
            overlay.outcome,
            Some(Some(Selection::Window { target: 1, .. }))
        ));
    }

    #[test]
    fn video_guides_move_with_the_overlay_cursor() {
        let mut overlay = overlay();
        let before = overlay.guide_rectangles().unwrap();
        overlay.pointer_moved(1400, 300);
        let after = overlay.guide_rectangles().unwrap();
        assert_eq!(before[3].0.center(), (100, 100));
        assert_eq!(after[3].0.center(), (1400, 300));
        assert_eq!(before[3].1, after[3].1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn video_guide_matches_fixed_selection_at_desktop_edges() {
        let layout = CaptureLayout::current().unwrap();
        let desktop = layout.virtual_screen();
        let cursor = (desktop.right() - 2, desktop.bottom() - 2);
        let mut overlay = Overlay::new(
            Frame::new(0, 0),
            (desktop.x, desktop.y),
            Targets::default(),
            layout.monitors(),
            cursor,
        )
        .unwrap()
        .with_display_layout(layout);
        overlay.bounds = Rect::new(0, 0, desktop.width, desktop.height);
        let guide = overlay.guide_rectangles().unwrap()[3].0;
        overlay.mode = Mode::VideoFixed(320, 240);
        let lit = overlay.lit_now().unwrap().rect();
        assert_eq!(guide, overlay.to_screen(lit));
    }

    #[test]
    fn overlay_failure_finishes_with_an_error_without_replacing_a_selection() {
        let mut failed = Session::new(1, overlay());
        assert!(failed.fail("could not show the selection window".into()));
        assert!(!failed.fail("later error".into()));
        assert_eq!(
            failed.finish().err().unwrap().to_string(),
            "could not show the selection window"
        );

        let mut selected = Session::new(2, overlay());
        let rect = Rect::new(20, 30, 40, 50);
        selected.overlay.outcome = Some(Some(Selection::Region(rect)));
        assert!(!selected.fail("too late".into()));
        assert!(matches!(selected.finish().unwrap().1, Some(Selection::Region(r)) if r == rect));

        let mut invalid = Session::new(3, overlay());
        invalid.overlay.outcome = Some(Some(Selection::Region(rect)));
        invalid.fail_input("invalid selection geometry".into());
        assert_eq!(
            invalid.finish().err().unwrap().to_string(),
            "invalid selection geometry"
        );
    }

    #[test]
    fn synthetic_announcements_keep_identity_monitor_pixels() {
        let json = announcement(42, &overlay()).unwrap();
        let root: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(root["id"], 42);
        assert_eq!(root["size"], serde_json::json!([1800, 600]));
        assert_eq!(
            root["monitors"][0]["pixels"],
            serde_json::json!([1000, 600])
        );
        assert_eq!(root["monitors"][1]["pixels"], serde_json::json!([800, 600]));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn retained_display_announcements_use_each_monitors_native_size() {
        let display = jiffshot_capture::screen::DisplayLayout::current().unwrap();
        let desktop = display.virtual_screen();
        let monitors = display.monitors().to_vec();
        let mut overlay = Overlay::new(
            Frame::new(0, 0),
            (desktop.x, desktop.y),
            Targets::default(),
            monitors,
            desktop.center(),
        )
        .unwrap()
        .with_display_layout(CaptureLayout::from_display(display.clone()));
        overlay.bounds = Rect::new(0, 0, desktop.width, desktop.height);
        let root: serde_json::Value =
            serde_json::from_str(&announcement(43, &overlay).unwrap()).unwrap();
        assert_eq!(
            root["size"],
            serde_json::json!([desktop.width, desktop.height])
        );
        for (index, monitor) in display.monitors().iter().enumerate() {
            let (width, height) = display.monitor_mapping(monitor).unwrap().size();
            assert_eq!(
                root["monitors"][index]["pixels"],
                serde_json::json!([width, height])
            );
        }
        overlay.monitors[0].rect.width += 1;
        assert!(announcement(43, &overlay).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn stale_display_geometry_rejects_an_overlay_before_showing_it() {
        let layout = CaptureLayout::current().unwrap().invalidated_for_test();
        let error = run(overlay().with_display_layout(layout))
            .err()
            .expect("a stale overlay must fail");
        assert!(error.to_string().contains("display configuration changed"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn invalid_pointer_geometry_cannot_select_a_fallback_display_or_fixed_region() {
        let layout = CaptureLayout::current().unwrap();
        let desktop = layout.virtual_screen();
        let cursor = desktop.center();
        let client = (cursor.0 - desktop.x, cursor.1 - desktop.y);
        for mode in [
            Mode::Normal,
            Mode::Fixed(120, 80),
            Mode::VideoFixed(120, 80),
        ] {
            let mut overlay = Overlay::new(
                Frame::new(0, 0),
                (desktop.x, desktop.y),
                Targets::default(),
                layout.monitors(),
                cursor,
            )
            .unwrap()
            .with_display_layout(layout.clone());
            overlay.bounds = Rect::new(0, 0, desktop.width, desktop.height);
            overlay.mode = mode;
            overlay.remember_pointer_position((f64::NAN, 0.0));
            assert!(overlay.view().is_err());
            assert!(overlay.key(VK_RIGHT, false).is_err());
            assert!(overlay.key(b'F' as u16, false).is_err());
            assert!(overlay.outcome.is_none());
            overlay.pointer_pressed(client.0, client.1);
            assert!(overlay
                .pointer_released_precise(client.0, client.1, (f64::NAN, 0.0))
                .is_err());
            assert!(overlay.outcome.is_none());
        }
    }

    #[test]
    fn hover_lights_the_screen_under_the_pointer() {
        let mut o = overlay();
        let v = o.view().unwrap();
        assert_eq!(
            (v.lit, v.lit_kind, v.scale),
            (Rect::new(0, 0, 1000, 600), 2, 1.0)
        );
        o.pointer_moved(1200, 300);
        let v = o.view().unwrap();
        assert_eq!(
            (v.lit, v.title.as_deref(), v.scale),
            (Rect::new(1000, 0, 800, 600), Some("Screen"), 1.5)
        );
    }

    #[test]
    fn a_drag_selects_a_region_and_modifiers_constrain_it() {
        let mut o = overlay();
        o.pointer_pressed(100, 100);
        o.pointer_moved(300, 150);
        assert_eq!(
            o.view().unwrap().lit,
            Rect::from_points((100, 100), (300, 150))
        );
        o.set_modifiers(true, false);
        o.pointer_moved(300, 150);
        let v = o.view().unwrap();
        assert_eq!(
            (v.lit.width, v.lit.height),
            (v.lit.width, v.lit.width),
            "Shift makes a square"
        );
        assert!(v.dragging);
        o.pointer_released(300, 150).unwrap();
        assert!(matches!(o.outcome, Some(Some(Selection::Region(r))) if r.width == r.height));
    }

    #[test]
    fn constrained_drags_keep_their_shape_at_desktop_edges() {
        let bounds = Rect::new(0, 0, 100, 80);
        assert_eq!(
            constrained((20, 20), (90, 30), true, false, bounds),
            Some(Rect::new(20, 20, 60, 60))
        );
        assert_eq!(
            constrained((80, 60), (10, 50), true, false, bounds),
            Some(Rect::new(20, 0, 61, 61))
        );
        assert_eq!(
            constrained((20, 20), (30, 75), false, true, bounds),
            Some(Rect::new(20, 20, 80, 45))
        );
        assert_eq!(
            constrained((20, 20), (90, 30), false, false, bounds),
            Some(Rect::new(20, 20, 71, 11))
        );
    }

    #[test]
    fn keys_toggle_help_and_pick_the_screen() {
        let mut o = overlay();
        o.key(VK_F1, false).unwrap();
        assert!(o.view().unwrap().help);
        assert_eq!(o.key(VK_RIGHT, true).unwrap(), Some((110, 100)));
        o.key(b'F' as u16, false).unwrap();
        assert!(
            matches!(o.outcome, Some(Some(Selection::Monitor(r))) if r == Rect::new(0, 0, 1000, 600))
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn fractional_nudges_accumulate_and_reset_on_mouse_motion() {
        let mut nudge = Nudge::default();
        let mut position = (0, 0);
        for expected in [1, 3, 4] {
            let next = nudge
                .target(position, (0, 0), (1, 0), (4.0 / 3.0, 2.0), 1)
                .unwrap();
            assert_eq!(next, (expected, 0));
            nudge.moved(next, (0, 0));
            position = next;
        }
        for _ in 0..3 {
            let next = nudge
                .target(position, (0, 0), (-1, 0), (4.0 / 3.0, 2.0), 1)
                .unwrap();
            nudge.moved(next, (0, 0));
            position = next;
        }
        assert_eq!(position, (0, 0));
        let next = nudge
            .target(position, (0, 0), (1, 0), (4.0 / 3.0, 2.0), 10)
            .unwrap();
        assert_eq!(next, (13, 0));
        nudge.moved(next, (0, 0));
        nudge.moved((20, 0), (0, 0));
        assert_eq!(
            nudge
                .target((20, 0), (0, 0), (1, 0), (4.0 / 3.0, 2.0), 1)
                .unwrap(),
            (21, 0)
        );

        let mut nudge = Nudge::default();
        let first = nudge.target((0, 0), (0, 0), (1, 0), (1.5, 1.0), 1).unwrap();
        nudge.moved(first, (0, 0));
        let down = nudge.target(first, (0, 0), (0, 1), (1.5, 1.0), 1).unwrap();
        assert_eq!(down, (2, 1));
        nudge.moved(down, (0, 0));
        assert_eq!(
            nudge.target(down, (0, 0), (1, 0), (1.5, 1.0), 1).unwrap(),
            (3, 1)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn fractional_nudges_keep_exact_global_positions_and_reject_invalid_targets() {
        let mut nudge = Nudge::default();
        let origin = (-100, 50);
        nudge.remember((-17.25, 60.125));
        let current = client_point((-17.25, 60.125), origin).unwrap();
        let first = nudge
            .target(current, origin, (1, 0), (4.0 / 3.0, 2.0), 1)
            .unwrap();
        let expected = -17.25 + 4.0 / 3.0;
        assert_eq!(first, client_point((expected, 60.125), origin).unwrap());
        assert_eq!(nudge.pending, Some((expected, 60.125)));
        nudge.moved(first, origin);
        let second = nudge
            .target(first, origin, (1, 0), (4.0 / 3.0, 2.0), 1)
            .unwrap();
        assert!((nudge.pending.unwrap().0 - (-17.25 + 8.0 / 3.0)).abs() < 1e-12);
        nudge.moved(second, origin);
        nudge.remember((-18.5, 60.25));
        let mouse = client_point((-18.5, 60.25), origin).unwrap();
        assert_eq!(mouse, (81, 10));
        let next = nudge.target(mouse, origin, (0, 1), (1.5, 2.0), 1).unwrap();
        assert_eq!(next, (81, 12));
        assert_eq!(nudge.pending, Some((-18.5, 62.25)));
        nudge.failed();
        assert!(nudge.position.is_none());
        assert!(nudge.pending.is_none());
        assert!(nudge
            .target((0, 0), (0, 0), (1, 0), (f64::INFINITY, 1.0), 1)
            .is_err());
        assert!(client_point((f64::NAN, 0.0), origin).is_err());
        assert!(client_point((0.0, 0.0), (i32::MIN, 0)).is_err());
    }

    #[test]
    fn video_fixed_region_can_be_toggled_and_placed() {
        let monitors = vec![Monitor {
            rect: Rect::new(0, 0, 2560, 1440),
            work: Rect::new(0, 0, 2560, 1400),
            dpi: 96,
            primary: true,
            device: "A".into(),
        }];
        let mut o = Overlay::new(
            Frame::new(2560, 1440),
            (0, 0),
            Targets::default(),
            monitors,
            (1200, 700),
        )
        .unwrap();
        o.mode = Mode::Video;

        for (index, &(w, h)) in GUIDES.iter().enumerate() {
            o.key(b'1' as u16 + index as u16, false).unwrap();
            assert_eq!(
                (o.view().unwrap().lit.width, o.view().unwrap().lit.height),
                (w, h)
            );
        }
        o.key(b'1' as u16, false).unwrap();
        assert_eq!(o.view().unwrap().lit, Rect::new(240, 160, 1920, 1080));
        o.key(b'1' as u16, false).unwrap();
        assert_eq!(
            o.view().unwrap().lit_kind,
            2,
            "pressing the active size again restores window selection"
        );

        o.key(b'1' as u16, false).unwrap();
        o.pointer_pressed(1200, 700);
        o.pointer_moved(1300, 800);
        o.pointer_released(1300, 800).unwrap();
        assert!(
            matches!(o.outcome, Some(Some(Selection::Region(r))) if r == Rect::new(340, 260, 1920, 1080))
        );
    }

    #[test]
    fn fixed_video_selection_keeps_its_size_when_the_desktop_is_smaller() {
        let monitors = vec![Monitor {
            rect: Rect::new(0, 0, 1280, 720),
            work: Rect::new(0, 0, 1280, 680),
            dpi: 96,
            primary: true,
            device: "A".into(),
        }];
        let mut o = Overlay::new(
            Frame::new(1280, 720),
            (0, 0),
            Targets::default(),
            monitors,
            (600, 300),
        )
        .unwrap();
        o.mode = Mode::VideoFixed(1920, 1080);

        assert_eq!(o.view().unwrap().size, "1920 × 1080");
        o.pointer_pressed(600, 300);
        o.pointer_released(600, 300).unwrap();
        assert!(
            matches!(o.outcome, Some(Some(Selection::Region(r))) if (r.width, r.height) == (1920, 1080))
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn fixed_video_reports_the_requested_size_outside_the_preview() {
        let layout = CaptureLayout::current().unwrap();
        let monitor = layout
            .monitors()
            .into_iter()
            .find(|monitor| monitor.primary)
            .unwrap();
        let origin = (monitor.rect.x, monitor.rect.y);
        let cursor = (origin.0 + 32, origin.1 + 32);
        let mut overlay = Overlay::new(
            Frame::new(64, 64),
            origin,
            Targets::default(),
            vec![monitor],
            cursor,
        )
        .unwrap()
        .with_display_layout(layout);
        overlay.mode = Mode::VideoFixed(128, 96);
        assert_eq!(overlay.view().unwrap().size, "128 × 96");
        overlay.key(VK_RETURN, false).unwrap();
        assert!(matches!(
            overlay.outcome,
            Some(Some(Selection::Fixed {
                size: (128, 96),
                ..
            }))
        ));
    }
}
