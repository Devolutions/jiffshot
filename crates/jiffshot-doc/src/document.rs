//! A document: a background raster plus annotation objects drawn over it.
//! Cloning is cheap (the background is shared), which is what the undo
//! history relies on.

use crate::color::Rgba;
use crate::geom::{Box2, IRect, Pt};
use crate::object::{Kind, Object, Style};
use crate::raster;
use std::sync::Arc;
use tiny_skia::Pixmap;

#[derive(Clone, Debug)]
pub struct Document {
    width: u32,
    height: u32,
    background: Arc<Pixmap>,
    pub objects: Vec<Object>,
    next_id: u64,
}

impl Document {
    pub fn new(background: Pixmap) -> Document {
        Document {
            width: background.width(),
            height: background.height(),
            background: Arc::new(background),
            objects: Vec::new(),
            next_id: 1,
        }
    }

    pub fn blank(width: u32, height: u32, color: Rgba) -> Document {
        Document::new(raster::solid(width, height, color).expect("non-zero size"))
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn bounds(&self) -> IRect {
        IRect::of_size(self.width, self.height)
    }

    pub fn background(&self) -> &Pixmap {
        &self.background
    }

    pub fn background_arc(&self) -> Arc<Pixmap> {
        self.background.clone()
    }

    /// The background for modification (copied first if shared with the
    /// history).
    pub fn background_mut(&mut self) -> &mut Pixmap {
        Arc::make_mut(&mut self.background)
    }

    /// Replaces the background; the canvas takes its size. Objects keep
    /// their positions.
    pub fn set_background(&mut self, background: Pixmap) {
        self.width = background.width();
        self.height = background.height();
        self.background = Arc::new(background);
    }

    pub fn allocate_id(&mut self) -> u64 {
        let start = self.next_id.max(1);
        let mut id = start;
        loop {
            let mut used = false;
            for object in &self.objects {
                object.walk(&mut |nested| used |= nested.id == id);
            }
            if !used {
                self.next_id = id.wrapping_add(1).max(1);
                return id;
            }
            id = id.wrapping_add(1).max(1);
            assert_ne!(id, start, "document object id space exhausted");
        }
    }

    pub fn add(&mut self, kind: Kind, style: Style) -> u64 {
        let id = self.allocate_id();
        self.objects.push(Object::new(id, kind, style));
        id
    }

    /// Families substituted while rendering text, including text in groups.
    #[cfg(target_os = "macos")]
    pub fn font_substitutions(&self) -> Vec<crate::text::FontSubstitution> {
        crate::text::object_font_substitutions(&self.objects)
    }

    /// Adds an existing object under a fresh id.
    pub fn insert(&mut self, mut object: Object) -> u64 {
        fn assign_ids(document: &mut Document, object: &mut Object) {
            object.id = document.allocate_id();
            if let Kind::Group { objects } = &mut object.kind {
                for child in objects {
                    assign_ids(document, child);
                }
            }
        }
        assign_ids(self, &mut object);
        let id = object.id;
        self.objects.push(object);
        id
    }

    pub fn index_of(&self, id: u64) -> Option<usize> {
        self.objects.iter().position(|o| o.id == id)
    }

    pub fn get(&self, id: u64) -> Option<&Object> {
        self.objects.iter().find(|o| o.id == id)
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut Object> {
        self.objects.iter_mut().find(|o| o.id == id)
    }

    pub fn remove(&mut self, id: u64) -> Option<Object> {
        let i = self.index_of(id)?;
        Some(self.objects.remove(i))
    }

    /// Topmost object under `p`.
    pub fn hit(&self, p: Pt, tolerance: f32) -> Option<u64> {
        self.objects
            .iter()
            .rev()
            .find(|o| o.hit(p, tolerance))
            .map(|o| o.id)
    }

    /// Restores ids after loading, so new objects never reuse one.
    pub fn renumber_from_objects(&mut self) {
        let mut max_id = 0;
        for object in &self.objects {
            object.walk(&mut |nested| max_id = max_id.max(nested.id));
        }
        self.next_id = max_id.wrapping_add(1).max(1);
    }

    /// The label for the next step in `format` ("Numbers", "Letters",
    /// "Lowercase" or "Roman"), continuing from the steps of that format.
    pub fn next_step_label_as(&self, format: &str) -> String {
        let labels = self.step_labels();
        match format {
            "Letters" | "Lowercase" => {
                let base = if format == "Letters" { b'A' } else { b'a' };
                let mut next = labels
                    .iter()
                    .filter(|label| {
                        !label.is_empty()
                            && label
                                .bytes()
                                .all(|byte| (base..=base + 25).contains(&byte))
                    })
                    .max_by(|left, right| {
                        left.len()
                            .cmp(&right.len())
                            .then_with(|| left.cmp(right))
                    })
                    .map(|label| label.as_bytes().to_vec())
                    .unwrap_or_default();
                let mut carry = !next.is_empty();
                for digit in next.iter_mut().rev() {
                    if *digit < base + 25 {
                        *digit += 1;
                        carry = false;
                        break;
                    }
                    *digit = base;
                }
                if carry {
                    next.insert(0, base);
                }
                if next.is_empty() {
                    next.push(base);
                }
                String::from_utf8(next).expect("ASCII letters are UTF-8")
            }
            "Roman" => {
                let (thousands, remainder) = labels
                    .iter()
                    .filter_map(|label| from_roman(label))
                    .max()
                    .unwrap_or((0, 0));
                if remainder == 999 {
                    "m".repeat(thousands + 1)
                } else {
                    format!("{}{}", "m".repeat(thousands), to_roman(remainder + 1))
                }
            }
            _ => Self::next_number_label(labels.into_iter()),
        }
    }

    fn next_step_label_from(labels: &[&str]) -> String {
        if let Some(last) = labels.last() {
            let mut chars = last.chars();
            if let (Some(c), None) = (chars.next(), chars.next()) {
                if c.is_ascii_alphabetic() && c != 'z' && c != 'Z' {
                    return char::from(c as u8 + 1).to_string();
                }
            }
        }
        Self::next_number_label(labels.iter().copied())
    }

    /// The label for the next step: one past the highest number used, or
    /// the next letter when the last step used letters.
    pub fn next_step_label(&self) -> String {
        Self::next_step_label_from(&self.step_labels())
    }

    pub(crate) fn next_step_labels(&self, count: usize) -> Vec<String> {
        let mut labels: Vec<String> = self.step_labels().into_iter().map(str::to_owned).collect();
        let mut next = Vec::with_capacity(count);
        for _ in 0..count {
            let current: Vec<&str> = labels.iter().map(String::as_str).collect();
            let label = Self::next_step_label_from(&current);
            labels.push(label.clone());
            next.push(label);
        }
        next
    }

    fn step_labels(&self) -> Vec<&str> {
        fn collect<'a>(objects: &'a [Object], labels: &mut Vec<&'a str>) {
            for object in objects {
                match &object.kind {
                    Kind::Step { label, .. } => labels.push(label),
                    Kind::Group { objects } => collect(objects, labels),
                    _ => {}
                }
            }
        }
        let mut labels = Vec::new();
        collect(&self.objects, &mut labels);
        labels
    }

    fn next_number_label<'a>(labels: impl Iterator<Item = &'a str>) -> String {
        let largest = labels
            .filter_map(|label| {
                let digits = label.strip_prefix('+').unwrap_or(label);
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                let trimmed = digits.trim_start_matches('0');
                Some(if trimmed.is_empty() { "0" } else { trimmed })
            })
            .max_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)))
            .unwrap_or("0");
        let mut digits = largest.as_bytes().to_vec();
        for digit in digits.iter_mut().rev() {
            if *digit != b'9' {
                *digit += 1;
                return String::from_utf8(digits).expect("decimal digits are UTF-8");
            }
            *digit = b'0';
        }
        digits.insert(0, b'1');
        String::from_utf8(digits).expect("decimal digits are UTF-8")
    }

    /// Grows `area` until it includes every pixel that objects touching it
    /// read, so a partial render matches a full one.
    fn closure(&self, area: Box2) -> Box2 {
        let mut area = area;
        loop {
            let mut grown = area;
            for o in &self.objects {
                if let Some(r) = o.reads() {
                    if o.bounds().intersects(&grown) {
                        grown = grown.union(&r).union(&o.bounds());
                    }
                }
            }
            if grown == area {
                return area;
            }
            area = grown;
        }
    }

    /// Renders `area` (grown as needed, clipped to the canvas) and returns
    /// the pixels with the document area they cover.
    pub fn render_area(&self, area: IRect, checker: bool) -> Option<(IRect, Pixmap)> {
        let area = self
            .closure(area.to_box())
            .to_irect()
            .intersect(&self.bounds())?;
        // Paths clipped at the edge of the target antialias slightly
        // differently, so draw with a margin and keep only the inside.
        let padded = area.inflate(2).intersect(&self.bounds())?;
        let mut scratch = raster::extract(&self.background, padded)?;
        let region = padded.to_box();
        for o in &self.objects {
            if o.bounds().intersects(&region) {
                o.draw(&mut scratch, (padded.x, padded.y));
            }
        }
        let mut scratch = if padded == area {
            scratch
        } else {
            raster::extract(
                &scratch,
                IRect::new(area.x - padded.x, area.y - padded.y, area.w, area.h),
            )?
        };
        if checker {
            let whole = IRect::of_size(scratch.width(), scratch.height());
            raster::checkerboard_under(&mut scratch, whole, (area.x, area.y));
        }
        Some((area, scratch))
    }

    /// Re-renders `area` of `display` (a canvas-sized pixmap); returns the
    /// area actually updated.
    pub fn render_into(&self, display: &mut Pixmap, area: IRect, checker: bool) -> Option<IRect> {
        let (area, pixels) = self.render_area(area, checker)?;
        raster::copy_area(
            &pixels,
            IRect::of_size(pixels.width(), pixels.height()),
            display,
            area.x,
            area.y,
        );
        Some(area)
    }

    /// The finished image with real transparency, for export.
    pub fn flatten(&self) -> Pixmap {
        self.render_area(self.bounds(), false)
            .map(|(_, p)| p)
            .unwrap_or_else(|| (*self.background).clone())
    }

    pub fn translate_objects(&mut self, dx: f32, dy: f32) {
        for o in &mut self.objects {
            o.translate(dx, dy);
        }
    }

    /// Crops the canvas to `r` (clipped to it). Objects keep their place on
    /// the image.
    pub fn crop(&mut self, r: IRect) -> bool {
        let Some(r) = r.intersect(&self.bounds()) else {
            return false;
        };
        if r == self.bounds() {
            return false;
        }
        let Some(bg) = raster::extract(&self.background, r) else {
            return false;
        };
        self.set_background(bg);
        self.translate_objects(-r.x as f32, -r.y as f32);
        true
    }

    /// Changes the canvas size; the old image lands at (`dx`, `dy`) and the
    /// new area is filled with `fill`.
    pub fn resize_canvas(&mut self, width: u32, height: u32, dx: i32, dy: i32, fill: Rgba) {
        let Some(mut bg) = raster::solid(width, height, fill) else {
            return;
        };
        bg.draw_pixmap(
            dx,
            dy,
            self.background.as_ref().as_ref(),
            &tiny_skia::PixmapPaint::default(),
            tiny_skia::Transform::identity(),
            None,
        );
        self.set_background(bg);
        self.translate_objects(dx as f32, dy as f32);
    }

    /// Resamples the image to `width` x `height`, scaling objects with it.
    pub fn scale(&mut self, width: u32, height: u32) {
        let Some(bg) = raster::resample(&self.background, width, height) else {
            return;
        };
        let from = self.bounds().to_box();
        let to = IRect::of_size(width, height).to_box();
        for o in &mut self.objects {
            o.map_geometry(&from, &to);
        }
        self.set_background(bg);
    }

    /// Quarter turn, clockwise or not.
    pub fn rotate(&mut self, clockwise: bool) {
        fn rotate_object(object: &mut Object, map: &dyn Fn(Pt) -> Pt, angle: f32) {
            if let Kind::Group { objects } = &mut object.kind {
                for child in objects {
                    rotate_object(child, map, angle);
                }
            } else if object.rotatable() {
                let center = object.pivot();
                let rotated = map(center);
                object.translate(rotated.x - center.x, rotated.y - center.y);
                object.rotation += angle;
            } else {
                object.map_points(map, 1.0);
            }
        }

        let (w, h) = (self.width as usize, self.height as usize);
        let Some(mut out) = Pixmap::new(self.height, self.width) else {
            return;
        };
        {
            let src = self.background.pixels();
            let dst = out.pixels_mut();
            for y in 0..h {
                for x in 0..w {
                    let (nx, ny) = if clockwise {
                        (h - 1 - y, x)
                    } else {
                        (y, w - 1 - x)
                    };
                    dst[ny * h + nx] = src[y * w + x];
                }
            }
        }
        let (wf, hf) = (w as f32, h as f32);
        let map = |p: Pt| {
            if clockwise {
                Pt::new(hf - p.y, p.x)
            } else {
                Pt::new(p.y, wf - p.x)
            }
        };
        let angle = if clockwise { 90.0 } else { -90.0 };
        for o in &mut self.objects {
            rotate_object(o, &map, angle);
        }
        self.set_background(out);
    }

    pub fn flip(&mut self, horizontal: bool) {
        let (w, h) = (self.width as usize, self.height as usize);
        let pixels = self.background_mut().pixels_mut();
        if horizontal {
            for row in pixels.chunks_exact_mut(w) {
                row.reverse();
            }
        } else {
            for y in 0..h / 2 {
                let (top, bottom) = pixels.split_at_mut((h - 1 - y) * w);
                top[y * w..y * w + w].swap_with_slice(&mut bottom[..w]);
            }
        }
        let (wf, hf) = (w as f32, h as f32);
        for o in &mut self.objects {
            if horizontal {
                o.map_points(&|p| Pt::new(wf - p.x, p.y), 1.0);
            } else {
                o.map_points(&|p| Pt::new(p.x, hf - p.y), 1.0);
            }
        }
    }

    /// Draws every object into the background and removes them.
    pub fn flatten_objects(&mut self) {
        let flat = self.flatten();
        self.objects.clear();
        self.set_background(flat);
    }
}

const ROMAN: &[(u32, &str)] = &[
    (1000, "m"),
    (900, "cm"),
    (500, "d"),
    (400, "cd"),
    (100, "c"),
    (90, "xc"),
    (50, "l"),
    (40, "xl"),
    (10, "x"),
    (9, "ix"),
    (5, "v"),
    (4, "iv"),
    (1, "i"),
];

fn to_roman(mut n: u32) -> String {
    let mut out = String::new();
    for &(v, s) in ROMAN {
        while n >= v {
            out.push_str(s);
            n -= v;
        }
    }
    out
}

fn from_roman(s: &str) -> Option<(usize, u32)> {
    if s.is_empty() {
        return None;
    }
    let thousands = s.bytes().take_while(|b| *b == b'm').count();
    let suffix = &s[thousands..];
    let mut rest = suffix;
    let mut value = 0_u32;
    for &(number, numeral) in ROMAN.iter().filter(|(number, _)| *number < 1000) {
        while let Some(next) = rest.strip_prefix(numeral) {
            value = value.checked_add(number)?;
            rest = next;
        }
    }
    (rest.is_empty() && value < 1000 && to_roman(value) == suffix)
        .then_some((thousands, value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{BlurMode, ShapeKind};
    use std::collections::HashSet;

    fn doc() -> Document {
        let mut d = Document::blank(200, 100, Rgba::WHITE);
        for x in 0..200 {
            for y in 0..100 {
                if (x / 10 + y / 10) % 2 == 0 {
                    raster::set_pixel(d.background_mut(), x, y, Rgba::BLACK);
                }
            }
        }
        d
    }

    #[test]
    fn allocating_after_maximum_object_id_does_not_wrap_or_duplicate() {
        let mut d = Document::blank(2, 2, Rgba::WHITE);
        d.objects.push(Object::new(
            u64::MAX,
            Kind::Text {
                rect: Box2::default(),
                text: String::new(),
            },
            Style::default(),
        ));
        d.renumber_from_objects();
        let id = d.add(
            Kind::Text {
                rect: Box2::default(),
                text: String::new(),
            },
            Style::default(),
        );
        assert_eq!(id, 1);
        assert_eq!(d.objects.iter().filter(|object| object.id == id).count(), 1);
    }

    #[test]
    fn allocating_after_loading_a_group_skips_nested_object_ids() {
        let mut d = Document::blank(2, 2, Rgba::WHITE);
        d.objects.push(Object::new(
            1,
            Kind::Group {
                objects: vec![Object::new(
                    2,
                    Kind::Text {
                        rect: Box2::default(),
                        text: String::new(),
                    },
                    Style::default(),
                )],
            },
            Style::default(),
        ));
        d.renumber_from_objects();
        let id = d.add(
            Kind::Text {
                rect: Box2::default(),
                text: String::new(),
            },
            Style::default(),
        );
        assert_eq!(id, 3);
    }

    #[test]
    fn inserting_a_duplicate_group_assigns_fresh_nested_ids() {
        let mut d = Document::blank(2, 2, Rgba::WHITE);
        let group = Kind::Group {
            objects: vec![
                Object::new(
                    2,
                    Kind::Text {
                        rect: Box2::default(),
                        text: "first".into(),
                    },
                    Style::default(),
                ),
                Object::new(
                    3,
                    Kind::Group {
                        objects: vec![Object::new(
                            4,
                            Kind::Text {
                                rect: Box2::default(),
                                text: "nested".into(),
                            },
                            Style::default(),
                        )],
                    },
                    Style::default(),
                ),
            ],
        };
        let id = d.add(group, Style::default());
        let duplicate = d.get(id).unwrap().clone();
        d.insert(duplicate);

        let mut ids = HashSet::new();
        let mut duplicate_found = false;
        for object in &d.objects {
            object.walk(&mut |nested| duplicate_found |= !ids.insert(nested.id));
        }
        assert!(!duplicate_found, "every object in every group must have a unique id");
    }

    #[test]
    fn scaling_to_zero_preserves_the_document() {
        let mut d = Document::blank(2, 2, Rgba::WHITE);
        d.add(
            Kind::Text {
                rect: Box2::new(0.0, 0.0, 2.0, 2.0),
                text: "keep".into(),
            },
            Style::default(),
        );
        let before = d.clone();
        d.scale(0, 2);
        assert_eq!(d.size(), before.size());
        assert_eq!(d.objects, before.objects);
        assert_eq!(d.background().data(), before.background().data());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn font_substitutions_follow_nested_text_and_step_rendering_without_changing_the_document() {
        let missing = "Jiffshot deliberately unavailable group face a4e290";
        let mut d = Document::blank(240, 140, Rgba::WHITE);
        let rect = Box2::new(5.0, 5.0, 115.0, 45.0);
        let italic = Style { font: missing.to_owned(), italic: true, ..Style::default() };
        d.add(Kind::Shape { rect, shape: ShapeKind::Rectangle }, italic.clone());
        d.add(Kind::Text { rect, text: String::new() }, italic.clone());
        let text = Object::new(101, Kind::Text { rect, text: "Text".into() }, italic.clone());
        let callout = Object::new(
            102,
            Kind::Callout { rect, tail: Pt::new(100.0, 65.0), text: "Callout".into(), shape: ShapeKind::Rectangle, arrow: false, more_tails: vec![] },
            italic.clone(),
        );
        let step = Object::new(
            103,
            Kind::Step { center: Pt::new(150.0, 70.0), radius: 25.0, label: "1".into(), shape: ShapeKind::Ellipse },
            italic.clone(),
        );
        let nested = Object::new(104, Kind::Group { objects: vec![callout, step] }, Style::default());
        d.add(Kind::Group { objects: vec![text, nested] }, Style::default());
        d.add(
            Kind::Text { rect, text: "Available".into() },
            Style { font: "Helvetica Neue".into(), ..Style::default() },
        );

        let original = d.objects.clone();
        let substitutions = d.font_substitutions();
        assert_eq!(substitutions.len(), 2);
        assert_eq!(substitutions[0], crate::text::font_substitution(missing, false, true).unwrap());
        assert_eq!(substitutions[1], crate::text::font_substitution(missing, true, false).unwrap());
        assert!(substitutions.iter().all(|font| font.reason == crate::text::FontSubstitutionReason::Family));
        assert_eq!(d.objects, original);

        let mut explicit = d.clone();
        for object in &mut explicit.objects {
            object.walk_mut(&mut |object| {
                if object.style.font == missing {
                    object.style.font = "Helvetica Neue".into();
                }
            });
        }
        assert_eq!(d.flatten().data(), explicit.flatten().data());
        let bytes = crate::io::document_bytes(&d).unwrap();
        let loaded = crate::io::document_from_bytes(bytes).unwrap();
        assert_eq!(loaded.objects, original);
        assert_eq!(loaded.font_substitutions(), substitutions);
    }

    #[test]
    fn partial_render_matches_full_with_reading_objects() {
        let mut d = doc();
        d.add(
            Kind::Shape {
                rect: Box2::new(20.0, 20.0, 120.0, 80.0),
                shape: ShapeKind::Ellipse,
            },
            Style::default(),
        );
        d.add(
            Kind::Blur {
                rect: Box2::new(50.0, 10.0, 150.0, 90.0),
                mode: BlurMode::Blur,
                amount: 6.0,
            },
            Style::default(),
        );
        d.add(
            Kind::Magnify {
                rect: Box2::new(100.0, 30.0, 160.0, 90.0),
                zoom: 2.0,
                ellipse: true,
                offset: Pt::default(),
            },
            Style::default(),
        );
        let full = d.flatten();
        let mut display = raster::solid(200, 100, Rgba::RED).unwrap();
        d.render_into(&mut display, IRect::new(0, 0, 200, 100), false);
        let mut partial = full.clone();
        raster::copy_area(
            &raster::solid(200, 100, Rgba::RED).unwrap(),
            IRect::new(60, 40, 20, 20),
            &mut partial,
            60,
            40,
        );
        let updated = d
            .render_into(&mut partial, IRect::new(60, 40, 20, 20), false)
            .unwrap();
        assert!(updated.w >= 100, "grown to the blur box: {updated:?}");
        // tiny-skia antialiases a few edge pixels differently depending on
        // the target size; a missing dependency differs far more widely.
        let channels =
            |p: &tiny_skia::PremultipliedColorU8| [p.red(), p.green(), p.blue(), p.alpha()];
        let diff = |a: &Pixmap, b: &Pixmap| {
            a.pixels()
                .iter()
                .zip(b.pixels())
                .map(|(x, y)| {
                    channels(x)
                        .iter()
                        .zip(channels(y))
                        .map(|(p, q)| p.abs_diff(q))
                        .max()
                        .unwrap()
                })
                .filter(|d| *d > 2)
                .collect::<Vec<u8>>()
        };
        let d1 = diff(&partial, &full);
        assert!(
            d1.len() <= 12 && d1.iter().all(|d| *d < 48),
            "partial render differs: {d1:?}"
        );
        assert!(diff(&display, &full).is_empty());
    }

    #[test]
    fn crop_rotate_flip_keep_objects_on_the_image() {
        let mut d = doc();
        let id = d.add(
            Kind::Shape {
                rect: Box2::new(20.0, 10.0, 40.0, 30.0),
                shape: ShapeKind::Rectangle,
            },
            Style::default(),
        );
        assert!(d.crop(IRect::new(10, 5, 100, 50)));
        assert_eq!(d.size(), (100, 50));
        assert_eq!(
            d.get(id).unwrap().rect(),
            Some(Box2::new(10.0, 5.0, 30.0, 25.0))
        );
        d.rotate(true);
        assert_eq!(d.size(), (50, 100));
        assert_eq!(
            d.get(id).unwrap().rect(),
            Some(Box2::new(25.0, 10.0, 45.0, 30.0))
        );
        d.flip(true);
        assert_eq!(
            d.get(id).unwrap().rect(),
            Some(Box2::new(5.0, 10.0, 25.0, 30.0))
        );
        d.rotate(false);
        assert_eq!(d.size(), (100, 50));
    }

    #[test]
    fn quarter_turn_updates_rotated_objects_and_group_children() {
        let mut d = doc();
        let object = Object::new(
            1,
            Kind::Shape {
                rect: Box2::new(1.0, 1.0, 6.0, 4.0),
                shape: crate::object::ShapeKind::Rectangle,
            },
            Style::default(),
        );
        let mut group = Object::new(
            2,
            Kind::Group {
                objects: vec![object],
            },
            Style::default(),
        );
        if let Kind::Group { objects } = &mut group.kind {
            objects[0].rotation = 35.0;
        }
        d.insert(group);
        d.rotate(true);
        let Kind::Group { objects } = &d.objects[0].kind else {
            panic!("group should remain a group");
        };
        assert_eq!(objects[0].rotation, 125.0);
        assert_eq!(objects[0].rect(), Some(Box2::new(95.0, 2.0, 100.0, 5.0)));
        d.rotate(false);
        let Kind::Group { objects } = &d.objects[0].kind else {
            panic!("group should remain a group");
        };
        assert_eq!(objects[0].rotation, 35.0);
    }

    #[test]
    fn quarter_turn_preserves_rectangular_annotation_orientation() {
        for (clockwise, inside, outside) in [
            (true, (35, 35), (45, 25)),
            (false, (25, 65), (15, 55)),
        ] {
            let mut d = Document::blank(80, 60, Rgba::WHITE);
            d.add(
                Kind::Shape {
                    rect: Box2::new(10.0, 20.0, 40.0, 30.0),
                    shape: ShapeKind::Rectangle,
                },
                Style {
                    fill: Rgba::RED,
                    width: 0.0,
                    ..Style::default()
                },
            );
            d.rotate(clockwise);
            let image = d.flatten();
            assert_eq!(raster::pixel(&image, inside.0, inside.1), Rgba::RED);
            assert_eq!(raster::pixel(&image, outside.0, outside.1), Rgba::WHITE);
        }
    }

    #[test]
    fn quarter_turn_of_group_matches_rotated_render() {
        for clockwise in [true, false] {
            let mut d = Document::blank(80, 60, Rgba::WHITE);
            let mut child = Object::new(
                1,
                Kind::Shape {
                    rect: Box2::new(15.0, 15.0, 45.0, 35.0),
                    shape: ShapeKind::Triangle,
                },
                Style {
                    fill: Rgba::RED,
                    width: 0.0,
                    ..Style::default()
                },
            );
            child.rotation = 31.0;
            d.add(
                Kind::Group {
                    objects: vec![child],
                },
                Style::default(),
            );
            let before = d.flatten();
            d.rotate(clockwise);
            let after = d.flatten();
            let mut filled = 0;
            let mut mismatches = 0;
            for y in 0..60 {
                for x in 0..80 {
                    let (nx, ny) = if clockwise { (59 - y, x) } else { (y, 79 - x) };
                    let expected = raster::pixel(&before, x, y).g < 128;
                    let actual = raster::pixel(&after, nx, ny).g < 128;
                    filled += usize::from(expected);
                    mismatches += usize::from(expected != actual);
                }
            }
            assert!(filled > 100);
            assert!(
                mismatches <= filled / 10,
                "rotated render differed at {mismatches} of {filled} filled pixels"
            );
        }
    }

    #[test]
    fn step_labels_continue() {
        let mut d = doc();
        assert_eq!(d.next_step_label(), "1");
        let step = |label: &str| Kind::Step {
            center: Pt::new(5.0, 5.0),
            radius: 10.0,
            label: label.into(),
            shape: crate::object::ShapeKind::Ellipse,
        };
        d.add(step("7"), Style::default());
        assert_eq!(d.next_step_label(), "8");
        d.add(step("B"), Style::default());
        assert_eq!(d.next_step_label(), "C");
        assert_eq!(d.next_step_label_as("Letters"), "C");
        assert_eq!(d.next_step_label_as("Lowercase"), "a");
        d.add(step("iii"), Style::default());
        assert_eq!(d.next_step_label_as("Roman"), "iv");
        assert_eq!(d.next_step_label_as("Numbers"), "8");
    }

    #[test]
    fn letter_labels_continue_past_single_letters() {
        let mut d = doc();
        let step = |label: &str| Kind::Step {
            center: Pt::new(5.0, 5.0),
            radius: 10.0,
            label: label.into(),
            shape: crate::object::ShapeKind::Ellipse,
        };
        d.add(step("Z"), Style::default());
        assert_eq!(d.next_step_label_as("Letters"), "AA");
        d.add(step("AZ"), Style::default());
        assert_eq!(d.next_step_label_as("Letters"), "BA");
        d.add(step("ZZ"), Style::default());
        assert_eq!(d.next_step_label_as("Letters"), "AAA");
        d.add(step("z"), Style::default());
        assert_eq!(d.next_step_label_as("Lowercase"), "aa");
    }

    #[test]
    fn grouped_step_labels_continue_including_nested_groups() {
        let step = |label: &str| Object::new(
            1,
            Kind::Step {
                center: Pt::new(5.0, 5.0),
                radius: 2.0,
                label: label.into(),
                shape: ShapeKind::Ellipse,
            },
            Style::default(),
        );
        let nested = Object::new(
            2,
            Kind::Group { objects: vec![step("7")] },
            Style::default(),
        );
        let mut d = Document::blank(10, 10, Rgba::WHITE);
        d.add(Kind::Group { objects: vec![nested] }, Style::default());
        assert_eq!(d.next_step_label_as("Numbers"), "8");
        assert_eq!(d.next_step_label(), "8");
    }

    #[test]
    fn scaling_a_group_scales_nested_styles_like_ungrouped_objects() {
        let kind = Kind::Step {
            center: Pt::new(5.0, 5.0),
            radius: 2.0,
            label: "1".into(),
            shape: ShapeKind::Ellipse,
        };
        let style = Style { width: 4.0, font_size: 12.0, ..Style::default() };
        let grouped_step = Object::new(1, kind.clone(), style.clone());
        let nested = Object::new(
            2,
            Kind::Group { objects: vec![grouped_step] },
            Style::default(),
        );
        let mut d = Document::blank(10, 10, Rgba::WHITE);
        let individual = d.add(kind, style);
        let group = d.add(Kind::Group { objects: vec![nested] }, Style::default());
        d.scale(20, 20);
        let Kind::Group { objects } = &d.get(group).unwrap().kind else { panic!("expected group") };
        let Kind::Group { objects } = &objects[0].kind else { panic!("expected nested group") };
        let grouped = &objects[0];
        let individual = d.get(individual).unwrap();
        assert_eq!(grouped.style.width, individual.style.width);
        assert_eq!(grouped.style.font_size, individual.style.font_size);
        assert_eq!(grouped.style.width, 8.0);
        assert_eq!(grouped.style.font_size, 24.0);
        assert_eq!(grouped.rect(), individual.rect());
    }

    #[test]
    fn resizing_a_group_scales_child_stroke_and_font_sizes() {
        let child = Object::new(
            1,
            Kind::Step {
                center: Pt::new(5.0, 5.0),
                radius: 2.0,
                label: "1".into(),
                shape: ShapeKind::Ellipse,
            },
            Style { width: 4.0, font_size: 12.0, ..Style::default() },
        );
        let mut group = Object::new(
            2,
            Kind::Group { objects: vec![child] },
            Style::default(),
        );
        group.move_handle(crate::object::H_BR, Pt::new(11.0, 11.0), false);
        let Kind::Group { objects } = &group.kind else { panic!("expected group") };
        assert_eq!(objects[0].style.width, 8.0);
        assert_eq!(objects[0].style.font_size, 24.0);
    }

    #[test]
    fn numeric_step_labels_continue_past_integer_limits() {
        let mut d = Document::blank(10, 10, Rgba::WHITE);
        d.add(
            Kind::Step {
                center: Pt::new(5.0, 5.0),
                radius: 2.0,
                label: "18446744073709551615".into(),
                shape: ShapeKind::Ellipse,
            },
            Style::default(),
        );
        assert_eq!(d.next_step_label(), "18446744073709551616");
        assert_eq!(d.next_step_label_as("Numbers"), "18446744073709551616");
        assert_eq!(
            Document::next_number_label(["+0004294967295", "99"].into_iter()),
            "4294967296"
        );
        assert_eq!(
            Document::next_number_label(["999999999999999999999999999999"].into_iter()),
            "1000000000000000000000000000000"
        );
    }

    #[test]
    fn roman_step_labels_continue_after_three_thousand_nine_hundred_ninety_nine() {
        let mut d = Document::blank(10, 10, Rgba::WHITE);
        let step = |label: &str| Kind::Step {
            center: Pt::new(5.0, 5.0),
            radius: 2.0,
            label: label.into(),
            shape: ShapeKind::Ellipse,
        };
        d.add(step("mmmcmxcix"), Style::default());
        assert_eq!(d.next_step_label_as("Roman"), "mmmm");
        d.add(step("mmmm"), Style::default());
        assert_eq!(d.next_step_label_as("Roman"), "mmmmi");
        assert_eq!(from_roman("iiii"), None);
        assert_eq!(from_roman(""), None);
    }
}
