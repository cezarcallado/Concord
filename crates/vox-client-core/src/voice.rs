//! Voice session: one LiveKit room = one voice channel.
//!
//! Subscription policy (saves bandwidth and CPU for viewers):
//! - room `auto_subscribe` is OFF;
//! - every remote **microphone** track is subscribed as soon as it is published: one per
//!   participant (a person has one microphone; extra microphone publications, which only a
//!   modified app would make, are ignored so they cannot pile decoders and mixer sources onto
//!   everybody else, see [`pick_mic`]);
//! - **screenshare** video/audio is subscribed only for the participants being watched (a set of
//!   at most [`MAX_WATCHED`], item 9). Each watched stream has its own viewer surface, decoder
//!   and mixer source. A watched stream whose video is nowhere on screen (chat view, app
//!   minimized, another stream fullscreen; see [`VoiceSession::set_stream_visible`]) has its
//!   video disabled at the SFU after a short grace period: nothing is forwarded or decoded, its
//!   audio keeps playing. Shown again, it is re-enabled and the SFU asks the sender for a
//!   keyframe. (The streams are single-layer pre-encoded H.264: there is no simulcast layer to
//!   pick for small tiles, so a visible small tile is still decoded at full size; the viewer only
//!   draws it less often, see [`crate::golive::Viewer::set_max_fps`].)
//! - **camera** video of everyone is subscribed while the camera tiles are visible
//!   ([`VoiceSession::set_cameras_visible`]: the call view is shown and the app is not
//!   minimized). Hidden, the subscribed cameras are disabled (the SFU stops forwarding them, the
//!   decoders idle); shown again, they are re-enabled and the SFU asks the sender for a keyframe.
//!   A camera turned off (track muted) is unsubscribed.
//!
//! All remote audio (voices + the watched screenshare audio) goes through one [`Mixer`] into a
//! single cpal output stream.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use livekit::options::{AudioEncoding, TrackPublishOptions};
use livekit::prelude::*;
use livekit::webrtc::stats::{IceCandidatePairState, RtcStats};
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_source::{AudioSourceOptions, RtcAudioSource};
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit::webrtc::video_stream::native::NativeVideoStream;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use url::Url;

use crate::audio::device::{new_voice_apm, AudioIo, MicConfig, MicSink, PlaybackConfig};
use crate::audio::fake_mic::FakeMics;
use crate::audio::MicStatus;
use crate::audio::speaking::{self, Speakers};
use crate::audio::{AlertKind, AudioPrefs, Mixer, NoiseControl, SoundTag, SourceId, UserAudio, VoiceGate, MIX_CHANNELS, MIX_RATE};
use crate::audio::sine::SineGenerator;
use crate::golive::publish::{paced, ProducerThread};
use crate::golive::camera::{camera_failure_pt, ActiveCamera, CameraOptions, CameraPublication, PreviewFactory};
use crate::golive::encoders;
use crate::golive::{AUDIO_ONLY_TRACK_NAME, ActiveShare, ShareOptions, SharePreview, SharePublication, ShareSource, SwitchReport, SwitchRequest, TileStyle, VideoPath, Viewer, ViewerNotice, ViewerStream, SELF_SHARE_KEY};
use livekit::webrtc::audio_frame::AudioFrame;
use crate::stats::{Direction, EncodedFeedback, StreamCounters, StreamEntry};

#[derive(Debug, Clone, Default)]
pub struct VoiceConfig {
    /// Capture and publish the microphone.
    pub mic: bool,
    /// Play remote audio on the speakers.
    pub playback: bool,
    /// Substring of the device name; `None` = system default.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    /// Start with the microphone muted (device open, nothing sent).
    pub start_muted: bool,
    /// Master output volume 0.0..=2.0 (1.0 = unity). `None` = 1.0.
    pub output_volume: Option<f32>,
    /// Publish a sine tone of this frequency as the "microphone" instead of a real device
    /// (headless tests; the tone is generated in-process and never played locally).
    pub mic_test_tone: Option<f32>,
    /// Autotests: made-up microphones instead of the real ones ([`crate::audio::fake_mic`]).
    pub fake_mics: Option<FakeMics>,
    /// Saved local listening settings by participant identity, applied before any track
    /// subscribes.
    pub user_audio: HashMap<String, UserAudio>,
    /// Start with every other participant's microphone silenced locally.
    pub others_muted: bool,
    /// Voice-activity gate settings and level meter, owned by the caller so the UI can change
    /// and read it at any time. `None` = a default gate (automatic) nobody reads.
    pub gate: Option<Arc<VoiceGate>>,
    /// Noise suppression level ("Supressão de ruído"), owned by the caller so the UI can change
    /// it mid-call. `None` = the default level ("IA leve").
    pub noise: Option<Arc<NoiseControl>>,
    /// v1.1 (T0): the hub's certificate pin, when the hub was reached over TLS. A `wss://` LiveKit
    /// URL (the hub's own port) is then reached through a local [`crate::tls::Tunnel`] in pinned
    /// TLS; without it a `wss://` URL is refused (never the SDK's own web-PKI TLS).
    pub tls_pin: Option<crate::tls::Pin>,
    /// v1.1 (E1): the room is end-to-end encrypted ([`vox_proto::VoiceJoinResponse::e2ee`]):
    /// every track we publish is encrypted with our own media key, which only the other people's
    /// directory devices in the room get ([`crate::media_e2ee`]). `None`: a plain room.
    pub e2ee: Option<crate::media_e2ee::MediaE2ee>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceEvent {
    Connected { room: String, identity: String, participants: Vec<String> },
    ParticipantJoined { identity: String, name: String },
    ParticipantLeft { identity: String },
    /// Identities currently speaking, sent whenever the set changes (v1.1: see
    /// [`crate::audio::speaking`]): remote voices by their decoded level, LiveKit's active
    /// speakers for people whose audio is not arriving, and whoever has a soundboard sound
    /// playing. Our own identity is in it only while one of our soundboard sounds plays (our own
    /// voice is shown from our gate).
    ActiveSpeakers { identities: Vec<String> },
    ShareStarted { identity: String },
    ShareStopped { identity: String },
    /// Whether `identity`'s live is audio-only ("Só áudio": no video publication, just the
    /// `audio-only` track), sent whenever that changes. It is read from the LiveKit publications,
    /// so it is only known for people in our own room.
    ShareKind { identity: String, audio_only: bool },
    /// Our own Go Live ended without being asked to (capture target closed, device lost).
    LocalShareEnded { reason: String },
    WatchStarted { identity: String },
    WatchEnded { identity: String, reason: String },
    /// A participant's camera came on (subscribed while the tiles are visible: frames go to the
    /// viewer tile [`camera_key`]).
    CameraStarted { identity: String },
    CameraStopped { identity: String },
    /// Our own camera stopped without being asked to (unplugged, taken by another app). The
    /// reason is Portuguese, ready to show.
    LocalCameraEnded { reason: String },
    Reconnecting,
    Reconnected,
    Disconnected { reason: String },
    /// The voice server stopped (or resumed) accepting our microphone: a moderator server-muted
    /// us (item 12). While muted the mic is not published at all; it comes back by itself.
    ServerMuted { muted: bool },
    /// Our microphone changed: as asked ([`VoiceSession::set_input_device`]) or by itself (the
    /// chosen one went away or came back).
    Mic(MicStatus),
    /// A session that joined without a microphone has one now: it is published and can be
    /// unmuted, the "no microphone" state is over.
    MicAvailable,
    /// Our Go Live (`camera == false`) or camera restarted on another encoder because its video
    /// was unstable: keyframe requests without pause on a clean uplink that a viewer confirmed
    /// (it decoded badly while its own network lost nothing), or WebRTC dropping our frames (v0.7).
    /// `label` is Portuguese ("NVIDIA GeForce GTX 1650 (NVENC)", "Processador (sem aceleração)"),
    /// `backend` is "nvenc" / "mf" / "software".
    EncoderSwitched { camera: bool, label: String, backend: String },
    /// v1.1 (E1, encrypted rooms only): how `identity`'s media decrypts here (our own identity:
    /// how ours encrypts), sent when it changes. `MissingKey`: "Aguardando a chave de Fulano…";
    /// `DecryptionFailed`: "Não deu para descriptografar o áudio de Fulano".
    E2eeState { identity: String, state: crate::media_e2ee::MediaE2eeState },
    /// v1.1 (E1): `identity` is in the room with this directory device (hex id), first heard now.
    E2eePeerDevice { identity: String, device_id: String },
}

/// Budget for `Room::connect` (signal WebSocket + ICE/DTLS). The SDK's own limits add up to 20 s
/// without retries (5 s signal + 15 s media); this is the safety net on top of them.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(25);
/// TCP reachability probe of the LiveKit signal port.
const SIGNAL_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Publishing the microphone track (the SDK waits at most 10 s for the server's answer).
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(15);
/// Cap on `get_stats` calls made only for the log.
const STATS_TIMEOUT: Duration = Duration::from_secs(3);

/// Streams that can be watched at the same time (item 9). Each one costs a full-size decoder
/// (single-layer streams): measured about a quarter of a core (1.5 % of a 16-thread PC) per
/// 1080p30 stream.
pub const MAX_WATCHED: usize = 4;

/// How long a watched stream may stay off screen before its video is paused at the SFU. A quick
/// look at the chat and back keeps it running (no keyframe request, no black tile on return).
pub const HIDDEN_GRACE: Duration = Duration::from_millis(1500);

/// [`VoiceSession::watch`] refused a stream because [`MAX_WATCHED`] are already being watched;
/// reachable with `err.downcast_ref::<WatchLimit>()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchLimit {
    pub max: usize,
}

impl std::fmt::Display for WatchLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "already watching {} streams (the maximum)", self.max)
    }
}

impl std::error::Error for WatchLimit {}

/// Where a failed join died.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinStep {
    /// The LiveKit signal connection (TCP/WebSocket to the signal port, or the token).
    Signal,
    /// The media connection (ICE/DTLS to the RTC ports) after the signal link worked.
    Media,
}

impl std::fmt::Display for JoinStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            JoinStep::Signal => "signal connection",
            JoinStep::Media => "media (ICE) connection",
        })
    }
}

/// Error of [`VoiceSession::connect`]; reachable with `err.downcast_ref::<JoinError>()`. `{:#}`
/// prints the full chain.
#[derive(Debug)]
pub struct JoinError {
    pub step: JoinStep,
    pub elapsed: Duration,
    pub source: anyhow::Error,
}

impl std::fmt::Display for JoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "voice join failed at the {} after {} ms", self.step, self.elapsed.as_millis())
    }
}

impl std::error::Error for JoinError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// What happened to the optional parts of a join that succeeded.
#[derive(Debug, Clone, Default)]
pub struct JoinReport {
    /// The microphone was wanted but could not be used (raw reason); the session is listen-only.
    pub mic_error: Option<String>,
    /// The speakers were wanted but could not be used (raw reason); the session is silent.
    pub playback_error: Option<String>,
    pub total: Duration,
}

/// Log a failed join with its full error chain and, when the media link is the suspect, whether
/// the usual ICE-TCP port (signal port + 1 in the default config) accepts a TCP connection from
/// here. Returns the error to hand to the caller.
async fn join_failed(step: JoinStep, started: Instant, source: anyhow::Error, url: &str) -> anyhow::Error {
    let elapsed = started.elapsed();
    // Over TLS the URL is the hub's port, so "its port + 1" says nothing about LiveKit's ICE-TCP.
    let tcp_probe = if step == JoinStep::Media && !url.starts_with("wss:") {
        match probe_tcp(url, 1, SIGNAL_PROBE_TIMEOUT).await {
            Ok(ms) => format!("reachable in {ms} ms"),
            Err(e) => format!("unreachable ({e:#})"),
        }
    } else {
        "not probed".to_string()
    };
    warn!(
        step = %step,
        elapsed_ms = elapsed.as_millis() as u64,
        ice_tcp_port_probe = %tcp_probe,
        error = %format!("{source:#}"),
        "voice join: FAILED"
    );
    anyhow::Error::new(JoinError { step, elapsed, source })
}

/// TCP connect to the host of `url` at its port plus `port_offset`; returns the time it took.
async fn probe_tcp(url: &str, port_offset: u16, limit: Duration) -> Result<u64> {
    let u = Url::parse(url).with_context(|| format!("invalid LiveKit url {url:?}"))?;
    let host = u.host_str().ok_or_else(|| anyhow!("LiveKit url {url:?} has no host"))?.trim_matches(|c| c == '[' || c == ']').to_string();
    let port = u.port_or_known_default().ok_or_else(|| anyhow!("LiveKit url {url:?} has no port"))?.saturating_add(port_offset);
    let started = Instant::now();
    match tokio::time::timeout(limit, tokio::net::TcpStream::connect((host.as_str(), port))).await {
        Ok(Ok(_)) => Ok(started.elapsed().as_millis() as u64),
        Ok(Err(e)) => Err(anyhow!("tcp connect to {host}:{port} failed: {e}")),
        Err(_) => Err(anyhow!("tcp connect to {host}:{port} timed out after {} s", limit.as_secs())),
    }
}

/// One info line with the ICE candidate pair in use, e.g. `udp/host -> udp/srflx` or
/// `tcp/host -> tcp/host (TCP fallback)`, plus the round-trip time. Best effort and bounded.
async fn log_selected_pair(room: &Room, when: &str) {
    let stats = match tokio::time::timeout(STATS_TIMEOUT, room.get_stats()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            debug!(error = %e, "ICE stats unavailable");
            return;
        }
        Err(_) => {
            debug!("ICE stats timed out");
            return;
        }
    };
    let all: Vec<&RtcStats> = stats.publisher_stats.iter().chain(stats.subscriber_stats.iter()).collect();
    let selected_id = all.iter().find_map(|s| match s {
        RtcStats::Transport(t) if !t.transport.selected_candidate_pair_id.is_empty() => Some(t.transport.selected_candidate_pair_id.clone()),
        _ => None,
    });
    let pair = all.iter().find_map(|s| match s {
        RtcStats::CandidatePair(p) if Some(&p.rtc.id) == selected_id.as_ref() => Some(p),
        // No transport entry: fall back to the nominated, succeeded pair.
        RtcStats::CandidatePair(p) if selected_id.is_none() && p.candidate_pair.nominated && p.candidate_pair.state == Some(IceCandidatePairState::Succeeded) => Some(p),
        _ => None,
    });
    let Some(pair) = pair else {
        info!(when, "ICE: no selected candidate pair reported");
        return;
    };
    let describe = |id: &str, local: bool| -> String {
        all.iter()
            .find_map(|s| match s {
                RtcStats::LocalCandidate(c) if local && c.rtc.id == id => Some(&c.local_candidate),
                RtcStats::RemoteCandidate(c) if !local && c.rtc.id == id => Some(&c.remote_candidate),
                _ => None,
            })
            .map(|c| {
                let kind = c.candidate_type.map(|t| format!("{t:?}").to_lowercase()).unwrap_or_else(|| "?".into());
                let tcp = c.tcp_type.map(|t| format!("/{}", format!("{t:?}").to_lowercase())).unwrap_or_default();
                format!("{}/{kind}{tcp} {}:{}", c.protocol, c.address, c.port)
            })
            .unwrap_or_else(|| "?".into())
    };
    let local = describe(&pair.candidate_pair.local_candidate_id, true);
    let remote = describe(&pair.candidate_pair.remote_candidate_id, false);
    let tcp = local.starts_with("tcp") || remote.starts_with("tcp");
    info!(
        when,
        local = %local,
        remote = %remote,
        rtt_ms = (pair.candidate_pair.current_round_trip_time * 1000.0).round() as u64,
        tcp_fallback = tcp,
        "ICE: selected candidate pair"
    );
}

/// Aborts the task when dropped, so a cancelled join does not leave a room connection running.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Closes the room of a join that was dropped before it finished (see `VoiceSession::connect`).
struct CloseOnDrop(Option<Arc<Inner>>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        let Some(inner) = self.0.take() else { return };
        info!("voice join dropped before it finished; closing the room");
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let _ = inner.room.close().await;
            });
        }
    }
}

struct MicPublished {
    publication: LocalTrackPublication,
    track: LocalAudioTrack,
    counters: Arc<StreamCounters>,
}

struct RemoteAudioSub {
    identity: String,
    source: TrackSource,
    publication: RemoteTrackPublication,
    track: RemoteAudioTrack,
    mixer_id: SourceId,
    counters: Arc<StreamCounters>,
    task: JoinHandle<()>,
    mixer: Arc<Mixer>,
}

impl Drop for RemoteAudioSub {
    fn drop(&mut self) {
        self.task.abort();
        self.mixer.remove_source(self.mixer_id);
    }
}

struct RemoteVideoSub {
    identity: String,
    track: RemoteVideoTrack,
    publication: RemoteTrackPublication,
    counters: Arc<StreamCounters>,
    task: JoinHandle<()>,
}

impl Drop for RemoteVideoSub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// One watched stream (item 9: there can be several).
struct Watched {
    /// Where its frames go (the viewer surface, inline or in its own window).
    view: Option<ViewerStream>,
    /// Its subscribed screenshare video, once the SFU sent it.
    video: Option<RemoteVideoSub>,
    /// The video is forwarded and decoded (false once it stayed hidden for [`HIDDEN_GRACE`]).
    visible: bool,
    /// What the app last said: on screen somewhere or not.
    wanted: bool,
    /// Bumped on every visibility change, so a pending pause is cancelled by a later show.
    visible_gen: u64,
}

impl Watched {
    /// Forward / decode the video only while it is visible (no-op until subscribed).
    fn apply_enabled(&self) {
        if let Some(v) = &self.video {
            if v.publication.is_enabled() != self.visible {
                v.publication.set_enabled(self.visible);
            }
        }
    }
}

/// Viewer key of a participant's camera tile.
pub fn camera_key(identity: &str) -> String {
    format!("cam:{identity}")
}

/// Viewer key of our own camera preview (mirrored, never sent through the network).
pub const SELF_CAMERA_KEY: &str = "cam:self";

/// Our camera: the running one, or the muted publication kept for the next start.
#[derive(Default)]
struct CameraSlot {
    active: Option<ActiveCamera>,
    idle: Option<CameraPublication>,
}

/// What a camera start reports.
#[derive(Debug, Clone)]
pub struct CameraStatus {
    /// `"nvenc"` / `"mf"` (hardware, pre-encoded) or `"software"`.
    pub backend: String,
    pub format: vox_capture::CameraFormat,
}

/// Our Go Live state: the running share, or the muted publication kept for the next one.
#[derive(Default)]
struct ShareSlot {
    active: Option<ActiveShare>,
    idle: Option<SharePublication>,
}

struct Inner {
    room: Room,
    identity: String,
    muted: Arc<AtomicBool>,
    deafened: Arc<AtomicBool>,
    muted_before_deafen: AtomicBool,
    /// The microphone was wanted but is not available: the session stays muted until one shows
    /// up ([`publish_late_mic`]).
    mic_unavailable: AtomicBool,
    /// What a microphone that shows up after the join is published with.
    mic_late: Mutex<Option<LateMic>>,
    volume: Arc<AtomicU32>,
    mixer: Arc<Mixer>,
    /// No output device was asked for (`VoiceConfig::playback == false`): somebody pulls the mix
    /// by hand ([`VoiceSession::pull_playback_block`]), so soundboard clips may be queued.
    headless: bool,
    gate: Arc<VoiceGate>,
    prefs: AudioPrefs,
    mic: Mutex<Option<MicPublished>>,
    audio_io: Mutex<Option<AudioIo>>,
    mic_tone: Mutex<Option<ProducerThread>>,
    share: tokio::sync::Mutex<ShareSlot>,
    share_watchdog: Mutex<Option<JoinHandle<()>>>,
    /// Streams we are watching, by streamer identity (at most [`MAX_WATCHED`]).
    watched: Mutex<HashMap<String, Watched>>,
    /// Participants (identities) whose live is audio-only; see [`VoiceEvent::ShareKind`].
    audio_only: Mutex<HashSet<String>>,
    remote_audio: Mutex<HashMap<TrackSid, RemoteAudioSub>>,
    /// The one microphone publication subscribed per participant (identity -> sid).
    mic_picks: Mutex<HashMap<String, TrackSid>>,
    camera: tokio::sync::Mutex<CameraSlot>,
    camera_watchdog: Mutex<Option<JoinHandle<()>>>,
    /// The encoder safety net ([`encoder_guard`]), running while we share or the camera is on.
    encoder_guard: Mutex<Option<JoinHandle<()>>>,
    /// What our viewers reported lately about how our share and camera decode (data channel,
    /// [`encoders::RX_HEALTH_TOPIC`]); the safety net only demotes on their word.
    rx_reports: Mutex<encoders::ViewerReports>,
    /// The viewer side of it ([`rx_health_guard`]): reports bad decoding of what we watch.
    rx_health: Mutex<Option<JoinHandle<()>>>,
    /// Frames of our own camera preview (stats).
    self_view_counters: Arc<StreamCounters>,
    /// What the app wants of the preview of our own Go Live (item 8a); applied to every share
    /// we start and to the running one.
    share_preview_cfg: Mutex<vox_capture::PreviewConfig>,
    /// Frames of our own Go Live preview (stats).
    share_preview_counters: Arc<StreamCounters>,
    /// Camera tiles are on screen (call view shown, app not minimized).
    cams_visible: AtomicBool,
    /// People whose camera I chose to hide ("Ocultar câmera para mim"): paused at the SFU like
    /// a hidden tile, whatever the call view shows.
    cams_hidden: Mutex<std::collections::HashSet<String>>,
    /// Subscribed remote cameras, by identity.
    remote_cams: Mutex<HashMap<String, RemoteVideoSub>>,
    /// Identities whose camera is on (drives `CameraStarted` / `CameraStopped`).
    cams_live: Mutex<HashSet<String>>,
    events: mpsc::UnboundedSender<VoiceEvent>,
    viewer_tx: mpsc::UnboundedSender<ViewerNotice>,
    /// Item 12: the microphone while the server does not accept it.
    server_mute: ServerMute,
    /// Who is speaking (v1.1): sends [`VoiceEvent::ActiveSpeakers`] when it changes.
    speakers: Arc<Speakers>,
    /// v1.1 (E1): the key exchange of an encrypted room (`None`: a plain room, or left).
    e2ee: Mutex<Option<crate::media_e2ee::Engine>>,
    /// Its messages, sent in order by one task.
    e2ee_tx: Option<mpsc::UnboundedSender<(Vec<String>, Vec<u8>)>>,
    /// The key index our own cryptors use (checked against new publications).
    e2ee_index: std::sync::atomic::AtomicU8,
    e2ee_tick: Mutex<Option<JoinHandle<()>>>,
    /// The SFU's active speakers (another person) arrived in this encrypted room (logged once).
    e2ee_sfu_speakers: AtomicBool,
}

/// Takes still frames of a session's running Go Live (v1.1 hover pictures); it does not keep the
/// session alive.
#[derive(Clone)]
pub struct ShareSnapshots(std::sync::Weak<Inner>);

impl ShareSnapshots {
    /// One frame of the running share, taken from the capture's preview tap (no second capture),
    /// within `timeout`. `None` when the session is gone, nothing with a picture is shared, or no
    /// frame came (the tap's settings are put back then).
    pub async fn take(&self, timeout: Duration) -> Option<crate::golive::thumbnail::Nv12Image> {
        let p = {
            let inner = self.0.upgrade()?;
            // (The share's lock is not held while waiting: stopping or switching never waits here.)
            let slot = inner.share.lock().await;
            slot.active.as_ref()?.preview_handle()?
        };
        p.request_snapshot();
        let until = Instant::now() + timeout;
        loop {
            if let Some(f) = p.take_snapshot() {
                return Some(f);
            }
            if Instant::now() >= until {
                p.cancel_snapshot();
                return None;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }
}

/// A joined voice channel. Call [`VoiceSession::leave`] to disconnect cleanly.
pub struct VoiceSession {
    inner: Arc<Inner>,
    event_task: JoinHandle<()>,
    report: JoinReport,
    /// v1.1 (T0): the local pinned-TLS tunnel to the hub's port, for the whole session (the SDK
    /// reconnects through it); closed with the session.
    _tunnel: Option<crate::tls::Tunnel>,
}

/// Which of one participant's microphone publications (`sids`) to play: the one already picked
/// while it is still published, otherwise the first by sid. Index into `sids`.
fn pick_mic(current: Option<&str>, sids: &[String]) -> Option<usize> {
    if let Some(i) = current.and_then(|c| sids.iter().position(|s| s == c)) {
        return Some(i);
    }
    sids.iter().enumerate().min_by(|a, b| a.1.cmp(b.1)).map(|(i, _)| i)
}

fn ident(p: &RemoteParticipant) -> String {
    p.identity().0.clone()
}

impl VoiceSession {
    /// Connect to a LiveKit room (`url`/`token` from `VoiceJoinResponse` or minted locally).
    ///
    /// Never waits forever: the signal probe, the room connect (spawned, so even a wedged native
    /// call cannot stall the caller), the audio devices and the mic publish all have limits.
    /// Failures of the signal/media connection come back as a [`JoinError`]; a missing
    /// microphone or speaker does not fail the join, it is reported in [`VoiceSession::join_report`].
    /// Every step is logged with its timing, and a failure logs the full error chain.
    pub async fn connect(url: &str, token: &str, cfg: VoiceConfig) -> Result<(VoiceSession, mpsc::UnboundedReceiver<VoiceEvent>)> {
        let t0 = Instant::now();
        info!(url, mic = cfg.mic, playback = cfg.playback, tls = cfg.tls_pin.is_some(), "voice join: start");

        // ---- step 0 (v1.1): over TLS the signalling goes through the hub's port, in pinned TLS
        // from a local tunnel the SDK talks plain WebSocket to.
        // Audit (T0): a session pinned to the hub's certificate never sends its LiveKit token in
        // the clear: a `ws://` (or any other) URL from the hub is refused before any connection
        // (rule 5, "once there is a pin, never plain"). A `wss://` one goes only through the
        // pinned tunnel, so whatever host it names, only the hub's certificate can answer it.
        let scheme = url::Url::parse(url).map(|u| u.scheme().to_string()).unwrap_or_default();
        if cfg.tls_pin.is_some() && scheme != "wss" {
            return Err(join_failed(JoinStep::Signal, t0, anyhow!("the hub is reached over TLS but gave a voice server URL that is not wss:// ({scheme}): refused, the voice token is not sent in the clear"), url).await);
        }
        let tunnel = if scheme == "wss" {
            let Some(pin) = cfg.tls_pin else {
                return Err(join_failed(JoinStep::Signal, t0, anyhow!("a wss:// voice server needs the hub's certificate pin"), url).await);
            };
            match crate::tls::Tunnel::open(url, pin).await {
                Ok(t) => Some(t),
                Err(e) => return Err(join_failed(JoinStep::Signal, t0, e, url).await),
            }
        } else {
            None
        };
        let sdk_url = tunnel.as_ref().map_or_else(|| url.to_string(), |t| t.url());

        // ---- step 1: can we even reach the signal port? (The real one: the hub's, over TLS.)
        let signal_ms = match probe_tcp(url, 0, SIGNAL_PROBE_TIMEOUT).await {
            Ok(ms) => {
                info!(signal_ms = ms, "voice join: signal port reachable (tcp)");
                ms
            }
            Err(e) => return Err(join_failed(JoinStep::Signal, t0, e.context("LiveKit signal port unreachable"), url).await),
        };

        // ---- step 2: signal WebSocket + ICE/DTLS, in a task so a native stall cannot block us
        let mut options = RoomOptions::default();
        // v1.1 (E1): frame encryption with per-sender keys. The deprecated `e2ee` field on purpose:
        // `encryption` would also encrypt the data channel under one global key index (which our
        // per-sender keys overwrite), breaking the key exchange and rx-health on it. Data packets
        // stay as they are; media is AES-GCM per sender, keys from `media_e2ee`.
        if cfg.e2ee.is_some() {
            let key_provider = livekit::e2ee::key_provider::KeyProvider::new(livekit::e2ee::key_provider::KeyProviderOptions {
                // Random keys: ratcheting forward can never find a working one.
                ratchet_window_size: 0,
                ratchet_salt: vox_e2ee::media::RATCHET_SALT.to_vec(),
                failure_tolerance: -1,
                key_ring_size: vox_e2ee::media::KEY_RING as i32,
                key_derivation_algorithm: livekit::e2ee::key_provider::KeyDerivationAlgorithm::HKDF,
            });
            #[allow(deprecated)]
            {
                options.e2ee = Some(livekit::e2ee::E2eeOptions { encryption_type: livekit::e2ee::EncryptionType::Gcm, key_provider });
            }
        }
        options.auto_subscribe = false;
        options.adaptive_stream = false;
        options.dynacast = false;
        // No SDK-internal retries: they would hide the real error behind a longer wait. The user
        // gets a retry button instead.
        options.join_retries = 0;
        let (u, t) = (sdk_url, token.to_string());
        let mut task = AbortOnDrop(tokio::spawn(async move { Room::connect(&u, &t, options).await }));
        let connected = match tokio::time::timeout(CONNECT_TIMEOUT, &mut task.0).await {
            Ok(Ok(Ok(c))) => c,
            Ok(Ok(Err(e))) => {
                let step = if e.to_string().contains("signal failure") { JoinStep::Signal } else { JoinStep::Media };
                let e = anyhow::Error::new(e).context(format!("connect to LiveKit at {url}"));
                return Err(join_failed(step, t0, e, url).await);
            }
            Ok(Err(e)) => return Err(join_failed(JoinStep::Media, t0, anyhow!("connect task failed: {e}"), url).await),
            Err(_) => {
                drop(task);
                let e = anyhow!("no answer within {} s", CONNECT_TIMEOUT.as_secs());
                return Err(join_failed(JoinStep::Media, t0, e, url).await);
            }
        };
        let (room, room_rx) = connected;
        let room_ms = t0.elapsed().as_millis() as u64;
        let identity = room.local_participant().identity().0.clone();
        let room_name = room.name();
        info!(room = %room_name, identity = %identity, url, signal_ms, room_ms, e2ee = cfg.e2ee.is_some(), "voice join: room connected");

        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (viewer_tx, viewer_rx) = mpsc::unbounded_channel();
        let mixer = Arc::new(Mixer::new());
        let prefs = AudioPrefs::new();
        for (who, a) in &cfg.user_audio {
            prefs.set_user_audio(who, a.sanitized());
        }
        prefs.set_others_muted(cfg.others_muted);
        let speakers = {
            let tx = ev_tx.clone();
            Speakers::new(&identity, move |identities| {
                let _ = tx.send(VoiceEvent::ActiveSpeakers { identities });
            })
        };
        let (e2ee_send, e2ee_recv) = match cfg.e2ee {
            Some(_) => {
                let (t, r) = mpsc::unbounded_channel();
                (Some(t), Some(r))
            }
            None => (None, None),
        };
        let inner = Arc::new(Inner {
            room,
            identity: identity.clone(),
            muted: Arc::new(AtomicBool::new(cfg.start_muted)),
            deafened: Arc::new(AtomicBool::new(false)),
            muted_before_deafen: AtomicBool::new(false),
            mic_unavailable: AtomicBool::new(false),
            mic_late: Mutex::new(None),
            volume: Arc::new(AtomicU32::new(cfg.output_volume.unwrap_or(1.0).clamp(0.0, 2.0).to_bits())),
            mixer: mixer.clone(),
            headless: !cfg.playback,
            gate: cfg.gate.clone().unwrap_or_default(),
            prefs,
            mic: Mutex::new(None),
            audio_io: Mutex::new(None),
            mic_tone: Mutex::new(None),
            share: tokio::sync::Mutex::new(ShareSlot::default()),
            share_watchdog: Mutex::new(None),
            watched: Mutex::new(HashMap::new()),
            audio_only: Mutex::new(HashSet::new()),
            remote_audio: Mutex::new(HashMap::new()),
            mic_picks: Mutex::new(HashMap::new()),
            camera: tokio::sync::Mutex::new(CameraSlot::default()),
            camera_watchdog: Mutex::new(None),
            encoder_guard: Mutex::new(None),
            rx_reports: Mutex::new(encoders::ViewerReports::default()),
            rx_health: Mutex::new(None),
            self_view_counters: Arc::new(StreamCounters::default()),
            share_preview_cfg: Mutex::new(vox_capture::PreviewConfig::default()),
            share_preview_counters: Arc::new(StreamCounters::default()),
            cams_visible: AtomicBool::new(true),
            cams_hidden: Mutex::new(Default::default()),
            remote_cams: Mutex::new(HashMap::new()),
            cams_live: Mutex::new(HashSet::new()),
            events: ev_tx,
            speakers,
            viewer_tx,
            server_mute: ServerMute::default(),
            e2ee: Mutex::new(cfg.e2ee.as_ref().map(|c| crate::media_e2ee::Engine::new(&room_name, &identity, c))),
            e2ee_tx: e2ee_send,
            e2ee_index: std::sync::atomic::AtomicU8::new(0),
            e2ee_tick: Mutex::new(None),
            e2ee_sfu_speakers: AtomicBool::new(false),
        });
        // v1.1 (E1): my first media key (index 0, where new sender cryptors start) before anything
        // is published, and a Hello to everybody present.
        start_e2ee(&inner, e2ee_recv);
        // From here on, dropping the in-flight join (the user cancelled) closes the room instead of
        // leaving a ghost participant behind.
        let mut guard = CloseOnDrop(Some(inner.clone()));
        log_selected_pair(&inner.room, "connected").await;
        let backends: Vec<_> = livekit::options::VideoEncoderBackend::list_available().into_iter().collect();
        info!(?backends, "available video encoder backends");
        let mut report = JoinReport::default();

        // ---- step 3: audio devices. Neither a missing mic nor missing speakers may stop the join.
        let noise = cfg.noise.clone().unwrap_or_default();
        let apm = cfg.mic.then(|| new_voice_apm(&noise));
        // The SDK's AudioSourceOptions are not applied to NativeAudioSource frames (they
        // bypass WebRTC's ADM/APM path), so AEC/NS/AGC run in our own WebRTC APM instance
        // (see audio::device) with the playback mix as far-end reference.
        let mic_source = cfg.mic.then(|| NativeAudioSource::new(AudioSourceOptions::default(), MIX_RATE, 1, 0));
        let counters = Arc::new(StreamCounters::default());
        let (mic_status_tx, mic_status_rx) = mpsc::unbounded_channel::<MicStatus>();
        let mic_cfg = match (&mic_source, cfg.mic_test_tone) {
            (Some(src), None) => Some(MicConfig {
                device: cfg.input_device.clone(),
                sink: MicSink::Source(src.clone()),
                muted: inner.muted.clone(),
                apm: apm.clone(),
                counters: counters.clone(),
                gate: inner.gate.clone(),
                noise: noise.clone(),
                fall_back: true,
                on_status: Some(Arc::new(move |s| {
                    let _ = mic_status_tx.send(s);
                })),
                fake: cfg.fake_mics.clone(),
            }),
            _ => None,
        };
        let make_playback = || {
            cfg.playback.then(|| PlaybackConfig {
                device: cfg.output_device.clone(),
                mixer: mixer.clone(),
                deafened: inner.deafened.clone(),
                volume: inner.volume.clone(),
                apm: apm.clone(),
            })
        };
        let audio_t = Instant::now();
        let mut io = None;
        let (want_mic_dev, want_out_dev) = (mic_cfg.is_some(), cfg.playback);
        if want_mic_dev || want_out_dev {
            let playback = make_playback();
            match tokio::task::spawn_blocking(move || AudioIo::start(mic_cfg, playback)).await {
                Ok(started) => io = Some(started),
                Err(e) => {
                    let why = format!("audio thread failed: {e}");
                    warn!(error = %why, "voice join: audio devices unavailable");
                    if want_mic_dev {
                        report.mic_error = Some(why.clone());
                    }
                    if want_out_dev {
                        report.playback_error = Some(why);
                    }
                }
            }
        }
        if let Some(io) = &io {
            if want_mic_dev && !io.mic_ok {
                report.mic_error = Some(io.mic_error.clone().unwrap_or_else(|| "unknown error".into()));
            }
            if want_out_dev && !io.playback_ok {
                report.playback_error = Some(io.playback_error.clone().unwrap_or_else(|| "unknown error".into()));
            }
        }
        let audio_ms = audio_t.elapsed().as_millis() as u64;
        info!(
            audio_ms,
            mic_ok = want_mic_dev && report.mic_error.is_none(),
            playback_ok = want_out_dev && report.playback_error.is_none(),
            "voice join: audio devices"
        );

        // ---- step 4: publish the microphone track (only when there is a mic to feed it)
        let mic_t = Instant::now();
        // Server-muted (item 12): the token does not allow a microphone. Keep it ready instead of
        // waiting for a publish the server refuses; it is published when the permission returns.
        let mic_held = mic_source.is_some() && report.mic_error.is_none() && !mic_allowed(&inner.room.local_participant());
        if let (true, Some(src)) = (mic_held, &mic_source) {
            let track = LocalAudioTrack::create_audio_track("microphone", RtcAudioSource::Native(src.clone()));
            if let Some(hz) = cfg.mic_test_tone {
                *inner.mic_tone.lock() = Some(spawn_mic_tone(hz, src.clone(), inner.muted.clone(), counters.clone()));
            }
            inner.server_mute.hold(track, counters.clone());
            let _ = inner.events.send(VoiceEvent::ServerMuted { muted: true });
            info!("voice join: server-muted, microphone held back");
        }
        if let (Some(src), None, false) = (&mic_source, &report.mic_error, mic_held) {
            let track = LocalAudioTrack::create_audio_track("microphone", RtcAudioSource::Native(src.clone()));
            let publish = mic_publish_options();
            let local = inner.room.local_participant();
            let publishing = local.publish_track(LocalTrack::Audio(track.clone()), publish);
            let published = match tokio::time::timeout(PUBLISH_TIMEOUT, publishing).await {
                Ok(Ok(p)) => Ok(p),
                Ok(Err(e)) => Err(format!("publish microphone: {:#}", anyhow::Error::new(e))),
                Err(_) => Err(format!("publish microphone: no answer within {} s", PUBLISH_TIMEOUT.as_secs())),
            };
            match published {
                Ok(publication) => {
                    if let Some(hz) = cfg.mic_test_tone {
                        *inner.mic_tone.lock() = Some(spawn_mic_tone(hz, src.clone(), inner.muted.clone(), counters.clone()));
                        info!(hz, "microphone replaced by test tone");
                    }
                    if cfg.start_muted {
                        publication.mute();
                    }
                    *inner.mic.lock() = Some(MicPublished { publication, track, counters: counters.clone() });
                    info!(mic_ms = mic_t.elapsed().as_millis() as u64, "voice join: microphone published");
                }
                Err(why) => {
                    warn!(error = %why, "voice join: microphone not published; listening only");
                    report.mic_error = Some(why);
                    // Stop capturing for a track nobody listens to (and release the mic indicator).
                    if let Some(old) = io.take() {
                        let playback = make_playback();
                        let restarted = tokio::task::spawn_blocking(move || {
                            drop(old);
                            AudioIo::start(None, playback)
                        })
                        .await;
                        match restarted {
                            Ok(new_io) => {
                                if want_out_dev && !new_io.playback_ok && report.playback_error.is_none() {
                                    report.playback_error = Some(new_io.playback_error.clone().unwrap_or_else(|| "unknown error".into()));
                                }
                                io = Some(new_io);
                            }
                            Err(e) => warn!(error = %e, "could not reopen playback after the mic failure"),
                        }
                    }
                }
            }
        }
        if report.mic_error.is_some() {
            // Listen-only: stay muted so the others see the truth. The audio thread keeps looking
            // for a microphone; one that opens is published then.
            inner.mic_unavailable.store(true, Ordering::Relaxed);
            inner.muted.store(true, Ordering::Relaxed);
            if let Some(source) = mic_source.clone() {
                *inner.mic_late.lock() = Some(LateMic { source, counters: counters.clone(), start_muted: cfg.start_muted });
            }
        }
        *inner.audio_io.lock() = io;
        report.total = t0.elapsed();
        info!(
            signal_ms,
            room_ms,
            audio_ms,
            total_ms = report.total.as_millis() as u64,
            mic_error = report.mic_error.as_deref().unwrap_or("none"),
            playback_error = report.playback_error.as_deref().unwrap_or("none"),
            "voice join: done"
        );

        // ---- subscribe to microphones already in the room
        let mut participants = Vec::new();
        for p in inner.room.remote_participants().values() {
            participants.push(ident(p));
            for publication in p.track_publications().values() {
                inner.on_publication(publication, p);
            }
        }
        let _ = inner.events.send(VoiceEvent::Connected { room: room_name, identity, participants });

        let event_task = tokio::spawn(run_events(inner.clone(), room_rx, viewer_rx, mic_status_rx));
        *inner.rx_health.lock() = Some(tokio::spawn(rx_health_guard(Arc::downgrade(&inner))));
        guard.0 = None; // the session owns the room now
        Ok((VoiceSession { inner, event_task, report, _tunnel: tunnel }, ev_rx))
    }

    /// What happened to the optional parts of the join (mic, speakers).
    pub fn join_report(&self) -> &JoinReport {
        &self.report
    }

    pub fn identity(&self) -> &str {
        &self.inner.identity
    }

    pub fn room_name(&self) -> String {
        self.inner.room.name()
    }

    /// v1.1 (E1): this session's media is end-to-end encrypted.
    pub fn is_e2ee(&self) -> bool {
        self.inner.e2ee_tx.is_some()
    }

    pub fn is_muted(&self) -> bool {
        self.inner.muted.load(Ordering::Relaxed)
    }

    pub fn is_deafened(&self) -> bool {
        self.inner.deafened.load(Ordering::Relaxed)
    }

    /// The input gate and level meter of this session (see [`VoiceGate`]). Settings changes on
    /// it apply immediately, mid-call.
    pub fn gate(&self) -> &Arc<VoiceGate> {
        &self.inner.gate
    }

    /// The microphone was wanted but could not be used: the session is listen-only and muted.
    pub fn mic_unavailable(&self) -> bool {
        self.inner.mic_unavailable.load(Ordering::Relaxed)
    }

    /// A moderator server-muted us (item 12): the voice server does not accept our microphone,
    /// whatever our own mute says.
    pub fn server_muted(&self) -> bool {
        self.inner.server_mute.on.load(Ordering::Relaxed)
    }

    pub fn has_mic(&self) -> bool {
        self.inner.mic.lock().is_some()
    }

    /// Capture from another microphone from now on (`None`: the system default): the old one
    /// stops at once and the track goes on, nobody has to rejoin. Does not wait for the device;
    /// the outcome comes as [`VoiceEvent::Mic`].
    pub fn set_input_device(&self, device: Option<String>) {
        if let Some(io) = self.inner.audio_io.lock().as_ref() {
            io.set_mic_device(device);
        }
    }

    /// Mute stops sending immediately (callback early-outs) but keeps the device open.
    pub fn set_muted(&self, muted: bool) {
        self.inner.set_muted(muted);
    }

    /// Deafen silences playback, pauses remote audio forwarding (saves bandwidth) and mutes.
    /// Undeafen restores the previous mute state.
    pub fn set_deafened(&self, deafened: bool) {
        let inner = &self.inner;
        if inner.deafened.swap(deafened, Ordering::Relaxed) == deafened {
            return;
        }
        if deafened {
            inner.muted_before_deafen.store(inner.muted.load(Ordering::Relaxed), Ordering::Relaxed);
            inner.set_muted(true);
        } else {
            inner.set_muted(inner.muted_before_deafen.load(Ordering::Relaxed));
        }
        for sub in inner.remote_audio.lock().values() {
            sub.publication.set_enabled(!deafened);
        }
        // The paused tracks no longer tell who speaks: LiveKit's speakers do meanwhile.
        inner.speakers.set_deafened(deafened);
        info!(deafened, "deafen toggled");
    }

    /// Master output volume 0.0..=2.0 (1.0 = unity).
    pub fn set_output_volume(&self, volume: f32) {
        self.inner.volume.store(volume.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }

    pub fn output_volume(&self) -> f32 {
        f32::from_bits(self.inner.volume.load(Ordering::Relaxed))
    }

    // ---- local listening controls: they only change what this client plays

    /// Volume of `identity`'s microphone for us: `1.0` = 100 %, clamped to 0..=2.0. Kept across
    /// the person leaving and rejoining.
    pub fn set_user_volume(&self, identity: &str, gain: f32) {
        self.inner.prefs.set_user_volume(identity, gain);
    }

    /// Silence `identity`'s microphone for us only (their volume setting is kept).
    pub fn set_user_muted(&self, identity: &str, muted: bool) {
        self.inner.prefs.set_user_muted(identity, muted);
    }

    /// Volume of `identity`'s screenshare audio, separate from their microphone.
    pub fn set_stream_volume(&self, identity: &str, gain: f32) {
        self.inner.prefs.set_stream_volume(identity, gain);
    }

    pub fn set_stream_muted(&self, identity: &str, muted: bool) {
        self.inner.prefs.set_stream_muted(identity, muted);
    }

    /// Apply a whole saved snapshot for `identity`.
    pub fn set_user_audio(&self, identity: &str, audio: UserAudio) {
        self.inner.prefs.set_user_audio(identity, audio.sanitized());
    }

    pub fn user_audio(&self, identity: &str) -> UserAudio {
        self.inner.prefs.user_audio(identity)
    }

    /// "Mute everyone": silence all other participants' microphones locally. Independent of
    /// deafen (our own mic is untouched) and of the per-person settings, which come back when
    /// this is turned off. Screenshare audio has its own volume / mute.
    pub fn set_others_muted(&self, muted: bool) {
        self.inner.prefs.set_others_muted(muted);
    }

    pub fn others_muted(&self) -> bool {
        self.inner.prefs.others_muted()
    }

    /// Headless diagnostics: pull one 10 ms block of the playback mix (gains applied), exactly
    /// as the speaker callback would. Only meaningful when no output device was opened
    /// (`VoiceConfig::playback == false`); with one open, the device callback owns the mix.
    pub fn pull_playback_block(&self, out: &mut [i16]) {
        self.inner.mixer.mix(out);
        self.inner.mixer.mix_ui(out);
    }

    /// Play a local alert sound (only we hear it) through this session's output: it is mixed
    /// with the voices, goes to the selected speakers and into the echo canceller's reference.
    /// Deafen does not silence it. `gain` is the alert volume (1.0 = the sound's own level).
    /// Returns false when the session has no open output device or too many alerts are playing.
    pub fn play_alert(&self, kind: AlertKind, gain: f32) -> bool {
        if !self.has_playback() {
            return false;
        }
        self.inner.mixer.play_ui(kind.samples(), gain)
    }

    /// Play a soundboard clip (only we hear it): it is mixed with the voices, so it goes to the
    /// selected speakers and into the echo canceller's reference, and it follows the soundboard
    /// volume and mute ([`VoiceSession::set_soundboard`]). Deafened, nothing is played. Returns
    /// false when the clip was not queued: deafened, no open output device, or too many clips at
    /// once.
    pub fn play_sound(&self, clip: &crate::soundboard::SoundPcm) -> bool {
        self.play_sound_by(clip, crate::audio::NO_TAG)
    }

    /// [`VoiceSession::play_sound`] for a sound `by` played (their user id), so
    /// [`VoiceSession::stop_sounds`] can stop it.
    pub fn play_sound_by(&self, clip: &crate::soundboard::SoundPcm, by: SoundTag) -> bool {
        if self.is_deafened() || !(self.has_playback() || self.inner.headless) {
            return false;
        }
        self.inner.mixer.play_sound_by(clip.samples.clone(), clip.channels, by)
    }

    /// Stop (5 ms fade) the soundboard clips `by` played, or all of them (`None`). Returns how
    /// many were playing.
    pub fn stop_sounds(&self, by: Option<SoundTag>) -> usize {
        self.inner.mixer.stop_sounds(by)
    }

    /// Stop (5 ms fade) only the newest playing copy of `clip` that `by` started (a play the hub
    /// refused; an older sound of theirs goes on). Returns whether one was playing.
    pub fn stop_clip(&self, clip: &crate::soundboard::SoundPcm, by: SoundTag) -> bool {
        self.inner.mixer.stop_clip(&clip.samples, by)
    }

    /// How much longer the soundboard clips `by` started play here (`None`: none is playing).
    pub fn sound_left(&self, by: SoundTag) -> Option<std::time::Duration> {
        self.inner.mixer.sound_left(by)
    }

    /// Stop the local previews ("Ouvir só aqui") still playing.
    pub fn stop_previews(&self) -> usize {
        self.inner.mixer.stop_previews()
    }

    /// The soundboard part of the speaking set: `identity` played a sound lasting `length`
    /// (`None`: it was stopped). They count as speaking until then, whether or not this app
    /// plays the clip (muted soundboard, deafened): everyone else in the room hears it.
    pub fn sound_speaking(&self, identity: &str, length: Option<std::time::Duration>) {
        self.inner.speakers.sound(identity, length);
    }

    /// The speaking set as last sent.
    pub fn speaking(&self) -> Vec<String> {
        self.inner.speakers.current()
    }

    /// Hear a clip locally without the room (the "preview" button): like an alert it is not
    /// silenced by deafen and ignores the soundboard mute, but follows `gain`.
    pub fn preview_sound(&self, clip: &crate::soundboard::SoundPcm, gain: f32) -> bool {
        if !(self.has_playback() || self.inner.headless) {
            return false;
        }
        self.inner.mixer.play_preview(clip.to_stereo(), gain)
    }

    /// The soundboard's volume (1.0 = the clip's own level) and personal mute; changes apply to
    /// clips already playing.
    pub fn set_soundboard(&self, volume: f32, muted: bool) {
        let g = self.inner.mixer.sound_gain();
        g.set_level(volume);
        g.set_muted(muted);
    }

    /// The playback mixer, for headless tests that pull the mix themselves.
    pub fn playback_mixer(&self) -> Arc<Mixer> {
        self.inner.mixer.clone()
    }

    /// The speakers are open (alerts and voices can be heard).
    pub fn has_playback(&self) -> bool {
        self.inner.audio_io.lock().as_ref().is_some_and(|io| io.playback_ok)
    }

    pub async fn is_sharing(&self) -> bool {
        self.inner.share.lock().await.active.is_some()
    }

    /// Video path of the running share (pre-encoded or raw + software encoder).
    pub async fn share_path(&self) -> Option<VideoPath> {
        self.inner.share.lock().await.active.as_ref().and_then(|s| s.path)
    }

    /// Snapshots of the running share for its hover picture (v1.1), usable from another task
    /// for as long as this session lives (see [`ShareSnapshots`]).
    pub fn share_snapshots(&self) -> ShareSnapshots {
        ShareSnapshots(Arc::downgrade(&self.inner))
    }

    /// The running share has an audio capture going.
    pub async fn share_has_audio(&self) -> bool {
        self.inner.share.lock().await.active.as_ref().is_some_and(|s| s.has_audio())
    }

    /// Encoder behind the running share: `"nvenc"` / `"mf"` (hardware, pre-encoded path),
    /// `"software"` (OpenH264 on the CPU) or `"h264file"`; `None` when not sharing.
    pub async fn share_encoder(&self) -> Option<String> {
        let slot = self.inner.share.lock().await;
        let share = slot.active.as_ref()?;
        Some(match share.encoder_report() {
            Some(r) => r.backend,
            None => "software".to_string(),
        })
    }

    /// Test hook: make the running share's encoder overshoot (see
    /// [`ActiveShare::set_encoder_overshoot`]). Returns `false` when not sharing.
    pub async fn set_share_encoder_overshoot(&self, pct: Option<u64>) -> bool {
        match self.inner.share.lock().await.active.as_ref() {
            Some(s) => {
                s.set_encoder_overshoot(pct);
                info!(overshoot_pct = pct, "share encoder overshoot test hook");
                true
            }
            None => false,
        }
    }

    /// Test hook: cap the running share's encoder target (see [`ActiveShare::set_bitrate_cap`]).
    /// Returns `false` when not sharing.
    pub async fn set_share_bitrate_cap(&self, bps: Option<u64>) -> bool {
        match self.inner.share.lock().await.active.as_ref() {
            Some(s) => {
                s.set_bitrate_cap(bps);
                info!(cap_kbps = bps.map(|b| b / 1000), "share bitrate test cap");
                true
            }
            None => false,
        }
    }

    /// Start Go Live (replaces a running share).
    pub async fn start_share(&self, source: ShareSource, opts: ShareOptions) -> Result<()> {
        // Unattended tests never capture the person's screen, windows or sound (fails loudly).
        if let Some(why) = crate::golive::publish::test_capture_refusal(&source, opts.audio_pid) {
            tracing::error!("{why}");
            bail!(why);
        }
        let mut slot = self.inner.share.lock().await;
        let reuse = match slot.active.take() {
            Some(old) => Some(old.stop()),
            None => slot.idle.take(),
        };
        // (An audio-only live has no picture, so no preview tile either.)
        let preview = if source.is_audio_only() { None } else { self.inner.open_share_preview() };
        match ActiveShare::start_with_preview(&self.inner.room, source, opts, reuse, preview).await {
            Ok(share) => slot.active = Some(share),
            Err(e) => {
                slot.idle = e.publication.map(|p| *p);
                self.inner.close_share_preview();
                return Err(e.error);
            }
        }
        drop(slot);
        let task = tokio::spawn(share_watchdog(Arc::downgrade(&self.inner)));
        if let Some(old) = self.inner.share_watchdog.lock().replace(task) {
            old.abort();
        }
        self.inner.ensure_encoder_guard();
        Ok(())
    }

    /// Switch the running Go Live to another window / monitor without ending it (same
    /// publication, hub `streaming` untouched). On failure the old source keeps streaming when it
    /// is still alive; the error is Portuguese, ready to show. `Err` also when not sharing.
    pub async fn switch_share(&self, req: SwitchRequest) -> Result<SwitchReport> {
        if let Some(why) = crate::golive::publish::test_capture_refusal(&req.source, req.audio_pid) {
            tracing::error!("{why}");
            bail!(why);
        }
        let mut slot = self.inner.share.lock().await;
        let Some(share) = slot.active.as_mut() else {
            bail!("Você não está transmitindo.");
        };
        share.switch_source(&self.inner.room, req).await
    }

    pub async fn stop_share(&self) -> bool {
        if let Some(t) = self.inner.share_watchdog.lock().take() {
            t.abort();
        }
        let mut slot = self.inner.share.lock().await;
        match slot.active.take() {
            Some(share) => {
                slot.idle = Some(share.stop());
                drop(slot);
                self.inner.close_share_preview();
                schedule_trim(Arc::downgrade(&self.inner));
                true
            }
            None => false,
        }
    }

    /// Set the preview of our own Go Live (item 8a): the tile [`SELF_SHARE_KEY`] shows a small
    /// picture of what we share, taken by the capture itself at `cfg.fps` at most and about
    /// `cfg.max_width` x `cfg.max_height` big. `cfg.on = false` stops it completely (no GPU work,
    /// no frames): use it whenever the tile is not visible. Remembered for later shares; applies
    /// at once to a running one (and follows "Trocar tela").
    pub async fn set_share_preview(&self, cfg: vox_capture::PreviewConfig) {
        let inner = &self.inner;
        {
            let mut c = inner.share_preview_cfg.lock();
            if *c == cfg {
                return;
            }
            *c = cfg;
        }
        if let Some(s) = inner.share.lock().await.active.as_ref() {
            s.set_preview(cfg);
        }
        info!(on = cfg.on, fps = cfg.fps, max = format!("{}x{}", cfg.max_width, cfg.max_height), "share preview settings");
    }

    /// Turn our camera on (replaces a running one, e.g. another device or quality). Independent
    /// of Go Live. The error is Portuguese, ready to show. While the camera tiles are visible,
    /// the self-view (viewer tile [`SELF_CAMERA_KEY`], mirrored) gets small preview frames.
    pub async fn start_camera(&self, opts: CameraOptions) -> Result<CameraStatus> {
        let inner = &self.inner;
        let mut slot = inner.camera.lock().await;
        let reuse = match slot.active.take() {
            Some(old) => Some(tokio::task::spawn_blocking(move || old.stop()).await.unwrap_or_default()),
            None => slot.idle.take(),
        };
        let preview = inner.self_view_factory();
        match ActiveCamera::start(&inner.room, opts, reuse, Some(preview)).await {
            Ok(cam) => {
                cam.set_preview(inner.cams_visible.load(Ordering::Relaxed));
                let status = CameraStatus { backend: cam.backend(), format: cam.format.clone() };
                slot.active = Some(cam);
                drop(slot);
                let task = tokio::spawn(camera_watchdog(Arc::downgrade(inner)));
                if let Some(old) = inner.camera_watchdog.lock().replace(task) {
                    old.abort();
                }
                inner.ensure_encoder_guard();
                Ok(status)
            }
            Err(e) => {
                warn!(error = %format!("{:#}", e.error), "camera start failed");
                slot.idle = Some(e.publication);
                drop(slot);
                inner.close_self_view();
                Err(anyhow!(e.message))
            }
        }
    }

    /// Turn our camera off: the device is released (light off) and the track muted (kept for the
    /// next start). Returns false when it was not on.
    pub async fn stop_camera(&self) -> bool {
        let inner = &self.inner;
        if let Some(t) = inner.camera_watchdog.lock().take() {
            t.abort();
        }
        let mut slot = inner.camera.lock().await;
        let Some(cam) = slot.active.take() else { return false };
        slot.idle = Some(tokio::task::spawn_blocking(move || cam.stop()).await.unwrap_or_default());
        drop(slot);
        inner.close_self_view();
        true
    }

    pub async fn camera_on(&self) -> bool {
        self.inner.camera.lock().await.active.is_some()
    }

    /// Run `f` on our running camera's capture (image controls, picture processing, frame rate);
    /// `None` when the camera is off. `f` must not block.
    pub async fn with_camera<R>(&self, f: impl FnOnce(&vox_capture::CameraCapture) -> R) -> Option<R> {
        let slot = self.inner.camera.lock().await;
        slot.active.as_ref().and_then(|c| c.capture()).map(f)
    }

    /// Encoder behind our camera (`"nvenc"` / `"mf"` / `"software"`), `None` when off.
    pub async fn camera_encoder(&self) -> Option<String> {
        self.inner.camera.lock().await.active.as_ref().map(|c| c.backend())
    }

    /// Whether the camera tiles are on screen. Hidden (chat shown, app minimized): remote cameras
    /// are paused at the SFU and our self-view stops producing frames; shown: everything resumes.
    pub async fn set_cameras_visible(&self, visible: bool) {
        let inner = &self.inner;
        if inner.cams_visible.swap(visible, Ordering::Relaxed) == visible {
            return;
        }
        for p in inner.room.remote_participants().values() {
            inner.select_camera(p);
        }
        if let Some(c) = inner.camera.lock().await.active.as_ref() {
            c.set_preview(visible);
        }
        info!(visible, remote = inner.remote_cams.lock().len(), "camera tiles visibility");
    }

    /// Hide (or show again) one person's camera only for me: while hidden their video is paused
    /// at the SFU (no bandwidth, no decoding). Remembered for the whole session.
    pub fn set_camera_hidden(&self, identity: &str, hidden: bool) {
        let inner = &self.inner;
        let changed = {
            let mut set = inner.cams_hidden.lock();
            if hidden { set.insert(identity.to_string()) } else { set.remove(identity) }
        };
        if !changed {
            return;
        }
        for p in inner.room.remote_participants().values() {
            if ident(p) == identity {
                inner.select_camera(p);
            }
        }
        info!(identity, hidden, "participant camera hidden for me");
    }

    /// Replace the whole hidden set (a fresh session starts with what the app remembers).
    pub fn set_cameras_hidden(&self, identities: &[String]) {
        *self.inner.cams_hidden.lock() = identities.iter().cloned().collect();
    }

    /// Identities of the other participants whose camera is on.
    pub fn cameras(&self) -> Vec<String> {
        self.inner.cams_live.lock().iter().cloned().collect()
    }

    /// Watch a participant's Go Live (by identity or display name), next to the streams already
    /// watched. Subscribes their screenshare video + audio (the audio goes into the mixer with
    /// its own volume / mute) and opens a viewer surface keyed by their identity. Watching
    /// someone already watched is a no-op. Fails with [`WatchLimit`] when [`MAX_WATCHED`]
    /// streams are already watched.
    pub fn watch(&self, who: &str) -> Result<String> {
        let inner = &self.inner;
        let participants = inner.room.remote_participants();
        let p = participants
            .values()
            .find(|p| p.identity().0 == who || p.name() == who)
            .ok_or_else(|| anyhow!("no participant {who:?} in the room"))?;
        let identity = ident(p);
        {
            let mut watched = inner.watched.lock();
            if watched.contains_key(&identity) {
                return Ok(identity);
            }
            if watched.len() >= MAX_WATCHED {
                return Err(anyhow::Error::new(WatchLimit { max: MAX_WATCHED }));
            }
            // In the set before any track subscribes, so `on_track_subscribed` takes it.
            watched.insert(identity.clone(), Watched { view: None, video: None, visible: true, wanted: true, visible_gen: 0 });
        }
        // An audio-only live has nothing to draw: no viewer surface, no video subscription.
        let audio_only = is_audio_only(p);
        let view = if audio_only {
            None
        } else {
            match Viewer::get() {
                Ok(v) => Some(v.open(&identity, &format!("concord - {}", p.name()), inner.viewer_tx.clone())),
                Err(e) => {
                    warn!(error = %e, "viewer unavailable; frames will only be counted");
                    None
                }
            }
        };
        if let Some(w) = inner.watched.lock().get_mut(&identity) {
            w.view = view;
        }
        let mut has_share = false;
        for publication in p.track_publications().values() {
            if publication.source() == TrackSource::Screenshare {
                has_share |= !publication.is_muted();
            }
        }
        select_share_audio(p);
        select_share_video(p);
        let count = inner.watched.lock().len();
        info!(identity = %identity, has_share, audio_only, watching = count, "watching");
        let _ = inner.events.send(VoiceEvent::WatchStarted { identity: identity.clone() });
        Ok(identity)
    }

    /// Stop watching `identity`: unsubscribe their screenshare tracks and close their viewer.
    /// The other watched streams are untouched. Returns false when they were not watched.
    pub fn unwatch(&self, identity: &str) -> bool {
        self.inner.unwatch(identity, "stopped").is_some()
    }

    /// Stop watching every stream; returns who was watched.
    pub fn unwatch_all(&self) -> Vec<String> {
        self.inner.unwatch_all("stopped")
    }

    /// Identities of the watched streams (sorted).
    pub fn watching(&self) -> Vec<String> {
        let mut v: Vec<String> = self.inner.watched.lock().keys().cloned().collect();
        v.sort();
        v
    }

    /// `identity`'s live is audio-only ("Só áudio"; see [`VoiceEvent::ShareKind`]).
    pub fn is_audio_only(&self, identity: &str) -> bool {
        self.inner.audio_only.lock().contains(identity)
    }

    pub fn is_watching(&self, identity: &str) -> bool {
        self.inner.watched.lock().contains_key(identity)
    }

    /// Whether `identity`'s watched stream is on screen anywhere (a tile, the strip, its own
    /// window, fullscreen). Hidden for [`HIDDEN_GRACE`], its video is disabled at the SFU (no
    /// network, no decode; the audio keeps playing); shown, it is enabled again at once (the SFU
    /// asks the streamer for a keyframe). Streams start visible.
    pub fn set_stream_visible(&self, identity: &str, visible: bool) {
        let inner = &self.inner;
        let gen = {
            let mut watched = inner.watched.lock();
            let Some(w) = watched.get_mut(identity) else { return };
            if w.wanted == visible {
                return;
            }
            w.wanted = visible;
            w.visible_gen += 1;
            if visible {
                w.visible = true;
                w.apply_enabled();
                info!(identity, "watched stream shown: video resumed");
                return;
            }
            w.visible_gen
        };
        // Hidden: pause after the grace period, unless it was shown again meanwhile.
        let weak = Arc::downgrade(inner);
        let identity = identity.to_string();
        let pause = move || {
            let Some(inner) = weak.upgrade() else { return };
            let mut watched = inner.watched.lock();
            if let Some(w) = watched.get_mut(&identity).filter(|w| w.visible_gen == gen) {
                w.visible = false;
                w.apply_enabled();
                info!(identity = %identity, "watched stream hidden: video paused at the SFU");
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn(async move {
                    tokio::time::sleep(HIDDEN_GRACE).await;
                    pause();
                });
            }
            Err(_) => pause(),
        }
    }

    /// Silence `identity`'s screenshare audio because another stream is in focus ("som só da
    /// transmissão em foco"). Independent of the saved stream volume / mute.
    pub fn set_stream_focus_muted(&self, identity: &str, muted: bool) {
        self.inner.prefs.set_stream_focus_muted(identity, muted);
    }

    /// Streams to report in stats.
    pub async fn streams(&self) -> Vec<StreamEntry> {
        let inner = &self.inner;
        let mut out = Vec::new();
        if let Some(m) = inner.mic.lock().as_ref() {
            out.push(StreamEntry {
                label: "mic".into(),
                dir: Direction::Send,
                is_video: false,
                track: Some(Track::LocalAudio(m.track.clone())),
                counters: m.counters.clone(),
                encoder: None,
            });
        }
        if let Some(s) = inner.share.lock().await.active.as_ref() {
            if let Some(t) = &s.video_track {
                out.push(StreamEntry {
                    label: "share-video".into(),
                    dir: Direction::Send,
                    is_video: true,
                    track: Some(Track::LocalVideo(t.clone())),
                    counters: s.video_counters.clone(),
                    encoder: s.encoder_report(),
                });
            }
            if s.preview_counters().is_some() {
                out.push(StreamEntry {
                    label: "share-preview".into(),
                    dir: Direction::Send,
                    is_video: true,
                    track: None,
                    counters: inner.share_preview_counters.clone(),
                    encoder: None,
                });
            }
            if let (Some(t), Some(c)) = (&s.audio_track, &s.audio_counters) {
                out.push(StreamEntry {
                    label: "share-audio".into(),
                    dir: Direction::Send,
                    is_video: false,
                    track: Some(Track::LocalAudio(t.clone())),
                    counters: c.clone(),
                    encoder: None,
                });
            }
        }
        {
            let watched = inner.watched.lock();
            let mut keys: Vec<&String> = watched.keys().collect();
            keys.sort();
            for v in keys.into_iter().filter_map(|k| watched[k].video.as_ref()) {
                out.push(StreamEntry {
                    label: format!("watch-video:{}", v.identity),
                    dir: Direction::Recv,
                    is_video: true,
                    track: Some(Track::RemoteVideo(v.track.clone())),
                    counters: v.counters.clone(),
                    encoder: None,
                });
            }
        }
        if let Some(c) = inner.camera.lock().await.active.as_ref() {
            out.push(StreamEntry {
                label: "camera".into(),
                dir: Direction::Send,
                is_video: true,
                track: Some(Track::LocalVideo(c.track.clone())),
                counters: c.counters.clone(),
                encoder: c.encoder_report(),
            });
            out.push(StreamEntry {
                label: "camera-preview".into(),
                dir: Direction::Send,
                is_video: true,
                track: None,
                counters: inner.self_view_counters.clone(),
                encoder: None,
            });
        }
        for v in inner.remote_cams.lock().values() {
            out.push(StreamEntry {
                label: format!("cam-video:{}", v.identity),
                dir: Direction::Recv,
                is_video: true,
                track: Some(Track::RemoteVideo(v.track.clone())),
                counters: v.counters.clone(),
                encoder: None,
            });
        }
        for sub in inner.remote_audio.lock().values() {
            let kind = if sub.source == TrackSource::ScreenshareAudio { "watch-audio" } else { "voice" };
            out.push(StreamEntry {
                label: format!("{kind}:{}", sub.identity),
                dir: Direction::Recv,
                is_video: false,
                track: Some(Track::RemoteAudio(sub.track.clone())),
                counters: sub.counters.clone(),
                encoder: None,
            });
        }
        out
    }

    /// Stop everything and disconnect.
    pub async fn leave(self) {
        let inner = self.inner.clone();
        if let Some(t) = inner.share_watchdog.lock().take() {
            t.abort();
        }
        if let Some(t) = inner.encoder_guard.lock().take() {
            t.abort();
        }
        if let Some(t) = inner.rx_health.lock().take() {
            t.abort();
        }
        if let Some(t) = inner.e2ee_tick.lock().take() {
            t.abort();
        }
        // Our media keys (and what we knew of the others') are wiped.
        inner.e2ee.lock().take();
        inner.mic_tone.lock().take();
        {
            let mut slot = inner.share.lock().await;
            if let Some(share) = slot.active.take() {
                slot.idle = Some(share.stop());
            }
            // Closing the room below releases the publication's senders and encoder.
            slot.idle.take();
        }
        inner.close_share_preview();
        if let Some(t) = inner.camera_watchdog.lock().take() {
            t.abort();
        }
        {
            let mut slot = inner.camera.lock().await;
            if let Some(cam) = slot.active.take() {
                // Releases the device (light off); closing the room releases the publication.
                let _ = tokio::task::spawn_blocking(move || drop(cam.stop())).await;
            }
            slot.idle.take();
        }
        inner.close_self_view();
        inner.drop_remote_cams();
        inner.unwatch_all("left room");
        inner.remote_audio.lock().clear();
        // Close the audio devices before the room so no callback touches a dying source.
        let io = inner.audio_io.lock().take();
        if let Some(io) = io {
            let _ = tokio::task::spawn_blocking(move || drop(io)).await;
        }
        if let Err(e) = inner.room.close().await {
            debug!(error = %e, "room close");
        }
        self.event_task.abort();
        info!("left voice room");
    }
}

impl Inner {
    /// Re-read whether `p`'s live is audio-only and tell the app when that changed. A stream
    /// already being watched follows: an audio-only one has no picture (its viewer surface goes
    /// away), a screen share needs one.
    fn refresh_kind(&self, p: &RemoteParticipant) {
        let identity = ident(p);
        let now = is_audio_only(p);
        let changed = {
            let mut set = self.audio_only.lock();
            if now {
                set.insert(identity.clone())
            } else {
                set.remove(&identity)
            }
        };
        if !changed {
            return;
        }
        info!(identity = %identity, audio_only = now, "live kind changed");
        let _ = self.events.send(VoiceEvent::ShareKind { identity: identity.clone(), audio_only: now });
        enum Act {
            CloseView,
            OpenView,
        }
        let act = match self.watched.lock().get_mut(&identity) {
            Some(w) if now => w.view.take().map(|_| Act::CloseView),
            Some(w) if w.view.is_none() => Some(Act::OpenView),
            _ => None,
        };
        match act {
            Some(Act::CloseView) => {
                if let Ok(v) = Viewer::get() {
                    v.close(&identity);
                }
            }
            Some(Act::OpenView) => {
                if let Ok(v) = Viewer::get() {
                    let view = v.open(&identity, &format!("concord - {}", p.name()), self.viewer_tx.clone());
                    if let Some(w) = self.watched.lock().get_mut(&identity) {
                        w.view = Some(view);
                    }
                }
            }
            None => {}
        }
        if self.watched.lock().contains_key(&identity) {
            select_share_audio(p);
            select_share_video(p);
        }
    }

    fn set_muted(&self, muted: bool) {
        if !muted && self.mic_unavailable.load(Ordering::Relaxed) {
            return; // no microphone: nothing to unmute
        }
        self.muted.store(muted, Ordering::Relaxed);
        if let Some(m) = self.mic.lock().as_ref() {
            if muted {
                m.publication.mute();
            } else {
                m.publication.unmute();
            }
        }
    }

    /// Decide whether to subscribe to a (newly seen) remote publication.
    fn on_publication(&self, publication: &RemoteTrackPublication, p: &RemoteParticipant) {
        match publication.source() {
            TrackSource::Microphone => self.select_mic(p, None),
            TrackSource::Camera => self.select_camera(p),
            TrackSource::Screenshare | TrackSource::ScreenshareAudio => {
                let watched = self.watched.lock().contains_key(p.identity().0.as_str());
                if !watched {
                    publication.set_subscribed(false);
                } else if publication.source() == TrackSource::Screenshare {
                    select_share_video(p);
                } else {
                    select_share_audio(p);
                }
            }
            _ => {}
        }
    }

    /// Open our mirrored self-view tile and make the preview callback that feeds it.
    fn self_view_factory(&self) -> PreviewFactory {
        let stream = match Viewer::get() {
            // (The capture mirrors the self-view itself, switchable: "Espelhar minha prévia".)
            Ok(v) if v.is_native() => v.open_tile(SELF_CAMERA_KEY, TileStyle { mirror: false, fill: true }, self.viewer_tx.clone()),
            _ => ViewerStream::null(),
        };
        let counters = self.self_view_counters.clone();
        Arc::new(move || {
            let (stream, counters) = (stream.clone(), counters.clone());
            Box::new(move |f: &vox_capture::Nv12Frame<'_>| {
                counters.frame(f.width, f.height);
                stream.submit_nv12(f);
            })
        })
    }

    /// Start the encoder safety net unless it runs already (it ends by itself once neither the
    /// share nor the camera is on).
    fn ensure_encoder_guard(self: &Arc<Self>) {
        let mut g = self.encoder_guard.lock();
        if g.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }
        *g = Some(tokio::spawn(encoder_guard(Arc::downgrade(self))));
    }

    /// The Go Live's encoder `id` makes unstable video: demote it (`persist`: viewer-confirmed,
    /// counts toward the saved failures) and restart the share on the next encoder of the plan
    /// (same source and options, same publications).
    async fn switch_share_encoder(self: &Arc<Self>, id: &str, reason: &str, persist: bool) {
        let next = encoders::demote(id, reason, persist);
        self.rx_reports.lock().clear(encoders::StreamKind::Share);
        let mut slot = self.share.lock().await;
        let Some(share) = slot.active.take() else { return };
        if share.attempt().map(|a| a.id.as_str()) != Some(id) {
            slot.active = Some(share); // restarted meanwhile
            return;
        }
        warn!(from = %encoders::label(id), to = %next, reason, "Go Live video unstable; switching encoder");
        let (source, opts) = (share.source.clone(), share.opts);
        let reuse = share.stop();
        let preview = if source.is_audio_only() { None } else { self.open_share_preview() };
        match ActiveShare::start_with_preview(&self.room, source, opts, Some(reuse), preview).await {
            Ok(new) => {
                let label = new.attempt().map(|a| a.label.clone()).unwrap_or_default();
                let backend = new.encoder_report().map(|r| r.backend).unwrap_or_else(|| "software".into());
                slot.active = Some(new);
                let _ = self.events.send(VoiceEvent::EncoderSwitched { camera: false, label, backend });
            }
            Err(e) => {
                warn!(error = %format!("{:#}", e.error), "Go Live restart on the next encoder failed");
                slot.idle = e.publication.map(|p| *p);
                drop(slot);
                self.close_share_preview();
                let _ = self.events.send(VoiceEvent::LocalShareEnded { reason: "encoder switch failed".into() });
            }
        }
    }

    /// [`Inner::switch_share_encoder`] for the camera.
    async fn switch_camera_encoder(self: &Arc<Self>, id: &str, reason: &str, persist: bool) {
        let next = encoders::demote(id, reason, persist);
        self.rx_reports.lock().clear(encoders::StreamKind::Camera);
        let mut slot = self.camera.lock().await;
        let Some(cam) = slot.active.take() else { return };
        if cam.attempt().id != id {
            slot.active = Some(cam);
            return;
        }
        warn!(from = %encoders::label(id), to = %next, reason, "camera video unstable; switching encoder");
        let opts = cam.opts.clone();
        let reuse = tokio::task::spawn_blocking(move || cam.stop()).await.unwrap_or_default();
        match ActiveCamera::start(&self.room, opts, Some(reuse), Some(self.self_view_factory())).await {
            Ok(new) => {
                new.set_preview(self.cams_visible.load(Ordering::Relaxed));
                let label = new.attempt().label.clone();
                let backend = new.backend();
                slot.active = Some(new);
                let _ = self.events.send(VoiceEvent::EncoderSwitched { camera: true, label, backend });
            }
            Err(e) => {
                warn!(error = %format!("{:#}", e.error), "camera restart on the next encoder failed");
                slot.idle = Some(e.publication);
                drop(slot);
                self.close_self_view();
                let _ = self.events.send(VoiceEvent::LocalCameraEnded { reason: e.message });
            }
        }
    }

    /// Open the tile of our own Go Live preview (native viewer only) and make its sink: the
    /// share's capture feeds it small frames while the app asks for them.
    fn open_share_preview(&self) -> Option<SharePreview> {
        // Without the native viewer there is no tile, but the tap still gives the hover picture
        // its snapshots (v1.1): frames for the tile go nowhere then.
        let stream = match Viewer::get() {
            Ok(v) if v.is_native() => v.open_tile(SELF_SHARE_KEY, TileStyle { mirror: false, fill: false }, self.viewer_tx.clone()),
            _ => ViewerStream::null(),
        };
        Some(SharePreview::new(stream, self.share_preview_counters.clone(), *self.share_preview_cfg.lock()))
    }

    fn close_share_preview(&self) {
        if let Ok(v) = Viewer::get() {
            if v.is_native() {
                v.close(SELF_SHARE_KEY);
            }
        }
    }

    fn close_self_view(&self) {
        if let Ok(v) = Viewer::get() {
            if v.is_native() {
                v.close(SELF_CAMERA_KEY);
            }
        }
    }

    /// One microphone per participant: subscribe the picked one ([`pick_mic`]), unsubscribe any
    /// other. `gone`: a publication that is being unpublished.
    fn select_mic(&self, p: &RemoteParticipant, gone: Option<&TrackSid>) {
        let identity = ident(p);
        let pubs: Vec<RemoteTrackPublication> =
            p.track_publications().values().filter(|x| x.source() == TrackSource::Microphone && Some(&x.sid()) != gone).cloned().collect();
        let sids: Vec<String> = pubs.iter().map(|x| x.sid().to_string()).collect();
        let pick = {
            let mut picks = self.mic_picks.lock();
            let current = picks.get(&identity).map(|s| s.to_string());
            let pick = pick_mic(current.as_deref(), &sids);
            match &pick {
                Some(i) => {
                    picks.insert(identity.clone(), pubs[*i].sid());
                }
                None => {
                    picks.remove(&identity);
                }
            }
            pick
        };
        if pubs.len() > 1 {
            warn!(identity = %identity, count = pubs.len(), "participant publishes more than one microphone: only one is played");
        }
        for (i, x) in pubs.iter().enumerate() {
            let want = Some(i) == pick;
            if x.is_desired() != want {
                x.set_subscribed(want);
            }
        }
    }

    /// Is this microphone publication the participant's picked one?
    fn is_picked_mic(&self, identity: &str, sid: &TrackSid) -> bool {
        self.mic_picks.lock().get(identity).is_some_and(|s| s == sid)
    }

    /// Apply the camera policy to one participant: their live (unmuted) camera publication is
    /// subscribed while the tiles are visible and disabled while hidden; idle ones (a camera that
    /// is off, or the other path's publication) are unsubscribed. Emits started / stopped.
    fn select_camera(&self, p: &RemoteParticipant) {
        let visible = self.cams_visible.load(Ordering::Relaxed) && !self.cams_hidden.lock().contains(&ident(p));
        let pubs: Vec<RemoteTrackPublication> = p.track_publications().values().filter(|x| x.source() == TrackSource::Camera).cloned().collect();
        let live = pubs.iter().find(|x| !x.is_muted()).map(|x| x.sid());
        for x in &pubs {
            if Some(x.sid()) != live {
                if x.is_subscribed() {
                    x.set_subscribed(false);
                }
            } else if visible {
                x.set_subscribed(true);
                x.set_enabled(true);
            } else if x.is_subscribed() {
                x.set_enabled(false);
            }
        }
        let identity = ident(p);
        let changed = {
            let mut set = self.cams_live.lock();
            if live.is_some() {
                set.insert(identity.clone())
            } else {
                set.remove(&identity)
            }
        };
        if changed {
            let on = live.is_some();
            info!(identity = %identity, on, visible, "participant camera");
            let _ = self.events.send(if on { VoiceEvent::CameraStarted { identity } } else { VoiceEvent::CameraStopped { identity } });
        }
    }

    fn on_camera_subscribed(&self, video: RemoteVideoTrack, publication: RemoteTrackPublication, identity: String) {
        let key = camera_key(&identity);
        let viewer = match Viewer::get() {
            Ok(v) => v.open_tile(&key, TileStyle { mirror: false, fill: true }, self.viewer_tx.clone()),
            Err(_) => ViewerStream::null(),
        };
        let counters = Arc::new(StreamCounters::default());
        let mut stream = NativeVideoStream::new(video.rtc_track());
        let c = counters.clone();
        let probe = crate::golive::camera::probe_enabled().then(|| identity.clone());
        let task = tokio::spawn(async move {
            while let Some(frame) = stream.next().await {
                c.frame(frame.buffer.width(), frame.buffer.height());
                if let Some(id) = &probe {
                    crate::golive::camera::probe_frame(id, frame.buffer.as_ref());
                }
                viewer.submit(frame);
            }
        });
        if !self.cams_visible.load(Ordering::Relaxed) || self.cams_hidden.lock().contains(&identity) {
            publication.set_enabled(false);
        }
        info!(identity = %identity, "camera video subscribed");
        // Replaces an older subscription of the same person (the tile stays open).
        self.remote_cams.lock().insert(identity.clone(), RemoteVideoSub { identity, track: video, publication, counters, task });
    }

    /// A participant left: forget their camera.
    fn camera_gone(&self, identity: &str) {
        if self.remote_cams.lock().remove(identity).is_some() {
            if let Ok(v) = Viewer::get() {
                v.close(&camera_key(identity));
            }
        }
        if self.cams_live.lock().remove(identity) {
            let _ = self.events.send(VoiceEvent::CameraStopped { identity: identity.to_string() });
        }
    }

    fn drop_remote_cams(&self) {
        let cams: Vec<String> = self.remote_cams.lock().drain().map(|(k, _)| k).collect();
        if let Ok(v) = Viewer::get() {
            for k in cams {
                v.close(&camera_key(&k));
            }
        }
        self.cams_live.lock().clear();
    }

    /// Stop watching one stream (the others go on). Returns its identity when it was watched.
    fn unwatch(&self, identity: &str, reason: &str) -> Option<String> {
        let gone = self.watched.lock().remove(identity)?;
        if let Some(p) = self.room.remote_participants().values().find(|p| p.identity().0 == identity) {
            for publication in p.track_publications().values() {
                if matches!(publication.source(), TrackSource::Screenshare | TrackSource::ScreenshareAudio) {
                    publication.set_subscribed(false);
                }
            }
        }
        drop(gone);
        self.remote_audio.lock().retain(|_, s| !(s.source == TrackSource::ScreenshareAudio && s.identity == identity));
        self.prefs.set_stream_focus_muted(identity, false);
        if let Ok(v) = Viewer::get() {
            v.close(identity);
        }
        let left = self.watched.lock().len();
        info!(identity = %identity, reason, still_watching = left, "stopped watching");
        let _ = self.events.send(VoiceEvent::WatchEnded { identity: identity.to_string(), reason: reason.to_string() });
        Some(identity.to_string())
    }

    fn unwatch_all(&self, reason: &str) -> Vec<String> {
        let mut all: Vec<String> = self.watched.lock().keys().cloned().collect();
        all.sort();
        all.into_iter().filter_map(|id| self.unwatch(&id, reason)).collect()
    }

    fn on_track_subscribed(&self, track: RemoteTrack, publication: RemoteTrackPublication, p: RemoteParticipant) {
        let identity = ident(&p);
        match track {
            RemoteTrack::Audio(audio) => {
                let source = publication.source();
                if source == TrackSource::Microphone && !self.is_picked_mic(&identity, &publication.sid()) {
                    // A second microphone of the same person (see `select_mic`): not played.
                    publication.set_subscribed(false);
                    return;
                }
                let ctl = if source == TrackSource::ScreenshareAudio { self.prefs.stream_ctl(&identity) } else { self.prefs.mic_ctl(&identity) };
                let id = self.mixer.add_source_with(ctl);
                let counters = Arc::new(StreamCounters::default());
                let mut stream = NativeAudioStream::new(audio.rtc_track(), MIX_RATE as i32, MIX_CHANNELS as i32);
                let mixer = self.mixer.clone();
                let c = counters.clone();
                // (`MIX_CHANNELS` = 2: the shared sound keeps its stereo image, so measure it.)
                let stereo = source == TrackSource::ScreenshareAudio;
                // A microphone also tells who is speaking, from what is decoded (v1.1).
                let mut level = (source == TrackSource::Microphone).then(|| self.speakers.add_track(&identity));
                let task = tokio::spawn(async move {
                    loop {
                        // Only while someone speaks is the wait bounded: audio that stops coming
                        // (paused track, network) must not leave the ring on.
                        let next = match level.as_mut() {
                            Some(l) if l.speaking() => match tokio::time::timeout(speaking::STALL, stream.next()).await {
                                Ok(f) => f,
                                Err(_) => {
                                    l.stalled();
                                    continue;
                                }
                            },
                            _ => stream.next().await,
                        };
                        let Some(frame) = next else { break };
                        mixer.push(id, &frame.data);
                        if let Some(l) = level.as_mut() {
                            let ms = (frame.samples_per_channel * 1000 / frame.sample_rate.max(1)).max(1);
                            l.frame(&frame.data, ms);
                        }
                        if stereo {
                            c.audio_stereo(&frame.data);
                        } else {
                            c.audio(&frame.data);
                        }
                    }
                });
                if self.deafened.load(Ordering::Relaxed) {
                    publication.set_enabled(false);
                }
                info!(identity = %identity, ?source, "remote audio subscribed");
                let sub = RemoteAudioSub { identity, source, publication: publication.clone(), track: audio, mixer_id: id, counters, task, mixer: self.mixer.clone() };
                self.remote_audio.lock().insert(publication.sid(), sub);
            }
            RemoteTrack::Video(video) if publication.source() == TrackSource::Camera => self.on_camera_subscribed(video, publication, identity),
            RemoteTrack::Video(video) => {
                // Not watched (any more), or a sharer's idle publication while the live one is
                // already here (a sharer has up to two screenshare video publications, raw and
                // pre-encoded, see SharePublication): not worth a decoder.
                let viewer = {
                    let watched = self.watched.lock();
                    match watched.get(&identity) {
                        Some(w) if publication.source() == TrackSource::Screenshare => {
                            let idle_dup = publication.is_muted() && w.video.as_ref().is_some_and(|v| !v.publication.is_muted());
                            (!idle_dup).then(|| w.view.clone())
                        }
                        _ => None,
                    }
                };
                let Some(viewer) = viewer else {
                    publication.set_subscribed(false);
                    return;
                };
                let counters = Arc::new(StreamCounters::default());
                let mut stream = NativeVideoStream::new(video.rtc_track());
                let c = counters.clone();
                let task = tokio::spawn(async move {
                    while let Some(frame) = stream.next().await {
                        c.frame(frame.buffer.width(), frame.buffer.height());
                        if let Some(v) = &viewer {
                            v.submit(frame);
                        }
                    }
                });
                let sub = RemoteVideoSub { identity: identity.clone(), track: video, publication, counters, task };
                let mut watched = self.watched.lock();
                let Some(w) = watched.get_mut(&identity) else {
                    // Unwatched meanwhile.
                    let publication = sub.publication.clone();
                    drop(watched);
                    drop(sub);
                    publication.set_subscribed(false);
                    return;
                };
                // Replaces an older subscription of the same streamer.
                w.video = Some(sub);
                w.apply_enabled();
                info!(identity = %identity, enabled = w.visible, "screenshare video subscribed");
            }
        }
    }

    fn on_track_unsubscribed(&self, publication: &RemoteTrackPublication) {
        if self.remote_audio.lock().remove(&publication.sid()).is_some() {
            debug!(sid = %publication.sid(), "remote audio unsubscribed");
        }
        let gone = {
            let mut cams = self.remote_cams.lock();
            let who = cams.iter().find(|(_, v)| v.publication.sid() == publication.sid()).map(|(k, _)| k.clone());
            who.and_then(|k| cams.remove(&k)).map(|v| v.identity.clone())
        };
        if let Some(identity) = gone {
            if let Ok(v) = Viewer::get() {
                v.close(&camera_key(&identity));
            }
            debug!(identity = %identity, "camera video unsubscribed");
        }
        let sid = publication.sid();
        let gone = self.watched.lock().values_mut().find(|w| w.video.as_ref().is_some_and(|v| v.track.sid() == sid)).and_then(|w| w.video.take());
        if let Some(v) = gone {
            debug!(identity = %v.identity, %sid, "screenshare video unsubscribed");
        }
    }
}

/// Subscribe the participant's live (unmuted) screenshare video and unsubscribe its idle ones.
/// A sharer keeps one muted publication per video path it used this session (raw OpenH264 and
/// pre-encoded), so only the unmuted one is worth a receiver + decoder. If none is live, a lone
/// publication stays subscribed (instant resume, as in Phase 1); with several we wait for the
/// unmute.
fn select_share_video(p: &RemoteParticipant) {
    let pubs: Vec<RemoteTrackPublication> = p.track_publications().values().filter(|x| x.source() == TrackSource::Screenshare).cloned().collect();
    if is_audio_only(p) {
        // Audio-only live: whatever video publications are left over from earlier screen shares
        // are muted and stay unsubscribed.
        for x in &pubs {
            x.set_subscribed(false);
        }
        return;
    }
    match pubs.iter().find(|x| !x.is_muted()) {
        Some(live) => {
            for x in &pubs {
                x.set_subscribed(x.sid() == live.sid());
            }
        }
        None if pubs.len() == 1 => pubs[0].set_subscribed(true),
        None => {}
    }
}

/// Subscribe the participant's live (unmuted) screenshare audio and unsubscribe its idle ones. A
/// sharer keeps one audio publication per kind it used this session (the screen share's
/// `screenshare_audio` and an `audio-only` one per quality, see [`SharePublication`]), and only
/// one is unmuted at a time. A lone publication stays subscribed even while muted (instant
/// resume); with several we wait for the unmute.
fn select_share_audio(p: &RemoteParticipant) {
    let pubs: Vec<RemoteTrackPublication> = p.track_publications().values().filter(|x| x.source() == TrackSource::ScreenshareAudio).cloned().collect();
    match pubs.iter().find(|x| !x.is_muted()) {
        Some(live) => {
            for x in &pubs {
                x.set_subscribed(x.sid() == live.sid());
            }
        }
        None if pubs.len() == 1 => pubs[0].set_subscribed(true),
        None => {}
    }
}

/// What a participant's screenshare publications say about their live: `(source, name, muted)`
/// of each.
type PubFacts = (TrackSource, String, bool);

/// Is the live made of nothing but the `audio-only` track (no screenshare video that is on)?
/// There is no protocol field for this, so it is the track's name and the absence of video.
fn audio_only_live(pubs: &[PubFacts]) -> bool {
    let video_on = pubs.iter().any(|(s, _, muted)| *s == TrackSource::Screenshare && !muted);
    let audio_only_on = pubs.iter().any(|(s, n, muted)| *s == TrackSource::ScreenshareAudio && n == AUDIO_ONLY_TRACK_NAME && !muted);
    audio_only_on && !video_on
}

fn share_facts(p: &RemoteParticipant) -> Vec<PubFacts> {
    p.track_publications()
        .values()
        .filter(|x| matches!(x.source(), TrackSource::Screenshare | TrackSource::ScreenshareAudio))
        .map(|x| (x.source(), x.name(), x.is_muted()))
        .collect()
}

fn is_audio_only(p: &RemoteParticipant) -> bool {
    audio_only_live(&share_facts(p))
}

/// Ends our share when its capture stops on its own (window closed, monitor unplugged, GPU
/// device lost) so the UI/hub can show `streaming = false`.
async fn share_watchdog(inner: std::sync::Weak<Inner>) {
    let mut audio_warned = false;
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let Some(inner) = inner.upgrade() else { return };
        let mut slot = inner.share.lock().await;
        let Some(share) = slot.active.as_ref() else { return };
        if !share.video_running() || share.source_gone() {
            let share = slot.active.take().expect("checked above");
            let reason = format!("{:?} capture stopped (target closed or lost)", share.source);
            warn!(reason = %reason, "Go Live ended by capture");
            slot.idle = Some(share.stop());
            drop(slot);
            inner.close_share_preview();
            schedule_trim(Arc::downgrade(&inner));
            let _ = inner.events.send(VoiceEvent::LocalShareEnded { reason });
            return;
        }
        if share.has_audio() && !share.audio_running() && !audio_warned {
            audio_warned = true;
            warn!("screenshare audio capture stopped; continuing with video only");
        }
    }
}

/// Per-stream state of the encoder safety net.
#[derive(Default)]
struct GuardState {
    detector: encoders::StormDetector,
    /// The encoder the baseline below belongs to.
    id: Option<String>,
    prev: Option<(Instant, crate::stats::SendHealth, u64)>,
    /// `VOX_TEST_PLI_STORM=1` already caused its one switch.
    hook_spent: bool,
}

impl GuardState {
    /// This window's sample; `None` for an encoder's first window (it only sets the baseline).
    async fn sample(&mut self, id: &str, track: &LocalVideoTrack, fb: &EncodedFeedback, hook: bool) -> Option<encoders::StormSample> {
        if self.id.as_deref() != Some(id) {
            self.id = Some(id.to_string());
            self.prev = None;
            self.detector.reset_windows();
        }
        let stats = tokio::time::timeout(STATS_TIMEOUT, track.get_stats()).await.ok()?.ok()?;
        let h = crate::stats::send_health(&stats);
        let drops = fb.drop_events.load(Ordering::Relaxed);
        let (t0, h0, d0) = self.prev.replace((Instant::now(), h, drops))?;
        let mut s = encoders::StormSample {
            secs: t0.elapsed().as_secs_f64(),
            plis: h.plis.saturating_sub(h0.plis),
            drop_events: drops.saturating_sub(d0),
            packets_sent: h.packets_sent.saturating_sub(h0.packets_sent),
            packets_lost: (h.packets_lost - h0.packets_lost).max(0) as u64,
            fraction_lost: h.fraction_lost,
        };
        if hook && !self.hook_spent {
            // Test hook: the first encoder of the stream behaves like the field report's (keyframe requests
            // at twice the threshold, no loss). Once switched, the new encoder is healthy.
            s.plis += (encoders::STORM_PLI_PER_S * s.secs * 2.0) as u64;
        }
        Some(s)
    }
}

/// The encoder safety net (v0.7): every [`encoders::STORM_WINDOW`], the running share's and
/// camera's send stats plus what the viewers reported ([`Inner::rx_reports`]). A sustained
/// keyframe-request storm on a clean uplink that a viewer confirms (it decodes badly while its
/// own network loses nothing), or such a viewer for two windows, means a bad bitstream: the
/// encoder is demoted and the stream restarted on the next one ([`encoders::StormDetector`]: at
/// most one switch a minute, never back). A storm nobody confirms keeps the encoder (old viewers
/// never report; one viewer's bad Wi-Fi makes the SFU forward its PLIs to us). WebRTC dropping
/// our own frames is local evidence: it switches too, for this session only. Ends when neither
/// runs.
async fn encoder_guard(inner: std::sync::Weak<Inner>) {
    let hook = std::env::var("VOX_TEST_PLI_STORM").is_ok_and(|v| v == "1");
    let (mut share, mut cam) = (GuardState::default(), GuardState::default());
    loop {
        tokio::time::sleep(encoders::STORM_WINDOW).await;
        let Some(inner) = inner.upgrade() else { return };
        let sp = inner.share.lock().await.active.as_ref().map(|s| s.encoder_probe());
        let cp = inner.camera.lock().await.active.as_ref().map(|c| c.encoder_probe());
        if sp.is_none() && cp.is_none() {
            return;
        }
        for (kind, st, probe) in [(encoders::StreamKind::Share, &mut share, sp.flatten()), (encoders::StreamKind::Camera, &mut cam, cp.flatten())] {
            let is_cam = kind == encoders::StreamKind::Camera;
            let Some((id, track, fb)) = probe else { continue };
            let Some(s) = st.sample(&id, &track, &fb, hook).await else { continue };
            let now = Instant::now();
            let ev = inner.rx_reports.lock().evidence(kind, now);
            if s.stormy() || ev.confirmed {
                debug!(camera = is_cam, encoder = %id, plis = s.plis, drop_events = s.drop_events, loss = format!("{:.3}", s.loss()), viewers = %ev.detail, "encoder guard: stormy window");
            }
            let confirmed = match st.detector.update(&s, &ev, now) {
                encoders::Verdict::Keep => continue,
                encoders::Verdict::Unconfirmed { log } => {
                    if log {
                        info!(
                            camera = is_cam,
                            encoder = %id,
                            plis_per_s = format!("{:.1}", s.plis as f64 / s.secs),
                            loss = format!("{:.3}", s.loss()),
                            "keyframe storm without viewer confirmation; keeping encoder"
                        );
                    }
                    continue;
                }
                encoders::Verdict::Switch { confirmed } => confirmed,
            };
            st.hook_spent = true;
            let reason = if confirmed {
                format!(
                    "{:.1} keyframe requests/s, uplink loss {:.1}%; viewers: {}",
                    s.plis as f64 / s.secs,
                    s.loss() * 100.0,
                    ev.detail
                )
            } else {
                format!(
                    "{:.1} drop events/s, uplink loss {:.1}% for {} windows (no viewer confirmation: this session only)",
                    s.drop_events as f64 / s.secs,
                    s.loss() * 100.0,
                    encoders::STORM_WINDOWS
                )
            };
            if is_cam {
                inner.switch_camera_encoder(&id, &reason, confirmed).await;
            } else {
                inner.switch_share_encoder(&id, &reason, confirmed).await;
            }
        }
    }
}

/// Per received stream state of [`rx_health_guard`].
#[derive(Default)]
struct RxTrackState {
    prev: Option<(Instant, crate::stats::RecvHealth)>,
}

/// Reports sent under `VOX_TEST_RX_DECODE_ERRORS=1`, per publisher and stream, for the whole
/// session: enough to confirm one storm, not to keep demoting the next encoder.
const RX_TEST_HOOK_REPORTS: u32 = 3;

/// The viewer side of the encoder safety net (v0.7): every [`encoders::STORM_WINDOW`], the decode
/// health of the screen shares we watch and the cameras we see. A window with decode errors
/// while our own network loses (almost) nothing means the publisher's encoder sends a broken
/// bitstream: tell that publisher only ([`encoders::RxHealthReport`] over the reliable data
/// channel, at most one per stream per window). Nothing is sent while all is well. Paused
/// (hidden) videos are skipped.
async fn rx_health_guard(inner: std::sync::Weak<Inner>) {
    let hook = std::env::var("VOX_TEST_RX_DECODE_ERRORS").is_ok_and(|v| v == "1");
    let mut tracks: HashMap<(String, encoders::StreamKind), RxTrackState> = HashMap::new();
    let mut hook_sent: HashMap<(String, encoders::StreamKind), u32> = HashMap::new();
    let mut gate = encoders::RxReportGate::default();
    loop {
        tokio::time::sleep(encoders::STORM_WINDOW).await;
        let Some(inner) = inner.upgrade() else { return };
        let mut live: Vec<(String, encoders::StreamKind, RemoteVideoTrack)> = Vec::new();
        for (who, w) in inner.watched.lock().iter() {
            if let Some(v) = w.video.as_ref().filter(|v| v.publication.is_enabled()) {
                live.push((who.clone(), encoders::StreamKind::Share, v.track.clone()));
            }
        }
        for (who, v) in inner.remote_cams.lock().iter() {
            if v.publication.is_enabled() {
                live.push((who.clone(), encoders::StreamKind::Camera, v.track.clone()));
            }
        }
        tracks.retain(|k, _| live.iter().any(|(w, kind, _)| (w, kind) == (&k.0, &k.1)));
        for (who, kind, track) in live {
            let Ok(Ok(stats)) = tokio::time::timeout(STATS_TIMEOUT, track.get_stats()).await else { continue };
            let Some(h) = crate::stats::recv_health(&stats) else { continue };
            let key = (who, kind);
            let now = Instant::now();
            let Some((t0, h0)) = tracks.entry(key.clone()).or_default().prev.replace((now, h)) else { continue };
            let mut r = encoders::RxHealthReport {
                kind,
                secs: (now.duration_since(t0).as_secs_f64() * 100.0).round() / 100.0,
                decoded: h.decoded.saturating_sub(h0.decoded),
                decode_errors: h.dropped.saturating_sub(h0.dropped),
                packets_lost: (h.packets_lost - h0.packets_lost).max(0) as u64,
                packets_received: h.packets_received.saturating_sub(h0.packets_received),
            };
            if hook {
                // Test hook: what the field report's viewers saw (decode errors at twice the threshold, no
                // loss), for the first few windows of each stream.
                let n = hook_sent.entry(key.clone()).or_default();
                if *n < RX_TEST_HOOK_REPORTS {
                    *n += 1;
                    r.decode_errors += (encoders::RX_DECODE_ERRORS_PER_S * r.secs * 2.0).ceil() as u64;
                }
            }
            if !gate.allow(&key.0, &r, now) {
                continue;
            }
            let packet = DataPacket {
                payload: r.to_payload(),
                topic: Some(encoders::RX_HEALTH_TOPIC.to_string()),
                reliable: true,
                destination_identities: vec![ParticipantIdentity(key.0.clone())],
            };
            match inner.room.local_participant().publish_data(packet).await {
                Ok(()) => info!(to = %key.0, ?kind, report = %r.describe(), "told the publisher its video decodes badly here"),
                Err(e) => debug!(to = %key.0, error = %e, "decode-health report not sent"),
            }
        }
    }
}

/// Turns our camera off when its capture stops on its own (unplugged, taken by another app), so
/// the UI can show why.
async fn camera_watchdog(inner: std::sync::Weak<Inner>) {
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let Some(inner) = inner.upgrade() else { return };
        let mut slot = inner.camera.lock().await;
        let Some(cam) = slot.active.as_ref() else { return };
        if cam.running() {
            continue;
        }
        let reason = match cam.failure() {
            Some(f) => {
                warn!(fault = ?f.fault, detail = %f.detail, "camera ended by itself");
                camera_failure_pt(&f)
            }
            None => "A câmera parou.".to_string(),
        };
        let cam = slot.active.take().expect("checked above");
        slot.idle = Some(tokio::task::spawn_blocking(move || cam.stop()).await.unwrap_or_default());
        drop(slot);
        inner.close_self_view();
        let _ = inner.events.send(VoiceEvent::LocalCameraEnded { reason });
        return;
    }
}

/// After a share ends, the (muted, reused) encoder and capture buffers stay allocated but cold.
/// Ten seconds later, if no new share started, trim the working set so they leave RAM
/// (measured: WS 96 MB -> 8 MB at 1080p; private bytes unchanged, pages fault back on reuse).
fn schedule_trim(inner: std::sync::Weak<Inner>) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        let Some(inner) = inner.upgrade() else { return };
        if inner.share.lock().await.active.is_some() {
            return;
        }
        let heaps = crate::stats::trim_memory(true);
        debug!(heaps, "trimmed working set after Go Live");
    });
}

/// 10 ms mono sine chunks into the mic source, honouring mute.
fn spawn_mic_tone(hz: f32, source: NativeAudioSource, muted: Arc<AtomicBool>, counters: Arc<StreamCounters>) -> ProducerThread {
    use futures_util::FutureExt;
    ProducerThread::spawn("vox-mic-tone", move |stop| {
        let mut gen = SineGenerator::new(MIX_RATE, hz as f64, hz as f64, 0.2);
        let mut stereo = vec![0i16; 960];
        let mut mono = vec![0i16; 480];
        paced(&stop, std::time::Duration::from_millis(10), || {
            gen.fill_stereo(&mut stereo);
            if muted.load(Ordering::Relaxed) {
                return;
            }
            for (m, f) in mono.iter_mut().zip(stereo.as_chunks::<2>().0) {
                *m = f[0];
            }
            let frame = AudioFrame { data: std::borrow::Cow::Borrowed(&mono), sample_rate: MIX_RATE, num_channels: 1, samples_per_channel: 480 };
            if let Some(Ok(())) = source.capture_frame(&frame).now_or_never() {
                counters.audio(&mono);
            }
        });
    })
}

async fn run_events(
    inner: Arc<Inner>,
    mut room_rx: mpsc::UnboundedReceiver<RoomEvent>,
    mut viewer_rx: mpsc::UnboundedReceiver<ViewerNotice>,
    mut mic_rx: mpsc::UnboundedReceiver<MicStatus>,
) {
    loop {
        tokio::select! {
            ev = room_rx.recv() => {
                let Some(ev) = ev else { break };
                if let RoomEvent::ParticipantPermissionChanged { participant: Participant::Local(_), .. } = &ev {
                    tokio::spawn(apply_mic_permission(inner.clone()));
                }
                if !handle_room_event(&inner, ev) {
                    break;
                }
            }
            n = viewer_rx.recv() => {
                if let Some(ViewerNotice::Closed { key }) = n {
                    inner.unwatch(&key, "viewer window closed");
                }
            }
            // (Without a microphone the sender is gone and this branch sits out.)
            Some(status) = mic_rx.recv() => {
                info!(?status, "microphone");
                if status != MicStatus::Lost && inner.mic_late.lock().is_some() {
                    tokio::spawn(publish_late_mic(inner.clone()));
                }
                let _ = inner.events.send(VoiceEvent::Mic(status));
            }
        }
    }
}

/// Returns false when the room is gone.
fn handle_room_event(inner: &Inner, ev: RoomEvent) -> bool {
    let emit = |e: VoiceEvent| {
        let _ = inner.events.send(e);
    };
    match ev {
        RoomEvent::ParticipantConnected(p) => {
            info!(identity = %ident(&p), "participant joined");
            inner.e2ee_input(|e, _| {
                e.connected(&ident(&p));
                Vec::new()
            });
            emit(VoiceEvent::ParticipantJoined { identity: ident(&p), name: p.name() });
        }
        RoomEvent::ParticipantDisconnected(p) => {
            let identity = ident(&p);
            info!(identity = %identity, "participant left");
            inner.unwatch(&identity, "participant left");
            inner.camera_gone(&identity);
            if inner.audio_only.lock().remove(&identity) {
                emit(VoiceEvent::ShareKind { identity: identity.clone(), audio_only: false });
            }
            inner.speakers.participant_left(&identity);
            inner.e2ee_input(|e, now| {
                e.left(&identity, now);
                Vec::new()
            });
            emit(VoiceEvent::ParticipantLeft { identity });
        }
        RoomEvent::TrackPublished { publication, participant } => {
            inner.on_publication(&publication, &participant);
            inner.refresh_kind(&participant);
            if publication.source() == TrackSource::Screenshare && !publication.is_muted() {
                info!(identity = %ident(&participant), "participant started Go Live");
                emit(VoiceEvent::ShareStarted { identity: ident(&participant) });
            }
        }
        RoomEvent::TrackUnpublished { publication, participant } => {
            inner.on_track_unsubscribed(&publication);
            inner.refresh_kind(&participant);
            if publication.source() == TrackSource::Microphone {
                inner.select_mic(&participant, Some(&publication.sid()));
            }
            if publication.source() == TrackSource::Camera {
                inner.select_camera(&participant);
            }
            if publication.source() == TrackSource::Screenshare {
                info!(identity = %ident(&participant), "participant stopped Go Live");
                emit(VoiceEvent::ShareStopped { identity: ident(&participant) });
            }
        }
        RoomEvent::TrackSubscribed { track, publication, participant } => {
            inner.on_track_subscribed(track, publication, participant);
        }
        RoomEvent::TrackUnsubscribed { publication, .. } => {
            inner.on_track_unsubscribed(&publication);
        }
        RoomEvent::TrackSubscriptionFailed { participant, error, track_sid } => {
            warn!(identity = %ident(&participant), %track_sid, error = %error, "track subscription failed");
        }
        RoomEvent::TrackMuted { participant: Participant::Remote(p), publication } | RoomEvent::TrackUnmuted { participant: Participant::Remote(p), publication }
            if publication.source() == TrackSource::Camera =>
        {
            inner.select_camera(&p);
        }
        RoomEvent::TrackMuted { participant: Participant::Remote(p), publication } | RoomEvent::TrackUnmuted { participant: Participant::Remote(p), publication }
            if publication.source() == TrackSource::ScreenshareAudio =>
        {
            // Which of the sharer's audio tracks is on (and whether the live is audio-only).
            inner.refresh_kind(&p);
            if inner.watched.lock().contains_key(ident(&p).as_str()) {
                select_share_audio(&p);
            }
        }
        RoomEvent::TrackMuted { participant: Participant::Remote(p), publication } if publication.source() == TrackSource::Screenshare => {
            inner.refresh_kind(&p);
            info!(identity = %ident(&p), "participant stopped Go Live (track muted)");
            emit(VoiceEvent::ShareStopped { identity: ident(&p) });
        }
        RoomEvent::TrackUnmuted { participant: Participant::Remote(p), publication } if publication.source() == TrackSource::Screenshare => {
            inner.refresh_kind(&p);
            info!(identity = %ident(&p), "participant started Go Live (track unmuted)");
            if inner.watched.lock().contains_key(ident(&p).as_str()) {
                select_share_video(&p);
            }
            emit(VoiceEvent::ShareStarted { identity: ident(&p) });
        }
        RoomEvent::DataReceived { participant: Some(p), topic, .. } if topic.as_deref() == Some(encoders::RX_HEALTH_TOPIC) && inner.e2ee_tx.is_some() && !inner.e2ee_keyed(&ident(&p)) => {
            // v1.1 (E1): in an encrypted room only somebody who gets our keys can judge how our
            // video decodes; a participant without them (an app that joined without encryption,
            // one that never said Hello) decodes nothing and must not push our encoder down.
            debug!(viewer = %ident(&p), "decode-health report from a participant without our keys ignored");
        }
        RoomEvent::DataReceived { payload, topic, participant: Some(p), .. } if topic.as_deref() == Some(encoders::RX_HEALTH_TOPIC) => {
            match encoders::RxHealthReport::from_payload(&payload) {
                Some(r) => {
                    debug!(viewer = %ident(&p), kind = ?r.kind, report = %r.describe(), "viewer decode-health report");
                    inner.rx_reports.lock().add(&ident(&p), r, Instant::now());
                }
                None => debug!(viewer = %ident(&p), bytes = payload.len(), "unreadable decode-health report ignored"),
            }
        }
        RoomEvent::DataReceived { payload, topic, participant: Some(p), .. } if topic.as_deref() == Some(vox_e2ee::media::TOPIC) => {
            inner.e2ee_input(|e, now| e.packet(&ident(&p), &payload, now));
        }
        RoomEvent::E2eeStateChanged { participant, state } => {
            if let Some(s) = crate::media_e2ee::MediaE2eeState::from_livekit(state) {
                let who = participant.identity().0.clone();
                inner.e2ee_input(|e, now| e.state(&who, s, now));
            }
        }
        RoomEvent::LocalTrackPublished { publication, .. } => {
            // v1.1 (E1, audit): in an encrypted room every track of ours goes out through a sender
            // cryptor (made by the SDK inside the publish). One without is taken down at once
            // rather than left sending in the clear.
            if inner.e2ee_tx.is_some() {
                let sid = publication.sid();
                let encrypted = inner.room.e2ee_manager().frame_cryptors().keys().any(|(who, s)| who.0 == inner.identity && *s == sid);
                if !encrypted {
                    warn!(%sid, "e2ee: our new track has no frame cryptor: unpublished");
                    let local = inner.room.local_participant();
                    tokio::spawn(async move {
                        let _ = local.unpublish_track(&sid).await;
                    });
                }
            }
            // A new sender cryptor starts at index 0: it gets ours.
            inner.e2ee_input(|e, _| e.local_published());
        }
        RoomEvent::ActiveSpeakersChanged { speakers } => {
            // Merged with the decoded levels and the soundboard; sent only when the set changes.
            // v1.1 (E1): the SFU's own list comes from the audio-level header extension, outside
            // the encrypted payload, so it keeps coming in an encrypted room (noted once, for the
            // e2v gate).
            if inner.e2ee_tx.is_some() && speakers.iter().any(|p| p.identity().0 != inner.identity) && !inner.e2ee_sfu_speakers.swap(true, Ordering::Relaxed) {
                info!(count = speakers.len(), "e2ee: the SFU's active speakers arrive in this encrypted room");
            }
            inner.speakers.set_active(speakers.iter().map(|p| p.identity().0.clone()));
        }
        RoomEvent::Connected { participants_with_tracks } => {
            for (p, pubs) in participants_with_tracks {
                for publication in pubs {
                    inner.on_publication(&publication, &p);
                }
                inner.refresh_kind(&p);
            }
        }
        RoomEvent::ConnectionStateChanged(state) => {
            info!(?state, "voice connection state");
        }
        // The SDK's own reconnects use this token, not the hub's join token (good for a minute
        // only, v1.0.1); LiveKit sends one as the session starts and then every few minutes.
        // The token itself is never logged.
        RoomEvent::TokenRefreshed { .. } => {
            info!("voice: LiveKit refreshed the session token");
        }
        RoomEvent::Reconnecting => {
            warn!("voice connection lost, reconnecting");
            emit(VoiceEvent::Reconnecting);
        }
        RoomEvent::Reconnected => {
            info!("voice connection restored");
            emit(VoiceEvent::Reconnected);
        }
        RoomEvent::Disconnected { reason } => {
            if format!("{reason:?}") == "ClientInitiated" {
                info!(?reason, "disconnected from voice room");
            } else {
                warn!(?reason, "disconnected from voice room");
            }
            emit(VoiceEvent::Disconnected { reason: format!("{reason:?}") });
            return false;
        }
        _ => {}
    }
    true
}

// ---------------------------------------------------------------- end-to-end encryption (v1.1 E1)

impl Inner {
    /// Feed the key exchange (nothing in a plain room) and carry out what it decides.
    fn e2ee_input(&self, f: impl FnOnce(&mut crate::media_e2ee::Engine, Instant) -> Vec<crate::media_e2ee::Action>) {
        let actions = match self.e2ee.lock().as_mut() {
            Some(e) => f(e, Instant::now()),
            None => return,
        };
        self.e2ee_apply(actions);
    }

    fn e2ee_apply(&self, actions: Vec<crate::media_e2ee::Action>) {
        use crate::media_e2ee::Action;
        for a in actions {
            match a {
                Action::Send { to, payload } => {
                    if let Some(tx) = &self.e2ee_tx {
                        let _ = tx.send((to, payload));
                    }
                }
                Action::SetKey { identity, idx, key } => match self.room.e2ee_manager().key_provider() {
                    Some(kp) => {
                        kp.set_key(&ParticipantIdentity(identity), idx as i32, key.0.to_vec());
                    }
                    None => warn!("e2ee: the room has no key provider"),
                },
                Action::UseIndex(idx) => {
                    self.e2ee_index.store(idx, Ordering::Relaxed);
                    self.e2ee_enforce_index();
                }
                Action::Event(e) => {
                    let _ = self.events.send(e);
                }
            }
        }
    }

    /// v1.1 (E1): `who` gets our media keys ([`crate::media_e2ee::Engine::keyed`]).
    fn e2ee_keyed(&self, who: &str) -> bool {
        self.e2ee.lock().as_ref().is_some_and(|e| e.keyed(who))
    }

    /// Every one of our sender cryptors on the current key index (LiveKit starts new ones at 0).
    fn e2ee_enforce_index(&self) {
        let idx = self.e2ee_index.load(Ordering::Relaxed) as i32;
        // New sender cryptors start on it too (made inside the publish, before its first frame:
        // index 0 may still hold an old key of ours that somebody who left has).
        self.room.e2ee_manager().set_sender_key_index(idx);
        // Test hook (`VOX_TEST_E2EE_RX_PLAIN=1`, background test mode only): this app's receivers
        // stop decrypting, so a gate can prove that what arrives is really encrypted (nothing of
        // it decodes then).
        let rx_plain = crate::testmode::background() && std::env::var("VOX_TEST_E2EE_RX_PLAIN").is_ok_and(|v| v == "1");
        for ((who, sid), c) in self.room.e2ee_manager().frame_cryptors() {
            if rx_plain && who.0 != self.identity && c.enabled() {
                c.set_enabled(false);
                warn!(%sid, who = %who.0, "test hook: receiver cryptor disabled (VOX_TEST_E2EE_RX_PLAIN)");
            }
            if who.0 == self.identity && c.key_index() != idx {
                c.set_key_index(idx);
                debug!(%sid, idx, "e2ee: sender cryptor moved to the current key index");
            }
        }
    }
}

/// v1.1 (E1): start the key exchange of an encrypted room (`rx`: its outgoing messages; `None` in
/// a plain room): my first key and the Hello, then the sender and the timers.
fn start_e2ee(inner: &Arc<Inner>, rx: Option<mpsc::UnboundedReceiver<(Vec<String>, Vec<u8>)>>) {
    let Some(mut rx) = rx else { return };
    let weak = Arc::downgrade(inner);
    // One task sends them all, in order (a key before the switch that needs it).
    tokio::spawn(async move {
        while let Some((to, payload)) = rx.recv().await {
            let Some(inner) = weak.upgrade() else { return };
            let local = inner.room.local_participant();
            drop(inner);
            let packet = DataPacket {
                payload,
                topic: Some(vox_e2ee::media::TOPIC.to_string()),
                reliable: true,
                destination_identities: to.iter().map(|t| ParticipantIdentity(t.clone())).collect(),
            };
            if let Err(e) = local.publish_data(packet).await {
                warn!(?to, error = %e, "e2ee: key exchange message not sent");
            }
        }
    });
    let present: Vec<String> = inner.room.remote_participants().keys().map(|k| k.0.clone()).collect();
    inner.e2ee_input(|e, now| e.start(&present, now));
    let weak = Arc::downgrade(inner);
    *inner.e2ee_tick.lock() = Some(tokio::spawn(async move {
        let mut n = 0u32;
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let Some(inner) = weak.upgrade() else { return };
            if inner.e2ee.lock().is_none() {
                return;
            }
            inner.e2ee_input(|e, now| e.tick(now));
            n = n.wrapping_add(1);
            if n % 5 == 0 {
                inner.e2ee_enforce_index();
            }
        }
    }));
}

// ---------------------------------------------------------------- server mute (item 12)

/// How the microphone is published (at the join, and again after a server unmute).
fn mic_publish_options() -> TrackPublishOptions {
    TrackPublishOptions {
        source: TrackSource::Microphone,
        audio_encoding: Some(AudioEncoding { max_bitrate: 32_000 }),
        dtx: true,
        red: true,
        ..Default::default()
    }
}

/// LiveKit's `TrackSource::MICROPHONE` on the wire.
const PROTO_SOURCE_MICROPHONE: i32 = 2;

/// Does our LiveKit permission let us publish a microphone? (An empty source list = any.)
fn mic_allowed(local: &LocalParticipant) -> bool {
    match local.permission() {
        None => true,
        Some(p) => p.can_publish && (p.can_publish_sources.is_empty() || p.can_publish_sources.contains(&PROTO_SOURCE_MICROPHONE)),
    }
}

/// The microphone while a moderator has us server-muted: LiveKit removed (or never accepted) the
/// track, so it waits here, still fed by the capture, until the permission comes back.
#[derive(Default)]
struct ServerMute {
    on: AtomicBool,
    held: Mutex<Option<(LocalAudioTrack, Arc<StreamCounters>)>>,
    /// One permission change applied at a time (they re-read the current permission).
    busy: tokio::sync::Mutex<()>,
}

impl ServerMute {
    fn hold(&self, track: LocalAudioTrack, counters: Arc<StreamCounters>) {
        *self.held.lock() = Some((track, counters));
        self.on.store(true, Ordering::Relaxed);
    }
}

/// A session that joined without a microphone: what publishing one takes.
struct LateMic {
    source: NativeAudioSource,
    counters: Arc<StreamCounters>,
    start_muted: bool,
}

/// A microphone opened in a session that joined without one: publish it, the way the join would
/// have, and lift the "no microphone" state. (A failed publish stays listen-only until the next
/// time the microphone changes.)
async fn publish_late_mic(inner: Arc<Inner>) {
    let _one = inner.server_mute.busy.lock().await;
    let Some(late) = inner.mic_late.lock().take() else { return };
    let track = LocalAudioTrack::create_audio_track("microphone", RtcAudioSource::Native(late.source.clone()));
    let local = inner.room.local_participant();
    let keep_muted = late.start_muted || inner.deafened.load(Ordering::Relaxed);
    if mic_allowed(&local) {
        let publishing = local.publish_track(LocalTrack::Audio(track.clone()), mic_publish_options());
        let publication = match tokio::time::timeout(PUBLISH_TIMEOUT, publishing).await {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                warn!(error = %e, "could not publish the microphone that showed up");
                *inner.mic_late.lock() = Some(late);
                return;
            }
            Err(_) => {
                warn!("the microphone that showed up: publish got no answer");
                *inner.mic_late.lock() = Some(late);
                return;
            }
        };
        if keep_muted {
            publication.mute();
        }
        *inner.mic.lock() = Some(MicPublished { publication, track, counters: late.counters });
    } else {
        inner.server_mute.hold(track, late.counters);
    }
    inner.muted_before_deafen.store(late.start_muted, Ordering::Relaxed);
    inner.muted.store(keep_muted, Ordering::Relaxed);
    inner.mic_unavailable.store(false, Ordering::Relaxed);
    info!("microphone published after the join");
    let _ = inner.events.send(VoiceEvent::MicAvailable);
}

/// Our LiveKit permission changed: follow what it says about the microphone. Taken away, the
/// (server-removed) publication is dropped locally too; given back, the mic is published again
/// with our own mute state.
async fn apply_mic_permission(inner: Arc<Inner>) {
    let _one = inner.server_mute.busy.lock().await;
    let allowed = mic_allowed(&inner.room.local_participant());
    let was_muted = inner.server_mute.on.load(Ordering::Relaxed);
    if allowed != was_muted {
        return; // nothing changed for the microphone
    }
    let local = inner.room.local_participant();
    if !allowed {
        let published = inner.mic.lock().take();
        if let Some(m) = published {
            if let Err(e) = local.unpublish_track(&m.publication.sid()).await {
                debug!(error = %e, "server mute: local unpublish of the microphone");
            }
            inner.server_mute.hold(m.track, m.counters);
        }
        inner.server_mute.on.store(true, Ordering::Relaxed);
        info!("server-muted by a moderator: microphone no longer published");
        let _ = inner.events.send(VoiceEvent::ServerMuted { muted: true });
        return;
    }
    let held = inner.server_mute.held.lock().take();
    if let Some((track, counters)) = held {
        let publishing = local.publish_track(LocalTrack::Audio(track.clone()), mic_publish_options());
        match tokio::time::timeout(PUBLISH_TIMEOUT, publishing).await {
            Ok(Ok(publication)) => {
                if inner.muted.load(Ordering::Relaxed) {
                    publication.mute();
                }
                *inner.mic.lock() = Some(MicPublished { publication, track, counters });
                info!("server unmute: microphone published again");
            }
            Ok(Err(e)) => {
                warn!(error = %e, "server unmute: could not publish the microphone again");
                *inner.server_mute.held.lock() = Some((track, counters));
            }
            Err(_) => {
                warn!("server unmute: microphone publish got no answer");
                *inner.server_mute.held.lock() = Some((track, counters));
            }
        }
    }
    inner.server_mute.on.store(false, Ordering::Relaxed);
    let _ = inner.events.send(VoiceEvent::ServerMuted { muted: false });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit (T0): a session pinned to the hub never sends its LiveKit token in the clear: a
    /// `ws://` URL (here pointing at a listener that would see it) is refused before any
    /// connection, and a `wss://` one without a pin is refused too.
    #[test]
    fn a_pinned_session_never_sends_the_voice_token_in_the_clear() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = l.local_addr().unwrap().port();
            for url in [format!("ws://127.0.0.1:{port}"), format!("WS://127.0.0.1:{port}"), format!("http://127.0.0.1:{port}")] {
                let cfg = VoiceConfig { tls_pin: Some([7u8; 32]), ..Default::default() };
                let err = match VoiceSession::connect(&url, "the-livekit-token", cfg).await {
                    Ok(_) => panic!("{url}: joined in the clear"),
                    Err(e) => format!("{e:#}"),
                };
                assert!(err.contains("not wss://"), "{url}: {err}");
            }
            let err = match VoiceSession::connect(&format!("wss://127.0.0.1:{port}"), "the-livekit-token", VoiceConfig::default()).await {
                Ok(_) => panic!("wss without a pin"),
                Err(e) => format!("{e:#}"),
            };
            assert!(err.contains("certificate pin"), "{err}");
            let nobody = tokio::time::timeout(Duration::from_millis(300), l.accept()).await;
            assert!(nobody.is_err(), "something connected: {nobody:?}");
        });
    }

    fn facts(items: &[(TrackSource, &str, bool)]) -> Vec<PubFacts> {
        items.iter().map(|(s, n, m)| (*s, n.to_string(), *m)).collect()
    }

    #[test]
    fn one_microphone_per_participant() {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(pick_mic(None, &[]), None);
        assert_eq!(pick_mic(None, &v(&["TR_b", "TR_a", "TR_c"])), Some(1), "the first by sid");
        // Extra ones published later do not take the place of the one playing.
        assert_eq!(pick_mic(Some("TR_b"), &v(&["TR_b", "TR_a", "TR_c"])), Some(0));
        let many: Vec<String> = (0..500).map(|i| format!("TR_{i:04}")).collect();
        assert_eq!(pick_mic(Some("TR_0250"), &many), Some(250), "hundreds of them: still one");
        // The playing one is unpublished: the next one takes over.
        assert_eq!(pick_mic(Some("TR_gone"), &v(&["TR_x", "TR_w"])), Some(1));
    }

    #[test]
    fn audio_only_live_is_the_named_track_without_video() {
        use TrackSource::{Screenshare as V, ScreenshareAudio as A};
        // Nothing published: not live.
        assert!(!audio_only_live(&[]));
        // The audio-only track is on, no video publication at all.
        assert!(audio_only_live(&facts(&[(A, AUDIO_ONLY_TRACK_NAME, false)])));
        // Leftovers of earlier screen shares are muted: still audio-only.
        assert!(audio_only_live(&facts(&[(V, "screenshare", true), (V, "screenshare", true), (A, "screenshare_audio", true), (A, AUDIO_ONLY_TRACK_NAME, false)])));
        // A screen share is on (with its own audio, or the audio-only track still unmuted).
        assert!(!audio_only_live(&facts(&[(V, "screenshare", false), (A, "screenshare_audio", false)])));
        assert!(!audio_only_live(&facts(&[(V, "screenshare", false), (A, AUDIO_ONLY_TRACK_NAME, false)])));
        // The audio-only track is muted (between shares, or a screen share's own sound is on).
        assert!(!audio_only_live(&facts(&[(A, AUDIO_ONLY_TRACK_NAME, true)])));
        assert!(!audio_only_live(&facts(&[(A, "screenshare_audio", false)])));
        // Other sources never count.
        assert!(!audio_only_live(&facts(&[(TrackSource::Microphone, AUDIO_ONLY_TRACK_NAME, false)])));
    }
}
