//! Devolutions DarkBlue colors used by Jiffshot's own Windows surfaces.

pub const WINDOW: u32 = 0xFF161B26;
pub const CHROME: u32 = 0xFF0C111D;
pub const PANEL: u32 = 0xFF1C222E;
pub const CARD: u32 = 0xFF212835;
pub const HOVER: u32 = 0xFF272E3C;
pub const BORDER: u32 = 0xFF2A2E39;
pub const INPUT_BORDER: u32 = 0xFF2F3441;
pub const STRONG_BORDER: u32 = 0xFF383F4E;
pub const TEXT: u32 = 0xFFFFFFFF;
pub const SECONDARY_TEXT: u32 = 0xFFC2C3C6;
pub const MUTED_TEXT: u32 = 0xFF96989D;
pub const ACCENT: u32 = 0xFF0A7FE6;
pub const ACCENT_TEXT: u32 = 0xFF3AA3FF;
pub const SELECTION: u32 = 0xFF21374F;
pub const TOAST: u32 = 0xFF2A3346;

/// Keeps the existing non-Windows palette when a surface is shared across platforms.
pub const fn windows_or(windows: u32, other: u32) -> u32 {
    if cfg!(windows) { windows } else { other }
}
