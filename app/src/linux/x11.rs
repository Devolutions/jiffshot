//! Window and pointer queries for native X11 sessions.
//!
//! Wayland compositors do not expose other applications' windows or the global pointer, and
//! XWayland only shows X11 clients in its own coordinate space, so these queries answer only
//! when the session itself is X11.

use jiffshot_capture::Rect;
use x11rb::connection::Connection;
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
        Some(connection.intern_atom(false, name.as_bytes()).ok()?.reply().ok()?.atom)
    };
    let stacking = atom("_NET_CLIENT_LIST_STACKING")?;
    let name = atom("_NET_WM_NAME")?;
    let utf8 = atom("UTF8_STRING")?;
    let state = atom("_NET_WM_STATE")?;
    let hidden = atom("_NET_WM_STATE_HIDDEN")?;
    let property = |window: Window, property: Atom, kind: Atom, length: u32| -> Option<GetPropertyReply> {
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
    use super::session_is_x11;

    #[test]
    fn only_native_x11_sessions_are_detected() {
        assert!(session_is_x11(Some("x11"), false, true));
        assert!(session_is_x11(None, false, true));
        assert!(!session_is_x11(Some("wayland"), false, true));
        assert!(!session_is_x11(Some("x11"), true, true));
        assert!(!session_is_x11(Some("x11"), false, false));
    }
}
