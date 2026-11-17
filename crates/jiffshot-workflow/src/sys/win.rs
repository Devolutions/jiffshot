//! Windows: the real Win32 calls, plus the few helpers the macOS side
//! implements differently.

pub use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
pub use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT,
    MOD_WIN, VK_BACK, VK_DOWN, VK_ESCAPE, VK_F1, VK_F10, VK_F11, VK_F9, VK_LBUTTON, VK_LEFT, VK_RBUTTON, VK_RETURN,
    VK_RIGHT, VK_SNAPSHOT, VK_SPACE, VK_UP,
};
pub use windows_sys::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DispatchMessageW, GetCursorPos, GetMessageW, KillTimer, MsgWaitForMultipleObjects, PeekMessageW,
    PostMessageW, PostQuitMessage, SetCursorPos, SetTimer, TranslateMessage, MSG, PM_REMOVE, QS_ALLINPUT, WM_APP, WM_CLOSE,
    WM_DESTROY, WM_HOTKEY, WM_QUIT, WM_TIMER,
};

use jiffshot_capture::wide;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_WHEEL, MOUSEINPUT};

pub type WndProc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

/// The hidden window of `class` in this session, or null.
pub fn find_window(class: &str) -> HWND {
    // SAFETY: a lookup by class name.
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW(wide(class).as_ptr(), std::ptr::null()) }
}

/// Creates a hidden window of `class` whose messages go to `proc`; null on
/// failure.
pub fn create_message_window(class: &str, title: &str, proc: WndProc) -> HWND {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, RegisterClassExW, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP};
    let class = wide(class);
    // SAFETY: standard window class and hidden window setup; the class name
    // is copied by RegisterClassExW.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassExW(&wc);
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            wide(title).as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        )
    }
}

fn send_wheel(flags: u32, notches: i32) {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: (notches * 120) as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    // SAFETY: one well-formed input event.
    unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) };
}

/// Turns the mouse wheel `notches` (positive: away from the user, up).
pub fn wheel(notches: i32) {
    send_wheel(MOUSEEVENTF_WHEEL, notches);
}

/// Tilts the wheel `notches` (positive: right).
pub fn hwheel(notches: i32) {
    send_wheel(MOUSEEVENTF_HWHEEL, notches);
}

/// Finer sleeps while recording (the default tick is 15.6 ms); undone on
/// drop.
pub struct FineTimer;

impl FineTimer {
    pub fn start() -> FineTimer {
        // SAFETY: paired with timeEndPeriod on drop.
        unsafe { windows_sys::Win32::Media::timeBeginPeriod(1) };
        FineTimer
    }
}

impl Drop for FineTimer {
    fn drop(&mut self) {
        // SAFETY: pairs with timeBeginPeriod in `start`.
        unsafe { windows_sys::Win32::Media::timeEndPeriod(1) };
    }
}

/// Claims the one running Jiffshot for this session; false when another
/// process already has it.
pub fn claim_instance(name: &str) -> bool {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    // SAFETY: a named mutex for single-instance detection; the handle is
    // intentionally held for the life of the process.
    unsafe {
        CreateMutexW(std::ptr::null(), 0, jiffshot_capture::wide(&format!("Local\\{name}")).as_ptr());
        GetLastError() != ERROR_ALREADY_EXISTS
    }
}

/// Lets other processes reach the window of `class` through
/// [`post_to_instance`]: on Windows they find it by class, and when the
/// running instance has a UIAccess token (`Medium+` integrity), UIPI blocks
/// standard (`Medium` integrity) second instances unless its window message
/// filter explicitly allows the cross-instance messages.
pub fn serve_instance(class: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{ChangeWindowMessageFilterEx, MSGFLT_ALLOW};
    let hwnd = find_window(class);
    if hwnd.is_null() {
        return;
    }
    for msg in [
        crate::tray::WM_APP_CAPTURE,
        crate::tray::WM_APP_MENU,
        crate::tray::WM_APP_CAPTURE_LATER,
    ] {
        // SAFETY: allows the message on our own window's UIPI filter.
        unsafe {
            ChangeWindowMessageFilterEx(hwnd, msg, MSGFLT_ALLOW, std::ptr::null_mut());
        }
    }
}

/// Posts `msg` to the running instance's window of `class` (in another
/// process), passing on this process's right to take the foreground so the
/// window it opens gets the keyboard; false when there is none.
pub fn post_to_instance(class: &str, msg: u32, wparam: WPARAM, lparam: LPARAM) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, GetWindowThreadProcessId};
    let hwnd = find_window(class);
    if hwnd.is_null() {
        return false;
    }
    // SAFETY: plain calls on a window handle that may have just closed,
    // which they tolerate.
    unsafe {
        let mut process = 0;
        GetWindowThreadProcessId(hwnd, &mut process);
        AllowSetForegroundWindow(process);
        PostMessageW(hwnd, msg, wparam, lparam) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, WNDCLASSW,
    };

    unsafe extern "system" fn test_wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    #[test]
    fn serve_instance_allows_cross_instance_messages_and_posts_to_window() {
        let class = format!("Jiffshot.Test.Instance.{}", std::process::id());
        let wide_class = jiffshot_capture::wide(&class);
        unsafe {
            let mut wc: WNDCLASSW = std::mem::zeroed();
            wc.lpfnWndProc = Some(test_wndproc);
            wc.lpszClassName = wide_class.as_ptr();
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                0,
                wide_class.as_ptr(),
                wide_class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            assert!(!hwnd.is_null());
            serve_instance(&class);
            assert!(post_to_instance(&class, crate::tray::WM_APP_CAPTURE, 0, 0));
            assert!(post_to_instance(&class, crate::tray::WM_APP_MENU, 0, 0));
            assert!(post_to_instance(
                &class,
                crate::tray::WM_APP_CAPTURE_LATER,
                0,
                0
            ));
            DestroyWindow(hwnd);
        }
    }
}
