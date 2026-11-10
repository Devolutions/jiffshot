//! Recording annotations rendered in the video stream and captured as captions.

use gstreamer as gst;
use gstreamer_video as video;
use gstreamer_video::prelude::*;
use jiffshot_doc::object::{Kind, Style};
use jiffshot_doc::{Document, Pixmap};
use jiffshot_video::recording_marks::RecordedCaption;
use parking_lot::Mutex;
use std::sync::Arc;

const MAX_ANNOTATIONS: usize = 256;

struct Annotation {
    id: u64,
    kind: Kind,
    style: Style,
    start_ms: u64,
    end_ms: Option<u64>,
    caption: Option<usize>,
}

#[derive(Default)]
pub(super) struct Annotations {
    size: Option<(u32, u32)>,
    observed_info: Option<video::VideoInfo>,
    next_id: u64,
    marks: Vec<Annotation>,
    rendered: Vec<u64>,
    image: Option<Pixmap>,
    document: Option<Document>,
    converters: Option<Converters>,
}

impl Annotations {
    pub(super) fn frame_height(&self) -> Option<u32> {
        self.size.map(|(_, height)| height)
    }

    pub(super) fn add(
        &mut self,
        kind: Kind,
        style: Style,
        at_ms: u64,
        duration_ms: u64,
        captions: &mut Vec<RecordedCaption>,
    ) -> Result<u64, String> {
        let info = self
            .observed_info
            .as_ref()
            .ok_or("recording frames are not ready")?;
        if u64::from(info.width()) * u64::from(info.height())
            > jiffshot_doc::io::MAX_DOCUMENT_PIXELS
        {
            return Err("recording is too large for annotations".into());
        }
        if self.marks.len() >= MAX_ANNOTATIONS {
            return Err("at most 256 live annotations; use record.clear".into());
        }
        let caption = match &kind {
            Kind::Text { rect, text } | Kind::Callout { rect, text, .. } => {
                if text.trim().is_empty() || text.chars().count() > 4096 {
                    return Err("recorded caption text must contain 1..4096 characters".into());
                }
                if captions.len() >= MAX_ANNOTATIONS {
                    return Err("at most 256 recorded text captions are supported".into());
                }
                Some(RecordedCaption {
                    at_ms,
                    duration_ms: Some(if duration_ms == 0 {
                        u64::MAX
                    } else {
                        duration_ms
                    }),
                    burned_in: true,
                    text: text.clone(),
                    x: rect.x0.round() as i32,
                    y: rect.y0.round() as i32,
                    font: style.font.clone(),
                    size: style.font_size,
                    color: style.text_color,
                    background: style.fill,
                    padding: style.padding.max(0.0).round().min(1024.0) as u32,
                })
            }
            _ => None,
        };
        let id = self
            .next_id
            .checked_add(1)
            .ok_or("too many recording annotations")?;
        if self
            .converters
            .as_ref()
            .is_none_or(|converters| converters.source != *info)
        {
            self.converters = Some(Converters::new(info)?);
        }
        let caption = caption.map(|caption| {
            let index = captions.len();
            captions.push(caption);
            index
        });
        self.next_id = id;
        self.marks.push(Annotation {
            id,
            kind,
            style,
            start_ms: at_ms,
            end_ms: (duration_ms != 0).then(|| at_ms.saturating_add(duration_ms)),
            caption,
        });
        Ok(id)
    }

    pub(super) fn clear(&mut self, at_ms: u64, captions: &mut [RecordedCaption]) {
        for mark in &mut self.marks {
            let end = mark.end_ms.map_or(at_ms, |end| end.min(at_ms));
            mark.end_ms = Some(end);
            if let Some(caption) = mark.caption.and_then(|index| captions.get_mut(index)) {
                caption.duration_ms = Some(end.saturating_sub(caption.at_ms));
            }
        }
    }

    fn prepare(&mut self, info: &video::VideoInfo, at_ms: u64) -> Result<bool, String> {
        let size = (info.width(), info.height());
        if self.observed_info.as_ref() != Some(info) {
            self.observed_info = Some(info.clone());
            self.converters = None;
        }
        if self.size != Some(size) {
            self.size = Some(size);
            self.rendered.clear();
            self.image = None;
            self.document = None;
            self.converters = None;
        }
        self.marks
            .retain(|mark| mark.end_ms.is_none_or(|end| at_ms < end));
        let visible: Vec<u64> = self
            .marks
            .iter()
            .filter(|mark| mark.start_ms <= at_ms)
            .map(|mark| mark.id)
            .collect();
        if visible != self.rendered {
            self.image = None;
            self.document = None;
            if !visible.is_empty() {
                if u64::from(size.0) * u64::from(size.1) > jiffshot_doc::io::MAX_DOCUMENT_PIXELS {
                    return Err("recording is too large for annotations".into());
                }
                let background = Pixmap::new(size.0, size.1)
                    .ok_or("could not create recording annotation canvas")?;
                let mut document = Document::new(background);
                for mark in self.marks.iter().filter(|mark| visible.contains(&mark.id)) {
                    document.add(mark.kind.clone(), mark.style.clone());
                }
                if document
                    .objects
                    .iter()
                    .any(|object| object.reads().is_some())
                {
                    self.document = Some(document);
                } else {
                    self.image = Some(document.flatten());
                }
            }
            self.rendered = visible;
        }
        Ok(!self.rendered.is_empty())
    }

    fn render(&mut self, info: &video::VideoInfo, buffer: &mut gst::Buffer) -> Result<(), String> {
        if self
            .converters
            .as_ref()
            .is_none_or(|converters| converters.source != *info)
        {
            self.converters = Some(Converters::new(info)?);
        }
        let converters = self.converters.as_mut().ok_or("missing video converter")?;
        let mut source = video::VideoFrameRef::from_buffer_ref_writable(buffer.make_mut(), info)
            .map_err(|error| format!("could not map recording frame: {error}"))?;
        let mut rgba = video::VideoFrameRef::from_buffer_ref_writable(
            converters.scratch.make_mut(),
            &converters.rgba,
        )
        .map_err(|error| format!("could not map annotation frame: {error}"))?;
        converters.to_rgba.frame_ref(&source, &mut rgba);
        let stride = rgba
            .plane_stride()
            .first()
            .and_then(|stride| usize::try_from(*stride).ok())
            .ok_or("invalid annotation frame stride")?;
        let pixels = rgba
            .plane_data_mut(0)
            .map_err(|error| format!("could not read annotation pixels: {error}"))?;
        let width = info.width() as usize;
        let height = info.height() as usize;
        if let Some(document) = &mut self.document {
            let background = opaque_pixmap(pixels, stride, info.width(), info.height())?;
            document.set_background(background);
            blend(pixels, stride, width, height, &document.flatten())?;
        } else if let Some(image) = &self.image {
            blend(pixels, stride, width, height, image)?;
        }
        converters.from_rgba.frame_ref(&rgba, &mut source);
        Ok(())
    }
}

struct Converters {
    source: video::VideoInfo,
    rgba: video::VideoInfo,
    to_rgba: video::video_converter::VideoConverter,
    from_rgba: video::video_converter::VideoConverter,
    scratch: gst::Buffer,
}

impl Converters {
    fn new(source: &video::VideoInfo) -> Result<Self, String> {
        let rgba =
            video::VideoInfo::builder(video::VideoFormat::Rgba, source.width(), source.height())
                .fps(source.fps())
                .par(source.par())
                .interlace_mode(source.interlace_mode())
                .build()
                .map_err(|error| format!("could not describe annotation frames: {error}"))?;
        let to_rgba = video::video_converter::VideoConverter::new(source, &rgba, None)
            .map_err(|error| format!("could not convert recording frames to RGBA: {error}"))?;
        let from_rgba = video::video_converter::VideoConverter::new(&rgba, source, None)
            .map_err(|error| format!("could not convert annotation frames: {error}"))?;
        let scratch = gst::Buffer::with_size(rgba.size())
            .map_err(|error| format!("could not allocate annotation frame: {error}"))?;
        Ok(Self {
            source: source.clone(),
            rgba,
            to_rgba,
            from_rgba,
            scratch,
        })
    }
}

fn rows(pixels: &[u8], stride: usize, width: usize, height: usize) -> Result<(), String> {
    let row = width
        .checked_mul(4)
        .ok_or("annotation width is too large")?;
    let required = height
        .checked_sub(1)
        .and_then(|height| height.checked_mul(stride))
        .and_then(|offset| offset.checked_add(row))
        .ok_or("invalid annotation frame dimensions")?;
    if stride < row || pixels.len() < required {
        return Err("invalid annotation frame layout".into());
    }
    Ok(())
}

fn opaque_pixmap(pixels: &[u8], stride: usize, width: u32, height: u32) -> Result<Pixmap, String> {
    rows(pixels, stride, width as usize, height as usize)?;
    let mut image = Pixmap::new(width, height).ok_or("could not create annotation background")?;
    for (row, destination) in image
        .data_mut()
        .chunks_exact_mut(width as usize * 4)
        .enumerate()
    {
        let source = &pixels[row * stride..row * stride + destination.len()];
        for (destination, source) in destination
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(source.as_chunks::<4>().0)
        {
            destination.copy_from_slice(&[source[0], source[1], source[2], 255]);
        }
    }
    Ok(image)
}

fn blend(
    pixels: &mut [u8],
    stride: usize,
    width: usize,
    height: usize,
    image: &Pixmap,
) -> Result<(), String> {
    rows(pixels, stride, width, height)?;
    let width = width.min(image.width() as usize);
    let height = height.min(image.height() as usize);
    let image_stride = image.width() as usize * 4;
    for row in 0..height {
        let source = &image.data()[row * image_stride..row * image_stride + width * 4];
        let destination = &mut pixels[row * stride..row * stride + width * 4];
        for (destination, source) in destination
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(source.as_chunks::<4>().0)
        {
            let inverse = 255 - u32::from(source[3]);
            for channel in 0..3 {
                destination[channel] = (u32::from(source[channel])
                    + (u32::from(destination[channel]) * inverse + 127) / 255)
                    as u8;
            }
        }
    }
    Ok(())
}

pub(super) fn attach(pipeline: &gst::Pipeline) -> Result<Arc<Mutex<Annotations>>, String> {
    let converter = pipeline
        .by_name("recording-video-converter")
        .ok_or("recording pipeline has no video converter")?;
    let pad = converter
        .static_pad("src")
        .ok_or("recording video converter has no output pad")?;
    let annotations = Arc::new(Mutex::new(Annotations::default()));
    let state = Arc::clone(&annotations);
    let weak_pipeline = pipeline.downgrade();
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, probe| {
        let result = (|| {
            let caps = pad
                .current_caps()
                .ok_or("recording video caps are unavailable")?;
            let info = video::VideoInfo::from_caps(&caps)
                .map_err(|error| format!("invalid recording video caps: {error}"))?;
            let buffer = probe
                .buffer_mut()
                .ok_or("recording video buffer is unavailable")?;
            let at_ms = buffer
                .pts()
                .ok_or("recording video timestamp is unavailable")?
                .mseconds();
            let mut annotations = state.lock();
            if annotations.prepare(&info, at_ms)? {
                annotations.render(&info, buffer)?;
            }
            Ok::<(), String>(())
        })();
        if let Err(error) = result {
            if let Some(pipeline) = weak_pipeline.upgrade() {
                gst::element_error!(
                    pipeline,
                    gst::StreamError::Format,
                    ("could not render recording annotations: {error}")
                );
            }
            return gst::PadProbeReturn::Drop;
        }
        gst::PadProbeReturn::Ok
    })
    .ok_or("could not observe recording annotation frames")?;
    Ok(annotations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiffshot_doc::object::BlurMode;
    use jiffshot_doc::{Box2, Rgba};

    fn info() -> video::VideoInfo {
        gst::init().expect("GStreamer initialization");
        video::VideoInfo::builder(video::VideoFormat::Rgba, 16, 16)
            .fps(gst::Fraction::new(30, 1))
            .build()
            .expect("video info")
    }

    fn text(value: &str) -> Kind {
        Kind::Text {
            rect: Box2::new(1.0, 2.0, 14.0, 12.0),
            text: value.to_owned(),
        }
    }

    #[test]
    fn annotations_validate_readiness_and_text_before_mutating_state() {
        let mut annotations = Annotations::default();
        let mut captions = Vec::new();
        assert!(annotations
            .add(text("hello"), Style::default(), 0, 0, &mut captions)
            .unwrap_err()
            .contains("not ready"));
        assert!(!annotations.prepare(&info(), 0).unwrap());
        assert!(annotations
            .add(text("  "), Style::default(), 0, 0, &mut captions)
            .unwrap_err()
            .contains("caption text"));
        assert!(annotations.marks.is_empty());
        assert!(captions.is_empty());
        assert_eq!(
            annotations
                .add(text("hello"), Style::default(), 10, 0, &mut captions)
                .unwrap(),
            1
        );
        assert!(captions[0].burned_in);
    }

    #[test]
    fn annotation_times_and_clear_keep_overlapping_captions_independent() {
        let mut annotations = Annotations::default();
        let mut captions = Vec::new();
        assert!(!annotations.prepare(&info(), 0).unwrap());
        annotations
            .add(text("first"), Style::default(), 100, 0, &mut captions)
            .unwrap();
        annotations
            .add(text("second"), Style::default(), 200, 300, &mut captions)
            .unwrap();

        assert!(!annotations.prepare(&info(), 99).unwrap());
        assert!(annotations.prepare(&info(), 100).unwrap());
        assert_eq!(annotations.rendered.len(), 1);
        assert!(annotations.prepare(&info(), 200).unwrap());
        assert_eq!(annotations.rendered.len(), 2);

        annotations.clear(250, &mut captions);
        assert_eq!(captions[0].duration_ms, Some(150));
        assert_eq!(captions[1].duration_ms, Some(50));
        assert!(annotations.prepare(&info(), 249).unwrap());
        assert!(!annotations.prepare(&info(), 250).unwrap());
    }

    #[test]
    fn timed_annotations_expire_on_the_output_timeline() {
        let mut annotations = Annotations::default();
        let mut captions = Vec::new();
        annotations.prepare(&info(), 0).unwrap();
        annotations
            .add(text("timed"), Style::default(), 100, 200, &mut captions)
            .unwrap();
        assert!(annotations.prepare(&info(), 299).unwrap());
        assert!(!annotations.prepare(&info(), 300).unwrap());
        assert_eq!(captions[0].duration_ms, Some(200));
    }

    #[test]
    fn background_reading_annotations_use_recorded_pixels() {
        let info = info();
        let mut annotations = Annotations::default();
        let mut captions = Vec::new();
        annotations.prepare(&info, 0).unwrap();
        annotations
            .add(
                Kind::Blur {
                    rect: Box2::new(4.0, 4.0, 12.0, 12.0),
                    mode: BlurMode::Redact,
                    amount: 0.0,
                },
                Style {
                    fill: Rgba::new(0, 0, 0, 255),
                    ..Style::default()
                },
                0,
                0,
                &mut captions,
            )
            .unwrap();
        assert!(annotations.prepare(&info, 0).unwrap());
        assert!(annotations.document.is_some());
        let mut buffer = gst::Buffer::with_size(info.size()).unwrap();
        buffer
            .get_mut()
            .unwrap()
            .map_writable()
            .unwrap()
            .as_mut_slice()
            .fill(255);
        annotations.render(&info, &mut buffer).unwrap();
        let mapped = buffer.map_readable().unwrap();
        let at = |x: usize, y: usize| &mapped.as_slice()[(y * 16 + x) * 4..][..3];
        assert_eq!(at(1, 1), [255, 255, 255]);
        assert_eq!(at(8, 8), [0, 0, 0]);
        assert!(captions.is_empty());
    }

    #[test]
    fn blending_respects_stride_and_rejects_truncated_frames() {
        let mut image = Pixmap::new(2, 1).unwrap();
        image.fill(tiny_skia::Color::from_rgba8(255, 0, 0, 128));
        let mut pixels = vec![40, 50, 60, 255, 40, 50, 60, 255, 7, 8, 9, 10];
        blend(&mut pixels, 12, 2, 1, &image).unwrap();
        assert_eq!(&pixels[..4], &[148, 25, 30, 255]);
        assert_eq!(&pixels[8..], &[7, 8, 9, 10]);
        assert!(blend(&mut pixels[..7], 8, 2, 1, &image).is_err());
    }
}
