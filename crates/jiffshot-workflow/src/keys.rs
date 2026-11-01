//! The global hotkeys each system gets by default, and how keys are written.
//!
//! Windows uses Print Screen. macOS keeps Command+Shift+3, 4 and 5 for its
//! own screenshots, so Jiffshot takes Command+Shift+2 to capture, with
//! Command+Shift+1 for the full screen and Command+Shift+7 to repeat.

#[allow(unused_imports)]
use crate::sys::{HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN, VK_F11, VK_SNAPSHOT};

/// A hotkey: modifiers, virtual key and how it is written.
pub type Key = (HOT_KEY_MODIFIERS, u16, &'static str);

/// The capture key.
#[cfg(windows)]
pub const CAPTURE: Key = (0, VK_SNAPSHOT, "PrtScn");
#[cfg(target_os = "macos")]
pub const CAPTURE: Key = (MOD_WIN | MOD_SHIFT, b'2' as u16, "Cmd+Shift+2");

/// Capture keys to try when [`CAPTURE`] is taken, in order.
#[cfg(windows)]
pub const CAPTURE_FALLBACKS: &[Key] = &[
    (MOD_CONTROL | MOD_SHIFT, b'X' as u16, "Ctrl+Shift+X"),
    (MOD_CONTROL | MOD_ALT, VK_SNAPSHOT, "Ctrl+Alt+PrtScn"),
    (MOD_CONTROL | MOD_SHIFT, VK_F11, "Ctrl+Shift+F11"),
];
#[cfg(target_os = "macos")]
pub const CAPTURE_FALLBACKS: &[Key] = &[
    (MOD_CONTROL | MOD_WIN | MOD_SHIFT, b'2' as u16, "Ctrl+Cmd+Shift+2"),
    (MOD_ALT | MOD_WIN | MOD_SHIFT, b'2' as u16, "Alt+Cmd+Shift+2"),
    (MOD_CONTROL | MOD_SHIFT, VK_F11, "Ctrl+Shift+F11"),
];

/// Captures the full screen.
#[cfg(windows)]
pub const SCREEN: Key = (MOD_CONTROL, VK_SNAPSHOT, "Ctrl+PrtScn");
#[cfg(target_os = "macos")]
pub const SCREEN: Key = (MOD_WIN | MOD_SHIFT, b'1' as u16, "Cmd+Shift+1");

/// Repeats the last region.
#[cfg(windows)]
pub const REPEAT: Key = (MOD_SHIFT, VK_SNAPSHOT, "Shift+PrtScn");
#[cfg(target_os = "macos")]
pub const REPEAT: Key = (MOD_WIN | MOD_SHIFT, b'7' as u16, "Cmd+Shift+7");

/// The modifier of command shortcuts ("Ctrl+1"), as written in hotkeys.
#[cfg(windows)]
pub const COMMAND: &str = "Ctrl";
#[cfg(target_os = "macos")]
pub const COMMAND: &str = "Cmd";

/// An example preset hotkey for hints.
#[cfg(windows)]
pub const EXAMPLE: &str = "Ctrl+Shift+1";
#[cfg(target_os = "macos")]
pub const EXAMPLE: &str = "Ctrl+Cmd+1";

/// Parses "Ctrl+Shift+1" (or "Cmd+Shift+2", "Option+F9", "PrtScn") into
/// hotkey modifiers and a virtual key.
pub fn parse(text: &str) -> Option<(HOT_KEY_MODIFIERS, u16)> {
    let mut mods = 0;
    let mut key = None;
    for part in text.split('+').map(|p| p.trim().to_ascii_lowercase()) {
        match part.as_str() {
            "ctrl" | "control" | "\u{2303}" => mods |= MOD_CONTROL,
            "shift" | "\u{21E7}" => mods |= MOD_SHIFT,
            "alt" | "option" | "opt" | "\u{2325}" => mods |= MOD_ALT,
            "win" | "cmd" | "command" | "\u{2318}" => mods |= MOD_WIN,
            k => {
                let vk = match k {
                    "prtscn" | "printscreen" | "print screen" => VK_SNAPSHOT,
                    k if k.len() == 1 && k.chars().next().unwrap().is_ascii_alphanumeric() => k.to_ascii_uppercase().as_bytes()[0] as u16,
                    k if k.starts_with('f') && k[1..].parse::<u16>().is_ok_and(|n| (1..=24).contains(&n)) => 0x6F + k[1..].parse::<u16>().unwrap(),
                    _ => return None,
                };
                if key.replace(vk).is_some() {
                    return None;
                }
            }
        }
    }
    key.map(|k| (mods, k))
}

/// Apple's built-in screenshot shortcuts must remain available.
pub fn reserved(modifiers: HOT_KEY_MODIFIERS, key: u16) -> bool {
    #[cfg(target_os = "macos")]
    {
        modifiers & (MOD_WIN | MOD_SHIFT) == MOD_WIN | MOD_SHIFT && (b'3' as u16..=b'5' as u16).contains(&key)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (modifiers, key);
        false
    }
}

pub fn supported(key: u16) -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::sys::mac_key_code(key).is_some()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = key;
        true
    }
}

/// How a hotkey reads to the user: "Print Screen" on Windows; the menu
/// symbols on macOS ("Cmd+Shift+2" reads ⇧⌘2).
pub fn display(key: &str) -> String {
    #[cfg(windows)]
    {
        key.replace("PrtScn", "Print Screen")
    }
    #[cfg(target_os = "macos")]
    {
        let parts: Vec<String> = key.split('+').map(|p| p.trim().to_ascii_lowercase()).collect();
        if parts.len() < 2 && !parts.iter().any(|p| p == "prtscn") {
            return key.to_owned();
        }
        let mut symbols = String::new();
        for (names, symbol) in [
            (&["ctrl", "control", "\u{2303}"][..], '\u{2303}'),
            (&["alt", "option", "opt", "\u{2325}"][..], '\u{2325}'),
            (&["shift", "\u{21E7}"][..], '\u{21E7}'),
            (&["cmd", "command", "win", "\u{2318}"][..], '\u{2318}'),
        ] {
            if parts.iter().any(|p| names.contains(&p.as_str())) {
                symbols.push(symbol);
            }
        }
        let last = parts.last().map_or("", String::as_str);
        let name = if last == "prtscn" { "F13".to_owned() } else { last.to_ascii_uppercase() };
        format!("{symbols}{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_displays() {
        assert_eq!(parse("Ctrl+Shift+1"), Some((MOD_CONTROL | MOD_SHIFT, b'1' as u16)));
        assert_eq!(parse("alt + F9"), Some((MOD_ALT, 0x78)));
        assert_eq!(parse("Cmd+Shift+2"), Some((MOD_WIN | MOD_SHIFT, b'2' as u16)));
        assert_eq!(parse("Option+X"), Some((MOD_ALT, b'X' as u16)));
        assert_eq!(parse("PrtScn"), Some((0, VK_SNAPSHOT)));
        assert_eq!(parse("Ctrl+"), None);
        assert_eq!(parse("Hyper+K"), None);
        assert_eq!(parse("Ctrl+A+B"), None);
        for key in [CAPTURE, SCREEN, REPEAT].iter().chain(CAPTURE_FALLBACKS) {
            assert_eq!(parse(key.2), Some((key.0, key.1)), "{}", key.2);
        }
        #[cfg(windows)]
        assert_eq!(display("Ctrl+PrtScn"), "Ctrl+Print Screen");
        #[cfg(target_os = "macos")]
        {
            assert_eq!(display("Cmd+Shift+2"), "\u{21E7}\u{2318}2");
            assert_eq!(display("⌘+Option+2"), "\u{2325}\u{2318}2");
            for key in b'3'..=b'5' {
                assert!(reserved(MOD_WIN | MOD_SHIFT, u16::from(key)));
                assert!(reserved(MOD_WIN | MOD_SHIFT | MOD_CONTROL, u16::from(key)));
            }
            assert!(!reserved(MOD_WIN | MOD_SHIFT, b'2' as u16));
        }
    }
}
