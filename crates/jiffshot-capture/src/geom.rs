use serde::{Deserialize, Serialize};

/// An integer rectangle; `width`/`height` of zero or less is empty.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    pub const fn from_ltrb(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        Rect {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        }
    }

    /// The rectangle spanning two pixels, both included.
    pub fn from_points(a: (i32, i32), b: (i32, i32)) -> Rect {
        Rect::from_ltrb(
            a.0.min(b.0),
            a.1.min(b.1),
            a.0.max(b.0) + 1,
            a.1.max(b.1) + 1,
        )
    }

    pub const fn right(self) -> i32 {
        self.x + self.width
    }

    pub const fn bottom(self) -> i32 {
        self.y + self.height
    }

    pub const fn is_empty(self) -> bool {
        self.width <= 0 || self.height <= 0
    }

    pub const fn area(self) -> i64 {
        if self.is_empty() {
            0
        } else {
            self.width as i64 * self.height as i64
        }
    }

    pub const fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }

    pub fn intersect(self, other: Rect) -> Option<Rect> {
        let r = Rect::from_ltrb(
            self.x.max(other.x),
            self.y.max(other.y),
            self.right().min(other.right()),
            self.bottom().min(other.bottom()),
        );
        (!r.is_empty()).then_some(r)
    }

    pub fn union(self, other: Rect) -> Rect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        Rect::from_ltrb(
            self.x.min(other.x),
            self.y.min(other.y),
            self.right().max(other.right()),
            self.bottom().max(other.bottom()),
        )
    }

    pub const fn inflate(self, d: i32) -> Rect {
        Rect::new(
            self.x - d,
            self.y - d,
            self.width + 2 * d,
            self.height + 2 * d,
        )
    }

    pub const fn offset(self, dx: i32, dy: i32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.width, self.height)
    }

    pub const fn center(self) -> (i32, i32) {
        (self.x + self.width / 2, self.y + self.height / 2)
    }
}

/// Maps a desktop-space capture rectangle to its output image pixels.
///
/// `rect` is the integer compatibility rectangle. The desktop bounds retain
/// the actual edges when a display cannot be represented on that grid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CaptureMapping {
    rect: Rect,
    width: u32,
    height: u32,
    bounds: [f64; 4],
}

// Bounds are private and every constructor rejects non-finite values.
impl Eq for CaptureMapping {}

impl CaptureMapping {
    pub fn new(rect: Rect, width: u32, height: u32) -> Option<Self> {
        let right = rect.x.checked_add(rect.width)?;
        let bottom = rect.y.checked_add(rect.height)?;
        Self::with_desktop_bounds(rect, width, height, [rect.x as f64, rect.y as f64, right as f64, bottom as f64])
    }

    /// Constructs a mapping with precise left, top, right and bottom edges.
    /// Each edge must round to the corresponding edge of `rect`.
    pub fn with_desktop_bounds(rect: Rect, width: u32, height: u32, bounds: [f64; 4]) -> Option<Self> {
        let right = rect.x.checked_add(rect.width)?;
        let bottom = rect.y.checked_add(rect.height)?;
        let [left, top, precise_right, precise_bottom] = bounds;
        if rect.is_empty() || width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32
            || !bounds.iter().all(|edge| edge.is_finite())
            || !((precise_right - left).is_finite() && (precise_bottom - top).is_finite())
            || precise_right <= left || precise_bottom <= top
            || left.round() != rect.x as f64 || top.round() != rect.y as f64
            || precise_right.round() != right as f64 || precise_bottom.round() != bottom as f64
        {
            return None;
        }
        Some(Self { rect, width, height, bounds })
    }

    pub fn identity(rect: Rect) -> Option<Self> {
        Self::new(rect, rect.width as u32, rect.height as u32)
    }

    /// Changes the image dimensions without changing the captured desktop.
    pub fn resized(self, width: u32, height: u32) -> Option<Self> {
        Self::with_desktop_bounds(self.rect, width, height, self.bounds)
    }

    pub fn rect(self) -> Rect {
        self.rect
    }

    /// Precise desktop-space edges in left, top, right, bottom order.
    pub fn desktop_bounds(self) -> [f64; 4] {
        self.bounds
    }

    pub fn size(self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn contains_point(self, x: i32, y: i32) -> bool {
        self.contains_precise_point(x as f64, y as f64)
    }

    /// Tests a desktop-space point without rounding it to the compatibility grid.
    pub fn contains_precise_point(self, x: f64, y: f64) -> bool {
        let [left, top, right, bottom] = self.bounds;
        x >= left && x < right && y >= top && y < bottom
    }

    pub fn intersects_rect(self, rect: Rect) -> bool {
        let [left, top, right, bottom] = self.bounds;
        !rect.is_empty()
            && (rect.x as f64) < right && (rect.y as f64) < bottom
            && (i64::from(rect.x) + i64::from(rect.width)) as f64 > left
            && (i64::from(rect.y) + i64::from(rect.height)) as f64 > top
    }

    fn projected_point(self, x: f64, y: f64) -> (f64, f64) {
        let [left, top, right, bottom] = self.bounds;
        (
            ((x - left) * self.width as f64 / (right - left)).floor(),
            ((y - top) * self.height as f64 / (bottom - top)).floor(),
        )
    }

    /// Maps a desktop-space point to its containing output pixel.
    pub fn image_point(self, x: i32, y: i32) -> (i32, i32) {
        let (x, y) = self.projected_point(x as f64, y as f64);
        (x as i32, y as i32)
    }

    /// Maps an unrounded desktop-space point, rejecting invalid or unrepresentable results.
    pub fn image_point_precise(self, x: f64, y: f64) -> Option<(i32, i32)> {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        let (x, y) = self.projected_point(x, y);
        let valid = |value: f64| value.is_finite() && value >= i32::MIN as f64 && value <= i32::MAX as f64;
        (valid(x) && valid(y)).then_some((x as i32, y as i32))
    }

    /// Maps desktop-space rectangle edges to output-image edges.
    pub fn image_rect(self, rect: Rect) -> Rect {
        let [left, top, right, bottom] = self.bounds;
        let edge = |x: i32, y: i32| {
            let x = (x as f64 - left) * self.width as f64 / (right - left);
            let y = (y as f64 - top) * self.height as f64 / (bottom - top);
            (x.round() as i32, y.round() as i32)
        };
        let (left, top) = edge(rect.x, rect.y);
        let (right, bottom) = edge(rect.right(), rect.bottom());
        Rect::from_ltrb(left, top, right, bottom)
    }

    /// Maps an image-pixel rectangle back to the covered desktop pixels.
    pub fn desktop_rect(self, rect: Rect) -> Option<Rect> {
        let image_right = rect.x.checked_add(rect.width)?;
        let image_bottom = rect.y.checked_add(rect.height)?;
        if rect.is_empty() || rect.x < 0 || rect.y < 0
            || image_right > self.width as i32 || image_bottom > self.height as i32
        {
            return None;
        }
        let [left, top, right, bottom] = self.bounds;
        let project_x = |x: i32| left + x as f64 * (right - left) / self.width as f64;
        let project_y = |y: i32| top + y as f64 * (bottom - top) / self.height as f64;
        let x = project_x(rect.x).floor().max(self.rect.x as f64) as i32;
        let y = project_y(rect.y).floor().max(self.rect.y as f64) as i32;
        let right = project_x(image_right).ceil().min(self.rect.right() as f64) as i32;
        let bottom = project_y(image_bottom).ceil().min(self.rect.bottom() as f64) as i32;
        (right > x && bottom > y).then(|| Rect::from_ltrb(x, y, right, bottom))
    }

    /// The desktop-space location at the center of an output pixel.
    pub fn pixel_center(self, x: u32, y: u32) -> (f64, f64) {
        let [left, top, right, bottom] = self.bounds;
        (
            left + (x as f64 + 0.5) * (right - left) / self.width as f64,
            top + (y as f64 + 0.5) * (bottom - top) / self.height as f64,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{CaptureMapping, Rect};

    #[test]
    fn from_points_includes_both_ends() {
        assert_eq!(
            Rect::from_points((10, 20), (5, 30)),
            Rect::new(5, 20, 6, 11)
        );
        assert_eq!(Rect::from_points((3, 3), (3, 3)), Rect::new(3, 3, 1, 1));
    }

    #[test]
    fn intersect_and_union() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        assert_eq!(a.intersect(b), Some(Rect::new(5, 5, 5, 5)));
        assert_eq!(a.intersect(Rect::new(10, 0, 5, 5)), None);
        assert_eq!(a.union(b), Rect::new(0, 0, 15, 15));
        assert_eq!(Rect::default().union(b), b);
    }

    #[test]
    fn capture_mapping_handles_negative_origins_and_shared_edges() {
        let mapping = CaptureMapping::new(Rect::new(-200, -40, 600, 200), 300, 100).unwrap();
        assert_eq!(mapping.size(), (300, 100));
        assert_eq!(mapping.image_point(-200, -40), (0, 0));
        assert_eq!(mapping.image_point(0, 0), (100, 20));
        assert_eq!(mapping.image_rect(Rect::new(-200, -40, 200, 200)), Rect::new(0, 0, 100, 100));
        assert_eq!(mapping.image_rect(Rect::new(0, -40, 400, 200)), Rect::new(100, 0, 200, 100));
        assert_eq!(mapping.pixel_center(100, 20), (1.0, 1.0));
        assert!(CaptureMapping::identity(Rect::default()).is_none());
    }

    #[test]
    fn downsampled_points_stay_inside_the_last_output_pixel() {
        let mapping = CaptureMapping::new(Rect::new(-4, 10, 4, 4), 2, 2).unwrap();
        assert_eq!(mapping.image_point(-4, 10), (0, 0));
        assert_eq!(mapping.image_point(-1, 13), (1, 1));
        assert_eq!(mapping.image_point(-5, 9), (-1, -1));
        assert_eq!(mapping.image_rect(Rect::new(-4, 10, 3, 4)), Rect::new(0, 0, 2, 2));
    }

    #[test]
    fn precise_points_use_fractional_edges_and_reject_invalid_coordinates() {
        let mapping = CaptureMapping::with_desktop_bounds(
            Rect::new(-3, 4, 20, 10), 40, 20, [-3.25, 4.25, 16.75, 14.25],
        ).unwrap();
        assert!(mapping.contains_precise_point(-3.2, 4.3));
        assert!(!mapping.contains_precise_point(16.75, 5.0));
        assert!(!mapping.contains_precise_point(f64::NAN, 5.0));
        assert_eq!(mapping.image_point_precise(-1.51, 5.74), Some((3, 2)));
        assert_eq!(mapping.image_point(-2, 6), (2, 3));
        assert_eq!(mapping.image_point_precise(f64::NAN, 0.0), None);
        assert_eq!(mapping.image_point_precise(f64::INFINITY, 0.0), None);
        assert_eq!(mapping.image_point_precise(f64::MAX, 0.0), None);
    }

    #[test]
    fn image_rectangles_map_back_to_desktop_pixels() {
        let mapping = CaptureMapping::new(Rect::new(-200, -40, 600, 200), 300, 100).unwrap();
        assert_eq!(mapping.desktop_rect(Rect::new(100, 20, 1, 1)), Some(Rect::new(0, 0, 2, 2)));
        assert_eq!(mapping.desktop_rect(Rect::new(0, 0, 300, 100)), Some(mapping.rect()));
        for invalid in [Rect::new(-1, 0, 1, 1), Rect::new(0, 0, 0, 1), Rect::new(299, 0, 2, 1), Rect::new(i32::MAX, 0, 1, 1)] {
            assert_eq!(mapping.desktop_rect(invalid), None);
        }
        let upsampled = CaptureMapping::new(Rect::new(10, -20, 100, 50), 200, 100).unwrap();
        assert_eq!(upsampled.desktop_rect(Rect::new(21, 21, 2, 2)), Some(Rect::new(20, -10, 2, 2)));
        assert_eq!(upsampled.desktop_rect(Rect::new(198, 98, 2, 2)), Some(Rect::new(109, 29, 1, 1)));
        let fractional = CaptureMapping::with_desktop_bounds(
            Rect::new(1, -1, 4, 5), 4, 4, [0.5, -0.5, 4.5, 3.5],
        ).unwrap();
        assert_eq!(fractional.desktop_rect(Rect::new(0, 0, 1, 1)), Some(Rect::new(1, -1, 1, 2)));
        assert_eq!(fractional.desktop_rect(Rect::new(0, 0, 4, 4)), Some(fractional.rect()));
    }

    #[test]
    fn precise_edges_drive_sampling_and_survive_resizing() {
        let rect = Rect::new(1, -1, 4, 5);
        let bounds = [0.5, -0.5, 4.5, 3.5];
        let mapping = CaptureMapping::with_desktop_bounds(rect, 4, 4, bounds).unwrap();
        assert_eq!(mapping.rect(), rect);
        assert_eq!(mapping.desktop_bounds(), bounds);
        assert_eq!(mapping.pixel_center(0, 0), (1.0, 0.0));
        assert_eq!(mapping.pixel_center(3, 3), (4.0, 3.0));
        assert_eq!(mapping.image_point(1, 0), (0, 0));
        assert_eq!(mapping.image_point(4, 3), (3, 3));
        assert!(mapping.contains_point(1, 0));
        assert!(!mapping.contains_point(0, 0));
        assert!(mapping.intersects_rect(Rect::new(0, 0, 1, 1)));
        assert!(!mapping.intersects_rect(Rect::new(-1, 0, 1, 1)));
        assert_eq!(mapping.resized(2, 2).unwrap().desktop_bounds(), bounds);
        assert_eq!(mapping.resized(2, 2).unwrap().pixel_center(0, 0), (1.5, 0.5));
    }

    #[test]
    fn precise_edges_must_be_finite_and_match_the_integer_rectangle() {
        let rect = Rect::new(1, -1, 4, 5);
        for bounds in [
            [f64::NAN, -0.5, 4.5, 3.5],
            [0.5, -0.5, f64::INFINITY, 3.5],
            [0.5, -0.5, 0.5, 3.5],
            [0.4, -0.5, 4.5, 3.5],
            [0.5, -0.5, 5.5, 3.5],
        ] {
            assert!(CaptureMapping::with_desktop_bounds(rect, 4, 4, bounds).is_none());
        }
        assert!(CaptureMapping::new(Rect::new(i32::MAX, 0, 1, 1), 1, 1).is_none());
        assert!(CaptureMapping::with_desktop_bounds(rect, 0, 4, [0.5, -0.5, 4.5, 3.5]).is_none());
    }
}
