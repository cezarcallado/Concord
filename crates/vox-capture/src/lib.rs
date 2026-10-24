//! Screen/window capture and per-process audio capture for Go-Live style streaming.
//!
//! CONTRACT — `vox-client-core` codes against exactly this public API. Changing a signature
//! here requires updating `docs/CONTRACTS.md` and the client.
//!
//! Video: Windows.Graphics.Capture -> D3D11 texture -> GPU BGRA->NV12 conversion
//! (ID3D11VideoProcessor, also does the downscale) -> one small staging readback -> callback.
//! Audio: WASAPI process loopback (AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK), so only the
//! shared app's sound is sent (or everything except our own voice chat).
//!
//! Callbacks run on capture threads and must return fast (copy and hand off). Never log per
//! frame — use `vox_log::RateLimiter` for periodic stats.
//!
//! macOS (`mac` module): ScreenCaptureKit for screens, windows and the shared app's sound,
//! AVFoundation for cameras. Frames are IOSurface-backed NV12 `CVPixelBuffer`s scaled by
//! ScreenCaptureKit / AVFoundation; [`retained_pixel_buffer`] hands one to libwebrtc's
//! VideoToolbox encoder without a copy. There is no encoder in this crate on macOS (the
//! [`EncoderBackend::VideoToolbox`] candidate runs inside libwebrtc on the raw path).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("target not found: {0}")]
    NotFound(String),
    #[error("not supported on this system: {0}")]
    Unsupported(String),
    #[cfg_attr(not(target_os = "macos"), error("windows api error: {0}"))]
    #[cfg_attr(target_os = "macos", error("macos api error: {0}"))]
    Os(String),
    #[error("capture stopped: {0}")]
    Stopped(String),
}

pub type Result<T> = std::result::Result<T, CaptureError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    /// Raw HWND value.
    pub hwnd: isize,
    pub title: String,
    pub process_id: u32,
    /// e.g. `chrome.exe`
    pub exe_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInfo {
    pub index: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoTarget {
    Monitor(usize),
    Window(isize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoConfig {
    pub max_fps: u32,
    /// Frames are downscaled on the GPU to fit inside max_width x max_height, keeping aspect.
    pub max_width: u32,
    pub max_height: u32,
    pub capture_cursor: bool,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            max_fps: 30,
            max_width: 1920,
            max_height: 1080,
            capture_cursor: true,
        }
    }
}

/// One captured frame in NV12 (Y plane + interleaved UV plane, half resolution).
/// Borrowed: valid only during the callback.
pub struct Nv12Frame<'a> {
    pub width: u32,
    pub height: u32,
    /// Monotonic microseconds.
    pub timestamp_us: i64,
    pub y: &'a [u8],
    pub y_stride: u32,
    pub uv: &'a [u8],
    pub uv_stride: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioTarget {
    /// Only audio produced by this process and its children (the "Go Live" app audio).
    ProcessTree { pid: u32 },
    /// All system audio except this process tree. Use our own pid when sharing a whole
    /// monitor, so viewers never hear the voice chat echoed back.
    SystemExcludingProcessTree { pid: u32 },
}

/// Interleaved PCM, always 48 kHz stereo i16, delivered in 10 ms chunks (480 frames).
pub struct AudioChunk<'a> {
    pub samples: &'a [i16],
    pub sample_rate: u32,
    pub channels: u16,
    pub timestamp_us: i64,
}

pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: u16 = 2;
pub const AUDIO_CHUNK_FRAMES: usize = 480;

pub type FrameCallback = Box<dyn FnMut(&Nv12Frame<'_>) + Send + 'static>;
pub type AudioCallback = Box<dyn FnMut(&AudioChunk<'_>) + Send + 'static>;

/// Running video capture. Dropping it stops capture and joins its thread.
pub struct VideoCapture {
    inner: imp::VideoInner,
}

/// Running audio capture. Dropping it stops capture and joins its thread.
pub struct AudioCapture {
    inner: imp::AudioInner,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct VideoStats {
    pub frames_delivered: u64,
    pub frames_dropped: u64,
    pub avg_gpu_convert_us: u32,
}

impl VideoCapture {
    pub fn stats(&self) -> VideoStats {
        self.inner.stats()
    }

    /// `false` once the capture thread has exited on its own (target window closed, monitor
    /// gone, GPU device lost). The reason is logged. Dropping is still required.
    pub fn is_running(&self) -> bool {
        self.inner.is_running()
    }
}

impl AudioCapture {
    /// `false` once the capture thread has exited on its own (audio stream error). The reason
    /// is logged. Dropping is still required.
    pub fn is_running(&self) -> bool {
        self.inner.is_running()
    }
}

/// Current time on the same monotonic clock as `Nv12Frame::timestamp_us` and
/// `AudioChunk::timestamp_us` (QueryPerformanceCounter, microseconds). Useful for A/V alignment.
pub fn monotonic_now_us() -> i64 {
    imp::monotonic_now_us()
}

pub fn list_monitors() -> Result<Vec<MonitorInfo>> {
    imp::list_monitors()
}

/// Visible, capturable top-level windows (no tool windows, no our-own windows).
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    imp::list_windows()
}

pub fn start_video(
    target: VideoTarget,
    cfg: VideoConfig,
    on_frame: FrameCallback,
) -> Result<VideoCapture> {
    imp::start_video(target, cfg, on_frame, None).map(|inner| VideoCapture { inner })
}

/// [`start_video`] plus a screen preview tap (see [`ScreenPreview`]).
pub fn start_video_with_preview(
    target: VideoTarget,
    cfg: VideoConfig,
    on_frame: FrameCallback,
    preview: Option<(&ScreenPreview, FrameCallback)>,
) -> Result<VideoCapture> {
    let attach = preview.map(|(p, cb)| imp::PreviewAttach::new(&p.inner, cb));
    imp::start_video(target, cfg, on_frame, attach).map(|inner| VideoCapture { inner })
}

pub fn start_audio(target: AudioTarget, on_audio: AudioCallback) -> Result<AudioCapture> {
    imp::start_audio(target, on_audio).map(|inner| AudioCapture { inner })
}

// ---------- Hardware-encoded video (Phase 3) ----------
//
// WGC texture -> GPU scale/convert -> hardware H.264 encoder on the same D3D11 device ->
// Annex-B access units on the CPU. No raw-frame readback and no CPU encode. The client
// publishes these as pre-encoded frames. If `available_encoders()` is empty, the client falls
// back to `start_video` + software encode.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderBackend {
    /// Best available hardware encoder for the GPU that owns the captured output.
    Auto,
    /// NVIDIA NVENC through nvEncodeAPI64.dll (loaded at runtime).
    Nvenc,
    /// Media Foundation hardware H.264 MFT (AMD AMF, Intel QSV, NVIDIA).
    MediaFoundation,
    /// macOS: Apple's hardware H.264 encoder, used by libwebrtc itself on the raw path
    /// ([`start_video`] frames are IOSurface-backed and reach it without a copy). Never a
    /// `start_video_encoded` backend.
    #[cfg(target_os = "macos")]
    VideoToolbox,
}

/// What the encoder gives up first when the network target can't carry the configured quality
/// (WebRTC congestion control lowering the bitrate through `EncoderControl::set_bitrate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DegradationPreference {
    /// "Priorizar nitidez": below a bits-per-pixel threshold, lower the frame rate first
    /// (60 -> 30 -> 20 -> 15) so every frame keeps its detail; restored with hysteresis when the
    /// bandwidth comes back.
    #[default]
    Sharpness,
    /// "Priorizar fluidez": keep the frame rate, accept softer frames.
    Smoothness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeConfig {
    pub video: VideoConfig,
    /// Initial target bitrate. Changed later with `EncoderControl::set_bitrate`. The encoder
    /// spends it at the real input rate (window capture delivers frames only on change).
    pub bitrate_bps: u32,
    pub backend: EncoderBackend,
    /// Added in v0.2 (item 5).
    pub degradation: DegradationPreference,
    /// Adapter (LUID, see [`GpuInfo::luid`]) whose encoder to use; the capture's D3D11 device is
    /// created there too, even when another adapter drives the monitor (hybrid laptops: the panel
    /// on the iGPU, the encoder on the NVIDIA). `None` = the monitor's adapter (v0.6 behaviour).
    pub adapter_luid: Option<u64>,
}

impl Default for EncodeConfig {
    fn default() -> Self {
        Self {
            video: VideoConfig::default(),
            bitrate_bps: 8_000_000,
            backend: EncoderBackend::Auto,
            degradation: DegradationPreference::default(),
            adapter_luid: None,
        }
    }
}

/// One H.264 access unit, Annex-B (start codes), baseline/constrained-baseline or main profile,
/// no B-frames. Keyframes are IDR with SPS/PPS in front. Borrowed: valid only during the
/// callback. The resolution may change mid-stream (window resize). The first frame at a new
/// size is always a keyframe.
pub struct EncodedFrame<'a> {
    pub data: &'a [u8],
    pub keyframe: bool,
    pub width: u32,
    pub height: u32,
    /// Capture time, same clock as `monotonic_now_us()`.
    pub timestamp_us: i64,
}

pub type EncodedCallback = Box<dyn FnMut(&EncodedFrame<'_>) + Send + 'static>;

/// Feedback channel from the network side (keyframe requests, congestion-control bitrate)
/// to the encoder thread. Cheap to clone; all methods are lock-free.
#[derive(Clone, Default)]
pub struct EncoderControl {
    shared: Arc<ControlShared>,
}

#[derive(Default)]
struct ControlShared {
    keyframe: AtomicBool,
    /// 0 = no pending change.
    bitrate_bps: AtomicU32,
}

impl EncoderControl {
    /// The next encoded frame will be an IDR keyframe.
    pub fn request_keyframe(&self) {
        self.shared.keyframe.store(true, Ordering::Relaxed);
    }

    /// Reconfigure the target bitrate without restarting the stream. Values are clamped by
    /// the encoder to a sane range.
    pub fn set_bitrate(&self, bps: u32) {
        self.shared.bitrate_bps.store(bps.max(1), Ordering::Relaxed);
    }

    /// Encoder side: is a keyframe request waiting (without consuming it)?
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn keyframe_requested(&self) -> bool {
        self.shared.keyframe.load(Ordering::Relaxed)
    }

    /// Encoder side: consume a pending keyframe request.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn take_keyframe_request(&self) -> bool {
        self.shared.keyframe.swap(false, Ordering::Relaxed)
    }

    /// Encoder side: consume a pending bitrate change.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn take_bitrate_change(&self) -> Option<u32> {
        match self.shared.bitrate_bps.swap(0, Ordering::Relaxed) {
            0 => None,
            bps => Some(bps),
        }
    }
}

/// Holds back delta frames until the next IDR once the reference chain broke.
///
/// When an access unit is lost between the encoder and the network (WebRTC's frame dropper
/// discarding a pre-encoded frame, a frame the video source refused, an encoder error), every
/// following P-frame references a picture viewers never got. Hardware encoders (Media Foundation
/// MFTs especially) are asynchronous: frames already in their pipeline come out as such P-frames
/// even after an IDR was requested. Sent anyway they decode to garbage or fail to decode, and
/// the decoder's errors make viewers ask for keyframes again and again (a PLI storm that eats the
/// bitrate). So after a loss nothing but the next IDR goes out: a short freeze instead.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IdrGate {
    awaiting: bool,
    /// Delta frames held back so far.
    pub withheld: u64,
}

impl IdrGate {
    /// The chain broke: hold deltas until a keyframe. Returns `true` if it was not held already
    /// (the caller asks the encoder for an IDR then).
    pub fn arm(&mut self) -> bool {
        !std::mem::replace(&mut self.awaiting, true)
    }

    /// May this frame go out? A keyframe always does (and reopens the gate).
    pub fn pass(&mut self, keyframe: bool) -> bool {
        if keyframe {
            self.awaiting = false;
            true
        } else if self.awaiting {
            self.withheld += 1;
            false
        } else {
            true
        }
    }

    pub fn awaiting(&self) -> bool {
        self.awaiting
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct EncodedStats {
    pub frames_encoded: u64,
    pub frames_dropped: u64,
    pub keyframes: u64,
    /// Submit-to-output latency of the encoder, averaged.
    pub avg_encode_us: u32,
    /// Measured output bitrate (last second).
    pub bitrate_bps: u32,
    /// Network target (initial config or the last `set_bitrate`).
    pub target_bitrate_bps: u32,
    /// Bitrate configured on the encoder: the target scaled by configured/real input rate.
    pub encoder_bitrate_bps: u32,
    /// Frame-rate cap of the degradation policy (the configured rate when not degraded).
    pub fps_cap: u32,
    /// Keyframe requests folded into a nearby IDR instead of making another one.
    pub keyframe_requests_coalesced: u64,
}

/// Running hardware-encoded capture. Dropping it stops capture and joins its thread(s).
pub struct EncodedVideoCapture {
    inner: imp::EncodedInner,
}

impl EncodedVideoCapture {
    pub fn control(&self) -> EncoderControl {
        self.inner.control()
    }

    /// The backend actually in use (never `Auto`).
    pub fn backend(&self) -> EncoderBackend {
        self.inner.backend()
    }

    pub fn stats(&self) -> EncodedStats {
        self.inner.stats()
    }

    /// Same meaning as `VideoCapture::is_running`.
    pub fn is_running(&self) -> bool {
        self.inner.is_running()
    }
}

/// Hardware encoders usable on the primary monitor's adapter, best first. Empty means there's no
/// hardware encoder: use `start_video` and software encoding instead. Probing is cached after the
/// first call. The client chooses among every adapter's encoders with [`encoder_candidates`].
pub fn available_encoders() -> Vec<EncoderBackend> {
    imp::available_encoders()
}

// ---------- Automatic encoder choice (v0.7) ----------
//
// Every GPU's encoders (NVENC per NVIDIA adapter, the Media Foundation hardware MFT per adapter)
// plus the processor, a static ranking, and a self-test that encodes synthetic frames and decodes
// them with the Microsoft software H.264 decoder. The client caches the results and picks.

pub mod gpu;
pub use gpu::{
    CandidateStatus, EncoderCandidate, EncoderKind, GpuInfo, GpuVendor, RankedCandidate, SelfTestResult, TestOutcome,
};

/// Hardware display adapters (DXGI order; software adapters skipped). Cheap (a few ms).
pub fn list_gpus() -> Vec<GpuInfo> {
    imp::list_gpus()
}

/// Every encoder candidate of this machine in static order, the processor last. Loads the NVENC
/// library and enumerates (never activates) the MFTs: tens of ms, call it off the UI thread.
pub fn encoder_candidates(gpus: &[GpuInfo]) -> Vec<EncoderCandidate> {
    imp::encoder_candidates(gpus)
}

/// Self-test every candidate (in the given order, which must be static order, see
/// [`gpu::should_test`]), on the calling thread: about half a second per hardware candidate.
/// `on_result` gets each result as soon as it is known.
pub fn self_test_encoders(cands: &[EncoderCandidate], on_result: &mut dyn FnMut(&SelfTestResult)) -> Vec<SelfTestResult> {
    imp::self_test_encoders(cands, on_result)
}

/// Returns `CaptureError::Unsupported` if the requested backend (or, for `Auto`, any hardware
/// encoder) is unavailable.
pub fn start_video_encoded(
    target: VideoTarget,
    cfg: EncodeConfig,
    on_frame: EncodedCallback,
) -> Result<EncodedVideoCapture> {
    imp::start_video_encoded(target, cfg, on_frame, None).map(|inner| EncodedVideoCapture { inner })
}

/// [`start_video_encoded`] plus a screen preview tap (see [`ScreenPreview`]).
pub fn start_video_encoded_with_preview(
    target: VideoTarget,
    cfg: EncodeConfig,
    on_frame: EncodedCallback,
    preview: Option<(&ScreenPreview, FrameCallback)>,
) -> Result<EncodedVideoCapture> {
    let attach = preview.map(|(p, cb)| imp::PreviewAttach::new(&p.inner, cb));
    imp::start_video_encoded(target, cfg, on_frame, attach).map(|inner| EncodedVideoCapture { inner })
}

// ---------- Screen preview tap (v0.3, item 8a) ----------
//
// A cheap picture of what a running screen/window capture shows, for the sharer's own tile.
// Taken on the capture thread, on the GPU: the converter's NV12 output (already made for the
// encoder) is downscaled by a second video processor to about the tile size, copied to a small
// staging ring and handed to `on_preview` as a borrowed `Nv12Frame`, at most `fps` times a second.
// It never decodes our own encoded stream, adds no latency or waiting to the encoder (it only
// reads the converter output after the encoder got its frame), and allocates nothing per frame.
// Works the same with the hardware encoder and the raw path. Off (the default) costs nothing.

/// Settings of a [`ScreenPreview`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewConfig {
    /// Produce frames at all. Off = no GPU work and no readback.
    pub on: bool,
    /// Upper bound of the frame rate (1..=30). A screen that does not change sends no frames.
    pub fps: u32,
    /// Frames fit inside this size (never upscaled; the capture's own size is the limit).
    pub max_width: u32,
    pub max_height: u32,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self { on: false, fps: 5, max_width: 640, max_height: 360 }
    }
}

/// Preview taps (of any capture in this process) that hold GPU objects right now: a second video
/// processor, its output texture, the staging ring. Zero while no preview is on (a preview that
/// stays off for a couple of seconds gives them back).
pub fn preview_gpu_holders() -> usize {
    imp::gpu_holders()
}

/// Settings handle of one capture's preview tap: create it, pass it (with the frame callback) to
/// [`start_video_with_preview`] / [`start_video_encoded_with_preview`], and change it at any time
/// from any thread (tile hidden -> off, focused -> bigger and faster). One handle per capture.
#[derive(Clone)]
pub struct ScreenPreview {
    inner: imp::PreviewHandle,
}

impl ScreenPreview {
    /// Off, 5 fps, 640x360.
    pub fn new() -> Result<Self> {
        imp::PreviewHandle::new().map(|inner| Self { inner })
    }

    pub fn set(&self, cfg: PreviewConfig) {
        self.inner.set(cfg)
    }

    pub fn config(&self) -> PreviewConfig {
        self.inner.config()
    }

    /// Frames delivered to the callback so far.
    pub fn frames(&self) -> u64 {
        self.inner.frames()
    }

    /// Does the tap hold GPU objects (a second video processor, an output texture, the staging
    /// ring) right now? Off for a couple of seconds gives them all back; on builds them again.
    pub fn holds_gpu(&self) -> bool {
        self.inner.holds_gpu()
    }
}

// ---------- Webcam (v0.3, item 11) ----------
//
// Media Foundation source reader (async, D3D11 device manager: decoder output stays on the GPU)
// -> the same GPU video processor (camera YCbCr -> BT.709 NV12, scale) -> the same hardware
// encoder sink as the screen path (`EncodedFrame` / `EncoderControl` contract unchanged), or an
// NV12 readback (`start_camera_raw`) on machines without a hardware encoder. An optional small
// NV12 preview readback feeds the local self-view without decoding our own stream.

/// A video capture device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraInfo {
    /// Stable id (the device's symbolic link, or [`SYNTHETIC_CAMERA_ID`]).
    pub id: String,
    /// Friendly name, e.g. "HD Pro Webcam C920".
    pub name: String,
}

/// The built-in test-pattern "camera": no device is opened, frames go through the same upload ->
/// convert -> encode path. Listed by [`list_cameras`] when `VOX_FAKE_CAMERA=1`, and always
/// accepted by the `start_camera_*` functions.
pub const SYNTHETIC_CAMERA_ID: &str = "vox:synthetic-camera";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CameraConfig {
    /// Wanted size and rate. The closest native camera mode is opened (preferring one that
    /// reaches `fps` and is at least this big) and the output is scaled to fit inside
    /// `width` x `height`, keeping the camera's aspect.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Initial encoder target; later changed through `EncoderControl::set_bitrate`.
    pub bitrate_bps: u32,
    pub backend: EncoderBackend,
    /// The local preview frames fit inside this size (NV12, delivered to `on_preview`).
    pub preview_max_width: u32,
    pub preview_max_height: u32,
    /// Open exactly this native mode (v0.6). `None` (or a mode the camera does not offer) is
    /// automatic: the closest mode to `width` x `height` @ `fps`.
    pub mode: Option<CameraMode>,
    /// Mirror / rotate / digital zoom, done by the GPU video processor (changeable while
    /// running: [`CameraCapture::set_transform`]).
    pub transform: CameraTransform,
    /// Adapter (LUID) for the camera's D3D11 device and encoder (v0.7); `None` = the adapter of
    /// the primary monitor.
    pub adapter_luid: Option<u64>,
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_bps: 1_200_000,
            backend: EncoderBackend::Auto,
            preview_max_width: 640,
            preview_max_height: 360,
            mode: None,
            transform: CameraTransform::default(),
            adapter_luid: None,
        }
    }
}

/// What a running camera actually delivers.
#[derive(Debug, Clone, PartialEq)]
pub struct CameraFormat {
    /// Device name (or "synthetic").
    pub device: String,
    /// Native mode opened on the device.
    pub native_width: u32,
    pub native_height: u32,
    pub native_fps: f32,
    /// "NV12", "YUY2", "MJPG" (decoded by Media Foundation) or "synthetic".
    pub subtype: String,
    /// Frames arrive as GPU textures (hardware decode or a D3D-aware source).
    pub gpu_frames: bool,
    /// Encoded / raw output size.
    pub width: u32,
    pub height: u32,
}

/// Why a camera could not start or stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraFault {
    /// No camera with that id / name, or no camera at all.
    NotFound,
    /// Another program holds the camera.
    InUse,
    /// Blocked by the Windows privacy setting (camera access for desktop apps).
    AccessDenied,
    /// Unplugged or the driver went away while running.
    Disconnected,
    /// Opened, but no picture came.
    NoFrames,
    /// No usable format / no GPU video processor / no hardware encoder.
    Unsupported,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraError {
    pub fault: CameraFault,
    /// English detail for logs (HRESULT and the failing call).
    pub detail: String,
}

impl std::fmt::Display for CameraError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "camera {:?}: {}", self.fault, self.detail)
    }
}

impl std::error::Error for CameraError {}

impl From<CameraError> for CaptureError {
    fn from(e: CameraError) -> Self {
        match e.fault {
            CameraFault::NotFound => CaptureError::NotFound(e.detail),
            CameraFault::Unsupported => CaptureError::Unsupported(e.detail),
            CameraFault::Disconnected | CameraFault::NoFrames => CaptureError::Stopped(e.detail),
            _ => CaptureError::Os(e.detail),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CameraStats {
    /// Frames received from the device.
    pub frames_captured: u64,
    /// Preview frames delivered.
    pub preview_frames: u64,
    /// Encoder statistics (`frames_encoded` = raw frames delivered on the raw path).
    pub encoded: EncodedStats,
    /// Frames per second the device delivered over the last second or so (0 until measured).
    pub fps: f32,
    /// The device has delivered far fewer frames than its mode promises for a while with
    /// auto-exposure on (a dim room): see [`LowLightDetector`].
    pub low_light: bool,
}

// ---------- Webcam configuration (v0.6) ----------

/// Pixel format of a native camera mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CameraSubtype {
    Nv12,
    Yuy2,
    Mjpg,
}

impl CameraSubtype {
    /// "NV12", "YUY2" or "MJPG" (what [`CameraFormat::subtype`] says).
    pub fn name(self) -> &'static str {
        match self {
            CameraSubtype::Nv12 => "NV12",
            CameraSubtype::Yuy2 => "YUY2",
            CameraSubtype::Mjpg => "MJPG",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "NV12" => Some(CameraSubtype::Nv12),
            "YUY2" => Some(CameraSubtype::Yuy2),
            "MJPG" | "MJPEG" => Some(CameraSubtype::Mjpg),
            _ => None,
        }
    }

    /// Shown to people: "MJPEG" for the camera's own JPEG stream.
    pub fn label(self) -> &'static str {
        match self {
            CameraSubtype::Mjpg => "MJPEG",
            other => other.name(),
        }
    }
}

/// One native mode a camera offers (what the media source lists).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CameraMode {
    pub width: u32,
    pub height: u32,
    /// Frames per second times 1000 (29970 = 29.97).
    pub fps_milli: u32,
    pub subtype: CameraSubtype,
}

impl CameraMode {
    pub fn fps(&self) -> f32 {
        self.fps_milli as f32 / 1000.0
    }

    /// Same mode: size and format equal, rate within 0.5 fps.
    pub fn matches(&self, other: &CameraMode) -> bool {
        self.width == other.width
            && self.height == other.height
            && self.subtype == other.subtype
            && self.fps_milli.abs_diff(other.fps_milli) <= 500
    }

    /// "1280×720 · 30 fps (MJPEG)".
    pub fn label(&self) -> String {
        format!("{}×{} · {} fps ({})", self.width, self.height, fps_text(self.fps()), self.subtype.label())
    }
}

/// 30 and 29.97 give "30"; 7.5 gives "7.5".
pub fn fps_text(fps: f32) -> String {
    if (fps - fps.round()).abs() < 0.1 {
        format!("{}", fps.round() as i64)
    } else {
        format!("{fps:.1}")
    }
}

/// Native modes of a camera, biggest first, then fastest, uncompressed before MJPEG. `device` is
/// a [`CameraInfo::id`], a friendly name, or empty for the default. Opens the media source
/// without streaming (the camera light stays off) and releases it before returning. The
/// synthetic camera lists a few fake modes.
pub fn list_camera_formats(device: &str) -> std::result::Result<Vec<CameraMode>, CameraError> {
    imp::list_camera_formats(device)
}

/// A camera setting the driver exposes (`IAMVideoProcAmp` / `IAMCameraControl`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CameraControlId {
    Brightness,
    Contrast,
    Saturation,
    Sharpness,
    Gamma,
    WhiteBalance,
    Backlight,
    Exposure,
    Gain,
    Focus,
    Zoom,
    /// UVC auto-exposure priority (`KSPROPERTY_CAMERACONTROL_AUTO_EXPOSURE_PRIORITY`): 1 lets
    /// automatic exposure lower the frame rate in dim light, 0 keeps the mode's frame rate.
    ExposurePriority,
}

impl CameraControlId {
    /// In the order the settings list them.
    pub const ALL: [CameraControlId; 12] = [
        CameraControlId::Brightness,
        CameraControlId::Contrast,
        CameraControlId::Saturation,
        CameraControlId::Sharpness,
        CameraControlId::Gamma,
        CameraControlId::WhiteBalance,
        CameraControlId::Backlight,
        CameraControlId::Exposure,
        CameraControlId::Gain,
        CameraControlId::Focus,
        CameraControlId::Zoom,
        CameraControlId::ExposurePriority,
    ];

    /// Stable key for settings files.
    pub fn key(self) -> &'static str {
        match self {
            CameraControlId::Brightness => "brightness",
            CameraControlId::Contrast => "contrast",
            CameraControlId::Saturation => "saturation",
            CameraControlId::Sharpness => "sharpness",
            CameraControlId::Gamma => "gamma",
            CameraControlId::WhiteBalance => "white_balance",
            CameraControlId::Backlight => "backlight",
            CameraControlId::Exposure => "exposure",
            CameraControlId::Gain => "gain",
            CameraControlId::Focus => "focus",
            CameraControlId::Zoom => "zoom",
            CameraControlId::ExposurePriority => "exposure_priority",
        }
    }

    pub fn from_key(k: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.key() == k)
    }

    /// Portuguese name for the settings.
    pub fn label_pt(self) -> &'static str {
        match self {
            CameraControlId::Brightness => "Brilho",
            CameraControlId::Contrast => "Contraste",
            CameraControlId::Saturation => "Saturação",
            CameraControlId::Sharpness => "Nitidez",
            CameraControlId::Gamma => "Gama",
            CameraControlId::WhiteBalance => "Balanço de branco",
            CameraControlId::Backlight => "Compensação de luz de fundo",
            CameraControlId::Exposure => "Exposição",
            CameraControlId::Gain => "Ganho",
            CameraControlId::Focus => "Foco",
            CameraControlId::Zoom => "Zoom óptico",
            CameraControlId::ExposurePriority => "Exposição pode reduzir os quadros",
        }
    }
}

/// A supported control with its real range and current setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CameraControl {
    pub id: CameraControlId,
    pub min: i32,
    pub max: i32,
    /// Smallest change the driver accepts (at least 1).
    pub step: i32,
    pub default: i32,
    pub value: i32,
    /// Automatic mode is on (the value is then the driver's; setting it does nothing).
    pub auto: bool,
    pub can_auto: bool,
    pub can_manual: bool,
}

impl CameraControl {
    /// A value inside the range, on the driver's step grid (`min + k * step`).
    pub fn snap(&self, v: i32) -> i32 {
        snap_to_range(self.min, self.max, self.step, v)
    }

    /// An on / off switch (backlight compensation: 0..1).
    pub fn is_toggle(&self) -> bool {
        self.max - self.min == self.step.max(1)
    }

    /// What "restore the default" means: automatic when the control has it, else the default
    /// value.
    pub fn default_setting(&self) -> (i32, bool) {
        (self.snap(self.default), self.can_auto)
    }

    /// The value as people read it ("1/64 s" for exposure, "4500 K" for white balance).
    pub fn format_value(&self, v: i32) -> String {
        match self.id {
            CameraControlId::Exposure => exposure_text(v),
            CameraControlId::WhiteBalance => format!("{v} K"),
            _ if self.is_toggle() => (if v > self.min { "ligada" } else { "desligada" }).to_string(),
            _ => v.to_string(),
        }
    }
}

/// `v` clamped into `min..=max` and rounded to the nearest `min + k * step`.
pub fn snap_to_range(min: i32, max: i32, step: i32, v: i32) -> i32 {
    let (lo, hi) = if min <= max { (min, max) } else { (max, min) };
    let step = step.max(1) as i64;
    let v = (v.clamp(lo, hi) - lo) as i64;
    let k = (v + step / 2) / step;
    ((lo as i64 + k * step).min(hi as i64)) as i32
}

/// DirectShow exposure is log2 of the time in seconds: -6 is 1/64 s, 0 is 1 s.
pub fn exposure_text(log2_seconds: i32) -> String {
    if log2_seconds >= 0 {
        format!("{} s", 1u64 << log2_seconds.min(20))
    } else {
        format!("1/{} s", 1u64 << (-log2_seconds).min(20))
    }
}

/// The longest exposure setting of `c` (an exposure control) that still lets the camera deliver
/// `fps` frames per second. DirectShow exposure is log2 seconds (C920: -11..-2), but some drivers
/// report the UVC absolute time in 100 µs units instead (positive ranges like 3..2047).
pub fn exposure_for_fps(c: &CameraControl, fps: f32) -> i32 {
    let fps = if fps.is_finite() && fps >= 1.0 { fps } else { 30.0 };
    let want = if c.min < 0 || c.max <= 10 {
        // log2(1 / fps), rounded down: 30 fps -> -5 (1/32 s), 60 -> -6, 15 -> -4.
        (1.0 / fps).log2().floor() as i32
    } else {
        (10_000.0 / fps).floor() as i32
    };
    // On the driver's grid without going over (`snap` rounds to the nearest step).
    let v = c.snap(want);
    if v > want && v - c.step.max(1) >= c.min { v - c.step.max(1) } else { v }
}

/// The modes worth offering in a menu: one per size and format, at its highest frame rate
/// (29.97 and 30 are the same), biggest first, then fastest, uncompressed before MJPEG. A C920
/// lists every size at 30, 24, 20, 15, 10, 7.5 and 5 fps; nobody picks 1080p at 7.5 on purpose.
pub fn camera_mode_menu(modes: &[CameraMode]) -> Vec<CameraMode> {
    let mut best: Vec<CameraMode> = Vec::new();
    for m in modes {
        match best.iter_mut().find(|b| b.width == m.width && b.height == m.height && b.subtype == m.subtype) {
            Some(b) if m.fps_milli > b.fps_milli => *b = *m,
            Some(_) => {}
            None => best.push(*m),
        }
    }
    sort_camera_modes(&mut best);
    best
}

/// Biggest first, then fastest, uncompressed before MJPEG; exact duplicates dropped.
pub fn sort_camera_modes(v: &mut Vec<CameraMode>) {
    let rank = |s: CameraSubtype| match s {
        CameraSubtype::Nv12 => 0,
        CameraSubtype::Yuy2 => 1,
        CameraSubtype::Mjpg => 2,
    };
    v.sort_by_key(|m| (std::cmp::Reverse(m.width as u64 * m.height as u64), std::cmp::Reverse(m.width), std::cmp::Reverse(m.fps_milli), rank(m.subtype)));
    v.dedup();
}

/// One control change: `auto` switches the driver's automatic mode on (`value` is ignored).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CameraAdjust {
    pub id: CameraControlId,
    pub value: i32,
    pub auto: bool,
}

/// Quarter turns, clockwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum CameraRotation {
    #[default]
    None,
    Cw90,
    Cw180,
    Cw270,
}

impl CameraRotation {
    pub fn degrees(self) -> u32 {
        match self {
            CameraRotation::None => 0,
            CameraRotation::Cw90 => 90,
            CameraRotation::Cw180 => 180,
            CameraRotation::Cw270 => 270,
        }
    }

    pub fn from_degrees(d: u32) -> Self {
        match d % 360 {
            90 => CameraRotation::Cw90,
            180 => CameraRotation::Cw180,
            270 => CameraRotation::Cw270,
            _ => CameraRotation::None,
        }
    }

    /// Width and height trade places.
    pub fn swaps_axes(self) -> bool {
        matches!(self, CameraRotation::Cw90 | CameraRotation::Cw270)
    }
}

/// Our own picture processing, done by the GPU video processor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CameraTransform {
    /// Mirror the local preview / self-view (on by default, like Discord).
    pub mirror_preview: bool,
    /// Mirror what the others receive (off by default: they see the un-mirrored image).
    pub mirror_others: bool,
    pub rotation: CameraRotation,
    /// Digital zoom in percent (center crop): 100 is none, 200 is 2x.
    pub zoom_pct: u32,
}

impl Default for CameraTransform {
    fn default() -> Self {
        Self { mirror_preview: true, mirror_others: false, rotation: CameraRotation::None, zoom_pct: 100 }
    }
}

impl CameraTransform {
    pub const MAX_ZOOM_PCT: u32 = 200;

    pub fn sanitized(mut self) -> Self {
        self.zoom_pct = self.zoom_pct.clamp(100, Self::MAX_ZOOM_PCT);
        self
    }
}

/// Where a camera frame comes from and goes to for one output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CameraGeometry {
    /// Source rectangle in the camera frame: left, top, right, bottom (even coordinates).
    pub src: (u32, u32, u32, u32),
    /// Output size (after rotation), fitted inside the box. It does not depend on the zoom, so
    /// moving the zoom never resizes the encoder.
    pub out: (u32, u32),
}

/// Centered crop of a `src_w` x `src_h` frame for a zoom of `zoom_pct` percent (even
/// coordinates, at least 2x2): left, top, right, bottom.
pub fn crop_rect(src_w: u32, src_h: u32, zoom_pct: u32) -> (u32, u32, u32, u32) {
    let z = zoom_pct.clamp(100, CameraTransform::MAX_ZOOM_PCT) as u64;
    let cw = (((src_w as u64 * 100 / z) as u32) & !1).clamp(2.min(src_w), src_w);
    let ch = (((src_h as u64 * 100 / z) as u32) & !1).clamp(2.min(src_h), src_h);
    let l = ((src_w - cw) / 2) & !1;
    let t = ((src_h - ch) / 2) & !1;
    (l, t, (l + cw).min(src_w), (t + ch).min(src_h))
}

/// Fit (w, h) inside (max_w, max_h) keeping the aspect, never enlarging, even sizes.
pub fn fit_inside(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let s = (max_w as f64 / w.max(1) as f64).min(max_h as f64 / h.max(1) as f64).min(1.0);
    let ow = ((w as f64 * s) as u32) & !1;
    let oh = ((h as f64 * s) as u32) & !1;
    (ow.max(2), oh.max(2))
}

/// The crop and the output size for a `src_w` x `src_h` camera frame. After a quarter turn the
/// box (`max_w` x `max_h`) takes the orientation of the turned picture, so a 720p camera rotated
/// 90 degrees comes out 720 x 1280 (same pixel budget) and a portrait camera turned upright
/// comes out 1280 x 720.
pub fn camera_geometry(src_w: u32, src_h: u32, t: &CameraTransform, max_w: u32, max_h: u32) -> CameraGeometry {
    let swap = t.rotation.swaps_axes();
    let (fw, fh) = if swap { (src_h, src_w) } else { (src_w, src_h) };
    let (long, short) = (max_w.max(max_h), max_w.min(max_h));
    let (bw, bh) = match (swap, fw >= fh) {
        (false, _) => (max_w, max_h),
        (true, true) => (long, short),
        (true, false) => (short, long),
    };
    CameraGeometry { src: crop_rect(src_w, src_h, t.zoom_pct), out: fit_inside(fw, fh, bw, bh) }
}

/// Decides when the camera has slowed down because the room is dark: cameras with automatic
/// exposure lengthen the exposure and drop frames. Feed it one sample per second.
#[derive(Debug, Clone, Default)]
pub struct LowLightDetector {
    age: f32,
    below: f32,
    ok: f32,
    active: bool,
}

impl LowLightDetector {
    /// Samples ignored after the camera started (it is still settling).
    pub const GRACE_SECS: f32 = 3.0;
    /// The rate must stay under [`Self::LOW_RATIO`] for more than this long.
    pub const SUSTAIN_SECS: f32 = 5.0;
    pub const LOW_RATIO: f32 = 0.7;
    /// Back to at least this much of the requested rate for [`Self::RECOVER_SECS`] clears it.
    pub const OK_RATIO: f32 = 0.85;
    pub const RECOVER_SECS: f32 = 2.0;

    pub fn new() -> Self {
        Self::default()
    }

    /// `dt` seconds passed with `fps` frames per second arriving, `requested` being the mode's
    /// rate. Without automatic exposure a slow camera is not a dark room: the hint goes away.
    pub fn update(&mut self, dt: f32, fps: f32, requested: f32, auto_exposure: bool) -> bool {
        self.age += dt;
        if !auto_exposure || requested <= 0.0 {
            *self = Self { age: self.age, ..Self::default() };
            return false;
        }
        if self.age <= Self::GRACE_SECS {
            return false;
        }
        if fps < requested * Self::LOW_RATIO {
            self.below += dt;
            self.ok = 0.0;
            if self.below > Self::SUSTAIN_SECS {
                self.active = true;
            }
        } else {
            self.below = 0.0;
            if fps >= requested * Self::OK_RATIO {
                self.ok += dt;
                if self.ok >= Self::RECOVER_SECS {
                    self.active = false;
                }
            } else {
                self.ok = 0.0;
            }
        }
        self.active
    }

    pub fn active(&self) -> bool {
        self.active
    }
}

/// Running webcam capture. Dropping it stops the capture and releases the device (the camera
/// light goes off) before returning.
pub struct CameraCapture {
    inner: imp::CameraInner,
}

impl CameraCapture {
    /// Keyframe / bitrate feedback (also present on the raw path, where it does nothing).
    pub fn control(&self) -> EncoderControl {
        self.inner.control()
    }

    /// The hardware encoder in use; `None` on the raw and preview-only paths.
    pub fn backend(&self) -> Option<EncoderBackend> {
        self.inner.backend()
    }

    pub fn format(&self) -> CameraFormat {
        self.inner.format()
    }

    pub fn stats(&self) -> CameraStats {
        self.inner.stats()
    }

    /// `false` once the capture ended on its own; [`CameraCapture::failure`] says why.
    pub fn is_running(&self) -> bool {
        self.inner.is_running()
    }

    pub fn failure(&self) -> Option<CameraError> {
        self.inner.failure()
    }

    /// Deliver preview frames (only when an `on_preview` callback was given). Off = no preview
    /// GPU work or readback at all (the self-view is hidden).
    pub fn set_preview(&self, on: bool) {
        self.inner.set_preview(on)
    }

    /// The controls this camera supports, with real ranges and the settings last applied.
    /// Empty for cameras that expose none.
    pub fn controls(&self) -> Vec<CameraControl> {
        self.inner.controls()
    }

    /// Change one control (applied on the camera thread; the newest request per control wins).
    /// `value` is snapped to the control's range and step; `auto` hands it to the driver.
    pub fn set_control(&self, id: CameraControlId, value: i32, auto: bool) {
        self.inner.set_controls(&[CameraAdjust { id, value, auto }])
    }

    /// Change several controls at once.
    pub fn apply_controls(&self, adjust: &[CameraAdjust]) {
        self.inner.set_controls(adjust)
    }

    /// Put one control back to its default (automatic when it has one).
    pub fn reset_control(&self, id: CameraControlId) {
        self.inner.reset_controls(Some(id))
    }

    /// Put every control back to its default.
    pub fn reset_controls(&self) {
        self.inner.reset_controls(None)
    }

    /// Read every control back from the driver (after something else changed them: the
    /// driver's own settings dialog, or an automatic mode moving the value).
    pub fn refresh_controls(&self) {
        self.inner.refresh_controls()
    }

    /// Every native mode the device offered when it was opened (sorted like
    /// [`list_camera_formats`]), without opening it again.
    pub fn native_modes(&self) -> Vec<CameraMode> {
        self.inner.native_modes()
    }

    /// Mirror / rotate / zoom, from the next frame on. No-op when nothing changed.
    pub fn set_transform(&self, t: CameraTransform) {
        self.inner.set_transform(t.sanitized())
    }

    pub fn transform(&self) -> CameraTransform {
        self.inner.transform()
    }

    /// `false` when the GPU video processor cannot mirror or rotate (the picture is then
    /// delivered as it comes; only the zoom works).
    pub fn transform_supported(&self) -> bool {
        self.inner.transform_supported()
    }
}

/// The camera driver's own settings dialog (the DirectShow property pages OBS shows under
/// "Configure Video": Logitech's, or Windows' generic "Video Proc Amp" / "Camera Control" pages).
/// `device` is a [`CameraInfo::id`] or friendly name. Runs on its own STA thread and blocks until
/// the dialog is closed; changes made there go straight to the device (a running capture sees
/// them after [`CameraCapture::refresh_controls`]).
pub fn show_camera_settings_dialog(device: &str, title: &str) -> std::result::Result<(), CameraError> {
    imp::camera_property_pages(device, Some(title)).map(|_| ())
}

/// How many property pages the driver's settings dialog has, without showing it (the same
/// lookup as [`show_camera_settings_dialog`]).
pub fn camera_settings_dialog_pages(device: &str) -> std::result::Result<u32, CameraError> {
    imp::camera_property_pages(device, None)
}

/// A read-only report of a camera for diagnostics: every native media type the device lists
/// (including formats we cannot use) and its image controls, opened without streaming.
pub fn camera_diagnostics(device: &str) -> std::result::Result<String, CameraError> {
    imp::camera_diagnostics(device)
}

/// Video capture devices, by friendly name. With `VOX_FAKE_CAMERA=1` the synthetic camera is
/// listed first.
pub fn list_cameras() -> Result<Vec<CameraInfo>> {
    imp::list_cameras()
}

/// Webcam -> hardware H.264 (same access-unit contract as [`start_video_encoded`]). `device` is
/// a [`CameraInfo::id`], a friendly name, or empty for the default (the synthetic camera when
/// `VOX_FAKE_CAMERA=1`, else the first camera). Blocks until the first frame arrived or the
/// camera failed (typically under a second). `CameraFault::Unsupported` when no hardware
/// encoder works: use [`start_camera_raw`].
pub fn start_camera_encoded(
    device: &str,
    cfg: CameraConfig,
    on_frame: EncodedCallback,
    on_preview: Option<FrameCallback>,
) -> std::result::Result<CameraCapture, CameraError> {
    imp::start_camera(device, cfg, imp::CameraOutput::Encoded(on_frame), on_preview)
        .map(|inner| CameraCapture { inner })
}

/// Webcam -> NV12 frames on the CPU (software-encoder fallback), plus the optional preview.
pub fn start_camera_raw(
    device: &str,
    cfg: CameraConfig,
    on_frame: FrameCallback,
    on_preview: Option<FrameCallback>,
) -> std::result::Result<CameraCapture, CameraError> {
    imp::start_camera(device, cfg, imp::CameraOutput::Raw(on_frame), on_preview)
        .map(|inner| CameraCapture { inner })
}

/// Webcam -> preview frames only (the camera picker's preview in Settings).
pub fn start_camera_preview(
    device: &str,
    cfg: CameraConfig,
    on_preview: FrameCallback,
) -> std::result::Result<CameraCapture, CameraError> {
    imp::start_camera(device, cfg, imp::CameraOutput::None, Some(on_preview))
        .map(|inner| CameraCapture { inner })
}

// ---------- Picker thumbnails (v0.3, item 8c) ----------
//
// For the Go Live picker: one picture of a window or monitor, taken once and in memory only.
// A single Windows.Graphics.Capture frame is scaled on the GPU, read back small and handed over
// as RGBA; the capture session is closed before the call returns. Windows that are minimized
// give no picture (`is_window_minimized`): the picker shows `window_icon` instead.

/// A small picture: `width * height` pixels, 4 bytes each, R G B A, rows tightly packed. The
/// thumbnails are always fully opaque; icons carry real alpha (straight, not premultiplied).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Takes thumbnails one after the other, reusing one GPU device (making a device costs more
/// than a grab), so a worker thread creates one and loops. Bound to the thread that made it.
pub struct ThumbnailGrabber {
    inner: imp::GrabberInner,
}

impl ThumbnailGrabber {
    pub fn new() -> Result<Self> {
        imp::GrabberInner::new().map(|inner| Self { inner })
    }

    /// How long a grab waits for the target's first frame (default 2.5 s). A window that is not
    /// drawing, or one that cannot be captured, ends in `CaptureError::Stopped` after this.
    pub fn set_timeout(&mut self, timeout: std::time::Duration) {
        self.inner.set_timeout(timeout)
    }

    /// One frame of `target`, scaled to fit inside `max_width` x `max_height` (aspect kept, never
    /// enlarged). A minimized window gives `CaptureError::Stopped`.
    pub fn grab(&mut self, target: VideoTarget, max_width: u32, max_height: u32) -> Result<RgbaImage> {
        self.inner.grab(target, max_width, max_height)
    }
}

/// [`ThumbnailGrabber::grab`] with a one-off device. Prefer a [`ThumbnailGrabber`] for many.
pub fn capture_thumbnail(target: VideoTarget, max_width: u32, max_height: u32) -> Result<RgbaImage> {
    ThumbnailGrabber::new()?.grab(target, max_width, max_height)
}

/// The window is minimized (it has no content to capture).
pub fn is_window_minimized(hwnd: isize) -> bool {
    imp::is_window_minimized(hwnd)
}

/// The icon of a window (its own, its class's, else its program's), drawn at its natural size but
/// at most `max_size` pixels wide and tall.
pub fn window_icon(hwnd: isize, max_size: u32) -> Result<RgbaImage> {
    imp::window_icon(hwnd, max_size)
}

// ---------- What a shared window is (Go Live hover cards, v1.1) ----------

mod game;
pub use game::{looks_like_game, program_name, AppInfo};

/// The program that owns window `hwnd`: its own name (never the window's title) and whether it
/// looks like a game ([`looks_like_game`]). `None` when the window or its process is gone (or the
/// process cannot be queried, e.g. elevated).
pub fn window_app(hwnd: isize) -> Option<AppInfo> {
    imp::window_app(hwnd)
}

// ---------- Privacy permissions (macOS) ----------
//
// macOS asks the person before an app may record the screen (and the sound of other apps, which
// ScreenCaptureKit delivers) or use a camera. Windows has nothing to ask here: every function
// below says "allowed" there.

/// A privacy permission's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Granted,
    /// Refused (or blocked by a profile): only the person can change it, in System Settings.
    Denied,
    /// Never asked: the first use shows the system prompt.
    NotDetermined,
}

/// Screen Recording (screens, windows, an app's sound, the picker's thumbnails). Cheap after the
/// first call (the answer is kept and refreshed whenever a capture call learns it).
pub fn screen_capture_permission() -> Permission {
    #[cfg(target_os = "macos")]
    {
        mac::screen_permission()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Permission::Granted
    }
}

/// Ask for Screen Recording: macOS shows its prompt the first time (later it only lists the app,
/// switched off, in System Settings). Returns whether it is granted now. macOS applies a new grant
/// only after the app restarts.
pub fn request_screen_capture_permission() -> bool {
    #[cfg(target_os = "macos")]
    {
        mac::request_screen_permission()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Camera access.
pub fn camera_permission() -> Permission {
    #[cfg(target_os = "macos")]
    {
        mac::camera_permission()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Permission::Granted
    }
}

/// Ask for the camera (the system prompt, the first time only); blocks until it is answered (at
/// most a minute). Returns whether it is granted.
pub fn request_camera_permission() -> bool {
    #[cfg(target_os = "macos")]
    {
        mac::request_camera_permission()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Where the person changes a permission: the System Settings page (`open` it). `None` off macOS.
pub fn privacy_settings_url(camera: bool) -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some(if camera {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Camera"
        } else {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
        })
    } else {
        None
    }
}

/// macOS: the IOSurface-backed `CVPixelBuffer` (NV12) behind a frame this crate delivered to a
/// [`FrameCallback`] (screen capture or camera), retained once for the caller, who must release it
/// or hand the reference to an API that takes it over (libwebrtc's
/// `NativeBuffer::from_cv_pixel_buffer`). Call it during the callback; `None` for any other frame
/// (one built elsewhere, the synthetic camera, a preview).
#[cfg(target_os = "macos")]
pub fn retained_pixel_buffer(frame: &Nv12Frame<'_>) -> Option<std::ptr::NonNull<std::ffi::c_void>> {
    mac::retained_pixel_buffer(frame)
}

// Handles can be moved into async tasks / other threads by the client.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<VideoCapture>();
    assert_send_sync::<AudioCapture>();
    assert_send_sync::<EncodedVideoCapture>();
    assert_send_sync::<EncoderControl>();
    assert_send_sync::<CameraCapture>();
    assert_send_sync::<ScreenPreview>();
};

#[cfg(windows)]
mod win;
#[cfg(windows)]
use win as imp;

/// Offline encoder benchmark hooks (`capture_probe bench`): synthetic frames through the real
/// video-processor -> encoder path. Not part of the client contract.
#[cfg(windows)]
#[doc(hidden)]
pub mod bench {
    pub use crate::win::bench::*;
}

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
use mac as imp;

/// Pure picture / sound helpers of the macOS backend (built for tests everywhere).
#[cfg(any(target_os = "macos", test))]
mod pixels;

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    //! Stub for other hosts (Linux) so the workspace still type-checks.
    use super::*;

    pub struct VideoInner;
    pub struct AudioInner;
    pub struct GrabberInner;

    impl GrabberInner {
        pub fn new() -> Result<Self> {
            unsupported()
        }
        pub fn set_timeout(&mut self, _: std::time::Duration) {}
        pub fn grab(&mut self, _: VideoTarget, _: u32, _: u32) -> Result<RgbaImage> {
            unsupported()
        }
    }
    pub fn is_window_minimized(_: isize) -> bool {
        false
    }
    pub fn window_icon(_: isize, _: u32) -> Result<RgbaImage> {
        unsupported()
    }
    pub fn window_app(_: isize) -> Option<AppInfo> {
        None
    }

    #[derive(Clone)]
    pub struct PreviewHandle(std::sync::Arc<std::sync::Mutex<PreviewConfig>>);
    impl PreviewHandle {
        pub fn new() -> Result<Self> {
            Ok(Self(Default::default()))
        }
        pub fn set(&self, cfg: PreviewConfig) {
            *self.0.lock().unwrap_or_else(|p| p.into_inner()) = cfg;
        }
        pub fn config(&self) -> PreviewConfig {
            *self.0.lock().unwrap_or_else(|p| p.into_inner())
        }
        pub fn frames(&self) -> u64 {
            0
        }
        pub fn holds_gpu(&self) -> bool {
            false
        }
    }
    pub fn gpu_holders() -> usize {
        0
    }
    pub struct PreviewAttach;
    impl PreviewAttach {
        pub fn new(_: &PreviewHandle, _: FrameCallback) -> Self {
            PreviewAttach
        }
    }
    pub struct EncodedInner;
    pub struct CameraInner;

    pub enum CameraOutput {
        Encoded(EncodedCallback),
        Raw(FrameCallback),
        None,
    }

    impl CameraInner {
        pub fn control(&self) -> EncoderControl {
            EncoderControl::default()
        }
        pub fn backend(&self) -> Option<EncoderBackend> {
            None
        }
        pub fn format(&self) -> CameraFormat {
            CameraFormat {
                device: String::new(),
                native_width: 0,
                native_height: 0,
                native_fps: 0.0,
                subtype: String::new(),
                gpu_frames: false,
                width: 0,
                height: 0,
            }
        }
        pub fn stats(&self) -> CameraStats {
            CameraStats::default()
        }
        pub fn is_running(&self) -> bool {
            false
        }
        pub fn failure(&self) -> Option<CameraError> {
            None
        }
        pub fn set_preview(&self, _: bool) {}
        pub fn controls(&self) -> Vec<CameraControl> {
            Vec::new()
        }
        pub fn set_controls(&self, _: &[CameraAdjust]) {}
        pub fn reset_controls(&self, _: Option<CameraControlId>) {}
        pub fn refresh_controls(&self) {}
        pub fn native_modes(&self) -> Vec<CameraMode> {
            Vec::new()
        }
        pub fn set_transform(&self, _: CameraTransform) {}
        pub fn transform(&self) -> CameraTransform {
            CameraTransform::default()
        }
        pub fn transform_supported(&self) -> bool {
            false
        }
    }

    pub fn list_camera_formats(_: &str) -> std::result::Result<Vec<CameraMode>, CameraError> {
        Err(CameraError {
            fault: CameraFault::Unsupported,
            detail: "vox-capture only supports Windows".into(),
        })
    }

    pub fn camera_property_pages(_: &str, _: Option<&str>) -> std::result::Result<u32, CameraError> {
        Err(CameraError { fault: CameraFault::Unsupported, detail: "vox-capture only supports Windows".into() })
    }

    pub fn camera_diagnostics(_: &str) -> std::result::Result<String, CameraError> {
        Err(CameraError { fault: CameraFault::Unsupported, detail: "vox-capture only supports Windows".into() })
    }

    pub fn list_cameras() -> Result<Vec<CameraInfo>> {
        unsupported()
    }
    pub fn start_camera(
        _: &str,
        _: CameraConfig,
        _: CameraOutput,
        _: Option<FrameCallback>,
    ) -> std::result::Result<CameraInner, CameraError> {
        Err(CameraError {
            fault: CameraFault::Unsupported,
            detail: "vox-capture only supports Windows".into(),
        })
    }

    impl EncodedInner {
        pub fn control(&self) -> EncoderControl {
            EncoderControl::default()
        }
        pub fn backend(&self) -> EncoderBackend {
            EncoderBackend::Auto
        }
        pub fn stats(&self) -> EncodedStats {
            EncodedStats::default()
        }
        pub fn is_running(&self) -> bool {
            false
        }
    }

    impl VideoInner {
        pub fn stats(&self) -> VideoStats {
            VideoStats::default()
        }
        pub fn is_running(&self) -> bool {
            false
        }
    }
    impl AudioInner {
        pub fn is_running(&self) -> bool {
            false
        }
    }

    fn unsupported<T>() -> Result<T> {
        Err(CaptureError::Unsupported(
            "vox-capture only supports Windows".into(),
        ))
    }
    pub fn monotonic_now_us() -> i64 {
        use std::sync::OnceLock;
        use std::time::Instant;
        static T0: OnceLock<Instant> = OnceLock::new();
        T0.get_or_init(Instant::now).elapsed().as_micros() as i64
    }
    pub fn list_monitors() -> Result<Vec<MonitorInfo>> {
        unsupported()
    }
    pub fn list_windows() -> Result<Vec<WindowInfo>> {
        unsupported()
    }
    pub fn start_video(_: VideoTarget, _: VideoConfig, _: FrameCallback, _: Option<PreviewAttach>) -> Result<VideoInner> {
        unsupported()
    }
    pub fn start_audio(_: AudioTarget, _: AudioCallback) -> Result<AudioInner> {
        unsupported()
    }
    pub fn available_encoders() -> Vec<EncoderBackend> {
        Vec::new()
    }
    pub fn list_gpus() -> Vec<GpuInfo> {
        Vec::new()
    }
    pub fn encoder_candidates(_: &[GpuInfo]) -> Vec<EncoderCandidate> {
        vec![EncoderCandidate::software()]
    }
    pub fn self_test_encoders(_: &[EncoderCandidate], _: &mut dyn FnMut(&SelfTestResult)) -> Vec<SelfTestResult> {
        Vec::new()
    }
    pub fn start_video_encoded(
        _: VideoTarget,
        _: EncodeConfig,
        _: EncodedCallback,
        _: Option<PreviewAttach>,
    ) -> Result<EncodedInner> {
        unsupported()
    }
}
#[cfg(test)]
mod idr_gate_tests {
    use super::IdrGate;

    #[test]
    fn after_a_loss_only_the_next_idr_goes_out() {
        let mut g = IdrGate::default();
        // Normal stream: everything passes.
        assert!(g.pass(true) && g.pass(false) && g.pass(false));
        // A frame was lost: the P-frames still in the encoder's pipeline are held...
        assert!(g.arm());
        assert!(!g.arm(), "a second loss while waiting asks for no second IDR");
        assert!(!g.pass(false) && !g.pass(false) && !g.pass(false));
        assert!(g.awaiting());
        // ...until the IDR, which goes out and reopens the gate.
        assert!(g.pass(true));
        assert!(g.pass(false));
        assert_eq!(g.withheld, 3);
        assert!(!g.awaiting());
    }
}

#[cfg(test)]
mod camera_config_tests {
    use super::*;

    fn ctl(min: i32, max: i32, step: i32, default: i32, can_auto: bool) -> CameraControl {
        CameraControl { id: CameraControlId::Brightness, min, max, step, default, value: default, auto: false, can_auto, can_manual: true }
    }

    #[test]
    fn values_snap_to_the_drivers_range_and_step() {
        // C920 brightness: 0..255 step 1.
        let b = ctl(0, 255, 1, 128, false);
        assert_eq!((b.snap(-5), b.snap(300), b.snap(77)), (0, 255, 77));
        // White balance 2000..6500 step 10 (K): 4567 -> 4570, 4564 -> 4560.
        let w = ctl(2000, 6500, 10, 4000, true);
        assert_eq!((w.snap(4567), w.snap(4564), w.snap(1)), (4570, 4560, 2000));
        // Focus 0..250 step 5; the grid starts at the minimum (min 3, step 5: 3, 8, 13...).
        assert_eq!(ctl(0, 250, 5, 0, true).snap(123), 125);
        assert_eq!(snap_to_range(3, 20, 5, 10), 8);
        assert_eq!(snap_to_range(3, 20, 5, 100), 18, "never past the maximum");
        // Reversed ranges and zero steps do not panic.
        assert_eq!(snap_to_range(10, 0, 0, 4), 4);
        // Exposure -11..-2 (log2 seconds).
        assert_eq!(snap_to_range(-11, -2, 1, -20), -11);
    }

    #[test]
    fn defaults_prefer_automatic() {
        assert_eq!(ctl(0, 255, 1, 128, false).default_setting(), (128, false));
        assert_eq!(ctl(0, 255, 1, 128, true).default_setting(), (128, true));
        assert!(ctl(0, 1, 1, 0, false).is_toggle());
        assert!(!ctl(0, 255, 1, 0, false).is_toggle());
    }

    #[test]
    fn values_read_like_people_say_them() {
        assert_eq!(exposure_text(-6), "1/64 s");
        assert_eq!(exposure_text(0), "1 s");
        assert_eq!(exposure_text(2), "4 s");
        let mut e = ctl(-11, -2, 1, -6, true);
        e.id = CameraControlId::Exposure;
        assert_eq!(e.format_value(-2), "1/4 s");
        let mut w = ctl(2000, 6500, 10, 4000, true);
        w.id = CameraControlId::WhiteBalance;
        assert_eq!(w.format_value(4500), "4500 K");
        for id in CameraControlId::ALL {
            assert_eq!(CameraControlId::from_key(id.key()), Some(id));
        }
    }

    #[test]
    fn mode_labels() {
        let m = CameraMode { width: 1280, height: 720, fps_milli: 30_000, subtype: CameraSubtype::Mjpg };
        assert_eq!(m.label(), "1280×720 · 30 fps (MJPEG)");
        let m = CameraMode { width: 640, height: 480, fps_milli: 7_500, subtype: CameraSubtype::Yuy2 };
        assert_eq!(m.label(), "640×480 · 7.5 fps (YUY2)");
        assert_eq!(fps_text(29.97), "30");
        assert_eq!(CameraSubtype::from_name("mjpeg"), Some(CameraSubtype::Mjpg));
        assert_eq!(CameraSubtype::from_name(CameraSubtype::Nv12.name()), Some(CameraSubtype::Nv12));
    }

    #[test]
    fn exposure_that_keeps_the_frame_rate() {
        let mut e = ctl(-11, -2, 1, -5, true);
        e.id = CameraControlId::Exposure;
        // 1/32 s fits in a 30 fps frame (1/30 s); 1/16 s would not.
        assert_eq!(exposure_for_fps(&e, 30.0), -5);
        assert_eq!(exposure_for_fps(&e, 29.97), -5);
        assert_eq!(exposure_for_fps(&e, 60.0), -6);
        assert_eq!(exposure_for_fps(&e, 15.0), -4);
        assert_eq!(exposure_for_fps(&e, 0.0), -5, "unknown rate: 30 fps");
        // Clamped into the camera's range.
        let short = CameraControl { min: -13, max: -7, ..e };
        assert_eq!(exposure_for_fps(&short, 30.0), -7);
        // Drivers that report 100 µs units: 1/30 s = 333, never rounded up past it.
        let units = CameraControl { min: 3, max: 2047, step: 1, default: 166, ..e };
        assert_eq!(exposure_for_fps(&units, 30.0), 333);
        let coarse = CameraControl { min: 0, max: 2000, step: 50, ..units };
        assert_eq!(exposure_for_fps(&coarse, 30.0), 300, "333 is between steps: the shorter one");
        assert_eq!(exposure_for_fps(&coarse, 60.0), 150);
    }

    #[test]
    fn mode_menu_keeps_the_fastest_per_size_and_format() {
        let m = |width, height, fps_milli, subtype| CameraMode { width, height, fps_milli, subtype };
        use CameraSubtype::*;
        // What a C920 lists (a part of it), in the driver's order.
        let raw = vec![
            m(640, 480, 30_000, Yuy2),
            m(640, 480, 15_000, Yuy2),
            m(1920, 1080, 5_000, Yuy2),
            m(1920, 1080, 30_000, Mjpg),
            m(1920, 1080, 24_000, Mjpg),
            m(1920, 1080, 15_000, Mjpg),
            m(1280, 720, 10_000, Yuy2),
            m(1280, 720, 29_970, Mjpg),
            m(1280, 720, 30_000, Mjpg),
            m(1280, 720, 30_000, Mjpg),
            m(2304, 1536, 2_000, Yuy2),
            m(960, 720, 30_000, Mjpg),
            m(1024, 576, 30_000, Mjpg),
        ];
        let menu = camera_mode_menu(&raw);
        let labels: Vec<String> = menu.iter().map(|x| x.label()).collect();
        assert_eq!(
            labels,
            vec![
                "2304×1536 · 2 fps (YUY2)",
                "1920×1080 · 30 fps (MJPEG)",
                "1920×1080 · 5 fps (YUY2)",
                "1280×720 · 30 fps (MJPEG)",
                "1280×720 · 10 fps (YUY2)",
                "960×720 · 30 fps (MJPEG)",
                "1024×576 · 30 fps (MJPEG)",
                "640×480 · 30 fps (YUY2)",
            ]
        );
        // The full list keeps every rate, sorted, without exact duplicates.
        let mut all = raw.clone();
        sort_camera_modes(&mut all);
        assert_eq!(all.len(), raw.len() - 1);
        assert_eq!(all[0], m(2304, 1536, 2_000, Yuy2));
        assert_eq!(all[1], m(1920, 1080, 30_000, Mjpg));
        // Equal size and rate: uncompressed first.
        let mut v = vec![m(1280, 720, 30_000, Mjpg), m(1280, 720, 30_000, Nv12)];
        sort_camera_modes(&mut v);
        assert_eq!(v[0].subtype, Nv12);
        assert!(camera_mode_menu(&[]).is_empty());
    }

    #[test]
    fn crop_is_centered_even_and_scales_with_the_zoom() {
        assert_eq!(crop_rect(1280, 720, 100), (0, 0, 1280, 720));
        assert_eq!(crop_rect(1280, 720, 200), (320, 180, 960, 540));
        // 1.5x: 853.3 -> 852 wide, 480 high; offsets stay even.
        let (l, t, r, b) = crop_rect(1280, 720, 150);
        assert_eq!((r - l, b - t), (852, 480));
        assert_eq!((l % 2, t % 2, (r - l) % 2, (b - t) % 2), (0, 0, 0, 0));
        assert_eq!((l, t), (214, 120));
        // Out-of-range zooms are clamped; odd sizes stay inside the frame.
        assert_eq!(crop_rect(1280, 720, 50), (0, 0, 1280, 720));
        assert_eq!(crop_rect(1280, 720, 900), crop_rect(1280, 720, 200));
        let (l, t, r, b) = crop_rect(641, 361, 173);
        assert!(r <= 641 && b <= 361 && r > l && b > t);
        assert_eq!((crop_rect(2, 2, 200).2, crop_rect(2, 2, 200).3), (2, 2));
    }

    #[test]
    fn rotation_swaps_the_box_and_the_zoom_never_resizes() {
        let t = |rotation, zoom_pct| CameraTransform { rotation, zoom_pct, ..Default::default() };
        let g = camera_geometry(1280, 720, &t(CameraRotation::None, 100), 1280, 720);
        assert_eq!((g.out, g.src), ((1280, 720), (0, 0, 1280, 720)));
        // A quarter turn: portrait, same pixel budget.
        for rot in [CameraRotation::Cw90, CameraRotation::Cw270] {
            assert_eq!(camera_geometry(1280, 720, &t(rot, 100), 1280, 720).out, (720, 1280));
        }
        assert_eq!(camera_geometry(1280, 720, &t(CameraRotation::Cw180, 100), 1280, 720).out, (1280, 720));
        // Smaller send size: 360p rotated is 360x640.
        assert_eq!(camera_geometry(1280, 720, &t(CameraRotation::Cw90, 100), 640, 360).out, (360, 640));
        // Never enlarged: a 640x360 camera stays 640x360 inside a 1280x720 box.
        assert_eq!(camera_geometry(640, 360, &t(CameraRotation::None, 100), 1280, 720).out, (640, 360));
        // The zoom only moves the source rectangle.
        let z = camera_geometry(1280, 720, &t(CameraRotation::Cw90, 200), 1280, 720);
        assert_eq!(z.out, (720, 1280));
        assert_eq!(z.src, (320, 180, 960, 540));
        // A portrait camera (phone as webcam) rotated 90 comes out landscape.
        assert_eq!(camera_geometry(720, 1280, &t(CameraRotation::Cw90, 100), 1280, 720).out, (1280, 720));
        assert_eq!(CameraRotation::from_degrees(450), CameraRotation::Cw90);
        assert_eq!(CameraRotation::from_degrees(45), CameraRotation::None);
        assert!(CameraTransform::default().mirror_preview && !CameraTransform::default().mirror_others);
    }

    /// Feed one sample per second.
    fn feed(d: &mut LowLightDetector, secs: u32, fps: f32, requested: f32, auto: bool) -> Vec<bool> {
        (0..secs).map(|_| d.update(1.0, fps, requested, auto)).collect()
    }

    #[test]
    fn low_light_needs_more_than_five_seconds_of_slow_frames() {
        let mut d = LowLightDetector::new();
        // Settling: a slow start is ignored.
        assert!(feed(&mut d, 3, 5.0, 30.0, true).iter().all(|x| !x));
        // Slow for 5 s is not yet "more than 5 s"; the sixth second is.
        let s = feed(&mut d, 6, 10.0, 30.0, true);
        assert_eq!(s, vec![false, false, false, false, false, true]);
        assert!(d.active());
        // Staying slow keeps it on; recovering takes two good seconds.
        assert!(feed(&mut d, 3, 9.0, 30.0, true).iter().all(|x| *x));
        assert_eq!(feed(&mut d, 3, 29.0, 30.0, true), vec![true, false, false]);
    }

    #[test]
    fn a_short_dip_or_manual_exposure_is_not_low_light() {
        let mut d = LowLightDetector::new();
        feed(&mut d, 4, 30.0, 30.0, true);
        // Dips of 4 s, interrupted by a normal second, never add up.
        for _ in 0..4 {
            assert!(feed(&mut d, 4, 12.0, 30.0, true).iter().all(|x| !x));
            assert!(!d.update(1.0, 30.0, 30.0, true));
        }
        // 22 fps of 30 is not "well below".
        assert!(feed(&mut d, 20, 22.0, 30.0, true).iter().all(|x| !x));
        // Slow with manual exposure: no hint (a busy USB port, not a dark room).
        assert!(feed(&mut d, 20, 8.0, 30.0, false).iter().all(|x| !x));
        // Turning exposure to manual clears an active hint at once.
        let mut d = LowLightDetector::new();
        feed(&mut d, 12, 8.0, 30.0, true);
        assert!(d.active());
        assert!(!d.update(1.0, 8.0, 30.0, false));
        assert!(!d.active());
        // The requested rate unknown (0): never.
        assert!(!d.update(1.0, 0.0, 0.0, true));
    }

    /// GPU: the synthetic camera's blinking dark box (top-left of the source) lands where the
    /// mirror / rotation / zoom say, in the picture others get and in the local preview.
    #[test]
    #[cfg(windows)]
    fn synthetic_camera_mirror_rotate_zoom_on_the_gpu() {
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        #[derive(Default)]
        struct Probe {
            size: (u32, u32),
            /// Darkest sample per corner (TL, TR, BL, BR) since the last reset.
            dark: [u8; 4],
            frames: u32,
        }
        impl Probe {
            fn reset(&mut self) {
                self.dark = [255; 4];
                self.frames = 0;
            }
        }
        fn probe_cb(p: Arc<Mutex<Probe>>) -> FrameCallback {
            Box::new(move |f: &Nv12Frame<'_>| {
                let mut p = p.lock().unwrap();
                if p.size != (f.width, f.height) {
                    p.size = (f.width, f.height);
                    p.reset();
                }
                let (w, h) = (f.width as usize, f.height as usize);
                // 12x12 patches, 12 pixels in from each corner (inside the box in any turn).
                let mean = |x0: usize, y0: usize| {
                    let mut s = 0u32;
                    for y in y0..y0 + 12 {
                        for x in x0..x0 + 12 {
                            s += f.y[y * f.y_stride as usize + x] as u32;
                        }
                    }
                    (s / 144) as u8
                };
                let spots = [(12, 12), (w - 24, 12), (12, h - 24), (w - 24, h - 24)];
                for (i, (x, y)) in spots.into_iter().enumerate() {
                    p.dark[i] = p.dark[i].min(mean(x, y));
                }
                p.frames += 1;
            })
        }

        let main = Arc::new(Mutex::new(Probe::default()));
        let prev = Arc::new(Mutex::new(Probe::default()));
        let cfg = CameraConfig { width: 640, height: 360, fps: 30, ..Default::default() };
        let cap = match start_camera_raw(SYNTHETIC_CAMERA_ID, cfg, probe_cb(main.clone()), Some(probe_cb(prev.clone()))) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("no GPU video processor here ({e}); skipping");
                return;
            }
        };
        std::thread::sleep(Duration::from_millis(300));
        // The dark box blinks (15 frames on, 15 off): watch a bit over a second.
        // (main size, main corners, preview size, preview corners)
        type Seen = ((u32, u32), [u8; 4], (u32, u32), [u8; 4]);
        let look = |cap: &CameraCapture, t: CameraTransform| -> Seen {
            cap.set_transform(t);
            std::thread::sleep(Duration::from_millis(250));
            main.lock().unwrap().reset();
            prev.lock().unwrap().reset();
            let t0 = Instant::now();
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let (m, p) = (main.lock().unwrap(), prev.lock().unwrap());
                if (m.frames >= 40 && p.frames >= 30) || t0.elapsed() > Duration::from_secs(8) {
                    return (m.size, m.dark, p.size, p.dark);
                }
            }
        };
        let dark_corner = |d: [u8; 4]| -> Option<usize> {
            let c: Vec<usize> = (0..4).filter(|&i| d[i] < 40).collect();
            (c.len() == 1).then(|| c[0])
        };
        const TL: usize = 0;
        const TR: usize = 1;
        const BL: usize = 2;
        const BR: usize = 3;
        let t = |mp, mo, deg, zoom| CameraTransform { mirror_preview: mp, mirror_others: mo, rotation: CameraRotation::from_degrees(deg), zoom_pct: zoom };
        if !cap.transform_supported() {
            eprintln!("this GPU's video processor cannot mirror / rotate; skipping the orientation checks");
            return;
        }
        // (preview mirror, others mirror, degrees) -> (corner others see, corner in the preview)
        let cases = [
            (true, false, 0, TL, TR),
            (false, false, 0, TL, TL),
            (true, true, 0, TR, TR),
            (false, false, 90, TR, TR),
            (false, false, 180, BR, BR),
            (false, false, 270, BL, BL),
            (false, true, 90, TL, TR),
            (false, true, 180, BL, BR),
            (false, true, 270, BR, BL),
            (true, false, 90, TR, TL),
        ];
        for (mp, mo, deg, want_main, want_prev) in cases {
            let (ms, md, ps, pd) = look(&cap, t(mp, mo, deg, 100));
            let portrait = deg == 90 || deg == 270;
            assert_eq!(ms.0 < ms.1, portrait, "size {ms:?} for {deg} deg");
            assert_eq!(ps.0 < ps.1, portrait, "preview size {ps:?} for {deg} deg");
            assert_eq!(dark_corner(md), Some(want_main), "others: mirror={mo} rot={deg}: {md:?}");
            assert_eq!(dark_corner(pd), Some(want_prev), "preview: mirror={mp} rot={deg}: {pd:?}");
        }
        // Zoom 2x crops the corner box away without changing the size.
        let (ms0, _, _, _) = look(&cap, t(false, false, 0, 100));
        let (ms, md, _, pd) = look(&cap, t(false, false, 0, 200));
        assert_eq!(ms, ms0, "the zoom must not resize the output");
        assert_eq!(dark_corner(md), None, "{md:?}");
        assert_eq!(dark_corner(pd), None, "{pd:?}");
        // And back: the box returns.
        let (_, md, _, _) = look(&cap, t(false, false, 0, 100));
        assert_eq!(dark_corner(md), Some(TL));
    }

    /// The synthetic camera's fake controls follow the requests (and reset to defaults).
    #[test]
    #[cfg(windows)]
    fn synthetic_camera_controls_apply_and_reset() {
        let cfg = CameraConfig { width: 640, height: 360, fps: 30, ..Default::default() };
        let cap = match start_camera_preview(SYNTHETIC_CAMERA_ID, cfg, Box::new(|_| {})) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("no GPU video processor here ({e}); skipping");
                return;
            }
        };
        let get = |id| cap.controls().into_iter().find(|c| c.id == id).expect("control listed");
        let wait = |ok: &dyn Fn() -> bool| {
            let t0 = std::time::Instant::now();
            while !ok() && t0.elapsed() < std::time::Duration::from_secs(3) {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert!(ok(), "control change was not applied");
        };
        assert!(cap.controls().len() >= 8);
        let ex = get(CameraControlId::Exposure);
        assert!(ex.auto && ex.can_auto);
        cap.set_control(CameraControlId::Exposure, -3, false);
        cap.set_control(CameraControlId::Brightness, 300, false);
        wait(&|| !get(CameraControlId::Exposure).auto && get(CameraControlId::Brightness).value == 255);
        assert_eq!(get(CameraControlId::Exposure).value, -3);
        cap.reset_controls();
        wait(&|| get(CameraControlId::Exposure).auto && get(CameraControlId::Brightness).value == 128);
    }
}
