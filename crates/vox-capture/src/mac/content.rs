//! What can be captured: ScreenCaptureKit's shareable content (displays, windows, apps), the
//! lists the picker shows, and the content filter of a target.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::AllocAnyThread;
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{SCContentFilter, SCDisplay, SCRunningApplication, SCShareableContent, SCWindow};

use super::{ffi, lock, note_screen_permission, os, SendBox};
use crate::{fit_inside, CaptureError, MonitorInfo, Result, VideoTarget, WindowInfo};

/// `SCStreamErrorUserDeclined`: Screen Recording is not allowed for this app.
pub(crate) const ERR_USER_DECLINED: isize = -3801;
/// How long ScreenCaptureKit gets to answer a content query.
const CONTENT_TIMEOUT: Duration = Duration::from_secs(5);
/// A content snapshot younger than this is reused (the picker asks once per thumbnail).
const CONTENT_REUSE: Duration = Duration::from_millis(1500);

/// The text of an `NSError` (domain, code, description) and whether it is the permission refusal.
pub(crate) fn ns_error(e: &NSError) -> (String, bool) {
    let code = e.code();
    let text = format!("{} {} ({})", e.domain(), code, e.localizedDescription());
    (text, code == ERR_USER_DECLINED)
}

/// The error for a refused Screen Recording permission (the app shows its own message from
/// `screen_capture_permission`).
pub(crate) fn permission_error(what: &str) -> CaptureError {
    note_screen_permission(false);
    CaptureError::Os(format!("{what}: Screen Recording permission not granted (System Settings > Privacy & Security > Screen & System Audio Recording)"))
}

/// A fresh snapshot of the shareable content: on-screen windows only, no desktop windows.
fn fetch() -> Result<Retained<SCShareableContent>> {
    let (tx, rx) = mpsc::sync_channel::<std::result::Result<SendBox<Retained<SCShareableContent>>, (String, bool)>>(1);
    let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        // SAFETY: the completion handler's arguments are valid (or null) for its duration.
        let r = match unsafe { Retained::retain(content) } {
            Some(c) => Ok(SendBox(c)),
            None => Err(unsafe { err.as_ref() }.map(ns_error).unwrap_or_else(|| ("no content and no error".into(), false))),
        };
        let _ = tx.try_send(r);
    });
    // SAFETY: the block lives until the call returns; ScreenCaptureKit copies it.
    unsafe { SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(true, true, &block) };
    match rx.recv_timeout(CONTENT_TIMEOUT) {
        Ok(Ok(c)) => {
            note_screen_permission(true);
            Ok(c.0)
        }
        Ok(Err((_, true))) => Err(permission_error("shareable content")),
        Ok(Err((text, false))) => Err(os("SCShareableContent", text)),
        Err(_) => Err(CaptureError::Stopped("ScreenCaptureKit did not answer the content query".into())),
    }
}

static CACHE: Mutex<Option<(Instant, SendBox<Retained<SCShareableContent>>)>> = Mutex::new(None);

/// The shareable content, reusing a snapshot of the last moment (`fresh` forces a new one).
pub(crate) fn shareable(fresh: bool) -> Result<Retained<SCShareableContent>> {
    if !fresh {
        if let Some((t, c)) = lock(&CACHE).as_ref() {
            if t.elapsed() < CONTENT_REUSE {
                return Ok(c.0.clone());
            }
        }
    }
    let c = fetch()?;
    *lock(&CACHE) = Some((Instant::now(), SendBox(c.clone())));
    Ok(c)
}

/// Window id -> owning process, from the last window list (for icons and audio).
static WINDOW_PIDS: Mutex<Option<HashMap<u32, i32>>> = Mutex::new(None);

pub(crate) fn window_pid(id: u32) -> Option<i32> {
    lock(&WINDOW_PIDS).as_ref().and_then(|m| m.get(&id).copied())
}

/// Displays in a stable order: the main display first, then left to right, top to bottom.
fn ordered_displays(content: &SCShareableContent) -> Vec<Retained<SCDisplay>> {
    // SAFETY: plain getters of an immutable snapshot.
    let mut v: Vec<Retained<SCDisplay>> = unsafe { content.displays() }.to_vec();
    let main = unsafe { ffi::CGMainDisplayID() };
    v.sort_by(|a, b| {
        let key = |d: &SCDisplay| {
            let f = unsafe { d.frame() };
            (unsafe { d.displayID() } != main, f.origin.x as i64, f.origin.y as i64)
        };
        key(a).cmp(&key(b))
    });
    v
}

/// A display's size in pixels (its current mode), or its size in points when unknown.
pub(crate) fn display_pixels(d: &SCDisplay) -> (u32, u32) {
    // SAFETY: CoreGraphics queries on a display id; the mode is released here.
    unsafe {
        let id = d.displayID();
        let mode = ffi::CGDisplayCopyDisplayMode(id);
        if !mode.is_null() {
            let (w, h) = (ffi::CGDisplayModeGetPixelWidth(mode), ffi::CGDisplayModeGetPixelHeight(mode));
            ffi::CGDisplayModeRelease(mode);
            if w > 0 && h > 0 {
                return (w as u32, h as u32);
            }
        }
        (d.width().max(1) as u32, d.height().max(1) as u32)
    }
}

/// Pixels per point of the display showing the centre of `frame` (2 on Retina screens).
fn scale_at(content: &SCShareableContent, x: f64, y: f64) -> f64 {
    let displays = ordered_displays(content);
    let hit = displays.iter().find(|d| {
        let f = unsafe { d.frame() };
        x >= f.origin.x && x < f.origin.x + f.size.width && y >= f.origin.y && y < f.origin.y + f.size.height
    });
    let d = hit.or(displays.first());
    d.map(|d| {
        let pts = unsafe { d.width() }.max(1) as f64;
        (display_pixels(d).0 as f64 / pts).clamp(1.0, 4.0)
    })
    .unwrap_or(1.0)
}

/// Windows worth listing: normal (layer 0) windows of other apps, big enough to be one.
fn listable(w: &SCWindow, own_pid: i32) -> Option<(Retained<SCRunningApplication>, i32)> {
    // SAFETY: getters of an immutable snapshot.
    unsafe {
        if w.windowLayer() != 0 || !w.isOnScreen() {
            return None;
        }
        let f = w.frame();
        if f.size.width < 48.0 || f.size.height < 48.0 {
            return None;
        }
        let app = w.owningApplication()?;
        let pid = app.processID();
        if pid <= 0 || pid == own_pid {
            return None;
        }
        Some((app, pid))
    }
}

pub fn list_monitors() -> Result<Vec<MonitorInfo>> {
    objc2::rc::autoreleasepool(|_| {
        let content = shareable(true)?;
        let main = unsafe { ffi::CGMainDisplayID() };
        Ok(ordered_displays(&content)
            .iter()
            .enumerate()
            .map(|(index, d)| {
                let id = unsafe { d.displayID() };
                let (width, height) = display_pixels(d);
                // SAFETY: plain query.
                let builtin = unsafe { ffi::CGDisplayIsBuiltin(id) } != 0;
                MonitorInfo { index, name: if builtin { "Tela integrada".into() } else { "Monitor externo".into() }, width, height, primary: id == main }
            })
            .collect())
    })
}

pub fn list_windows() -> Result<Vec<WindowInfo>> {
    objc2::rc::autoreleasepool(|_| {
        let content = shareable(true)?;
        let own = std::process::id() as i32;
        let mut out = Vec::new();
        let mut pids = HashMap::new();
        for w in unsafe { content.windows() }.iter() {
            let Some((app, pid)) = listable(&w, own) else { continue };
            // SAFETY: getters of an immutable snapshot.
            let (id, title, name) = unsafe { (w.windowID(), w.title().map(|t| t.to_string()).unwrap_or_default(), app.applicationName().to_string()) };
            let title = if title.trim().is_empty() { name.clone() } else { title };
            if title.trim().is_empty() {
                continue;
            }
            pids.insert(id, pid);
            out.push(WindowInfo { hwnd: id as isize, title, process_id: pid as u32, exe_name: name });
        }
        *lock(&WINDOW_PIDS) = Some(pids);
        Ok(out)
    })
}

/// The app behind a listed window (Go Live hover cards, v1.1): ScreenCaptureKit's name of the
/// running application (its localized name), never the window's title. The game guess only has the
/// name here (no exe path), so it is right for the well-known games only.
pub fn window_app(hwnd: isize) -> Option<crate::AppInfo> {
    let w = list_windows().ok()?.into_iter().find(|w| w.hwnd == hwnd)?;
    let name = crate::program_name(&w.exe_name, None, None);
    if name.is_empty() {
        return None;
    }
    let is_game = crate::looks_like_game(&format!("{}.exe", w.exe_name.to_lowercase()));
    Some(crate::AppInfo { name, is_game })
}

/// Only on-screen windows are listed: a listed window is never minimized.
pub fn is_window_minimized(_: isize) -> bool {
    false
}

/// A target resolved against the current content: the filter and its natural size in pixels.
pub(crate) struct Resolved {
    pub filter: Retained<SCContentFilter>,
    pub pixels: (u32, u32),
}

pub(crate) fn resolve(target: VideoTarget, fresh: bool) -> Result<Resolved> {
    let content = shareable(fresh)?;
    match target {
        VideoTarget::Monitor(i) => {
            let displays = ordered_displays(&content);
            let d = displays.get(i).ok_or_else(|| CaptureError::NotFound(format!("monitor {i}")))?;
            let none: Retained<NSArray<SCWindow>> = NSArray::new();
            // SAFETY: a display of the snapshot and an empty exclusion list.
            let filter = unsafe { SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), d, &none) };
            Ok(Resolved { filter, pixels: display_pixels(d) })
        }
        VideoTarget::Window(h) => {
            let id = u32::try_from(h).map_err(|_| CaptureError::NotFound(format!("window {h}")))?;
            let windows = unsafe { content.windows() };
            let w = windows.iter().find(|w| unsafe { w.windowID() } == id).ok_or_else(|| CaptureError::NotFound(format!("window {id}")))?;
            let f = unsafe { w.frame() };
            let s = scale_at(&content, f.origin.x + f.size.width / 2.0, f.origin.y + f.size.height / 2.0);
            let px = (((f.size.width * s).round() as u32).max(2), ((f.size.height * s).round() as u32).max(2));
            // SAFETY: a window of the snapshot.
            let filter = unsafe { SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &w) };
            Ok(Resolved { filter, pixels: px })
        }
    }
}

/// The audio filter: only `pid`'s app (`only`), or everything but that app (`!only`; our own pid
/// means everything, since our own sound is left out by `excludesCurrentProcessAudio` anyway).
pub(crate) fn audio_filter(pid: u32, only: bool) -> Result<Retained<SCContentFilter>> {
    let content = shareable(true)?;
    let displays = ordered_displays(&content);
    let d = displays.first().ok_or_else(|| CaptureError::NotFound("no display".into()))?;
    let apps = unsafe { content.applications() };
    let app = apps.iter().find(|a| unsafe { a.processID() } == pid as i32);
    let none: Retained<NSArray<SCWindow>> = NSArray::new();
    // SAFETY: a display and apps of the snapshot.
    unsafe {
        match (only, app) {
            (true, Some(a)) => {
                let list = NSArray::from_retained_slice(&[a]);
                Ok(SCContentFilter::initWithDisplay_includingApplications_exceptingWindows(SCContentFilter::alloc(), d, &list, &none))
            }
            (true, None) => Err(CaptureError::NotFound(format!("no app with pid {pid} (closed?)"))),
            (false, Some(a)) if pid != std::process::id() => {
                let list = NSArray::from_retained_slice(&[a]);
                Ok(SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(SCContentFilter::alloc(), d, &list, &none))
            }
            (false, _) => Ok(SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), d, &none)),
        }
    }
}

/// Output size for a capture of `pixels` that must fit inside the wanted box.
pub(crate) fn output_size(pixels: (u32, u32), max_w: u32, max_h: u32) -> (u32, u32) {
    fit_inside(pixels.0, pixels.1, max_w.max(2), max_h.max(2))
}
