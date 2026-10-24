//! Webcam publishing (v0.3, item 11): vox-capture's camera -> a `TrackSource::Camera` video
//! publication, independent of Go Live (both can run at once).
//!
//! - **Encoded** path: the camera's frames go through the same hardware encoder as Go Live and
//!   are published pre-encoded, behind the same [`EncodedSink`] (keyframe / congestion feedback,
//!   WebRTC frame-drop guard and recovery IDRs).
//! - **Raw** path (no hardware encoder): NV12 frames -> SDK -> OpenH264.
//! - **Self-view**: the camera thread also produces small NV12 preview frames (no decode of our
//!   own stream), handed to a callback; the voice session sends them to a mirrored viewer tile.
//!
//! Publish once, mute to stop: like the Go Live tracks, a camera publication is created the first
//! time it is needed and then kept (muted while the camera is off) for the whole voice session,
//! because `unpublish_track` leaks the SDK's sender + encoder. Turning the camera off drops the
//! capture, which releases the device (the camera light goes off).

use std::sync::{Arc, OnceLock};

use anyhow::{anyhow, Result};
use livekit::options::{TrackPublishOptions, VideoCodec, VideoEncoderBackend, VideoEncoding};
use livekit::prelude::*;
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::{RtcVideoSource, VideoResolution};
use parking_lot::Mutex;
use tracing::{info, warn};
use vox_capture::{
    CameraAdjust, CameraCapture, CameraConfig, CameraControlId, CameraError, CameraFault, CameraFormat, CameraMode,
    CameraTransform, EncoderBackend, FrameCallback,
};

use super::encoders;
use super::publish::{EncHandle, EncStage, EncodedSink, EncoderChoice, RawHandle, RawStage, Stage, TestHooks, TimingSlot, VideoPath};
use crate::stats::{EncodedFeedback, EncoderReport, StreamCounters};

/// Camera quality presets (the Settings combo). All at 30 fps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CameraQuality {
    P360,
    P540,
    #[default]
    P720,
    P1080,
}

impl CameraQuality {
    pub const ALL: [CameraQuality; 4] = [CameraQuality::P360, CameraQuality::P540, CameraQuality::P720, CameraQuality::P1080];

    /// (width, height, fps)
    pub fn dims(self) -> (u32, u32, u32) {
        match self {
            CameraQuality::P360 => (640, 360, 30),
            CameraQuality::P540 => (960, 540, 30),
            CameraQuality::P720 => (1280, 720, 30),
            CameraQuality::P1080 => (1920, 1080, 30),
        }
    }
}

/// Webcam configuration beyond the quality preset (v0.6): the native mode, our own picture
/// processing and the driver's image controls. Everything here is saved per camera by the app.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CameraSetup {
    /// `None` = automatic (the closest native mode to the quality preset).
    pub mode: Option<CameraMode>,
    pub transform: CameraTransform,
    /// Image controls to set once the camera is open (the ones the user changed).
    pub controls: Vec<CameraAdjust>,
}

/// What changing a running camera's setup takes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SetupDelta {
    pub transform: Option<CameraTransform>,
    /// Controls whose setting changed.
    pub set: Vec<CameraAdjust>,
    /// Controls that went back to the camera's default.
    pub reset: Vec<CameraControlId>,
    /// The native mode changed: only a restart applies it.
    pub restart: bool,
}

impl SetupDelta {
    pub fn is_empty(&self) -> bool {
        self.transform.is_none() && self.set.is_empty() && self.reset.is_empty() && !self.restart
    }
}

/// What to do to a camera running with `old` so that it runs with `new`. Controls are only
/// touched when their setting changed.
pub fn setup_delta(old: &CameraSetup, new: &CameraSetup) -> SetupDelta {
    let mut d = SetupDelta {
        transform: (old.transform != new.transform).then_some(new.transform),
        restart: old.mode != new.mode,
        ..Default::default()
    };
    for a in &new.controls {
        if !old.controls.iter().any(|o| o == a) {
            d.set.push(*a);
        }
    }
    for o in &old.controls {
        if !new.controls.iter().any(|a| a.id == o.id) {
            d.reset.push(o.id);
        }
    }
    d
}

/// Apply a [`SetupDelta`] (without its `restart`) to a running capture.
pub fn apply_delta(cap: &CameraCapture, d: &SetupDelta) {
    if let Some(t) = d.transform {
        cap.set_transform(t);
    }
    for id in &d.reset {
        cap.reset_control(*id);
    }
    cap.apply_controls(&d.set);
}

/// What camera to publish and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraOptions {
    /// `vox_capture::CameraInfo::id`, a friendly name, or empty for the default camera.
    pub device: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// `None` = [`camera_bitrate`] for the size and rate.
    pub max_bitrate: Option<u64>,
    pub encoder: EncoderChoice,
    pub setup: CameraSetup,
}

impl Default for CameraOptions {
    fn default() -> Self {
        Self { device: String::new(), width: 1280, height: 720, fps: 30, max_bitrate: None, encoder: EncoderChoice::Auto, setup: CameraSetup::default() }
    }
}

impl CameraOptions {
    pub fn with_quality(device: impl Into<String>, q: CameraQuality) -> Self {
        let (width, height, fps) = q.dims();
        Self { device: device.into(), width, height, fps, ..Default::default() }
    }

    /// Add the saved setup. A chosen native mode also sets the frame rate we publish at.
    pub fn with_setup(mut self, setup: CameraSetup) -> Self {
        if let Some(m) = setup.mode {
            self.fps = ((m.fps() + 0.5) as u32).clamp(1, 60);
        }
        self.setup = setup;
        self
    }

    pub fn bitrate(&self) -> u64 {
        self.max_bitrate.unwrap_or_else(|| camera_bitrate(self.width, self.height, self.fps))
    }
}

/// Webcam bitrate: 720p30 = 1.2 Mbps (talking heads compress well), scaled by pixel count^0.75
/// and frame rate: 360p ~0.42, 540p ~0.78, 1080p ~2.2 Mbps.
pub fn camera_bitrate(width: u32, height: u32, fps: u32) -> u64 {
    let px = (width as f64 * height as f64) / (1280.0 * 720.0);
    let fps = (fps.max(1) as f64 / 30.0).min(2.0);
    ((1_200_000.0 * px.powf(0.75) * fps) as u64).clamp(250_000, 4_000_000)
}

/// Portuguese text for a camera that failed to start or stopped.
pub fn camera_failure_pt(e: &CameraError) -> String {
    if cfg!(target_os = "macos") && e.fault == CameraFault::AccessDenied {
        return "O macOS não deixou o concord usar a câmera. Ligue o concord em Ajustes do Sistema > Privacidade e Segurança > Câmera.".into();
    }
    match e.fault {
        CameraFault::NotFound => "Nenhuma câmera encontrada. Conecte uma câmera ou escolha outra em Configurações.".into(),
        CameraFault::InUse => "A câmera está sendo usada por outro programa. Feche o outro programa e tente de novo.".into(),
        CameraFault::AccessDenied => {
            "O Windows bloqueou o acesso à câmera. Ative em Configurações do Windows > Privacidade e segurança > Câmera (inclusive \"Permitir que aplicativos da área de trabalho acessem a câmera\").".into()
        }
        CameraFault::Disconnected => "A câmera foi desconectada.".into(),
        CameraFault::NoFrames => "A câmera não enviou imagem. Ela pode estar em uso por outro programa ou com defeito.".into(),
        CameraFault::Unsupported => "Esta câmera não pode ser usada neste computador (formato ou vídeo não suportado).".into(),
        CameraFault::Other => format!("Não foi possível usar a câmera ({}).", e.detail),
    }
}

/// Makes the self-view frame callback (once per capture attempt).
pub type PreviewFactory = Arc<dyn Fn() -> FrameCallback + Send + Sync>;

/// One camera publication (raw or pre-encoded), kept muted while the camera is off.
struct CameraTrack {
    path: VideoPath,
    source: NativeVideoSource,
    track: LocalVideoTrack,
    publication: LocalTrackPublication,
    fps: u32,
    max_bitrate: u64,
    /// libwebrtc's encoder for this sender (`PreEncoded` on the encoded path).
    encoder: VideoEncoderBackend,
}

impl CameraTrack {
    async fn publish(room: &Room, path: VideoPath, source: NativeVideoSource, fps: u32, max_bitrate: u64, encoder: VideoEncoderBackend) -> Result<Self> {
        let track = LocalVideoTrack::create_video_track("camera", RtcVideoSource::Native(source.clone()));
        // v1.1 (E1): the encoded path's per-frame timing events (`publish_timing_events`) need a
        // sender frame transformer. Subscribed BEFORE publishing, the SDK makes it at the publish
        // and an end-to-end encrypted room's frame cryptor chains itself in front of it. Made after
        // (the first subscription on a published track), it REPLACED the cryptor on the sender:
        // the pre-encoded H.264 then went out unencrypted (found by the e2v gate). Later
        // subscriptions reuse that transformer.
        if encoder == VideoEncoderBackend::PreEncoded {
            drop(track.publish_timing_events());
        }
        let opts = TrackPublishOptions {
            source: TrackSource::Camera,
            video_codec: VideoCodec::H264,
            simulcast: false,
            video_encoding: Some(VideoEncoding { max_bitrate, max_framerate: fps as f64 }),
            video_encoder: encoder,
            ..Default::default()
        };
        let publication = room
            .local_participant()
            .publish_track(LocalTrack::Video(track.clone()), opts)
            .await
            .map_err(|e| anyhow!("publish camera ({path:?} path): {e}"))?;
        info!(?path, fps, max_kbps = max_bitrate / 1000, "camera video published");
        Ok(Self { path, source, track, publication, fps, max_bitrate, encoder })
    }

    async fn unpublish(self, room: &Room) {
        if let Err(e) = room.local_participant().unpublish_track(&self.publication.sid()).await {
            warn!(error = %e, path = ?self.path, "unpublish camera");
        }
    }
}

/// The session's camera publications (at most one per path), reused by every camera start.
#[derive(Default)]
pub struct CameraPublication {
    raw: Option<CameraTrack>,
    encoded: Option<CameraTrack>,
}

impl CameraPublication {
    fn slot(&mut self, path: VideoPath) -> &mut Option<CameraTrack> {
        match path {
            VideoPath::Raw => &mut self.raw,
            VideoPath::Encoded => &mut self.encoded,
        }
    }

    /// Republish when the encoding settings changed (the old sender stays allocated until leave:
    /// SDK limitation, see `SharePublication`).
    async fn drop_incompatible(&mut self, room: &Room, path: VideoPath, fps: u32, max_bitrate: u64, encoder: VideoEncoderBackend) {
        let slot = self.slot(path);
        if slot.as_ref().is_some_and(|t| t.fps != fps || t.max_bitrate != max_bitrate || t.encoder != encoder) {
            info!(?path, fps, max_kbps = max_bitrate / 1000, "camera settings changed; republishing the camera track");
            if let Some(old) = slot.take() {
                old.unpublish(room).await;
            }
        }
    }

    fn mute(&self) {
        for t in [&self.raw, &self.encoded].into_iter().flatten() {
            t.publication.mute();
        }
    }

    /// Unpublish for good (leaving the room).
    pub async fn unpublish(self, room: &Room) {
        for t in [self.raw, self.encoded].into_iter().flatten() {
            t.unpublish(room).await;
        }
    }
}

/// A capture that started: (capture, path, video source, frame stage, feedback, timing slot).
type Started = (vox_capture::CameraCapture, VideoPath, NativeVideoSource, Stage2, Option<Arc<EncodedFeedback>>, TimingSlot);

#[allow(dead_code)] // the stages are held so the capture callbacks' targets outlive the capture
enum Stage2 {
    Encoded(EncHandle),
    Raw(RawHandle),
}

/// A running camera (capture on, publication unmuted).
pub struct ActiveCamera {
    pub opts: CameraOptions,
    pub path: VideoPath,
    pub track: LocalVideoTrack,
    pub counters: Arc<StreamCounters>,
    pub format: CameraFormat,
    encoder: Option<(String, Arc<EncodedFeedback>)>,
    /// The encoder in use (one of [`encoders::plan`]).
    attempt: encoders::Attempt,
    capture: Option<vox_capture::CameraCapture>,
    _stage: Stage2,
    publication: Option<CameraPublication>,
}

/// Failed camera start: the (muted) publications come back for reuse.
pub struct CameraStartError {
    pub error: anyhow::Error,
    /// Portuguese, ready to show.
    pub message: String,
    pub publication: CameraPublication,
}

fn backend_name(b: Option<EncoderBackend>) -> &'static str {
    match b {
        Some(EncoderBackend::Nvenc) => "nvenc",
        Some(EncoderBackend::MediaFoundation) => "mf",
        Some(EncoderBackend::Auto) => "auto",
        #[cfg(target_os = "macos")]
        Some(EncoderBackend::VideoToolbox) => "videotoolbox",
        None => "software",
    }
}

impl ActiveCamera {
    /// Open the camera and publish (or unmute) its track. `preview` makes the callback that
    /// receives the small NV12 self-view frames (switch them with [`ActiveCamera::set_preview`]).
    pub async fn start(
        room: &Room,
        opts: CameraOptions,
        reuse: Option<CameraPublication>,
        preview: Option<PreviewFactory>,
    ) -> std::result::Result<ActiveCamera, Box<CameraStartError>> {
        let mut pubs = reuse.unwrap_or_default();
        let max_bitrate = opts.bitrate();
        let counters = Arc::new(StreamCounters::default());
        let plan = encoders::plan(opts.encoder);
        info!(plan = %encoders::describe(&plan), "camera encoder plan");
        let cfg = CameraConfig {
            width: opts.width,
            height: opts.height,
            fps: opts.fps,
            bitrate_bps: max_bitrate.min(u32::MAX as u64) as u32,
            backend: EncoderBackend::Auto,
            mode: opts.setup.mode,
            transform: opts.setup.transform,
            ..Default::default()
        };
        let fail = |error: anyhow::Error, message: String, pubs: CameraPublication| {
            pubs.mute();
            Box::new(CameraStartError { error, message, publication: pubs })
        };

        // 1. Capture first: a camera that cannot start never shows viewers a dead track.
        let make_preview = || preview.as_ref().map(|f| f());
        let mut started: Option<Started> = None;
        let mut last_err: Option<CameraError> = None;
        let mut used = plan.last().cloned().expect("the plan ends with the processor");
        // Hardware encoders in plan order; an encoder that cannot start on its GPU hands over to
        // the next (camera trouble, like a camera in use, ends the start instead).
        for attempt in plan.iter().filter(|a| !a.is_software() && !encoders::in_libwebrtc(a)) {
            let cfg = CameraConfig { backend: attempt.backend.unwrap_or(EncoderBackend::Auto), adapter_luid: attempt.luid, ..cfg };
            let video_source = match &pubs.encoded {
                Some(t) => t.source.clone(),
                None => NativeVideoSource::new_encoded(VideoResolution { width: opts.width, height: opts.height }),
            };
            let feedback = Arc::new(EncodedFeedback::default());
            let timing: TimingSlot = Arc::new(Mutex::new(None));
            let control = Arc::new(OnceLock::new());
            let mut sink = EncodedSink::new(video_source.clone(), counters.clone(), feedback.clone(), control.clone(), max_bitrate);
            sink.test = Arc::new(TestHooks::default());
            sink.timing = timing.clone();
            let stage: EncHandle = Arc::new(Mutex::new(EncStage { stage: Stage::new(false), sink }));
            let cb = stage.clone();
            let device = opts.device.clone();
            let pv = make_preview();
            let res = tokio::task::spawn_blocking(move || {
                vox_capture::start_camera_encoded(&device, cfg, Box::new(move |f| cb.lock().on_frame(f)), pv)
            })
            .await
            .map_err(|e| CameraError { fault: CameraFault::Other, detail: format!("camera start task: {e}") });
            match res.and_then(|r| r) {
                Ok(cap) => {
                    let _ = control.set(cap.control());
                    started = Some((cap, VideoPath::Encoded, video_source, Stage2::Encoded(stage), Some(feedback), timing));
                    used = attempt.clone();
                    break;
                }
                Err(e) if e.fault == CameraFault::Unsupported => {
                    warn!(error = %e, encoder = %attempt.label, "camera: hardware encoder unavailable; trying the next one");
                }
                Err(e) => {
                    last_err = Some(e);
                    break;
                }
            }
        }
        // macOS: the plan's hardware encoder (VideoToolbox) runs inside libwebrtc, on the raw path.
        let (raw_encoder, raw_hw) = encoders::raw_encoder(opts.encoder);
        if started.is_none() && last_err.is_none() {
            let video_source = match &pubs.raw {
                Some(t) => t.source.clone(),
                None => NativeVideoSource::new(VideoResolution { width: opts.width, height: opts.height }, true),
            };
            let stage: RawHandle = Arc::new(Mutex::new(RawStage { stage: Stage::new(false), source: video_source.clone(), counters: counters.clone() }));
            let cb = stage.clone();
            let device = opts.device.clone();
            let pv = make_preview();
            let res = tokio::task::spawn_blocking(move || vox_capture::start_camera_raw(&device, cfg, Box::new(move |f| cb.lock().on_frame(f)), pv))
                .await
                .map_err(|e| CameraError { fault: CameraFault::Other, detail: format!("camera start task: {e}") });
            match res.and_then(|r| r) {
                Ok(cap) => {
                    started = Some((cap, VideoPath::Raw, video_source, Stage2::Raw(stage), None, Arc::new(Mutex::new(None))));
                    if let Some(a) = raw_hw {
                        used = a;
                    }
                }
                Err(e) => last_err = Some(e),
            }
        }
        let Some((capture, path, video_source, stage, feedback, timing)) = started else {
            let e = last_err.unwrap_or(CameraError { fault: CameraFault::Other, detail: "camera did not start".into() });
            let message = camera_failure_pt(&e);
            return Err(fail(anyhow!("camera start failed: {e}"), message, pubs));
        };
        let format = capture.format();
        let backend = if path == VideoPath::Raw && encoders::in_libwebrtc(&used) { "videotoolbox" } else { backend_name(capture.backend()) };
        // The user's image settings, as soon as the device is open.
        capture.apply_controls(&opts.setup.controls);

        // 2. Publication (once per session and path), then unmute.
        let encoder = match path {
            VideoPath::Raw => raw_encoder,
            VideoPath::Encoded => VideoEncoderBackend::PreEncoded,
        };
        pubs.drop_incompatible(room, path, opts.fps, max_bitrate, encoder).await;
        if pubs.slot(path).is_none() {
            match CameraTrack::publish(room, path, video_source, opts.fps, max_bitrate, encoder).await {
                Ok(t) => *pubs.slot(path) = Some(t),
                Err(e) => {
                    drop(capture);
                    return Err(fail(e, "Não foi possível publicar a câmera na sala.".into(), pubs));
                }
            }
        }
        let t = pubs.slot(path).as_ref().expect("published above");
        if path == VideoPath::Encoded {
            // Per-frame "WebRTC encoded it" events for the drop guard (observe-only).
            *timing.lock() = Some(t.track.publish_timing_events());
        }
        let track = t.track.clone();
        // Unmute right away: frames pushed into a muted track become black raw frames, which the
        // pass-through encoder rejects.
        t.publication.unmute();
        info!(
            device = %format.device,
            mode = format!("{} {}x{} @ {:.1}", format.subtype, format.native_width, format.native_height, format.native_fps),
            out = format!("{}x{}", format.width, format.height),
            gpu_frames = format.gpu_frames,
            ?path,
            backend,
            encoder = %used.label,
            max_kbps = max_bitrate / 1000,
            "camera on"
        );
        Ok(ActiveCamera {
            opts,
            path,
            track,
            counters,
            format,
            encoder: feedback.map(|f| (backend.to_string(), f)),
            attempt: used,
            capture: Some(capture),
            _stage: stage,
            publication: Some(pubs),
        })
    }

    /// `false` once the capture ended on its own ([`ActiveCamera::failure`] says why).
    pub fn running(&self) -> bool {
        self.capture.as_ref().is_some_and(|c| c.is_running())
    }

    pub fn failure(&self) -> Option<CameraError> {
        self.capture.as_ref().and_then(|c| c.failure())
    }

    /// Self-view frames on/off (off while the call view is hidden).
    pub fn set_preview(&self, on: bool) {
        if let Some(c) = &self.capture {
            c.set_preview(on);
        }
    }

    /// The running capture (image controls, picture processing, frame rate).
    pub fn capture(&self) -> Option<&CameraCapture> {
        self.capture.as_ref()
    }

    /// `"nvenc"` / `"mf"` (pre-encoded), `"videotoolbox"` (macOS, inside libwebrtc) or `"software"`.
    pub fn backend(&self) -> String {
        self.encoder.as_ref().map(|(b, _)| b.clone()).unwrap_or_else(|| if encoders::in_libwebrtc(&self.attempt) { "videotoolbox" } else { "software" }.into())
    }

    /// The encoder in use.
    pub fn attempt(&self) -> &encoders::Attempt {
        &self.attempt
    }

    /// What the encoder safety net watches (see `ActiveShare::encoder_probe`); `None` on the
    /// processor.
    pub fn encoder_probe(&self) -> Option<(String, LocalVideoTrack, Arc<EncodedFeedback>)> {
        if self.attempt.is_software() {
            return None;
        }
        let (_, fb) = self.encoder.as_ref()?;
        Some((self.attempt.id.clone(), self.track.clone(), fb.clone()))
    }

    pub fn encoder_report(&self) -> Option<EncoderReport> {
        let (backend, feedback) = self.encoder.as_ref()?;
        let hw = self.capture.as_ref().map(|c| c.stats().encoded);
        Some(EncoderReport { backend: backend.clone(), feedback: feedback.clone(), hw })
    }

    pub fn capture_stats(&self) -> Option<vox_capture::CameraStats> {
        self.capture.as_ref().map(|c| c.stats())
    }

    /// Release the camera (light off) and mute the publication; returns it for the next start.
    /// Blocks while the device shuts down (well under a second): call it off the async runtime.
    pub fn stop(mut self) -> CameraPublication {
        let t0 = std::time::Instant::now();
        self.capture.take();
        let publication = self.publication.take().expect("publication present while the camera runs");
        publication.mute();
        info!(release_ms = t0.elapsed().as_millis() as u64, "camera off (device released, track muted for reuse)");
        publication
    }
}

/// Viewer key of the camera picker's preview (Settings).
pub const PREVIEW_KEY: &str = "cam:preview";

/// The camera picker's preview: the camera with no encoder, its small frames in the viewer tile
/// [`PREVIEW_KEY`] (shown once the app attaches a host for it; the capture mirrors them itself
/// when "Espelhar minha prévia" is on). Dropping it releases the camera and closes the tile.
pub struct CameraPreview {
    capture: Option<vox_capture::CameraCapture>,
}

impl CameraPreview {
    /// Blocks while the camera opens (call it off the async runtime). The error is Portuguese.
    pub fn start(device: &str, quality: CameraQuality) -> std::result::Result<CameraPreview, String> {
        Self::start_with(device, quality, &CameraSetup::default())
    }

    /// [`CameraPreview::start`] with the saved native mode, picture processing and image controls.
    pub fn start_with(device: &str, quality: CameraQuality, setup: &CameraSetup) -> std::result::Result<CameraPreview, String> {
        let stream = match super::Viewer::get() {
            Ok(v) if v.is_native() => v.open_tile(PREVIEW_KEY, super::TileStyle { mirror: false, fill: true }, tokio::sync::mpsc::unbounded_channel().0),
            _ => super::ViewerStream::null(),
        };
        let (width, height, fps) = quality.dims();
        let fps = setup.mode.map_or(fps, |m| ((m.fps() + 0.5) as u32).clamp(1, 60));
        let cfg = CameraConfig { width, height, fps, mode: setup.mode, transform: setup.transform, ..Default::default() };
        let cb: FrameCallback = Box::new(move |f| {
            stream.submit_nv12(f);
        });
        match vox_capture::start_camera_preview(device, cfg, cb) {
            Ok(c) => {
                info!(format = ?c.format(), "camera preview on");
                c.apply_controls(&setup.controls);
                Ok(CameraPreview { capture: Some(c) })
            }
            Err(e) => {
                close_tile(PREVIEW_KEY);
                warn!(error = %e, "camera preview failed");
                Err(camera_failure_pt(&e))
            }
        }
    }

    pub fn running(&self) -> bool {
        self.capture.as_ref().is_some_and(|c| c.is_running())
    }

    /// The running capture (image controls, picture processing, frame rate).
    pub fn capture(&self) -> Option<&CameraCapture> {
        self.capture.as_ref()
    }

    pub fn failure(&self) -> Option<String> {
        self.capture.as_ref().and_then(|c| c.failure()).map(|e| camera_failure_pt(&e))
    }
}

impl Drop for CameraPreview {
    fn drop(&mut self) {
        if self.capture.take().is_some() {
            info!("camera preview off");
        }
        close_tile(PREVIEW_KEY);
    }
}

fn close_tile(key: &str) {
    if let Ok(v) = super::Viewer::get() {
        if v.is_native() {
            v.close(key);
        }
    }
}

// ---------- Test hook: where is the marker in a remote camera picture? ----------
//
// `VOX_CAM_PROBE=1` makes the viewers of remote cameras sample the four corners of every
// decoded frame (an 8x8 luma patch 14 pixels in from each corner), so an autotest can tell the
// orientation of the synthetic camera's marker (a dark box that starts top-left) as the sender
// mirrors / rotates / zooms. Off by default: nothing is read from the frames.

/// How many recent frames the probe looks back over (a bit more than the box's blink period).
const PROBE_WINDOW: usize = 45;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraProbe {
    pub width: u32,
    pub height: u32,
    /// Darkest sample per corner (top-left, top-right, bottom-left, bottom-right) over the last
    /// frames, 0 = black.
    pub dark: [u8; 4],
    /// Frames the numbers cover (a full window is [`PROBE_WINDOW`]).
    pub frames: usize,
}

impl CameraProbe {
    /// The one corner that is dark (`< 40`), if exactly one is.
    pub fn dark_corner(&self) -> Option<usize> {
        let c: Vec<usize> = (0..4).filter(|&i| self.dark[i] < 40).collect();
        (c.len() == 1).then(|| c[0])
    }

    /// "L-TL" / "P-BR" / "L-none": orientation (landscape or portrait) and the dark corner.
    pub fn label(&self) -> String {
        let o = if self.width >= self.height { "L" } else { "P" };
        match self.dark_corner() {
            Some(c) => format!("{o}-{}", ["TL", "TR", "BL", "BR"][c]),
            None => format!("{o}-none"),
        }
    }

    /// Enough frames to judge, and not a mix of two states (two corners dark).
    pub fn settled(&self) -> bool {
        self.frames >= PROBE_WINDOW && (0..4).filter(|&i| self.dark[i] < 40).count() <= 1
    }
}

/// Per camera: the frame size and the corner samples of the last frames.
type ProbeMap = std::collections::HashMap<String, ((u32, u32), std::collections::VecDeque<[u8; 4]>)>;

fn probes() -> &'static Mutex<ProbeMap> {
    static P: OnceLock<Mutex<ProbeMap>> = OnceLock::new();
    P.get_or_init(Default::default)
}

pub fn probe_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VOX_CAM_PROBE").is_ok_and(|v| !v.is_empty() && v != "0"))
}

/// Sample the corners of one decoded frame of `identity`'s camera.
pub(crate) fn probe_frame(identity: &str, buf: &dyn livekit::webrtc::video_frame::VideoBuffer) {
    let (w, h) = (buf.width() as usize, buf.height() as usize);
    let (y, stride) = if let Some(i) = buf.as_i420() {
        (i.data().0, i.strides().0 as usize)
    } else if let Some(n) = buf.as_nv12() {
        (n.data().0, n.strides().0 as usize)
    } else {
        return;
    };
    if w < 64 || h < 64 || y.len() < stride * (h - 1) + w {
        return;
    }
    let mean = |x0: usize, y0: usize| {
        let mut s = 0u32;
        for r in y0..y0 + 8 {
            for c in x0..x0 + 8 {
                s += y[r * stride + c] as u32;
            }
        }
        (s / 64) as u8
    };
    let s = [mean(14, 14), mean(w - 22, 14), mean(14, h - 22), mean(w - 22, h - 22)];
    let mut map = probes().lock();
    let (size, ring) = map.entry(identity.to_string()).or_default();
    if *size != (w as u32, h as u32) {
        *size = (w as u32, h as u32);
        ring.clear();
    }
    ring.push_back(s);
    while ring.len() > PROBE_WINDOW {
        ring.pop_front();
    }
}

/// What the probe saw of `identity`'s camera lately (needs `VOX_CAM_PROBE=1`).
pub fn camera_probe(identity: &str) -> Option<CameraProbe> {
    let map = probes().lock();
    let ((w, h), ring) = map.get(identity)?;
    if ring.is_empty() {
        return None;
    }
    let mut dark = [255u8; 4];
    for s in ring {
        for i in 0..4 {
            dark[i] = dark[i].min(s[i]);
        }
    }
    Some(CameraProbe { width: *w, height: *h, dark, frames: ring.len() })
}

/// Copy one self-view frame into a decoded-frame box for the viewer (two plane copies; the
/// frames are small, 640x360 at most by default).
pub(crate) fn preview_frame(f: &vox_capture::Nv12Frame<'_>) -> Option<livekit::webrtc::video_frame::BoxVideoFrame> {
    use livekit::webrtc::video_frame::{NV12Buffer, VideoFrame, VideoRotation};
    let (w, h) = (f.width, f.height);
    if w < 2 || h < 2 {
        return None;
    }
    let mut buf = NV12Buffer::new(w, h);
    if !fill_nv12(&mut buf, f) {
        return None;
    }
    Some(VideoFrame { rotation: VideoRotation::VideoRotation0, timestamp_us: f.timestamp_us, frame_metadata: None, buffer: Box::new(buf) })
}

/// Copy a captured NV12 frame into `buf` (same size), plane by plane. `false` when the frame is
/// malformed or does not fit.
pub(crate) fn fill_nv12(buf: &mut livekit::webrtc::video_frame::NV12Buffer, f: &vox_capture::Nv12Frame<'_>) -> bool {
    use livekit::webrtc::video_frame::VideoBuffer;
    let (w, h) = (f.width, f.height);
    if w < 2 || h < 2 || buf.width() != w || buf.height() != h {
        return false;
    }
    let (dsy, dsuv) = buf.strides();
    let (dy, duv) = buf.data_mut();
    let uv_row = (w.div_ceil(2) * 2) as usize;
    super::publish::copy_plane(f.y, f.y_stride as usize, dy, dsy as usize, w as usize, h as usize)
        && super::publish::copy_plane(f.uv, f.uv_stride as usize, duv, dsuv as usize, uv_row, h.div_ceil(2) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_bitrates() {
        assert_eq!(camera_bitrate(1280, 720, 30), 1_200_000);
        let b360 = camera_bitrate(640, 360, 30);
        assert!((400_000..=450_000).contains(&b360), "{b360}");
        let b1080 = camera_bitrate(1920, 1080, 30);
        assert!((2_100_000..=2_300_000).contains(&b1080), "{b1080}");
        assert!(camera_bitrate(160, 90, 5) >= 250_000);
        assert_eq!(CameraOptions::with_quality("", CameraQuality::P540).bitrate(), camera_bitrate(960, 540, 30));
    }

    #[test]
    fn setup_delta_touches_only_what_changed() {
        use vox_capture::{CameraRotation, CameraSubtype};
        let adj = |id, value, auto| CameraAdjust { id, value, auto };
        let old = CameraSetup {
            controls: vec![adj(CameraControlId::Brightness, 100, false), adj(CameraControlId::Exposure, -5, false)],
            ..Default::default()
        };
        assert!(setup_delta(&old, &old).is_empty());

        // One slider moved; one control back to its default; the zoom changed.
        let new = CameraSetup {
            controls: vec![adj(CameraControlId::Brightness, 120, false)],
            transform: CameraTransform { zoom_pct: 150, rotation: CameraRotation::Cw90, ..Default::default() },
            ..Default::default()
        };
        let d = setup_delta(&old, &new);
        assert_eq!(d.set, vec![adj(CameraControlId::Brightness, 120, false)]);
        assert_eq!(d.reset, vec![CameraControlId::Exposure]);
        assert_eq!(d.transform, Some(new.transform));
        assert!(!d.restart);

        // A new native mode needs a restart; nothing else does.
        let mode = CameraMode { width: 1280, height: 720, fps_milli: 30_000, subtype: CameraSubtype::Mjpg };
        let d = setup_delta(&old, &CameraSetup { mode: Some(mode), ..old.clone() });
        assert!(d.restart && d.set.is_empty() && d.reset.is_empty() && d.transform.is_none());

        // A chosen mode sets the published frame rate.
        let o = CameraOptions::with_quality("", CameraQuality::P720).with_setup(CameraSetup { mode: Some(CameraMode { fps_milli: 15_000, ..mode }), ..Default::default() });
        assert_eq!(o.fps, 15);
        assert!(o.bitrate() < camera_bitrate(1280, 720, 30));
    }

    #[test]
    fn probe_names_the_marker_corner() {
        let p = |w, h, dark| CameraProbe { width: w, height: h, dark, frames: PROBE_WINDOW };
        assert_eq!(p(1280, 720, [16, 90, 150, 150]).label(), "L-TL");
        assert_eq!(p(720, 1280, [90, 16, 150, 150]).label(), "P-TR");
        assert_eq!(p(1280, 720, [90, 90, 150, 150]).label(), "L-none");
        // Two dark corners = a window that straddles a change: not settled.
        assert!(!p(1280, 720, [16, 16, 150, 150]).settled());
        assert!(p(1280, 720, [90, 90, 150, 150]).settled());
        assert!(!CameraProbe { frames: 5, ..p(1280, 720, [90, 90, 150, 150]) }.settled());
    }

    #[test]
    fn failures_have_portuguese_text() {
        for fault in [CameraFault::NotFound, CameraFault::InUse, CameraFault::AccessDenied, CameraFault::Disconnected, CameraFault::NoFrames, CameraFault::Unsupported, CameraFault::Other] {
            let t = camera_failure_pt(&CameraError { fault, detail: "x".into() });
            assert!(t.contains("câmera") || t.contains("Câmera"), "{fault:?}: {t}");
        }
        assert!(camera_failure_pt(&CameraError { fault: CameraFault::AccessDenied, detail: String::new() }).contains("Privacidade"));
    }
}
