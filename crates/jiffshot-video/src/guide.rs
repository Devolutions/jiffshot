//! Editable step-guide layout shared by the capture workflows.

use jiffshot_capture::{CaptureMapping, Frame, Rect};
use jiffshot_doc::object::{Kind, ShapeKind, Style};
use jiffshot_doc::stamps::Stamp;
use jiffshot_doc::text::Align;
use jiffshot_doc::{Box2, Document, Pixmap, Pt, Rgba};
use std::io;

const WIDTH: u32 = 1400;
const SHOT: (f32, f32) = (1000.0, 560.0);
const ROW_HEIGHT: u32 = 620;
const HEADER_HEIGHT: u32 = 180;

/// One captured step, including its desktop-to-image mapping.
pub struct Step<'a> {
    pub frame: &'a Frame,
    pub area: Rect,
    pub mapping: Option<CaptureMapping>,
    pub click: (i32, i32),
    pub click_precise: Option<(f64, f64)>,
    pub right: bool,
    pub description: &'a str,
}

/// Converts an opaque captured frame into a pixmap.
pub fn frame_to_pixmap(frame: &Frame) -> Option<Pixmap> {
    frame.validate().ok()?;
    let mut image = Pixmap::new(frame.width, frame.height)?;
    for (source, target) in frame
        .pixels
        .chunks_exact(4)
        .zip(image.data_mut().chunks_exact_mut(4))
    {
        target.copy_from_slice(&[source[2], source[1], source[0], 255]);
    }
    Some(image)
}

/// Checks that the guide canvas and its cropped screenshots fit document limits.
pub fn fits(images: impl IntoIterator<Item = (u32, u32)>) -> bool {
    let mut count = 0_u64;
    let mut image_pixels = 0_u64;
    for (width, height) in images {
        if width == 0 || height == 0 {
            return false;
        }
        let Some(next_count) = count.checked_add(1) else {
            return false;
        };
        count = next_count;
        let pixels = u64::from(width.min(SHOT.0 as u32)) * u64::from(height.min(SHOT.1 as u32));
        let Some(next_pixels) = image_pixels.checked_add(pixels) else {
            return false;
        };
        image_pixels = next_pixels;
        if image_pixels > jiffshot_doc::io::MAX_DOCUMENT_PIXELS {
            return false;
        }
    }
    count
        .checked_mul(u64::from(ROW_HEIGHT))
        .and_then(|height| height.checked_add(u64::from(HEADER_HEIGHT)))
        .and_then(|height| height.checked_mul(u64::from(WIDTH)))
        .and_then(|canvas| canvas.checked_add(image_pixels))
        .is_some_and(|pixels| pixels <= jiffshot_doc::io::MAX_DOCUMENT_PIXELS)
}

/// Builds an editable guide with a marked screenshot and caption per step.
///
/// # Errors
/// Returns an error for invalid captured frames or an unrepresentable guide.
pub fn render(steps: &[Step<'_>], title: &str) -> io::Result<Document> {
    for (index, step) in steps.iter().enumerate() {
        step.frame.validate().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("invalid screenshot for step {}: {error}", index + 1),
            )
        })?;
    }
    if !fits(
        steps
            .iter()
            .map(|step| (step.frame.width, step.frame.height)),
    ) {
        return Err(io::Error::other("the step guide is too large"));
    }
    let height = u32::try_from(steps.len())
        .ok()
        .and_then(|count| count.checked_mul(ROW_HEIGHT))
        .and_then(|height| height.checked_add(HEADER_HEIGHT))
        .ok_or_else(|| io::Error::other("the step guide is too large"))?;
    let width = WIDTH as f32;
    let pad = 40.0;
    let row = ROW_HEIGHT as f32;
    let mut doc = Document::blank(WIDTH, height, Rgba::WHITE);
    let ink = Rgba::rgb(0x22, 0x26, 0x2E);
    let accent = Rgba::rgb(0x1E, 0x6F, 0xD9);
    let text = |size: f32, bold: bool, color: Rgba| Style {
        font_size: size,
        bold,
        text_color: color,
        width: 0.0,
        fill: Rgba::TRANSPARENT,
        align: Align::Left,
        ..Style::default()
    };
    doc.add(
        Kind::Text {
            rect: Box2::from_xywh(pad, 30.0, width - 2.0 * pad, 60.0),
            text: title.into(),
        },
        text(40.0, true, ink),
    );
    doc.add(
        Kind::Text {
            rect: Box2::from_xywh(pad, 88.0, width - 2.0 * pad, 30.0),
            text: format!("{} steps", steps.len()),
        },
        text(18.0, false, Rgba::rgb(0x6B, 0x72, 0x80)),
    );
    for (index, step) in steps.iter().enumerate() {
        let y = 140.0 + index as f32 * row;
        let (frame_width, frame_height) = (step.frame.width as f32, step.frame.height as f32);
        let mapping = match step.mapping {
            Some(mapping)
                if mapping.rect() == step.area
                    && mapping.size() == (step.frame.width, step.frame.height) =>
            {
                Some(mapping)
            }
            Some(_) => None,
            None => CaptureMapping::new(step.area, step.frame.width, step.frame.height),
        }
        .ok_or_else(|| io::Error::other("a Step Capture image has invalid screen dimensions"))?;
        let (cx, cy) = match step.click_precise {
            Some((x, y)) => mapping.image_point_precise(x, y).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a Step Capture click position is invalid",
                )
            })?,
            None => mapping.image_point(step.click.0, step.click.1),
        };
        let (cx, cy) = (cx as f32, cy as f32);
        let (view_width, view_height) = (SHOT.0.min(frame_width), SHOT.1.min(frame_height));
        let vx = (cx - view_width / 2.0).clamp(0.0, frame_width - view_width);
        let vy = (cy - view_height / 2.0).clamp(0.0, frame_height - view_height);
        let crop = step
            .frame
            .crop(Rect::new(
                vx as i32,
                vy as i32,
                view_width as i32,
                view_height as i32,
            ))
            .ok_or_else(|| io::Error::other("could not crop the step screenshot"))?;
        let image = frame_to_pixmap(&crop)
            .ok_or_else(|| io::Error::other("could not create the step screenshot"))?;
        let x0 = pad + 70.0;
        let rect = Box2::from_xywh(x0, y, image.width() as f32, image.height() as f32);
        let style = Style {
            width: 1.5,
            stroke: Rgba::rgb(0xD0, 0xD4, 0xDA),
            shadow: true,
            ..Style::default()
        };
        doc.add(
            Kind::Image {
                rect,
                image: jiffshot_doc::object::ImageRef::new(format!("step-{}", index + 1), image),
            },
            style,
        );
        let (px, py) = (x0 + cx - vx, y + cy - vy);
        let ring = Style {
            stroke: if step.right {
                Rgba::rgb(0xF5, 0x9E, 0x0B)
            } else {
                Rgba::RED
            },
            width: 4.0,
            fill: Rgba::TRANSPARENT,
            ..Style::default()
        };
        doc.add(
            Kind::Shape {
                rect: Box2::around(Pt::new(px, py), 22.0, 22.0),
                shape: ShapeKind::Ellipse,
            },
            ring,
        );
        doc.add(
            Kind::Stamp {
                rect: Box2::from_xywh(px - 6.0, py - 4.0, 34.0, 40.0),
                stamp: Stamp::Pointer,
            },
            Style::default(),
        );
        let badge = Style {
            fill: accent,
            text_color: Rgba::WHITE,
            width: 0.0,
            font_size: 24.0,
            ..Style::default()
        };
        doc.add(
            Kind::Step {
                center: Pt::new(pad + 26.0, y + 26.0),
                radius: 24.0,
                label: (index + 1).to_string(),
                shape: ShapeKind::Ellipse,
            },
            badge,
        );
        doc.add(
            Kind::Text {
                rect: Box2::from_xywh(
                    x0 + SHOT.0 + 24.0,
                    y,
                    width - x0 - SHOT.0 - 24.0 - pad,
                    120.0,
                ),
                text: step.description.to_owned(),
            },
            text(22.0, false, ink),
        );
    }
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_pixel_budget_includes_the_canvas_and_each_cropped_image() {
        assert!(fits([]));
        assert!(!fits([(0, 1)]));
        assert!(fits(vec![(1, 1); 37]));
        assert!(!fits(vec![(1, 1); 38]));
        assert!(fits(vec![(1000, 560); 23]));
        assert!(!fits(vec![(1000, 560); 24]));
        assert_eq!(fits([(1000, 560)]), fits([(8000, 4320)]));
    }

    #[test]
    fn invalid_frames_are_rejected_instead_of_omitting_the_screenshot() {
        let frame = Frame {
            width: 2,
            height: 2,
            pixels: vec![0; 4],
            alpha: false,
        };
        assert!(frame_to_pixmap(&frame).is_none());
        let step = Step {
            frame: &frame,
            area: Rect::new(0, 0, 2, 2),
            mapping: None,
            click: (1, 1),
            click_precise: None,
            right: false,
            description: "Test",
        };
        let error = render(&[step], "Guide")
            .err()
            .expect("invalid frame must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("step 1"));
    }
}
