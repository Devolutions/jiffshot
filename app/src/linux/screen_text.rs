//! Recognize a desktop selection without delivering an image to the editor.

use super::{clipboard, editor, ocr, screenshot};
use jiffshot_doc::smart::TextLine;
use jiffshot_doc::{Document, Pixmap};

#[derive(Debug, Eq, PartialEq)]
pub(super) enum ResultKind {
    Cancelled,
    NoText,
    Copied(usize),
}

pub(super) fn grab() -> Result<ResultKind, String> {
    grab_with(
        || futures_lite::future::block_on(screenshot::capture_interactive()),
        |image| ocr::Recognizer::new()?.recognize(image),
        clipboard::copy_text,
    )
}

fn grab_with(
    capture: impl FnOnce() -> Result<Option<Document>, String>,
    recognize: impl FnOnce(&Pixmap) -> Result<Vec<TextLine>, String>,
    copy: impl FnOnce(String) -> Result<(), String>,
) -> Result<ResultKind, String> {
    let Some(document) = capture()? else {
        return Ok(ResultKind::Cancelled);
    };
    let lines = recognize(&document.flatten())?;
    let text = editor::recognized_text(&lines);
    if text.is_empty() {
        return Ok(ResultKind::NoText);
    }
    let count = text.lines().count();
    copy(text)?;
    Ok(ResultKind::Copied(count))
}

pub(super) fn message(result: Result<ResultKind, String>) -> String {
    match result {
        Ok(ResultKind::Cancelled) => "Text capture cancelled.".to_owned(),
        Ok(ResultKind::NoText) => "No text recognized; the clipboard was not changed.".to_owned(),
        Ok(ResultKind::Copied(count)) => {
            format!("Copied {count} recognized line(s) from the screen.")
        }
        Err(error) => format!("Could not grab screen text: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiffshot_doc::editor::Editor;
    use jiffshot_doc::object::{Kind, ShapeKind, Style};
    use jiffshot_doc::{Box2, IRect, Rgba};
    use std::cell::RefCell;

    fn line(text: &str) -> TextLine {
        TextLine {
            area: IRect::new(0, 0, 1, 1),
            text: text.to_owned(),
            words: Vec::new(),
        }
    }

    fn selection() -> Option<Document> {
        Some(Document::blank(3, 2, Rgba::WHITE))
    }

    #[test]
    fn screen_text_copies_only_nonempty_trimmed_lines() {
        let copied = RefCell::new(String::new());
        let result = grab_with(
            || Ok(selection()),
            |image| {
                assert_eq!((image.width(), image.height()), (3, 2));
                Ok(vec![line("  first "), line("  "), line("second")])
            },
            |text| {
                *copied.borrow_mut() = text;
                Ok(())
            },
        );
        assert_eq!(result, Ok(ResultKind::Copied(2)));
        assert_eq!(*copied.borrow(), "first\nsecond");
        assert_eq!(
            message(result),
            "Copied 2 recognized line(s) from the screen."
        );
    }

    #[test]
    fn cancellation_and_empty_text_leave_the_clipboard_alone() {
        let cancelled = grab_with(
            || Ok(None),
            |_| panic!("cancellation must not run OCR"),
            |_| panic!("cancellation must not copy"),
        );
        assert_eq!(cancelled, Ok(ResultKind::Cancelled));
        assert_eq!(message(cancelled), "Text capture cancelled.");

        let empty = grab_with(
            || Ok(selection()),
            |_| Ok(vec![line("  \n ")]),
            |_| panic!("empty recognition must not copy"),
        );
        assert_eq!(empty, Ok(ResultKind::NoText));
        assert_eq!(
            message(empty),
            "No text recognized; the clipboard was not changed."
        );
    }

    #[test]
    fn blank_screenshot_is_reported_as_no_text() {
        let result = grab_with(
            || Ok(Some(Document::blank(240, 140, Rgba::WHITE))),
            |image| ocr::Recognizer::new()?.recognize(image),
            |_| panic!("a blank screenshot must not change the clipboard"),
        );
        assert_eq!(result, Ok(ResultKind::NoText));
    }

    #[test]
    fn capture_recognition_and_clipboard_errors_are_reported() {
        let failed_capture = grab_with(
            || Err("chooser failed".into()),
            |_| panic!("a failed chooser must not run OCR"),
            |_| panic!("a failed chooser must not copy"),
        );
        assert_eq!(
            message(failed_capture),
            "Could not grab screen text: chooser failed"
        );

        let failed_ocr = grab_with(
            || Ok(selection()),
            |_| Err("OCR failed".into()),
            |_| panic!("failed recognition must not copy"),
        );
        assert_eq!(
            message(failed_ocr),
            "Could not grab screen text: OCR failed"
        );

        let failed_copy = grab_with(
            || Ok(selection()),
            |_| Ok(vec![line("recognized")]),
            |_| Err("clipboard unavailable".into()),
        );
        assert_eq!(
            message(failed_copy),
            "Could not grab screen text: clipboard unavailable"
        );
    }

    #[test]
    fn edits_made_during_screen_selection_keep_their_undo_history() {
        let editor = RefCell::new(Editor::new(Document::blank(20, 20, Rgba::WHITE)));
        let result = grab_with(
            || {
                editor.borrow_mut().add_object(
                    "Add rectangle",
                    Kind::Shape {
                        rect: Box2::from_xywh(2.0, 2.0, 8.0, 8.0),
                        shape: ShapeKind::Rectangle,
                    },
                    Style::default(),
                );
                Ok(selection())
            },
            |_| Ok(vec![line("screen text")]),
            |_| Ok(()),
        );
        assert_eq!(result, Ok(ResultKind::Copied(1)));
        let mut editor = editor.borrow_mut();
        assert!(editor.is_modified());
        assert_eq!(editor.document().objects.len(), 1);
        assert!(editor.undo());
        assert!(editor.document().objects.is_empty());
        assert!(editor.redo());
        assert_eq!(editor.document().objects.len(), 1);
    }
}
