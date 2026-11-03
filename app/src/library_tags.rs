//! Tag input shared by the Windows, macOS and Linux capture libraries.

pub(crate) fn parse_tags(text: &str) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for tag in text
        .split([',', ';'])
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
    {
        let folded = tag.to_lowercase();
        if !tags.iter().any(|existing| existing.to_lowercase() == folded) {
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
        assert_eq!(
            parse_tags("École,éCOLE"),
            ["École"],
            "Unicode case variants should not create duplicate tags"
        );
    }
}
