//! Window and pointer queries for native X11 sessions.
//!
//! Wayland compositors do not expose other applications' windows or the global pointer, and
//! XWayland only shows X11 clients in its own coordinate space, so these queries answer only
//! when the session itself is X11.

use jiffshot_capture::Rect;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::screensaver::ConnectionExt as _;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt, GetPropertyReply, KeyButMask, Window,
};
use x11rb::rust_connection::RustConnection;

/// Whether the desktop session is a native X11 session.
pub(crate) fn is_x11_session() -> bool {
    session_is_x11(
        std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
        std::env::var_os("DISPLAY").is_some(),
    )
}

fn session_is_x11(session_type: Option<&str>, wayland_display: bool, display: bool) -> bool {
    display
        && !wayland_display
        && !session_type.is_some_and(|kind| kind.eq_ignore_ascii_case("wayland"))
}

/// The pointer position and whether any mouse button is held, in desktop pixels.
pub(crate) fn pointer() -> Option<((i32, i32), bool)> {
    if !is_x11_session() {
        return None;
    }
    let (connection, screen) = RustConnection::connect(None).ok()?;
    let root = connection.setup().roots.get(screen)?.root;
    let reply = connection.query_pointer(root).ok()?.reply().ok()?;
    let buttons = KeyButMask::BUTTON1 | KeyButMask::BUTTON2 | KeyButMask::BUTTON3;
    Some((
        (i32::from(reply.root_x), i32::from(reply.root_y)),
        reply.mask.intersects(buttons),
    ))
}

/// Samples native X11 activity without recording key contents.
pub(crate) struct ActivityTracker {
    connection: RustConnection,
    root: Window,
    previous_pointer: Option<(i16, i16)>,
    previous_sample: Option<Instant>,
}

impl ActivityTracker {
    pub(crate) fn connect() -> Result<Self, String> {
        if !is_x11_session() {
            return Err("physical input tracking requires a native X11 session".into());
        }
        let (connection, screen) = RustConnection::connect(None)
            .map_err(|error| format!("could not connect to X11 for input tracking: {error}"))?;
        let root = connection
            .setup()
            .roots
            .get(screen)
            .ok_or("the X11 display has no screen for input tracking")?
            .root;
        connection
            .screensaver_query_version(1, 1)
            .map_err(|error| format!("could not query X11 input tracking: {error}"))?
            .reply()
            .map_err(|error| format!("X11 input tracking is unavailable: {error}"))?;
        Ok(Self {
            connection,
            root,
            previous_pointer: None,
            previous_sample: None,
        })
    }

    pub(crate) fn resume(&mut self, now: Instant) {
        self.previous_pointer = None;
        self.previous_sample = Some(now);
    }

    pub(crate) fn sample(&mut self, now: Instant) -> Result<bool, String> {
        let elapsed = self.previous_sample.replace(now).map(|previous| {
            now.checked_duration_since(previous)
                .expect("X11 input samples use a monotonic clock")
        });
        let idle_ms = self
            .connection
            .screensaver_query_info(self.root)
            .map_err(|error| format!("could not query X11 input activity: {error}"))?
            .reply()
            .map_err(|error| format!("could not read X11 input activity: {error}"))?
            .ms_since_user_input;
        let keys = self
            .connection
            .query_keymap()
            .map_err(|error| format!("could not query X11 keyboard state: {error}"))?
            .reply()
            .map_err(|error| format!("could not read X11 keyboard state: {error}"))?;
        let pointer = self
            .connection
            .query_pointer(self.root)
            .map_err(|error| format!("could not query X11 pointer state: {error}"))?
            .reply()
            .map_err(|error| format!("could not read X11 pointer state: {error}"))?;
        let position = (pointer.root_x, pointer.root_y);
        let moved = self
            .previous_pointer
            .replace(position)
            .is_some_and(|previous| previous != position);
        let buttons = KeyButMask::BUTTON1
            | KeyButMask::BUTTON2
            | KeyButMask::BUTTON3
            | KeyButMask::BUTTON4
            | KeyButMask::BUTTON5;
        let recent = elapsed.is_some_and(|elapsed| {
            Duration::from_millis(u64::from(idle_ms)) <= elapsed + Duration::from_millis(5)
        });
        let active = recent
            || moved
            || pointer.mask.intersects(buttons)
            || keys.keys.iter().any(|key| *key != 0);
        if active && elapsed.is_some_and(|gap| gap > Duration::from_millis(200)) {
            return Err("physical input sampling was delayed too long".into());
        }
        Ok(active)
    }
}

/// Visible top-level windows with their titles, bottom to top.
pub(crate) fn windows() -> Vec<(Rect, String)> {
    if !is_x11_session() {
        return Vec::new();
    }
    list_windows().unwrap_or_default()
}

fn list_windows() -> Option<Vec<(Rect, String)>> {
    let (connection, screen) = RustConnection::connect(None).ok()?;
    let root = connection.setup().roots.get(screen)?.root;
    let atom = |name: &str| -> Option<Atom> {
        Some(
            connection
                .intern_atom(false, name.as_bytes())
                .ok()?
                .reply()
                .ok()?
                .atom,
        )
    };
    let stacking = atom("_NET_CLIENT_LIST_STACKING")?;
    let name = atom("_NET_WM_NAME")?;
    let utf8 = atom("UTF8_STRING")?;
    let state = atom("_NET_WM_STATE")?;
    let hidden = atom("_NET_WM_STATE_HIDDEN")?;
    let property =
        |window: Window, property: Atom, kind: Atom, length: u32| -> Option<GetPropertyReply> {
            connection
                .get_property(false, window, property, kind, 0, length)
                .ok()?
                .reply()
                .ok()
        };
    let list = property(root, stacking, AtomEnum::WINDOW.into(), 4096)?;
    let mut result = Vec::new();
    for window in list.value32()? {
        let minimized = property(window, state, AtomEnum::ATOM.into(), 64)
            .and_then(|reply| Some(reply.value32()?.any(|value| value == hidden)))
            .unwrap_or(false);
        if minimized {
            continue;
        }
        let geometry = connection.get_geometry(window).ok()?.reply().ok();
        let origin = connection
            .translate_coordinates(window, root, 0, 0)
            .ok()
            .and_then(|cookie| cookie.reply().ok());
        let (Some(geometry), Some(origin)) = (geometry, origin) else {
            continue;
        };
        let rect = Rect::new(
            i32::from(origin.dst_x),
            i32::from(origin.dst_y),
            i32::from(geometry.width),
            i32::from(geometry.height),
        );
        if rect.width <= 0 || rect.height <= 0 {
            continue;
        }
        let title = property(window, name, utf8, 256)
            .map(|reply| String::from_utf8_lossy(&reply.value).into_owned())
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| "Window".to_owned());
        result.push((rect, title));
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::protocol::xproto::{
        BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
    };
    use x11rb::protocol::xtest::ConnectionExt as _;

    #[test]
    fn only_native_x11_sessions_are_detected() {
        assert!(session_is_x11(Some("x11"), false, true));
        assert!(session_is_x11(None, false, true));
        assert!(!session_is_x11(Some("wayland"), false, true));
        assert!(!session_is_x11(Some("x11"), true, true));
        assert!(!session_is_x11(Some("x11"), false, false));
    }

    #[test]
    #[ignore = "requires a private native X11 display"]
    fn native_x11_activity_tracking_detects_input_and_sampling_gaps() {
        let mut tracker = ActivityTracker::connect().expect("connect to private X11 display");
        let (injector, screen) = RustConnection::connect(None).expect("connect X11 input injector");
        let root = injector.setup().roots[screen].root;
        let now = Instant::now();
        assert!(!tracker.sample(now).expect("baseline sample"));

        let key = 38;
        injector
            .xtest_fake_input(KEY_PRESS_EVENT, key, 0, root, 0, 0, 0)
            .unwrap()
            .check()
            .unwrap();
        assert!(
            tracker.sample(now + Duration::from_millis(50)).unwrap(),
            "a held key must count as activity"
        );
        injector
            .xtest_fake_input(KEY_RELEASE_EVENT, key, 0, root, 0, 0, 0)
            .unwrap()
            .check()
            .unwrap();
        tracker.resume(now + Duration::from_millis(60));
        injector
            .xtest_fake_input(KEY_PRESS_EVENT, key, 0, root, 0, 0, 0)
            .unwrap()
            .check()
            .unwrap();
        injector
            .xtest_fake_input(KEY_RELEASE_EVENT, key, 0, root, 0, 0, 0)
            .unwrap()
            .check()
            .unwrap();
        assert!(
            tracker.sample(now + Duration::from_millis(80)).unwrap(),
            "a short input event immediately after resume must be recorded"
        );

        injector
            .warp_pointer(0u32, root, 0, 0, 0, 0, 20, 20)
            .unwrap()
            .check()
            .unwrap();
        assert!(
            tracker.sample(now + Duration::from_millis(120)).unwrap(),
            "pointer motion must count as activity"
        );
        injector
            .xtest_fake_input(BUTTON_PRESS_EVENT, 1, 0, root, 0, 0, 0)
            .unwrap()
            .check()
            .unwrap();
        tracker.resume(now + Duration::from_millis(400));
        assert!(
            tracker.sample(now + Duration::from_millis(400)).unwrap(),
            "a button held across resume must count as activity"
        );
        let error = tracker
            .sample(now + Duration::from_millis(601))
            .expect_err("a long sampling gap with activity makes the history incomplete");
        assert!(error.contains("delayed too long"), "{error}");
        injector
            .xtest_fake_input(BUTTON_RELEASE_EVENT, 1, 0, root, 0, 0, 0)
            .unwrap()
            .check()
            .unwrap();
    }
}
