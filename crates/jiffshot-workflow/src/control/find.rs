//! Finding things on screen for the control server: top-level windows (and
//! bringing one to the front), text by OCR, and controls by UI Automation.

use jiffshot_capture::{ocr, screen, targets, Rect};
use serde::Serialize;
use std::time::{Duration, Instant};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation, IUIAutomationElement, TreeScope_Descendants};
use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GetClassNameW, GetForegroundWindow, GetWindowLongW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId,
    IsIconic, IsWindow, IsWindowVisible, SetForegroundWindow, ShowWindow, GWL_EXSTYLE, SW_RESTORE, WS_EX_TOOLWINDOW,
};

#[derive(Clone, Debug, Serialize)]
pub struct Window {
    /// The window handle, as a number.
    pub hwnd: isize,
    pub title: String,
    pub class: String,
    pub pid: u32,
    /// Executable file name, e.g. `notepad.exe`.
    pub process: String,
    /// On-screen bounds (without the invisible resize border).
    pub rect: Rect,
    pub minimized: bool,
    pub foreground: bool,
}

/// What to look for among top-level windows.
#[derive(Default)]
pub struct Query<'a> {
    pub hwnd: Option<isize>,
    /// Substring of the title, any case.
    pub title: Option<&'a str>,
    /// Executable name, any case, `.exe` optional.
    pub process: Option<&'a str>,
    pub class: Option<&'a str>,
    /// Untitled, tool and our own windows too.
    pub all: bool,
}

impl Query<'_> {
    pub fn is_empty(&self) -> bool {
        self.hwnd.is_none() && self.title.is_none() && self.process.is_none() && self.class.is_none()
    }

    fn matches(&self, w: &Window) -> bool {
        let process = |p: &str| {
            let p = p.to_ascii_lowercase();
            let name = w.process.to_ascii_lowercase();
            name == p || name.strip_suffix(".exe") == Some(p.as_str())
        };
        self.hwnd.is_none_or(|h| h == w.hwnd)
            && self.title.is_none_or(|t| w.title.to_lowercase().contains(&t.to_lowercase()))
            && self.process.is_none_or(process)
            && self.class.is_none_or(|c| w.class.eq_ignore_ascii_case(c))
    }
}

pub fn bounds(hwnd: HWND) -> Rect {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: out-parameters of the right size.
    unsafe {
        if DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS as u32, &mut r as *mut RECT as *mut _, std::mem::size_of::<RECT>() as u32) != 0 {
            GetWindowRect(hwnd, &mut r);
        }
    }
    Rect::from_ltrb(r.left, r.top, r.right, r.bottom)
}

fn describe(hwnd: HWND, foreground: HWND) -> Window {
    let mut title = [0u16; 512];
    let mut class = [0u16; 256];
    let mut pid = 0;
    // SAFETY: buffers with their sizes; out-parameter.
    let (tlen, clen, minimized) = unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
        (GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32), GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32), IsIconic(hwnd) != 0)
    };
    Window {
        hwnd: hwnd as isize,
        title: String::from_utf16_lossy(&title[..tlen.max(0) as usize]),
        class: String::from_utf16_lossy(&class[..clen.max(0) as usize]),
        pid,
        process: targets::process_name(pid).unwrap_or_default(),
        rect: bounds(hwnd),
        minimized,
        foreground: hwnd == foreground,
    }
}

/// Visible top-level windows matching `query`, front to back.
pub fn windows(query: &Query) -> Vec<Window> {
    unsafe extern "system" fn each(hwnd: HWND, list: LPARAM) -> BOOL {
        (*(list as *mut Vec<HWND>)).push(hwnd);
        1
    }
    let mut handles: Vec<HWND> = Vec::new();
    // SAFETY: the callback only pushes into `handles`, which outlives the call.
    unsafe { EnumWindows(Some(each), &mut handles as *mut _ as LPARAM) };
    // SAFETY: plain calls.
    let (own, foreground) = unsafe { (GetCurrentProcessId(), GetForegroundWindow()) };
    handles
        .into_iter()
        .filter(|&hwnd| {
            // SAFETY: plain reads of a window that may since have closed.
            unsafe {
                if IsWindowVisible(hwnd) == 0 {
                    return false;
                }
                let mut cloaked = 0u32;
                DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED as u32, &mut cloaked as *mut u32 as *mut _, 4);
                cloaked == 0
            }
        })
        .map(|hwnd| describe(hwnd, foreground))
        .filter(|w| {
            if query.all || query.hwnd.is_some() {
                return true;
            }
            // SAFETY: plain read.
            let tool = unsafe { GetWindowLongW(w.hwnd as HWND, GWL_EXSTYLE) } as u32 & WS_EX_TOOLWINDOW != 0;
            !w.title.is_empty() && !tool && w.pid != own && w.rect.width > 0 && w.rect.height > 0
        })
        .filter(|w| query.matches(w))
        .collect()
}

pub fn alive(hwnd: isize) -> bool {
    // SAFETY: plain call.
    unsafe { IsWindow(hwnd as HWND) != 0 }
}

/// Restores and brings `hwnd` to the front, then gives it a moment to
/// paint. Returns whether it is the foreground window.
pub fn activate(hwnd: isize) -> bool {
    let hwnd = hwnd as HWND;
    // SAFETY: plain calls on a window handle (harmless if it has closed).
    unsafe {
        if GetForegroundWindow() == hwnd && IsIconic(hwnd) == 0 {
            return true;
        }
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        super::input::nudge();
        SetForegroundWindow(hwnd);
        BringWindowToTop(hwnd);
    }
    let started = Instant::now();
    // SAFETY: plain call.
    while unsafe { GetForegroundWindow() } != hwnd && started.elapsed() < Duration::from_millis(1500) {
        std::thread::sleep(Duration::from_millis(25));
        // SAFETY: as above.
        unsafe { SetForegroundWindow(hwnd) };
    }
    // Restore animations and the first repaint.
    std::thread::sleep(Duration::from_millis(250));
    // SAFETY: plain call.
    unsafe { GetForegroundWindow() == hwnd }
}

#[derive(Clone, Debug, Serialize)]
pub struct Text {
    pub text: String,
    /// Screen rectangle.
    pub rect: Rect,
    /// Its center, where a click goes.
    pub center: (i32, i32),
}

fn text(text: String, rect: Rect) -> Text {
    Text { center: (rect.x + rect.width / 2, rect.y + rect.height / 2), text, rect }
}

fn union(a: Rect, b: Rect) -> Rect {
    Rect::from_ltrb(a.x.min(b.x), a.y.min(b.y), a.right().max(b.right()), a.bottom().max(b.bottom()))
}

fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Lines of text recognized in `area` (screen coordinates).
pub fn read(area: Rect) -> Result<Vec<ocr::Line>, String> {
    let frame = screen::grab(area).map_err(|e| e.to_string())?;
    let mut lines = ocr::recognize(frame.width, frame.height, &frame.pixels)?;
    let shift = |r: &mut Rect| {
        r.x += area.x;
        r.y += area.y;
    };
    for line in &mut lines {
        shift(&mut line.rect);
        line.words.iter_mut().for_each(|w| shift(&mut w.rect));
    }
    Ok(lines)
}

pub fn lines(lines: &[ocr::Line]) -> Vec<Text> {
    lines.iter().map(|l| text(l.text.clone(), l.rect)).collect()
}

/// Where `phrase` appears (any case and spacing): the smallest run of words
/// on a line that contains it, in reading order.
pub fn find_text(lines: &[ocr::Line], phrase: &str) -> Vec<Text> {
    let phrase = normalize(phrase);
    let mut found = Vec::new();
    if phrase.is_empty() {
        return found;
    }
    for line in lines {
        if !normalize(&line.text).contains(&phrase) {
            continue;
        }
        let words = &line.words;
        let mut best: Option<(usize, usize)> = None;
        for start in 0..words.len() {
            let mut joined = String::new();
            for end in start..words.len() {
                if end > start {
                    joined.push(' ');
                }
                joined.push_str(&words[end].text);
                if normalize(&joined).contains(&phrase) {
                    if best.is_none_or(|(s, e)| end - start < e - s) {
                        best = Some((start, end));
                    }
                    break;
                }
            }
        }
        match best {
            Some((s, e)) => {
                let rect = words[s + 1..=e].iter().fold(words[s].rect, |r, w| union(r, w.rect));
                let t = words[s..=e].iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
                found.push(text(t, rect));
            }
            // Joined differently than the words (e.g. punctuation): the line.
            None => found.push(text(line.text.clone(), line.rect)),
        }
    }
    found
}

#[derive(Clone, Debug, Serialize)]
pub struct Element {
    pub name: String,
    /// Localized control type, e.g. `button`.
    pub control_type: String,
    pub automation_id: String,
    pub rect: Rect,
    pub center: (i32, i32),
    pub enabled: bool,
}

fn automation() -> Result<IUIAutomation, String> {
    // SAFETY: COM is initialized on the calling (connection) thread.
    unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }.map_err(|e| format!("UI Automation is unavailable: {e}"))
}

fn element(e: &IUIAutomationElement) -> Option<Element> {
    // SAFETY: plain property reads on a live element.
    unsafe {
        if e.CurrentIsOffscreen().map(|b| b.as_bool()).unwrap_or(false) {
            return None;
        }
        let r = e.CurrentBoundingRectangle().ok()?;
        let rect = Rect::from_ltrb(r.left, r.top, r.right, r.bottom);
        if rect.is_empty() {
            return None;
        }
        Some(Element {
            name: e.CurrentName().map(|s| s.to_string()).unwrap_or_default(),
            control_type: e.CurrentLocalizedControlType().map(|s| s.to_string()).unwrap_or_default(),
            automation_id: e.CurrentAutomationId().map(|s| s.to_string()).unwrap_or_default(),
            center: (rect.x + rect.width / 2, rect.y + rect.height / 2),
            rect,
            enabled: e.CurrentIsEnabled().map(|b| b.as_bool()).unwrap_or(true),
        })
    }
}

/// The on-screen controls inside window `hwnd` (at most `max`).
pub fn elements(hwnd: isize, max: usize) -> Result<Vec<Element>, String> {
    let automation = automation()?;
    // SAFETY: COM calls on objects we hold.
    unsafe {
        let root = automation.ElementFromHandle(windows::Win32::Foundation::HWND(hwnd as *mut _)).map_err(|e| e.to_string())?;
        let all = automation.CreateTrueCondition().map_err(|e| e.to_string())?;
        let found = root.FindAll(TreeScope_Descendants, &all).map_err(|e| e.to_string())?;
        let n = found.Length().unwrap_or(0).max(0) as usize;
        let mut out = Vec::new();
        for i in 0..n {
            if out.len() >= max {
                break;
            }
            if let Some(e) = found.GetElement(i as i32).ok().as_ref().and_then(element) {
                out.push(e);
            }
        }
        Ok(out)
    }
}

/// Controls named `name` (exactly, any case; else containing it), of
/// `control_type` if given.
pub fn find_elements(all: Vec<Element>, name: &str, control_type: Option<&str>) -> Vec<Element> {
    let name = normalize(name);
    let typed: Vec<Element> = all.into_iter().filter(|e| control_type.is_none_or(|t| e.control_type.eq_ignore_ascii_case(t))).collect();
    let exact: Vec<Element> = typed.iter().filter(|e| normalize(&e.name) == name).cloned().collect();
    if !exact.is_empty() {
        return exact;
    }
    typed.into_iter().filter(|e| !name.is_empty() && normalize(&e.name).contains(&name)).collect()
}

/// UI Automation's "Click the Save button" for the control at `p`.
pub fn describe_click(p: (i32, i32), right: bool) -> String {
    crate::stepcap::describe(automation().ok().as_ref(), p, right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiffshot_capture::ocr::{Line, Word};

    fn line(words: &[(&str, i32)]) -> Line {
        let words: Vec<Word> = words.iter().map(|&(t, x)| Word { rect: Rect::new(x, 10, 40, 12), text: t.into() }).collect();
        let text = words.iter().map(|w| w.text.clone()).collect::<Vec<_>>().join(" ");
        let rect = Rect::from_ltrb(words[0].rect.x, 10, words.last().unwrap().rect.right(), 22);
        Line { rect, text, words }
    }

    #[test]
    fn text_is_found_as_the_smallest_run_of_words() {
        let lines = [line(&[("File", 0), ("Edit", 50), ("Save", 100), ("As", 150)]), line(&[("nothing", 0)])];
        let found = find_text(&lines, "save  as");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text, "Save As");
        assert_eq!(found[0].rect, Rect::from_ltrb(100, 10, 190, 22));
        assert_eq!(found[0].center, (145, 16));
        assert!(find_text(&lines, "print").is_empty());
        assert!(find_text(&lines, "").is_empty());
    }

    #[test]
    fn elements_match_exact_names_before_partial_ones() {
        let e = |name: &str, t: &str| Element { name: name.into(), control_type: t.into(), automation_id: String::new(), rect: Rect::new(0, 0, 10, 10), center: (5, 5), enabled: true };
        let all = vec![e("Save as", "button"), e("Save", "button"), e("Save", "menu item")];
        assert_eq!(find_elements(all.clone(), "save", None).len(), 2);
        assert_eq!(find_elements(all.clone(), "save", Some("Button")).len(), 1);
        assert_eq!(find_elements(all, "save a", None)[0].name, "Save as");
    }

    #[test]
    fn window_queries_match_title_process_and_class() {
        let w = Window { hwnd: 1, title: "Untitled - Notepad".into(), class: "Notepad".into(), pid: 1, process: "Notepad.exe".into(), rect: Rect::new(0, 0, 1, 1), minimized: false, foreground: false };
        assert!(Query { title: Some("notepad"), ..Query::default() }.matches(&w));
        assert!(Query { process: Some("notepad"), ..Query::default() }.matches(&w));
        assert!(Query { process: Some("NOTEPAD.EXE"), class: Some("notepad"), ..Query::default() }.matches(&w));
        assert!(!Query { process: Some("note"), ..Query::default() }.matches(&w));
        assert!(!Query { hwnd: Some(2), ..Query::default() }.matches(&w));
    }
}
