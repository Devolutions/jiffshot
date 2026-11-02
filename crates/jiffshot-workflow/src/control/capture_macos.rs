use super::{find, internal, invalid, not_found, point_json, Args, Reply};
use crate::geometry::CaptureLayout;
use jiffshot_capture::library::{CaptureKind, Library};
use jiffshot_capture::ocr::{self, Line};
use jiffshot_capture::screen::{self, DisplayLayout, Monitor};
use jiffshot_capture::Rect;
use jiffshot_control::{Error, ErrorCode};
use serde::Serialize;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub(super) const TARGET: &[&str] = &["screen", "desktop", "region", "window", "title", "process", "class", "no_activate"];

pub(super) struct Target {
    pub(super) rect: Rect,
    pub(super) kind: CaptureKind,
    pub(super) layout: CaptureLayout,
    pub(super) window: Option<jiffshot_capture::targets::Window>,
}

impl Target {
    pub(super) fn json(&self) -> Value {
        json!({ "rect": self.rect, "kind": self.kind, "window": self.window })
    }
}

#[derive(Debug, PartialEq, Eq)]
enum MonitorSpec {
    Primary,
    Cursor,
    Index(u64),
}

fn monitor_spec(value: &Value) -> Result<MonitorSpec, Error> {
    match value {
        Value::Bool(true) => Ok(MonitorSpec::Primary),
        Value::String(value) if value.eq_ignore_ascii_case("primary") => Ok(MonitorSpec::Primary),
        Value::String(value) if value.eq_ignore_ascii_case("cursor") => Ok(MonitorSpec::Cursor),
        _ => {
            let index = match value {
                Value::Number(value) => value.as_u64(),
                Value::String(value) => value.trim().parse().ok(),
                _ => None,
            };
            index.map(MonitorSpec::Index)
                .ok_or_else(|| invalid("screen must be a display index (from `monitors`), primary or cursor"))
        }
    }
}

fn display_target(value: Option<&Value>, default_desktop: bool) -> Result<Option<MonitorSpec>, Error> {
    match value.filter(|value| !value.is_null()) {
        Some(value) => monitor_spec(value).map(Some),
        None if default_desktop => Ok(None),
        None => Ok(Some(MonitorSpec::Primary)),
    }
}

fn monitor(layout: &DisplayLayout, spec: MonitorSpec) -> Result<Monitor, Error> {
    let monitors = layout.monitors();
    match spec {
        MonitorSpec::Primary => monitors.iter().find(|monitor| monitor.primary).or(monitors.first())
            .cloned().ok_or_else(|| internal("no displays")),
        MonitorSpec::Cursor => layout.monitor_under_cursor().map_err(internal),
        MonitorSpec::Index(index) => usize::try_from(index).ok()
            .and_then(|index| monitors.get(index)).cloned()
            .ok_or_else(|| not_found(format!("no display {index}; there are {}", monitors.len()))),
    }
}

fn clip(rect: Rect, desktop: Rect) -> Result<Rect, Error> {
    if rect.is_empty() || rect.x.checked_add(rect.width).is_none() || rect.y.checked_add(rect.height).is_none() {
        return Err(invalid(format!("rectangle must have positive dimensions and valid coordinates, not {rect:?}")));
    }
    let left = i64::from(rect.x).max(i64::from(desktop.x));
    let top = i64::from(rect.y).max(i64::from(desktop.y));
    let right = (i64::from(rect.x) + i64::from(rect.width))
        .min(i64::from(desktop.x) + i64::from(desktop.width));
    let bottom = (i64::from(rect.y) + i64::from(rect.height))
        .min(i64::from(desktop.y) + i64::from(desktop.height));
    if right <= left || bottom <= top {
        return Err(invalid(format!("{rect:?} is off screen")));
    }
    Ok(Rect::new(left as i32, top as i32, (right - left) as i32, (bottom - top) as i32))
}

fn clip_region(text: &str, desktop: Rect) -> Result<Rect, Error> {
    let rect = crate::parse_rect(text).ok_or_else(|| invalid(format!("region must be X,Y,W,H, not {text:?}")))?;
    clip(rect, desktop)
}

fn desktop_target(display: DisplayLayout) -> Result<Target, Error> {
    let rect = display.virtual_screen();
    let layout = CaptureLayout::from_display(display).with_desktop().map_err(internal)?;
    Ok(Target { rect, kind: CaptureKind::Desktop, layout, window: None })
}

pub(super) fn target(a: &Args, default_desktop: bool) -> Result<Target, Error> {
    let display = DisplayLayout::current().map_err(internal)?;
    let desktop = display.virtual_screen();
    if let Some(region) = a.string("region")? {
        return Ok(Target { rect: clip_region(&region, desktop)?, kind: CaptureKind::Region, layout: CaptureLayout::from_display(display), window: None });
    }
    if a.bool("desktop")? {
        return desktop_target(display);
    }
    if find::specified(a) {
        let window = find::selected(a, &display, true)?;
        if !a.bool("no_activate")? && !window.foreground {
            return Err(Error::new(ErrorCode::Conflict,
                "the selected window could not be brought to the front; use no_activate to capture the visible screen"));
        }
        let rect = clip(window.rect, desktop)?;
        let (retained_rect, bounds) = window.clipped_to_layout(&display).map_err(internal)?;
        if retained_rect != rect {
            return Err(internal("the selected window geometry changed"));
        }
        let layout = CaptureLayout::from_display(display).with_logical_bounds(rect, bounds).map_err(internal)?;
        return Ok(Target { rect, kind: CaptureKind::Window, layout, window: Some(window) });
    }
    let Some(spec) = display_target(a.0.get("screen"), default_desktop)? else {
        return desktop_target(display);
    };
    let selected = monitor(&display, spec)?;
    let rect = selected.rect;
    let layout = CaptureLayout::from_display(display).with_monitor(&selected).map_err(internal)?;
    Ok(Target { rect, kind: CaptureKind::Monitor, layout, window: None })
}

pub(super) fn wait(delay: Duration, cancelled: impl Fn() -> bool) -> Result<(), Error> {
    let start = Instant::now();
    loop {
        if cancelled() {
            return Err(Error::new(ErrorCode::Conflict, "Jiffshot is shutting down"));
        }
        let remaining = delay.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Ok(());
        }
        std::thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

pub(super) fn cursor(a: &Args) -> Reply {
    a.only(&[])?;
    Ok(point_json(screen::try_cursor_pos().map_err(internal)?))
}

pub(super) fn screenshot(a: &Args) -> Reply {
    let mut allowed = TARGET.to_vec();
    allowed.extend_from_slice(&["out", "cursor", "delay_ms"]);
    a.only(&allowed)?;
    let out = a.out("out", &["png"])?;
    let cursor = a.bool("cursor")?;
    let delay = Duration::from_millis(a.int("delay_ms", 0, 3_600_000)?.unwrap_or(0) as u64);
    wait(delay, crate::shutdown_macos::requested)?;
    let target = target(a, false)?;
    let mut capture = crate::workflow::immediate_in_layout(target.kind, target.rect, cursor, &target.layout).map_err(internal)?;
    if let Some(window) = &target.window {
        capture.meta.window_title = Some(window.title.clone());
        capture.meta.process = Some(window.process.clone());
    }
    let path = match out {
        Some(path) => {
            capture.frame.save_png(&path).map_err(internal)?;
            path
        }
        None => Library::new(jiffshot_capture::config::library_root())
            .save(&capture.frame, &capture.meta).map_err(internal)?,
    };
    Ok(json!({
        "path": path,
        "width": capture.frame.width,
        "height": capture.frame.height,
        "captured_at": capture.meta.captured_at,
        "target": target.json(),
    }))
}

#[derive(Debug, Serialize)]
pub(super) struct Text {
    pub(super) text: String,
    pub(super) rect: Rect,
    pub(super) center: (i32, i32),
}

fn text(text: String, rect: Rect) -> Text {
    Text { text, rect, center: rect.center() }
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

pub(super) fn find_text(lines: &[Line], phrase: &str) -> Vec<Text> {
    let phrase = normalize(phrase);
    if phrase.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for line in lines {
        if !normalize(&line.text).contains(&phrase) {
            continue;
        }
        let mut best: Option<(usize, usize)> = None;
        for start in 0..line.words.len() {
            let mut joined = String::new();
            for end in start..line.words.len() {
                if end > start {
                    joined.push(' ');
                }
                joined.push_str(&line.words[end].text);
                if normalize(&joined).contains(&phrase) {
                    if best.is_none_or(|(first, last)| end - start < last - first) {
                        best = Some((start, end));
                    }
                    break;
                }
            }
        }
        match best {
            Some((start, end)) => {
                let words = &line.words[start..=end];
                let rect = words[1..].iter().fold(words[0].rect, |rect, word| rect.union(word.rect));
                let content = words.iter().map(|word| word.text.as_str()).collect::<Vec<_>>().join(" ");
                found.push(text(content, rect));
            }
            None => found.push(text(line.text.clone(), line.rect)),
        }
    }
    found
}

pub(super) fn recognize(target: &Target) -> Result<Vec<Line>, Error> {
    let mapping = target.layout.mapping(target.rect).map_err(internal)?;
    let frame = target.layout.grab(target.rect).map_err(internal)?;
    if crate::shutdown_macos::requested() {
        return Err(Error::new(ErrorCode::Conflict, "Jiffshot is shutting down"));
    }
    let mut lines = ocr::recognize(frame.width, frame.height, &frame.pixels).map_err(internal)?;
    target.layout.validate().map_err(internal)?;
    for line in &mut lines {
        line.rect = mapping.desktop_rect(line.rect).ok_or_else(|| internal("OCR returned an invalid line rectangle"))?;
        for word in &mut line.words {
            word.rect = mapping.desktop_rect(word.rect).ok_or_else(|| internal("OCR returned an invalid word rectangle"))?;
        }
    }
    Ok(lines)
}

pub(super) fn ocr(a: &Args) -> Reply {
    let mut allowed = TARGET.to_vec();
    allowed.push("find");
    a.only(&allowed)?;
    let phrase = a.string("find")?;
    let target = target(a, true)?;
    let lines = recognize(&target)?;
    match phrase {
        Some(phrase) => Ok(json!({ "target": target.json(), "matches": find_text(&lines, &phrase) })),
        None => {
            let lines: Vec<Text> = lines.into_iter().map(|line| text(line.text, line.rect)).collect();
            Ok(json!({ "target": target.json(), "lines": lines }))
        }
    }
}

pub(super) fn wait_text(a: &Args) -> Reply {
    let mut allowed = TARGET.to_vec();
    allowed.extend_from_slice(&["text", "gone", "timeout_ms", "interval_ms"]);
    a.only(&allowed)?;
    let phrase = a.string("text")?.ok_or_else(|| invalid("text is required"))?;
    let gone = a.bool("gone")?;
    let timeout = Duration::from_millis(a.int("timeout_ms", 0, 3_600_000)?.unwrap_or(10_000) as u64);
    let interval = Duration::from_millis(a.int("interval_ms", 0, 3_600_000)?.unwrap_or(400) as u64);
    let started = Instant::now();
    let mut quiet = Args(a.0.clone());
    quiet.0.insert("no_activate".into(), json!(true));
    let mut first = true;
    loop {
        wait(Duration::ZERO, crate::shutdown_macos::requested)?;
        let target = target(if first { a } else { &quiet }, true)?;
        first = false;
        let lines = recognize(&target)?;
        let matches = find_text(&lines, &phrase);
        if matches.is_empty() == gone {
            return Ok(json!({
                "matches": matches, "gone": gone,
                "waited_ms": started.elapsed().as_millis() as u64, "target": target.json(),
            }));
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            let what = if gone { "still shows" } else { "did not show" };
            let mut snippet = String::new();
            for line in &lines {
                if snippet.len() > 400 {
                    snippet.push_str(" …");
                    break;
                }
                if !snippet.is_empty() {
                    snippet.push_str(" | ");
                }
                snippet.push_str(&line.text);
            }
            return Err(Error::new(ErrorCode::Timeout, format!(
                "the screen {what} {phrase:?} within {} ms; on screen: {snippet}", timeout.as_millis()
            )));
        }
        wait(interval.min(remaining), crate::shutdown_macos::requested)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn region_clipping_rejects_empty_overflow_and_distant_rectangles() {
        let desktop = Rect::new(-200, -100, 800, 600);
        assert_eq!(clip_region("-250, -20, 100, 40", desktop).unwrap(), Rect::new(-200, -20, 50, 40));
        for region in ["1,2,0,3", "2147483647,0,1,1", "0,2147483647,1,1", "-2147483648,0,1,1", "0,0,1"] {
            assert_eq!(clip_region(region, desktop).unwrap_err().code, ErrorCode::InvalidRequest, "{region}");
        }
    }

    #[test]
    fn monitor_selection_accepts_the_upstream_request_shapes() {
        assert_eq!(monitor_spec(&json!(true)).unwrap(), MonitorSpec::Primary);
        assert_eq!(monitor_spec(&json!("PRIMARY")).unwrap(), MonitorSpec::Primary);
        assert_eq!(monitor_spec(&json!("cursor")).unwrap(), MonitorSpec::Cursor);
        assert_eq!(monitor_spec(&json!(" 42 ")).unwrap(), MonitorSpec::Index(42));
        assert_eq!(monitor_spec(&json!(0)).unwrap(), MonitorSpec::Index(0));
        for value in [json!(false), json!(-1), json!(1.5), json!("other")] {
            assert_eq!(monitor_spec(&value).unwrap_err().code, ErrorCode::InvalidRequest);
        }
    }

    #[test]
    fn untargeted_capture_and_ocr_use_the_upstream_display_defaults() {
        assert_eq!(display_target(None, false).unwrap(), Some(MonitorSpec::Primary));
        assert_eq!(display_target(None, true).unwrap(), None);
        assert_eq!(display_target(Some(&Value::Null), false).unwrap(), Some(MonitorSpec::Primary));
        assert_eq!(display_target(Some(&Value::Null), true).unwrap(), None);
        assert_eq!(display_target(Some(&json!("primary")), true).unwrap(), Some(MonitorSpec::Primary));
        assert_eq!(display_target(Some(&json!("cursor")), true).unwrap(), Some(MonitorSpec::Cursor));
        assert_eq!(display_target(Some(&json!(2)), true).unwrap(), Some(MonitorSpec::Index(2)));
        assert_eq!(display_target(Some(&json!(false)), true).unwrap_err().code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn explicit_and_default_desktop_targets_keep_logical_bounds() {
        for (args, default_desktop) in [
            (json!({ "desktop": true }), false),
            (json!({}), true),
        ] {
            let target = target(&Args::new(args).unwrap(), default_desktop).unwrap();
            assert_eq!(target.kind, CaptureKind::Desktop);
            assert_eq!(target.rect, target.layout.virtual_screen());
            let bounds = target.layout.display().logical_virtual_bounds().unwrap();
            let expected = target.layout.display().logical_mapping(bounds).unwrap();
            let mapping = target.layout.mapping(target.rect).unwrap();
            assert_eq!(mapping.size(), expected.size());
            assert_eq!(mapping.desktop_bounds(), bounds.edges().map(|edge| edge * target.layout.desktop_scale()));
            let (recording, spec) = crate::recorder::recording_geometry(target.rect, 30, &target.layout).unwrap();
            assert_eq!(recording.desktop_bounds(), mapping.desktop_bounds());
            assert_eq!(recording.size(), (spec.width, spec.height));
        }
    }

    #[test]
    fn delayed_capture_stops_promptly_when_shutdown_begins() {
        let cancel = Arc::new(AtomicBool::new(false));
        let trigger = cancel.clone();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            trigger.store(true, Ordering::Release);
        });
        let start = Instant::now();
        let error = wait(Duration::from_secs(30), || cancel.load(Ordering::Acquire)).unwrap_err();
        worker.join().unwrap();
        assert_eq!(error.code, ErrorCode::Conflict);
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(wait(Duration::ZERO, || false).is_ok());
    }

    #[test]
    fn ocr_matching_uses_the_smallest_case_and_space_insensitive_word_run() {
        use jiffshot_capture::ocr::Word;
        let lines = vec![Line {
            text: "Open    fixture window".into(),
            rect: Rect::new(-50, 20, 150, 15),
            words: vec![
                Word { text: "Open".into(), rect: Rect::new(-50, 20, 32, 15) },
                Word { text: "fixture".into(), rect: Rect::new(-10, 20, 48, 15) },
                Word { text: "window".into(), rect: Rect::new(45, 20, 55, 15) },
            ],
        }];
        let found = find_text(&lines, "FIXTURE   window");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text, "fixture window");
        assert_eq!(found[0].rect, Rect::new(-10, 20, 110, 15));
        assert_eq!(found[0].center, (45, 27));
        assert_eq!(find_text(&lines, "open")[0].text, "Open");
        assert!(find_text(&lines, " ").is_empty());
        assert!(find_text(&lines, "missing").is_empty());
        let punctuation = Line { text: "one-two".into(), rect: Rect::new(1, 2, 8, 3), words: Vec::new() };
        assert_eq!(find_text(&[punctuation], "one-two")[0].rect, Rect::new(1, 2, 8, 3));
    }
}
