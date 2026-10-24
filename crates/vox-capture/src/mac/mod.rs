//! macOS backend.
//!
//! ```text
//! screens / windows: ScreenCaptureKit stream, scaled to the wanted size and converted to NV12
//!   (BT.709, video range) by the window server -> IOSurface-backed CVPixelBuffer -> callback
//!   (planes locked for reading; `retained_pixel_buffer` hands the buffer itself to libwebrtc,
//!   whose VideoToolbox encoder takes it without a copy)
//! app sound: a second ScreenCaptureKit stream filtered to the shared app (or to everything but
//!   us), `excludesCurrentProcessAudio` on, 48 kHz stereo float -> 10 ms i16 chunks
//! cameras: AVFoundation capture session -> NV12 CVPixelBuffer at the wanted size -> callback
//! thumbnails: one ScreenCaptureKit screenshot (macOS 14+) as BGRA; app icons: NSRunningApplication
//! ```
//!
//! Callbacks run on dispatch queues owned by the capture; a capture's queue delivers one buffer at a
//! time. Screen Recording (screens, windows, sound) and Camera are privacy permissions the person
//! grants in System Settings (see `crate::screen_capture_permission`).

mod camera;
mod content;
mod ffi;
mod stream;
mod thumb;

use std::cell::Cell;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::pixels::{downscale_nv12, Planes};
use crate::{
    fit_inside, CaptureError, EncodedStats, EncoderBackend, EncoderCandidate, EncoderControl, EncoderKind,
    FrameCallback, GpuInfo, Nv12Frame, Permission, PreviewConfig, Result, SelfTestResult,
};

pub use camera::{camera_diagnostics, camera_property_pages, list_camera_formats, list_cameras, start_camera, CameraInner, CameraOutput};
pub use content::{is_window_minimized, list_monitors, list_windows, window_app};
pub use stream::{start_audio, start_video, AudioInner, VideoInner};
pub use thumb::{window_icon, GrabberInner};

/// Apple's PCI vendor id (for the GPU entry; nothing else reads it).
const VENDOR_APPLE: u32 = 0x106B;

pub fn monotonic_now_us() -> i64 {
    ffi::host_time_us()
}

/// Lock poisoning never matters here (the data is plain state).
pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Moves an Objective-C object (immutable value objects of ScreenCaptureKit / AVFoundation, which
/// are safe to read from any thread) through a channel or into another thread.
pub(crate) struct SendBox<T>(pub T);
// SAFETY: only used for objects documented thread-safe, read (never mutated) after the move.
unsafe impl<T> Send for SendBox<T> {}
// SAFETY: as above.
unsafe impl<T> Sync for SendBox<T> {}

pub(crate) fn os(what: &str, detail: impl std::fmt::Display) -> CaptureError {
    CaptureError::Os(format!("{what}: {detail}"))
}

// ---------------------------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------------------------

/// Screen Recording as last learned: 0 unknown, 1 granted, 2 denied.
static SCREEN_PERMISSION: AtomicU8 = AtomicU8::new(0);

pub(crate) fn note_screen_permission(granted: bool) {
    let v = if granted { 1 } else { 2 };
    if SCREEN_PERMISSION.swap(v, Ordering::Relaxed) != v {
        tracing::info!(granted, "Screen Recording permission");
    }
}

pub fn screen_permission() -> Permission {
    match SCREEN_PERMISSION.load(Ordering::Relaxed) {
        1 => Permission::Granted,
        2 => Permission::Denied,
        _ => {
            // SAFETY: no arguments; a TCC lookup.
            let ok = unsafe { ffi::CGPreflightScreenCaptureAccess() };
            note_screen_permission(ok);
            if ok {
                Permission::Granted
            } else {
                Permission::Denied
            }
        }
    }
}

pub fn request_screen_permission() -> bool {
    // SAFETY: no arguments; shows the system prompt the first time.
    let ok = unsafe { ffi::CGRequestScreenCaptureAccess() };
    note_screen_permission(ok);
    ok
}

pub fn camera_permission() -> Permission {
    camera::permission()
}

pub fn request_camera_permission() -> bool {
    camera::request_permission(Duration::from_secs(60))
}

// ---------------------------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------------------------

thread_local! {
    /// The pixel buffer of the frame being delivered on this thread: (Y plane address, buffer).
    static CURRENT: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}

pub fn retained_pixel_buffer(frame: &Nv12Frame<'_>) -> Option<NonNull<c_void>> {
    let (y, pb) = CURRENT.with(|c| c.get());
    if y == 0 || y != frame.y.as_ptr() as usize {
        return None;
    }
    // SAFETY: `pb` is the buffer being delivered right now (alive until the callback returns).
    NonNull::new(unsafe { ffi::CVPixelBufferRetain(pb as ffi::CVPixelBufferRef) })
}

/// Lock an NV12 pixel buffer for reading and hand its planes to `f` (with the buffer itself
/// reachable through [`retained_pixel_buffer`] for the duration). `false` when it is not NV12.
///
/// # Safety
/// `pb` is a valid CVPixelBuffer.
pub(crate) unsafe fn with_nv12(pb: ffi::CVPixelBufferRef, timestamp_us: i64, f: impl FnOnce(&Nv12Frame<'_>)) -> bool {
    use ffi::*;
    let fmt = CVPixelBufferGetPixelFormatType(pb);
    if (fmt != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange && fmt != kCVPixelFormatType_420YpCbCr8BiPlanarFullRange) || CVPixelBufferGetPlaneCount(pb) != 2 {
        return false;
    }
    if CVPixelBufferLockBaseAddress(pb, kCVPixelBufferLock_ReadOnly) != 0 {
        return false;
    }
    let (w, h) = (CVPixelBufferGetWidth(pb), CVPixelBufferGetHeight(pb));
    let (y0, uv0) = (CVPixelBufferGetBaseAddressOfPlane(pb, 0), CVPixelBufferGetBaseAddressOfPlane(pb, 1));
    let (ys, uvs) = (CVPixelBufferGetBytesPerRowOfPlane(pb, 0), CVPixelBufferGetBytesPerRowOfPlane(pb, 1));
    let (yh, uvh) = (CVPixelBufferGetHeightOfPlane(pb, 0), CVPixelBufferGetHeightOfPlane(pb, 1));
    let ok = !y0.is_null() && !uv0.is_null() && w >= 2 && h >= 2 && ys >= w && uvs >= w;
    if ok {
        let frame = Nv12Frame {
            width: w as u32,
            height: h as u32,
            timestamp_us,
            y: std::slice::from_raw_parts(y0, ys * yh),
            y_stride: ys as u32,
            uv: std::slice::from_raw_parts(uv0, uvs * uvh),
            uv_stride: uvs as u32,
        };
        CURRENT.with(|c| c.set((y0 as usize, pb as usize)));
        f(&frame);
        CURRENT.with(|c| c.set((0, 0)));
    }
    CVPixelBufferUnlockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);
    ok
}

/// A small NV12 copy of `f` fitting inside `max_w` x `max_h` (never enlarged), into `buf`, handed
/// to `cb`.
pub(crate) fn deliver_small(f: &Nv12Frame<'_>, max_w: u32, max_h: u32, mirror: bool, buf: &mut Vec<u8>, cb: &mut FrameCallback) {
    let (ow, oh) = fit_inside(f.width, f.height, max_w.max(2), max_h.max(2));
    let src = Planes { width: f.width, height: f.height, y: f.y, y_stride: f.y_stride as usize, uv: f.uv, uv_stride: f.uv_stride as usize };
    downscale_nv12(&src, ow, oh, mirror, buf);
    let (y, uv) = buf.split_at(ow as usize * oh as usize);
    cb(&Nv12Frame { width: ow, height: oh, timestamp_us: f.timestamp_us, y, y_stride: ow, uv, uv_stride: ow });
}

// ---------------------------------------------------------------------------------------------
// Screen preview tap
// ---------------------------------------------------------------------------------------------

struct PreviewState {
    cfg: Mutex<PreviewConfig>,
    frames: AtomicU64,
}

#[derive(Clone)]
pub struct PreviewHandle(Arc<PreviewState>);

impl PreviewHandle {
    pub fn new() -> Result<Self> {
        Ok(Self(Arc::new(PreviewState { cfg: Mutex::new(PreviewConfig::default()), frames: AtomicU64::new(0) })))
    }
    pub fn set(&self, cfg: PreviewConfig) {
        *lock(&self.0.cfg) = cfg;
    }
    pub fn config(&self) -> PreviewConfig {
        *lock(&self.0.cfg)
    }
    pub fn frames(&self) -> u64 {
        self.0.frames.load(Ordering::Relaxed)
    }
    /// The preview is a small CPU copy of frames already in memory: no GPU objects.
    pub fn holds_gpu(&self) -> bool {
        false
    }
}

pub fn gpu_holders() -> usize {
    0
}

/// A capture's preview tap: at most `fps` small copies a second while the handle says `on`.
pub struct PreviewAttach {
    state: Arc<PreviewState>,
    cb: FrameCallback,
    last: Option<Instant>,
    buf: Vec<u8>,
}

impl PreviewAttach {
    pub fn new(h: &PreviewHandle, cb: FrameCallback) -> Self {
        Self { state: h.0.clone(), cb, last: None, buf: Vec::new() }
    }

    pub(crate) fn offer(&mut self, f: &Nv12Frame<'_>) {
        let cfg = *lock(&self.state.cfg);
        if !cfg.on {
            if !self.buf.is_empty() {
                self.buf = Vec::new();
            }
            return;
        }
        let period = Duration::from_micros(1_000_000 / cfg.fps.clamp(1, 30) as u64);
        if self.last.is_some_and(|t| t.elapsed() < period) {
            return;
        }
        self.last = Some(Instant::now());
        deliver_small(f, cfg.max_width, cfg.max_height, false, &mut self.buf, &mut self.cb);
        self.state.frames.fetch_add(1, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------------------------
// Encoders: VideoToolbox lives in libwebrtc
// ---------------------------------------------------------------------------------------------

/// Never constructed: there is no encoder in this crate on macOS.
pub enum EncodedInner {}

impl EncodedInner {
    pub fn control(&self) -> EncoderControl {
        match *self {}
    }
    pub fn backend(&self) -> EncoderBackend {
        match *self {}
    }
    pub fn stats(&self) -> EncodedStats {
        match *self {}
    }
    pub fn is_running(&self) -> bool {
        match *self {}
    }
}

pub fn start_video_encoded(
    _: crate::VideoTarget,
    _: crate::EncodeConfig,
    _: crate::EncodedCallback,
    _: Option<PreviewAttach>,
) -> Result<EncodedInner> {
    Err(CaptureError::Unsupported("on macOS VideoToolbox encodes inside libwebrtc (raw path)".into()))
}

/// No pre-encoded backend here (see [`start_video_encoded`]).
pub fn available_encoders() -> Vec<EncoderBackend> {
    Vec::new()
}

/// The Apple chip (one GPU; it drives every display).
pub fn list_gpus() -> Vec<GpuInfo> {
    let name = ffi::sysctl_string(c"machdep.cpu.brand_string").unwrap_or_else(|| "Apple".into());
    let os = ffi::sysctl_string(c"kern.osproductversion").unwrap_or_default();
    vec![GpuInfo {
        name,
        vendor_id: VENDOR_APPLE,
        device_id: 0,
        subsys_id: 0,
        revision: 0,
        luid: 0,
        dedicated_mb: 0,
        integrated: true,
        driver: os,
        primary: true,
        instance: 0,
    }]
}

/// VideoToolbox (in libwebrtc), then the processor (OpenH264 in libwebrtc).
pub fn encoder_candidates(gpus: &[GpuInfo]) -> Vec<EncoderCandidate> {
    let mut v: Vec<EncoderCandidate> = gpus.iter().take(1).map(|g| EncoderCandidate::hardware(EncoderKind::VideoToolbox, g)).collect();
    v.push(EncoderCandidate::software());
    v
}

/// Nothing to test: VideoToolbox runs inside libwebrtc, which falls back to OpenH264 by itself.
pub fn self_test_encoders(_: &[EncoderCandidate], _: &mut dyn FnMut(&SelfTestResult)) -> Vec<SelfTestResult> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn videotoolbox_then_the_processor() {
        let gpus = list_gpus();
        assert_eq!(gpus.len(), 1);
        assert!(gpus[0].name.starts_with("Apple"), "{}", gpus[0].name);
        let c = encoder_candidates(&gpus);
        assert_eq!(c.iter().map(|c| c.kind).collect::<Vec<_>>(), vec![EncoderKind::VideoToolbox, EncoderKind::Software]);
        assert_eq!(c[0].backend(), Some(EncoderBackend::VideoToolbox));
        assert_eq!(c[0].label_pt(&c), "Apple VideoToolbox");
        assert!(self_test_encoders(&c, &mut |_| {}).is_empty());
        assert!(start_video_encoded(crate::VideoTarget::Monitor(0), Default::default(), Box::new(|_| {}), None).is_err());
    }

    #[test]
    fn host_clock_runs_in_microseconds() {
        let a = monotonic_now_us();
        std::thread::sleep(Duration::from_millis(20));
        let d = monotonic_now_us() - a;
        assert!((15_000..200_000).contains(&d), "{d}");
    }

    #[test]
    fn frames_without_a_pixel_buffer_have_none() {
        let (y, uv) = (vec![0u8; 16], vec![128u8; 8]);
        let f = Nv12Frame { width: 4, height: 4, timestamp_us: 0, y: &y, y_stride: 4, uv: &uv, uv_stride: 4 };
        assert!(retained_pixel_buffer(&f).is_none());
    }
}
