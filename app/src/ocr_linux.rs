//! Linux OCR backed by the system Tesseract and Leptonica libraries.

use jiffshot_doc::editor::Analyzer;
use jiffshot_doc::smart::TextLine;
use jiffshot_doc::{IRect, Pixmap};
use leptess::{capi, LepTess};

pub struct Recognizer {
    api: LepTess,
}

impl Recognizer {
    pub fn new() -> Result<Self, String> {
        LepTess::new(None, "eng")
            .map(|api| Self { api })
            .map_err(|error| format!("could not initialize Tesseract OCR: {error}"))
    }

    pub fn recognize(&mut self, image: &Pixmap) -> Result<Vec<TextLine>, String> {
        self.recognize_area(image, IRect::of_size(image.width(), image.height()))
    }

    fn encoded_image(image: &Pixmap) -> Result<Vec<u8>, String> {
        let rgba: Vec<u8> = image
            .pixels()
            .iter()
            .flat_map(|pixel| {
                let inverse_alpha = 255 - pixel.alpha() as u16;
                let composite = |channel: u8| (channel as u16 + inverse_alpha).min(255) as u8;
                [
                    composite(pixel.red()),
                    composite(pixel.green()),
                    composite(pixel.blue()),
                    255,
                ]
            })
            .collect();

        let mut output = Vec::new();
        let mut encoder = png::Encoder::new(&mut output, image.width(), image.height());
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|error| format!("could not encode OCR image: {error}"))?;
        writer
            .write_image_data(&rgba)
            .map_err(|error| format!("could not encode OCR image: {error}"))?;
        drop(writer);
        Ok(output)
    }

    fn recognize_area(&mut self, image: &Pixmap, area: IRect) -> Result<Vec<TextLine>, String> {
        let Some(area) = area.intersect(&IRect::of_size(image.width(), image.height())) else {
            return Ok(Vec::new());
        };
        let Some(part) = jiffshot_doc::raster::extract(image, area) else {
            return Ok(Vec::new());
        };
        let encoded = Self::encoded_image(&part)?;
        self.api
            .set_image_from_mem(&encoded)
            .map_err(|error| format!("Tesseract could not read the selected image: {error}"))?;

        let lines = self
            .api
            .get_component_boxes(capi::TessPageIteratorLevel_RIL_TEXTLINE, true)
            .ok_or_else(|| "Tesseract did not return text line bounds".to_owned())?;
        let words = self
            .api
            .get_component_boxes(capi::TessPageIteratorLevel_RIL_WORD, true)
            .ok_or_else(|| "Tesseract did not return word bounds".to_owned())?;

        let mut result = Vec::with_capacity(lines.get_n());
        for line in &lines {
            let geometry = line.get_geometry();
            if geometry.w <= 0 || geometry.h <= 0 {
                continue;
            }
            self.api.set_rectangle_from_box(&line);
            let text = self
                .api
                .get_utf8_text()
                .map_err(|error| format!("Tesseract could not read a text line: {error}"))?
                .trim()
                .to_owned();

            let line_bottom = geometry.y + geometry.h;
            let line_words: Vec<(IRect, String)> = words
                .into_iter()
                .filter_map(|word| {
                    let word_geometry = word.get_geometry();
                    let center_y = word_geometry.y + word_geometry.h / 2;
                    if word_geometry.w <= 0
                        || word_geometry.h <= 0
                        || center_y < geometry.y
                        || center_y >= line_bottom
                    {
                        return None;
                    }
                    self.api.set_rectangle_from_box(&word);
                    let recognized = self.api.get_utf8_text().ok()?.trim().to_owned();
                    (!recognized.is_empty()).then_some((
                        IRect::new(
                            area.x + word_geometry.x,
                            area.y + word_geometry.y,
                            word_geometry.w,
                            word_geometry.h,
                        ),
                        recognized,
                    ))
                })
                .collect();

            if !text.is_empty() {
                result.push(TextLine {
                    area: IRect::new(
                        area.x + geometry.x,
                        area.y + geometry.y,
                        geometry.w,
                        geometry.h,
                    ),
                    text,
                    words: line_words,
                });
            }
        }
        Ok(result)
    }
}

impl Analyzer for Recognizer {
    fn text_in(&mut self, image: &Pixmap, area: IRect) -> Vec<TextLine> {
        match self.recognize_area(image, area) {
            Ok(lines) => lines,
            Err(error) => {
                eprintln!("Linux OCR failed: {error}");
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiffshot_doc::object::{Kind, Style};
    use jiffshot_doc::{Box2, Document, Rgba};

    #[test]
    fn ocr_image_encoding_is_a_valid_png() {
        let image = Pixmap::new(2, 1).expect("a 2x1 pixmap should be valid");
        let encoded = Recognizer::encoded_image(&image).expect("PNG encoding should succeed");
        let decoder = png::Decoder::new(encoded.as_slice());
        let reader = decoder
            .read_info()
            .expect("encoded image should be a valid PNG");
        assert_eq!(reader.info().width, 2);
        assert_eq!(reader.info().height, 1);
        assert_eq!(reader.info().color_type, png::ColorType::Rgba);
    }

    #[test]
    fn recognizes_text_from_an_image() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/images/editor.png");
        let document = jiffshot_doc::io::open(&path).expect("editor screenshot should load");
        let mut recognizer = Recognizer::new().expect("Tesseract should initialize");
        let area = IRect::new(400, 300, 300, 100);
        let lines = recognizer.text_in(document.background(), area);

        assert!(
            lines
                .iter()
                .any(|line| line.text.to_ascii_lowercase().contains("capture")),
            "OCR should recognize text from a cropped image region; got {lines:?}"
        );
        assert!(
            lines.iter().any(|line| !line.words.is_empty()),
            "OCR should return word bounds for Text Replace"
        );
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.words)
                .all(|(word_area, _)| {
                    word_area.x >= area.x
                        && word_area.y >= area.y
                        && word_area.x + word_area.w <= area.x + area.w
                        && word_area.y + word_area.h <= area.y + area.h
                }),
            "word positions should be relative to the original image, not the OCR crop"
        );
    }

    #[test]
    fn text_replace_creates_editable_objects_from_recognized_text() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/images/editor.png");
        let document = jiffshot_doc::io::open(&path).expect("editor screenshot should load");
        let mut editor = jiffshot_doc::editor::Editor::new(document);
        editor.analyzer = Some(Box::new(
            Recognizer::new().expect("Tesseract should initialize"),
        ));
        editor.set_tool(jiffshot_doc::editor::Tool::TextReplace);
        let start = jiffshot_doc::Pt::new(400.0, 300.0);
        let end = jiffshot_doc::Pt::new(700.0, 400.0);
        let modifiers = jiffshot_doc::editor::Modifiers::default();
        editor.pointer_down(start, modifiers, 4.0);
        editor.pointer_up(end, modifiers);

        assert!(
            !editor.document().objects.is_empty(),
            "Text Replace should create editable text objects from the selected text"
        );
        assert_ne!(
            editor.take_events() & jiffshot_doc::editor::EVENT_FOCUS_TEXT,
            0,
            "Text Replace should request focus for the first recognized text object"
        );
    }

    #[test]
    fn full_document_recognition_drives_conservative_smart_redaction() {
        let mut document = Document::blank(900, 160, Rgba::WHITE);
        document.add(
            Kind::Text {
                rect: Box2::from_xywh(20.0, 20.0, 850.0, 100.0),
                text: "Contact test@example.com".to_owned(),
            },
            Style {
                font: "DejaVu Sans".to_owned(),
                font_size: 48.0,
                text_color: Rgba::BLACK,
                width: 0.0,
                ..Style::default()
            },
        );
        let mut recognizer = Recognizer::new().expect("Tesseract should initialize");
        let lines = recognizer
            .recognize(&document.flatten())
            .expect("full-image text recognition should succeed");
        assert!(
            lines
                .iter()
                .any(|line| line.text.contains("test@example.com")),
            "OCR should recognize the email in the rendered document: {lines:?}"
        );
        let mut editor = jiffshot_doc::editor::Editor::new(document);
        assert_eq!(crate::editor_linux::smart_redact(&mut editor, &lines), 1);
        assert!(
            matches!(
                editor.document().objects.last().map(|object| &object.kind),
                Some(Kind::Blur {
                    mode: jiffshot_doc::object::BlurMode::Redact,
                    ..
                })
            ),
            "recognized sensitive text should produce an editable redaction"
        );
    }
}
