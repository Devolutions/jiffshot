//! Platform-specific substitutes for the Windows icon font.

#[cfg(windows)]
pub const FONT: &str = "Segoe Fluent Icons, Segoe MDL2 Assets, Segoe UI Symbol";
#[cfg(target_os = "macos")]
pub const FONT: &str = "Apple Symbols, Helvetica Neue";
#[cfg(target_os = "linux")]
pub const FONT: &str = "Noto Sans Symbols 2, Noto Sans Symbols, DejaVu Sans, Symbola";

#[cfg(windows)]
pub fn glyph(text: &str) -> &str {
    text
}

#[cfg(not(windows))]
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

#[cfg(all(test, not(windows)))]
mod tests {
    #[test]
    fn windows_icons_have_visible_mac_substitutes() {
        let windows = [
            '\u{E7C9}', '\u{E72A}', '\u{E738}', '\u{E739}', '\u{E8D2}', '\u{E8BD}', '\u{E70F}',
            '\u{E7E6}', '\u{E8A3}', '\u{E706}', '\u{E734}', '\u{E790}', '\u{E7A8}', '\u{E8C6}',
            '\u{E8B3}', '\u{E75C}', '\u{E8AC}', '\u{E7B3}', '\u{E722}', '\u{E714}', '\u{E74B}',
            '\u{E72C}', '\u{E8A7}', '\u{E713}', '\u{E710}', '\u{E70D}', '\u{E76C}', '\u{E8B0}',
            '\u{E767}', '\u{E74F}', '\u{E720}', '\u{E711}', '\u{E921}', '\u{E740}', '\u{E962}',
            '\u{E8C8}', '\u{E8B8}',
        ];
        for icon in windows {
            assert_ne!(super::glyph(&icon.to_string()), icon.to_string());
        }
        assert_eq!(super::glyph("①"), "①");
    }
}

/// Crisp 18×18 stroke-path geometries for the editor toolbar and overflow menu.
pub mod vector {
    pub const SELECT_REGION: &str =
        "M 2,6.5 L 2,2 L 6.5,2 M 11.5,2 L 16,2 L 16,6.5 M 16,11.5 L 16,16 L 11.5,16 M 6.5,16 L 2,16 L 2,11.5";
    pub const MOVE: &str =
        "M 4.5,2 L 4.5,14.2 L 7.6,11.2 L 9.9,15.8 L 11.8,14.9 L 9.5,10.3 L 14,10.3 Z";
    pub const ARROW: &str = "M 3,15 L 15,3 M 8.5,3 L 15,3 L 15,9.5";
    pub const LINE: &str =
        "M 4,14 L 14,4 M 3,13 A 1.4,1.4 0 1 1 2.99,13 Z M 15,3 A 1.4,1.4 0 1 1 14.99,3 Z";
    pub const SHAPE: &str = "M 3,3.5 L 15,3.5 L 15,14.5 L 3,14.5 Z";
    pub const TEXT: &str =
        "M 3.5,3.5 L 14.5,3.5 M 9,3.5 L 9,15 M 6.5,15 L 11.5,15 M 3.5,3.5 L 3.5,6 M 14.5,3.5 L 14.5,6";
    pub const CALLOUT: &str =
        "M 2.5,3.5 L 15.5,3.5 L 15.5,11.5 L 8.5,11.5 L 5,15 L 5,11.5 L 2.5,11.5 Z";
    pub const STEP: &str =
        "M 9,2 A 7,7 0 1 1 8.99,2 Z M 7.6,6.8 L 9.3,5.5 L 9.3,12.5 M 7.4,12.5 L 11.2,12.5";
    pub const PEN: &str =
        "M 12.5,2.5 L 15.5,5.5 L 6.5,14.5 L 2.5,15.5 L 3.5,11.5 Z M 10.5,4.5 L 13.5,7.5";
    pub const HIGHLIGHT: &str =
        "M 11.5,2.5 L 15,6 L 8.5,12.5 L 5,9 Z M 5,9 L 3.5,12.5 L 8.5,12.5 M 3,15.5 L 15,15.5";
    pub const BLUR: &str =
        "M 3,3 L 15,3 L 15,15 L 3,15 Z M 7,3 L 7,15 M 11,3 L 11,15 M 3,7 L 15,7 M 3,11 L 15,11";
    pub const MAGNIFY: &str =
        "M 8,2.5 A 5.5,5.5 0 1 1 7.99,2.5 Z M 12,12 L 16,16 M 8,5.5 L 8,10.5 M 5.5,8 L 10.5,8";
    pub const SPOTLIGHT: &str =
        "M 9,5 A 4,4 0 1 1 8.99,5 Z M 9,1.5 L 9,3.2 M 9,14.8 L 9,16.5 M 1.5,9 L 3.2,9 M 14.8,9 L 16.5,9 M 3.7,3.7 L 4.9,4.9 M 13.1,13.1 L 14.3,14.3 M 14.3,3.7 L 13.1,4.9 M 4.9,13.1 L 3.7,14.3";
    pub const STAMP: &str =
        "M 9,2 L 11.1,6.5 L 16,7.1 L 12.4,10.5 L 13.3,15.4 L 9,13 L 4.7,15.4 L 5.6,10.5 L 2,7.1 L 6.9,6.5 Z";
    pub const FILL: &str =
        "M 8.5,3 L 13.5,8 L 8.5,13 L 3.5,8 Z M 5.5,2 L 8.5,5 M 14.8,11 C 14.8,11 16.3,12.8 16.3,13.9 A 1.5,1.5 0 1 1 13.3,13.9 C 13.3,12.8 14.8,11 14.8,11 Z";
    pub const CROP: &str = "M 5,1.5 L 5,13 L 16.5,13 M 1.5,5 L 13,5 L 13,16.5";
    pub const CUT_OUT: &str =
        "M 6,2 L 6,16 M 12,2 L 12,16 M 2,9 L 4.5,9 M 13.5,9 L 16,9 M 7.5,6.5 L 10.5,11.5 M 10.5,6.5 L 7.5,11.5";
    pub const LIFT_SELECTION: &str =
        "M 2.5,6 L 2.5,2.5 L 6,2.5 M 12,2.5 L 15.5,2.5 L 15.5,6 M 2.5,12 L 2.5,15.5 L 6,15.5 M 7.5,8.5 L 15.5,8.5 L 15.5,15.5 L 7.5,15.5 Z";
    pub const MAGIC_WAND: &str =
        "M 3,15 L 12.5,5.5 M 10.2,7.8 L 12.5,5.5 M 14,2 L 14,5.2 M 12.4,3.6 L 15.6,3.6 M 8.8,2.2 L 8.8,4.6 M 7.6,3.4 L 10,3.4 M 14.6,8.2 L 14.6,10.6 M 13.4,9.4 L 15.8,9.4";
    pub const ERASER: &str =
        "M 10.5,3 L 15.5,8 L 8.5,15 L 3.5,15 L 2.5,11 Z M 6.5,7 L 11.5,12 M 8.5,15 L 16,15";
    pub const EYEDROPPER: &str =
        "M 12.5,2.5 L 15.5,5.5 L 13.5,7.5 L 10.5,4.5 Z M 9.5,5.5 L 12.5,8.5 M 11.5,7.5 L 5.5,13.5 L 2.5,15.5 L 4.5,12.5 L 10.5,6.5";
    pub const SMART_MOVE: &str =
        "M 9,2 L 9,16 M 2,9 L 16,9 M 6.8,4.2 L 9,2 L 11.2,4.2 M 6.8,13.8 L 9,16 L 11.2,13.8 M 4.2,6.8 L 2,9 L 4.2,11.2 M 13.8,6.8 L 16,9 L 13.8,11.2";
    pub const TEXT_REPLACE: &str =
        "M 2.5,4 L 8.5,4 M 5.5,4 L 5.5,13 M 8.5,8.5 L 15.5,8.5 M 13,6 L 15.5,8.5 L 13,11 M 15.5,13.5 L 8.5,13.5 M 11,11 L 8.5,13.5 L 11,16";
    pub const MORE: &str =
        "M 4,9 A 1.1,1.1 0 1 1 3.99,9 Z M 9,9 A 1.1,1.1 0 1 1 8.99,9 Z M 14,9 A 1.1,1.1 0 1 1 13.99,9 Z";

    // Chrome, feedback, and action icons (16×16 coordinate space).
    pub const CAPTURE_DOT: &str = "M 8,3.5 A 4.5,4.5 0 1 1 7.99,3.5 Z";
    pub const CREATE_PLUS: &str = "M 8,3 L 8,13 M 3,8 L 13,8";
    pub const PROPERTIES_SLIDERS: &str =
        "M 2.5,4.5 L 13.5,4.5 M 5.5,2.8 L 5.5,6.2 M 2.5,8 L 13.5,8 M 10.5,6.3 L 10.5,9.7 M 2.5,11.5 L 13.5,11.5 M 7,9.8 L 7,13.2";
    pub const EFFECTS_SPARKLE: &str =
        "M 8,2 L 9.4,6.6 L 14,8 L 9.4,9.4 L 8,14 L 6.6,9.4 L 2,8 L 6.6,6.6 Z";
    pub const COPY_SHEETS: &str =
        "M 5.5,5.5 L 13,5.5 L 13,13.5 L 5.5,13.5 Z M 3,10.5 L 3,3 L 10.5,3";
    pub const SHARE_NODE: &str =
        "M 11.5,2.5 A 1.8,1.8 0 1 1 11.49,2.5 Z M 4,6.2 A 1.8,1.8 0 1 1 3.99,6.2 Z M 11.5,10 A 1.8,1.8 0 1 1 11.49,10 Z M 5.6,7.1 L 9.9,4.9 M 5.6,8.9 L 9.9,11.1";
    pub const CHEVRON_DOWN: &str = "M 4.5,6.2 L 8,9.8 L 11.5,6.2";
    pub const UNDO: &str =
        "M 6,4.5 L 2.8,7.5 L 6,10.5 M 3.2,7.5 L 10,7.5 A 3.5,3.5 0 0 1 10,14.5 L 7.5,14.5";
    pub const REDO: &str =
        "M 10,4.5 L 13.2,7.5 L 10,10.5 M 12.8,7.5 L 6,7.5 A 3.5,3.5 0 0 0 6,14.5 L 8.5,14.5";
    pub const CLOSE: &str = "M 4,4 L 12,12 M 12,4 L 4,12";
    pub const EXTERNAL_LINK: &str =
        "M 9.5,3 L 13,3 L 13,6.5 M 13,3 L 7.5,8.5 M 11,9.5 L 11,13 L 3,13 L 3,5 L 6.5,5";

    // Toast severity icons (16×16 coordinate space).
    pub const STATUS_SUCCESS: &str =
        "M 8,1.5 A 6.5,6.5 0 1 1 7.99,1.5 Z M 5.2,8.2 L 7.1,10.1 L 11,5.9";
    pub const STATUS_INFO: &str =
        "M 8,1.5 A 6.5,6.5 0 1 1 7.99,1.5 Z M 8,7.2 L 8,11.2 M 8,4.8 L 8.01,4.8";
    pub const STATUS_WARNING: &str =
        "M 8,2 L 14.5,13.5 L 1.5,13.5 Z M 8,6.5 L 8,9.8 M 8,11.6 L 8.01,11.6";
    pub const STATUS_ERROR: &str =
        "M 8,1.5 A 6.5,6.5 0 1 1 7.99,1.5 Z M 5.7,5.7 L 10.3,10.3 M 10.3,5.7 L 5.7,10.3";
}
