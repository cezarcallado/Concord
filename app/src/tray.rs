//! Windows shell integration for the chat notifications: the tray icon (restore / quit menu and
//! the balloon that Windows 10/11 shows as a toast) and the red number on the taskbar button.
//!
//! Everything runs on one small thread with a hidden message-only window of its own, so the UI
//! thread never waits for the shell and keeps working while the main window is hidden. The app
//! talks to it through a queue (`Tray::balloon`, `Tray::set_badge`) and hears back through a
//! channel ([`TrayEvent`]) plus the repaint waker. The decisions (who deserves a notification,
//! what the badge says) are not here: see `notify.rs`.

#[cfg(not(windows))]
use std::sync::mpsc::Sender;

/// What the tray told the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// Left click, double click or "Abrir concord": show the window and focus it.
    Open,
    /// "Sair".
    Quit,
    /// The balloon (toast) was clicked: show the window on the conversation it was about.
    BalloonClicked,
}

// ---------------------------------------------------------------- the badge picture (pure)

#[cfg_attr(not(windows), allow(dead_code))]
pub const BADGE_PX: usize = 32;

/// What the red circle says: the number, "9+" from ten on, nothing for zero.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn badge_text(count: u32) -> Option<String> {
    match count {
        0 => None,
        1..=9 => Some(count.to_string()),
        _ => Some("9+".to_string()),
    }
}

/// 5x7 glyphs of the digits and the plus sign, one row per entry (bit 4 = leftmost column).
#[cfg_attr(not(windows), allow(dead_code))]
fn glyph(c: char) -> [u8; 7] {
    match c {
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        '+' => [0b00000, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0b00000],
        _ => [0; 7],
    }
}

/// A red circle with `text` in white, as 32x32 non-premultiplied BGRA pixels (`0xAARRGGBB`).
/// The glyphs are drawn at twice their size, so one or two of them fit well inside the circle.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn render_badge(text: &str) -> Vec<u32> {
    const RED: (u32, u32, u32) = (237, 66, 69);
    let n = BADGE_PX;
    let mid = n as f32 / 2.0;
    let mut px = vec![0u32; n * n];
    for y in 0..n {
        for x in 0..n {
            let (dx, dy) = (x as f32 + 0.5 - mid, y as f32 + 0.5 - mid);
            let cover = (mid - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);
            if cover > 0.0 {
                px[y * n + x] = (((cover * 255.0) as u32) << 24) | (RED.0 << 16) | (RED.1 << 8) | RED.2;
            }
        }
    }
    let glyphs: Vec<char> = text.chars().collect();
    let width = glyphs.len() * 10 + glyphs.len().saturating_sub(1) * 2;
    let (x0, y0) = ((n - width) / 2, (n - 14) / 2);
    for (i, c) in glyphs.iter().enumerate() {
        let gx = x0 + i * 12;
        for (row, bits) in glyph(*c).iter().enumerate() {
            for col in 0..5 {
                if bits & (0b10000 >> col) != 0 {
                    for (sx, sy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                        px[(y0 + row * 2 + sy) * n + gx + col * 2 + sx] = 0xFFFF_FFFF;
                    }
                }
            }
        }
    }
    px
}

#[cfg(windows)]
mod imp {
    use std::collections::VecDeque;
    use std::ffi::c_void;
    use std::sync::mpsc::{channel, Sender};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{ITaskbarList3, TaskbarList};
    use windows::Win32::UI::WindowsAndMessaging::HICON as ComHicon;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIIF_LARGE_ICON, NIIF_NOSOUND, NIIF_USER, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_BALLOONUSERCLICK, NIN_SELECT,
        NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GetClassLongPtrW, GetCursorPos, GetMessageW,
        GetSystemMetrics, GetWindowLongPtrW, LoadIconW, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SendMessageW, SetForegroundWindow, SetMenuDefaultItem, SetWindowLongPtrW, TrackPopupMenu,
        TranslateMessage,
        CREATESTRUCTW, GCLP_HICONSM, GWLP_USERDATA, HICON, HWND_MESSAGE, ICONINFO, ICON_BIG, ICON_SMALL, IDI_APPLICATION, MF_SEPARATOR, SM_CXICON, SM_CXSMICON, MF_STRING, MSG, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_COMMAND,
        WM_CONTEXTMENU, WM_DESTROY, WM_GETICON, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NCCREATE, WM_NULL, WNDCLASSW,
    };

    use super::{badge_text, render_badge, TrayEvent, BADGE_PX};

    /// The shell's callback message for the icon, and our own "look at the queue" message.
    const WM_TRAYICON: u32 = WM_APP + 1;
    const WM_QUEUE: u32 = WM_APP + 2;
    const ICON_ID: u32 = 1;
    /// Enter / Space on the focused icon (`NIN_SELECT | NINF_KEY`).
    const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
    const CMD_OPEN: usize = 1;
    const CMD_QUIT: usize = 2;

    enum Cmd {
        Balloon { title: String, body: String },
        Badge(u32),
        Shutdown,
    }

    struct Shared {
        queue: Mutex<VecDeque<Cmd>>,
    }

    /// The tray icon and the taskbar badge. Dropping it removes the icon.
    pub struct Tray {
        hwnd: isize,
        shared: Arc<Shared>,
        thread: Option<JoinHandle<()>>,
    }

    struct State {
        main: isize,
        hwnd: HWND,
        icon: isize,
        /// The logo shown in the balloon / toast (0: the system's "info" icon).
        balloon_icon: isize,
        tip: Vec<u16>,
        tx: Sender<TrayEvent>,
        wake: Arc<dyn Fn() + Send + Sync>,
        shared: Arc<Shared>,
        taskbar: Option<ITaskbarList3>,
        overlay: Option<HICON>,
        /// The "TaskbarCreated" message: the shell restarted and forgot our icon.
        taskbar_created: u32,
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn fill(dst: &mut [u16], s: &str) {
        // Always leave room for the terminating zero; cut on a char boundary with an ellipsis.
        let max = dst.len() - 1;
        let mut units: Vec<u16> = s.encode_utf16().collect();
        if units.len() > max {
            units.truncate(max - 1);
            units.push(0x2026);
        }
        dst[..units.len()].copy_from_slice(&units);
        dst[units.len()] = 0;
    }

    impl Tray {
        /// Show the icon. `main` is the app window (its taskbar button gets the badge, its icon
        /// is the tray icon); events arrive on `tx` and `wake` is called after each one.
        pub fn start(main: isize, tooltip: &str, tx: Sender<TrayEvent>, wake: impl Fn() + Send + Sync + 'static) -> Result<Tray, String> {
            // The concord logo at the tray's size (`brand.rs`), else the window's own icon, else the
            // system's default one. The balloon gets the logo at the large icon size.
            // SAFETY: plain queries on a window of this process (this is the UI thread).
            let (small, large) = unsafe { (GetSystemMetrics(SM_CXSMICON).max(16) as u32, GetSystemMetrics(SM_CXICON).max(32) as u32) };
            let balloon_icon = crate::brand::hicon(large).unwrap_or(0);
            let icon = unsafe {
                let h = main as HWND;
                let mut i = crate::brand::hicon(small).unwrap_or(0);
                if i == 0 {
                    i = SendMessageW(h, WM_GETICON, ICON_SMALL as usize, 0);
                }
                if i == 0 {
                    i = SendMessageW(h, WM_GETICON, ICON_BIG as usize, 0);
                }
                if i == 0 {
                    i = GetClassLongPtrW(h, GCLP_HICONSM) as isize;
                }
                if i == 0 {
                    i = LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) as isize;
                }
                i
            };
            let shared = Arc::new(Shared { queue: Mutex::new(VecDeque::new()) });
            let (ready_tx, ready_rx) = channel::<Result<isize, String>>();
            let (s2, tip, wake) = (shared.clone(), wide(tooltip), Arc::new(wake) as Arc<dyn Fn() + Send + Sync>);
            let thread = std::thread::Builder::new()
                .name("tray".into())
                .spawn(move || run(State { main, hwnd: std::ptr::null_mut(), icon, balloon_icon, tip, tx, wake, shared: s2, taskbar: None, overlay: None, taskbar_created: 0 }, ready_tx))
                .map_err(|e| e.to_string())?;
            match ready_rx.recv_timeout(Duration::from_secs(3)) {
                Ok(Ok(hwnd)) => Ok(Tray { hwnd, shared, thread: Some(thread) }),
                Ok(Err(e)) => Err(e),
                Err(_) => Err("the tray thread did not start".into()),
            }
        }

        fn push(&self, cmd: Cmd) {
            self.shared.queue.lock().unwrap_or_else(|e| e.into_inner()).push_back(cmd);
            // SAFETY: the message-only window lives until `drop` joins the thread.
            unsafe { PostMessageW(self.hwnd as HWND, WM_QUEUE, 0, 0) };
        }

        /// A Windows notification (the tray balloon, shown as a toast on Windows 10/11).
        pub fn balloon(&self, title: &str, body: &str) {
            self.push(Cmd::Balloon { title: title.to_string(), body: body.to_string() });
        }

        /// The number on the taskbar button; 0 removes it.
        pub fn set_badge(&self, count: u32) {
            self.push(Cmd::Badge(count));
        }

        /// Autotest: do what the shell does when the icon is clicked, the menu is used or the
        /// balloon is clicked (the same messages, through the real window procedure).
        pub fn simulate(&self, ev: TrayEvent) {
            let h = self.hwnd as HWND;
            // SAFETY: as in `push`.
            unsafe {
                match ev {
                    TrayEvent::Open => PostMessageW(h, WM_TRAYICON, ICON_ID as usize, NIN_SELECT as isize),
                    TrayEvent::Quit => PostMessageW(h, WM_COMMAND, CMD_QUIT, 0),
                    TrayEvent::BalloonClicked => PostMessageW(h, WM_TRAYICON, ICON_ID as usize, NIN_BALLOONUSERCLICK as isize),
                };
            }
        }
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            self.push(Cmd::Shutdown);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    fn run(state: State, ready: std::sync::mpsc::Sender<Result<isize, String>>) {
        // SAFETY: standard Win32 window-class / message-loop code on this thread; `state` lives in
        // a Box owned by the window (freed at WM_DESTROY).
        unsafe {
            let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
            let class = wide("concord_tray_window");
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: GetModuleHandleW(std::ptr::null()),
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class.as_ptr(),
            };
            RegisterClassW(&wc);
            let state = Box::into_raw(Box::new(state));
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                wide("concord tray").as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                state as *const c_void,
            );
            if hwnd.is_null() {
                drop(Box::from_raw(state));
                let _ = ready.send(Err("could not create the tray window".into()));
                return;
            }
            let ok = add_icon(&*state);
            let _ = ready.send(if ok { Ok(hwnd as isize) } else { Err("Shell_NotifyIcon refused the icon".into()) });
            if !ok {
                DestroyWindow(hwnd);
            }
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if com {
                CoUninitialize();
            }
        }
    }

    unsafe fn nid(st: &State) -> NOTIFYICONDATAW {
        let mut d: NOTIFYICONDATAW = std::mem::zeroed();
        d.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        d.hWnd = st.hwnd;
        d.uID = ICON_ID;
        d
    }

    unsafe fn add_icon(st: &State) -> bool {
        let mut d = nid(st);
        d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        d.uCallbackMessage = WM_TRAYICON;
        d.hIcon = st.icon as HICON;
        d.szTip[..st.tip.len().min(127)].copy_from_slice(&st.tip[..st.tip.len().min(127)]);
        if Shell_NotifyIconW(NIM_ADD, &d) == 0 {
            return false;
        }
        // Version 4: the callback's lParam carries the event in its low word (NIN_SELECT,
        // NIN_BALLOONUSERCLICK, WM_CONTEXTMENU...), the same on every Windows since Vista.
        let mut v = nid(st);
        v.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Shell_NotifyIconW(NIM_SETVERSION, &v);
        true
    }

    unsafe fn balloon(st: &State, title: &str, body: &str) {
        let mut d = nid(st);
        d.uFlags = NIF_INFO;
        fill(&mut d.szInfoTitle, title);
        fill(&mut d.szInfo, body);
        // Our own sound (or silence) is already decided by the app: the toast stays quiet. Its
        // picture is the concord logo when there is one.
        d.dwInfoFlags = NIIF_INFO | NIIF_NOSOUND;
        if st.balloon_icon != 0 {
            d.hBalloonIcon = st.balloon_icon as HICON;
            d.dwInfoFlags = NIIF_USER | NIIF_LARGE_ICON | NIIF_NOSOUND;
        }
        if Shell_NotifyIconW(NIM_MODIFY, &d) == 0 {
            tracing::warn!("tray balloon refused by the shell");
        }
    }

    /// The red number on the taskbar button (`ITaskbarList3::SetOverlayIcon`).
    unsafe fn badge(st: &mut State, count: u32) {
        if st.taskbar.is_none() {
            st.taskbar = CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_INPROC_SERVER).ok().filter(|t| t.HrInit().is_ok());
        }
        let Some(tb) = st.taskbar.clone() else {
            tracing::warn!("no taskbar list: the unread badge is not shown");
            return;
        };
        let main = windows::Win32::Foundation::HWND(st.main as *mut c_void);
        let new = badge_text(count).map(|t| (make_icon(&render_badge(&t)), t));
        let result = match &new {
            Some((icon, _)) => {
                let desc = wide(&if count == 1 { "1 mensagem não lida".to_string() } else { format!("{count} mensagens não lidas") });
                tb.SetOverlayIcon(main, ComHicon(*icon), windows::core::PCWSTR(desc.as_ptr()))
            }
            None => tb.SetOverlayIcon(main, ComHicon(std::ptr::null_mut()), windows::core::PCWSTR::null()),
        };
        match result {
            Ok(()) => tracing::info!(count, "taskbar badge set"),
            // (Before the window has a taskbar button, e.g. hidden to the tray: the app sets it
            // again once it is shown.)
            Err(e) => tracing::info!(error = %e, count, "SetOverlayIcon failed"),
        }
        if let Some(old) = st.overlay.take() {
            DestroyIcon(old);
        }
        st.overlay = new.map(|(i, _)| i);
    }

    unsafe fn make_icon(px: &[u32]) -> HICON {
        let n = BADGE_PX as i32;
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader = BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: n, biHeight: -n, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB, ..std::mem::zeroed() };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let color = CreateDIBSection(std::ptr::null_mut(), &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        if !bits.is_null() {
            std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u32, px.len());
        }
        let mask = CreateBitmap(n, n, 1, 1, std::ptr::null());
        let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info);
        DeleteObject(mask);
        DeleteObject(color);
        icon
    }

    unsafe fn menu(st: &State) -> Option<usize> {
        let m = CreatePopupMenu();
        // v1.0: sentence case, "Abrir" in bold (what a double click does), "Sair" set apart.
        AppendMenuW(m, MF_STRING, CMD_OPEN, wide("Abrir concord").as_ptr());
        SetMenuDefaultItem(m, CMD_OPEN as u32, 0);
        AppendMenuW(m, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(m, MF_STRING, CMD_QUIT, wide("Sair do concord").as_ptr());
        let mut p = POINT { x: 0, y: 0 };
        GetCursorPos(&mut p);
        // The menu closes when you click elsewhere only if its owner is the foreground window
        // (never in background test mode: an autotest takes nobody's foreground).
        if !vox_client_core::testmode::background() {
            SetForegroundWindow(st.hwnd);
        }
        let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, p.x, p.y, 0, st.hwnd, std::ptr::null());
        PostMessageW(st.hwnd, WM_NULL, 0, 0);
        DestroyMenu(m);
        (cmd > 0).then_some(cmd as usize)
    }

    fn tell(st: &State, ev: TrayEvent) {
        let _ = st.tx.send(ev);
        (st.wake)();
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if msg == WM_NCCREATE {
            let cs = &*(lp as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            let st = &mut *(cs.lpCreateParams as *mut State);
            st.hwnd = hwnd;
            st.taskbar_created = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let st = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
        if st.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let st = &mut *st;
        match msg {
            WM_TRAYICON => match (lp & 0xFFFF) as u32 {
                NIN_SELECT | NIN_KEYSELECT | WM_LBUTTONUP | WM_LBUTTONDBLCLK => tell(st, TrayEvent::Open),
                NIN_BALLOONUSERCLICK => tell(st, TrayEvent::BalloonClicked),
                WM_CONTEXTMENU => match menu(st) {
                    Some(CMD_OPEN) => tell(st, TrayEvent::Open),
                    Some(CMD_QUIT) => tell(st, TrayEvent::Quit),
                    _ => {}
                },
                _ => {}
            },
            WM_COMMAND => match wp & 0xFFFF {
                CMD_OPEN => tell(st, TrayEvent::Open),
                CMD_QUIT => tell(st, TrayEvent::Quit),
                _ => {}
            },
            WM_QUEUE => loop {
                let cmd = st.shared.queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
                match cmd {
                    Some(Cmd::Balloon { title, body }) => balloon(st, &title, &body),
                    Some(Cmd::Badge(n)) => badge(st, n),
                    Some(Cmd::Shutdown) => {
                        let d = nid(st);
                        Shell_NotifyIconW(NIM_DELETE, &d);
                        badge(st, 0);
                        DestroyWindow(hwnd);
                        break;
                    }
                    None => break,
                }
            },
            WM_DESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(st as *mut State));
                PostQuitMessage(0);
            }
            m if m == st.taskbar_created && m != 0 => {
                // Explorer restarted: put the icon back (and the taskbar object is stale).
                st.taskbar = None;
                add_icon(st);
            }
            _ => return DefWindowProcW(hwnd, msg, wp, lp),
        }
        0
    }
}

#[cfg(windows)]
pub use imp::Tray;

/// No tray off Windows: the app simply has no icon and closing the window quits.
#[cfg(not(windows))]
pub struct Tray;

#[cfg(not(windows))]
impl Tray {
    pub fn start(_main: isize, _tooltip: &str, _tx: Sender<TrayEvent>, _wake: impl Fn() + Send + Sync + 'static) -> Result<Tray, String> {
        Err("no tray on this system".into())
    }
    pub fn balloon(&self, _title: &str, _body: &str) {}
    pub fn set_badge(&self, _count: u32) {}
    pub fn simulate(&self, _ev: TrayEvent) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_text_caps_at_nine_plus() {
        assert_eq!(badge_text(0), None);
        assert_eq!(badge_text(1).as_deref(), Some("1"));
        assert_eq!(badge_text(9).as_deref(), Some("9"));
        assert_eq!(badge_text(10).as_deref(), Some("9+"));
        assert_eq!(badge_text(4000).as_deref(), Some("9+"));
    }

    fn at(px: &[u32], x: usize, y: usize) -> u32 {
        px[y * BADGE_PX + x]
    }

    #[test]
    fn badge_is_a_red_circle_with_white_digits() {
        for text in ["1", "7", "9", "9+"] {
            let px = render_badge(text);
            assert_eq!(px.len(), BADGE_PX * BADGE_PX);
            assert_eq!(at(&px, 0, 0) >> 24, 0, "{text}: the corner is transparent");
            assert_eq!(at(&px, 31, 31) >> 24, 0);
            assert_eq!(at(&px, 3, 16), 0xFF_ED4245, "{text}: the disc is the app's red");
            let white = px.iter().filter(|p| **p == 0xFFFF_FFFF).count();
            assert!(white > 20, "{text}: the digits are drawn ({white} white pixels)");
        }
        // Different numbers look different, and "9+" has more ink than "9".
        assert_ne!(render_badge("1"), render_badge("2"));
        let ink = |t: &str| render_badge(t).iter().filter(|p| **p == 0xFFFF_FFFF).count();
        assert!(ink("9+") > ink("9"));
    }

    #[test]
    fn every_glyph_fits_inside_the_circle() {
        // Corners of the widest text ("9+" is 22 px wide, 14 tall) stay inside the disc.
        let px = render_badge("9+");
        let red_or_white = |p: u32| p >> 24 == 0xFF;
        for (x, y) in [(5, 9), (26, 9), (5, 22), (26, 22)] {
            assert!(red_or_white(at(&px, x, y)), "({x},{y}) lies outside the disc");
        }
    }
}
