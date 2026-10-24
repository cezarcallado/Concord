//! What depends on the operating system and is not the same everywhere yet (v1.1, macOS phase M1).
//!
//! - Capture (screen, window, an app's sound, the camera) comes with vox-capture's macOS backend.
//!   Until it answers anything but "not supported", the controls that need it say "Em breve no
//!   Mac" instead of failing. The flags are set from what the listing calls the app makes anyway
//!   return (no extra probes, so no permission prompt), and flip on by themselves once the backend
//!   lists something. Windows never sets them.
//! - Global push-to-talk and keyboard shortcuts need a system-wide key hook (Windows: a low-level
//!   keyboard hook; macOS: the Input Monitoring / Accessibility permission, phase M4). On the Mac
//!   the shortcuts work with the window in front.
//! - Hosting a server from the app: the pinned LiveKit download is the Windows build (macOS: M4).
//! - Opening a link or a folder: the shell on Windows, `open` on macOS.

use std::sync::atomic::{AtomicBool, Ordering};

/// The label of a control that is not there on this system yet.
pub const SOON_MAC: &str = "Em breve no Mac";
/// The same in the middle of a sentence ("Câmera: em breve no Mac"; `to_lowercase` would also
/// lower the "Mac").
pub const SOON_MAC_LC: &str = "em breve no Mac";

static SCREEN_UNSUPPORTED: AtomicBool = AtomicBool::new(false);
static CAMERA_UNSUPPORTED: AtomicBool = AtomicBool::new(false);

/// What a screen / window listing said: `unsupported` when the capture backend does not exist.
pub fn note_screen_capture(unsupported: bool) {
    if SCREEN_UNSUPPORTED.swap(unsupported, Ordering::Relaxed) != unsupported {
        tracing::info!(unsupported, "screen capture availability");
    }
}

/// What the camera listing said.
pub fn note_camera(unsupported: bool) {
    if CAMERA_UNSUPPORTED.swap(unsupported, Ordering::Relaxed) != unsupported {
        tracing::info!(unsupported, "camera availability");
    }
}

/// Transmitting (screen, window or only an app's sound) is "em breve" on this system.
pub fn screen_soon() -> bool {
    !cfg!(windows) && SCREEN_UNSUPPORTED.load(Ordering::Relaxed)
}

/// The camera is "em breve" on this system.
pub fn camera_soon() -> bool {
    !cfg!(windows) && CAMERA_UNSUPPORTED.load(Ordering::Relaxed)
}

/// A vox-capture error that means "there is no capture backend here".
pub fn is_unsupported(e: &vox_capture::CaptureError) -> bool {
    matches!(e, vox_capture::CaptureError::Unsupported(_))
}

/// The same, through the core's error wrapper.
pub fn is_unsupported_any(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<vox_client_core::golive::publish::CaptureFailure>().is_some_and(|f| is_unsupported(&f.error)))
}

/// Hosting a server from the app needs a LiveKit build for this system; the app only knows the
/// pinned Windows one (macOS: phase M4).
pub const HOSTING_SOON: bool = !cfg!(windows);

/// Push-to-talk with a key that works while another app has the focus.
pub const fn global_ptt() -> bool {
    cfg!(windows)
}

/// Keyboard shortcuts ("Atalhos") that work while another app has the focus: the low-level
/// keyboard hook on Windows. macOS: only with concord's window in front (a global listener there
/// needs the Input Monitoring / Accessibility permission and a CGEventTap; not done yet).
pub const fn global_keybinds() -> bool {
    cfg!(windows)
}

/// macOS: open a URL or a folder with `open` (the default browser / Finder). Never blocks.
#[cfg(target_os = "macos")]
pub fn open(target: &std::ffi::OsStr) {
    match std::process::Command::new("/usr/bin/open").arg(target).stdin(std::process::Stdio::null()).spawn() {
        Ok(mut child) => {
            // Reap it in the background (it exits as soon as the job is handed over).
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => tracing::warn!(error = %e, "could not run open"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flags_follow_the_listings_and_never_apply_on_windows() {
        note_camera(true);
        note_screen_capture(true);
        assert_eq!(camera_soon(), !cfg!(windows));
        assert_eq!(screen_soon(), !cfg!(windows));
        // The backend landed: the next listing works and the controls come back.
        note_camera(false);
        note_screen_capture(false);
        assert!(!camera_soon() && !screen_soon());
        assert!(is_unsupported(&vox_capture::CaptureError::Unsupported("x".into())));
        assert!(!is_unsupported(&vox_capture::CaptureError::NotFound("x".into())));
        let wrapped = vox_client_core::golive::publish::capture_error("list_monitors", vox_capture::CaptureError::Unsupported("x".into()));
        assert!(is_unsupported_any(&wrapped));
        assert!(!is_unsupported_any(&anyhow::anyhow!("other")));
    }
}
