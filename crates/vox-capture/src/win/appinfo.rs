//! The program behind a shared window (Go Live hover cards, v1.1): the exe's FileDescription /
//! ProductName from its version resource (what Task Manager shows), and the game guess on its
//! path. Read once per share or source switch; no window title is read.

use std::ffi::c_void;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

use super::enumerate::exe_path_of;
use crate::{looks_like_game, program_name, AppInfo};

/// Languages tried when the exe lists none (English and Portuguese, Unicode and Windows-1252).
const FALLBACK_LANGS: [(u16, u16); 4] = [(0x0409, 0x04B0), (0x0409, 0x04E4), (0x0416, 0x04B0), (0x0416, 0x04E4)];

pub fn window_app(hwnd: isize) -> Option<AppInfo> {
    let mut pid = 0u32;
    // SAFETY: plain query; an invalid HWND gives 0.
    unsafe { GetWindowThreadProcessId(HWND(hwnd as *mut _), Some(&mut pid)) };
    if pid == 0 {
        return None;
    }
    let path = exe_path_of(pid)?;
    let (desc, product) = version_strings(&path);
    Some(AppInfo { name: program_name(&path, desc.as_deref(), product.as_deref()), is_game: looks_like_game(&path) })
}

/// (FileDescription, ProductName) of the exe at `path`, in its first language that has them.
fn version_strings(path: &str) -> (Option<String>, Option<String>) {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let file = PCWSTR(wide.as_ptr());
    // SAFETY: `wide` is NUL-terminated and outlives the calls.
    let size = unsafe { GetFileVersionInfoSizeW(file, None) };
    if size == 0 {
        return (None, None);
    }
    let mut block = vec![0u8; size as usize];
    // SAFETY: `block` has exactly `size` bytes.
    if unsafe { GetFileVersionInfoW(file, None, size, block.as_mut_ptr() as *mut c_void) }.is_err() {
        return (None, None);
    }
    let mut langs: Vec<(u16, u16)> = Vec::new();
    if let Some(raw) = query(&block, w!("\\VarFileInfo\\Translation"), 1) {
        langs.extend(raw.chunks_exact(4).map(|c| (u16::from_le_bytes([c[0], c[1]]), u16::from_le_bytes([c[2], c[3]]))));
    }
    langs.extend(FALLBACK_LANGS);
    let read = |key: &str| {
        langs.iter().find_map(|(lang, cp)| {
            let sub: Vec<u16> = format!("\\StringFileInfo\\{lang:04x}{cp:04x}\\{key}").encode_utf16().chain(std::iter::once(0)).collect();
            let raw = query(&block, PCWSTR(sub.as_ptr()), 2)?;
            let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
            let s = String::from_utf16_lossy(&units).trim().to_string();
            (!s.is_empty()).then_some(s)
        })
    };
    (read("FileDescription"), read("ProductName"))
}

/// The bytes of one value of a version block (for strings: UTF-16 code units, NUL included).
/// `unit`: bytes per unit of the length the block reports (2 for strings, which count characters;
/// 1 for the translation table, which counts bytes).
fn query(block: &[u8], sub: PCWSTR, unit: usize) -> Option<Vec<u8>> {
    let mut ptr: *mut c_void = std::ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: `block` is a version block from GetFileVersionInfoW; the answer points inside it.
    let ok = unsafe { VerQueryValueW(block.as_ptr() as *const c_void, sub, &mut ptr, &mut len) }.as_bool();
    if !ok || ptr.is_null() || len == 0 {
        return None;
    }
    let start = block.as_ptr() as usize;
    let at = ptr as usize;
    if at < start {
        return None;
    }
    // Never past the end of the block, whatever the resource claims.
    let max = (start + block.len()).checked_sub(at)?;
    let bytes = (len as usize).saturating_mul(unit).min(max);
    // SAFETY: [at, at + bytes) lies inside `block` (checked above).
    Some(unsafe { std::slice::from_raw_parts(ptr as *const u8, bytes) }.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_programs_have_their_description() {
        // explorer.exe always has a version resource (its description is localized).
        let path = std::env::var("WINDIR").map(|w| format!("{w}\\explorer.exe")).unwrap_or_else(|_| r"C:\Windows\explorer.exe".into());
        let (desc, product) = version_strings(&path);
        assert!(desc.as_deref().is_some_and(|d| !d.is_empty()), "{desc:?}");
        assert!(product.as_deref().is_some_and(|p| p.contains("Windows")), "{product:?}");
        assert_eq!(version_strings(r"C:\nao\existe.exe"), (None, None));
        // This test's own exe has none: the file name is used.
        let me = std::env::current_exe().unwrap();
        let name = program_name(&me.to_string_lossy(), None, None);
        assert!(!name.is_empty() && !name.ends_with(".exe"));
        assert!(window_app(0).is_none());
    }
}
