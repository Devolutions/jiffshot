//! Synthesized mouse and keyboard input (`SendInput`) for the control
//! server. Pointer moves can glide so a recording shows the pointer travel
//! instead of jumping.

use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, VkKeyScanW, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    pub fn parse(name: &str) -> Option<Button> {
        match name.to_ascii_lowercase().as_str() {
            "left" => Some(Button::Left),
            "right" => Some(Button::Right),
            "middle" => Some(Button::Middle),
            _ => None,
        }
    }

    fn flags(self) -> (u32, u32) {
        match self {
            Button::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
            Button::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
            Button::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
        }
    }
}

fn send(inputs: &[INPUT]) -> bool {
    if inputs.is_empty() {
        return true;
    }
    // SAFETY: well-formed inputs, count and size passed alongside.
    let sent = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
    sent as usize == inputs.len()
}

fn mouse(flags: u32, data: i32) -> INPUT {
    INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 } } }
}

fn key(vk: u16, scan: u16, flags: u32) -> INPUT {
    INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } } }
}

/// An input event of our own: Windows lets only the process that sent the
/// last input bring a window to the front.
pub fn nudge() {
    send(&[mouse(MOUSEEVENTF_MOVE, 0)]);
}

pub fn cursor() -> (i32, i32) {
    let mut p = POINT { x: 0, y: 0 };
    // SAFETY: out-parameter only.
    unsafe { GetCursorPos(&mut p) };
    (p.x, p.y)
}

/// Moves the pointer to `to`, gliding (eased) over `glide` when non-zero.
pub fn move_to(to: (i32, i32), glide: Duration) {
    let from = cursor();
    if !glide.is_zero() && from != to {
        let started = Instant::now();
        loop {
            let t = (started.elapsed().as_secs_f32() / glide.as_secs_f32()).min(1.0);
            let e = if t < 0.5 { 2.0 * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(2) / 2.0 };
            let x = from.0 + ((to.0 - from.0) as f32 * e).round() as i32;
            let y = from.1 + ((to.1 - from.1) as f32 * e).round() as i32;
            // SAFETY: plain call.
            unsafe { SetCursorPos(x, y) };
            if t >= 1.0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(8));
        }
    }
    // SAFETY: plain call.
    unsafe { SetCursorPos(to.0, to.1) };
}

pub fn click(at: (i32, i32), button: Button, count: u32, glide: Duration) -> bool {
    move_to(at, glide);
    std::thread::sleep(Duration::from_millis(40));
    let (down, up) = button.flags();
    let mut ok = true;
    for i in 0..count.max(1) {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        ok &= send(&[mouse(down, 0)]);
        std::thread::sleep(Duration::from_millis(30));
        ok &= send(&[mouse(up, 0)]);
    }
    ok
}

pub fn drag(from: (i32, i32), to: (i32, i32), button: Button, glide: Duration) -> bool {
    move_to(from, glide / 2);
    std::thread::sleep(Duration::from_millis(60));
    let (down, up) = button.flags();
    let mut ok = send(&[mouse(down, 0)]);
    std::thread::sleep(Duration::from_millis(80));
    // A small first step, so the target sees a drag start.
    move_to((from.0 + (to.0 - from.0).signum() * 4, from.1 + (to.1 - from.1).signum() * 4), Duration::ZERO);
    std::thread::sleep(Duration::from_millis(40));
    move_to(to, glide.max(Duration::from_millis(250)));
    std::thread::sleep(Duration::from_millis(80));
    ok &= send(&[mouse(up, 0)]);
    ok
}

/// Wheel `notches` (positive scrolls up, or right when `horizontal`).
pub fn scroll(notches: i32, horizontal: bool) -> bool {
    let flag = if horizontal { MOUSEEVENTF_HWHEEL } else { MOUSEEVENTF_WHEEL };
    let mut ok = true;
    for _ in 0..notches.unsigned_abs() {
        ok &= send(&[mouse(flag, notches.signum() * 120)]);
        std::thread::sleep(Duration::from_millis(30));
    }
    ok
}

/// Types `text` as Unicode characters (newlines and tabs as their keys),
/// waiting `delay` between characters.
pub fn type_text(text: &str, delay: Duration) -> bool {
    let mut ok = true;
    for ch in text.chars() {
        ok &= match ch {
            '\n' => tap(&[], 0x0D),
            '\r' => true,
            '\t' => tap(&[], 0x09),
            _ => {
                let mut units = [0u16; 2];
                let mut inputs = Vec::new();
                for &u in ch.encode_utf16(&mut units).iter() {
                    inputs.push(key(0, u, KEYEVENTF_UNICODE));
                    inputs.push(key(0, u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
                }
                send(&inputs)
            }
        };
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
    }
    ok
}

fn extended(vk: u16) -> bool {
    matches!(vk, 0x21..=0x28 | 0x2D | 0x2E | 0x5B | 0x5C | 0x5D | 0x6F | 0x90 | 0xA3 | 0xA5)
}

fn tap(modifiers: &[u16], vk: u16) -> bool {
    let flags = |vk: u16| if extended(vk) { KEYEVENTF_EXTENDEDKEY } else { 0 };
    let mut inputs: Vec<INPUT> = modifiers.iter().map(|&m| key(m, 0, flags(m))).collect();
    inputs.push(key(vk, 0, flags(vk)));
    inputs.push(key(vk, 0, flags(vk) | KEYEVENTF_KEYUP));
    inputs.extend(modifiers.iter().rev().map(|&m| key(m, 0, flags(m) | KEYEVENTF_KEYUP)));
    send(&inputs)
}

fn named_key(name: &str) -> Option<u16> {
    Some(match name {
        "ctrl" | "control" => 0x11,
        "shift" => 0x10,
        "alt" | "menu" => 0x12,
        "win" | "windows" | "meta" | "super" => 0x5B,
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "esc" | "escape" => 0x1B,
        "space" => 0x20,
        "backspace" => 0x08,
        "delete" | "del" => 0x2E,
        "insert" | "ins" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" => 0x22,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "apps" | "contextmenu" => 0x5D,
        "printscreen" | "prtsc" => 0x2C,
        "capslock" => 0x14,
        "plus" => 0xBB,
        "minus" => 0xBD,
        _ => {
            let n = name.strip_prefix('f')?.parse::<u16>().ok()?;
            return (1..=24).contains(&n).then_some(0x70 + n - 1);
        }
    })
}

/// A key combination such as `ctrl+shift+s`, `alt+f4` or `enter`: modifier
/// keys plus one key.
pub fn parse_combo(combo: &str) -> Result<(Vec<u16>, u16), String> {
    let parts: Vec<String> = combo.split('+').map(|p| p.trim().to_ascii_lowercase()).collect();
    if parts.iter().any(String::is_empty) {
        return Err(format!("bad key combination {combo:?}"));
    }
    let mut modifiers = Vec::new();
    let mut main = 0;
    for (i, part) in parts.iter().enumerate() {
        let vk = match named_key(part) {
            Some(vk) => vk,
            None => {
                let mut chars = part.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) if c.is_ascii_alphanumeric() => c.to_ascii_uppercase() as u16,
                    (Some(c), None) => {
                        // SAFETY: plain call.
                        let scan = unsafe { VkKeyScanW(c as u16) };
                        if scan == -1 {
                            return Err(format!("no key types {c:?}"));
                        }
                        (scan as u16) & 0xFF
                    }
                    _ => return Err(format!("unknown key {part:?} in {combo:?}")),
                }
            }
        };
        if i + 1 < parts.len() {
            if !matches!(vk, 0x10 | 0x11 | 0x12 | 0x5B) {
                return Err(format!("{part:?} is not a modifier (ctrl, shift, alt, win) in {combo:?}"));
            }
            modifiers.push(vk);
        } else {
            main = vk;
        }
    }
    Ok((modifiers, main))
}

pub fn press(combo: &str, repeat: u32) -> Result<bool, String> {
    let (modifiers, vk) = parse_combo(combo)?;
    let mut ok = true;
    for i in 0..repeat.max(1) {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(40));
        }
        ok &= tap(&modifiers, vk);
    }
    Ok(ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combos_parse_to_modifiers_and_a_key() {
        assert_eq!(parse_combo("ctrl+shift+s").unwrap(), (vec![0x11, 0x10], b'S' as u16));
        assert_eq!(parse_combo("Alt+F4").unwrap(), (vec![0x12], 0x73));
        assert_eq!(parse_combo("enter").unwrap(), (vec![], 0x0D));
        assert_eq!(parse_combo("win+up").unwrap(), (vec![0x5B], 0x26));
        assert!(parse_combo("ctrl+").is_err());
        assert!(parse_combo("s+ctrl").is_err());
        assert!(parse_combo("ctrl+nosuchkey").is_err());
    }
}
