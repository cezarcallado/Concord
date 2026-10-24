//! Windows implementation: shared helpers + target enumeration.

mod adapters;
mod appinfo;
mod audio;
pub mod bench;
mod camctl;
mod camera;
mod dshow;
mod encode;
mod enumerate;
mod icon;
mod preview;
mod thumb;
mod video;

pub use audio::{start_audio, AudioInner};
pub use camera::{camera_diagnostics, list_camera_formats, list_cameras, start_camera, CameraInner, CameraOutput};
pub use dshow::camera_property_pages;
pub use adapters::{encoder_candidates, list_gpus};
pub use appinfo::window_app;
pub use encode::{available_encoders, self_test_encoders, start_video_encoded, EncodedInner};
pub use enumerate::{list_monitors, list_windows};
pub use icon::{is_window_minimized, window_icon};
pub use thumb::GrabberInner;
pub use preview::{gpu_holders, PreviewHandle};
pub(crate) use preview::PreviewAttach;
pub use video::{start_video, VideoInner};

use std::sync::OnceLock;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent};

use crate::CaptureError;

/// Owned Win32 event handle, shareable across threads (event handles are thread-agnostic).
pub(crate) struct Event(HANDLE);

// SAFETY: a kernel event handle may be used from any thread; we only close it on drop.
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    pub fn new(manual_reset: bool) -> crate::Result<Self> {
        let h = unsafe { CreateEventW(None, manual_reset, false, None) }
            .map_err(|e| os("CreateEventW", e))?;
        Ok(Self(h))
    }
    pub fn set(&self) {
        unsafe {
            let _ = SetEvent(self.0);
        }
    }
    pub fn reset(&self) {
        unsafe {
            let _ = ResetEvent(self.0);
        }
    }
    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Owned generic handle (process, timer, ...).
pub(crate) struct OwnedHandle(pub HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Format a Windows error with context and the raw HRESULT (logs and `CaptureError::Os`).
pub(crate) fn os(ctx: &str, e: windows_core::Error) -> CaptureError {
    CaptureError::Os(format!(
        "{ctx} failed: hr=0x{:08X} {}",
        e.code().0 as u32,
        e.message()
    ))
}

fn qpc_freq() -> i64 {
    static FREQ: OnceLock<i64> = OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut f = 0i64;
        unsafe {
            let _ = QueryPerformanceFrequency(&mut f);
        }
        f.max(1)
    })
}

/// QPC "now" in microseconds. Same clock as WGC `SystemRelativeTime` and WASAPI QPC positions.
pub fn monotonic_now_us() -> i64 {
    let mut c = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
    }
    ((c as i128) * 1_000_000 / qpc_freq() as i128) as i64
}

/// Pin the process MTA for the lifetime of the process (never released).
///
/// windows-rs caches WinRT activation factories (GraphicsCaptureItem interop, capture session
/// statics, ...) process-wide. If the capture thread held the last MTA reference, its
/// `CoUninitialize` tears the apartment down while those cached proxies survive, and the next
/// `start_video` uses a dead apartment and crashes. One permanent usage reference makes per-thread
/// init/uninit harmless.
fn pin_mta() {
    static PINNED: OnceLock<()> = OnceLock::new();
    PINNED.get_or_init(|| {
        if let Err(e) = unsafe { windows::Win32::System::Com::CoIncrementMTAUsage() } {
            tracing::warn!(
                hr = format!("0x{:08X}", e.code().0 as u32),
                "CoIncrementMTAUsage failed"
            );
        }
    });
}

/// RAII apartment init for capture threads (MTA). Only uninitializes if we initialized.
pub(crate) struct ComInit(bool);

impl ComInit {
    pub fn mta() -> Self {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
        pin_mta();
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self(hr.is_ok())
    }
}

impl Drop for ComInit {
    fn drop(&mut self) {
        if self.0 {
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}

pub(crate) fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}
