//! Native hit testing for the fixed-size Capture window's title bar.

use std::io;
use windows_sys::Win32::Foundation::{
    GetLastError, SetLastError, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnableMenuItem, GetClientRect, GetSystemMenu, GetWindowLongPtrW, SetWindowLongPtrW,
    SetWindowPos, GWL_STYLE, HTCAPTION, HTCLIENT, MF_BYCOMMAND, MF_ENABLED, MF_GRAYED, SC_CLOSE,
    SC_MAXIMIZE, SC_SIZE, STYLESTRUCT, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, WM_INITMENUPOPUP, WM_NCDESTROY, WM_NCHITTEST, WM_STYLECHANGED, WM_STYLECHANGING,
    WM_WINDOWPOSCHANGED, WS_SYSMENU,
};

pub const MENU_WIDTH: f64 = 250.0;
pub const BUTTON_WIDTH: f64 = 46.0;
pub const HEIGHT: f64 = 40.0;
const SUBCLASS_ID: usize = 1;

fn draggable(x: f64, y: f64, width: f64) -> bool {
    (MENU_WIDTH..width - 2.0 * BUTTON_WIDTH).contains(&x) && (0.0..HEIGHT).contains(&y)
}

/// Installs the title-bar hit test on this window's UI thread.
pub fn install(hwnd: HWND) -> io::Result<()> {
    // SAFETY: hwnd belongs to this UI thread; the callback and identifier
    // remain valid until the window is destroyed.
    if unsafe { SetWindowSubclass(hwnd, Some(subclass), SUBCLASS_ID, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }

    // Border-only windows omit WS_SYSMENU. Retaining it preserves the
    // standard Alt+Space and Alt+F4 window commands.
    // SAFETY: hwnd is our live window and these calls do not retain pointers.
    let result = unsafe {
        (|| {
            SetLastError(0);
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            if style == 0 && GetLastError() != 0 {
                return Err(io::Error::last_os_error());
            }
            SetLastError(0);
            let previous = SetWindowLongPtrW(hwnd, GWL_STYLE, style | WS_SYSMENU as isize);
            if previous == 0 && GetLastError() != 0 {
                return Err(io::Error::last_os_error());
            }
            if SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })()
    };
    let result = result.and_then(|()| configure_system_menu(hwnd));
    if result.is_err() {
        // SAFETY: undo the subclass installed above before falling back.
        unsafe { RemoveWindowSubclass(hwnd, Some(subclass), SUBCLASS_ID) };
    }
    result
}

fn configure_system_menu(hwnd: HWND) -> io::Result<()> {
    // SAFETY: hwnd is a live window on this thread; the returned menu is owned by Windows.
    let menu = unsafe { GetSystemMenu(hwnd, 0) };
    if menu.is_null() {
        return Err(io::Error::last_os_error());
    }
    for (command, state) in [
        (SC_CLOSE, MF_ENABLED),
        (SC_MAXIMIZE, MF_GRAYED),
        (SC_SIZE, MF_GRAYED),
    ] {
        // SAFETY: menu is this window's system menu and the flags are Win32 constants.
        if unsafe { EnableMenuItem(menu, command, MF_BYCOMMAND | state) } == -1 {
            return Err(io::Error::other(format!(
                "system menu command {command:#x} unavailable"
            )));
        }
    }
    Ok(())
}

unsafe extern "system" fn subclass(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    _data: usize,
) -> LRESULT {
    if message == WM_STYLECHANGING && wparam as i32 == GWL_STYLE && lparam != 0 {
        // SAFETY: WM_STYLECHANGING supplies a writable STYLESTRUCT for the
        // duration of this call.
        let style = unsafe { &mut *(lparam as *mut STYLESTRUCT) };
        style.styleNew |= WS_SYSMENU;
    }
    if message == WM_NCDESTROY {
        // SAFETY: this callback was installed with the same window and id.
        unsafe { RemoveWindowSubclass(hwnd, Some(subclass), id) };
    }
    // SAFETY: forward the original message to the rest of the subclass chain.
    let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
    // Extended client-area updates disable Close before sending a frame change.
    if message == WM_WINDOWPOSCHANGED
        || (message == WM_STYLECHANGED && wparam as i32 == GWL_STYLE)
        || (message == WM_INITMENUPOPUP && ((lparam >> 16) & 1) != 0)
    {
        if let Err(error) = configure_system_menu(hwnd) {
            eprintln!("Capture window system menu: {error}");
        }
    }
    if message != WM_NCHITTEST
        || !matches!(result, x if x == HTCLIENT as isize || x == HTCAPTION as isize)
    {
        return result;
    }

    let mut point = POINT {
        x: (lparam as u16 as i16).into(),
        y: ((lparam >> 16) as u16 as i16).into(),
    };
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: the live window and both writable structures are valid.
    if unsafe { ScreenToClient(hwnd, &mut point) == 0 || GetClientRect(hwnd, &mut rect) == 0 } {
        return result;
    }
    // SAFETY: querying the DPI does not retain the window handle.
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let scale = if dpi == 0 { 1.0 } else { f64::from(dpi) / 96.0 };
    if draggable(
        f64::from(point.x) / scale,
        f64::from(point.y) / scale,
        f64::from(rect.right - rect.left) / scale,
    ) {
        HTCAPTION as isize
    } else {
        HTCLIENT as isize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GetMenuState, SendMessageW, MF_DISABLED,
        WS_OVERLAPPEDWINDOW,
    };

    struct TestWindow(HWND);

    impl Drop for TestWindow {
        fn drop(&mut self) {
            // SAFETY: this test created the window on the current thread.
            unsafe { DestroyWindow(self.0) };
        }
    }

    #[test]
    fn system_menu_keeps_close_and_disables_resize_and_maximize() {
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        // SAFETY: the class string is terminated; no owner or creation data is retained.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                std::ptr::null(),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                560,
                320,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        assert!(
            !hwnd.is_null(),
            "create hidden test window: {}",
            io::Error::last_os_error()
        );
        let _window = TestWindow(hwnd);
        // SAFETY: the hidden window is live on this thread.
        let menu = unsafe { GetSystemMenu(hwnd, 0) };
        assert!(!menu.is_null(), "test window must have a system menu");
        // SAFETY: the command belongs to this window's system menu.
        assert_ne!(
            unsafe { EnableMenuItem(menu, SC_CLOSE, MF_BYCOMMAND | MF_GRAYED) },
            -1
        );

        install(hwnd).expect("install the Capture caption");
        let state = |command| {
            // SAFETY: the menu remains owned by the live test window.
            unsafe { GetMenuState(menu, command, MF_BYCOMMAND) }
        };
        for command in [SC_CLOSE, SC_MAXIMIZE, SC_SIZE] {
            assert_ne!(
                state(command),
                u32::MAX,
                "system command {command:#x} is missing"
            );
        }
        assert_eq!(state(SC_CLOSE) & (MF_DISABLED | MF_GRAYED), 0);
        assert_ne!(state(SC_MAXIMIZE) & (MF_DISABLED | MF_GRAYED), 0);
        assert_ne!(state(SC_SIZE) & (MF_DISABLED | MF_GRAYED), 0);

        // The framework disables Close before notifying Windows of a frame change.
        // SAFETY: the window and its menu are live on this thread.
        unsafe {
            EnableMenuItem(menu, SC_CLOSE, MF_BYCOMMAND | MF_DISABLED | MF_GRAYED);
            assert_ne!(
                SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    0,
                    0,
                    0,
                    0,
                    SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE
                ),
                0,
            );
        }
        assert_eq!(state(SC_CLOSE) & (MF_DISABLED | MF_GRAYED), 0);

        // SAFETY: both messages address the live test window and its system menu.
        unsafe {
            EnableMenuItem(menu, SC_CLOSE, MF_BYCOMMAND | MF_GRAYED);
            SendMessageW(hwnd, WM_INITMENUPOPUP, menu as usize, 1 << 16);
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            SetWindowLongPtrW(hwnd, GWL_STYLE, style & !(WS_SYSMENU as isize));
        }
        assert_eq!(state(SC_CLOSE) & (MF_DISABLED | MF_GRAYED), 0);
        // SAFETY: querying the live test window has no retained arguments.
        assert_ne!(
            unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } & WS_SYSMENU as isize,
            0
        );
    }

    #[test]
    fn title_drag_area_excludes_menus_caption_buttons_and_content() {
        assert!(draggable(MENU_WIDTH, 0.0, 560.0));
        assert!(draggable(
            560.0 - 2.0 * BUTTON_WIDTH - 0.01,
            HEIGHT - 0.01,
            560.0
        ));
        assert!(!draggable(MENU_WIDTH - 0.01, 10.0, 560.0));
        assert!(!draggable(560.0 - 2.0 * BUTTON_WIDTH, 10.0, 560.0));
        assert!(!draggable(300.0, HEIGHT, 560.0));
        assert!(!draggable(300.0, -0.01, 560.0));
        assert!(!draggable(
            MENU_WIDTH,
            10.0,
            MENU_WIDTH + 2.0 * BUTTON_WIDTH
        ));
    }
}
