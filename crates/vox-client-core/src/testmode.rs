//! Background test mode: unattended tests must never take the user's foreground.
//!
//! On while the app runs with `--autotest` (it sets `VOX_TEST_BACKGROUND=1` for itself before
//! anything else) or whenever `VOX_TEST_BACKGROUND=1` is set. Then every top-level window of the
//! process (the main window, a stream's pop-out / fullscreen window, the `test_window` example) is
//! created without activation, kept at the bottom of the z-order with `WS_EX_NOACTIVATE`, and shown
//! with `SW_SHOWNOACTIVATE`; nothing calls `SetForegroundWindow`, `ViewportCommand::Focus` or
//! minimizes / restores a window (the tests simulate "focused" and "minimized" instead). WGC still
//! captures an occluded window, and the viewer still draws into one at the bottom.
//!
//! Normal runs (no flag) are untouched.

use std::sync::OnceLock;

/// The environment variable that turns it on.
pub const ENV: &str = "VOX_TEST_BACKGROUND";

/// Is background test mode on (read once)?
pub fn background() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var(ENV).is_ok_and(|v| v == "1"))
}

/// In test mode, a Go Live may only capture the `test_window` example, this app's own window or a
/// synthetic source (pattern, file, tone): never a real screen, the PC's sound or somebody's app.
/// `VOX_TEST_ALLOW_REAL_CAPTURE=1` lifts that for a person running a test by hand at their PC.
pub const ALLOW_REAL_CAPTURE_ENV: &str = "VOX_TEST_ALLOW_REAL_CAPTURE";

/// Must a test refuse real screens, real windows and the PC's sound right now?
pub fn guards_real_capture() -> bool {
    background() && !std::env::var(ALLOW_REAL_CAPTURE_ENV).is_ok_and(|v| v == "1")
}

/// The exe names (lowercase, without `.exe`) a test may capture besides this process.
pub const TEST_CAPTURE_EXES: &[&str] = &["test_window"];

/// Is `exe_name` (`chrome.exe`, `test_window`...) one of [`TEST_CAPTURE_EXES`]?
pub fn is_test_capture_exe(exe_name: &str) -> bool {
    let n = exe_name.trim().to_lowercase();
    let n = n.strip_suffix(".exe").unwrap_or(&n);
    TEST_CAPTURE_EXES.contains(&n)
}

/// Make a top-level window one that never activates, and put it behind every other window (no
/// activation, position and size kept). `taskbar`: keep its taskbar button (`WS_EX_APPWINDOW`).
#[cfg(windows)]
pub fn keep_behind(hwnd: isize, taskbar: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, HWND_BOTTOM, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, WS_EX_APPWINDOW, WS_EX_NOACTIVATE,
    };
    if hwnd == 0 {
        return;
    }
    let extra = WS_EX_NOACTIVATE | if taskbar { WS_EX_APPWINDOW } else { 0 };
    // SAFETY: style and z-order changes of a window of this process; nothing is activated.
    unsafe {
        let ex = GetWindowLongPtrW(hwnd as _, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd as _, GWL_EXSTYLE, ex | extra as isize);
        SetWindowPos(hwnd as _, HWND_BOTTOM, 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_FRAMECHANGED);
    }
}

/// Show a top-level window without activating it, behind the others.
#[cfg(windows)]
pub fn show_behind(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowPos, ShowWindow, HWND_BOTTOM, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_SHOWNOACTIVATE};
    if hwnd == 0 {
        return;
    }
    // SAFETY: show / z-order of a window of this process, without activation.
    unsafe {
        ShowWindow(hwnd as _, SW_SHOWNOACTIVATE);
        SetWindowPos(hwnd as _, HWND_BOTTOM, 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE);
    }
}

/// Borderless fullscreen over the window's monitor (`on`), or back exactly where it was, without
/// activating it and staying behind the other windows: what the app's own fullscreen does in
/// background test mode (winit's activates the window).
#[cfg(windows)]
pub fn fullscreen_behind(hwnd: isize, on: bool) {
    use std::sync::Mutex;
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, GetWindowRect, SetWindowLongPtrW, SetWindowPos, GWL_STYLE, HWND_BOTTOM, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOOWNERZORDER, WS_OVERLAPPEDWINDOW, WS_POPUP,
    };
    /// The window, its style and its rectangle (left, top, right, bottom) before fullscreen.
    type Saved = (isize, isize, (i32, i32, i32, i32));
    static SAVED: Mutex<Option<Saved>> = Mutex::new(None);
    if hwnd == 0 {
        return;
    }
    let mut saved = SAVED.lock().unwrap_or_else(|e| e.into_inner());
    let flags = SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_FRAMECHANGED;
    // SAFETY: style / placement of a window of this process, plain queries into stack structs.
    unsafe {
        if on {
            if saved.is_some() {
                return;
            }
            let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            let mut mi: MONITORINFO = std::mem::zeroed();
            mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if GetWindowRect(hwnd as _, &mut r) == 0 || GetMonitorInfoW(MonitorFromWindow(hwnd as _, MONITOR_DEFAULTTONEAREST), &mut mi) == 0 {
                return;
            }
            let style = GetWindowLongPtrW(hwnd as _, GWL_STYLE);
            *saved = Some((hwnd, style, (r.left, r.top, r.right, r.bottom)));
            SetWindowLongPtrW(hwnd as _, GWL_STYLE, (style & !(WS_OVERLAPPEDWINDOW as isize)) | WS_POPUP as isize);
            let m = mi.rcMonitor;
            SetWindowPos(hwnd as _, HWND_BOTTOM, m.left, m.top, m.right - m.left, m.bottom - m.top, flags);
        } else if let Some((h, style, (l, t, r, b))) = saved.take().filter(|s| s.0 == hwnd) {
            SetWindowLongPtrW(h as _, GWL_STYLE, style);
            SetWindowPos(h as _, HWND_BOTTOM, l, t, r - l, b - t, flags);
        }
    }
}

/// Hide a top-level window (no other window is activated because of it: ours never was).
#[cfg(windows)]
pub fn hide(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
    if hwnd != 0 {
        // SAFETY: hiding a window of this process.
        unsafe { ShowWindow(hwnd as _, SW_HIDE) };
    }
}

// Other platforms (macOS): window handles are not HWNDs; the background mode is a no-op there.
#[cfg(not(windows))]
pub fn keep_behind(_hwnd: isize, _taskbar: bool) {}
#[cfg(not(windows))]
pub fn show_behind(_hwnd: isize) {}
#[cfg(not(windows))]
pub fn fullscreen_behind(_hwnd: isize, _on: bool) {}
#[cfg(not(windows))]
pub fn hide(_hwnd: isize) {}
