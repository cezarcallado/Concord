//! The Windows clipboard for chat images: noticing Ctrl+V (egui only reports a paste when the
//! clipboard has text, and a screenshot has none), reading a pasted picture or copied files, and
//! "Copiar" in the image viewer.
//!
//! The DIB <-> picture conversions are plain functions (unit-tested without touching the real
//! clipboard).

use std::path::PathBuf;

use image::{DynamicImage, RgbaImage};

/// What the clipboard holds that the chat can attach (macOS reads text only so far).
#[cfg_attr(not(windows), allow(dead_code))]
pub enum Clip {
    Nothing,
    /// Text only: egui pastes it into the box, nothing to attach.
    Text,
    /// Files copied in Explorer.
    Files(Vec<PathBuf>),
    /// An encoded picture (the "PNG" format browsers and the Snipping Tool put there).
    Encoded(Vec<u8>),
    /// A bitmap (a screenshot) as a packed DIB, decoded later with [`dib_to_image`] (off the
    /// UI thread).
    Dib(Vec<u8>),
}

/// A packed DIB (`BITMAPINFOHEADER` / V4 / V5 + optional masks + palette + pixels) as a decoded
/// picture. The BMP decoder does the work once the 14-byte file header is put in front.
pub fn dib_to_image(dib: &[u8]) -> Option<DynamicImage> {
    if dib.len() < 40 {
        return None;
    }
    let u32_at = |i: usize| u32::from_le_bytes(dib[i..i + 4].try_into().unwrap());
    let header = u32_at(0) as usize;
    if !matches!(header, 40 | 52 | 56 | 108 | 124) || dib.len() < header {
        return None;
    }
    let bits = u16::from_le_bytes([dib[14], dib[15]]);
    let compression = u32_at(16);
    let used = u32_at(32) as usize;
    let masks = if header == 40 && matches!(compression, 3 | 6) { if compression == 6 { 16 } else { 12 } } else { 0 };
    let palette = if bits <= 8 { 4 * if used == 0 { 1usize << bits } else { used } } else { 4 * used };
    let offset = 14 + header + masks + palette;
    let mut file = Vec::with_capacity(dib.len() + 14);
    file.extend(b"BM");
    file.extend(((dib.len() + 14) as u32).to_le_bytes());
    file.extend([0u8; 4]);
    file.extend((offset as u32).to_le_bytes());
    file.extend(dib);
    let mut img = image::load_from_memory_with_format(&file, image::ImageFormat::Bmp).ok()?;
    // A 32-bit BI_RGB bitmap's fourth byte is usually junk (often all zero): opaque then.
    if let DynamicImage::ImageRgba8(b) = &mut img {
        if b.pixels().all(|p| p[3] == 0) {
            for p in b.pixels_mut() {
                p[3] = 255;
            }
        }
    }
    Some(img)
}

/// A picture as a packed 32-bit `BITMAPINFOHEADER` DIB (bottom-up BGRA), what `CF_DIB` holds.
pub fn image_to_dib(img: &RgbaImage) -> Vec<u8> {
    let (w, h) = (img.width(), img.height());
    let mut out = Vec::with_capacity(40 + (w * h * 4) as usize);
    out.extend(40u32.to_le_bytes());
    out.extend((w as i32).to_le_bytes());
    out.extend((h as i32).to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(32u16.to_le_bytes());
    out.extend(0u32.to_le_bytes()); // BI_RGB
    out.extend((w * h * 4).to_le_bytes());
    out.extend([0u8; 16]); // resolution, palette counts
    for y in (0..h).rev() {
        for x in 0..w {
            let p = img.get_pixel(x, y);
            out.extend([p[2], p[1], p[0], p[3]]);
        }
    }
    out
}

#[cfg(windows)]
mod win {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::Win32::Foundation::{GlobalFree, HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData};
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
    use windows_sys::Win32::System::Ole::{CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_INSERT, VK_MENU, VK_SHIFT};
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, DragQueryFileW, SetWindowSubclass};
    use windows_sys::Win32::UI::WindowsAndMessaging::WM_KEYDOWN;

    use super::Clip;

    static PASTE: AtomicBool = AtomicBool::new(false);
    static HOOKED: AtomicBool = AtomicBool::new(false);
    const SUBCLASS_ID: usize = 0x766f_7868; // "voxh"

    unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, _data: usize) -> LRESULT {
        if msg == WM_KEYDOWN {
            // SAFETY: plain key-state queries on the UI thread.
            let down = |vk: u16| unsafe { GetKeyState(i32::from(vk)) } < 0;
            let ctrl_v = wparam == usize::from(b'V') && down(VK_CONTROL) && !down(VK_MENU);
            let shift_ins = wparam == usize::from(VK_INSERT) && down(VK_SHIFT) && !down(VK_CONTROL);
            if ctrl_v || shift_ins {
                PASTE.store(true, Ordering::Relaxed);
            }
        }
        // SAFETY: hands the message on to winit's own window procedure.
        unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
    }

    /// Watch the main window for Ctrl+V / Shift+Insert (once).
    pub fn hook(hwnd: isize) {
        if HOOKED.swap(true, Ordering::Relaxed) {
            return;
        }
        // SAFETY: the window is ours and lives as long as the app; the subclass is never removed.
        let ok = unsafe { SetWindowSubclass(hwnd as HWND, Some(proc), SUBCLASS_ID, 0) };
        if ok == 0 {
            tracing::warn!("could not watch the window for Ctrl+V: pasting images will not work");
            HOOKED.store(false, Ordering::Relaxed);
        }
    }

    pub fn take_paste() -> bool {
        PASTE.swap(false, Ordering::Relaxed)
    }

    fn png_format() -> u32 {
        let name: Vec<u16> = "PNG\0".encode_utf16().collect();
        // SAFETY: a NUL-terminated wide string.
        unsafe { RegisterClipboardFormatW(name.as_ptr()) }
    }

    /// The clipboard, open while this lives.
    struct Open;

    impl Open {
        fn new(owner: isize) -> Option<Open> {
            for _ in 0..5 {
                // SAFETY: plain call; closed in Drop.
                if unsafe { OpenClipboard(owner as HWND) } != 0 {
                    return Some(Open);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            None
        }
    }

    impl Drop for Open {
        fn drop(&mut self) {
            // SAFETY: we opened it.
            unsafe { CloseClipboard() };
        }
    }

    /// The bytes of a clipboard format (`None` when absent).
    fn read_format(format: u32) -> Option<Vec<u8>> {
        // SAFETY: the clipboard is open (caller); the handle is the clipboard's (not freed here),
        // locked while copied.
        unsafe {
            let h = GetClipboardData(format);
            if h.is_null() {
                return None;
            }
            let size = GlobalSize(h);
            let p = GlobalLock(h) as *const u8;
            if p.is_null() || size == 0 || size > 512 * 1024 * 1024 {
                if !p.is_null() {
                    GlobalUnlock(h);
                }
                return None;
            }
            let v = std::slice::from_raw_parts(p, size).to_vec();
            GlobalUnlock(h);
            Some(v)
        }
    }

    fn read_files() -> Option<Vec<PathBuf>> {
        // SAFETY: the clipboard is open; the HDROP stays owned by the clipboard.
        unsafe {
            let h = GetClipboardData(u32::from(CF_HDROP));
            if h.is_null() {
                return None;
            }
            let n = DragQueryFileW(h, u32::MAX, std::ptr::null_mut(), 0);
            let mut out = Vec::new();
            for i in 0..n.min(32) {
                let len = DragQueryFileW(h, i, std::ptr::null_mut(), 0);
                let mut buf = vec![0u16; len as usize + 1];
                let got = DragQueryFileW(h, i, buf.as_mut_ptr(), buf.len() as u32);
                out.push(PathBuf::from(String::from_utf16_lossy(&buf[..got as usize])));
            }
            Some(out)
        }
    }

    pub fn read(owner: isize) -> Clip {
        let Some(_open) = Open::new(owner) else { return Clip::Nothing };
        let has = |f: u32| unsafe { IsClipboardFormatAvailable(f) } != 0;
        if has(u32::from(CF_HDROP)) {
            if let Some(files) = read_files() {
                return Clip::Files(files);
            }
        }
        // Text on the clipboard (Office, Excel copy both a picture and text): the text wins.
        if has(u32::from(CF_UNICODETEXT)) {
            return Clip::Text;
        }
        let png = png_format();
        if png != 0 && has(png) {
            if let Some(b) = read_format(png) {
                return Clip::Encoded(b);
            }
        }
        for f in [CF_DIBV5, CF_DIB] {
            if has(u32::from(f)) {
                if let Some(dib) = read_format(u32::from(f)) {
                    return Clip::Dib(dib);
                }
            }
        }
        Clip::Nothing
    }

    /// What "Colar" would paste (the formats are only looked at, the clipboard is not opened), in
    /// [`read`]'s order, so the menu does what Ctrl+V does: files, then text, then a picture.
    pub fn paste_kind() -> super::PasteKind {
        // SAFETY: plain queries.
        let has = |f: u32| unsafe { IsClipboardFormatAvailable(f) } != 0;
        if has(u32::from(CF_HDROP)) {
            return super::PasteKind::Image;
        }
        if has(u32::from(CF_UNICODETEXT)) {
            return super::PasteKind::Text;
        }
        let png = png_format();
        if (png != 0 && has(png)) || has(u32::from(CF_DIB)) || has(u32::from(CF_DIBV5)) {
            return super::PasteKind::Image;
        }
        super::PasteKind::Nothing
    }

    /// The text on the clipboard.
    pub fn read_text(owner: isize) -> Option<String> {
        let _open = Open::new(owner)?;
        let bytes = read_format(u32::from(CF_UNICODETEXT))?;
        let wide: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).take(super::PASTE_MAX_UNITS).collect();
        Some(String::from_utf16_lossy(&wide))
    }

    /// Put a moveable global copy of `data` on the clipboard as `format`.
    fn put(format: u32, data: &[u8]) -> bool {
        // SAFETY: a fresh allocation of the right size, filled while locked; ownership passes to
        // the clipboard on success, freed here otherwise.
        unsafe {
            let h = GlobalAlloc(GMEM_MOVEABLE, data.len());
            if h.is_null() {
                return false;
            }
            let p = GlobalLock(h) as *mut u8;
            if p.is_null() {
                GlobalFree(h);
                return false;
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            GlobalUnlock(h);
            if SetClipboardData(format, h).is_null() {
                GlobalFree(h);
                return false;
            }
            true
        }
    }

    pub fn write(owner: isize, dib: &[u8], png: &[u8]) -> Result<(), String> {
        let Some(_open) = Open::new(owner) else { return Err("A área de transferência está ocupada.".into()) };
        // SAFETY: the clipboard is open.
        unsafe { EmptyClipboard() };
        let ok_dib = put(u32::from(CF_DIB), dib);
        let fmt = png_format();
        let ok_png = fmt != 0 && put(fmt, png);
        if ok_dib || ok_png { Ok(()) } else { Err("Não deu para copiar a imagem.".into()) }
    }
}

/// Start noticing Ctrl+V in the main window.
pub fn hook(hwnd: isize) {
    #[cfg(windows)]
    win::hook(hwnd);
    #[cfg(not(windows))]
    let _ = hwnd;
}

/// Ctrl+V (or Shift+Insert) was pressed since the last call.
pub fn take_paste() -> bool {
    #[cfg(windows)]
    return win::take_paste();
    #[cfg(not(windows))]
    false
}

/// What the clipboard holds now.
pub fn read(owner: isize) -> Clip {
    #[cfg(windows)]
    return win::read(owner);
    #[cfg(not(windows))]
    {
        let _ = owner;
        Clip::Nothing
    }
}

/// What "Colar" in a text box's menu would paste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub enum PasteKind {
    Nothing,
    /// Text (it goes into the box, as Ctrl+V does).
    Text,
    /// A picture or files (they go to the chat's attachments, as Ctrl+V does).
    Image,
}

/// What the clipboard would paste now (cheap: run when a text box's menu opens).
pub fn paste_kind() -> PasteKind {
    #[cfg(windows)]
    return win::paste_kind();
    #[cfg(target_os = "macos")]
    return if mac_text().is_some_and(|t| !t.is_empty()) { PasteKind::Text } else { PasteKind::Nothing };
    #[cfg(not(any(windows, target_os = "macos")))]
    PasteKind::Nothing
}

/// "Colar" takes at most this much text (UTF-16 units; a message holds far less): a huge clipboard
/// must not freeze the text box.
#[cfg(any(windows, target_os = "macos"))]
const PASTE_MAX_UNITS: usize = 100_000;

/// The text on the clipboard ("Colar" in a text box's menu), at most [`PASTE_MAX_UNITS`].
pub fn read_text(owner: isize) -> Option<String> {
    #[cfg(windows)]
    return win::read_text(owner);
    #[cfg(not(windows))]
    {
        let _ = owner;
        #[cfg(target_os = "macos")]
        return mac_text().map(|t| t.chars().take(PASTE_MAX_UNITS).collect());
        #[cfg(not(target_os = "macos"))]
        None
    }
}

/// macOS: the pasteboard's text, through `pbpaste`.
#[cfg(target_os = "macos")]
fn mac_text() -> Option<String> {
    let out = std::process::Command::new("/usr/bin/pbpaste").stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// "Copiar": the picture as a bitmap (every app) and as PNG (keeps transparency).
pub fn write_image(owner: isize, img: &RgbaImage) -> Result<(), String> {
    let dib = image_to_dib(img);
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(img.clone()).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).map_err(|e| e.to_string())?;
    #[cfg(windows)]
    return win::write(owner, &dib, &png);
    #[cfg(not(windows))]
    {
        let _ = (owner, dib, png);
        Err(format!("Copiar imagem: {}.", crate::platform::SOON_MAC_LC))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dibs_round_trip() {
        let img = RgbaImage::from_fn(37, 21, |x, y| image::Rgba([(x * 6) as u8, (y * 11) as u8, 77, if x < 5 { 128 } else { 255 }]));
        let dib = image_to_dib(&img);
        assert_eq!(dib.len(), 40 + 37 * 21 * 4);
        let back = dib_to_image(&dib).unwrap().to_rgba8();
        assert_eq!((back.width(), back.height()), (37, 21));
        assert_eq!(back.get_pixel(20, 3), img.get_pixel(20, 3), "rows come back in order");
    }

    #[test]
    fn a_24_bit_top_down_screenshot_dib_reads() {
        // 3x2, 24-bit, top-down (negative height), rows padded to 4 bytes.
        let mut dib = Vec::new();
        dib.extend(40u32.to_le_bytes());
        dib.extend(3i32.to_le_bytes());
        dib.extend((-2i32).to_le_bytes());
        dib.extend(1u16.to_le_bytes());
        dib.extend(24u16.to_le_bytes());
        dib.extend([0u8; 24]);
        for row in [[255u8, 0, 0], [0, 255, 0]] {
            for _ in 0..3 {
                dib.extend(row); // BGR
            }
            dib.extend([0u8; 3]); // 9 bytes -> 12
        }
        let img = dib_to_image(&dib).unwrap().to_rgb8();
        assert_eq!(img.get_pixel(0, 0).0, [0, 0, 255], "first row is blue (top-down)");
        assert_eq!(img.get_pixel(2, 1).0, [0, 255, 0]);
    }

    #[test]
    fn a_32_bit_dib_with_junk_alpha_is_opaque() {
        let img = RgbaImage::from_pixel(4, 4, image::Rgba([10, 20, 30, 0]));
        let back = dib_to_image(&image_to_dib(&img)).unwrap().to_rgba8();
        assert!(back.pixels().all(|p| p[3] == 255));
        assert!(dib_to_image(b"junk").is_none());
        assert!(dib_to_image(&[0u8; 64]).is_none());
    }
}
