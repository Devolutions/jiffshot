//! Platform-specific substitutes for the Windows icon font.

#[cfg(windows)]
pub const FONT: &str = "Segoe Fluent Icons, Segoe MDL2 Assets, Segoe UI Symbol";
#[cfg(target_os = "macos")]
pub const FONT: &str = "Apple Symbols, Helvetica Neue";

#[cfg(windows)]
pub fn glyph(text: &str) -> &str {
    text
}

#[cfg(target_os = "macos")]
pub fn glyph(text: &str) -> &str {
    match text {
        "\u{E7C9}" | "\u{E8B0}" | "\u{E962}" => "↖",
        "\u{E72A}" => "➜",
        "\u{E738}" => "╱",
        "\u{E739}" => "□",
        "\u{E8D2}" => "T",
        "\u{E8BD}" => "▱",
        "\u{E70F}" => "✎",
        "\u{E7E6}" => "▰",
        "\u{E8A3}" => "⊕",
        "\u{E706}" => "◉",
        "\u{E734}" => "★",
        "\u{E790}" => "◩",
        "\u{E7A8}" => "⊞",
        "\u{E8C6}" => "✂",
        "\u{E8C8}" => "⧉",
        "\u{E8B8}" => "📷",
        "\u{E8B3}" => "▧",
        "\u{E75C}" => "▱",
        "\u{E8AC}" => "T↔",
        "\u{E7B3}" => "✣",
        "\u{E722}" => "▣",
        "\u{E714}" => "●",
        "\u{E74B}" => "↕",
        "\u{E72C}" => "↻",
        "\u{E8A7}" => "⌕",
        "\u{E713}" => "⚙",
        "\u{E710}" => "+",
        "\u{E70D}" => "▾",
        "\u{E76C}" => "⋯",
        "\u{E767}" => "♫",
        "\u{E74F}" => "♪",
        "\u{E720}" => "🎙",
        "\u{E711}" => "✕",
        "\u{E921}" => "−",
        "\u{E740}" => "□",
        _ => text,
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn windows_icons_have_visible_mac_substitutes() {
        let windows = [
            '\u{E7C9}', '\u{E72A}', '\u{E738}', '\u{E739}', '\u{E8D2}', '\u{E8BD}', '\u{E70F}', '\u{E7E6}', '\u{E8A3}',
            '\u{E706}', '\u{E734}', '\u{E790}', '\u{E7A8}', '\u{E8C6}', '\u{E8B3}', '\u{E75C}', '\u{E8AC}', '\u{E7B3}',
            '\u{E722}', '\u{E714}', '\u{E74B}', '\u{E72C}', '\u{E8A7}', '\u{E713}', '\u{E710}', '\u{E70D}', '\u{E76C}',
            '\u{E8B0}', '\u{E767}', '\u{E74F}', '\u{E720}', '\u{E711}', '\u{E921}', '\u{E740}', '\u{E962}', '\u{E8C8}',
            '\u{E8B8}',
        ];
        for icon in windows {
            assert_ne!(super::glyph(&icon.to_string()), icon.to_string());
        }
        assert_eq!(super::glyph("①"), "①");
    }
}
