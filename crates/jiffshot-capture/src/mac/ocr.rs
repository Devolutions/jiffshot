//! Text recognition with Vision (`VNRecognizeTextRequest`, accurate level,
//! language correction, automatic language detection). Boxes come back
//! normalized with a bottom-left origin and are mapped to image pixels.

use super::{Line, Word};
use crate::macos::sck;
use crate::Rect;
use objc2::rc::Retained;
use objc2::AllocAnyThread;
use objc2_core_foundation::CGRect;
use objc2_core_graphics::CGImage;
use objc2_foundation::{NSArray, NSDictionary, NSRange};
use objc2_vision::{
    VNImageRequestHandler, VNRecognizeTextRequest, VNRecognizedText, VNRequest, VNRequestTextRecognitionLevel,
};

/// Recognizes text in a top-down BGRA image, line by line.
pub fn recognize(width: u32, height: u32, bgra: &[u8]) -> Result<Vec<Line>, String> {
    if width == 0 || height == 0 || bgra.len() < (width * height * 4) as usize {
        return Ok(Vec::new());
    }
    let image = sck::bgra_image(width as usize, height as usize, bgra).map_err(|e| e.to_string())?;
    // SAFETY: `image` owns a live CGImage for the duration of the request.
    let cg: &CGImage = unsafe { &*(image.0 as *const CGImage) };
    let options = NSDictionary::new();
    // SAFETY: an empty options dictionary is valid.
    let handler = unsafe { VNImageRequestHandler::initWithCGImage_options(VNImageRequestHandler::alloc(), cg, &options) };
    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    request.setUsesLanguageCorrection(true);
    request.setAutomaticallyDetectsLanguage(true);
    let as_request: Retained<VNRequest> = Retained::into_super(Retained::into_super(request.clone()));
    handler
        .performRequests_error(&NSArray::from_retained_slice(&[as_request]))
        .map_err(|e| e.localizedDescription().to_string())?;
    let (w, h) = (width as f64, height as f64);
    let to_px = |b: CGRect| -> Rect {
        let left = (b.origin.x * w).round() as i32;
        let top = ((1.0 - b.origin.y - b.size.height) * h).round() as i32;
        let right = ((b.origin.x + b.size.width) * w).round() as i32;
        let bottom = ((1.0 - b.origin.y) * h).round() as i32;
        Rect::from_ltrb(left, top, right, bottom).intersect(Rect { x: 0, y: 0, width: width as i32, height: height as i32 }).unwrap_or_default()
    };
    let mut lines = Vec::new();
    for observation in request.results().unwrap_or_default().iter() {
        let candidates = observation.topCandidates(1);
        let Some(best) = candidates.firstObject() else { continue };
        let text = best.string().to_string();
        if text.trim().is_empty() {
            continue;
        }
        // SAFETY: reading a property of a live observation.
        let rect = to_px(unsafe { observation.boundingBox() });
        let words = words_of(&best, &text, rect, &to_px);
        lines.push(Line { rect, text, words });
    }
    // Vision reports lines in reading order already; keep a stable
    // top-to-bottom order for callers that rely on it.
    lines.sort_by_key(|l| (l.rect.y, l.rect.x));
    Ok(lines)
}

/// The whitespace-separated words of a line with their boxes. Scripts
/// without spaces come back as a single word spanning the line.
fn words_of(best: &VNRecognizedText, text: &str, line: Rect, to_px: &dyn Fn(CGRect) -> Rect) -> Vec<Word> {
    let mut words = Vec::new();
    let mut utf16 = 0usize;
    let mut start: Option<(usize, usize)> = None;
    let mut push = |from_byte: usize, to_byte: usize, from16: usize, to16: usize| {
        let range = NSRange { location: from16, length: to16 - from16 };
        // SAFETY: the range lies within the candidate's string.
        let rect = unsafe { best.boundingBoxForRange_error(range) }
            .ok()
            // SAFETY: reading a property of a live observation.
            .map(|o| to_px(unsafe { o.boundingBox() }))
            .filter(|r| !r.is_empty())
            .unwrap_or(line);
        words.push(Word { rect, text: text[from_byte..to_byte].to_owned() });
    };
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            if let Some((b, u)) = start.take() {
                push(b, i, u, utf16);
            }
        } else if start.is_none() {
            start = Some((i, utf16));
        }
        utf16 += c.len_utf16();
    }
    if let Some((b, u)) = start {
        push(b, text.len(), u, utf16);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_images_have_no_text() {
        assert!(recognize(0, 0, &[]).unwrap().is_empty());
        let white = vec![255u8; 64 * 32 * 4];
        assert!(recognize(64, 32, &white).unwrap().is_empty());
    }
}
