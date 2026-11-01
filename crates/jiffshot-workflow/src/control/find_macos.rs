use super::{capture, input, internal, invalid, not_found, Args, Reply};
use jiffshot_capture::screen::DisplayLayout;
use jiffshot_capture::targets::{self, AccessibleElement, AccessibleElements, Window, WindowActivation};
use jiffshot_control::{Error, ErrorCode};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const MAX_OFFSCREEN_CHECKS: usize = 64;
const MAX_OFFSCREEN_TIME: Duration = Duration::from_secs(3);

#[derive(Default)]
struct Query {
    hwnd: Option<u32>,
    title: Option<String>,
    process: Option<String>,
    class: Option<String>,
    all: bool,
}

impl Query {
    fn empty(&self) -> bool {
        self.hwnd.is_none() && self.title.is_none() && self.process.is_none() && self.class.is_none()
    }

    fn matches(&self, window: &Window) -> bool {
        if window.on_screen && !window.is_visible_candidate() {
            return false;
        }
        if !self.all && self.hwnd.is_none()
            && (window.pid == std::process::id() || window.title.is_empty() || !window.is_regular())
        {
            return false;
        }
        self.hwnd.is_none_or(|id| window.hwnd == id as isize)
            && self.title.as_ref().is_none_or(|title| window.title.to_lowercase().contains(&title.to_lowercase()))
            && self.process.as_ref().is_none_or(|process| window.process.eq_ignore_ascii_case(process))
            && self.class.as_ref().is_none_or(|class| window.class.eq_ignore_ascii_case(class))
    }
}

fn window_number(value: &Value) -> Result<Option<u32>, Error> {
    let number = match value {
        Value::Null => return Ok(None),
        Value::Number(number) => number.as_u64(),
        Value::String(value) => {
            let value = value.trim();
            value.strip_prefix("0x").or_else(|| value.strip_prefix("0X"))
                .map_or_else(|| value.parse().ok(), |hex| u64::from_str_radix(hex, 16).ok())
        }
        _ => None,
    };
    number.and_then(|number| u32::try_from(number).ok()).map(Some)
        .ok_or_else(|| invalid("window must be a window number (decimal or 0x hex)"))
}

fn query(a: &Args) -> Result<Query, Error> {
    Ok(Query {
        hwnd: match a.0.get("window") {
            Some(value) => window_number(value)?,
            None => None,
        },
        title: a.string("title")?,
        process: a.string("process")?,
        class: a.string("class")?,
        all: false,
    })
}

pub(super) fn specified(a: &Args) -> bool {
    ["window", "title", "process", "class"].iter().any(|key| a.0.get(*key).is_some_and(|value| !value.is_null()))
}

struct Matches {
    windows: Vec<Window>,
    omitted: usize,
    offscreen_error: Option<String>,
}

impl Matches {
    fn first(self) -> Result<Option<Window>, Error> {
        if let Some(window) = self.windows.into_iter().next() {
            return Ok(Some(window));
        }
        match self.offscreen_error {
            Some(error) => Err(Error::new(ErrorCode::Conflict, format!("could not identify an off-screen window safely: {error}"))),
            None => Ok(None),
        }
    }
}

fn offscreen_unavailable(trusted: bool, index: usize, elapsed: Duration) -> Option<String> {
    if !trusted {
        Some("Accessibility permission is required".into())
    } else if index >= MAX_OFFSCREEN_CHECKS {
        Some(format!("the limit of {MAX_OFFSCREEN_CHECKS} off-screen window checks was reached"))
    } else if elapsed >= MAX_OFFSCREEN_TIME {
        Some("the off-screen window check time limit was reached".into())
    } else {
        None
    }
}

fn matching(layout: &DisplayLayout, query: &Query, first_only: bool, prompt: bool) -> Result<Matches, Error> {
    let candidates = targets::window_candidates(layout).map_err(internal)?;
    let mut result = Matches { windows: Vec::new(), omitted: 0, offscreen_error: None };
    for window in candidates.iter().filter(|window| window.on_screen && query.matches(window)) {
        result.windows.push(window.clone());
        if first_only {
            layout.validate().map_err(internal)?;
            return Ok(result);
        }
    }
    let offscreen: Vec<_> = candidates.iter().filter(|window| !window.on_screen && query.matches(window)).collect();
    if offscreen.is_empty() {
        layout.validate().map_err(internal)?;
        return Ok(result);
    }
    let trusted = if prompt { targets::request_accessibility() } else { targets::accessibility_trusted() };
    let started = Instant::now();
    for (index, window) in offscreen.into_iter().enumerate() {
        capture::wait(Duration::ZERO, crate::shutdown_macos::requested)?;
        let state = match offscreen_unavailable(trusted, index, started.elapsed()) {
            Some(error) => Err(error),
            None => targets::offscreen_minimized(window, &candidates).map_err(|error| error.to_string()),
        };
        match state {
            Ok(minimized) => {
                let mut window = window.clone();
                window.minimized = minimized;
                result.windows.push(window);
                if first_only {
                    break;
                }
            }
            Err(error) => {
                result.omitted += 1;
                result.offscreen_error.get_or_insert(error);
                if first_only {
                    break;
                }
            }
        }
    }
    layout.validate().map_err(internal)?;
    Ok(result)
}

pub(super) fn list(a: &Args) -> Reply {
    a.only(&["window", "title", "process", "class", "all"])?;
    let mut query = query(a)?;
    query.all = a.bool("all")?;
    let layout = DisplayLayout::current().map_err(internal)?;
    let result = matching(&layout, &query, false, false)?;
    Ok(json!({
        "windows": result.windows,
        "offscreen_omitted": result.omitted,
        "offscreen_error": result.offscreen_error,
    }))
}

pub(super) fn selected(a: &Args, layout: &DisplayLayout, activate: bool) -> Result<Window, Error> {
    let query = query(a)?;
    if query.empty() {
        return Err(invalid("name a window with window, title, process or class"));
    }
    let window = matching(layout, &query, true, true)?.first()?
        .ok_or_else(|| not_found("no window matches; `windows` lists them"))?;
    if !activate || a.bool("no_activate")? {
        if !window.on_screen {
            return Err(Error::new(ErrorCode::Conflict, "the selected window is off screen and cannot be targeted without activation"));
        }
        return Ok(window);
    }
    input::serialized(|| {
        if !window.on_screen {
            let candidates = targets::window_candidates(layout).map_err(internal)?;
            let current = candidates.iter().find(|candidate| candidate.hwnd == window.hwnd && candidate.pid == window.pid)
                .ok_or_else(|| not_found("the selected window disappeared before activation"))?;
            if current.title != window.title || current.rect != window.rect {
                return Err(Error::new(ErrorCode::Conflict, "the selected off-screen window changed before activation"));
            }
            targets::offscreen_minimized(&window, &candidates).map_err(accessibility_error)?;
        }
        let strict_identity = !window.on_screen;
        let activation = targets::activate_window(&window, strict_identity, true).map_err(accessibility_error)?;
        if strict_identity || activation == WindowActivation::Restoring {
            raise_when_visible(layout, &window)?;
        }
        let started = Instant::now();
        loop {
            let current = targets::windows(layout).map_err(internal)?.into_iter()
                .find(|candidate| candidate.hwnd == window.hwnd && candidate.pid == window.pid);
            if let Some(current) = current {
                if (current.foreground && started.elapsed() >= Duration::from_millis(250))
                    || started.elapsed() >= Duration::from_millis(1500)
                {
                    return Ok(current);
                }
            } else if started.elapsed() >= Duration::from_millis(1500) {
                return Err(not_found("the selected window did not appear on screen after activation"));
            }
            capture::wait(Duration::from_millis(50), crate::shutdown_macos::requested)?;
        }
    })
}

fn raise_when_visible(layout: &DisplayLayout, original: &Window) -> Result<(), Error> {
    let started = Instant::now();
    let mut visible_since = None;
    loop {
        capture::wait(Duration::ZERO, crate::shutdown_macos::requested)?;
        if started.elapsed() >= Duration::from_secs(5) {
            return Err(Error::new(ErrorCode::Timeout, "the off-screen window did not finish appearing"));
        }
        let candidates = targets::window_candidates(layout).map_err(internal)?;
        let current = candidates.iter().find(|candidate| candidate.hwnd == original.hwnd && candidate.pid == original.pid)
            .ok_or_else(|| not_found("the selected window disappeared while being restored"))?;
        if current.title != original.title {
            return Err(Error::new(ErrorCode::Conflict, "the selected window changed while being restored"));
        }
        if current.on_screen && current.is_visible_candidate() {
            let visible_since = visible_since.get_or_insert_with(Instant::now);
            if visible_since.elapsed() >= Duration::from_millis(250) {
                targets::ensure_unique_window_identity(current, &candidates).map_err(accessibility_error)?;
                match targets::activate_window(current, true, false) {
                    Ok(WindowActivation::Raised) => return Ok(()),
                    Ok(WindowActivation::Restoring) => {
                        return Err(Error::new(ErrorCode::Conflict, "the selected window was minimized again while being restored"));
                    }
                    Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::NotFound) => {}
                    Err(error) => return Err(accessibility_error(error)),
                }
            }
        } else {
            visible_since = None;
        }
        capture::wait(Duration::from_millis(50), crate::shutdown_macos::requested)?;
    }
}

pub(super) fn focus(a: &Args) -> Reply {
    a.only(&["window", "title", "process", "class"])?;
    let layout = DisplayLayout::current().map_err(internal)?;
    let window = selected(a, &layout, true)?;
    Ok(json!({ "foreground": window.foreground, "window": window }))
}

fn accessibility_error(error: std::io::Error) -> Error {
    let code = match error.kind() {
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Interrupted | std::io::ErrorKind::InvalidData => ErrorCode::Conflict,
        std::io::ErrorKind::TimedOut => ErrorCode::Timeout,
        std::io::ErrorKind::InvalidInput => ErrorCode::InvalidRequest,
        _ => ErrorCode::Internal,
    };
    Error::new(code, error.to_string())
}

fn normalized(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn matching_elements(all: Vec<AccessibleElement>, name: &str, control_type: Option<&str>) -> Vec<AccessibleElement> {
    let name = normalized(name);
    let typed: Vec<_> = all.into_iter()
        .filter(|element| control_type.is_none_or(|kind| element.control_type.eq_ignore_ascii_case(kind)))
        .collect();
    let exact: Vec<_> = typed.iter().filter(|element| normalized(&element.name) == name).cloned().collect();
    if !exact.is_empty() {
        return exact;
    }
    typed.into_iter().filter(|element| !name.is_empty() && normalized(&element.name).contains(&name)).collect()
}

fn read_elements(window: &Window, layout: &DisplayLayout, max: usize) -> Result<AccessibleElements, Error> {
    targets::accessibility_elements(window, layout, max, crate::shutdown_macos::requested).map_err(accessibility_error)
}

fn element_window(a: &Args, layout: &DisplayLayout) -> Result<Window, Error> {
    if !specified(a) {
        return Err(invalid("element needs a window: give window, title, process or class"));
    }
    let window = selected(a, layout, true)?;
    if !a.bool("no_activate")? && !window.foreground {
        return Err(Error::new(ErrorCode::Conflict, "the selected window could not be brought to the front"));
    }
    Ok(window)
}

pub(super) fn element_target(
    a: &Args,
    layout: &DisplayLayout,
    name: &str,
    nth: usize,
) -> Result<(Window, AccessibleElement), Error> {
    if normalized(name).is_empty() {
        return Err(invalid("element must be a non-empty Accessibility name"));
    }
    let control_type = a.string("type")?;
    let window = element_window(a, layout)?;
    let snapshot = read_elements(&window, layout, 5000)?;
    if snapshot.truncated {
        return Err(Error::new(ErrorCode::Conflict,
            "the Accessibility tree exceeded the traversal limit; an element cannot be selected safely"));
    }
    let found = matching_elements(snapshot.elements, name, control_type.as_deref());
    let element = found.get(nth - 1).cloned()
        .ok_or_else(|| not_found(format!("element {name:?} not found ({} matches)", found.len())))?;
    Ok((window, element))
}

pub(super) fn elements(a: &Args) -> Reply {
    let mut names = capture::TARGET.to_vec();
    names.extend_from_slice(&["name", "type", "max"]);
    a.only(&names)?;
    let max = a.int("max", 1, 5000)?.unwrap_or(300) as usize;
    let name = a.string("name")?;
    let control_type = a.string("type")?;
    let layout = DisplayLayout::current().map_err(internal)?;
    let window = element_window(a, &layout)?;
    let filtered = name.is_some() || control_type.is_some();
    let snapshot = read_elements(&window, &layout, if filtered { 5000 } else { max })?;
    let mut all = match name {
        Some(name) => matching_elements(snapshot.elements, &name, control_type.as_deref()),
        None => snapshot.elements.into_iter()
            .filter(|element| control_type.as_ref().is_none_or(|kind| element.control_type.eq_ignore_ascii_case(kind)))
            .collect(),
    };
    let truncated = snapshot.truncated || all.len() > max;
    all.truncate(max);
    Ok(json!({ "window": window, "elements": all, "truncated": truncated }))
}

pub(super) fn wait_window(a: &Args) -> Reply {
    a.only(&["window", "title", "process", "class", "gone", "timeout_ms"])?;
    let query = query(a)?;
    if query.empty() {
        return Err(invalid("name a window with window, title, process or class"));
    }
    let gone = a.bool("gone")?;
    let timeout = Duration::from_millis(a.int("timeout_ms", 0, 3_600_000)?.unwrap_or(10_000) as u64);
    let started = Instant::now();
    loop {
        capture::wait(Duration::ZERO, crate::shutdown_macos::requested)?;
        let layout = DisplayLayout::current().map_err(internal)?;
        let window = matching(&layout, &query, true, true)?.first()?;
        match (window, gone) {
            (Some(window), false) => return Ok(json!({ "window": window, "waited_ms": started.elapsed().as_millis() as u64 })),
            (None, true) => return Ok(json!({ "gone": true, "waited_ms": started.elapsed().as_millis() as u64 })),
            _ => {}
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            let what = if gone { "still open" } else { "did not appear" };
            return Err(Error::new(ErrorCode::Timeout, format!("the window {what} within {} ms", timeout.as_millis())));
        }
        capture::wait(remaining.min(Duration::from_millis(150)), crate::shutdown_macos::requested)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_decimal_and_hexadecimal_window_numbers_without_truncation() {
        assert_eq!(window_number(&json!(null)).unwrap(), None);
        assert_eq!(window_number(&json!(123)).unwrap(), Some(123));
        assert_eq!(window_number(&json!("0X7b")).unwrap(), Some(123));
        assert_eq!(window_number(&json!(" 123 ")).unwrap(), Some(123));
        for value in [json!(-1), json!(4_294_967_296u64), json!("0x100000000"), json!(1.5), json!(true)] {
            assert_eq!(window_number(&value).unwrap_err().code, ErrorCode::InvalidRequest);
        }
    }

    #[test]
    fn inaccessible_offscreen_matches_do_not_look_absent() {
        let result = Matches {
            windows: Vec::new(),
            omitted: 1,
            offscreen_error: Some("Accessibility permission is required".into()),
        };
        let error = result.first().unwrap_err();
        assert_eq!(error.code, ErrorCode::Conflict);
        assert!(error.message.contains("Accessibility permission"));
    }

    #[test]
    fn offscreen_checks_report_permission_and_exact_limits() {
        assert!(offscreen_unavailable(true, 63, Duration::from_millis(2999)).is_none());
        assert!(offscreen_unavailable(false, 0, Duration::ZERO).unwrap().contains("Accessibility"));
        assert!(offscreen_unavailable(true, 64, Duration::ZERO).unwrap().contains("limit of 64"));
        assert!(offscreen_unavailable(true, 0, Duration::from_secs(3)).unwrap().contains("time limit"));
    }

    #[test]
    fn element_matching_prefers_exact_names_and_respects_type() {
        let element = |name: &str, control_type: &str| AccessibleElement {
            name: name.into(),
            control_type: control_type.into(),
            automation_id: String::new(),
            rect: jiffshot_capture::Rect::new(0, 0, 10, 10),
            center: (5, 5),
            enabled: Some(true),
        };
        let all = vec![element("Save As", "button"), element("  SAVE  ", "button"), element("Save", "menu")];
        let exact = matching_elements(all.clone(), "save", Some("button"));
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].name, "  SAVE  ");
        let partial = matching_elements(all, "as", None);
        assert_eq!(partial.len(), 1);
        assert_eq!(partial[0].name, "Save As");
    }
}
