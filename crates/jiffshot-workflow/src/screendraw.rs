//! Screen Draw: while recording, draw over the recorded area with the
//! mouse. A arrow, S box (square), E ellipse, L line, P pen, 1 numbered
//! steps; C changes the color; right-click or the panel's Draw button
//! ends drawing. Marks fade out after a few seconds.
//!
//! The marks live here and are rendered in recording pixels. The app shows
//! them over the selected desktop area, and the recorder blends them into
//! each frame.

use jiffshot_capture::{CaptureMapping, Frame};
use jiffshot_doc::object::{Cap, Kind, ShapeKind, Style};
use jiffshot_doc::{Box2, Document, Pt, Rgba};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tiny_skia::Pixmap;

const COLORS: [Rgba; 5] = [
    Rgba::RED,
    Rgba::rgb(0xFF, 0xC1, 0x07),
    Rgba::rgb(0x1E, 0x88, 0xE5),
    Rgba::rgb(0x43, 0xA0, 0x47),
    Rgba::WHITE,
];

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tool {
    Arrow,
    Box,
    Ellipse,
    Line,
    Pen,
    Step,
}

struct Mark {
    kind: Kind,
    style: Style,
    done: Option<Instant>,
}

pub(crate) struct State {
    pub id: u64,
    pub mapping: CaptureMapping,
    tool: Tool,
    color: usize,
    marks: Vec<Mark>,
    drawing: Option<Pt>,
    step: u32,
    fade_after: Duration,
    /// The marks as last rendered (premultiplied RGBA, the recording's size),
    /// and how many times they were rendered.
    pub image: Option<Arc<Pixmap>>,
    pub generation: u64,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn state() -> MutexGuard<'static, Option<State>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn style(color: Rgba, tool: Tool) -> Style {
    let mut s = Style {
        stroke: color,
        fill: Rgba::TRANSPARENT,
        width: 6.0,
        shadow: true,
        start_cap: Cap::None,
        end_cap: Cap::None,
        ..Style::default()
    };
    match tool {
        Tool::Arrow => s.end_cap = Cap::Arrow,
        Tool::Step => {
            s.fill = color;
            s.text_color = if color == Rgba::WHITE {
                Rgba::BLACK
            } else {
                Rgba::WHITE
            };
            s.font_size = 26.0;
            s.width = 0.0;
        }
        _ => {}
    }
    s
}

/// Whether a mark is still visible, and how much.
fn alpha(mark: &Mark, fade_after: Duration) -> f32 {
    match mark.done {
        None => 1.0,
        Some(_) if fade_after.is_zero() => 1.0,
        Some(t) => {
            let e = t.elapsed();
            if e < fade_after {
                1.0
            } else {
                (1.0 - (e - fade_after).as_secs_f32() / 0.8).max(0.0)
            }
        }
    }
}

fn kind_for(tool: Tool, a: Pt, b: Pt, label: u32) -> Kind {
    match tool {
        Tool::Arrow | Tool::Line => Kind::Line {
            a,
            b,
            control: None,
        },
        Tool::Box => Kind::Shape {
            rect: Box2::from_points(a, b),
            shape: ShapeKind::Rectangle,
        },
        Tool::Ellipse => Kind::Shape {
            rect: Box2::from_points(a, b),
            shape: ShapeKind::Ellipse,
        },
        Tool::Pen => Kind::Pen { points: vec![a, b] },
        Tool::Step => Kind::Step {
            center: b,
            radius: 22.0,
            label: label.to_string(),
            shape: ShapeKind::Ellipse,
        },
    }
}

impl State {
    /// Renders the marks (dropping faded ones) into `image`.
    fn render(&mut self) {
        let (w, h) = self.mapping.size();
        let mut doc = Document::blank(w, h, Rgba::TRANSPARENT);
        let fade = self.fade_after;
        self.marks.retain(|m| alpha(m, fade) > 0.0);
        for m in &self.marks {
            let st = Style {
                opacity: alpha(m, fade),
                ..m.style.clone()
            };
            doc.add(m.kind.clone(), st);
        }
        self.image = (!self.marks.is_empty()).then(|| Arc::new(doc.flatten()));
        self.generation += 1;
    }

    /// The left button went down at `p` (recording pixels).
    pub fn press(&mut self, p: Pt) {
        if self.tool == Tool::Step {
            self.step += 1;
            self.marks.push(Mark {
                kind: kind_for(Tool::Step, p, p, self.step),
                style: style(COLORS[self.color], Tool::Step),
                done: Some(Instant::now()),
            });
        } else {
            self.drawing = Some(p);
            self.marks.push(Mark {
                kind: kind_for(self.tool, p, p, 0),
                style: style(COLORS[self.color], self.tool),
                done: None,
            });
        }
        self.render();
    }

    /// The pointer moved; true when that changed a mark.
    pub fn moved(&mut self, p: Pt) -> bool {
        let (Some(a), Some(last)) = (self.drawing, self.marks.last_mut()) else {
            return false;
        };
        match &mut last.kind {
            Kind::Pen { points } => points.push(p),
            k => *k = kind_for(self.tool, a, p, 0),
        }
        self.render();
        true
    }

    pub fn release(&mut self) {
        self.drawing = None;
        if let Some(m) = self.marks.last_mut() {
            m.done.get_or_insert_with(Instant::now);
        }
        self.render();
    }

    /// A, S, E, L, P and 1 pick a tool, C the next color, 0 restarts the
    /// step numbers.
    pub fn key(&mut self, vk: u16) {
        let tool = match vk as u8 {
            b'A' => Some(Tool::Arrow),
            b'S' => Some(Tool::Box),
            b'E' => Some(Tool::Ellipse),
            b'L' => Some(Tool::Line),
            b'P' => Some(Tool::Pen),
            b'1' => Some(Tool::Step),
            _ => None,
        };
        if let Some(t) = tool {
            self.tool = t;
        } else if vk as u8 == b'C' {
            self.color = (self.color + 1) % COLORS.len();
        } else if vk as u8 == b'0' {
            self.step = 0;
        }
    }

    /// Re-renders while finished marks fade; true when it did.
    pub fn tick(&mut self) -> bool {
        if self.marks.iter().any(|m| m.done.is_some()) && !self.fade_after.is_zero() {
            self.render();
            true
        } else {
            false
        }
    }
}

/// Whether Screen Draw is on.
pub fn active() -> bool {
    state().is_some()
}

pub(crate) fn announcement(id: u64, mapping: CaptureMapping) -> String {
    let area = mapping.rect();
    let (width, height) = mapping.size();
    serde_json::json!({
        "id": id,
        "area": [area.x, area.y, area.width, area.height],
        "pixels": [width, height],
        "bounds": mapping.desktop_bounds(),
    })
    .to_string()
}

/// Starts drawing over the mapped recording area; marks never fade when
/// `fade_after` is zero. Returns false if another drawing session is active.
pub fn show(mapping: CaptureMapping, fade_after: Duration) -> bool {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    {
        let mut s = state();
        if s.is_some() {
            return false;
        }
        *s = Some(State {
            id,
            mapping,
            tool: Tool::Arrow,
            color: 0,
            marks: Vec::new(),
            drawing: None,
            step: 0,
            fade_after,
            image: None,
            generation: 0,
        });
    }
    if let Some(hooks) = crate::hooks() {
        (hooks.screen_draw)(announcement(id, mapping));
    }
    true
}

fn end(session: Option<u64>) -> bool {
    let removed = {
        let mut current = state();
        if current
            .as_ref()
            .is_some_and(|s| session.is_none_or(|id| id == s.id))
        {
            current.take();
            true
        } else {
            false
        }
    };
    if removed {
        if let Some(hooks) = crate::hooks() {
            (hooks.screen_draw)(String::new());
        }
    }
    removed
}

pub fn hide() {
    end(None);
}

pub(crate) fn fail(session: u64, message: &str) -> bool {
    if !end(Some(session)) {
        return false;
    }
    if let Some(hooks) = crate::hooks() {
        (hooks.notify)("Screen Draw failed", message, true);
    } else {
        crate::log(&format!("Screen Draw failed: {message}"));
    }
    true
}

/// Blends the marks into a recorded frame of the area.
pub fn blend_into(frame: &mut Frame) -> Result<(), String> {
    let guard = state();
    let Some(s) = guard.as_ref() else {
        return Ok(());
    };
    #[cfg(target_os = "macos")]
    if s.mapping.size() != (frame.width, frame.height) {
        return Err("Screen Draw dimensions do not match the recording".to_owned());
    }
    let image = s.image.clone();
    drop(guard);
    if let Some(image) = image {
        blend_image(frame, &image);
    }
    Ok(())
}

pub(crate) fn blend_image(frame: &mut Frame, image: &Pixmap) {
    let (w, h) = (
        image.width().min(frame.width) as usize,
        image.height().min(frame.height) as usize,
    );
    let (src, stride) = (image.data(), image.width() as usize * 4);
    let dst_stride = frame.stride();
    for y in 0..h {
        let s = &src[y * stride..y * stride + w * 4];
        let d = &mut frame.pixels[y * dst_stride..y * dst_stride + w * 4];
        for (d, s) in d.chunks_exact_mut(4).zip(s.chunks_exact(4)) {
            let a = s[3] as u32;
            if a == 0 {
                continue;
            }
            // Premultiplied RGBA over BGRA.
            let inv = 255 - a;
            d[0] = (s[2] as u32 + (d[0] as u32 * inv + 127) / 255) as u8;
            d[1] = (s[1] as u32 + (d[1] as u32 * inv + 127) / 255) as u8;
            d[2] = (s[0] as u32 + (d[2] as u32 * inv + 127) / 255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiffshot_capture::Rect;

    #[test]
    fn marks_fade_after_the_delay() {
        let mark = Mark {
            kind: kind_for(Tool::Box, Pt::new(0.0, 0.0), Pt::new(10.0, 10.0), 0),
            style: style(Rgba::RED, Tool::Box),
            done: Some(Instant::now() - Duration::from_millis(3400)),
        };
        let a = alpha(&mark, Duration::from_secs(3));
        assert!(a > 0.0 && a < 1.0, "fading: {a}");
        assert_eq!(alpha(&mark, Duration::ZERO), 1.0, "never fades when zero");
        let drawing = Mark { done: None, ..mark };
        assert_eq!(alpha(&drawing, Duration::from_secs(3)), 1.0);
    }

    #[test]
    fn a_drawn_box_ends_up_in_the_recorded_frame() {
        struct End;
        impl Drop for End {
            fn drop(&mut self) {
                hide();
            }
        }
        let _end = End;
        assert!(show(
            CaptureMapping::identity(Rect::new(0, 0, 120, 80)).unwrap(),
            Duration::ZERO
        ));
        let id = state().as_ref().unwrap().id;
        if let Some(s) = state().as_mut() {
            s.key(b'S' as u16);
            s.press(Pt::new(20.0, 20.0));
            assert!(s.moved(Pt::new(100.0, 60.0)));
            s.release();
        }
        let mut frame = Frame::new(120, 80);
        frame.pixels.iter_mut().for_each(|b| *b = 0xFF);
        blend_into(&mut frame).unwrap();
        let mut preview = vec![0; 120 * 80 * 4];
        assert_eq!(
            crate::ui_api::draw_copy(id, &mut preview, 120, 80),
            Ok(true)
        );
        assert!(crate::ui_api::draw_copy(id, &mut preview, 119, 80).is_err());
        assert!(crate::ui_api::draw_copy(id, &mut preview[..100], 120, 80).is_err());
        assert_eq!(
            crate::ui_api::draw_copy(id + 1, &mut preview, 120, 80),
            Ok(false)
        );
        assert!(!crate::ui_api::draw_failed(id + 1, "stale failure".into()));
        assert!(crate::ui_api::draw_pending(id));
        #[cfg(target_os = "macos")]
        assert!(blend_into(&mut Frame::new(119, 80)).is_err());
        hide();
        assert!(!active());
        // On the box's top edge: red (BGRA), not white.
        let at = (20 * 120 + 60) * 4;
        let px = &frame.pixels[at..at + 3];
        assert!(px[2] > 200 && px[0] < 80 && px[1] < 80, "{px:?}");
        // Inside the box: untouched.
        let at = (40 * 120 + 60) * 4;
        assert_eq!(&frame.pixels[at..at + 3], &[0xFF, 0xFF, 0xFF]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn screen_draw_uses_recording_pixels_without_resampling() {
        let mapping = CaptureMapping::new(Rect::new(-200, 30, 240, 160), 120, 80).unwrap();
        let mut draw = State {
            id: 1,
            mapping,
            tool: Tool::Box,
            color: 0,
            marks: Vec::new(),
            drawing: None,
            step: 0,
            fade_after: Duration::ZERO,
            image: None,
            generation: 0,
        };
        draw.press(Pt::new(20.0, 20.0));
        assert!(draw.moved(Pt::new(100.0, 60.0)));
        draw.release();
        let image = draw.image.unwrap();
        assert_eq!((image.width(), image.height()), (120, 80));
        let mut frame = Frame::new(120, 80);
        frame.pixels.fill(255);
        blend_image(&mut frame, &image);
        assert!(frame
            .rgb(60, 20)
            .is_some_and(|(r, g, b)| r > 200 && g < 80 && b < 80));
        assert_eq!(frame.rgb(60, 40), Some((255, 255, 255)));

        let mut image = Pixmap::new(2, 2).unwrap();
        let pixel = (1 * 2 + 1) * 4;
        image.data_mut()[pixel..pixel + 4].copy_from_slice(&[128, 0, 0, 128]);
        let mut frame = Frame::new(2, 2);
        for pixel in frame.pixels.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[100, 50, 25, 255]);
        }
        blend_image(&mut frame, &image);
        assert_eq!(&frame.pixels[..4], &[100, 50, 25, 255]);
        assert_eq!(&frame.pixels[12..16], &[50, 25, 140, 255]);
    }
}
