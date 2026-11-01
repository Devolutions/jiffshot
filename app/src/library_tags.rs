//! Tag input shared by the Windows, macOS and Linux capture libraries.

pub(crate) fn parse_tags(text: &str) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for tag in text
        .split([',', ';'])
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
    {
        if !tags
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(tag))
        {
            tags.push(tag.to_owned());
        }
    }
    tags
}

#[cfg(test)]
mod tests {
    use super::parse_tags;

    #[test]
    fn trims_deduplicates_and_preserves_tag_order() {
        assert_eq!(
            parse_tags(" a, b ;; c ,A; b "),
            ["a", "b", "c"],
            "tag editing should match Windows separators and case-insensitive deduplication"
        );
        assert!(
            parse_tags(" , ; ").is_empty(),
            "empty tags should be removed"
        );
    }
}
