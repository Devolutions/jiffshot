//! Text recognition in a captured image: the Windows OCR engine
//! (`Windows.Media.Ocr`) on Windows, Vision on macOS. Both recognize in the
//! user's preferred languages and report boxes in image pixels.

use crate::Rect;

#[cfg_attr(windows, path = "win/ocr.rs")]
#[cfg_attr(target_os = "macos", path = "mac/ocr.rs")]
mod imp;

pub use imp::recognize;

#[derive(Clone, Debug, PartialEq)]
pub struct Word {
    pub rect: Rect,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub rect: Rect,
    pub text: String,
    pub words: Vec<Word>,
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn word_position(text: &str, word: &str, offset: usize) -> Option<usize> {
    // Adjacent OCR words, including scripts without spaces, continue at the cursor.
    if text[offset..].starts_with(word) {
        return Some(offset);
    }
    let first_is_alphanumeric = word.chars().next().is_some_and(char::is_alphanumeric);
    let last_is_alphanumeric = word.chars().next_back().is_some_and(char::is_alphanumeric);
    text[offset..]
        .match_indices(word)
        .map(|(relative, _)| offset + relative)
        .find(|&start| {
            let end = start + word.len();
            !(first_is_alphanumeric && text[..start].chars().next_back().is_some_and(char::is_alphanumeric)
                || last_is_alphanumeric && text[end..].chars().next().is_some_and(char::is_alphanumeric))
        })
}

/// Finds each case-insensitive occurrence, returning its text and word bounds.
/// When the recognizer cannot map an occurrence to words, its line is returned.
pub fn find_text(lines: &[Line], phrase: &str) -> Vec<(String, Rect)> {
    let phrase = normalize(phrase);
    if phrase.is_empty() {
        return Vec::new();
    }

    let mut found = Vec::new();
    for line in lines {
        let text = normalize(&line.text);
        if !text.contains(&phrase) {
            continue;
        }
        let mut ranges = Vec::new();
        let mut next_word = 0;
        for (index, word) in line.words.iter().enumerate() {
            let content = normalize(&word.text);
            if content.is_empty() {
                continue;
            }
            if let Some(start) = word_position(&text, &content, next_word) {
                let end = start + content.len();
                ranges.push((start, end, index));
                next_word = end;
            }
        }

        let mut offset = 0;
        while let Some(relative) = text[offset..].find(&phrase) {
            let start = offset + relative;
            let end = start + phrase.len();
            let mut covered = start;
            let mut words = Vec::new();
            let mut complete = true;
            for &(from, to, index) in &ranges {
                if to <= start || from >= end {
                    continue;
                }
                if text[covered..from.max(covered)].chars().any(|c| !c.is_whitespace()) {
                    complete = false;
                    break;
                }
                if line.words[index].rect.is_empty() {
                    complete = false;
                    break;
                }
                covered = covered.max(to.min(end));
                words.push(&line.words[index]);
            }
            complete &= !text[covered..end].chars().any(|c| !c.is_whitespace());
            if complete && !words.is_empty() {
                let rect = words[1..].iter().fold(words[0].rect, |rect, word| rect.union(word.rect));
                let matched = words.iter().map(|word| word.text.as_str()).collect::<Vec<_>>().join(" ");
                found.push((matched, rect));
            } else {
                found.push((line.text.clone(), line.rect));
            }
            offset = text[start..].chars().next().map_or(text.len(), |c| start + c.len_utf8());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_repeated_and_overlapping_word_spans() {
        let words = (0..3)
            .map(|x| Word { text: "Save".into(), rect: Rect::new(x * 20, 0, 10, 10) })
            .collect();
        let line = Line { text: "SAVE save Save".into(), rect: Rect::new(0, 0, 50, 10), words };
        let matches = find_text(&[line], "save  SAVE");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].1, Rect::new(0, 0, 30, 10));
        assert_eq!(matches[1].1, Rect::new(20, 0, 30, 10));
    }

    #[test]
    fn finds_unicode_and_falls_back_to_the_line_when_words_do_not_match() {
        let line = Line {
            text: "CAFÉ, tea".into(),
            rect: Rect::new(5, 6, 70, 10),
            words: vec![Word { text: "CAFÉ".into(), rect: Rect::new(5, 6, 30, 10) }],
        };
        assert_eq!(find_text(std::slice::from_ref(&line), "café")[0].1, Rect::new(5, 6, 30, 10));
        assert_eq!(find_text(&[line], "café, tea")[0].1, Rect::new(5, 6, 70, 10));
    }

    #[test]
    fn missing_word_boxes_fall_back_for_each_unmapped_occurrence() {
        let line = Line {
            text: "Save Save".into(),
            rect: Rect::new(0, 0, 80, 10),
            words: vec![Word { text: "Save".into(), rect: Rect::new(0, 0, 30, 10) }],
        };
        let matches = find_text(&[line], "save");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].1, Rect::new(0, 0, 30, 10));
        assert_eq!(matches[1].1, Rect::new(0, 0, 80, 10));
    }

    #[test]
    fn empty_word_bounds_fall_back_to_the_line() {
        let line = Line {
            text: "Save".into(),
            rect: Rect::new(10, 20, 50, 12),
            words: vec![Word { text: "Save".into(), rect: Rect::new(10, 20, 0, 12) }],
        };
        assert_eq!(find_text(&[line], "save")[0].1, Rect::new(10, 20, 50, 12));
    }

    #[test]
    fn word_alignment_does_not_match_the_middle_of_another_word() {
        let line = Line {
            text: "cat at".into(),
            rect: Rect::new(0, 0, 60, 10),
            words: vec![Word { text: "at".into(), rect: Rect::new(40, 0, 20, 10) }],
        };
        let matches = find_text(&[line], "at");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].1, Rect::new(0, 0, 60, 10));
        assert_eq!(matches[1].1, Rect::new(40, 0, 20, 10));
    }

    #[test]
    fn adjacent_words_without_spaces_keep_their_own_bounds() {
        let line = Line {
            text: "你好世界".into(),
            rect: Rect::new(0, 0, 40, 10),
            words: vec![
                Word { text: "你好".into(), rect: Rect::new(0, 0, 20, 10) },
                Word { text: "世界".into(), rect: Rect::new(20, 0, 20, 10) },
            ],
        };
        assert_eq!(find_text(&[line], "世界")[0].1, Rect::new(20, 0, 20, 10));
    }

    #[test]
    fn adjacent_word_alignment_precedes_a_later_standalone_match() {
        let line = Line {
            text: "你好 你".into(),
            rect: Rect::new(0, 0, 40, 10),
            words: vec![
                Word { text: "你".into(), rect: Rect::new(0, 0, 10, 10) },
                Word { text: "好".into(), rect: Rect::new(10, 0, 10, 10) },
                Word { text: "你".into(), rect: Rect::new(30, 0, 10, 10) },
            ],
        };
        let matches = find_text(&[line], "你");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].1, Rect::new(0, 0, 10, 10));
        assert_eq!(matches[1].1, Rect::new(30, 0, 10, 10));
    }
}
