//! The selection overlay's logic on Linux, the same behavior as the Windows and macOS overlay
//! (`jiffshot-workflow`'s `overlay.rs`) over one frozen screenshot of the whole desktop.
//!
//! Wayland offers no window geometry, so what is lit is a dragged region or the whole screen.
//! The window shows the overlay and feeds its input here (see `win/ui/overlay_linux.rs`).

use jiffshot_capture::{Frame, Rect};

pub const VK_BACK: u16 = 0x08;
pub const VK_RETURN: u16 = 0x0D;
pub const VK_ESCAPE: u16 = 0x1B;
pub const VK_SPACE: u16 = 0x20;
pub const VK_LEFT: u16 = 0x25;
pub const VK_UP: u16 = 0x26;
pub const VK_RIGHT: u16 = 0x27;
pub const VK_DOWN: u16 = 0x28;
pub const VK_F1: u16 = 0x70;

/// How the overlay selects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// A dragged region, or the whole screen.
    #[default]
    Normal,
    Freehand,
    /// Several regions, finished with Enter.
    MultiArea,
    /// A rectangle of this size, placed by clicking.
    Fixed(i32, i32),
}

/// What the user picked, in desktop pixels.
#[derive(Clone, Debug, PartialEq)]
pub enum Selection {
    Region(Rect),
    /// A freehand outline inside `rect`.
    Freehand { rect: Rect, points: Vec<(i32, i32)> },
    /// Several regions, all inside `rect`.
    Multi { rect: Rect, regions: Vec<Rect> },
    Desktop(Rect),
}

impl Selection {
    pub fn rect(&self) -> Rect {
        match self {
            Selection::Region(r) | Selection::Desktop(r) | Selection::Freehand { rect: r, .. } | Selection::Multi { rect: r, .. } => *r,
        }
    }
}

const DRAG_THRESHOLD: i32 = 3;

/// The F1 help, one line each.
pub const HELP: &[&str] = &[
    "Click: the whole screen   ·   Drag: region",
    "Shift: square   ·   Ctrl: 16:9   ·   Ctrl+Shift: 4:3",
    "Arrows: move 1 px (Shift: 10)   ·   Shift+Enter: start a region at the pointer",
    "Enter / Space: capture   ·   F or A: the whole screen",
    "M: magnifier   ·   F1: this help   ·   Esc: cancel",
];

fn hint(mode: Mode) -> &'static str {
    match mode {
        Mode::Freehand => "Draw around the area to capture  ·  Esc: cancel  ·  F1: help",
        Mode::MultiArea => "Drag each area to capture, then press Enter  ·  Backspace: remove the last  ·  Esc: cancel",
        Mode::Fixed(..) => "Click to place the fixed-size area  ·  Arrows: nudge  ·  Esc: cancel",
        Mode::Normal => "Drag a region, or click for the whole screen  ·  F1: help  ·  Esc: cancel",
    }
}

/// What to draw now, in desktop pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    pub cursor: (i32, i32),
    pub lit: Rect,
    /// 0 a region, 2 a screen.
    pub lit_kind: i32,
    pub title: Option<String>,
    pub size: String,
    pub monitor: Rect,
    pub color: Option<(u8, u8, u8)>,
    pub hint: &'static str,
    pub magnifier: bool,
    pub help: bool,
    pub dragging: bool,
}

pub struct Overlay {
    pub snapshot: Frame,
    bounds: Rect,
    cursor: (i32, i32),
    press: Option<(i32, i32)>,
    dragging: bool,
    shift: bool,
    ctrl: bool,
    pub outcome: Option<Option<Selection>>,
    pub mode: Mode,
    show_magnifier: bool,
    show_help: bool,
    path: Vec<(i32, i32)>,
    regions: Vec<Rect>,
}

/// The drag rectangle, constrained by modifiers and the desktop bounds.
fn constrained(a: (i32, i32), b: (i32, i32), shift: bool, ctrl: bool, bounds: Rect) -> Option<Rect> {
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
    let max_x = if dx < 0 { a.0 - bounds.x } else { bounds.right() - 1 - a.0 };
    let max_y = if dy < 0 { a.1 - bounds.y } else { bounds.bottom() - 1 - a.1 };
    let width = w.min(max_x as f32).min(max_y as f32 * ratio);
    let height = width / ratio;
    Some(Rect::from_points(a, (a.0 + (width * sx) as i32, a.1 + (height * sy) as i32)))
}

fn place_fixed(bounds: Rect, cursor: (i32, i32), width: i32, height: i32) -> Rect {
    let x = if bounds.width >= width { (cursor.0 - width / 2).clamp(bounds.x, bounds.right() - width) } else { bounds.x };
    let y = if bounds.height >= height { (cursor.1 - height / 2).clamp(bounds.y, bounds.bottom() - height) } else { bounds.y };
    Rect::new(x, y, width, height)
}

impl Overlay {
    pub fn new(snapshot: Frame, cursor: (i32, i32), mode: Mode) -> Overlay {
        let bounds = snapshot.bounds();
        Overlay {
            snapshot,
            bounds,
            cursor,
            press: None,
            dragging: false,
            shift: false,
            ctrl: false,
            outcome: None,
            mode,
            show_magnifier: true,
            show_help: false,
            path: Vec::new(),
            regions: Vec::new(),
        }
    }

    pub fn path(&self) -> &[(i32, i32)] {
        &self.path
    }

    pub fn regions(&self) -> &[Rect] {
        &self.regions
    }

    pub fn set_modifiers(&mut self, shift: bool, ctrl: bool) {
        self.shift = shift;
        self.ctrl = ctrl;
    }

    fn lit_now(&self) -> (Rect, i32) {
        if self.mode == Mode::Freehand && !self.path.is_empty() {
            let r = self.path.iter().fold(Rect::new(self.path[0].0, self.path[0].1, 1, 1), |r, p| r.union(Rect::new(p.0, p.1, 1, 1)));
            return (r, 0);
        }
        if let (true, Some(press)) = (self.dragging, self.press) {
            return (constrained(press, self.cursor, self.shift, self.ctrl, self.bounds).unwrap_or_default(), 0);
        }
        if let Mode::Fixed(w, h) = self.mode {
            return (place_fixed(self.bounds, self.cursor, w, h).intersect(self.bounds).unwrap_or_default(), 0);
        }
        (self.bounds, 2)
    }

    pub fn view(&self) -> View {
        let (lit, lit_kind) = self.lit_now();
        let area = lit.intersect(self.bounds).unwrap_or_default();
        View {
            cursor: self.cursor,
            lit: area,
            lit_kind,
            title: (lit_kind == 2).then(|| "Screen".to_owned()),
            size: format!("{} × {}", lit.width, lit.height),
            monitor: self.bounds,
            color: self.snapshot.rgb(self.cursor.0, self.cursor.1),
            hint: hint(self.mode),
            magnifier: self.show_magnifier,
            help: self.show_help,
            dragging: self.dragging,
        }
    }

    pub fn pointer_moved(&mut self, x: i32, y: i32) {
        self.cursor = (x, y);
        if let (false, Some((px, py))) = (self.dragging, self.press) {
            if (x - px).abs() > DRAG_THRESHOLD || (y - py).abs() > DRAG_THRESHOLD {
                self.dragging = true;
            }
        }
        if self.mode == Mode::Freehand && self.press.is_some() && self.path.last() != Some(&(x, y)) {
            self.path.push((x, y));
        }
    }

    pub fn pointer_pressed(&mut self, x: i32, y: i32) {
        self.cursor = (x, y);
        self.press = Some((x, y));
    }

    pub fn pointer_released(&mut self, x: i32, y: i32) {
        self.cursor = (x, y);
        if self.mode == Mode::Freehand {
            if self.path.len() >= 3 {
                let rect = self.lit_now().0.intersect(self.bounds).unwrap_or_default();
                self.outcome = Some(Some(Selection::Freehand { rect, points: self.path.clone() }));
            }
            self.path.clear();
            self.press = None;
            self.dragging = false;
            return;
        }
        if self.mode == Mode::MultiArea {
            if self.dragging {
                if let Some(r) = self.press.and_then(|p| constrained(p, self.cursor, self.shift, self.ctrl, self.bounds)) {
                    self.regions.push(r);
                }
            }
            self.press = None;
            self.dragging = false;
            return;
        }
        if self.dragging {
            if let Some(press) = self.press {
                let region = constrained(press, self.cursor, self.shift, self.ctrl, self.bounds);
                self.outcome = Some(region.map(Selection::Region));
            }
        } else if self.press.is_some() {
            self.select_lit();
        }
        self.press = None;
        self.dragging = false;
    }

    fn select_lit(&mut self) {
        let (rect, kind) = self.lit_now();
        self.outcome = Some(Some(if kind == 2 { Selection::Desktop(rect) } else { Selection::Region(rect) }));
    }

    pub fn cancel(&mut self) {
        self.outcome = Some(None);
    }

    /// Handles a key; returns a new pointer position when the key nudges it.
    pub fn key(&mut self, vk: u16, shift: bool) -> Option<(i32, i32)> {
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
            return Some((x + dx * step, y + dy * step));
        }
        match vk {
            VK_ESCAPE => self.cancel(),
            VK_RETURN if shift && !self.dragging => {
                self.press = Some((x, y));
                self.dragging = true;
            }
            VK_RETURN | VK_SPACE if self.mode == Mode::MultiArea => {
                if self.dragging {
                    self.pointer_released(x, y);
                }
                if let Some(first) = self.regions.first().copied() {
                    let rect = self.regions.iter().fold(first, |a, r| a.union(*r));
                    self.outcome = Some(Some(Selection::Multi { rect, regions: self.regions.clone() }));
                }
            }
            VK_BACK if self.mode == Mode::MultiArea => {
                self.regions.pop();
            }
            VK_RETURN | VK_SPACE => {
                if self.dragging {
                    self.pointer_released(x, y);
                } else {
                    self.select_lit();
                }
            }
            VK_F1 => self.show_help = !self.show_help,
            _ if vk == b'M' as u16 => self.show_magnifier = !self.show_magnifier,
            _ if vk == b'F' as u16 || vk == b'A' as u16 => self.outcome = Some(Some(Selection::Desktop(self.bounds))),
            _ => {}
        }
        None
    }
}

/// The part of `frame` a selection covers: the region itself, or the bounding box of a
/// freehand outline or several regions with everything outside them made transparent.
pub fn crop(frame: &Frame, selection: &Selection) -> Option<Frame> {
    let mut out = frame.crop(selection.rect())?;
    let keep: Box<dyn Fn(i32, i32) -> bool> = match selection {
        Selection::Region(_) | Selection::Desktop(_) => return Some(out),
        Selection::Multi { regions, .. } => {
            let regions = regions.clone();
            Box::new(move |x, y| regions.iter().any(|r| r.contains(x, y)))
        }
        Selection::Freehand { points, .. } => {
            let points = points.clone();
            Box::new(move |x, y| inside_polygon(&points, x, y))
        }
    };
    let origin = frame.bounds().intersect(selection.rect())?;
    out.alpha = true;
    for y in 0..out.height as usize {
        for x in 0..out.width as usize {
            if !keep(origin.x + x as i32, origin.y + y as i32) {
                let at = (y * out.width as usize + x) * 4;
                out.pixels[at..at + 4].fill(0);
            }
        }
    }
    Some(out)
}

fn inside_polygon(points: &[(i32, i32)], x: i32, y: i32) -> bool {
    let (px, py) = (x as f64 + 0.5, y as f64 + 0.5);
    let mut inside = false;
    let mut j = points.len().saturating_sub(1);
    for i in 0..points.len() {
        let (xi, yi) = (points[i].0 as f64, points[i].1 as f64);
        let (xj, yj) = (points[j].0 as f64, points[j].1 as f64);
        if (yi > py) != (yj > py) && px < (xj - xi) * (py - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlay(mode: Mode) -> Overlay {
        Overlay::new(Frame::new(200, 100), (10, 10), mode)
    }

    #[test]
    fn click_selects_the_screen() {
        let mut o = overlay(Mode::Normal);
        o.pointer_pressed(20, 20);
        o.pointer_released(20, 20);
        assert_eq!(o.outcome, Some(Some(Selection::Desktop(Rect::new(0, 0, 200, 100)))));
    }

    #[test]
    fn drag_selects_a_region_clipped_to_the_screen() {
        let mut o = overlay(Mode::Normal);
        o.pointer_pressed(50, 40);
        o.pointer_moved(300, 90);
        assert_eq!(o.view().lit, Rect::new(50, 40, 150, 51));
        o.pointer_released(300, 90);
        assert_eq!(o.outcome, Some(Some(Selection::Region(Rect::new(50, 40, 150, 51)))));
    }

    #[test]
    fn modifiers_constrain_the_ratio() {
        let mut o = overlay(Mode::Normal);
        o.set_modifiers(true, false);
        o.pointer_pressed(10, 10);
        o.pointer_moved(60, 30);
        assert_eq!(o.view().lit, Rect::new(10, 10, 51, 51));
    }

    #[test]
    fn keys_nudge_cancel_and_toggle() {
        let mut o = overlay(Mode::Normal);
        assert_eq!(o.key(VK_RIGHT, true), Some((20, 10)));
        assert_eq!(o.key(VK_UP, false), Some((10, 9)));
        o.key(VK_F1, false);
        assert!(o.view().help);
        o.key(b'M' as u16, false);
        assert!(!o.view().magnifier);
        o.key(VK_ESCAPE, false);
        assert_eq!(o.outcome, Some(None));
    }

    #[test]
    fn keyboard_region_starts_at_the_pointer_and_ends_with_enter() {
        let mut o = overlay(Mode::Normal);
        o.key(VK_RETURN, true);
        o.pointer_moved(40, 30);
        o.key(VK_RETURN, false);
        assert_eq!(o.outcome, Some(Some(Selection::Region(Rect::new(10, 10, 31, 21)))));
    }

    #[test]
    fn multi_area_collects_regions_until_enter() {
        let mut o = overlay(Mode::MultiArea);
        for (a, b) in [((10, 10), (30, 30)), ((50, 50), (80, 70))] {
            o.pointer_pressed(a.0, a.1);
            o.pointer_moved(b.0, b.1);
            o.pointer_released(b.0, b.1);
        }
        assert_eq!(o.regions().len(), 2);
        o.key(VK_BACK, false);
        assert_eq!(o.regions().len(), 1);
        o.key(VK_RETURN, false);
        assert_eq!(o.outcome, Some(Some(Selection::Multi { rect: Rect::new(10, 10, 21, 21), regions: vec![Rect::new(10, 10, 21, 21)] })));
    }

    #[test]
    fn freehand_needs_a_few_points() {
        let mut o = overlay(Mode::Freehand);
        o.pointer_pressed(10, 10);
        for p in [(40, 10), (40, 40), (10, 40)] {
            o.pointer_moved(p.0, p.1);
        }
        o.pointer_released(10, 40);
        let Some(Some(Selection::Freehand { rect, points })) = o.outcome else { panic!("no freehand selection") };
        assert_eq!(rect, Rect::new(10, 10, 31, 31));
        assert_eq!(points.len(), 3);
    }

    #[test]
    fn fixed_size_follows_the_pointer_and_stays_inside() {
        let mut o = overlay(Mode::Fixed(60, 40));
        o.pointer_moved(5, 5);
        assert_eq!(o.view().lit, Rect::new(0, 0, 60, 40));
        o.pointer_pressed(5, 5);
        o.pointer_released(5, 5);
        assert_eq!(o.outcome, Some(Some(Selection::Region(Rect::new(0, 0, 60, 40)))));
    }

    #[test]
    fn crop_clears_pixels_outside_a_freehand_outline() {
        let mut frame = Frame::new(20, 20);
        frame.pixels.fill(255);
        let selection = Selection::Freehand { rect: Rect::new(0, 0, 10, 10), points: vec![(0, 0), (10, 0), (0, 10)] };
        let out = crop(&frame, &selection).unwrap();
        assert!(out.alpha);
        assert_eq!(out.pixels[(10 + 1) * 4 + 3], 255);
        assert_eq!(out.pixels[(9 * 10 + 9) * 4 + 3], 0);
    }
}
