//! Go Live publishing: capture (or test source) -> NativeVideoSource / NativeAudioSource ->
//! `Screenshare` + `ScreenshareAudio` tracks.
//!
//! Two video paths:
//! - **Encoded** (Phase 3): vox-capture's hardware H.264 encoder (or the `h264file` test source)
//!   produces Annex-B access units that are published as-is (`VideoEncoderBackend::PreEncoded`).
//!   WebRTC's keyframe requests and congestion-control targets are forwarded back to the encoder.
//! - **Raw** (Phase 1 fallback): NV12 frames -> SDK -> OpenH264 software encoder.
//!
//! Each path has its own publication, created on first use and kept (muted when idle) for the
//! whole voice session; see [`SharePublication`].

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{FutureExt, StreamExt};
use parking_lot::Mutex;
use livekit::options::{AudioEncoding, TrackPublishOptions, VideoCodec, VideoEncoderBackend, VideoEncoding};
use livekit::prelude::*;
use livekit::prelude::{PublishTimingEventStream, PublishTimingStage};
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_source::{AudioSourceOptions, RtcAudioSource};
use livekit::webrtc::video_frame::{EncodedFrameType, EncodedVideoCodec, EncodedVideoFrame, FrameMetadata, NV12Buffer, VideoFrame, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::{RtcVideoSource, VideoResolution};
use tracing::{debug, info, warn};
use vox_capture::{AudioTarget, EncoderBackend, EncoderControl, IdrGate, Nv12Frame, VideoConfig, VideoTarget};
pub use vox_capture::DegradationPreference;

use super::encoders;
use super::h264;
use super::pattern::TestPattern;
use super::share_preview::{CaptureTap, SharePreview};
use crate::audio::sine::SineGenerator;
use crate::stats::{EncodedFeedback, EncoderReport, StreamCounters};

/// Pre-encoded H.264 test source: an Annex-B file replayed at a fixed rate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H264FileSource {
    pub path: PathBuf,
    pub fps: u32,
    /// Start over at the end instead of ending the share.
    pub looped: bool,
}

/// What to share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareSource {
    /// Raw HWND.
    Window(isize),
    /// Monitor index (as in `vox_capture::list_monitors`).
    Monitor(usize),
    /// Moving NV12 pattern + sine tone; no capture backend needed (raw path only).
    TestPattern,
    /// Pre-encoded H.264 from a file (encoded path only); no audio.
    H264File(H264FileSource),
    /// Audio-only Go Live ("Só áudio"): only this app's sound (its process tree). No video track,
    /// no capture, no encoder.
    AudioApp { pid: u32 },
    /// Audio-only Go Live of the whole PC's sound except our own process.
    AudioSystem,
    /// Audio-only Go Live of a stereo test tone (440 Hz left, 660 Hz right); autotests only.
    AudioTone,
}

impl ShareSource {
    /// The share publishes no video (see [`AUDIO_ONLY_TRACK_NAME`]).
    pub fn is_audio_only(&self) -> bool {
        matches!(self, Self::AudioApp { .. } | Self::AudioSystem | Self::AudioTone)
    }
}

/// Name of the track published by an audio-only Go Live (`TrackSource::ScreenshareAudio`). A
/// screen share's companion audio is called `screenshare_audio`, so viewers tell the two kinds
/// apart by this name, with no protocol change (see [`crate::voice`]).
pub const AUDIO_ONLY_TRACK_NAME: &str = "audio-only";
/// Opus bitrate of the audio-only quality choices: "Normal" and "Alta".
pub const AUDIO_ONLY_NORMAL_BPS: u64 = 128_000;
pub const AUDIO_ONLY_HIGH_BPS: u64 = 256_000;

/// Which video encoder to use for window/monitor shares (and the camera).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncoderChoice {
    /// The user's preference and the self-tested ranking ([`encoders::plan`]): the best hardware
    /// encoder of any GPU, the others as fallbacks, the processor last.
    #[default]
    Auto,
    Nvenc,
    MediaFoundation,
    /// SDK + OpenH264 on the CPU (Phase 1 path).
    Software,
}

impl std::str::FromStr for EncoderChoice {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "nvenc" => Self::Nvenc,
            "mf" | "mediafoundation" => Self::MediaFoundation,
            "software" | "sw" | "openh264" => Self::Software,
            other => bail!("unknown encoder {other:?} (auto|nvenc|mf|software)"),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareOptions {
    /// Max (capture) or exact (test pattern) output size.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Also publish app/system audio.
    pub audio: bool,
    /// Override the audio source: capture only this process tree (instead of the default
    /// derived from the video target).
    pub audio_pid: Option<u32>,
    /// Audio-only shares: Opus bitrate cap (bps), [`AUDIO_ONLY_NORMAL_BPS`] or
    /// [`AUDIO_ONLY_HIGH_BPS`]. Stereo 48 kHz either way.
    pub audio_bitrate: u64,
    pub capture_cursor: bool,
    pub encoder: EncoderChoice,
    /// Video bitrate cap (bps); `None` = [`screenshare_bitrate`] for the size and rate.
    pub max_bitrate: Option<u64>,
    /// What to give up first when the network can't carry `max_bitrate` (hardware path).
    pub degradation: DegradationPreference,
}

impl Default for ShareOptions {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 30,
            audio: true,
            audio_pid: None,
            audio_bitrate: AUDIO_ONLY_NORMAL_BPS,
            capture_cursor: true,
            encoder: EncoderChoice::Auto,
            max_bitrate: None,
            degradation: DegradationPreference::default(),
        }
    }
}

impl ShareOptions {
    /// The video bitrate cap this share publishes with.
    pub fn bitrate(&self) -> u64 {
        self.max_bitrate.unwrap_or_else(|| screenshare_bitrate(self.width, self.height, self.fps))
    }
}

/// Which video publication a share uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoPath {
    /// NV12 -> SDK -> OpenH264.
    Raw,
    /// Pre-encoded H.264 pass-through.
    Encoded,
}

/// Default max video bitrate for a screenshare (v0.2, from `docs/quality-bench.md`):
/// 1080p30 = 8 Mbps, 1080p60 = 12 Mbps, 720p30 = 4.4 Mbps. Scaled by pixel count^0.75 (smaller
/// frames need more bits per pixel for the same look) and frame rate (sub-linear above 30 fps:
/// consecutive frames are more alike).
pub fn screenshare_bitrate(width: u32, height: u32, fps: u32) -> u64 {
    let px = (width as f64 * height as f64) / (1920.0 * 1080.0);
    let fps = fps as f64;
    let fps_factor = if fps <= 30.0 { fps / 30.0 } else { 1.0 + 0.5 * ((fps - 30.0) / 30.0) };
    ((SCREENSHARE_1080P30_BPS * px.powf(0.75) * fps_factor) as u64).clamp(500_000, 20_000_000)
}

const SCREENSHARE_1080P30_BPS: f64 = 8_000_000.0;

/// Lowest bitrate we ever hand to the hardware encoder (WebRTC may briefly ask for less while
/// probing or when the link stalls; an encoder starved below this just produces mush).
const MIN_ENCODER_BPS: u64 = 150_000;

/// Test hooks shared by a share's encoded sinks (0 = off).
#[derive(Debug, Default)]
pub(crate) struct TestHooks {
    /// Upper bound on the encoder target (bps), as if the congestion controller asked for less.
    pub(crate) cap_bps: AtomicU64,
    /// Encoder output in % of the target it should produce (an encoder that overshoots).
    pub(crate) overshoot_pct: AtomicU64,
}

/// A vox-capture failure with the operation that hit it. Displays as the English log/CLI text;
/// [`capture_failure_pt`] turns it into the Portuguese text the app shows.
#[derive(Debug)]
pub struct CaptureFailure {
    pub error: vox_capture::CaptureError,
    msg: String,
}

impl std::fmt::Display for CaptureFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for CaptureFailure {}

/// Turn a vox-capture error into a user-facing error with context.
pub fn capture_error(what: &str, e: vox_capture::CaptureError) -> anyhow::Error {
    use vox_capture::CaptureError as E;
    let msg = match &e {
        E::NotFound(m) => format!("{what}: target not found ({m})"),
        E::Unsupported(m) => format!("{what}: not supported on this system ({m})"),
        E::Stopped(m) => format!("{what}: capture stopped ({m})"),
        E::Os(m) => format!("{what}: {} ({m})", if cfg!(windows) { "Windows API error" } else { "system error" }),
    };
    warn!(op = what, error = %e, "capture error");
    anyhow::Error::new(CaptureFailure { error: e, msg })
}

/// macOS: what to do when Screen Recording (which also covers an app's sound) is off for us.
pub const SCREEN_PERMISSION_PT: &str = "O macOS não deixou o concord gravar a tela. Ligue o concord em Ajustes do Sistema > Privacidade e Segurança > Gravação de Tela e Áudio do Sistema e abra o concord de novo.";

/// The capture failed because Screen Recording is off (macOS only; never on Windows).
fn screen_permission_refused(e: &anyhow::Error) -> bool {
    cfg!(target_os = "macos")
        && matches!(e.downcast_ref::<CaptureFailure>().map(|f| &f.error), Some(vox_capture::CaptureError::Os(m)) if m.contains("Screen Recording permission"))
}

/// "Windows" in the Portuguese error texts, "macOS" on the Mac.
const OS_NAME: &str = if cfg!(target_os = "macos") { "macOS" } else { "Windows" };

/// Portuguese explanation of a failed audio capture start (audio-only shares).
pub fn audio_failure_pt(e: &anyhow::Error) -> String {
    use vox_capture::CaptureError as E;
    if screen_permission_refused(e) {
        return SCREEN_PERMISSION_PT.into();
    }
    match e.downcast_ref::<CaptureFailure>().map(|f| &f.error) {
        Some(E::NotFound(_)) => "Não foi possível capturar o som: o app não existe mais (foi fechado?).".into(),
        Some(E::Unsupported(_)) if cfg!(target_os = "macos") => "Este macOS não deixa capturar o som de um app só (precisa do macOS 13 ou mais novo).".into(),
        Some(E::Unsupported(_)) => "Este Windows não deixa capturar o som de um app só (precisa do Windows 10 versão 2004 ou mais novo).".into(),
        Some(E::Stopped(_)) => "A captura de som parou assim que começou.".into(),
        Some(E::Os(m)) => format!("Erro do {OS_NAME} ao capturar o som: {m}"),
        None => format!("Não foi possível iniciar a transmissão de áudio: {e:#}"),
    }
}

/// Portuguese explanation of a failed capture start (for [`ActiveShare::switch_source`]).
pub fn capture_failure_pt(e: &anyhow::Error) -> String {
    use vox_capture::CaptureError as E;
    if screen_permission_refused(e) {
        return SCREEN_PERMISSION_PT.into();
    }
    match e.downcast_ref::<CaptureFailure>().map(|f| &f.error) {
        Some(E::NotFound(_)) => "A janela ou tela escolhida não existe mais (foi fechada?).".into(),
        Some(E::Unsupported(_)) => "Esta janela ou tela não pode ser capturada neste computador.".into(),
        Some(E::Stopped(_)) => "A captura parou assim que começou.".into(),
        Some(E::Os(m)) => format!("Erro do {OS_NAME} ao iniciar a captura: {m}"),
        None => format!("{e:#}"),
    }
}

/// Run a vox-capture call and map its error (see [`capture_error`]).
pub fn capture_call<T>(what: &str, f: impl FnOnce() -> vox_capture::Result<T>) -> Result<T> {
    f().map_err(|e| capture_error(what, e))
}

/// PID owning a window (for `AudioTarget::ProcessTree`).
pub fn window_pid(hwnd: isize) -> Option<u32> {
    if let Ok(list) = capture_call("list_windows", vox_capture::list_windows) {
        if let Some(w) = list.iter().find(|w| w.hwnd == hwnd) {
            return Some(w.process_id);
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
        let mut pid = 0u32;
        // SAFETY: plain Win32 query; an invalid HWND just returns 0.
        let tid = unsafe { GetWindowThreadProcessId(hwnd as *mut core::ffi::c_void, &mut pid) };
        if tid != 0 && pid != 0 {
            return Some(pid);
        }
    }
    None
}

/// Default audio target for a video target (see module docs of vox-capture):
/// window -> only that app's process tree; monitor -> everything except our own process.
pub fn default_audio_target(source: &ShareSource) -> Option<AudioTarget> {
    match source {
        ShareSource::Window(hwnd) => window_pid(*hwnd).map(|pid| AudioTarget::ProcessTree { pid }),
        ShareSource::Monitor(_) => Some(AudioTarget::SystemExcludingProcessTree { pid: std::process::id() }),
        ShareSource::AudioApp { pid } => Some(AudioTarget::ProcessTree { pid: *pid }),
        ShareSource::AudioSystem => Some(AudioTarget::SystemExcludingProcessTree { pid: std::process::id() }),
        ShareSource::TestPattern | ShareSource::H264File(_) | ShareSource::AudioTone => None,
    }
}

/// Test mode only ([`crate::testmode::guards_real_capture`]): why `source`, with the sound of
/// `audio_pid`, must not be captured. Only the `test_window` example, this app's own window and
/// the synthetic sources may be: never a real screen, the PC's sound or another program.
pub fn test_capture_refusal(source: &ShareSource, audio_pid: Option<u32>) -> Option<String> {
    if !crate::testmode::guards_real_capture() {
        return None;
    }
    refusal_for(source, audio_pid, window_pid, is_test_process)
}

fn is_test_process(pid: u32) -> bool {
    capture_call("list_windows", vox_capture::list_windows).is_ok_and(|l| l.iter().any(|w| w.process_id == pid && crate::testmode::is_test_capture_exe(&w.exe_name)))
}

fn refusal_for(source: &ShareSource, audio_pid: Option<u32>, pid_of: impl Fn(isize) -> Option<u32>, is_test: impl Fn(u32) -> bool) -> Option<String> {
    let what = match source {
        ShareSource::Monitor(i) => Some(format!("a real screen (Monitor({i}))")),
        ShareSource::AudioSystem => Some("the PC's sound".to_string()),
        ShareSource::Window(h) => match pid_of(*h) {
            Some(pid) if pid == std::process::id() || is_test(pid) => None,
            _ => Some(format!("window {h}, which is not a test window")),
        },
        ShareSource::AudioApp { pid } if !is_test(*pid) => Some(format!("the sound of process {pid}, which is not a test program")),
        _ => None,
    };
    let what = what.or_else(|| audio_pid.filter(|p| !is_test(*p)).map(|p| format!("the sound of process {p}, which is not a test program")))?;
    Some(format!(
        "test mode refuses to capture {what}: only the test window, this app's own window or a test source (set {}=1 to allow it by hand)",
        crate::testmode::ALLOW_REAL_CAPTURE_ENV
    ))
}

/// A stoppable producer thread (test pattern / sine / file).
pub(crate) struct ProducerThread {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ProducerThread {
    pub(crate) fn spawn(name: &str, f: impl FnOnce(Arc<AtomicBool>) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let handle = std::thread::Builder::new().name(name.into()).spawn(move || f(s)).expect("spawn producer");
        Self { stop, handle: Some(handle) }
    }

    fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(|h| h.is_finished())
    }
}

impl Drop for ProducerThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// What a frame gate does with the frames of one capture while a source switch is in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameMode {
    /// Started but not yet in charge: frames are held back (only the latest keyframe is kept).
    Standby,
    /// In charge, but the cached keyframe was stale: waiting for the next keyframe.
    AwaitKey,
    /// In charge: every frame goes to the video source.
    Live,
    /// Replaced by another capture: every frame is dropped.
    Retired,
}

/// The pure state machine of a source switch, for one capture's frames. A new capture is started
/// in [`FrameMode::Standby`] next to the running one; once it proves it can produce frames, the old
/// gate is retired and the new one activated, so the shared video source never sees two captures
/// interleaved and the stream continues at a keyframe.
///
/// Delta frames are only ever forwarded after a keyframe: in standby the latest keyframe is kept
/// and, if no delta arrived after it, handed out on activation (a static window delivers one
/// frame and then nothing); otherwise the gate waits for the next keyframe.
pub(crate) struct FrameGate<F> {
    mode: FrameMode,
    cached: Option<F>,
    seen_frame: bool,
}

impl<F> FrameGate<F> {
    pub(crate) fn live() -> Self {
        Self { mode: FrameMode::Live, cached: None, seen_frame: true }
    }

    pub(crate) fn standby() -> Self {
        Self { mode: FrameMode::Standby, cached: None, seen_frame: false }
    }

    pub(crate) fn mode(&self) -> FrameMode {
        self.mode
    }

    /// Standby only: the capture has delivered at least one frame.
    pub(crate) fn seen_frame(&self) -> bool {
        self.seen_frame
    }

    /// One captured frame arrives; returns what to forward now. `build` makes the owned frame and
    /// only runs when the gate keeps or forwards it.
    pub(crate) fn offer(&mut self, keyframe: bool, build: impl FnOnce() -> F) -> Option<F> {
        match self.mode {
            FrameMode::Live => Some(build()),
            FrameMode::Retired => None,
            FrameMode::AwaitKey => {
                if keyframe {
                    self.mode = FrameMode::Live;
                    Some(build())
                } else {
                    None
                }
            }
            FrameMode::Standby => {
                self.seen_frame = true;
                // A delta makes the kept keyframe useless as a starting point.
                self.cached = if keyframe { Some(build()) } else { None };
                None
            }
        }
    }

    /// Take over the stream. Returns the kept keyframe to forward right away, or `None` when the
    /// caller must ask the encoder for one (the gate then waits in [`FrameMode::AwaitKey`]).
    pub(crate) fn activate(&mut self) -> Option<F> {
        if self.mode != FrameMode::Standby {
            return None;
        }
        match self.cached.take() {
            Some(f) => {
                self.mode = FrameMode::Live;
                Some(f)
            }
            None => {
                self.mode = FrameMode::AwaitKey;
                None
            }
        }
    }

    pub(crate) fn retire(&mut self) {
        self.mode = FrameMode::Retired;
        self.cached = None;
    }
}

/// A frame gate plus the timestamps used to measure the gap of a switch.
pub(crate) struct Stage<F> {
    gate: FrameGate<F>,
    first_pass: Option<Instant>,
    last_pass: Option<Instant>,
}

impl<F> Stage<F> {
    pub(crate) fn new(standby: bool) -> Self {
        Self { gate: if standby { FrameGate::standby() } else { FrameGate::live() }, first_pass: None, last_pass: None }
    }

    fn note_pass(&mut self) {
        let now = Instant::now();
        self.first_pass.get_or_insert(now);
        self.last_pass = Some(now);
    }
}

/// An owned encoded frame, kept by a standby gate.
pub(crate) struct EncFrame {
    data: Vec<u8>,
    keyframe: bool,
    width: u32,
    height: u32,
    timestamp_us: i64,
}

/// The pre-encoded path's frame sink behind a gate. Shared between the capture thread and the
/// switch logic (locks are held for one frame at most).
pub(crate) struct EncStage {
    pub(crate) stage: Stage<EncFrame>,
    pub(crate) sink: EncodedSink,
}

pub(crate) type EncHandle = Arc<Mutex<EncStage>>;

impl EncStage {
    pub(crate) fn on_frame(&mut self, f: &vox_capture::EncodedFrame<'_>) {
        if self.stage.gate.mode() == FrameMode::Live {
            self.sink.push(f.data, f.keyframe, f.width, f.height, f.timestamp_us);
            self.stage.note_pass();
        } else if let Some(o) = self.stage.gate.offer(f.keyframe, || EncFrame {
            data: f.data.to_vec(),
            keyframe: f.keyframe,
            width: f.width,
            height: f.height,
            timestamp_us: f.timestamp_us,
        }) {
            self.push_owned(o);
        }
    }

    fn push_owned(&mut self, o: EncFrame) {
        self.sink.push(&o.data, o.keyframe, o.width, o.height, o.timestamp_us);
        self.stage.note_pass();
    }

    /// Take over the stream; asks the encoder for a keyframe when none could be handed out.
    fn activate(&mut self) -> FrameMode {
        match self.stage.gate.activate() {
            Some(o) => self.push_owned(o),
            None => self.sink.request_keyframe(),
        }
        self.stage.gate.mode()
    }
}

/// One raw frame, ready for the SDK (kept by a standby gate).
pub(crate) struct RawFrame {
    frame: RawBuffer,
    width: u32,
    height: u32,
}

/// The picture of a raw frame: our own NV12 copy, or (macOS) the capture's IOSurface-backed
/// `CVPixelBuffer` itself, which libwebrtc's VideoToolbox encoder takes without a copy.
pub(crate) enum RawBuffer {
    Nv12(VideoFrame<NV12Buffer>),
    #[cfg(target_os = "macos")]
    Native(VideoFrame<livekit::webrtc::video_frame::native::NativeBuffer>),
}

/// The raw path's frame sink behind a gate.
pub(crate) struct RawStage {
    pub(crate) stage: Stage<Option<RawFrame>>,
    pub(crate) source: NativeVideoSource,
    pub(crate) counters: Arc<StreamCounters>,
}

pub(crate) type RawHandle = Arc<Mutex<RawStage>>;

impl RawStage {
    pub(crate) fn on_frame(&mut self, f: &Nv12Frame<'_>) {
        // Every raw frame is a full picture, so each one can start a stream.
        let counters = self.counters.clone();
        if let Some(o) = self.stage.gate.offer(true, || build_nv12_frame(&counters, f)).flatten() {
            self.submit(o);
        }
    }

    fn submit(&mut self, o: RawFrame) {
        let _ = match &o.frame {
            RawBuffer::Nv12(f) => self.source.capture_frame(f),
            #[cfg(target_os = "macos")]
            RawBuffer::Native(f) => self.source.capture_frame(f),
        };
        self.counters.frame(o.width, o.height);
        self.stage.note_pass();
    }

    fn activate(&mut self) -> FrameMode {
        if let Some(Some(o)) = self.stage.gate.activate() {
            self.submit(o);
        }
        self.stage.gate.mode()
    }
}

/// Dropping it stops the capture / joins the thread.
#[allow(dead_code)] // the capture handles are held only for their Drop
enum VideoProducer {
    Capture(vox_capture::VideoCapture, RawHandle, Option<CaptureTap>),
    Test(ProducerThread),
    Encoded(vox_capture::EncodedVideoCapture, EncHandle, Option<CaptureTap>),
    File(ProducerThread),
}

impl VideoProducer {
    fn running(&self) -> bool {
        match self {
            Self::Capture(c, ..) => c.is_running(),
            Self::Encoded(c, ..) => c.is_running(),
            Self::Test(_) => true,
            Self::File(t) => !t.is_finished(),
        }
    }

    /// Gate state of a capture-backed producer (`None` for the test pattern / file replay).
    fn gate_mode(&self) -> Option<FrameMode> {
        match self {
            Self::Capture(_, h, _) => Some(h.lock().stage.gate.mode()),
            Self::Encoded(_, h, _) => Some(h.lock().stage.gate.mode()),
            Self::Test(_) | Self::File(_) => None,
        }
    }

    fn seen_frame(&self) -> bool {
        match self {
            Self::Capture(_, h, _) => h.lock().stage.gate.seen_frame(),
            Self::Encoded(_, h, _) => h.lock().stage.gate.seen_frame(),
            Self::Test(_) | Self::File(_) => true,
        }
    }

    fn retire(&self) {
        match self {
            Self::Capture(_, h, _) => h.lock().stage.gate.retire(),
            Self::Encoded(_, h, _) => h.lock().stage.gate.retire(),
            Self::Test(_) | Self::File(_) => {}
        }
    }

    fn activate(&self) -> FrameMode {
        match self {
            Self::Capture(_, h, _) => h.lock().activate(),
            Self::Encoded(_, h, _) => h.lock().activate(),
            Self::Test(_) | Self::File(_) => FrameMode::Live,
        }
    }

    fn preview_tap(&self) -> Option<&CaptureTap> {
        match self {
            Self::Capture(_, _, t) | Self::Encoded(_, _, t) => t.as_ref(),
            Self::Test(_) | Self::File(_) => None,
        }
    }

    /// When this producer first and last handed a frame to the video source.
    fn pass_times(&self) -> (Option<Instant>, Option<Instant>) {
        match self {
            Self::Capture(_, h, _) => {
                let h = h.lock();
                (h.stage.first_pass, h.stage.last_pass)
            }
            Self::Encoded(_, h, _) => {
                let h = h.lock();
                (h.stage.first_pass, h.stage.last_pass)
            }
            Self::Test(_) | Self::File(_) => (None, None),
        }
    }
}

/// Dropping it stops the capture / joins the thread.
#[allow(dead_code)] // Sine's thread is held only for its Drop
enum AudioProducer {
    Capture(vox_capture::AudioCapture),
    Sine(ProducerThread),
}

/// One screenshare video publication (raw or pre-encoded).
struct VideoPublication {
    path: VideoPath,
    source: NativeVideoSource,
    track: LocalVideoTrack,
    publication: LocalTrackPublication,
    fps: u32,
    max_bitrate: u64,
    /// libwebrtc's encoder for this sender (`PreEncoded` on the encoded path).
    encoder: VideoEncoderBackend,
}

impl VideoPublication {
    #[allow(clippy::too_many_arguments)]
    async fn publish(room: &Room, path: VideoPath, source: NativeVideoSource, width: u32, height: u32, fps: u32, max_bitrate: u64, encoder: VideoEncoderBackend) -> Result<Self> {
        let track = LocalVideoTrack::create_video_track("screenshare", RtcVideoSource::Native(source.clone()));
        // v1.1 (E1): the encoded path's per-frame timing events (`publish_timing_events`) need a
        // sender frame transformer. Subscribed BEFORE publishing, the SDK makes it at the publish
        // and an end-to-end encrypted room's frame cryptor chains itself in front of it. Made after
        // (the first subscription on a published track), it REPLACED the cryptor on the sender:
        // the pre-encoded H.264 then went out unencrypted (found by the e2v gate). Later
        // subscriptions reuse that transformer.
        if encoder == VideoEncoderBackend::PreEncoded {
            drop(track.publish_timing_events());
        }
        let publish = TrackPublishOptions {
            source: TrackSource::Screenshare,
            video_codec: VideoCodec::H264,
            simulcast: false,
            video_encoding: Some(VideoEncoding { max_bitrate, max_framerate: fps as f64 }),
            video_encoder: encoder,
            ..Default::default()
        };
        let publication = room
            .local_participant()
            .publish_track(LocalTrack::Video(track.clone()), publish)
            .await
            .with_context(|| format!("publish screenshare video ({path:?} path)"))?;
        info!(
            ?path,
            width,
            height,
            fps,
            max_kbps = max_bitrate / 1000,
            codec = "h264",
            "screenshare video published (host upload: ~{:.1} Mbps per viewer at full quality)",
            max_bitrate as f64 / 1e6
        );
        Ok(Self { path, source, track, publication, fps, max_bitrate, encoder })
    }

    fn compatible(&self, fps: u32, max_bitrate: u64, encoder: VideoEncoderBackend) -> bool {
        self.fps == fps && self.max_bitrate == max_bitrate && self.encoder == encoder
    }

    async fn unpublish(self, room: &Room) {
        if let Err(e) = room.local_participant().unpublish_track(&self.publication.sid()).await {
            warn!(error = %e, path = ?self.path, "unpublish screenshare video");
        }
    }
}

/// The published Go Live tracks. Kept for the whole voice session and reused by every share:
/// LiveKit's `unpublish_track` only calls `RemoveTrack` and never stops the transceiver, so each
/// unpublish left a sender + H.264 encoder (~54 MB at 1080p) alive until the room closed.
/// Between shares the publications are muted and no frames are pushed: no encoding, no
/// bandwidth, and the SFU tells viewers the share stopped.
///
/// The raw and pre-encoded paths need different senders (OpenH264 vs pass-through encoder), so
/// each path gets its own video publication the first time it is used, and switching paths
/// across shares just mutes one and unmutes the other: at most two video senders per session.
#[derive(Default)]
pub struct SharePublication {
    raw: Option<VideoPublication>,
    encoded: Option<VideoPublication>,
    /// The audio that goes with a screen share (`screenshare_audio`, 128 kbps).
    audio: Option<AudioPublication>,
    /// The tracks of audio-only shares (`audio-only`), one per bitrate choice, made on first use.
    /// Each Opus sender is small (no video encoder), so a session that used both qualities holds
    /// two; the opposite of re-publishing, which would leak a sender each time.
    audio_only: Vec<AudioPublication>,
}

struct AudioPublication {
    source: NativeAudioSource,
    track: LocalAudioTrack,
    publication: LocalTrackPublication,
    bitrate: u64,
}

/// Which audio publication a share uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioKind {
    /// Next to a screen share.
    Companion,
    /// A share of nothing but audio, at this Opus bitrate cap.
    Only { bitrate: u64 },
}

impl SharePublication {
    /// The publication of `kind`, published on first use (48 kHz stereo Opus, no voice
    /// processing, no DTX).
    ///
    /// Stereo: the source has two channels, but libwebrtc only encodes stereo (Opus audio
    /// application, not voip) when the negotiated send codec says `stereo=1`, and the server
    /// only answers that when the publish request asks for it (`TrackPublishOptions::stereo`,
    /// added by the vendored livekit crate: the SDK's own options cannot). The result shows in
    /// the stats line of the stream (`codec=... stereo=1`) and is checked end to end, with a tone
    /// that differs between the channels, by the `audio-a` / `audio-b` autotests.
    async fn ensure_audio(&mut self, room: &Room, kind: AudioKind) -> Result<&AudioPublication> {
        let found = match kind {
            AudioKind::Companion => self.audio.is_some(),
            AudioKind::Only { bitrate } => self.audio_only.iter().any(|a| a.bitrate == bitrate),
        };
        if !found {
            // App/music audio: no echo cancellation / noise suppression / AGC.
            let opts = AudioSourceOptions { echo_cancellation: false, noise_suppression: false, auto_gain_control: false };
            let source = NativeAudioSource::new(opts, vox_capture::AUDIO_SAMPLE_RATE, vox_capture::AUDIO_CHANNELS as u32, 0);
            let (name, bitrate) = match kind {
                AudioKind::Companion => ("screenshare_audio", 128_000),
                AudioKind::Only { bitrate } => (AUDIO_ONLY_TRACK_NAME, bitrate),
            };
            let track = LocalAudioTrack::create_audio_track(name, RtcAudioSource::Native(source.clone()));
            let publish = TrackPublishOptions {
                source: TrackSource::ScreenshareAudio,
                audio_encoding: Some(AudioEncoding { max_bitrate: bitrate }),
                dtx: false,
                red: false,
                // Without this the server answers without `stereo=1` and libwebrtc sends mono (a
                // two-channel source is downmixed): see the vendored livekit crate.
                stereo: true,
                ..Default::default()
            };
            let publication = room
                .local_participant()
                .publish_track(LocalTrack::Audio(track.clone()), publish)
                .await
                .context("publish screenshare audio")?;
            info!(name, max_kbps = bitrate / 1000, "screenshare audio published (48 kHz stereo Opus, no DTX, APM off)");
            let a = AudioPublication { source, track, publication, bitrate };
            match kind {
                AudioKind::Companion => self.audio = Some(a),
                AudioKind::Only { .. } => self.audio_only.push(a),
            }
        }
        Ok(match kind {
            AudioKind::Companion => self.audio.as_ref().expect("published above"),
            AudioKind::Only { bitrate } => self.audio_only.iter().find(|a| a.bitrate == bitrate).expect("published above"),
        })
    }

    fn audio_pubs(&self) -> impl Iterator<Item = &AudioPublication> {
        self.audio.iter().chain(self.audio_only.iter())
    }

    fn slot(&mut self, path: VideoPath) -> &mut Option<VideoPublication> {
        match path {
            VideoPath::Raw => &mut self.raw,
            VideoPath::Encoded => &mut self.encoded,
        }
    }

    fn video(&self, path: VideoPath) -> Option<&VideoPublication> {
        match path {
            VideoPath::Raw => self.raw.as_ref(),
            VideoPath::Encoded => self.encoded.as_ref(),
        }
    }

    /// Drop the path's publication if its encoding settings don't match (the old sender stays
    /// allocated until leave: SDK limitation, see the type docs).
    async fn drop_incompatible(&mut self, room: &Room, path: VideoPath, fps: u32, max_bitrate: u64, encoder: VideoEncoderBackend) {
        let slot = self.slot(path);
        if slot.as_ref().is_some_and(|v| !v.compatible(fps, max_bitrate, encoder)) {
            info!(?path, fps, max_kbps = max_bitrate / 1000, "share settings changed; republishing screenshare track");
            if let Some(old) = slot.take() {
                old.unpublish(room).await;
            }
        }
    }

    fn mute(&self) {
        for v in [&self.raw, &self.encoded].into_iter().flatten() {
            v.publication.mute();
        }
        for a in self.audio_pubs() {
            a.publication.mute();
        }
    }

    /// Unpublish for good (leaving the room).
    pub async fn unpublish(self, room: &Room) {
        for v in [self.raw, self.encoded].into_iter().flatten() {
            v.unpublish(room).await;
        }
        for a in self.audio.into_iter().chain(self.audio_only) {
            if let Err(e) = room.local_participant().unpublish_track(&a.publication.sid()).await {
                warn!(error = %e, "unpublish screenshare audio");
            }
        }
    }
}

/// What `start_video_path` started: path, producer, counters, pre-encoded feedback, encoder.
type StartedVideo = (VideoPath, VideoProducer, Arc<StreamCounters>, Option<(String, Arc<EncodedFeedback>)>, Option<encoders::Attempt>);

/// A running Go Live (tracks published and unmuted, producer running).
pub struct ActiveShare {
    pub source: ShareSource,
    pub opts: ShareOptions,
    /// `None` for an audio-only share: there is no video track, capture or encoder at all.
    pub path: Option<VideoPath>,
    pub video_track: Option<LocalVideoTrack>,
    pub audio_track: Option<LocalAudioTrack>,
    pub video_counters: Arc<StreamCounters>,
    pub audio_counters: Option<Arc<StreamCounters>>,
    /// Video bitrate cap the publication was made with (kept across source switches).
    max_bitrate: u64,
    /// Pre-encoded path: feedback counters and backend name for the stats line.
    encoder: Option<(String, Arc<EncodedFeedback>)>,
    /// The encoder in use ([`encoders::Attempt`]): hardware backend + adapter, kept for "Trocar
    /// tela" (the new capture uses the same one).
    attempt: Option<encoders::Attempt>,
    /// Test hooks (bitrate cap, encoder overshoot); see [`ActiveShare::set_bitrate_cap`].
    bitrate_cap: Arc<TestHooks>,
    video: Option<VideoProducer>,
    audio: Option<AudioProducer>,
    publication: Option<SharePublication>,
    /// Our own preview tile (item 8a), when the app asked for one.
    preview: Option<SharePreview>,
}

/// What to do with the screenshare audio when the shared source changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioPlan {
    /// Audio was off and stays off.
    Nothing,
    /// Audio was on and gets turned off: stop the capture, mute the publication.
    Stop,
    /// Capture the new source's audio. `replace`: an audio capture of the old source is running
    /// and is stopped first. `publish`: the audio track does not exist yet and is published once.
    Start { replace: bool, publish: bool },
}

pub(crate) fn plan_audio(had_capture: bool, want: bool, publication_exists: bool) -> AudioPlan {
    match (had_capture, want) {
        (false, false) => AudioPlan::Nothing,
        (true, false) => AudioPlan::Stop,
        (had, true) => AudioPlan::Start { replace: had, publish: !publication_exists },
    }
}

/// What the switch does once the new capture had its chance to produce a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwitchDecision {
    /// The new capture is delivering: hand the stream over.
    Commit,
    /// The new capture failed or stayed silent and the old one is alive: keep sharing the old one.
    KeepOld,
    /// The old capture is already dead and the new one runs, silent for now: use it anyway.
    CommitSilent,
    /// Neither capture runs.
    Fail,
}

pub(crate) fn switch_decision(new_running: bool, new_delivered: bool, old_alive: bool) -> SwitchDecision {
    if new_running && new_delivered {
        SwitchDecision::Commit
    } else if old_alive {
        SwitchDecision::KeepOld
    } else if new_running {
        SwitchDecision::CommitSilent
    } else {
        SwitchDecision::Fail
    }
}

/// How long a new capture gets to deliver its first frame before the switch is given up.
const SWITCH_READY_TIMEOUT: Duration = Duration::from_millis(2000);
/// How long to wait for the first keyframe of the new capture after the hand-over.
const SWITCH_LIVE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Change what a running share captures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchRequest {
    /// Window or monitor.
    pub source: ShareSource,
    /// Share the new source's audio (window: that app's process tree; monitor: the system minus
    /// us), or none.
    pub audio: bool,
    /// Capture this process tree instead of the one derived from the source.
    pub audio_pid: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct SwitchReport {
    pub from: ShareSource,
    pub to: ShareSource,
    pub path: Option<VideoPath>,
    /// Time between the last frame of the old source and the first of the new one.
    pub gap: Duration,
    /// Total time of the switch, including starting the new capture.
    pub total: Duration,
    pub audio_before: bool,
    pub audio_after: bool,
    /// Why the new source's audio is missing although it was requested (Portuguese).
    pub audio_error: Option<String>,
}

/// Failed share start; hands the (muted) publications back so they can still be reused.
pub struct StartError {
    pub error: anyhow::Error,
    pub publication: Option<Box<SharePublication>>,
}

impl ActiveShare {
    /// Start sharing, reusing the session's publications (`reuse`) when their settings match.
    pub async fn start(
        room: &Room,
        source: ShareSource,
        opts: ShareOptions,
        reuse: Option<SharePublication>,
    ) -> std::result::Result<ActiveShare, StartError> {
        Self::start_with_preview(room, source, opts, reuse, None).await
    }

    /// [`ActiveShare::start`] with the sharer's own preview (see [`SharePreview`]): the capture
    /// produces small frames for it while its settings say `on`.
    pub async fn start_with_preview(
        room: &Room,
        source: ShareSource,
        opts: ShareOptions,
        reuse: Option<SharePublication>,
        preview: Option<SharePreview>,
    ) -> std::result::Result<ActiveShare, StartError> {
        if source.is_audio_only() {
            return Self::start_audio_only(room, source, opts, reuse).await;
        }
        let mut pubs = reuse.unwrap_or_default();
        let bitrate_cap = Arc::new(TestHooks::default());
        let res = Self::start_video_path(room, &source, &opts, &mut pubs, bitrate_cap.clone(), preview.as_ref()).await;
        let (path, video, video_counters, encoder, attempt) = match res {
            Ok(v) => v,
            Err(error) => {
                pubs.mute();
                return Err(StartError { error, publication: Some(Box::new(pubs)) });
            }
        };
        let vp = pubs.video(path).expect("video publication present after start");
        let video_track = vp.track.clone();
        let vp_max_bitrate = vp.max_bitrate;
        // Unmute right away: frames pushed into a disabled track become black raw frames, which
        // the pass-through encoder rejects.
        vp.publication.unmute();
        let mut share = ActiveShare {
            source,
            opts,
            path: Some(path),
            video_track: Some(video_track),
            audio_track: None,
            video_counters,
            audio_counters: None,
            max_bitrate: vp_max_bitrate,
            encoder,
            attempt,
            bitrate_cap,
            video: Some(video),
            audio: None,
            publication: Some(pubs),
            preview,
        };
        if let Some(t) = share.video.as_ref().and_then(|v| v.preview_tap()) {
            t.activate();
        }
        // Audio is best effort: video-only if it fails.
        if opts.audio {
            if let Err(e) = share.start_audio(room).await {
                warn!(error = %format!("{e:#}"), "screenshare audio unavailable; sharing video only");
            }
        }
        info!(source = ?share.source, ?path, fps = opts.fps, audio = share.audio.is_some(), "screenshare started");
        Ok(share)
    }

    /// Audio-only Go Live: publish just the audio track (the `audio-only` one of the chosen
    /// bitrate) and capture the sound. No video track, capture or encoder exists; the idle cost is
    /// the audio capture plus Opus. Unlike the companion audio of a screen share this is not
    /// best effort: without sound there is no share, so a failure is an error (in Portuguese).
    async fn start_audio_only(
        room: &Room,
        source: ShareSource,
        opts: ShareOptions,
        reuse: Option<SharePublication>,
    ) -> std::result::Result<ActiveShare, StartError> {
        let pubs = reuse.unwrap_or_default();
        pubs.mute();
        let mut share = ActiveShare {
            source,
            opts,
            path: None,
            video_track: None,
            audio_track: None,
            video_counters: Arc::new(StreamCounters::default()),
            audio_counters: None,
            max_bitrate: 0,
            encoder: None,
            attempt: None,
            bitrate_cap: Arc::new(TestHooks::default()),
            video: None,
            audio: None,
            publication: Some(pubs),
            preview: None,
        };
        match share.start_audio(room).await {
            Ok(()) => {
                info!(source = ?share.source, kbps = opts.audio_bitrate / 1000, "audio-only screenshare started");
                Ok(share)
            }
            Err(e) => {
                let publication = share.publication.take().map(|p| {
                    p.mute();
                    Box::new(p)
                });
                Err(StartError { error: anyhow!(audio_failure_pt(&e)), publication })
            }
        }
    }

    /// Pick the video path (encoded if possible, else raw), start its producer and make sure its
    /// publication exists. Other paths' publications stay muted. Window / monitor shares try the
    /// encoders of [`encoders::plan`] in order (a GPU whose encoder or cross-adapter capture
    /// fails hands over to the next one); the processor is the last resort.
    async fn start_video_path(
        room: &Room,
        source: &ShareSource,
        opts: &ShareOptions,
        pubs: &mut SharePublication,
        bitrate_cap: Arc<TestHooks>,
        preview: Option<&SharePreview>,
    ) -> Result<StartedVideo> {
        let (width, height) = initial_resolution(source, opts);
        let fps = match source {
            ShareSource::H264File(f) => f.fps,
            _ => opts.fps,
        };
        let max_bitrate = opts.max_bitrate.unwrap_or_else(|| screenshare_bitrate(width, height, fps));
        let counters = Arc::new(StreamCounters::default());

        let mut software = None;
        if let Some(backend) = encoded_backend(source, opts.encoder) {
            // The file is its own encoder; captures go through the plan.
            let attempts: Vec<Option<encoders::Attempt>> = match backend {
                None => vec![None],
                Some(_) => {
                    let plan = encoders::plan(opts.encoder);
                    info!(plan = %encoders::describe(&plan), "share encoder plan");
                    software = plan.iter().find(|a| a.is_software()).cloned();
                    plan.into_iter().filter(|a| !a.is_software() && !encoders::in_libwebrtc(a)).map(Some).collect()
                }
            };
            for attempt in attempts {
                pubs.drop_incompatible(room, VideoPath::Encoded, fps, max_bitrate, VideoEncoderBackend::PreEncoded).await;
                // Reuse the session's encoded source, or make a new one (published below, after
                // the encoder started, so a failed start never shows viewers a dead track).
                let video_source = match &pubs.encoded {
                    Some(v) => v.source.clone(),
                    None => NativeVideoSource::new_encoded(VideoResolution { width, height }),
                };
                let feedback = Arc::new(EncodedFeedback::default());
                let timing: TimingSlot = Arc::new(Mutex::new(None));
                let hw = attempt.as_ref().and_then(|a| a.backend.map(|b| (b, a.luid)));
                let started = start_encoded(source, opts, hw, max_bitrate, video_source.clone(), counters.clone(), feedback.clone(), bitrate_cap.clone(), false, timing.clone(), preview);
                let label = attempt.as_ref().map(|a| a.label.clone()).unwrap_or_else(|| "-".into());
                match started {
                    Ok((producer, backend_name)) => {
                        if pubs.encoded.is_none() {
                            // The encoder may already be producing; frames pushed before the track
                            // is published are dropped and the first subscriber's PLI gets an IDR.
                            let v = VideoPublication::publish(room, VideoPath::Encoded, video_source, width, height, fps, max_bitrate, VideoEncoderBackend::PreEncoded).await?;
                            pubs.encoded = Some(v);
                        }
                        // Per-frame "WebRTC encoded it" events (see `DropTracker`). This installs a
                        // sender frame transformer that only observes: packet trailers stay off, so
                        // the bitstream viewers get is unchanged.
                        let track = &pubs.encoded.as_ref().expect("published above").track;
                        *timing.lock() = Some(track.publish_timing_events());
                        info!(backend = %backend_name, encoder = %label, max_kbps = max_bitrate / 1000, "share path: pre-encoded H.264 (no software encoder)");
                        return Ok((VideoPath::Encoded, producer, counters, Some((backend_name, feedback)), attempt));
                    }
                    Err(e) if matches!(source, ShareSource::H264File(_)) => return Err(e),
                    Err(e) => {
                        warn!(error = %format!("{e:#}"), encoder = %label, "hardware encoder failed to start; trying the next one");
                    }
                }
            }
        }

        // macOS: the plan's hardware encoder (VideoToolbox) runs inside libwebrtc, on this path.
        let (encoder, hw_attempt) = encoders::raw_encoder(opts.encoder);
        pubs.drop_incompatible(room, VideoPath::Raw, fps, max_bitrate, encoder).await;
        if pubs.raw.is_none() {
            let video_source = NativeVideoSource::new(VideoResolution { width, height }, true);
            pubs.raw = Some(VideoPublication::publish(room, VideoPath::Raw, video_source, width, height, fps, max_bitrate, encoder).await?);
        }
        let video_source = pubs.raw.as_ref().expect("published above").source.clone();
        let producer = start_raw(source, opts, width, height, video_source, counters.clone(), false, preview)?;
        match &hw_attempt {
            Some(a) => info!(encoder = %a.label, "share path: raw frames + libwebrtc's hardware H.264 (VideoToolbox, frames passed without a copy)"),
            None => info!(reason = raw_reason(source, opts.encoder), "share path: raw frames + software H.264 (OpenH264)"),
        }
        let capture = matches!(source, ShareSource::Window(_) | ShareSource::Monitor(_));
        let attempt = hw_attempt.or(software).or_else(|| capture.then(|| encoders::plan(EncoderChoice::Software).remove(0)));
        Ok((VideoPath::Raw, producer, counters, None, attempt))
    }

    async fn start_audio(&mut self, room: &Room) -> Result<()> {
        if matches!(self.source, ShareSource::H264File(_)) && self.opts.audio_pid.is_none() {
            return Ok(()); // a file has no sound of its own
        }
        let source = self.source.clone();
        let (producer, counters, track) = self.make_audio_producer(room, &source).await?;
        self.audio_track = Some(track);
        self.audio_counters = Some(counters);
        self.audio = Some(producer);
        Ok(())
    }

    /// Start capturing the sound of `source` into the share's audio publication (published
    /// first if the session never had it) and unmute that publication. Nothing in `self` is
    /// replaced, so the caller decides what happens to the previous capture.
    async fn make_audio_producer(&mut self, room: &Room, source: &ShareSource) -> Result<(AudioProducer, Arc<StreamCounters>, LocalAudioTrack)> {
        let audio_only = source.is_audio_only();
        let target = match self.opts.audio_pid {
            Some(pid) if !audio_only => Some(AudioTarget::ProcessTree { pid }),
            _ => default_audio_target(source),
        };
        let kind = if audio_only { AudioKind::Only { bitrate: self.opts.audio_bitrate } } else { AudioKind::Companion };
        let publication = self.publication.as_mut().expect("publication present while sharing");
        let audio = publication.ensure_audio(room, kind).await?;
        let counters = Arc::new(StreamCounters::default());
        let producer = match (source, target) {
            (ShareSource::TestPattern | ShareSource::AudioTone, None) => AudioProducer::Sine(spawn_sine(audio.source.clone(), counters.clone())),
            (other, target) => {
                let target = target.ok_or_else(|| anyhow!("cannot resolve audio target for {other:?}"))?;
                let src = audio.source.clone();
                let c = counters.clone();
                let cap = capture_call("start_audio", move || {
                    vox_capture::start_audio(target, Box::new(move |chunk| push_audio_chunk(&src, &c, chunk)))
                })?;
                info!(?target, "screenshare audio capture started");
                AudioProducer::Capture(cap)
            }
        };
        let track = audio.track.clone();
        // One audio publication is live at a time: the others stay muted.
        for a in publication.audio_pubs() {
            if a.track.sid() == track.sid() {
                a.publication.unmute();
            } else {
                a.publication.mute();
            }
        }
        Ok((producer, counters, track))
    }

    /// `false` once the capture thread ended on its own (window closed, monitor gone, device
    /// lost, file finished). Test patterns never stop.
    pub fn video_running(&self) -> bool {
        if self.source.is_audio_only() {
            return self.audio_running();
        }
        match &self.video {
            Some(VideoProducer::Capture(c, ..)) => c.is_running(),
            Some(VideoProducer::Encoded(c, ..)) => c.is_running(),
            Some(VideoProducer::Test(_)) => true,
            Some(VideoProducer::File(t)) => !t.is_finished(),
            None => false,
        }
    }

    pub fn audio_running(&self) -> bool {
        match &self.audio {
            Some(AudioProducer::Capture(c)) => c.is_running(),
            Some(AudioProducer::Sine(_)) => true,
            None => false,
        }
    }

    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    /// An audio-only share of one app whose process has ended (nothing left to hear). The
    /// capture itself would go on, sending silence.
    pub fn source_gone(&self) -> bool {
        match &self.source {
            ShareSource::AudioApp { pid } => !process_alive(*pid),
            _ => false,
        }
    }

    /// Test hook (vox-cli `bwcap`): cap the encoder target as if the congestion controller asked
    /// for at most `bps` (`None` lifts it). Exercises the degradation policy end to end.
    pub fn set_bitrate_cap(&self, bps: Option<u64>) {
        self.bitrate_cap.cap_bps.store(bps.unwrap_or(0), Ordering::Relaxed);
    }

    /// Test hook (vox-cli `overshoot`): make the hardware encoder produce `pct`% of the target it
    /// should (`None` = 100%), like an encoder that ignores WebRTC's rate requests. Exercises the
    /// WebRTC drop guard and recovery end to end.
    pub fn set_encoder_overshoot(&self, pct: Option<u64>) {
        self.bitrate_cap.overshoot_pct.store(pct.unwrap_or(0), Ordering::Relaxed);
    }

    /// Stats extras for the pre-encoded path.
    pub fn encoder_report(&self) -> Option<EncoderReport> {
        let (backend, feedback) = self.encoder.as_ref()?;
        let hw = match &self.video {
            Some(VideoProducer::Encoded(c, ..)) => Some(c.stats()),
            _ => None,
        };
        Some(EncoderReport { backend: backend.clone(), feedback: feedback.clone(), hw })
    }

    /// The encoder in use (`None` for audio-only shares and the test sources).
    pub fn attempt(&self) -> Option<&encoders::Attempt> {
        self.attempt.as_ref()
    }

    /// What the encoder safety net watches: the hardware encoder's id, the published track and
    /// its feedback counters. `None` on the processor (nothing left to switch to) and the test
    /// sources.
    pub fn encoder_probe(&self) -> Option<(String, LocalVideoTrack, Arc<EncodedFeedback>)> {
        let a = self.attempt.as_ref().filter(|a| !a.is_software())?;
        let (_, fb) = self.encoder.as_ref()?;
        Some((a.id.clone(), self.video_track.clone()?, fb.clone()))
    }

    /// Change the preview of our own share (hidden -> off, focused -> bigger and faster). A
    /// no-op without a preview or on sources that have no capture (test pattern, file).
    pub fn set_preview(&self, cfg: vox_capture::PreviewConfig) {
        let Some(p) = &self.preview else { return };
        p.set_config(cfg);
        if let Some(t) = self.video.as_ref().and_then(|v| v.preview_tap()) {
            t.apply(cfg);
        }
    }

    /// The preview tap's owner, for the hover picture's snapshots (v1.1). `None` without a
    /// preview (audio-only, test pattern, file).
    pub fn preview_handle(&self) -> Option<SharePreview> {
        self.preview.clone()
    }

    /// Frames of our own preview shown so far (stats), when there is one.
    pub fn preview_counters(&self) -> Option<Arc<StreamCounters>> {
        self.preview.as_ref().map(|p| p.counters())
    }

    /// Switch the shared window / monitor without ending the live: the same video publication
    /// (and, if it exists, audio publication) keeps going, quality settings unchanged.
    ///
    /// The new capture starts in standby next to the running one, so a failure (window closed,
    /// unsupported) leaves the old share untouched. Once it delivered a frame, the old capture is
    /// retired and the new one takes over at a keyframe; only then is the old one torn down.
    /// Errors are Portuguese, ready to show.
    pub async fn switch_source(&mut self, room: &Room, req: SwitchRequest) -> Result<SwitchReport> {
        let t0 = Instant::now();
        let from = self.source.clone();
        if from.is_audio_only() || req.source.is_audio_only() {
            return self.switch_audio_source(room, req, t0).await;
        }
        let capturable = |s: &ShareSource| matches!(s, ShareSource::Window(_) | ShareSource::Monitor(_));
        if !capturable(&req.source) || !capturable(&from) {
            bail!("Só dá para trocar entre janelas e telas.");
        }
        let path = self.path.ok_or_else(|| anyhow!("A transmissão não está ativa."))?;
        let video_source = self
            .publication
            .as_ref()
            .and_then(|p| p.video(path))
            .map(|v| v.source.clone())
            .ok_or_else(|| anyhow!("A transmissão não está ativa."))?;
        let audio_before = self.audio.is_some();

        // 1. New capture in standby; the old one keeps streaming meanwhile.
        let started = match path {
            // The same encoder (backend and GPU) as the running capture.
            VideoPath::Encoded => match self.attempt.as_ref().and_then(|a| a.backend.map(|b| (b, a.luid))) {
                Some(hw) => {
                    let feedback = self.encoder.as_ref().map(|(_, f)| f.clone()).unwrap_or_default();
                    // Same track, so the new sink watches the same publish-timing events.
                    let timing: TimingSlot = Arc::new(Mutex::new(self.publication.as_ref().and_then(|p| p.video(path)).map(|v| v.track.publish_timing_events())));
                    start_encoded(&req.source, &self.opts, Some(hw), self.max_bitrate, video_source, self.video_counters.clone(), feedback, self.bitrate_cap.clone(), true, timing, self.preview.as_ref())
                        .map(|(p, name)| (p, Some(name)))
                }
                None => bail!("Esta janela ou tela não pode usar o codificador da transmissão atual."),
            },
            VideoPath::Raw => start_raw(&req.source, &self.opts, self.opts.width, self.opts.height, video_source, self.video_counters.clone(), true, self.preview.as_ref()).map(|p| (p, None)),
        };
        let (new_video, new_name) = started.map_err(|e| anyhow!(capture_failure_pt(&e)))?;

        // 2. Give it a moment to prove it produces frames.
        let deadline = Instant::now() + SWITCH_READY_TIMEOUT;
        while new_video.running() && !new_video.seen_frame() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let old_alive = self.video_running();
        match switch_decision(new_video.running(), new_video.seen_frame(), old_alive) {
            SwitchDecision::Commit | SwitchDecision::CommitSilent => {}
            SwitchDecision::KeepOld => {
                warn!(from = ?from, to = ?req.source, "source switch given up: new capture silent or stopped; keeping the old one");
                drop(new_video);
                bail!("A janela ou tela escolhida não enviou imagem (está minimizada ou protegida?). A transmissão continua como estava.");
            }
            SwitchDecision::Fail => {
                drop(new_video);
                bail!("Não foi possível capturar a janela ou tela escolhida.");
            }
        }

        // 3. Hand over: retire the old gate, activate the new one (forces a keyframe).
        let old = self.video.take().expect("video producer present while sharing");
        if let (VideoProducer::Encoded(_, o, _), VideoProducer::Encoded(_, n, _)) = (&old, &new_video) {
            // The new encoder starts at the configured bitrate; give it WebRTC's current target.
            let bps = o.lock().sink.webrtc_bps;
            n.lock().sink.webrtc_bps = bps;
        }
        old.retire();
        let mut mode = new_video.activate();
        // The preview follows the stream: the old capture goes quiet, the new one shows.
        if let Some(t) = old.preview_tap() {
            t.deactivate();
        }
        if let Some(t) = new_video.preview_tap() {
            t.activate();
        }
        self.video = Some(new_video);
        self.source = req.source.clone();
        if let Some(name) = new_name {
            if let Some(e) = &mut self.encoder {
                e.0 = name;
            }
        }
        let live_deadline = Instant::now() + SWITCH_LIVE_TIMEOUT;
        while mode != FrameMode::Live && Instant::now() < live_deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
            mode = self.video.as_ref().and_then(|v| v.gate_mode()).unwrap_or(FrameMode::Live);
        }
        if mode != FrameMode::Live {
            warn!("source switch: no keyframe from the new capture yet");
        }
        let (_, old_last) = old.pass_times();
        let new_first = self.video.as_ref().and_then(|v| v.pass_times().0);
        let gap = match (old_last, new_first) {
            (Some(a), Some(b)) => b.saturating_duration_since(a),
            (Some(a), None) => a.elapsed(),
            _ => Duration::ZERO,
        };

        // 4. Audio follows the new source (mute/unmute of the one audio publication). The old
        // app's sound is cut first so viewers never hear it over the new picture.
        self.opts.audio_pid = req.audio_pid;
        let plan = plan_audio(audio_before, req.audio, self.publication.as_ref().is_some_and(|p| p.audio.is_some()));
        let mut audio_error = None;
        match plan {
            AudioPlan::Nothing => {}
            AudioPlan::Stop => self.stop_audio(),
            AudioPlan::Start { .. } => {
                self.audio.take();
                if let Err(e) = self.start_audio(room).await {
                    warn!(error = %format!("{e:#}"), "screenshare audio unavailable after the switch");
                    self.stop_audio();
                    audio_error = Some(match e.downcast_ref::<CaptureFailure>() {
                        Some(_) => format!("Áudio indisponível: {}", capture_failure_pt(&e)),
                        None => "Áudio indisponível: não foi possível capturar o som do novo app.".to_string(),
                    });
                }
            }
        }
        self.opts.audio = self.audio.is_some();
        // Tearing down the old encoder + capture takes up to a couple of seconds; nothing waits
        // for it (its gate already drops every frame).
        std::thread::Builder::new()
            .name("vox-switch-teardown".into())
            .spawn(move || drop(old))
            .ok();

        let report = SwitchReport {
            from,
            to: req.source,
            path: Some(path),
            gap,
            total: t0.elapsed(),
            audio_before,
            audio_after: self.audio.is_some(),
            audio_error,
        };
        info!(
            from = ?report.from,
            to = ?report.to,
            path = ?report.path,
            gap_ms = report.gap.as_millis() as u64,
            total_ms = report.total.as_millis() as u64,
            audio_before,
            audio_after = report.audio_after,
            "screenshare source switched (same publication, no republish)"
        );
        Ok(report)
    }

    /// "Trocar" on an audio-only share: capture another app's (or the whole PC's) sound without
    /// ending the live. The new capture starts next to the old one and only replaces it once it
    /// is running, so a closed app leaves the old sound going. Going between audio-only and a
    /// screen share is not a switch: the app restarts the Go Live for that.
    async fn switch_audio_source(&mut self, room: &Room, req: SwitchRequest, t0: Instant) -> Result<SwitchReport> {
        let from = self.source.clone();
        if !from.is_audio_only() || !req.source.is_audio_only() {
            bail!("Para trocar entre só áudio e tela, pare e comece a transmissão de novo.");
        }
        if self.publication.is_none() {
            bail!("A transmissão não está ativa.");
        }
        let (producer, counters, track) = self.make_audio_producer(room, &req.source).await.map_err(|e| anyhow!(audio_failure_pt(&e)))?;
        // Replaced: the old capture stops here (its callback is gone before the call returns).
        self.audio = Some(producer);
        self.audio_counters = Some(counters);
        self.audio_track = Some(track);
        self.source = req.source.clone();
        let report = SwitchReport {
            from,
            to: req.source,
            path: None,
            gap: Duration::ZERO,
            total: t0.elapsed(),
            audio_before: true,
            audio_after: true,
            audio_error: None,
        };
        info!(from = ?report.from, to = ?report.to, total_ms = report.total.as_millis() as u64, "audio-only screenshare source switched (same publication, no republish)");
        Ok(report)
    }

    /// Stop the audio capture and mute the audio publication (it stays published for reuse).
    fn stop_audio(&mut self) {
        self.audio.take();
        self.audio_track = None;
        self.audio_counters = None;
        if let Some(p) = self.publication.as_ref() {
            for a in p.audio_pubs() {
                a.publication.mute();
            }
        }
    }

    /// Stop producers and mute the publications; returns them for the next share.
    pub fn stop(mut self) -> SharePublication {
        // Dropping stops capture threads / joins producer threads.
        self.video.take();
        self.audio.take();
        let publication = self.publication.take().expect("publication present while sharing");
        publication.mute();
        info!(source = ?self.source, path = ?self.path, "screenshare stopped (tracks muted for reuse)");
        publication
    }
}

/// Is process `pid` still running? (`true` when that cannot be told.)
fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        const STILL_ACTIVE: u32 = 259;
        // SAFETY: plain Win32 queries on a handle we open and close here.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                // Gone (no such process), or not ours to ask about: only "gone" ends the share.
                return std::io::Error::last_os_error().raw_os_error() != Some(87); // ERROR_INVALID_PARAMETER
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code);
            CloseHandle(h);
            ok == 0 || code == STILL_ACTIVE
        }
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        true
    }
}

fn initial_resolution(source: &ShareSource, opts: &ShareOptions) -> (u32, u32) {
    match source {
        ShareSource::TestPattern => (opts.width, opts.height),
        ShareSource::Monitor(i) => capture_call("list_monitors", vox_capture::list_monitors)
            .ok()
            .and_then(|ms| ms.into_iter().find(|m| m.index == *i))
            .map(|m| fit(m.width, m.height, opts.width, opts.height))
            .unwrap_or((opts.width, opts.height)),
        // Unknown until the first frame; WebRTC adapts to the real size.
        ShareSource::Window(_) => (opts.width, opts.height),
        ShareSource::H264File(f) => std::fs::read(&f.path)
            .ok()
            .and_then(|d| h264::access_units(&d).first().map(|au| (au.width, au.height)))
            .unwrap_or((opts.width, opts.height)),
        // No video at all (never asked: audio-only shares skip the video path).
        ShareSource::AudioApp { .. } | ShareSource::AudioSystem | ShareSource::AudioTone => (opts.width, opts.height),
    }
}

fn fit(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let s = (max_w as f64 / w as f64).min(max_h as f64 / h as f64).min(1.0);
    (((w as f64 * s) as u32) & !1, ((h as f64 * s) as u32) & !1)
}

/// Whether the encoded path is possible at all: `Some(None)` = the file source, `Some(Some(_))`
/// = a capture (the plan says which encoders), `None` = raw path only.
fn encoded_backend(source: &ShareSource, choice: EncoderChoice) -> Option<Option<EncoderBackend>> {
    match source {
        // The file is the encoder.
        ShareSource::H264File(_) => Some(None),
        ShareSource::TestPattern | ShareSource::AudioApp { .. } | ShareSource::AudioSystem | ShareSource::AudioTone => None,
        ShareSource::Window(_) | ShareSource::Monitor(_) => {
            // Which hardware encoders (if any) is up to the plan (`encoders::plan`).
            match choice {
                EncoderChoice::Software => None,
                EncoderChoice::Auto => Some(Some(EncoderBackend::Auto)),
                EncoderChoice::Nvenc => Some(Some(EncoderBackend::Nvenc)),
                EncoderChoice::MediaFoundation => Some(Some(EncoderBackend::MediaFoundation)),
            }
        }
    }
}

fn raw_reason(source: &ShareSource, choice: EncoderChoice) -> &'static str {
    match (source, choice) {
        (ShareSource::TestPattern, _) => "test pattern is a raw source",
        (_, EncoderChoice::Software) => "software encoder requested",
        _ => "no usable hardware encoder (or hardware acceleration off)",
    }
}

/// Start the pre-encoded producer: the hardware encoder for window/monitor targets, or the file
/// replay thread. Returns it with a backend name for logs/stats.
#[allow(clippy::too_many_arguments)]
fn start_encoded(
    source: &ShareSource,
    opts: &ShareOptions,
    hw: Option<(EncoderBackend, Option<u64>)>,
    max_bitrate: u64,
    video_source: NativeVideoSource,
    counters: Arc<StreamCounters>,
    feedback: Arc<EncodedFeedback>,
    bitrate_cap: Arc<TestHooks>,
    standby: bool,
    timing: TimingSlot,
    preview: Option<&SharePreview>,
) -> Result<(VideoProducer, String)> {
    let control = Arc::new(OnceLock::new());
    let mut sink = EncodedSink::new(video_source, counters, feedback, control.clone(), max_bitrate);
    sink.test = bitrate_cap;
    sink.timing = timing;
    match source {
        ShareSource::H264File(f) => {
            let t = spawn_h264_file(f, sink)?;
            Ok((VideoProducer::File(t), "h264file".to_string()))
        }
        ShareSource::Window(_) | ShareSource::Monitor(_) => {
            let target = match source {
                ShareSource::Window(h) => VideoTarget::Window(*h),
                ShareSource::Monitor(m) => VideoTarget::Monitor(*m),
                _ => unreachable!(),
            };
            let cfg = vox_capture::EncodeConfig {
                video: VideoConfig { max_fps: opts.fps, max_width: opts.width, max_height: opts.height, capture_cursor: opts.capture_cursor },
                bitrate_bps: max_bitrate.min(u32::MAX as u64) as u32,
                backend: hw.map_or(EncoderBackend::Auto, |(b, _)| b),
                degradation: opts.degradation,
                adapter_luid: hw.and_then(|(_, l)| l),
            };
            let stage: EncHandle = Arc::new(Mutex::new(EncStage { stage: Stage::new(standby), sink }));
            let cb = stage.clone();
            let tap = preview.and_then(|p| p.attach());
            let pv = tap.as_ref().map(|t| (t.sp.clone(), t.callback()));
            let cap = capture_call("start_video_encoded", move || {
                let (sp, pcb) = match pv {
                    Some((sp, cb)) => (Some(sp), Some(cb)),
                    None => (None, None),
                };
                vox_capture::start_video_encoded_with_preview(target, cfg, Box::new(move |f| cb.lock().on_frame(f)), sp.as_ref().zip(pcb))
            })?;
            let _ = control.set(cap.control());
            let name = match cap.backend() {
                EncoderBackend::Nvenc => "nvenc",
                EncoderBackend::MediaFoundation => "mf",
                EncoderBackend::Auto => "auto",
                #[cfg(target_os = "macos")]
                EncoderBackend::VideoToolbox => "videotoolbox",
            };
            info!(?target, backend = name, max_fps = opts.fps, max_w = opts.width, max_h = opts.height, start_kbps = max_bitrate / 1000, degradation = ?opts.degradation, standby, "hardware-encoded capture started");
            Ok((VideoProducer::Encoded(cap, stage, tap), name.to_string()))
        }
        ShareSource::TestPattern => bail!("the test pattern has no encoded path"),
        ShareSource::AudioApp { .. } | ShareSource::AudioSystem | ShareSource::AudioTone => bail!("an audio-only share has no video"),
    }
}

/// Where the share start hands the publication's publish-timing event stream to the sink (the
/// sink exists before the track is published).
pub(crate) type TimingSlot = Arc<Mutex<Option<PublishTimingEventStream>>>;

/// Finds access units that WebRTC dropped after `capture_encoded_frame` accepted them.
///
/// WebRTC's `VideoStreamEncoder` treats our access units like raw frames: its frame dropper
/// (a leaky bucket at the congestion-control target, active because the pass-through encoder
/// does not claim a trusted rate controller), its encoder-queue overload check, a paused encoder
/// (target 0) and the duplicate-timestamp check can each discard a frame before the pass-through
/// sees it. A dropped P frame is never packetized, so RTP sequence numbers stay contiguous: no
/// loss, no NACK, no PLI, and every viewer decodes the following frames against the wrong
/// reference (smeared blocks until the next IDR).
///
/// Every pushed frame carries a `frame_id`; the sender transformer reports the id of each frame
/// that reached the packetizer (`PublishTimingStage::EncoderOutput`). The encoder queue is FIFO,
/// so an id that is reported while frames pushed before it are still unreported means those were
/// dropped. Ids come from one process-wide counter, so the sinks of a source switch (old capture,
/// then new capture on the same track) never reuse each other's ids; they are not contiguous per
/// track (a webcam and a screen share push at the same time), which is why the tracker keeps the
/// ids it pushed instead of looking for numeric gaps.
#[derive(Debug, Default)]
struct DropTracker {
    /// Pushed frames WebRTC has not reported yet, oldest first: (id, keyframe).
    pending: std::collections::VecDeque<(u32, bool)>,
    /// A report arrived already. Frames pushed before the first one (before the track was
    /// published, or before its events were hooked up) are not drops.
    started: bool,
    /// The reference chain is broken (a drop was seen and no keyframe was pushed since).
    broken: bool,
}

/// Unreported frames kept at most (WebRTC reports within a frame or two; this only bounds a
/// stream whose events never arrive).
const DROP_TRACKER_MAX_PENDING: usize = 512;

/// What one "WebRTC encoded frame N" event revealed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct DropReport {
    /// Frames dropped right before N.
    dropped: u32,
    /// Delta frames already pushed from N on that reference a dropped frame (viewers decode them
    /// against the wrong picture).
    broken: u32,
}

static NEXT_FRAME_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

impl DropTracker {
    /// Id for the frame about to be pushed (never 0: the SDK maps 0 to "no id"); returns how many
    /// pushed delta frames so far follow a drop without a keyframe in between (0 or 1).
    fn push(&mut self, keyframe: bool) -> (u32, u32) {
        let id = loop {
            let id = NEXT_FRAME_ID.fetch_add(1, Ordering::Relaxed);
            if id != 0 {
                break id;
            }
        };
        if self.pending.len() >= DROP_TRACKER_MAX_PENDING {
            self.pending.pop_front();
        }
        self.pending.push_back((id, keyframe));
        let broken = if keyframe {
            self.broken = false;
            0
        } else {
            self.broken as u32
        };
        (id, broken)
    }

    /// WebRTC encoded frame `id`.
    fn encoded(&mut self, id: u32) -> DropReport {
        let mut dropped = 0;
        while self.pending.front().is_some_and(|&(p, _)| p < id) {
            self.pending.pop_front();
            dropped += self.started as u32;
        }
        self.started = true;
        // `id` itself (if it is ours: another sink on the same track reports its own ids).
        let ours = self.pending.front().filter(|&&(p, _)| p == id).map(|&(_, k)| k);
        if ours.is_some() {
            self.pending.pop_front();
        }
        if dropped == 0 {
            return DropReport::default();
        }
        // Everything of ours that went out after the hole decodes against a wrong reference,
        // up to the first keyframe.
        let after = ours.into_iter().chain(self.pending.iter().map(|&(_, k)| k));
        let mut broken = 0;
        let mut repaired = false;
        for k in after {
            if k {
                repaired = true;
                break;
            }
            broken += 1;
        }
        if !repaired {
            self.broken = true;
        }
        DropReport { dropped, broken }
    }
}

/// Floor for the guarded encoder target: the guard never pushes the encoder below this (or below
/// the unguarded target, if that is lower).
const MIN_GUARDED_BPS: u64 = 100_000;

/// Keeps our encoder inside the budget of WebRTC's frame dropper, so it never has a reason to
/// drop one of our (already encoded) frames.
///
/// WebRTC's `FrameDropper` is a leaky bucket: every encoded frame fills it, it drains at the
/// congestion-control target, and once it holds more than half a second of the target it starts
/// dropping frames. For a real encoder that is harmless (the dropped frame was never encoded);
/// for us it breaks the reference chain. Hardware encoders mostly follow the target, but not
/// always: IDRs (2-4x a P frame, more at low rates), encoders that ignore or lag a rate change
/// (the NVIDIA MFT never goes above its start rate, AMD/Intel MFTs are unverified), the input-rate
/// compensation (up to 3x the per-frame budget while the frame rate estimate catches up), the
/// encoder floor, and congestion cutting the target faster than the encoder reacts.
///
/// The guard mirrors that bucket with our frame sizes and WebRTC's target, and scales the target
/// handed to the encoder down while the bucket is filling (integral control: a persistent
/// overshoot is cancelled, not just damped), then lets it recover once the debt is paid.
#[derive(Debug)]
struct BudgetGuard {
    /// Bits sent beyond what the target allowed so far (WebRTC's accumulator).
    debt_bits: f64,
    last: Option<Instant>,
    /// Multiplier for the encoder target (1 = no correction).
    factor: f64,
}

impl Default for BudgetGuard {
    fn default() -> Self {
        Self { debt_bits: 0.0, last: None, factor: 1.0 }
    }
}

/// WebRTC's frame dropper drops once its bucket holds this much of the target (seconds).
const DROPPER_BUCKET_S: f64 = 0.5;
/// ... and caps its accumulator at this much.
const DROPPER_CAP_S: f64 = 3.0;
/// Start correcting at this fill of the bucket, recover below `GUARD_RELAX`.
const GUARD_START: f64 = 0.3;
const GUARD_RELAX: f64 = 0.15;
const GUARD_MIN_FACTOR: f64 = 0.1;

impl BudgetGuard {
    /// Account one frame pushed at `now` against WebRTC's target (`0` = unknown: no guard).
    fn on_frame(&mut self, bytes: usize, now: Instant, target_bps: u64) {
        let dt = self.last.map(|l| now.duration_since(l).as_secs_f64()).unwrap_or(0.0);
        self.last = Some(now);
        if target_bps == 0 {
            self.debt_bits = 0.0;
            return;
        }
        let target = target_bps as f64;
        // WebRTC drains the bucket before it looks at it, then adds the new frame.
        self.debt_bits = (self.debt_bits - target * dt).max(0.0);
        let fill = self.debt_bits / (target * DROPPER_BUCKET_S);
        if fill > GUARD_START {
            // Proportional to how far past the threshold we are, per frame (integral overall).
            self.factor *= 1.0 - 0.5 * (fill - GUARD_START).min(0.7);
        } else if fill < GUARD_RELAX {
            self.factor *= 1.02;
        }
        self.factor = self.factor.clamp(GUARD_MIN_FACTOR, 1.0);
        self.debt_bits = (self.debt_bits + bytes as f64 * 8.0).min(target * DROPPER_CAP_S);
    }

    /// WebRTC dropped frames anyway: cut the target at once.
    fn on_drop(&mut self) {
        self.factor = (self.factor * 0.7).max(GUARD_MIN_FACTOR);
    }

    fn factor(&self) -> f64 {
        self.factor
    }
}

/// Test knob: `VOX_DROP_RECOVERY=0` keeps counting WebRTC drops but turns off the budget guard
/// and the recovery IDR (reproduces v0.2.1 for before/after measurements).
fn drop_recovery_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VOX_DROP_RECOVERY").map(|v| v != "0").unwrap_or(true))
}

/// Test knob: `VOX_IDR_GATE=0` sends the delta frames after a drop anyway (v0.6 behaviour, for
/// before/after measurements of the [`IdrGate`]); the recovery IDR and the budget guard stay on.
fn idr_gate_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VOX_IDR_GATE").map(|v| v != "0").unwrap_or(true))
}

/// Pushes access units into the pre-encoded source and, right after each one, forwards WebRTC's
/// feedback to the encoder. The pass-through encoder only raises keyframe / rate requests while
/// encoding a frame we pushed, so polling on frame arrival loses nothing and needs no thread.
pub(crate) struct EncodedSink {
    source: NativeVideoSource,
    counters: Arc<StreamCounters>,
    feedback: Arc<EncodedFeedback>,
    /// Set once `start_video_encoded` returned; stays empty for the file source.
    control: Arc<OnceLock<EncoderControl>>,
    file: bool,
    max_bitrate: u64,
    last_forwarded_bps: u64,
    /// Last non-zero WebRTC target (0 = none yet: the encoder keeps its initial `max_bitrate`).
    webrtc_bps: u64,
    /// Test hooks, see [`ActiveShare::set_bitrate_cap`].
    pub(crate) test: Arc<TestHooks>,
    /// Keeps the encoder inside WebRTC's frame-dropper budget.
    guard: BudgetGuard,
    pending_keyframe: bool,
    pub(crate) timing: TimingSlot,
    drops: DropTracker,
    /// After a drop (or a frame the source refused) only the next IDR goes out: the P-frames
    /// still in the encoder's pipeline reference the lost picture (see [`IdrGate`]).
    idr_gate: IdrGate,
    /// A drop was reported (logged once per share at info, then at debug).
    drop_logged: bool,
}

impl EncodedSink {
    pub(crate) fn new(
        source: NativeVideoSource,
        counters: Arc<StreamCounters>,
        feedback: Arc<EncodedFeedback>,
        control: Arc<OnceLock<EncoderControl>>,
        max_bitrate: u64,
    ) -> Self {
        Self {
            source,
            counters,
            feedback,
            control,
            file: false,
            max_bitrate,
            last_forwarded_bps: 0,
            webrtc_bps: 0,
            test: Arc::new(TestHooks::default()),
            guard: BudgetGuard::default(),
            pending_keyframe: false,
            timing: Arc::new(Mutex::new(None)),
            drops: DropTracker::default(),
            idr_gate: IdrGate::default(),
            drop_logged: false,
        }
    }

    fn push(&mut self, payload: &[u8], keyframe: bool, width: u32, height: u32, timestamp_us: i64) {
        if payload.is_empty() || width == 0 || height == 0 {
            self.counters.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // Frames WebRTC finished since the last push (and drops among them).
        self.poll_encoded();
        if idr_gate_enabled() && !self.idr_gate.pass(keyframe) {
            // Would decode against a picture viewers never got: hold it, the IDR is on its way.
            self.feedback.withheld_frames.fetch_add(1, Ordering::Relaxed);
            if self.idr_gate.withheld.is_multiple_of(30) {
                self.request_keyframe(); // still no IDR after a second of frames: ask again
            }
            self.poll_feedback();
            return;
        }
        let (frame_id, broken) = self.drops.push(keyframe);
        if broken > 0 {
            self.feedback.broken_frames.fetch_add(broken as u64, Ordering::Relaxed);
        }
        let frame = EncodedVideoFrame {
            codec: EncodedVideoCodec::H264,
            payload,
            timestamp_us,
            frame_type: if keyframe { EncodedFrameType::Key } else { EncodedFrameType::Delta },
            resolution: VideoResolution { width, height },
            frame_metadata: Some(FrameMetadata { user_timestamp: None, frame_id: Some(frame_id), user_data: None }),
        };
        if !self.source.capture_encoded_frame(&frame) {
            self.counters.errors.fetch_add(1, Ordering::Relaxed);
            self.break_chain();
        }
        self.guard.on_frame(payload.len(), Instant::now(), self.webrtc_bps);
        self.counters.frame(width, height);
        if keyframe {
            self.feedback.keyframes_sent.fetch_add(1, Ordering::Relaxed);
        }
        self.poll_feedback();
    }

    /// Drain the publish-timing events: count what WebRTC encoded and react to drops.
    fn poll_encoded(&mut self) {
        let mut dropped = 0u64;
        let mut broken = 0u64;
        let mut encoded = 0u64;
        {
            let mut slot = self.timing.lock();
            let Some(stream) = slot.as_mut() else { return };
            while let Some(Some(ev)) = stream.next().now_or_never() {
                if ev.stage != PublishTimingStage::EncoderOutput {
                    continue;
                }
                encoded += 1;
                if let Some(id) = ev.frame_id {
                    let r = self.drops.encoded(id);
                    dropped += r.dropped as u64;
                    broken += r.broken as u64;
                }
            }
        }
        self.feedback.webrtc_encoded.fetch_add(encoded, Ordering::Relaxed);
        self.feedback.broken_frames.fetch_add(broken, Ordering::Relaxed);
        if dropped == 0 {
            return;
        }
        self.feedback.webrtc_dropped.fetch_add(dropped, Ordering::Relaxed);
        self.feedback.drop_events.fetch_add(1, Ordering::Relaxed);
        let recover = drop_recovery_enabled() && !self.file;
        if !self.drop_logged {
            info!(dropped, recover, webrtc_kbps = self.webrtc_bps / 1000, "WebRTC dropped pre-encoded frames (reference chain broken)");
            self.drop_logged = true;
        } else {
            debug!(dropped, recover, "WebRTC dropped pre-encoded frames");
        }
        if recover {
            self.feedback.recovery_idrs.fetch_add(1, Ordering::Relaxed);
            self.request_keyframe();
            // Whatever made WebRTC drop (overshoot, IDR debt), the IDR just requested adds to it.
            self.guard.on_drop();
            // No keyframe went out since the hole: hold the deltas still coming (async encoders
            // have a few in flight) until the requested IDR.
            if self.drops.broken {
                self.idr_gate.arm();
            }
        }
    }

    /// A frame was lost before WebRTC took it: hold deltas until an IDR and ask for one.
    fn break_chain(&mut self) {
        if drop_recovery_enabled() && !self.file && self.idr_gate.arm() {
            self.request_keyframe();
        }
    }

    /// Ask the encoder for an IDR (WebRTC's PLI/FIR, a WebRTC drop, a source switch). Requests
    /// before the encoder control exists are kept and sent with the next frame; the encoder side
    /// folds requests closer than 250 ms into one IDR.
    fn request_keyframe(&mut self) {
        match self.control.get() {
            Some(c) => c.request_keyframe(),
            None => self.pending_keyframe = true,
        }
    }

    fn poll_feedback(&mut self) {
        if self.source.take_keyframe_request() {
            self.feedback.keyframe_requests.fetch_add(1, Ordering::Relaxed);
            self.pending_keyframe = true;
        }
        if self.pending_keyframe {
            if let Some(c) = self.control.get() {
                c.request_keyframe();
                self.pending_keyframe = false;
            } else if self.file {
                // A file can't make an IDR on demand; viewers wait for the next one in the file.
                self.feedback.keyframe_requests_ignored.fetch_add(1, Ordering::Relaxed);
                debug!("keyframe requested; file source can't honour it (next IDR comes from the file)");
                self.pending_keyframe = false;
            }
        }
        if let Some(rc) = self.source.take_rate_control_request() {
            // 0 = encoder paused (no network / no subscribers): keep the last target.
            if rc.target_bitrate_bps != 0 {
                self.feedback.observe_target(rc.target_bitrate_bps);
                self.webrtc_bps = rc.target_bitrate_bps;
            }
        }
        let cap = self.test.cap_bps.load(Ordering::Relaxed);
        let Some(base) = encoder_target(self.webrtc_bps, cap, self.max_bitrate) else { return };
        let guard = if drop_recovery_enabled() { self.guard.factor() } else { 1.0 };
        let mut target = ((base as f64 * guard) as u64).max(MIN_GUARDED_BPS.min(base));
        let overshoot = self.test.overshoot_pct.load(Ordering::Relaxed);
        if overshoot > 0 {
            target = target * overshoot / 100;
        }
        self.feedback.guard_pct.store((guard * 100.0).round() as u64, Ordering::Relaxed);
        self.feedback.guard_min_pct.fetch_min((guard * 100.0).round() as u64, Ordering::Relaxed);
        if let Some(c) = self.control.get() {
            if rate_change_worth_forwarding(self.last_forwarded_bps, target) {
                c.set_bitrate(target.min(u32::MAX as u64) as u32);
                debug!(target_kbps = target / 1000, webrtc_kbps = self.webrtc_bps / 1000, guard = format!("{guard:.2}"), test_cap_kbps = cap / 1000, "encoder bitrate updated");
                self.last_forwarded_bps = target;
                self.feedback.rate_changes.fetch_add(1, Ordering::Relaxed);
                self.feedback.target_bps.store(target, Ordering::Relaxed);
            }
        } else {
            self.feedback.target_bps.store(target, Ordering::Relaxed);
        }
    }
}

/// Encoder target from WebRTC's congestion-control target (`0` = none yet) and the test cap
/// (`0` = none), within [MIN_ENCODER_BPS, max]. `None` while there is nothing to apply.
fn encoder_target(webrtc_bps: u64, test_cap: u64, max_bitrate: u64) -> Option<u64> {
    let max = max_bitrate.max(MIN_ENCODER_BPS);
    let t = match (webrtc_bps, test_cap) {
        (0, 0) => return None,
        (0, c) => c.min(max),
        (w, 0) => w,
        (w, c) => w.min(c),
    };
    Some(t.clamp(MIN_ENCODER_BPS, max))
}

/// Reconfiguring a hardware encoder per tiny congestion-control wiggle is pointless; forward
/// changes of 5% or more (and always the first one).
fn rate_change_worth_forwarding(last: u64, new: u64) -> bool {
    last == 0 || new.abs_diff(last) * 20 >= last
}

/// Replay an Annex-B file as pre-encoded frames at a fixed rate.
fn spawn_h264_file(src: &H264FileSource, mut sink: EncodedSink) -> Result<ProducerThread> {
    let data = std::fs::read(&src.path).with_context(|| format!("read {}", src.path.display()))?;
    let aus = h264::access_units(&data);
    if aus.is_empty() {
        bail!("{}: no H.264 access units with an SPS found (Annex-B expected)", src.path.display());
    }
    let keyframes = aus.iter().filter(|a| a.keyframe).count();
    if !aus[0].keyframe {
        warn!("h264 file does not start with an IDR; viewers see nothing until the first one");
    }
    let fps = src.fps.clamp(1, 240);
    let looped = src.looped;
    info!(
        path = %src.path.display(),
        access_units = aus.len(),
        keyframes,
        first = format!("{}x{}", aus[0].width, aus[0].height),
        fps,
        looped,
        mb = format!("{:.1}", data.len() as f64 / 1048576.0),
        "h264 file source loaded"
    );
    sink.file = true;
    Ok(ProducerThread::spawn("vox-h264-file", move |stop| {
        let t0 = Instant::now();
        let mut i = 0usize;
        let done = AtomicBool::new(false);
        let stop_or_done = StopOrDone { stop: &stop, done: &done };
        paced_until(&stop_or_done, Duration::from_secs_f64(1.0 / fps as f64), || {
            let au = &aus[i];
            let ts = t0.elapsed().as_micros() as i64 + 1;
            sink.push(&data[au.range.clone()], au.keyframe, au.width, au.height, ts);
            i += 1;
            if i == aus.len() {
                if looped {
                    i = 0;
                } else {
                    info!("h264 file source reached the end");
                    done.store(true, Ordering::Relaxed);
                }
            }
        });
    }))
}

struct StopOrDone<'a> {
    stop: &'a AtomicBool,
    done: &'a AtomicBool,
}

impl StopOrDone<'_> {
    fn load(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || self.done.load(Ordering::Relaxed)
    }
}

fn paced_until(stop: &StopOrDone<'_>, period: Duration, mut tick: impl FnMut()) {
    let max_debt = period.max(Duration::from_millis(200));
    let mut next = Instant::now();
    while !stop.load() {
        tick();
        next += period;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else if now - next > max_debt {
            next = now;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn start_raw(
    source: &ShareSource,
    opts: &ShareOptions,
    width: u32,
    height: u32,
    video_source: NativeVideoSource,
    counters: Arc<StreamCounters>,
    standby: bool,
    preview: Option<&SharePreview>,
) -> Result<VideoProducer> {
    match source {
        ShareSource::TestPattern => Ok(VideoProducer::Test(spawn_test_pattern(width, height, opts.fps, video_source, counters))),
        ShareSource::Window(_) | ShareSource::Monitor(_) => {
            let target = match source {
                ShareSource::Window(h) => VideoTarget::Window(*h),
                ShareSource::Monitor(m) => VideoTarget::Monitor(*m),
                _ => unreachable!(),
            };
            let cfg = VideoConfig { max_fps: opts.fps, max_width: opts.width, max_height: opts.height, capture_cursor: opts.capture_cursor };
            let stage: RawHandle = Arc::new(Mutex::new(RawStage { stage: Stage::new(standby), source: video_source, counters }));
            let cb = stage.clone();
            let tap = preview.and_then(|p| p.attach());
            let pv = tap.as_ref().map(|t| (t.sp.clone(), t.callback()));
            let cap = capture_call("start_video", move || {
                let (sp, pcb) = match pv {
                    Some((sp, cb)) => (Some(sp), Some(cb)),
                    None => (None, None),
                };
                vox_capture::start_video_with_preview(target, cfg, Box::new(move |f| cb.lock().on_frame(f)), sp.as_ref().zip(pcb))
            })?;
            info!(?target, max_fps = cfg.max_fps, max_w = cfg.max_width, max_h = cfg.max_height, standby, "screen capture started");
            Ok(VideoProducer::Capture(cap, stage, tap))
        }
        ShareSource::H264File(_) => bail!("an h264 file has no raw path"),
        ShareSource::AudioApp { .. } | ShareSource::AudioSystem | ShareSource::AudioTone => bail!("an audio-only share has no video"),
    }
}

/// Copy one captured NV12 frame into a WebRTC NV12 buffer (two plane memcpys, no conversion;
/// libwebrtc converts to I420 only if the encoder needs it). On macOS a captured frame is not
/// copied at all: its pixel buffer goes to libwebrtc as is.
fn build_nv12_frame(counters: &StreamCounters, f: &Nv12Frame<'_>) -> Option<RawFrame> {
    let (w, h) = (f.width, f.height);
    if w < 2 || h < 2 {
        return None;
    }
    #[cfg(target_os = "macos")]
    if let Some(pb) = vox_capture::retained_pixel_buffer(f) {
        // SAFETY: a CVPixelBuffer retained once for us; `from_cv_pixel_buffer` takes that
        // reference over (libwebrtc releases it with the frame).
        let buffer = unsafe { livekit::webrtc::video_frame::native::NativeBuffer::from_cv_pixel_buffer(pb.as_ptr()) };
        let frame = VideoFrame { rotation: VideoRotation::VideoRotation0, timestamp_us: f.timestamp_us, frame_metadata: None, buffer };
        return Some(RawFrame { frame: RawBuffer::Native(frame), width: w, height: h });
    }
    let mut buf = NV12Buffer::new(w, h);
    let (dsy, dsuv) = buf.strides();
    let (dy, duv) = buf.data_mut();
    let uv_row = (w.div_ceil(2) * 2) as usize;
    if !copy_plane(f.y, f.y_stride as usize, dy, dsy as usize, w as usize, h as usize)
        || !copy_plane(f.uv, f.uv_stride as usize, duv, dsuv as usize, uv_row, h.div_ceil(2) as usize)
    {
        counters.errors.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let frame = VideoFrame { rotation: VideoRotation::VideoRotation0, timestamp_us: f.timestamp_us, frame_metadata: None, buffer: buf };
    Some(RawFrame { frame: RawBuffer::Nv12(frame), width: w, height: h })
}

pub(crate) fn copy_plane(src: &[u8], src_stride: usize, dst: &mut [u8], dst_stride: usize, row_bytes: usize, rows: usize) -> bool {
    if rows == 0 || src.len() < src_stride * (rows - 1) + row_bytes || dst.len() < dst_stride * (rows - 1) + row_bytes {
        return false;
    }
    for r in 0..rows {
        dst[r * dst_stride..r * dst_stride + row_bytes].copy_from_slice(&src[r * src_stride..r * src_stride + row_bytes]);
    }
    true
}

fn push_audio_chunk(source: &NativeAudioSource, counters: &StreamCounters, chunk: &vox_capture::AudioChunk<'_>) {
    let channels = chunk.channels as u32;
    if channels == 0 || chunk.samples.is_empty() {
        return;
    }
    let frame = AudioFrame {
        data: Cow::Borrowed(chunk.samples),
        sample_rate: chunk.sample_rate,
        num_channels: channels,
        samples_per_channel: (chunk.samples.len() / channels as usize) as u32,
    };
    match source.capture_frame(&frame).now_or_never() {
        Some(Ok(())) if channels == 2 => counters.audio_stereo(chunk.samples),
        Some(Ok(())) => counters.audio(chunk.samples),
        _ => {
            counters.errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Paced loop helper: calls `tick` on average once per `period`. Windows sleeps can overshoot
/// by a timer tick (~1-16 ms), so missed ticks are caught up back-to-back (keeps a 10 ms audio
/// source at exactly 100 chunks/s); the backlog is only dropped after a real stall (>200 ms).
pub(crate) fn paced(stop: &AtomicBool, period: Duration, tick: impl FnMut()) {
    let never = AtomicBool::new(false);
    paced_until(&StopOrDone { stop, done: &never }, period, tick);
}

fn spawn_test_pattern(
    width: u32,
    height: u32,
    fps: u32,
    source: NativeVideoSource,
    counters: Arc<StreamCounters>,
) -> ProducerThread {
    ProducerThread::spawn("vox-test-pattern", move |stop| {
        let pattern = TestPattern::new(width, height);
        let mut index = 0u64;
        info!(width, height, fps, "test pattern source running");
        paced(&stop, Duration::from_secs_f64(1.0 / fps as f64), || {
            let mut buf = NV12Buffer::new(width, height);
            let (sy, suv) = buf.strides();
            let (y, uv) = buf.data_mut();
            pattern.render(index, y, sy as usize, uv, suv as usize);
            let frame = VideoFrame { rotation: VideoRotation::VideoRotation0, timestamp_us: 0, frame_metadata: None, buffer: buf };
            source.capture_frame(&frame);
            counters.frame(width, height);
            index += 1;
        });
    })
}

fn spawn_sine(source: NativeAudioSource, counters: Arc<StreamCounters>) -> ProducerThread {
    ProducerThread::spawn("vox-test-tone", move |stop| {
        let mut gen = SineGenerator::new(vox_capture::AUDIO_SAMPLE_RATE, 440.0, 660.0, 0.25);
        let mut buf = vec![0i16; vox_capture::AUDIO_CHUNK_FRAMES * vox_capture::AUDIO_CHANNELS as usize];
        paced(&stop, Duration::from_millis(10), || {
            gen.fill_stereo(&mut buf);
            let chunk = vox_capture::AudioChunk {
                samples: &buf,
                sample_rate: vox_capture::AUDIO_SAMPLE_RATE,
                channels: vox_capture::AUDIO_CHANNELS,
                timestamp_us: 0,
            };
            push_audio_chunk(&source, &counters, &chunk);
        });
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_targets() {
        assert_eq!(screenshare_bitrate(1920, 1080, 30), 8_000_000);
        assert_eq!(screenshare_bitrate(1920, 1080, 60), 12_000_000);
        let b720 = screenshare_bitrate(1280, 720, 30);
        assert!((4_200_000..=4_500_000).contains(&b720), "{b720}");
        assert_eq!(screenshare_bitrate(320, 180, 5), 500_000); // floor
        assert_eq!(screenshare_bitrate(3840, 2160, 60), 20_000_000); // cap
        let o = ShareOptions { max_bitrate: Some(15_000_000), ..Default::default() };
        assert_eq!(o.bitrate(), 15_000_000);
        assert_eq!(ShareOptions::default().bitrate(), 8_000_000);
    }

    #[test]
    fn drop_tracker_finds_holes_and_broken_frames() {
        let mut t = DropTracker::default();
        let (k, _) = t.push(true);
        let (d1, _) = t.push(false);
        let (_d2, _) = t.push(false);
        let (d3, _) = t.push(false);
        assert_eq!(t.encoded(k), DropReport::default());
        assert_eq!(t.encoded(d1), DropReport::default());
        // d2 dropped: d3 (already pushed) decodes against the wrong reference.
        assert_eq!(t.encoded(d3), DropReport { dropped: 1, broken: 1 });
        // Every later delta is broken until a keyframe goes out.
        assert_eq!(t.push(false).1, 1);
        assert_eq!(t.push(true).1, 0);
        assert_eq!(t.push(false).1, 0);
    }

    #[test]
    fn drop_tracker_starts_at_the_first_report() {
        // Frames pushed before the track was published never get a report: not drops.
        let mut t = DropTracker::default();
        let (_early, _) = t.push(true);
        let (_early2, _) = t.push(false);
        let (k, _) = t.push(true);
        let (d, _) = t.push(false);
        assert_eq!(t.encoded(k), DropReport::default());
        assert_eq!(t.encoded(d), DropReport::default());
    }

    #[test]
    fn drop_tracker_ignores_other_tracks_ids() {
        // A webcam and a screen share push at the same time: their ids interleave, and each
        // track's events only report its own frames. No false drops.
        let mut cam = DropTracker::default();
        let mut screen = DropTracker::default();
        let mut cam_ids = Vec::new();
        for i in 0..20 {
            cam_ids.push(cam.push(i == 0).0);
            screen.push(i == 0);
        }
        for id in &cam_ids {
            assert_eq!(cam.encoded(*id), DropReport::default());
        }
        // A real hole is still found.
        let (a, _) = cam.push(false);
        let (_b, _) = cam.push(false);
        screen.push(false);
        let (c, _) = cam.push(false);
        assert_eq!(cam.encoded(a), DropReport::default());
        assert_eq!(cam.encoded(c), DropReport { dropped: 1, broken: 1 });
    }

    #[test]
    fn drop_tracker_keyframe_after_hole_repairs() {
        let mut t = DropTracker::default();
        let (a, _) = t.push(true);
        let (_b, _) = t.push(false);
        let (c, _) = t.push(false);
        let (key, _) = t.push(true);
        t.encoded(a);
        // b dropped; c broken, the keyframe after it repairs the chain.
        assert_eq!(t.encoded(c), DropReport { dropped: 1, broken: key - c });
        assert_eq!(t.push(false).1, 0);
        // Out-of-order / older ids (another sink's frames) are never counted as drops.
        assert_eq!(t.encoded(a), DropReport::default());
    }

    #[test]
    fn budget_guard_cancels_a_persistent_overshoot() {
        let target = 2_000_000u64;
        let fps = 30.0;
        let mut g = BudgetGuard::default();
        let t0 = Instant::now();
        let mut max_fill: f64 = 0.0;
        for i in 0..300u32 {
            let now = t0 + Duration::from_secs_f64(i as f64 / fps);
            // An encoder that produces 3x whatever it is asked for.
            let bytes = (target as f64 * g.factor() * 3.0 / fps / 8.0) as usize;
            g.on_frame(bytes, now, target);
            if i > 60 {
                max_fill = max_fill.max(g.debt_bits / (target as f64 * DROPPER_BUCKET_S));
            }
        }
        // Settled near 1/3 and well inside the dropper's bucket.
        assert!((0.25..0.45).contains(&g.factor()), "{}", g.factor());
        assert!(max_fill < 0.8, "{max_fill}");
    }

    #[test]
    fn budget_guard_leaves_an_honest_encoder_alone() {
        let target = 8_000_000u64;
        let mut g = BudgetGuard::default();
        let t0 = Instant::now();
        for i in 0..600u32 {
            let now = t0 + Duration::from_secs_f64(i as f64 / 60.0);
            // On target on average (CBR), with a 4x IDR every 2 s paid back by the P frames.
            let unit = target as f64 / 60.0 / 8.0;
            let bytes = if i % 120 == 0 { 4.0 * unit } else { unit * 116.0 / 119.0 };
            g.on_frame(bytes as usize, now, target);
        }
        assert!(g.factor() > 0.95, "{}", g.factor());
        // Unknown target: no correction.
        let mut g = BudgetGuard::default();
        g.on_frame(10_000_000, t0, 0);
        g.on_frame(10_000_000, t0 + Duration::from_millis(30), 0);
        assert_eq!(g.factor(), 1.0);
    }

    #[test]
    fn encoder_target_combines_webrtc_and_test_cap() {
        let max = 12_000_000;
        assert_eq!(encoder_target(0, 0, max), None);
        assert_eq!(encoder_target(9_000_000, 0, max), Some(9_000_000));
        assert_eq!(encoder_target(30_000_000, 0, max), Some(max));
        assert_eq!(encoder_target(0, 3_000_000, max), Some(3_000_000));
        assert_eq!(encoder_target(9_000_000, 3_000_000, max), Some(3_000_000));
        assert_eq!(encoder_target(2_000_000, 3_000_000, max), Some(2_000_000));
        assert_eq!(encoder_target(10_000, 0, max), Some(MIN_ENCODER_BPS));
    }

    #[test]
    fn fit_keeps_aspect_and_even() {
        assert_eq!(fit(3840, 2160, 1920, 1080), (1920, 1080));
        assert_eq!(fit(2560, 1080, 1920, 1080), (1920, 810));
        assert_eq!(fit(1280, 720, 1920, 1080), (1280, 720));
        let (w, h) = fit(1999, 1001, 1920, 1080);
        assert!(w % 2 == 0 && h % 2 == 0 && w <= 1920);
    }

    #[test]
    fn plane_copy_bounds() {
        let src = vec![7u8; 10 * 4];
        let mut dst = vec![0u8; 12 * 4];
        assert!(copy_plane(&src, 10, &mut dst, 12, 8, 4));
        assert_eq!(&dst[12..20], &[7u8; 8]);
        assert_eq!(dst[20], 0);
        assert!(!copy_plane(&src, 10, &mut dst, 12, 8, 5));
    }

    #[test]
    fn encoder_choice_parsing() {
        assert_eq!("auto".parse::<EncoderChoice>().unwrap(), EncoderChoice::Auto);
        assert_eq!("NVENC".parse::<EncoderChoice>().unwrap(), EncoderChoice::Nvenc);
        assert_eq!("mf".parse::<EncoderChoice>().unwrap(), EncoderChoice::MediaFoundation);
        assert_eq!("software".parse::<EncoderChoice>().unwrap(), EncoderChoice::Software);
        assert!("x264".parse::<EncoderChoice>().is_err());
    }

    #[test]
    fn rate_forwarding_threshold() {
        assert!(rate_change_worth_forwarding(0, 1_000_000));
        assert!(!rate_change_worth_forwarding(1_000_000, 1_040_000));
        assert!(rate_change_worth_forwarding(1_000_000, 1_050_000));
        assert!(rate_change_worth_forwarding(1_000_000, 900_000));
    }

    #[test]
    fn raw_sources_never_take_the_encoded_path() {
        assert_eq!(encoded_backend(&ShareSource::TestPattern, EncoderChoice::Auto), None);
        assert_eq!(encoded_backend(&ShareSource::Monitor(0), EncoderChoice::Software), None);
        let f = ShareSource::H264File(H264FileSource { path: "x.h264".into(), fps: 30, looped: false });
        assert_eq!(encoded_backend(&f, EncoderChoice::Software), Some(None));
    }

    // ---- source switch state machine (pure parts) ----

    fn drive(g: &mut FrameGate<u32>, frames: &[(u32, bool)]) -> Vec<u32> {
        frames.iter().filter_map(|&(id, key)| g.offer(key, || id)).collect()
    }

    #[test]
    fn live_gate_forwards_everything_and_retired_nothing() {
        let mut g = FrameGate::<u32>::live();
        assert_eq!(drive(&mut g, &[(1, true), (2, false), (3, false)]), vec![1, 2, 3]);
        g.retire();
        assert_eq!(g.mode(), FrameMode::Retired);
        assert!(drive(&mut g, &[(4, true), (5, false)]).is_empty());
        assert_eq!(g.activate(), None, "a retired gate never comes back");
    }

    #[test]
    fn standby_holds_frames_and_hands_over_a_fresh_keyframe() {
        let mut g = FrameGate::<u32>::standby();
        assert!(!g.seen_frame());
        assert!(drive(&mut g, &[(1, true)]).is_empty());
        assert!(g.seen_frame());
        // Static window: the keyframe is still the newest frame, so it starts the stream.
        assert_eq!(g.activate(), Some(1));
        assert_eq!(g.mode(), FrameMode::Live);
        assert_eq!(drive(&mut g, &[(2, false), (3, false)]), vec![2, 3]);
    }

    #[test]
    fn standby_with_deltas_waits_for_the_next_keyframe() {
        let mut g = FrameGate::<u32>::standby();
        drive(&mut g, &[(1, true), (2, false), (3, false)]);
        assert_eq!(g.activate(), None, "kept keyframe is stale once a delta followed it");
        assert_eq!(g.mode(), FrameMode::AwaitKey);
        // Deltas are never forwarded before a keyframe.
        assert!(drive(&mut g, &[(4, false), (5, false)]).is_empty());
        assert_eq!(drive(&mut g, &[(6, true), (7, false)]), vec![6, 7]);
        assert_eq!(g.mode(), FrameMode::Live);
    }

    #[test]
    fn standby_keeps_only_the_latest_keyframe() {
        let mut g = FrameGate::<u32>::standby();
        drive(&mut g, &[(1, true), (2, true)]);
        assert_eq!(g.activate(), Some(2));
    }

    #[test]
    fn activating_an_empty_standby_gate_awaits_a_keyframe() {
        let mut g = FrameGate::<u32>::standby();
        assert_eq!(g.activate(), None);
        assert_eq!(g.mode(), FrameMode::AwaitKey);
        assert_eq!(g.activate(), None, "second activation changes nothing");
        assert_eq!(drive(&mut g, &[(1, true)]), vec![1]);
    }

    #[test]
    fn gate_builds_frames_only_when_it_keeps_them() {
        let mut g = FrameGate::<u32>::standby();
        let mut built = 0;
        for key in [false, false, true] {
            g.offer(key, || {
                built += 1;
                0
            });
        }
        assert_eq!(built, 1, "deltas in standby are not copied");
    }

    #[test]
    fn audio_plan_transitions() {
        assert_eq!(plan_audio(false, false, false), AudioPlan::Nothing);
        assert_eq!(plan_audio(false, false, true), AudioPlan::Nothing);
        assert_eq!(plan_audio(true, false, true), AudioPlan::Stop);
        // off -> on: publish the audio track the first time, just unmute afterwards
        assert_eq!(plan_audio(false, true, false), AudioPlan::Start { replace: false, publish: true });
        assert_eq!(plan_audio(false, true, true), AudioPlan::Start { replace: false, publish: false });
        // on -> on (new app): restart the capture on the same publication
        assert_eq!(plan_audio(true, true, true), AudioPlan::Start { replace: true, publish: false });
    }

    #[test]
    fn switch_decision_keeps_the_old_share_on_failure() {
        // new capture delivering: switch
        assert_eq!(switch_decision(true, true, true), SwitchDecision::Commit);
        assert_eq!(switch_decision(true, true, false), SwitchDecision::Commit);
        // new capture silent or dead while the old one lives: keep the old one
        assert_eq!(switch_decision(true, false, true), SwitchDecision::KeepOld);
        assert_eq!(switch_decision(false, false, true), SwitchDecision::KeepOld);
        // old one already dead: take the new one even if it has not delivered yet
        assert_eq!(switch_decision(true, false, false), SwitchDecision::CommitSilent);
        assert_eq!(switch_decision(false, false, false), SwitchDecision::Fail);
    }

    #[test]
    fn capture_failures_have_portuguese_text() {
        let e = capture_error("start_video", vox_capture::CaptureError::NotFound("hwnd 5".into()));
        assert!(capture_failure_pt(&e).contains("não existe mais"));
        assert!(format!("{e}").contains("target not found"), "English text kept for logs");
        let other = anyhow!("boom");
        assert_eq!(capture_failure_pt(&other), "boom");
    }
}

#[cfg(test)]
mod audio_only_tests {
    use super::*;

    #[test]
    fn test_mode_never_captures_real_screens_windows_or_sound() {
        // Window 10 belongs to the test window (pid 50), window 11 to somebody's browser (pid 60),
        // window 12 to this process.
        let me = std::process::id();
        let pid_of = |h: isize| match h {
            10 => Some(50),
            11 => Some(60),
            12 => Some(me),
            _ => None,
        };
        let is_test = |pid: u32| pid == 50;
        let refused = |s: ShareSource, audio_pid: Option<u32>| refusal_for(&s, audio_pid, pid_of, is_test);
        for s in [ShareSource::Monitor(0), ShareSource::Monitor(2), ShareSource::AudioSystem, ShareSource::Window(11), ShareSource::Window(99), ShareSource::AudioApp { pid: 60 }] {
            let why = refused(s.clone(), None).unwrap_or_else(|| panic!("{s:?} must be refused"));
            assert!(why.contains(crate::testmode::ALLOW_REAL_CAPTURE_ENV), "{why}");
        }
        for s in [ShareSource::Window(10), ShareSource::Window(12), ShareSource::AudioApp { pid: 50 }, ShareSource::TestPattern, ShareSource::AudioTone] {
            assert_eq!(refused(s.clone(), None), None, "{s:?}");
        }
        // The test window's picture with somebody else's sound: refused too.
        assert!(refused(ShareSource::Window(10), Some(60)).is_some());
        assert!(refused(ShareSource::TestPattern, Some(50)).is_none());
        assert!(crate::testmode::is_test_capture_exe("Test_Window.exe") && crate::testmode::is_test_capture_exe("test_window"));
        assert!(!crate::testmode::is_test_capture_exe("chrome.exe") && !crate::testmode::is_test_capture_exe("test_window2.exe"));
    }

    #[test]
    fn audio_only_sources_have_audio_targets_and_no_video() {
        assert!(ShareSource::AudioApp { pid: 7 }.is_audio_only());
        assert!(ShareSource::AudioSystem.is_audio_only());
        assert!(ShareSource::AudioTone.is_audio_only());
        for s in [ShareSource::Window(1), ShareSource::Monitor(0), ShareSource::TestPattern] {
            assert!(!s.is_audio_only(), "{s:?}");
        }
        // One app: its process tree. All the sound: everything except us (never echo the call).
        assert_eq!(default_audio_target(&ShareSource::AudioApp { pid: 7 }), Some(AudioTarget::ProcessTree { pid: 7 }));
        assert_eq!(default_audio_target(&ShareSource::AudioSystem), Some(AudioTarget::SystemExcludingProcessTree { pid: std::process::id() }));
        assert_eq!(default_audio_target(&ShareSource::AudioTone), None);
        // G1 (v1.1): a whole-screen share's sound leaves out our own process tree too, where the
        // call's voices, the soundboard and the alerts are played (cpal, in this process).
        assert_eq!(default_audio_target(&ShareSource::Monitor(0)), Some(AudioTarget::SystemExcludingProcessTree { pid: std::process::id() }));
        assert_eq!(default_audio_target(&ShareSource::Monitor(3)), Some(AudioTarget::SystemExcludingProcessTree { pid: std::process::id() }));
        // They never take the encoded video path.
        for s in [ShareSource::AudioApp { pid: 7 }, ShareSource::AudioSystem, ShareSource::AudioTone] {
            assert!(encoded_backend(&s, EncoderChoice::Auto).is_none(), "{s:?}");
        }
    }

    #[test]
    fn quality_choices() {
        assert_eq!(AUDIO_ONLY_NORMAL_BPS, 128_000);
        assert_eq!(AUDIO_ONLY_HIGH_BPS, 256_000);
        assert_eq!(ShareOptions::default().audio_bitrate, AUDIO_ONLY_NORMAL_BPS);
    }

    #[test]
    fn a_running_process_is_alive_and_a_bogus_one_is_not() {
        assert!(process_alive(std::process::id()));
        if cfg!(windows) {
            assert!(!process_alive(0x7fff_fff0));
        }
    }

    #[test]
    fn audio_failures_are_portuguese() {
        let e = capture_error("start_audio", vox_capture::CaptureError::Unsupported("old windows".into()));
        assert!(audio_failure_pt(&e).contains(if cfg!(target_os = "macos") { "macOS 13" } else { "Windows 10" }));
        let e = capture_error("start_audio", vox_capture::CaptureError::NotFound("pid 9".into()));
        assert!(audio_failure_pt(&e).contains("não existe mais"));
    }
}
