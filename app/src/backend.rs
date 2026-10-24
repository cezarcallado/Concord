//! Everything that talks to the network or the media stack, on its own threads.
//!
//! The UI sends [`Cmd`]s through an unbounded channel and drains [`Ev`]s from a std channel;
//! after every event the egui context is woken (`request_repaint`) so the UI repaints only when
//! there is something new. All logic is the vox-client-core API, driven the same way as in
//! `vox-cli`.

use std::collections::HashMap;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{info, warn};
use vox_capture::{MonitorInfo, WindowInfo};
use vox_client_core::audio::device::{default_device_name, list_devices};
use vox_client_core::audio::MicStatus;
use vox_client_core::golive::{capture_call, SwitchRequest};
use vox_client_core::hub::{HubClient, HubEvent, HubSender};
use vox_client_core::stats::Reporter;
use vox_client_core::{
    AlertKind, JoinError, JoinStep, MicTest, MicTestConfig, NoiseControl, ShareOptions, ShareSource, UiPlayer, UserAudio, VoiceConfig, VoiceEvent, VoiceGate,
    VoiceSession, WatchLimit,
};
use vox_proto::{ChannelId, ClientEvent, Message, MessageId, ServerEvent, User, UserId, PROTOCOL_VERSION, WATCH_STATE_REJECTED};

mod camera;
pub mod e2ee;
mod sounds;
mod thumbs;
pub mod tls;

pub use camera::CameraLiveInfo;
pub use tls::{PinSource, TlsPlan};
use vox_client_core::tls::Pin;

/// Voice options captured when joining.
#[derive(Debug, Clone, Default)]
pub struct VoiceOpts {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub volume_pct: u32,
    pub ptt: bool,
    /// Saved local volume / mute per participant identity.
    pub user_audio: HashMap<String, UserAudio>,
    pub others_muted: bool,
    /// The input gate / level meter of the voice session (owned by the UI).
    pub gate: Arc<VoiceGate>,
    /// Noise suppression level, shared with the mic test (owned by the UI; changes apply live).
    pub noise: Arc<NoiseControl>,
}

#[derive(Debug)]
pub enum Cmd {
    Connect {
        url: String,
        user: String,
        password: String,
        /// Item 12: the name's personal password (or a reset code), when one was typed.
        personal: Option<String>,
        /// This computer's remembered login token for the name, if any.
        device_token: Option<String>,
        /// Ask the hub to remember this computer (answer: `LoggedIn::device_token`).
        remember_device: bool,
        /// v0.8: joining with an invite code (the token, and the addresses to try in order:
        /// the first that answers is used instead of `url`). v0.9.1: a saved server's addresses
        /// (no token).
        invite: Option<crate::host::InviteJoin>,
        /// v1.0.1: which of the secrets above were saved on this computer, and for which server
        /// (they are sent only to that server, see [`Guard`]).
        guard: Guard,
        /// v1.1 (T0): the certificate the server must have, and where the address came from (the
        /// trust rules, `backend/tls.rs`).
        tls: TlsPlan,
    },
    Logout,
    /// v0.8: an invite call (create, list, revoke); answer: `Ev::Invite`.
    Invite(crate::host::InviteCmd),
    /// An item-12 call (account, moderation, administration); answer: `Ev::Admin`.
    Admin(crate::roles::AdminCmd),
    /// The server soundboard (answers: `Ev::Sound`).
    Sound(crate::soundboard::SoundCmd),
    /// v1.1 (E0): the device list of my account, "Remover" (answers: `Ev::E2ee`).
    E2ee(e2ee::E2eeCmd),
    LoadHistory {
        channel: ChannelId,
        before: Option<MessageId>,
    },
    /// A profile-picture call (answer: `Ev::Avatar`).
    Avatar(crate::avatars::AvatarCmd),
    /// A profile call: load someone's, edit mine, presence, status (answer: `Ev::Profile`).
    Profile(crate::profile::ProfileCmd),
    /// Ask the hub for the preview card of a link (answer: `Ev::Preview`).
    LinkPreview {
        url: String,
    },
    /// Fetch the thumbnail of an already-previewed link (answer: `Ev::PreviewImage`).
    LinkPreviewImage {
        url: String,
    },
    /// Someone's Go Live hover preview (v1.1; answer: `Ev::StreamCard`). `have_icon`: the icon
    /// hash already cached.
    StreamPreview {
        user: UserId,
        have_icon: Option<String>,
    },
    /// "Prévia da minha transmissão ao passar o mouse" (v1.1): may my live's still picture go to
    /// the hub? Sent at start and on every change; off mid-live drops the hub's copy.
    StreamPreviewUpload(bool),
    SendMessage {
        channel: ChannelId,
        content: String,
        /// Mentions and the reply (v0.8; empty = a plain message, the JSON of before).
        extras: crate::app::chat_extras::SendExtras,
    },
    /// Put my reaction on a message, or take it off (the hub's event updates the chat).
    React {
        target: crate::chat_images::Target,
        message: i64,
        emoji: String,
        add: bool,
    },
    JoinVoice {
        channel: ChannelId,
        opts: VoiceOpts,
    },
    /// Direct messages (answer: `Ev::Dm`).
    Dm(crate::app::dms::DmCmd),
    /// Chat images: limits, downloads, upload-and-send (answers: `Ev::Image`).
    Image(crate::chat_images::ImageCmd),
    /// Join the private room of DM `dm`'s call (ring it first, unless `accept`: answer it). The
    /// session is a voice session like a channel's, as the "channel" `DM_CALL_CHANNEL`.
    JoinDmCall {
        dm: vox_proto::DmId,
        accept: bool,
        opts: VoiceOpts,
    },
    LeaveVoice,
    SetMuted(bool),
    SetDeafened(bool),
    SetVolume(u32),
    /// Local volume / mute of one participant (identity = user id), mic and stream.
    SetUserAudio {
        identity: String,
        audio: UserAudio,
    },
    /// "Silenciar todos": silence every other participant locally.
    SetOthersMuted(bool),
    /// Play a local alert sound (only the user hears it). `volume` is the alert gain (0..=1),
    /// `device` the selected output. `standalone` skips the voice session's mixer (used for the
    /// "left the room" chime, because the session is being torn down).
    PlayAlert {
        kind: AlertKind,
        volume: f32,
        device: Option<String>,
        standalone: bool,
    },
    ListSources,
    ListDevices,
    StartShare {
        source: ShareSource,
        opts: ShareOptions,
    },
    StopShare,
    /// Change what the running Go Live captures, without ending it.
    SwitchShare {
        source: ShareSource,
        audio: bool,
    },
    /// Watch one more stream (item 9: several at once, up to `MAX_WATCHED`).
    Watch(String),
    /// Stop watching one stream; the others go on.
    Unwatch(String),
    /// A watched stream's video is on screen somewhere or not (hidden: paused at the SFU).
    StreamVisible {
        identity: String,
        visible: bool,
    },
    /// Silence a watched stream's audio while another one is in focus (or undo it).
    StreamFocusMuted {
        identity: String,
        muted: bool,
    },
    /// Turn the camera on: the saved device (by id, else by name, else the default camera).
    StartCamera {
        id: Option<String>,
        name: Option<String>,
        preset: crate::settings::CameraPreset,
    },
    StopCamera,
    /// The camera tiles are on screen (call view shown, window not minimized).
    CamerasVisible(bool),
    /// "Ocultar câmera para mim": hide / show one person's camera (voice identity) only for me.
    CameraHidden { identity: String, hidden: bool },
    /// Preview of our own Go Live in our stream tile (item 8a): on while the tile is visible,
    /// faster and bigger when it is focused.
    SharePreview(vox_capture::PreviewConfig),
    ListCameras,
    /// Settings' camera preview: `Some` = start (or restart) it with that camera, `None` = stop.
    CameraPreview(Option<(Option<String>, Option<String>, crate::settings::CameraPreset)>),
    /// The saved setup (native format, image controls, mirror / rotation / zoom) of the camera
    /// chosen in the settings (v0.6). Changes go to the running camera at once; the rest waits
    /// for the next start.
    CameraSetup {
        id: Option<String>,
        name: Option<String>,
        setup: vox_client_core::golive::camera::CameraSetup,
    },
    /// "Restaurar padrão": every image control of that camera back to its default.
    CameraResetControls {
        id: Option<String>,
        name: Option<String>,
    },
    /// The native formats of that camera (answer: `Ev::CameraFormats` with the same `key`).
    ListCameraFormats {
        id: Option<String>,
        name: Option<String>,
        key: String,
    },
    /// "Configurar câmera" is open (v0.8.1): read the running camera's controls back once a
    /// second, so automatic values (exposure, white balance...) show as they move.
    CameraWatch(bool),
    /// Read the running camera's controls back now (after the driver's own dialog closed).
    CameraRefreshControls,
    /// "Configurações avançadas do fabricante…": the driver's property pages for that camera
    /// (answer: `Ev::CameraNativeClosed` once the dialog is closed).
    CameraNativeDialog {
        id: Option<String>,
        name: Option<String>,
    },
    /// Push-to-talk mode on/off (while on, the mic is only open while the key is held).
    SetPtt(bool),
    PttKey(bool),
    /// Another microphone was picked in the settings (`None`: the system default): a call in
    /// progress follows it at once.
    SetInputDevice(Option<String>),
    /// "Testar microfone": open the mic and play it back locally with the gate applied.
    /// Nothing is sent to the room, even when in voice.
    StartMicTest {
        input_device: Option<String>,
        output_device: Option<String>,
        volume_pct: u32,
        /// The test's own gate / meter (not the voice session's).
        gate: Arc<VoiceGate>,
        noise: Arc<NoiseControl>,
    },
    StopMicTest,
    Stats,
    Quit,
}

#[derive(Debug)]
pub enum Ev {
    ConnectFailed(String),
    /// Item 12: this name needs its personal password (none was given, it was wrong, or the
    /// remembered device is no longer valid). `forget_device`: drop the saved device token.
    NeedPersonalPassword {
        message: String,
        forget_device: bool,
    },
    /// v1.0.1: the address that answered is not the server the saved password / login token
    /// belong to (or that could not be confirmed): nothing saved was sent. The form asks for the
    /// credentials again.
    NotSameServer(String),
    /// Login done; the hub snapshot follows as `Hub(Server(Ready))`.
    LoggedIn {
        server_name: String,
        user: User,
        url: String,
        livekit_ok: bool,
        /// A new login token for this computer (keep it, encrypted).
        device_token: Option<String>,
        /// v1.0.1: the hub's identity (`None`: an older hub).
        server_id: Option<String>,
        /// v1.0.1: this login confirmed that `server_id` is the server of the saved entry
        /// (a matching id, the address the user typed or picked, or an accepted invite), so the
        /// entry may keep it.
        pin: bool,
        /// v1.1 (T0): the certificate this session is pinned to (`None`: plain HTTP, an older
        /// hub: the open lock).
        tls_pin: Option<Pin>,
        /// v1.1 (T0): `tls_pin` was seen on this login for the first time (kept only when the
        /// login confirmed the server, `pin`).
        tls_learned: bool,
    },
    /// v1.1 (T0): the server answered with another certificate than the one saved / in the
    /// invite. Nothing was sent. "A identidade do servidor mudou".
    TlsMismatch {
        url: String,
        expected: Pin,
        got: Pin,
    },
    /// v1.1 (T0): a typed address without TLS (an older hub): ask before plain HTTP.
    PlainPrompt {
        url: String,
    },
    /// The soundboard: the list, who played what, upload results.
    Sound(crate::soundboard::SoundEv),
    /// v1.1 (E0): this computer's identity on the server's device list, and new devices of mine.
    E2ee(e2ee::E2eeEv),
    /// The answer to a `Cmd::Admin`.
    Admin {
        cmd: crate::roles::AdminCmd,
        result: Result<crate::roles::AdminReply, String>,
    },
    /// The answer to a `Cmd::Invite`.
    Invite(crate::host::InviteEv),
    /// The session is gone for good (token rejected and re-login failed).
    LoggedOut {
        reason: Option<String>,
    },
    Hub(HubEvent),
    /// Whether the hub understands `WatchState` (false: an older server; hide the viewer UI).
    /// Sent with every hub snapshot (true) and again (false) when this hub rejects it.
    ViewersSupported(bool),
    History {
        channel: ChannelId,
        /// The page's cursor (`None`: the newest page).
        before: Option<MessageId>,
        messages: Vec<Message>,
    },
    Avatar(crate::avatars::AvatarEv),
    /// Go Live hover cards (v1.1): whether the hub keeps previews, and the answers.
    StreamCard(crate::stream_cards::CardEv),
    Profile(crate::profile::ProfileEv),
    /// Direct messages: the answers to `Cmd::Dm`.
    Dm(crate::app::dms::DmEv),
    /// Chat images: the answers to `Cmd::Image`.
    Image(crate::chat_images::ImageEv),
    Preview {
        url: String,
        reply: vox_client_core::hub::PreviewReply,
    },
    PreviewImage {
        url: String,
        bytes: Option<Vec<u8>>,
    },
    VoiceJoining(ChannelId),
    VoiceJoined {
        channel: ChannelId,
        identity: String,
        /// The join worked but something optional did not (no mic, no speakers): shown in the
        /// voice bar for as long as the session lasts.
        warning: Option<String>,
        /// Listen-only: the microphone could not be used.
        no_mic: bool,
        /// v1.1 (E1): the room's media is end-to-end encrypted.
        e2ee: bool,
    },
    /// v1.1 (E1): somebody joined our encrypted call from a device that appeared under their
    /// account after this computer first knew them ("Fulano entrou de um aparelho novo").
    CallNewDevice { identity: String },
    VoiceFailed {
        /// Channel to offer a retry for.
        channel: Option<ChannelId>,
        message: String,
    },
    VoiceLeft,
    Voice(VoiceEvent),
    /// Our own mic / headphones state, after any change.
    LocalVoice {
        muted: bool,
        deafened: bool,
    },
    Sources {
        windows: Vec<WindowInfo>,
        monitors: Vec<MonitorInfo>,
        error: Option<String>,
    },
    Devices {
        inputs: Vec<String>,
        outputs: Vec<String>,
        default_input: Option<String>,
        default_output: Option<String>,
    },
    ShareStarted {
        encoder: String,
    },
    ShareFailed(String),
    ShareStopped,
    /// The running Go Live now captures another source (it never stopped).
    ShareSwitched {
        audio: bool,
        /// Why the new source's audio is missing although requested.
        audio_error: Option<String>,
    },
    /// The switch did not happen; the live goes on with its previous source (or ends by itself,
    /// which arrives as `LocalShareEnded`).
    SwitchFailed(String),
    Notice(String),
    /// The mic test opened (`warning`: the speakers did not, only the meter works).
    MicTestStarted {
        warning: Option<String>,
    },
    /// The mic test could not start, or ended on its own.
    MicTestFailed(String),
    /// (label, frames, width, height) per media stream, for diagnostics / autotest.
    StreamInfo(Vec<(String, u64, u32, u32)>),
    /// Audio streams of the last stats report, for the autotest: what arrived (or went out) and
    /// whether it is stereo. Sent just before `StreamInfo`.
    AudioInfo(Vec<AudioInfo>),
    CameraStarted {
        encoder: String,
        /// e.g. "HD Pro Webcam C920 · MJPG 1280x720 @ 30".
        info: String,
    },
    CameraFailed(String),
    CameraStopped,
    Cameras {
        list: Vec<vox_capture::CameraInfo>,
        error: Option<String>,
    },
    /// The settings preview started (`Ok`) or could not (`Err`, Portuguese).
    CameraPreview(Result<(), String>),
    /// The native formats of the camera asked about with `Cmd::ListCameraFormats`.
    CameraFormats {
        key: String,
        list: Vec<vox_capture::CameraMode>,
    },
    /// What the running camera (call, else the preview) reports now.
    CameraLive(CameraLiveInfo),
    /// No camera is running any more.
    CameraLiveOff,
    /// The driver's settings dialog was closed (`Err`: it could not be opened, Portuguese).
    CameraNativeClosed(Result<(), String>),
}

/// One audio stream of the stats report (see `Ev::AudioInfo`); levels are since the previous
/// report.
#[derive(Debug, Clone, Default)]
pub struct AudioInfo {
    pub label: String,
    /// Chunks counted so far.
    pub chunks: u64,
    pub rms_dbfs: Option<f64>,
    /// RMS of `(L - R) / 2`: `None` = both channels identical (mono) or silence.
    pub side_dbfs: Option<f64>,
    pub kbps: Option<f64>,
    /// Negotiated codec ("audio/opus 48000 Hz ch=2 ... stereo=1").
    pub codec: String,
    /// v1.1 (E1, autotests): the longest stretch without sound since the previous report (ms).
    pub max_quiet_ms: u64,
}

/// Wakes the egui loop from backend threads.
#[derive(Clone, Default)]
pub struct Repaint(Arc<OnceLock<eframe::egui::Context>>, Arc<std::sync::atomic::AtomicBool>);

impl Repaint {
    pub fn set(&self, ctx: eframe::egui::Context) {
        let _ = self.0.set(ctx);
    }

    /// The egui context (set once the window exists).
    pub fn ctx(&self) -> Option<eframe::egui::Context> {
        self.0.get().cloned()
    }

    pub fn wake(&self) {
        if let Some(c) = self.0.get() {
            c.request_repaint();
        }
    }

    /// Like [`wake`](Self::wake), but for one pass only: egui answers a plain repaint request
    /// with two passes (to let things settle), which for a 60 fps video doubles the work.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn wake_once(&self) {
        if let Some(c) = self.0.get() {
            c.request_repaint_after(std::time::Duration::from_millis(1));
        }
    }

    /// The window is hidden in the tray (set by the app): nobody sees the meters.
    pub fn set_hidden(&self, hidden: bool) {
        self.1.store(hidden, std::sync::atomic::Ordering::Relaxed);
    }

    /// Like [`wake`](Self::wake), but not while the window is minimized or hidden in the tray
    /// (nobody sees the meter).
    pub fn wake_if_shown(&self) {
        if let Some(c) = self.0.get() {
            if !self.1.load(std::sync::atomic::Ordering::Relaxed) && !crate::app::window_minimized(c) {
                c.request_repaint();
            }
        }
    }
}

#[derive(Clone)]
struct Out {
    tx: std_mpsc::Sender<Ev>,
    repaint: Repaint,
}

impl Out {
    fn send(&self, ev: Ev) {
        if self.tx.send(ev).is_ok() {
            self.repaint.wake();
        }
    }
}

/// UI-side handle.
pub struct Backend {
    pub tx: mpsc::UnboundedSender<Cmd>,
    pub rx: std_mpsc::Receiver<Ev>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Backend {
    pub fn start(repaint: Repaint) -> Backend {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = std_mpsc::channel();
        let out = Out { tx: ev_tx, repaint };
        let thread = std::thread::Builder::new()
            .name("vox-backend".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_name("vox-rt")
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                rt.block_on(Controller::new(cmd_rx, out).run());
                // Give lingering tasks (room close, capture teardown) a moment, then drop.
                rt.shutdown_timeout(Duration::from_secs(2));
            })
            .expect("spawn backend thread");
        Backend {
            tx: cmd_tx,
            rx: ev_rx,
            thread: Some(thread),
        }
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }

    /// Leave voice, stop sharing and stop the runtime (waits a few seconds at most).
    pub fn shutdown(&mut self) {
        let _ = self.tx.send(Cmd::Quit);
        if let Some(t) = self.thread.take() {
            let deadline = std::time::Instant::now() + Duration::from_secs(6);
            while !t.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if t.is_finished() {
                let _ = t.join();
            } else {
                warn!("backend did not stop in time; exiting anyway");
            }
        }
    }
}

// ---------------------------------------------------------------- controller

type JoinOutcome = Result<(VoiceSession, mpsc::UnboundedReceiver<VoiceEvent>), String>;

enum Internal {
    Hub(u64, HubEvent),
    Voice(u64, VoiceEvent),
    Relogin(u64),
    /// A background voice join finished (`join_gen`, channel, result). The error is already the
    /// user-facing Portuguese message.
    Joined(u64, ChannelId, JoinOutcome),
    /// The mic test finished opening its devices (`mic_test_gen`, result).
    MicTestOpened(u64, Result<MicTest, String>),
    /// The sound list, fetched after a hub snapshot (`hub_gen`, answer).
    Sounds(u64, vox_client_core::hub::SoundsReply),
    /// A soundboard clip that had to be downloaded before it could be played.
    LateSound(sounds::LateSound),
}

#[derive(Clone)]
struct Creds {
    /// Item 12: what proves the name (for re-logins after a hub restart or an expired session).
    personal: Option<String>,
    device_token: Option<String>,
    user: String,
    password: String,
    /// v0.8: the invite token of the first login (re-logins use the device token it brings).
    invite: Option<String>,
    /// v1.0.1: the identity of the hub these were sent to; re-logins check it first.
    server_id: Option<String>,
}

struct Controller {
    cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    int_tx: mpsc::UnboundedSender<Internal>,
    int_rx: mpsc::UnboundedReceiver<Internal>,
    out: Out,
    no_mic: bool,
    no_playback: bool,
    /// The microphone picked in the settings: a join in flight opens what it was started with,
    /// and this is what the session must be using once it is up.
    input_device: Option<String>,

    creds: Option<Creds>,
    hub: Option<Arc<HubClient>>,
    hub_tx: Option<HubSender>,
    hub_gen: u64,

    voice: Option<VoiceSession>,
    /// The join in flight, if any. It runs as its own task so the controller (chat, leave,
    /// cancel, quit) stays responsive however long the network takes.
    join_task: Option<tokio::task::JoinHandle<()>>,
    join_gen: u64,
    voice_gen: u64,
    voice_channel: Option<ChannelId>,
    streaming: bool,
    /// The running live is audio-only ("Só áudio"); announced as `audio_only` while `streaming`.
    share_audio_only: bool,
    /// Our webcam is on in the current voice session (announced to the hub as `camera`).
    camera: bool,
    /// Streamers (user ids) this client is watching, as announced to the hub with `WatchState`.
    watch_set: Vec<UserId>,
    /// What the hub was last told on this connection (None: nothing yet / new connection).
    watch_sent: Option<Vec<UserId>>,
    watch_sent_at: Option<Instant>,
    /// False for this hub connection once it answered `WatchState` with "invalid event".
    watch_supported: bool,
    /// After a snapshot: say it once more when a watched streamer shows up live (a restarted hub
    /// may have heard us before it heard them, and then ignored the entry).
    watch_heal: bool,
    ptt: bool,
    ptt_down: bool,
    /// The gate of the current voice session (to switch push-to-talk on and off).
    voice_gate: Option<Arc<VoiceGate>>,
    mic_test: Option<MicTest>,
    mic_test_gen: u64,
    /// Output volume (percent) the mic test was last started with / the user last chose.
    volume_pct: u32,
    reporter: Reporter,
    /// The camera tiles are on screen (applied to every new voice session).
    cams_visible: bool,
    cams_hidden: std::collections::HashSet<String>,
    /// What the UI last asked of the Go Live preview (applied to every new voice session).
    share_preview: vox_capture::PreviewConfig,
    camera_preview: Option<vox_client_core::golive::camera::CameraPreview>,
    camera_preview_gen: u64,
    /// Saved camera setup and what the running camera reports (see `backend/camera.rs`).
    cam: camera::CamBackend,
    /// Alert sounds while not in a voice channel (opens the output lazily).
    ui_player: UiPlayer,
    /// The server soundboard (see `backend/sounds.rs`).
    sb: sounds::SoundState,
    /// Go Live hover pictures (see `backend/thumbs.rs`).
    thumbs: thumbs::ThumbState,
    /// v1.1 (E0): this computer's identity key on the server (see `backend/e2ee.rs`).
    e2ee: e2ee::E2eeBackend,
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).map(|v| !v.is_empty() && v != "0").unwrap_or(false)
}

impl Controller {
    fn new(cmd_rx: mpsc::UnboundedReceiver<Cmd>, out: Out) -> Self {
        let (int_tx, int_rx) = mpsc::unbounded_channel();
        let no_mic = env_flag("VOX_NO_MIC");
        let no_playback = env_flag("VOX_NO_PLAYBACK");
        if no_mic || no_playback {
            info!(no_mic, no_playback, "audio devices disabled by environment");
        }
        Controller {
            cmd_rx,
            int_tx,
            int_rx,
            out,
            no_mic,
            no_playback,
            input_device: None,
            creds: None,
            hub: None,
            hub_tx: None,
            hub_gen: 0,
            voice: None,
            join_task: None,
            join_gen: 0,
            voice_gen: 0,
            voice_channel: None,
            streaming: false,
            share_audio_only: false,
            camera: false,
            watch_set: Vec::new(),
            watch_sent: None,
            watch_sent_at: None,
            watch_supported: true,
            watch_heal: false,
            ptt: false,
            ptt_down: false,
            voice_gate: None,
            mic_test: None,
            mic_test_gen: 0,
            volume_pct: 100,
            reporter: Reporter::new(),
            cams_visible: true,
            cams_hidden: Default::default(),
            share_preview: vox_capture::PreviewConfig::default(),
            camera_preview: None,
            camera_preview_gen: 0,
            cam: Default::default(),
            ui_player: UiPlayer::new(),
            sb: Default::default(),
            thumbs: Default::default(),
            e2ee: Default::default(),
        }
    }

    async fn run(mut self) {
        let mut stats_tick = tokio::time::interval(vox_log::STATS_INTERVAL);
        stats_tick.tick().await;
        let mut cam_tick = tokio::time::interval(Duration::from_secs(1));
        cam_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => match cmd {
                    Some(Cmd::Quit) | None => break,
                    Some(cmd) => self.on_cmd(cmd).await,
                },
                Some(ev) = self.int_rx.recv() => self.on_internal(ev).await,
                _ = stats_tick.tick() => {
                    if self.voice.is_some() {
                        self.report_stats().await;
                    }
                }
                _ = cam_tick.tick() => self.camera_tick().await,
            }
        }
        self.cancel_join();
        self.mic_test_gen += 1;
        drop(self.mic_test.take());
        self.stop_camera_preview().await;
        self.leave_voice(false).await;
        self.hub_tx = None;
        self.hub = None;
        info!("backend stopped");
    }

    // ------------------------------------------------------------ commands

    async fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Connect { url, user, password, personal, device_token, remember_device, invite, guard, tls } => {
                let (url, invite) = match invite {
                    Some(join) => (pick(&join.urls, &guard, &tls).await.unwrap_or(url), join.token),
                    None => (url, None),
                };
                let creds = Creds { personal, device_token, user, password, invite, server_id: None };
                self.connect(url, creds, remember_device, &guard, &tls).await
            }
            Cmd::Admin(cmd) => self.admin(cmd).await,
            Cmd::Invite(cmd) => self.invite(cmd),
            Cmd::Sound(cmd) => self.sound_cmd(cmd),
            Cmd::E2ee(cmd) => self.e2ee_cmd(cmd),
            Cmd::Logout => {
                self.teardown().await;
                self.out.send(Ev::LoggedOut { reason: None });
            }
            Cmd::LoadHistory { channel, before } => {
                let Some(hub) = self.hub.clone() else { return };
                let out = self.out.clone();
                tokio::spawn(async move {
                    match hub.messages(channel, before, 50).await {
                        Ok(messages) => out.send(Ev::History { channel, before, messages }),
                        Err(e) => {
                            warn!(error = %format!("{e:#}"), channel, "loading history failed");
                            out.send(Ev::Notice("Não foi possível carregar as mensagens.".into()));
                        }
                    }
                });
            }
            Cmd::Avatar(cmd) => {
                let out = self.out.clone();
                let Some(hub) = self.hub.clone() else {
                    out.send(Ev::Avatar(cmd.offline()));
                    return;
                };
                tokio::spawn(async move {
                    let ev = crate::avatars::run(&hub, cmd).await;
                    out.send(Ev::Avatar(ev));
                });
            }
            Cmd::Profile(cmd) => {
                let out = self.out.clone();
                let Some(hub) = self.hub.clone() else {
                    out.send(Ev::Profile(cmd.offline()));
                    return;
                };
                tokio::spawn(async move {
                    let ev = crate::profile::run(&hub, cmd).await;
                    out.send(Ev::Profile(ev));
                });
            }
            Cmd::LinkPreview { url } => {
                let out = self.out.clone();
                let Some(hub) = self.hub.clone() else {
                    out.send(Ev::Preview { url, reply: vox_client_core::hub::PreviewReply::Failed("offline".into()) });
                    return;
                };
                tokio::spawn(async move {
                    let reply = hub.link_preview(&url).await;
                    out.send(Ev::Preview { url, reply });
                });
            }
            Cmd::StreamPreview { user, have_icon } => self.stream_card_fetch(user, have_icon),
            Cmd::StreamPreviewUpload(on) => self.thumbs_allow(on),
            Cmd::LinkPreviewImage { url } => {
                let out = self.out.clone();
                let Some(hub) = self.hub.clone() else {
                    out.send(Ev::PreviewImage { url, bytes: None });
                    return;
                };
                tokio::spawn(async move {
                    let bytes = hub.link_preview_image(&url).await;
                    out.send(Ev::PreviewImage { url, bytes });
                });
            }
            Cmd::SendMessage { channel, content, extras } => {
                let sent = self.hub_tx.as_ref().map(|tx| {
                    tx.send(ClientEvent::SendMessage {
                        channel_id: channel,
                        content,
                        mentions: extras.mentions,
                        reply_to: extras.reply_to,
                        reply_mention: extras.reply_mention,
                    })
                });
                if !matches!(sent, Some(Ok(()))) {
                    self.out
                        .send(Ev::Notice("Mensagem não enviada: sem conexão com o servidor.".into()));
                }
            }
            Cmd::React { target, message, emoji, add } => {
                let Some(hub) = self.hub.clone() else { return };
                let out = self.out.clone();
                tokio::spawn(async move {
                    let r = match target {
                        crate::chat_images::Target::Channel(_) => hub.react(message, &emoji, add).await,
                        crate::chat_images::Target::Dm(dm) => hub.dm_react(dm, message, &emoji, add).await,
                    };
                    if let Err(e) = r {
                        warn!(error = %format!("{e:#}"), "reaction failed");
                        out.send(Ev::Notice(crate::app::chat_extras::reaction_error_text(&e)));
                    }
                });
            }
            Cmd::JoinVoice { channel, opts } => self.join_voice(channel, opts).await,
            Cmd::JoinDmCall { dm, accept, opts } => self.join_target(crate::app::dms::DM_CALL_CHANNEL, JoinVia::Dm { dm, accept }, opts).await,
            Cmd::Dm(cmd) => {
                let out = self.out.clone();
                let Some(hub) = self.hub.clone() else {
                    out.send(Ev::Dm(cmd.offline()));
                    return;
                };
                let e2 = self.e2ee.clone();
                tokio::spawn(async move {
                    let ev = crate::app::dms::run(&hub, &e2, cmd).await;
                    out.send(Ev::Dm(ev));
                });
            }
            Cmd::Image(cmd) => {
                let out = self.out.clone();
                let Some(hub) = self.hub.clone() else {
                    out.send(Ev::Image(cmd.offline()));
                    return;
                };
                let e2 = self.e2ee.clone();
                tokio::spawn(async move {
                    crate::chat_images::run(&hub, &e2, cmd, |ev| out.send(Ev::Image(ev))).await;
                });
            }
            Cmd::LeaveVoice => self.leave_voice(true).await,
            Cmd::SetMuted(m) => {
                if let Some(v) = &self.voice {
                    if !v.is_deafened() {
                        v.set_muted(m);
                    }
                }
                self.local_voice_changed();
            }
            Cmd::SetDeafened(d) => {
                if let Some(v) = &self.voice {
                    v.set_deafened(d);
                }
                self.apply_ptt();
                self.local_voice_changed();
            }
            Cmd::SetVolume(pct) => {
                self.volume_pct = pct;
                if let Some(v) = &self.voice {
                    v.set_output_volume(pct.min(200) as f32 / 100.0);
                }
                if let Some(t) = &self.mic_test {
                    t.set_volume(pct.min(200) as f32 / 100.0);
                }
            }
            Cmd::SetInputDevice(device) => {
                self.input_device = device.clone();
                if let Some(v) = &self.voice {
                    v.set_input_device(device);
                }
            }
            Cmd::StartMicTest { input_device, output_device, volume_pct, gate, noise } => {
                self.start_mic_test(input_device, output_device, volume_pct, gate, noise);
            }
            Cmd::StopMicTest => self.stop_mic_test(),
            Cmd::SetUserAudio { identity, audio } => {
                if let Some(v) = &self.voice {
                    v.set_user_audio(&identity, audio);
                }
            }
            Cmd::SetOthersMuted(m) => {
                if let Some(v) = &self.voice {
                    v.set_others_muted(m);
                }
            }
            Cmd::PlayAlert { kind, volume, device, standalone } => self.play_alert(kind, volume, device, standalone),
            Cmd::ListSources => {
                let out = self.out.clone();
                tokio::task::spawn_blocking(move || {
                    let mut error = None;
                    let mut unsupported = false;
                    let mut windows = capture_call("list_windows", vox_capture::list_windows).unwrap_or_else(|e| {
                        unsupported |= crate::platform::is_unsupported_any(&e);
                        error = Some(format!("{e:#}"));
                        Vec::new()
                    });
                    let mut monitors = capture_call("list_monitors", vox_capture::list_monitors).unwrap_or_else(|e| {
                        unsupported |= crate::platform::is_unsupported_any(&e);
                        error.get_or_insert(format!("{e:#}"));
                        Vec::new()
                    });
                    // Unattended tests: the picker (and its thumbnails) shows the test windows
                    // only, never the person's screens or windows (see `testmode`).
                    if vox_client_core::testmode::guards_real_capture() {
                        let before = (windows.len(), monitors.len());
                        windows.retain(|w| vox_client_core::testmode::is_test_capture_exe(&w.exe_name));
                        monitors.clear();
                        info!(windows = before.0, monitors = before.1, kept = windows.len(), "test mode: sources other than the test windows left out");
                    }
                    // macOS before its capture backend: say what it is, not an error code.
                    crate::platform::note_screen_capture(unsupported);
                    if unsupported && crate::platform::screen_soon() {
                        error = Some(format!("Transmitir a tela, uma janela ou o som de um app: {}.", crate::platform::SOON_MAC_LC));
                    }
                    out.send(Ev::Sources { windows, monitors, error });
                });
            }
            Cmd::ListDevices => {
                let out = self.out.clone();
                tokio::task::spawn_blocking(move || {
                    if crate::app::shot_privacy() {
                        // Screenshots that may be shared: generic devices, never the PC's own.
                        let (mic, phones) = ("Microfone (headset USB)".to_string(), "Fones de ouvido (headset USB)".to_string());
                        out.send(Ev::Devices {
                            inputs: vec![mic.clone(), "Microfone da webcam".into()],
                            outputs: vec![phones.clone(), "Alto-falantes".into()],
                            default_input: Some(mic),
                            default_output: Some(phones),
                        });
                        return;
                    }
                    out.send(Ev::Devices {
                        inputs: list_devices(true),
                        outputs: list_devices(false),
                        default_input: default_device_name(true),
                        default_output: default_device_name(false),
                    });
                });
            }
            Cmd::StartShare { source, opts } => {
                let Some(v) = &self.voice else {
                    self.out
                        .send(Ev::ShareFailed("Entre em um canal de voz antes de transmitir.".into()));
                    return;
                };
                match v.start_share(source.clone(), opts).await {
                    Ok(()) => {
                        let encoder = v.share_encoder().await.unwrap_or_else(|| "software".into());
                        info!(?source, w = opts.width, h = opts.height, fps = opts.fps, audio = opts.audio, %encoder, "Go Live started");
                        self.streaming = true;
                        self.share_audio_only = source.is_audio_only();
                        self.out.send(Ev::ShareStarted { encoder });
                        self.push_voice_state();
                        self.thumbs_start(&source);
                    }
                    Err(e) => {
                        warn!(error = %format!("{e:#}"), "Go Live failed to start");
                        // Starting replaces a running live, so a failed start leaves none.
                        self.thumbs_stop();
                        if self.streaming {
                            self.streaming = false;
                            self.push_voice_state();
                        }
                        let text = format!("{e:#}");
                        // Audio-only errors are already complete sentences in Portuguese.
                        let msg = if source.is_audio_only() { text } else { format!("Não foi possível iniciar a transmissão: {text}") };
                        self.out.send(Ev::ShareFailed(msg));
                    }
                }
            }
            Cmd::SwitchShare { source, audio } => {
                let Some(v) = &self.voice else {
                    self.out.send(Ev::SwitchFailed("Entre em um canal de voz antes de transmitir.".into()));
                    return;
                };
                match v.switch_share(SwitchRequest { source: source.clone(), audio, audio_pid: None }).await {
                    Ok(r) => {
                        info!(from = ?r.from, to = ?source, path = ?r.path, gap_ms = r.gap.as_millis() as u64, total_ms = r.total.as_millis() as u64, audio = r.audio_after, "Go Live source switched");
                        self.out.send(Ev::ShareSwitched { audio: r.audio_after, audio_error: r.audio_error });
                        self.thumbs_start(&source);
                        // (A switch can go between a screen and an audio-only live.)
                        if self.share_audio_only != source.is_audio_only() {
                            self.share_audio_only = source.is_audio_only();
                            self.push_voice_state();
                        }
                    }
                    Err(e) => {
                        warn!(error = %format!("{e:#}"), "Go Live source switch failed");
                        self.out.send(Ev::SwitchFailed(format!("Não foi possível trocar a tela: {e:#}")));
                    }
                }
            }
            Cmd::StopShare => {
                self.thumbs_stop();
                if let Some(v) = &self.voice {
                    v.stop_share().await;
                }
                self.streaming = false;
                self.out.send(Ev::ShareStopped);
                self.push_voice_state();
            }
            Cmd::Watch(who) => {
                // The hub can announce a live member a moment before the voice room lists them.
                for attempt in 0..12 {
                    let Some(v) = &self.voice else { return };
                    match v.watch(&who) {
                        Ok(_) => return,
                        Err(e) if attempt < 11 && format!("{e}").contains("no participant") => {
                            tokio::time::sleep(Duration::from_millis(250)).await
                        }
                        Err(e) if e.downcast_ref::<WatchLimit>().is_some() => {
                            info!(who, watching = ?v.watching(), "watch refused: limit reached");
                            self.out.send(Ev::Notice(crate::call::watch_limit_text()));
                            return;
                        }
                        Err(e) => {
                            warn!(error = %format!("{e:#}"), who, "watch failed");
                            self.out.send(Ev::Notice("Não foi possível assistir essa transmissão.".into()));
                            return;
                        }
                    }
                }
            }
            Cmd::Unwatch(who) => {
                if let Some(v) = &self.voice {
                    v.unwatch(&who);
                }
            }
            Cmd::StreamVisible { identity, visible } => {
                if let Some(v) = &self.voice {
                    v.set_stream_visible(&identity, visible);
                }
            }
            Cmd::StreamFocusMuted { identity, muted } => {
                if let Some(v) = &self.voice {
                    v.set_stream_focus_muted(&identity, muted);
                }
            }
            Cmd::StartCamera { id, name, preset } => {
                self.start_camera(id, name, preset).await;
                self.sync_camera().await;
            }
            Cmd::StopCamera => {
                if let Some(v) = &self.voice {
                    v.stop_camera().await;
                }
                self.out.send(Ev::CameraStopped);
                self.sync_camera().await;
            }
            Cmd::CamerasVisible(on) => {
                self.cams_visible = on;
                if let Some(v) = &self.voice {
                    v.set_cameras_visible(on).await;
                }
            }
            Cmd::CameraHidden { identity, hidden } => {
                if hidden {
                    self.cams_hidden.insert(identity.clone());
                } else {
                    self.cams_hidden.remove(&identity);
                }
                if let Some(v) = &self.voice {
                    v.set_camera_hidden(&identity, hidden);
                }
            }
            Cmd::SharePreview(cfg) => {
                self.share_preview = cfg;
                if let Some(v) = &self.voice {
                    v.set_share_preview(cfg).await;
                }
            }
            Cmd::ListCameras => {
                let out = self.out.clone();
                tokio::task::spawn_blocking(move || match vox_capture::list_cameras() {
                    Ok(list) => {
                        crate::platform::note_camera(false);
                        out.send(Ev::Cameras { list, error: None })
                    }
                    Err(e) => {
                        warn!(error = %e, "cannot list cameras");
                        crate::platform::note_camera(crate::platform::is_unsupported(&e));
                        let error = if crate::platform::camera_soon() { format!("Câmera: {}.", crate::platform::SOON_MAC_LC) } else { "Não foi possível listar as câmeras.".into() };
                        out.send(Ev::Cameras { list: Vec::new(), error: Some(error) })
                    }
                });
            }
            Cmd::CameraPreview(want) => self.camera_preview(want).await,
            Cmd::CameraSetup { id, name, setup } => self.camera_setup_cmd((id, name), setup).await,
            Cmd::CameraResetControls { id, name } => self.camera_reset_controls((id, name)).await,
            Cmd::ListCameraFormats { id, name, key } => self.camera_formats(id, name, key).await,
            Cmd::CameraWatch(on) => self.cam.watch = on,
            Cmd::CameraRefreshControls => self.camera_refresh_controls().await,
            Cmd::CameraNativeDialog { id, name } => self.camera_native_dialog(id, name),
            Cmd::SetPtt(on) => {
                if let Some(g) = &self.voice_gate {
                    g.set_ptt(on);
                }
                self.ptt = on;
                self.ptt_down = false;
                self.apply_ptt();
                self.local_voice_changed();
            }
            Cmd::PttKey(down) => {
                tracing::debug!(down, enabled = self.ptt, "push-to-talk key");
                self.ptt_down = down;
                if self.ptt {
                    self.apply_ptt();
                    if let Some(v) = &self.voice {
                        tracing::debug!(muted = v.is_muted(), "push-to-talk mic state");
                        self.out.send(Ev::LocalVoice {
                            muted: v.is_muted(),
                            deafened: v.is_deafened(),
                        });
                    }
                }
            }
            Cmd::Stats => self.report_stats().await,
            Cmd::Quit => {}
        }
    }

    /// The saved camera: its id if that device is plugged in, else a device with the saved name,
    /// else the default ("").
    fn resolve_camera(id: Option<String>, name: Option<String>) -> String {
        let list = vox_capture::list_cameras().unwrap_or_default();
        if let Some(i) = id.filter(|i| list.iter().any(|c| &c.id == i)) {
            return i;
        }
        if let Some(n) = name.filter(|n| list.iter().any(|c| &c.name == n)) {
            return n;
        }
        String::new()
    }

    async fn start_camera(&mut self, id: Option<String>, name: Option<String>, preset: crate::settings::CameraPreset) {
        if self.voice.is_none() {
            self.out.send(Ev::CameraFailed("Entre em um canal de voz antes de ligar a câmera.".into()));
            return;
        }
        // The settings preview holds the device: hand it over to the call.
        self.stop_camera_preview().await;
        let dev = (id.clone(), name.clone());
        let setup = self.cam.setup_for(&dev);
        let device = tokio::task::spawn_blocking(move || Self::resolve_camera(id, name)).await.unwrap_or_default();
        let opts = vox_client_core::golive::camera::CameraOptions::with_quality(device, preset.to_core()).with_setup(setup.clone());
        let Some(v) = &self.voice else { return };
        match v.start_camera(opts).await {
            Ok(st) => {
                self.cam.call_started(dev, setup);
                let f = &st.format;
                let info = format!("{} · {} {}x{} @ {:.0}", f.device, f.subtype, f.native_width, f.native_height, f.native_fps);
                info!(encoder = %st.backend, %info, out = format!("{}x{}", f.width, f.height), "camera started");
                self.out.send(Ev::CameraStarted { encoder: st.backend, info });
            }
            Err(e) => self.out.send(Ev::CameraFailed(format!("{e:#}"))),
        }
    }

    /// Tell the hub when our camera went on or off (a failed start, a replaced device and a
    /// released camera all end up here: the session's own state is the truth).
    async fn sync_camera(&mut self) {
        let on = match &self.voice {
            Some(v) => v.camera_on().await,
            None => false,
        };
        if on != self.camera {
            self.camera = on;
            self.push_voice_state();
        }
    }

    async fn stop_camera_preview(&mut self) {
        self.camera_preview_gen += 1;
        self.cam.preview_stopped();
        if let Some(p) = self.camera_preview.take() {
            let _ = tokio::task::spawn_blocking(move || drop(p)).await;
        }
    }

    /// Settings' preview. While the call camera is on, the settings show its self-view instead
    /// (one capture per device).
    async fn camera_preview(&mut self, want: Option<(Option<String>, Option<String>, crate::settings::CameraPreset)>) {
        self.stop_camera_preview().await;
        let Some((id, name, preset)) = want else { return };
        if let Some(v) = &self.voice {
            if v.camera_on().await {
                self.out.send(Ev::CameraPreview(Ok(())));
                return;
            }
        }
        let gen = self.camera_preview_gen;
        let dev = (id.clone(), name.clone());
        let setup = self.cam.setup_for(&dev);
        let started_with = setup.clone();
        let res = tokio::task::spawn_blocking(move || {
            let device = Self::resolve_camera(id, name);
            vox_client_core::golive::camera::CameraPreview::start_with(&device, preset.to_core(), &setup)
        })
        .await
        .unwrap_or_else(|e| Err(format!("{e}")));
        match res {
            Ok(p) if gen == self.camera_preview_gen => {
                self.camera_preview = Some(p);
                self.cam.preview_started(dev, started_with);
                self.out.send(Ev::CameraPreview(Ok(())));
            }
            Ok(p) => {
                let _ = tokio::task::spawn_blocking(move || drop(p)).await;
            }
            Err(e) => self.out.send(Ev::CameraPreview(Err(e))),
        }
    }

    /// Play an alert: through the voice session's mixer while in voice (so it reaches the same
    /// speakers and the echo canceller's reference), else on the output device opened just for
    /// it. With `VOX_NO_PLAYBACK` nothing is ever played; the alert is only logged.
    fn play_alert(&self, kind: AlertKind, volume: f32, device: Option<String>, standalone: bool) {
        if self.no_playback {
            tracing::debug!(alert = kind.name(), "alert sound suppressed (VOX_NO_PLAYBACK)");
            return;
        }
        if !standalone {
            if let Some(v) = &self.voice {
                if v.play_alert(kind, volume) {
                    tracing::debug!(alert = kind.name(), path = "voice mixer", "alert sound");
                } else {
                    tracing::debug!(alert = kind.name(), "alert sound dropped: voice output unavailable");
                }
                return;
            }
        }
        let queued = self.ui_player.play(kind, volume, device.as_deref());
        tracing::debug!(alert = kind.name(), path = "standalone output", queued, "alert sound");
    }

    /// Open the mic test on its own thread (opening a device can take a while); the result comes
    /// back as `Internal::MicTestOpened`. Restarting replaces a test that is running.
    fn start_mic_test(&mut self, input_device: Option<String>, output_device: Option<String>, volume_pct: u32, gate: Arc<VoiceGate>, noise: Arc<NoiseControl>) {
        self.stop_mic_test();
        if self.no_mic {
            self.out.send(Ev::MicTestFailed("O microfone está desativado neste PC (VOX_NO_MIC).".into()));
            return;
        }
        self.volume_pct = volume_pct;
        let cfg = MicTestConfig {
            input_device,
            output_device,
            playback: !self.no_playback,
            volume: volume_pct.min(200) as f32 / 100.0,
            gate,
            noise,
        };
        let gen = self.mic_test_gen;
        let int_tx = self.int_tx.clone();
        info!(playback = cfg.playback, "mic test: start");
        tokio::task::spawn_blocking(move || {
            let res = MicTest::start(cfg);
            let _ = int_tx.send(Internal::MicTestOpened(gen, res));
        });
    }

    /// Stop the mic test (also one still opening: its result is discarded and closed).
    fn stop_mic_test(&mut self) {
        self.mic_test_gen += 1;
        if let Some(t) = self.mic_test.take() {
            info!("mic test: stop");
            // Closing the devices can take a moment on a slow driver: never on the controller.
            tokio::task::spawn_blocking(move || drop(t));
        }
    }

    /// In push-to-talk mode the mic follows the key; otherwise the user's mute button rules.
    fn apply_ptt(&self) {
        if !self.ptt {
            return;
        }
        if let Some(v) = &self.voice {
            if !v.is_deafened() {
                v.set_muted(!self.ptt_down);
            }
        }
    }

    fn local_voice_changed(&self) {
        if let Some(v) = &self.voice {
            self.out.send(Ev::LocalVoice {
                muted: v.is_muted(),
                deafened: v.is_deafened(),
            });
        }
        self.push_voice_state();
    }

    fn push_voice_state(&self) {
        let (Some(tx), Some(v)) = (&self.hub_tx, &self.voice) else { return };
        let Some(channel) = self.voice_channel else { return };
        if channel == crate::app::dms::DM_CALL_CHANNEL {
            // A DM call is private: the hub never hears about it as voice state.
            return;
        }
        // In push-to-talk mode the mic opens and closes with every key press; the others see the
        // speaking highlight for that, so do not flood the hub with a mute flip per press.
        let _ = tx.send(ClientEvent::VoiceState {
            channel_id: Some(channel),
            muted: v.is_muted() && (!self.ptt || v.mic_unavailable()),
            deafened: v.is_deafened(),
            streaming: self.streaming,
            camera: self.camera,
            audio_only: self.streaming && self.share_audio_only,
        });
    }

    /// Tell the hub who we are watching (only when it changed, unless `force`). Nothing is sent
    /// to a hub that already answered "invalid event" to it.
    fn push_watch_state(&mut self, force: bool) {
        if !self.watch_supported {
            return;
        }
        let Some(tx) = &self.hub_tx else { return };
        if !force && self.watch_sent.as_ref() == Some(&self.watch_set) {
            return;
        }
        if tx.send(ClientEvent::WatchState { watching: self.watch_set.clone() }).is_ok() {
            self.watch_sent = Some(self.watch_set.clone());
            self.watch_sent_at = Some(Instant::now());
        }
    }

    /// The voice session is gone: nobody is watched any more (the hub drops our entry along with
    /// our place in the room, so there is nothing to send).
    fn clear_watch_set(&mut self) {
        self.watch_set.clear();
        self.watch_sent = Some(Vec::new());
    }

    async fn report_stats(&mut self) {
        let streams = match &self.voice {
            Some(v) => v.streams().await,
            None => return,
        };
        let info: Vec<_> = streams
            .iter()
            .map(|s| {
                use std::sync::atomic::Ordering::Relaxed;
                (
                    s.label.clone(),
                    s.counters.frames.load(Relaxed),
                    s.counters.width.load(Relaxed),
                    s.counters.height.load(Relaxed),
                )
            })
            .collect();
        let (reports, _) = self.reporter.report(&streams).await;
        let audio: Vec<AudioInfo> = streams
            .iter()
            .zip(&reports)
            .filter(|(s, _)| !s.is_video)
            .map(|(s, r)| AudioInfo {
                label: s.label.clone(),
                chunks: s.counters.frames.load(std::sync::atomic::Ordering::Relaxed),
                rms_dbfs: r.rms_dbfs,
                side_dbfs: r.side_dbfs,
                kbps: r.kbps,
                codec: r.codec.clone(),
                max_quiet_ms: s.counters.take_max_quiet_ms(),
            })
            .collect();
        self.out.send(Ev::AudioInfo(audio));
        self.out.send(Ev::StreamInfo(info));
    }

    // ------------------------------------------------------------ hub

    async fn connect(&mut self, url: String, creds: Creds, remember_device: bool, guard: &Guard, tls: &TlsPlan) {
        self.teardown().await;
        match self.login_and_listen(&url, &creds, remember_device, guard, tls).await {
            Ok(done) => {
                let mut creds = done.creds;
                if done.device_token.is_some() {
                    creds.device_token = done.device_token.clone();
                }
                // An invite is used once; re-logins go with the device token it brought.
                creds.invite = None;
                creds.server_id = done.server_id.clone();
                self.creds = Some(creds);
                schedule_trim();
                self.out.send(Ev::LoggedIn {
                    server_name: done.server_name,
                    user: done.user,
                    url: done.url,
                    livekit_ok: done.livekit_ok,
                    device_token: done.device_token,
                    server_id: done.server_id,
                    pin: done.pin,
                    tls_pin: done.tls_pin,
                    tls_learned: done.tls_learned,
                });
            }
            Err(fail) => {
                self.hub = None;
                self.hub_tx = None;
                self.out.send(match fail {
                    LoginFail::Msg(msg) => Ev::ConnectFailed(msg),
                    LoginFail::Personal { message, forget_device } => Ev::NeedPersonalPassword { message, forget_device },
                    LoginFail::NotSameServer(msg) => Ev::NotSameServer(msg),
                    LoginFail::TlsMismatch { url, expected, got } => Ev::TlsMismatch { url, expected, got },
                    LoginFail::AskPlain(url) => Ev::PlainPrompt { url },
                });
            }
        }
    }

    /// Health check, login and event socket. Errors are user-facing Portuguese messages.
    async fn login_and_listen(&mut self, url: &str, creds: &Creds, remember_device: bool, guard: &Guard, tls: &TlsPlan) -> Result<LoggedIn, LoginFail> {
        let (hub, done) = login_at(url, creds, remember_device, guard, tls).await?;
        self.hub = Some(hub);
        self.start_events().map_err(|e| LoginFail::Msg(friendly(&e)))?;
        Ok(done)
    }
}

/// Health check, the saved-data check ([`Guard`]) and the login itself, without the event socket
/// (v1.0.1: a function of its own, so the check can be tested against real addresses).
async fn login_at(url: &str, creds: &Creds, remember_device: bool, guard: &Guard, tls: &TlsPlan) -> Result<(Arc<HubClient>, LoggedIn), LoginFail> {
    let url = normalize_url(url);
    // v1.1 (T0): the transport first (the trust rules). A certificate that is not the pinned one,
    // or no TLS for a pinned server, stops here: not even the health check goes out.
    let transport = match tls::decide_at(&url, tls).await {
        tls::Decision::Use(t) => t,
        tls::Decision::Mismatch { expected, got } => return Err(LoginFail::TlsMismatch { url, expected, got }),
        tls::Decision::NoTlsForPinned => return Err(LoginFail::Msg(tls::NO_TLS_FOR_PINNED.into())),
        tls::Decision::AskPlain => return Err(LoginFail::AskPlain(url)),
        tls::Decision::Unreachable(why) => {
            warn!(%url, error = %why, "hub not reachable");
            return Err(LoginFail::Msg(friendly(&anyhow::anyhow!("connect: {why}"))));
        }
    };
    let (tls_pin, tls_learned) = match transport {
        tls::Transport::Tls { pin, learned } => (Some(pin), learned),
        tls::Transport::Plain => (None, false),
    };
    let hub = tls::client(&url, transport).map_err(|e| LoginFail::Msg(friendly(&e)))?;
    let health = hub.health().await.map_err(|e| {
        warn!(error = %format!("{e:#}"), %url, "hub health check failed");
        LoginFail::Msg(friendly(&e))
    })?;
    if health.protocol_version != PROTOCOL_VERSION {
        warn!(
            server = health.protocol_version,
            client = PROTOCOL_VERSION,
            "protocol version mismatch"
        );
        return Err(LoginFail::Msg("Versão incompatível com o servidor. Peça a versão mais recente do concord ao host.".into()));
    }
    // v1.0.1: a saved password / login token goes only to the server it was saved for.
    let answered = health.server_id.as_deref().filter(|id| vox_proto::is_server_id(id));
    // v1.1 (T0): saved data tied to a certificate goes only over TLS with that certificate.
    let verdict = match guard.check_pin(tls_pin) {
        Check::Ok => guard.check(&url, answered),
        other => other,
    };
    let creds = match verdict {
        Check::Ok => creds.clone(),
        // An invite carries its own credential: go on with it alone.
        _ if creds.invite.is_some() => {
            info!(%url, ?verdict, "saved login data not sent: not confirmed as the server it was saved for");
            guard.strip(creds)
        }
        _ => {
            warn!(%url, ?verdict, "saved login data not sent: not confirmed as the server it was saved for");
            return Err(LoginFail::NotSameServer(verdict.message().into()));
        }
    };
    let login = hub.login_with(&creds.login_request(remember_device)).await.map_err(|e| {
        warn!(error = %format!("{e:#}"), "login failed");
        LoginFail::from_error(&e, &creds)
    })?;
    let pin = creds.invite.is_some() || guard.confirms(&url, answered);
    let server_id = answered.map(str::to_string);
    Ok((
        hub,
        LoggedIn { server_name: login.server_name, user: login.user, url, livekit_ok: health.livekit_ok, device_token: login.device_token, server_id, pin, creds, tls_pin, tls_learned },
    ))
}

impl Controller {
    /// An item-12 call. A password change also updates what re-logins use.
    /// A v0.8 invite call (create, list, revoke), off the controller.
    fn invite(&mut self, cmd: crate::host::InviteCmd) {
        let Some(hub) = self.hub.clone() else {
            self.out.send(Ev::Invite(crate::host::InviteEv { cmd, result: Err("Sem conexão com o servidor.".into()) }));
            return;
        };
        let out = self.out.clone();
        tokio::spawn(async move {
            let result = crate::host::run_invite(&hub, &cmd).await;
            out.send(Ev::Invite(crate::host::InviteEv { cmd, result }));
        });
    }

    async fn admin(&mut self, cmd: crate::roles::AdminCmd) {
        let Some(hub) = self.hub.clone() else {
            self.out.send(Ev::Admin { cmd, result: Err("Sem conexão com o servidor.".into()) });
            return;
        };
        if let crate::roles::AdminCmd::SetPassword(req) = &cmd {
            let result = crate::roles::run_admin(&hub, &cmd).await;
            if let (Ok(crate::roles::AdminReply::Password(resp)), Some(c)) = (&result, self.creds.as_mut()) {
                c.personal = req.new.clone();
                c.device_token = resp.device_token.clone();
            }
            self.out.send(Ev::Admin { cmd, result });
            return;
        }
        let out = self.out.clone();
        tokio::spawn(async move {
            let result = crate::roles::run_admin(&hub, &cmd).await;
            out.send(Ev::Admin { cmd, result });
        });
    }

    /// (Re)open the event socket for the current session token.
    fn start_events(&mut self) -> anyhow::Result<()> {
        let hub = self.hub.clone().ok_or_else(|| anyhow::anyhow!("not logged in"))?;
        let (tx, mut rx) = hub.connect_events()?;
        self.hub_tx = Some(tx);
        self.hub_gen += 1;
        let gen = self.hub_gen;
        let int_tx = self.int_tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if int_tx.send(Internal::Hub(gen, ev)).is_err() {
                    break;
                }
            }
        });
        Ok(())
    }

    async fn teardown(&mut self) {
        self.leave_voice(true).await;
        self.hub_gen += 1; // drops the event socket's forwarder results
        self.hub_tx = None;
        self.hub = None;
        self.creds = None;
        self.sounds_reset();
        self.e2ee_teardown();
    }

    async fn on_internal(&mut self, ev: Internal) {
        match ev {
            Internal::MicTestOpened(gen, res) => {
                if gen != self.mic_test_gen {
                    // Stopped (or restarted) while the devices were opening.
                    if let Ok(t) = res {
                        tokio::task::spawn_blocking(move || drop(t));
                    }
                    return;
                }
                match res {
                    Ok(t) => {
                        let warning = match (&t.playback_error, t.playing) {
                            (Some(e), _) => Some(format!("Sem saída de áudio: {}", output_reason(e))),
                            (None, false) => Some("a reprodução está desativada; só o medidor funciona.".to_string()),
                            _ => None,
                        };
                        t.set_volume(self.volume_pct.min(200) as f32 / 100.0);
                        self.mic_test = Some(t);
                        self.out.send(Ev::MicTestStarted { warning });
                    }
                    Err(e) => {
                        warn!(error = %e, "mic test: cannot open the microphone");
                        self.out.send(Ev::MicTestFailed(format!("Teste do microfone: {}", mic_reason(&e))));
                    }
                }
            }
            Internal::Hub(gen, ev) => {
                if gen != self.hub_gen {
                    return;
                }
                match ev {
                    HubEvent::AuthFailed => {
                        info!("hub rejected the session; logging in again");
                        self.out.send(Ev::Hub(HubEvent::Disconnected {
                            reason: "sessão expirada".into(),
                        }));
                        self.relogin().await;
                    }
                    HubEvent::Server(ServerEvent::Ready { .. }) => {
                        let (me, previews, e2ee) = match &ev {
                            HubEvent::Server(ServerEvent::Ready { user, stream_previews, e2ee, server_id, .. }) => (Some(user.clone()), *stream_previews, (*e2ee, server_id.clone())),
                            _ => (None, false, (false, None)),
                        };
                        self.out.send(Ev::Hub(ev));
                        // The hub forgot our voice state across the reconnect: announce it again.
                        self.push_voice_state();
                        // Same for who we watch. This also probes a new connection (even with an
                        // empty set) so an older hub's "invalid event" answer shows up at once.
                        self.watch_supported = true;
                        self.watch_sent = None;
                        self.out.send(Ev::ViewersSupported(true));
                        self.push_watch_state(true);
                        self.watch_heal = !self.watch_set.is_empty();
                        if let Some(me) = me {
                            // v1.1 (E0): this computer on the server's device list.
                            self.e2ee_on_ready(e2ee.0, e2ee.1, &me);
                            self.sounds_on_ready(me);
                        }
                        self.thumbs_on_ready(previews);
                    }
                    HubEvent::Server(ServerEvent::SoundsChanged { sounds }) => self.sounds_changed(sounds),
                    HubEvent::Server(ServerEvent::E2eeDevicesChanged { user_id, devices }) => self.e2ee_devices_changed(user_id, devices),
                    // v1.1 (E2): a sealed DM line is opened before the UI hears of it.
                    HubEvent::Server(ServerEvent::DmMessageCreated { message }) if message.sealed.is_some() || message.reply.as_ref().is_some_and(|r| r.sealed) => {
                        self.e2ee_dm_created(message)
                    }
                    HubEvent::Server(ServerEvent::SoundPlayed { sound_id, by, channel_id, stoppable }) => self.sound_played(sound_id, by, channel_id, stoppable),
                    HubEvent::Server(ServerEvent::SoundStopped { by, channel_id }) => self.sound_stopped(by, channel_id),
                    HubEvent::Server(ServerEvent::Error { error })
                        if error == WATCH_STATE_REJECTED
                            && self.watch_sent_at.is_some_and(|t| t.elapsed() < Duration::from_secs(10)) =>
                    {
                        // An older hub: it keeps the socket, we stop talking about watching.
                        info!("the hub does not know WatchState (older server); viewer lists are off for this connection");
                        self.watch_supported = false;
                        self.watch_heal = false;
                        self.out.send(Ev::ViewersSupported(false));
                    }
                    other => {
                        // The hub refused our play: the clip started at the click stops (v1.1).
                        if matches!(&other, HubEvent::Server(ServerEvent::Error { error }) if crate::soundboard::is_play_refusal(error)) {
                            self.sound_refused();
                        }
                        let heal = self.watch_heal
                            && matches!(&other, HubEvent::Server(ServerEvent::VoiceStateChanged { member })
                                if member.streaming && member.channel_id == self.voice_channel && self.watch_set.contains(&member.user.id));
                        self.out.send(Ev::Hub(other));
                        if heal {
                            self.watch_heal = false;
                            self.push_watch_state(true);
                        }
                    }
                }
            }
            Internal::Voice(gen, ev) => {
                if gen != self.voice_gen {
                    return;
                }
                match &ev {
                    VoiceEvent::LocalShareEnded { .. } => {
                        self.thumbs_stop();
                        self.streaming = false;
                        self.push_voice_state();
                    }
                    VoiceEvent::LocalCameraEnded { .. } => {
                        self.camera = false;
                        self.push_voice_state();
                    }
                    VoiceEvent::MicAvailable => {
                        self.apply_ptt();
                        self.local_voice_changed();
                    }
                    VoiceEvent::WatchStarted { identity } => {
                        if let Ok(id) = identity.parse::<UserId>() {
                            if !self.watch_set.contains(&id) {
                                self.watch_set.push(id);
                            }
                            self.push_watch_state(false);
                        }
                    }
                    VoiceEvent::WatchEnded { identity, .. } => {
                        if let Ok(id) = identity.parse::<UserId>() {
                            self.watch_set.retain(|w| *w != id);
                            self.push_watch_state(false);
                        }
                    }
                    VoiceEvent::E2eePeerDevice { identity, device_id } => {
                        if identity.parse::<UserId>().is_ok_and(|u| self.e2ee.is_new_device(u, device_id)) {
                            info!(who = %identity, "e2ee: joined the call from a new device");
                            self.out.send(Ev::CallNewDevice { identity: identity.clone() });
                        }
                    }
                    VoiceEvent::Disconnected { .. } => {
                        self.thumbs_stop();
                        self.sounds_voice_left();
                        self.clear_watch_set();
                        self.streaming = false;
                        self.camera = false;
                        if let Some(v) = self.voice.take() {
                            v.leave().await;
                        }
                        self.voice_channel = None;
                        self.voice_gen += 1;
                        if let Some(tx) = &self.hub_tx {
                            let _ = tx.send(ClientEvent::VoiceState {
                                channel_id: None,
                                muted: false,
                                deafened: false,
                                streaming: false,
                                camera: false,
                                audio_only: false,
                            });
                        }
                    }
                    _ => {}
                }
                self.out.send(Ev::Voice(ev));
            }
            Internal::Relogin(gen) => {
                if gen == self.hub_gen {
                    self.relogin().await;
                }
            }
            Internal::Joined(gen, channel, outcome) => self.on_joined(gen, channel, outcome).await,
            Internal::Sounds(gen, reply) => {
                if gen == self.hub_gen {
                    self.sounds_listed(reply);
                }
            }
            Internal::LateSound(late) => self.late_sound_ready(late),
        }
    }

    async fn relogin(&mut self) {
        let (Some(hub), Some(c)) = (self.hub.clone(), self.creds.as_ref()) else {
            return;
        };
        let c = c.clone();
        // v1.0.1: the same server as at login (an address can change hands).
        if let Some(want) = c.server_id.as_deref() {
            if let Ok(h) = hub.health().await {
                if h.server_id.as_deref() != Some(want) {
                    warn!("re-login refused: the address now answers as another server");
                    self.teardown().await;
                    self.out.send(Ev::LoggedOut { reason: Some(Check::Mismatch.message().into()) });
                    return;
                }
            }
        }
        match hub.login_with(&c.login_request(false)).await {
            Ok(_) => {
                if let Err(e) = self.start_events() {
                    warn!(error = %format!("{e:#}"), "cannot reopen the event socket");
                }
            }
            Err(e) => {
                let msg = match LoginFail::from_error(&e, &c) {
                    LoginFail::Msg(m) | LoginFail::NotSameServer(m) | LoginFail::AskPlain(m) => m,
                    LoginFail::Personal { message, .. } => format!("{message} Entre de novo."),
                    LoginFail::TlsMismatch { .. } => "A identidade do servidor mudou.".into(),
                };
                warn!(error = %format!("{e:#}"), "re-login failed");
                if msg.starts_with("Não foi possível conectar") {
                    // Server still down: try again shortly.
                    self.hub_gen += 1;
                    let gen = self.hub_gen;
                    let tx = self.int_tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        let _ = tx.send(Internal::Relogin(gen));
                    });
                } else {
                    self.teardown().await;
                    self.out.send(Ev::LoggedOut { reason: Some(msg) });
                }
            }
        }
    }

    // ------------------------------------------------------------ voice

    async fn join_voice(&mut self, channel: ChannelId, opts: VoiceOpts) {
        self.join_target(channel, JoinVia::Channel, opts).await
    }

    /// Join a voice room: a channel's, or (`JoinVia::Dm`) a DM call's, which the app sees as the
    /// "channel" `DM_CALL_CHANNEL`.
    async fn join_target(&mut self, channel: ChannelId, via: JoinVia, opts: VoiceOpts) {
        let Some(hub) = self.hub.clone() else {
            self.out.send(Ev::VoiceFailed {
                channel: None,
                message: "Sem conexão com o servidor.".into(),
            });
            return;
        };
        self.out.send(Ev::VoiceJoining(channel));
        self.cancel_join();
        self.leave_voice(false).await;
        self.ptt = opts.ptt;
        self.ptt_down = false;
        opts.gate.set_ptt(opts.ptt);
        self.voice_gate = Some(opts.gate.clone());
        self.input_device = opts.input_device.clone();
        let cfg = VoiceConfig {
            mic: !self.no_mic,
            playback: !self.no_playback,
            input_device: opts.input_device,
            output_device: opts.output_device,
            start_muted: opts.ptt,
            output_volume: Some(opts.volume_pct.min(200) as f32 / 100.0),
            mic_test_tone: None,
            fake_mics: None,
            user_audio: opts.user_audio,
            others_muted: opts.others_muted,
            gate: Some(opts.gate),
            noise: Some(opts.noise),
            // v1.1 (T0): over TLS, LiveKit is reached through the hub's port in pinned TLS.
            tls_pin: hub.tls_pin(),
            // v1.1 (E1): set by `run_join` when the hub says the room is encrypted.
            e2ee: None,
        };
        // Autotests (v1.1 E1): a synthetic tone as the microphone (`VOX_TEST_MIC_TONE=<Hz>`), never
        // played here; with `VOX_NO_MIC=1` no device is opened.
        let mut cfg = cfg;
        if vox_client_core::testmode::background() {
            if let Some(hz) = std::env::var("VOX_TEST_MIC_TONE").ok().and_then(|v| v.parse::<f32>().ok()).filter(|h| (20.0..=4000.0).contains(h)) {
                cfg.mic = true;
                cfg.mic_test_tone = Some(hz);
                info!(hz, "autotest: the microphone is a synthetic tone");
            }
            // Made-up microphones (`autotest_micswitch.rs`): the real one stays closed.
            if let Some(bank) = vox_client_core::audio::fake_mic::installed() {
                cfg.mic = true;
                cfg.fake_mics = Some(bank);
                info!("autotest: the microphones are fake");
            }
        }
        // v1.1 (E1): this computer can encrypt a room (registered, directory read).
        let media = self.e2ee.media();
        let e2ee_status = self.e2ee.status();
        self.join_gen += 1;
        let gen = self.join_gen;
        let int_tx = self.int_tx.clone();
        let e2 = self.e2ee.clone();
        self.join_task = Some(tokio::spawn(async move {
            // v1.1 (E1, audit): a DM call is encrypted when the caller's app says it can as the
            // call starts. While this computer is still registering or reading the device list,
            // wait for it; if it still cannot, do not start the call in the clear (the other side
            // would be told "uses an old version"): say so, and let the person try again.
            let starting_call = matches!(via, JoinVia::Dm { accept: false, .. });
            let (media, e2ee_status) = if starting_call && media.is_none() { e2.media_for_call(&hub).await } else { (media, e2ee_status) };
            if starting_call && media.is_none() && matches!(e2ee_status, crate::backend::e2ee::E2eeStatus::Starting | crate::backend::e2ee::E2eeStatus::Active) {
                warn!(?e2ee_status, "dm call not started: this computer's encryption is not ready");
                let _ = int_tx.send(Internal::Joined(gen, channel, Err(CALL_E2EE_NOT_READY.to_string())));
                return;
            }
            let outcome = run_join(&hub, channel, via, cfg, media, e2ee_status).await;
            let _ = int_tx.send(Internal::Joined(gen, channel, outcome));
        }));
    }

    /// Abandon the join in flight (if any); its result, should it still arrive, is discarded.
    fn cancel_join(&mut self) {
        if let Some(t) = self.join_task.take() {
            t.abort();
            info!("voice join cancelled");
        }
        self.join_gen += 1;
    }

    async fn on_joined(&mut self, gen: u64, channel: ChannelId, outcome: JoinOutcome) {
        if gen != self.join_gen {
            // Cancelled or superseded while connecting: do not leave a ghost in the room.
            if let Ok((session, _rx)) = outcome {
                info!("discarding a voice join that finished after it was cancelled");
                session.leave().await;
            }
            return;
        }
        self.join_task = None;
        match outcome {
            Ok((session, mut rx)) => {
                let identity = session.identity().to_string();
                let report = session.join_report().clone();
                let no_mic = report.mic_error.is_some();
                let e2ee = session.is_e2ee();
                let warning = join_warning(&report);
                if let Some(w) = &warning {
                    warn!(warning = %w, mic_error = ?report.mic_error, playback_error = ?report.playback_error, "joined voice with limitations");
                }
                // (Picked while the join was on its way.)
                session.set_input_device(self.input_device.clone());
                session.set_cameras_hidden(&self.cams_hidden.iter().cloned().collect::<Vec<_>>());
                session.set_cameras_visible(self.cams_visible).await;
                session.set_share_preview(self.share_preview).await;
                self.voice = Some(session);
                self.voice_channel = Some(channel);
                self.streaming = false;
                self.camera = false;
                self.clear_watch_set();
                self.voice_gen += 1;
                let gen = self.voice_gen;
                let int_tx = self.int_tx.clone();
                tokio::spawn(async move {
                    while let Some(ev) = rx.recv().await {
                        if int_tx.send(Internal::Voice(gen, ev)).is_err() {
                            break;
                        }
                    }
                });
                schedule_trim();
                self.sounds_voice_joined();
                self.out.send(Ev::VoiceJoined {
                    channel,
                    identity,
                    warning,
                    no_mic,
                    e2ee,
                });
                self.local_voice_changed();
            }
            Err(message) => {
                self.out.send(Ev::VoiceFailed {
                    // A DM call is retried from its DM ("Ligar"), not from the voice bar.
                    channel: Some(channel).filter(|c| *c != crate::app::dms::DM_CALL_CHANNEL),
                    message,
                });
            }
        }
    }

    async fn leave_voice(&mut self, notify: bool) {
        if notify {
            // An explicit leave (or logout) also cancels a join still connecting.
            self.cancel_join();
        }
        self.voice_gen += 1;
        self.sounds_voice_left();
        self.thumbs_stop();
        let had = self.voice_channel.take().is_some();
        self.streaming = false;
        self.camera = false;
        self.clear_watch_set();
        if let Some(v) = self.voice.take() {
            v.leave().await;
        }
        if had {
            if let Some(tx) = &self.hub_tx {
                let _ = tx.send(ClientEvent::VoiceState {
                    channel_id: None,
                    muted: false,
                    deafened: false,
                    streaming: false,
                    camera: false,
                    audio_only: false,
                });
            }
        }
        if notify && had {
            self.out.send(Ev::VoiceLeft);
        }
    }
}

// ---------------------------------------------------------------- identity (item 12)

impl Creds {
    /// Device token first (no password typed), else the personal password, if any.
    fn login_request(&self, remember_device: bool) -> vox_proto::LoginRequest {
        vox_proto::LoginRequest {
            username: self.user.clone(),
            server_password: self.password.clone(),
            password: self.personal.clone().filter(|p| !p.is_empty()),
            device_token: self.device_token.clone(),
            remember_device,
            invite: self.invite.clone(),
            ..Default::default()
        }
    }
}

/// Why a login failed: a message, or "this name needs its personal password".
enum LoginFail {
    Msg(String),
    Personal { message: String, forget_device: bool },
    /// v1.0.1: saved login data was kept back (see [`Guard`]).
    NotSameServer(String),
    /// v1.1 (T0): another certificate than the pinned one (nothing sent).
    TlsMismatch { url: String, expected: Pin, got: Pin },
    /// v1.1 (T0): a typed address without TLS: ask first (rule 4).
    AskPlain(String),
}

/// A login that went through ([`Controller::login_and_listen`]).
struct LoggedIn {
    server_name: String,
    user: User,
    url: String,
    livekit_ok: bool,
    device_token: Option<String>,
    server_id: Option<String>,
    pin: bool,
    /// What was actually sent (saved data kept back is not in here).
    creds: Creds,
    /// v1.1 (T0): the session's certificate (`None`: plain HTTP), seen for the first time or not.
    tls_pin: Option<Pin>,
    tls_learned: bool,
}

impl LoginFail {
    fn from_error(e: &anyhow::Error, creds: &Creds) -> LoginFail {
        let l = format!("{e:#}").to_lowercase();
        let typed = creds.personal.as_ref().is_some_and(|p| !p.is_empty());
        if l.contains(vox_proto::ERR_PERSONAL_PASSWORD_REQUIRED) {
            let message = if creds.device_token.is_some() && !typed {
                "Este computador não está mais lembrado para esse nome. Digite sua senha pessoal."
            } else {
                "Esse nome é protegido por uma senha pessoal. Digite-a no campo \"Senha pessoal\"."
            };
            LoginFail::Personal { message: message.into(), forget_device: creds.device_token.is_some() }
        } else if l.contains(vox_proto::ERR_WRONG_PERSONAL_PASSWORD) {
            LoginFail::Personal { message: "Senha pessoal incorreta.".into(), forget_device: false }
        } else if l.contains(vox_proto::ERR_RESET_CODE_EXPIRED) {
            LoginFail::Personal { message: "Esse código de redefinição expirou. Peça um novo ao dono do servidor.".into(), forget_device: false }
        } else if l.contains("wrong server password") && creds.password.is_empty() && creds.device_token.is_some() {
            // v0.8: somebody who joined with an invite and whose remembered computer was dropped.
            LoginFail::Msg("Este computador não está mais lembrado neste servidor. Peça um convite novo (ou a senha do servidor).".into())
        } else {
            LoginFail::Msg(friendly(e))
        }
    }
}

// ---------------------------------------------------------------- helpers

/// Once things have settled (hub snapshot, room joined), compact the heaps and trim the working
/// set: start-up allocations and GL/driver pages that are never touched again leave RAM.
fn schedule_trim() {
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(6)).await;
        let heaps = tokio::task::spawn_blocking(|| vox_client_core::stats::trim_memory(true))
            .await
            .unwrap_or(0);
        tracing::debug!(heaps, "trimmed working set");
    });
}

// ---------------------------------------------------------------- saved secrets (v1.0.1)

/// A saved secret's server: where it was saved, and the identity of the server that issued it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Binding {
    /// The address it was saved under (normalized).
    pub home: String,
    /// That server's [`vox_proto::HealthResponse::server_id`], when known (saved by v1.0.1 or
    /// later, from a hub that has one).
    pub server_id: Option<String>,
    /// v1.1 (T0): that server's certificate, when known: the secret then goes only over TLS with
    /// exactly that certificate (never plain HTTP).
    pub tls_pin: Option<Pin>,
}

/// What of a login was saved on this computer, and for which server (audit
/// `voxhub:app:device-token-sent-to-unbound-candidate-address`). An invite code (anybody can
/// make one, with any addresses in any order) or a saved entry can offer several addresses; the
/// saved server password and this computer's login token go only to the server they belong to:
///
/// - with a known identity, to an address whose `/api/health` answers with that same identity;
/// - without one (saved before v1.0.1, or an older hub), only to the very address they were
///   saved under, never to another address of an invite or entry. The first login there pins the
///   identity.
///
/// Typed passwords and an invite's own token are not "saved": they go where the user sent them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Guard {
    /// The login token in `Cmd::Connect` is a saved one.
    pub token: Option<Binding>,
    /// The server password in `Cmd::Connect` is the saved one.
    pub password: Option<Binding>,
    /// The address the user typed or picked from the list (normalized), as opposed to an
    /// invite's: a server that answers there may be pinned for an entry without an identity.
    pub confirmed: Option<String>,
}

/// May the saved data go to this address?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    Ok,
    /// It answers as another server (or as none, while one is known).
    Mismatch,
    /// No identity is known and this is not the address the data was saved under.
    Unconfirmed,
}

impl Check {
    pub fn message(self) -> &'static str {
        match self {
            Check::Ok => "",
            Check::Mismatch => {
                "Este endereço não é o mesmo servidor que você salvou. Por segurança, a senha e o login salvos não foram enviados. \
                 Se você confia neste endereço, digite a senha do servidor (e a senha pessoal, se tiver) e entre de novo."
            }
            Check::Unconfirmed => {
                "Não deu para confirmar que este endereço é o mesmo servidor que você salvou. Por segurança, a senha e o login salvos \
                 não foram enviados. Se você confia neste endereço, digite a senha do servidor (e a senha pessoal, se tiver) e entre de novo."
            }
        }
    }
}

impl Binding {
    fn check(&self, url: &str, answered: Option<&str>) -> Check {
        match &self.server_id {
            Some(id) if answered == Some(id.as_str()) => Check::Ok,
            Some(_) => Check::Mismatch,
            None if normalize_url(url).eq_ignore_ascii_case(&normalize_url(&self.home)) => Check::Ok,
            None => Check::Unconfirmed,
        }
    }
}

impl Guard {
    /// v1.1 (T0): may the saved data go over a connection pinned to `conn` (`None`: plain HTTP)?
    /// A secret saved with a certificate goes only to that certificate.
    pub fn check_pin(&self, conn: Option<Pin>) -> Check {
        let tied = [&self.token, &self.password].into_iter().flatten().filter_map(|b| b.tls_pin);
        if tied.into_iter().all(|p| conn == Some(p)) {
            Check::Ok
        } else {
            Check::Mismatch
        }
    }

    /// The verdict for sending the saved data to `url`, whose hub answered `answered`.
    pub fn check(&self, url: &str, answered: Option<&str>) -> Check {
        let all = [&self.token, &self.password].into_iter().flatten().map(|b| b.check(url, answered));
        all.fold(Check::Ok, |worst, c| match (worst, c) {
            (Check::Mismatch, _) | (_, Check::Mismatch) => Check::Mismatch,
            (Check::Unconfirmed, _) | (_, Check::Unconfirmed) => Check::Unconfirmed,
            _ => Check::Ok,
        })
    }

    /// `creds` without the saved data.
    fn strip(&self, creds: &Creds) -> Creds {
        let mut c = creds.clone();
        if self.token.is_some() {
            c.device_token = None;
        }
        if self.password.is_some() {
            c.password.clear();
        }
        c
    }

    /// Did a login at `url` (answering `answered`) confirm which server this is? A saved identity
    /// that matched, or the address the user typed or picked.
    pub fn confirms(&self, url: &str, answered: Option<&str>) -> bool {
        let matched = [&self.token, &self.password].into_iter().flatten().any(|b| b.server_id.is_some() && b.check(url, answered) == Check::Ok);
        matched || self.confirmed.as_deref().is_some_and(|c| normalize_url(c).eq_ignore_ascii_case(&normalize_url(url)))
    }
}

/// v0.8 invites (and v0.9.1 saved servers that came from one): which of `urls` to log in at. In
/// order (the LAN address first when we are on the host's network), the first whose hub answers
/// its health check within a short time, preferring (v1.0.1) one the saved data may go to; else
/// the first that answered at all (the login there then goes without the saved data, or not at
/// all).
async fn pick(urls: &[String], guard: &Guard, plan: &TlsPlan) -> Option<String> {
    let mut first = None;
    for url in urls {
        // v1.1 (T0): asked over the transport the login would use (pinned TLS when there is a pin).
        let transport = match tls::decide_at(url, plan).await {
            tls::Decision::Use(t) => t,
            tls::Decision::Unreachable(_) => {
                info!(%url, "invite: address does not answer, trying the next");
                continue;
            }
            // Something answers there, but not as this server must (the login says why).
            _ => {
                first.get_or_insert_with(|| url.clone());
                continue;
            }
        };
        let Ok(hub) = tls::client(url, transport) else { continue };
        let conn_pin = match transport {
            tls::Transport::Tls { pin, .. } => Some(pin),
            tls::Transport::Plain => None,
        };
        match tokio::time::timeout(Duration::from_secs(3), hub.health()).await {
            Ok(Ok(h)) => {
                let verdict = match guard.check_pin(conn_pin) {
                    Check::Ok => guard.check(url, h.server_id.as_deref()),
                    other => other,
                };
                info!(%url, ?verdict, "invite: this address answers");
                if verdict == Check::Ok {
                    return Some(url.clone());
                }
                first.get_or_insert_with(|| url.clone());
            }
            _ => info!(%url, "invite: address does not answer, trying the next"),
        }
    }
    first
}

/// "1.2.3.4" -> "http://1.2.3.4:7890"; keeps an explicit scheme, port and path.
pub fn normalize_url(input: &str) -> String {
    let s = input.trim();
    let (scheme, rest) = match s.split_once("://") {
        Some((sch, rest)) => (sch.to_ascii_lowercase(), rest),
        None => ("http".to_string(), s),
    };
    let rest = rest.trim_end_matches('/');
    let (authority, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    let has_port = if authority.starts_with('[') {
        authority.contains("]:")
    } else {
        authority.contains(':')
    };
    let authority = if has_port || authority.is_empty() {
        authority.to_string()
    } else {
        format!("{authority}:7890")
    };
    format!("{scheme}://{authority}{path}")
}

/// What a voice join asks the hub for: a channel's room, or a DM call's private room.
#[derive(Debug, Clone, Copy)]
enum JoinVia {
    Channel,
    /// `accept`: answer the ringing call (else ring it, or rejoin it).
    Dm { dm: vox_proto::DmId, accept: bool },
}

/// v1.1 (E1, audit): a DM call not started because this computer's encryption is not ready yet
/// (still registering, or the device list could not be read).
pub const CALL_E2EE_NOT_READY: &str = "A criptografia deste computador ainda está sendo preparada, e a chamada sairia sem criptografia. Tente de novo em alguns segundos.";

/// v1.1 (E1): why this computer cannot enter an encrypted room (it has no usable key), in words.
fn no_key_text(status: crate::backend::e2ee::E2eeStatus, call: bool) -> String {
    use crate::backend::e2ee::E2eeStatus as S;
    let what = if call { "Esta chamada é criptografada de ponta a ponta" } else { "Esta sala usa criptografia de ponta a ponta" };
    match status {
        S::Starting | S::Off => format!("{what}, e a criptografia deste computador ainda está sendo preparada. Tente de novo em alguns segundos."),
        _ => format!("{what}, e este computador não tem uma chave de criptografia ativa neste servidor (veja Configurações → Privacidade)."),
    }
}

/// Hub join request, then the LiveKit connection, with every step logged. The error is the
/// user-facing message. v1.1 (E1): `media` = this computer can encrypt (the join says so, and an
/// encrypted room's answer turns the encryption on).
async fn run_join(hub: &HubClient, channel: ChannelId, via: JoinVia, mut cfg: VoiceConfig, media: Option<vox_client_core::MediaE2ee>, e2ee_status: crate::backend::e2ee::E2eeStatus) -> JoinOutcome {
    let started = std::time::Instant::now();
    let capable = media.is_some();
    let asked = match via {
        JoinVia::Channel => hub.voice_join_with(channel, capable).await.map_err(|e| {
            if !capable && format!("{e:#}").contains(vox_proto::ERR_E2EE_REQUIRED) {
                no_key_text(e2ee_status, false)
            } else {
                friendly(&e)
            }
        }),
        JoinVia::Dm { dm, accept } => {
            let r = if accept { hub.dm_call_accept_with(dm, capable).await } else { hub.dm_call_with(dm, capable).await };
            r.map(|c| c.join).map_err(|e| {
                if !capable && format!("{e:#}").contains(vox_proto::ERR_E2EE_CALL_REQUIRED) {
                    no_key_text(e2ee_status, true)
                } else {
                    crate::app::dms::error_text(&e)
                }
            })
        }
    };
    let resp = match asked {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, channel, ?via, "voice join: hub request failed");
            return Err(e);
        }
    };
    info!(channel, hub_ms = started.elapsed().as_millis() as u64, url = %resp.livekit_url, e2ee = resp.e2ee, "voice join: hub join ok");
    if resp.e2ee {
        // (The hub only says so to a join that asked with `?e2ee=1`.)
        match media {
            Some(m) => cfg.e2ee = Some(m),
            None => return Err(no_key_text(e2ee_status, matches!(via, JoinVia::Dm { .. }))),
        }
    }
    match VoiceSession::connect(&resp.livekit_url, &resp.livekit_token, cfg).await {
        Ok(ok) => {
            info!(channel, total_ms = started.elapsed().as_millis() as u64, "voice join: complete");
            Ok(ok)
        }
        // The core already logged the failing step and the full error chain.
        Err(e) => {
            if let JoinVia::Dm { dm, .. } = via {
                // Nobody should keep ringing (or wait in the room) for a call we could not join.
                if let Err(e) = hub.dm_call_end(dm).await {
                    warn!(error = %format!("{e:#}"), dm, "ending the dm call after a failed join");
                }
            }
            Err(voice_error_message(&e))
        }
    }
}

/// Which step failed, in words for the colleagues.
pub fn voice_error_message(e: &anyhow::Error) -> String {
    match e.downcast_ref::<JoinError>().map(|j| j.step) {
        Some(JoinStep::Signal) => "Não foi possível conectar ao servidor de voz (etapa: conexão com a porta 7880). \
             Verifique sua internet e se VPN, antivírus ou firewall não estão bloqueando o concord."
            .into(),
        Some(JoinStep::Media) => "Não foi possível conectar o áudio da sala (etapa: conexão de mídia). \
             A rede parece estar bloqueando a conexão de voz. Tente outra rede ou desligue VPN, antivírus ou firewall."
            .into(),
        None => format!("Não foi possível entrar na voz: {e:#}"),
    }
}

/// "Entrou sem microfone: ..." / "Entrou sem saída de áudio: ..." for a join that worked with
/// limitations, `None` when everything opened.
pub fn join_warning(report: &vox_client_core::JoinReport) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(raw) = &report.mic_error {
        parts.push(format!("Entrou sem microfone: {}", mic_reason(raw)));
    }
    if let Some(raw) = &report.playback_error {
        parts.push(format!("Entrou sem saída de áudio: {}", output_reason(raw)));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// What the person is told when the microphone changes by itself. (A switch they asked for needs
/// no words: the level meter moves.)
pub fn mic_status_text(status: &MicStatus) -> Option<String> {
    match status {
        MicStatus::Switched { .. } => None,
        MicStatus::FellBack { wanted, device } => {
            Some(format!("Microfone \"{wanted}\" indisponível: usando o padrão do sistema ({device}). Quando ele voltar, a voz volta para ele sozinha."))
        }
        MicStatus::Restored { device } => Some(format!("Microfone de volta: {device}.")),
        MicStatus::Lost => Some("Microfone desconectado. Conecte um microfone: ele volta sozinho, sem sair da voz.".into()),
    }
}

/// A notice left by [`mic_status_text`].
pub fn is_mic_notice(notice: &str) -> bool {
    notice.starts_with("Microfone")
}

/// The voice bar's warning once a microphone showed up: what is left of it.
pub fn warning_without_mic(warning: String) -> Option<String> {
    let left: Vec<&str> = warning.split(" · ").filter(|p| !p.starts_with("Entrou sem microfone:")).collect();
    (!left.is_empty()).then(|| left.join(" · "))
}

fn is_denied(l: &str) -> bool {
    l.contains("denied") || l.contains("permission") || l.contains("0x80070005") || l.contains("not allowed")
}

fn mic_reason(raw: &str) -> String {
    let l = raw.to_lowercase();
    if l.contains("matching") {
        "o microfone escolhido nas configurações não foi encontrado.".into()
    } else if l.contains("no default input device") {
        "nenhum microfone foi encontrado. Conecte um microfone ou escolha outro em Configurações.".into()
    } else if is_denied(&l) {
        "o Windows bloqueou o acesso. Verifique as permissões de microfone do Windows (Configurações > Privacidade e segurança > Microfone).".into()
    } else if l.contains("did not answer") {
        "o dispositivo de áudio não respondeu. Verifique as permissões de microfone do Windows e se outro programa o está usando.".into()
    } else if l.contains("publish microphone") {
        "o servidor não aceitou o áudio do microfone.".into()
    } else if l.contains("not available") || l.contains("busy") || l.contains("in use") || l.contains("exclusive") {
        "o microfone está em uso por outro programa (modo exclusivo) ou foi desconectado.".into()
    } else {
        format!("não foi possível abrir o microfone ({raw}). Verifique as permissões de microfone do Windows.")
    }
}

fn output_reason(raw: &str) -> String {
    let l = raw.to_lowercase();
    if l.contains("matching") {
        "a saída escolhida nas configurações não foi encontrada.".into()
    } else if l.contains("no default output device") {
        "nenhuma saída de áudio foi encontrada.".into()
    } else if l.contains("did not answer") {
        "o dispositivo de áudio não respondeu.".into()
    } else if l.contains("not available") || l.contains("busy") || l.contains("in use") || l.contains("exclusive") {
        "o dispositivo está em uso por outro programa (modo exclusivo) ou foi desconectado.".into()
    } else {
        format!("não foi possível abrir a saída de áudio ({raw}).")
    }
}

/// Map core/hub errors to messages for the colleagues (Portuguese, no jargon).
pub fn friendly(e: &anyhow::Error) -> String {
    let full = format!("{e:#}");
    let l = full.to_lowercase();
    if l.contains("wrong server password") {
        "Senha do servidor incorreta.".into()
    } else if l.contains(vox_proto::ERR_INVITE_INVALID) {
        "Convite inválido: confira se copiou o código inteiro, ou peça um novo a quem te convidou.".into()
    } else if l.contains(vox_proto::ERR_INVITE_EXPIRED) {
        "Este convite venceu. Peça um novo a quem te convidou.".into()
    } else if l.contains(vox_proto::ERR_INVITE_USED_UP) {
        "Este convite já foi usado o máximo de vezes. Peça um novo a quem te convidou.".into()
    } else if l.contains(vox_proto::ERR_INVITE_REVOKED) {
        "Este convite foi cancelado por quem cuida do servidor. Peça um novo.".into()
    } else if l.contains(vox_proto::ERR_INVITE_NAME_TAKEN) {
        // v1.0.1: an invite only creates new accounts.
        "Esse nome já existe neste servidor. Escolha outro nome ou peça a senha do servidor.".into()
    } else if l.contains(vox_proto::ERR_TOO_MANY_MENTIONS) {
        "Menções demais numa mensagem só.".into()
    } else if l.contains(&vox_proto::ERR_E2EE_REQUIRED.to_lowercase()) {
        // v1.1 (E1): an encrypted room (the hub's own text).
        vox_proto::ERR_E2EE_REQUIRED.into()
    } else if l.contains("http 426") || l.contains("conexão protegida") {
        // v1.1: a hub with `tls = "required"` (an older app shows the hub's text as is).
        vox_proto::ERR_TLS_REQUIRED.into()
    } else if l.contains("protocol version mismatch") {
        "Versão incompatível com o servidor. Peça a versão mais recente do concord ao host.".into()
    } else if l.contains(vox_proto::ERR_TOO_MANY_ATTEMPTS) {
        "Muitas tentativas seguidas. Aguarde alguns segundos e tente de novo.".into()
    } else if l.contains("username must be") {
        "O nome deve ter entre 2 e 32 caracteres.".into()
    } else if l.contains("invalid hub url") {
        "Endereço do servidor inválido. Exemplo: http://192.168.0.10:7890".into()
    } else if l.contains("http 401") {
        "Sessão recusada pelo servidor.".into()
    } else if l.contains("http 5") {
        "O servidor retornou um erro. Tente novamente em instantes.".into()
    } else if l.contains("request") || l.contains("connect") || l.contains("timed out") || l.contains("dns") {
        "Não foi possível conectar ao servidor. Confira o endereço e se o servidor está ligado (porta 7890).".into()
    } else {
        format!("Erro: {full}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_get_scheme_and_default_port() {
        assert_eq!(normalize_url("1.2.3.4"), "http://1.2.3.4:7890");
        assert_eq!(normalize_url(" 1.2.3.4:9000/ "), "http://1.2.3.4:9000");
        assert_eq!(normalize_url("https://hub.example/base"), "https://hub.example:7890/base");
        assert_eq!(normalize_url("http://[::1]:7890"), "http://[::1]:7890");
        assert_eq!(normalize_url("localhost"), "http://localhost:7890");
    }

    #[test]
    fn voice_errors_name_the_failed_step() {
        let join = |step| anyhow::Error::new(JoinError { step, elapsed: Duration::from_secs(15), source: anyhow::anyhow!("wait_pc_connection timed out") });
        let media = voice_error_message(&join(JoinStep::Media));
        assert!(media.contains("áudio da sala") && media.contains("bloqueando"), "{media}");
        let signal = voice_error_message(&join(JoinStep::Signal));
        assert!(signal.contains("servidor de voz") && signal.contains("7880"), "{signal}");
        assert!(voice_error_message(&anyhow::anyhow!("boom")).contains("boom"));
    }

    #[test]
    fn missing_devices_become_warnings() {
        let mut report = vox_client_core::JoinReport::default();
        assert_eq!(join_warning(&report), None);
        report.mic_error = Some("no default input device".into());
        let w = join_warning(&report).unwrap();
        assert!(w.starts_with("Entrou sem microfone:") && w.contains("nenhum microfone"), "{w}");
        report.mic_error = Some("build mic stream: Access is denied. (0x80070005)".into());
        assert!(join_warning(&report).unwrap().contains("permissões de microfone do Windows"));
        report.mic_error = None;
        report.playback_error = Some("the audio device did not answer within 8 s".into());
        assert!(join_warning(&report).unwrap().starts_with("Entrou sem saída de áudio:"));
    }

    #[test]
    fn microphone_changes_are_explained_in_portuguese() {
        assert_eq!(mic_status_text(&MicStatus::Switched { device: "USB".into() }), None);
        let fell = mic_status_text(&MicStatus::FellBack { wanted: "Headset".into(), device: "Realtek".into() }).unwrap();
        assert!(fell.contains("\"Headset\" indisponível") && fell.contains("padrão do sistema (Realtek)"), "{fell}");
        assert_eq!(mic_status_text(&MicStatus::Restored { device: "Headset".into() }).unwrap(), "Microfone de volta: Headset.");
        assert!(mic_status_text(&MicStatus::Lost).unwrap().contains("sem sair da voz"));
        for s in [MicStatus::FellBack { wanted: "a".into(), device: "b".into() }, MicStatus::Restored { device: "a".into() }, MicStatus::Lost] {
            assert!(is_mic_notice(&mic_status_text(&s).unwrap()));
        }
        assert!(!is_mic_notice("Mensagem não enviada: sem conexão com o servidor."));
    }

    #[test]
    fn the_warning_keeps_only_what_is_still_true_when_a_mic_shows_up() {
        let mut report = vox_client_core::JoinReport::default();
        report.mic_error = Some("no default input device".into());
        assert_eq!(warning_without_mic(join_warning(&report).unwrap()), None);
        report.playback_error = Some("no default output device".into());
        let left = warning_without_mic(join_warning(&report).unwrap()).unwrap();
        assert!(left.starts_with("Entrou sem saída de áudio:") && !left.contains("microfone"), "{left}");
    }

    #[test]
    fn errors_are_friendly() {
        let e = anyhow::anyhow!("login failed: HTTP 401 Unauthorized: wrong server password");
        assert_eq!(friendly(&e), "Senha do servidor incorreta.");
        let e = anyhow::anyhow!("login failed: HTTP 400 Bad Request: protocol version mismatch: server 1, client 2");
        assert!(friendly(&e).starts_with("Versão incompatível"));
        let e = anyhow::anyhow!("error sending request").context("health request");
        assert!(friendly(&e).starts_with("Não foi possível conectar"));
        // v1.0.1: an invite on a name that already exists.
        let e = anyhow::anyhow!("login failed: HTTP 401 Unauthorized: {}", vox_proto::ERR_INVITE_NAME_TAKEN);
        assert_eq!(friendly(&e), "Esse nome já existe neste servidor. Escolha outro nome ou peça a senha do servidor.");
    }

    /// A stand-in for some other server (v1.0.1 test): answers its health check as `server_id`
    /// (`None`: an older hub, with no identity) and refuses every login, keeping its body.
    fn recorder(server_id: Option<&'static str>) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let logins = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = logins.clone();
        std::thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(mut sock) = sock else { continue };
                let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, body) = loop {
                    let n = sock.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break (String::new(), String::new());
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if buf[0] == 0x16 {
                        // A TLS hello: answered like an older hub's HTTP server does.
                        let _ = sock.write_all(b"HTTP/1.1 400 Bad Request\r\nconnection: close\r\n\r\n");
                        break (String::new(), String::new());
                    }
                    let text = String::from_utf8_lossy(&buf).to_string();
                    let Some(end) = text.find("\r\n\r\n") else { continue };
                    let len = text[..end]
                        .lines()
                        .filter_map(|l| l.split_once(':'))
                        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + len {
                        break (text[..end].to_string(), String::from_utf8_lossy(&buf[end + 4..end + 4 + len]).to_string());
                    }
                };
                if head.is_empty() {
                    // v1.1: a TLS probe (no HTTP at all), or a connection closed early.
                    continue;
                }
                let (status, json) = if head.starts_with("GET /api/health") {
                    let id = server_id.map_or(String::new(), |id| format!(r#","server_id":"{id}""#));
                    ("200 OK", format!(r#"{{"ok":true,"version":"9.9.9","protocol_version":{PROTOCOL_VERSION},"livekit_ok":false{id}}}"#))
                } else {
                    seen.lock().unwrap().push(body);
                    ("401 Unauthorized", r#"{"error":"wrong server password"}"#.to_string())
                };
                let resp = format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{json}", json.len());
                let _ = sock.write_all(resp.as_bytes());
            }
        });
        (url, logins)
    }

    /// v1.0.1 (audit `voxhub:app:device-token-sent-to-unbound-candidate-address`): the saved server
    /// password and this computer's login token go only to the server they were saved for, however
    /// an invite code or a saved entry orders its addresses. A real hub A, and two stand-ins: B
    /// answers as another server, an older hub answers with no identity.
    #[test]
    fn saved_secrets_go_only_to_the_server_they_belong_to() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let dir = std::env::temp_dir().join(format!("concord-guard-test-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("config.toml"),
                "server_name = \"A\"\nserver_password = \"senha-de-a\"\n[livekit]\napi_key = \"APItest0000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123456789abcdef\"\nsignal_port = 1\n",
            )
            .unwrap();
            let opts = voxhub_server::Options { data_dir: dir.clone(), no_livekit: true, livekit_bin: "unused".into(), bind_override: Some("127.0.0.1:0".into()) };
            let hub = voxhub_server::start(opts).await.unwrap();
            let a = format!("http://127.0.0.1:{}", hub.addr.port());
            let a_id = HubClient::new(&a).unwrap().health().await.unwrap().server_id.expect("a v1.0.1 hub has an identity");
            let (b, b_logins) = recorder(Some("ffffffffffffffffffffffffffffffff"));
            const INVITE: &str = "00112233445566778899aabbccddeeff";
            let creds = |invite: bool| Creds {
                personal: None,
                device_token: Some("token-salvo-de-a".into()),
                user: "ana".into(),
                password: "senha-de-a".into(),
                invite: invite.then(|| INVITE.to_string()),
                server_id: None,
            };
            let at_a = |id: Option<&str>| Some(Binding { home: a.clone(), server_id: id.map(str::to_string), tls_pin: None });
            let saved = Guard { token: at_a(Some(&a_id)), password: at_a(Some(&a_id)), confirmed: None };
            let not_same = |r: &Result<(Arc<HubClient>, LoggedIn), LoginFail>| matches!(r, Err(LoginFail::NotSameServer(_)));
            // v1.1: the stand-ins speak plain HTTP (as if the user said "Conectar sem proteção").
            let open = TlsPlan { plain_ok: true, ..Default::default() };

            // An invite code (or a saved entry) that lists B first: B answers, A is the one used.
            assert_eq!(pick(&[b.clone(), a.clone()], &saved, &open).await.as_deref(), Some(a.as_str()));
            let (_, done) = login_at(&a, &creds(false), false, &saved, &open).await.ok().expect("A takes its own saved password");
            assert!(done.pin && done.server_id.as_deref() == Some(a_id.as_str()));
            // Only B answers: with an invite, B gets the invite's own token and nothing saved...
            assert_eq!(pick(std::slice::from_ref(&b), &saved, &open).await.as_deref(), Some(b.as_str()));
            assert!(login_at(&b, &creds(true), true, &saved, &open).await.is_err(), "B refuses (it is not A)");
            // ...and without one (one click on a saved entry), nothing at all.
            assert!(not_same(&login_at(&b, &creds(false), false, &saved, &open).await));
            let got = b_logins.lock().unwrap().clone();
            assert_eq!(got.len(), 1, "{got:?}");
            assert!(!got[0].contains("token-salvo-de-a") && !got[0].contains("senha-de-a"), "{}", got[0]);
            assert!(got[0].contains(INVITE), "{}", got[0]);

            // Saved before v1.0.1 (no identity): only the very address it was saved under.
            let legacy = Guard { token: at_a(None), password: at_a(None), confirmed: None };
            assert!(not_same(&login_at(&b, &creds(false), false, &legacy, &open).await));
            assert_eq!(b_logins.lock().unwrap().len(), 1, "B got nothing more");
            let (_, done) = login_at(&a, &creds(false), false, &legacy, &open).await.ok().expect("A, where it was saved");
            assert!(!done.pin, "an address from an invite or entry list does not pin");
            let typed = Guard { confirmed: Some(a.clone()), ..legacy };
            let (_, done) = login_at(&a, &creds(false), false, &typed, &open).await.ok().expect("the address the user typed");
            assert!(done.pin && done.server_id.as_deref() == Some(a_id.as_str()), "it pins the identity");

            // An older hub with no identity, at the address the data was saved under: as before.
            let (old, old_logins) = recorder(None);
            let at_old = Guard { token: Some(Binding { home: old.clone(), server_id: None, tls_pin: None }), password: None, confirmed: Some(old.clone()) };
            assert!(login_at(&old, &creds(false), false, &at_old, &open).await.is_err(), "(the stand-in refuses every login)");
            assert!(old_logins.lock().unwrap()[0].contains("token-salvo-de-a"), "no regression with an older hub");
            // But data saved with an identity never goes to an address that answers with none.
            let (old2, old2_logins) = recorder(None);
            assert!(not_same(&login_at(&old2, &creds(false), false, &saved, &open).await));
            assert!(old2_logins.lock().unwrap().is_empty());

            hub.shutdown().await;
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// v1.1 (T0): the transport follows the trust rules end to end, against a real hub (TLS on)
    /// and a plain stand-in (an older hub): a pinned login talks only to its certificate, and
    /// nothing at all goes to a server that does not have it.
    #[test]
    fn logins_follow_the_trust_rules() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let dir = std::env::temp_dir().join(format!("concord-tls-login-test-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("config.toml"),
                "server_name = \"T\"\nserver_password = \"senha\"\n[livekit]\napi_key = \"APItest0000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123456789abcdef\"\nsignal_port = 1\n",
            )
            .unwrap();
            let opts = voxhub_server::Options { data_dir: dir.clone(), no_livekit: true, livekit_bin: "unused".into(), bind_override: Some("127.0.0.1:0".into()) };
            let hub = voxhub_server::start(opts).await.unwrap();
            let pin = hub.tls_pin().expect("TLS is on by default");
            let a = format!("http://127.0.0.1:{}", hub.addr.port());
            let creds = Creds { personal: None, device_token: None, user: "ana".into(), password: "senha".into(), invite: None, server_id: None };
            let none = Guard::default();
            let plan = |pin: Option<Pin>, source: PinSource| TlsPlan { pin, source, plain_ok: false };

            // Rule 4, a typed address: TLS, its certificate seen for the first time.
            let (h, done) = login_at(&a, &creds, false, &none, &TlsPlan::default()).await.ok().expect("TLS on first use");
            assert_eq!((done.tls_pin, done.tls_learned, h.tls_pin()), (Some(pin), true, Some(pin)));
            assert_eq!(h.base_url().scheme(), "https");
            // Rule 1, an invite with the pin: that certificate.
            let (_, done) = login_at(&a, &creds, false, &none, &plan(Some(pin), PinSource::Invite)).await.ok().expect("pinned TLS");
            assert_eq!((done.tls_pin, done.tls_learned), (Some(pin), false));
            // Another certificate pinned: stopped before anything is sent.
            let other = [1u8; 32];
            let r = login_at(&a, &creds, false, &none, &plan(Some(other), PinSource::Saved)).await;
            assert!(matches!(r, Err(LoginFail::TlsMismatch { expected, got, .. }) if expected == other && got == pin));

            // An older hub (plain HTTP only).
            let (old, logins) = recorder(None);
            assert!(matches!(login_at(&old, &creds, false, &none, &TlsPlan::default()).await, Err(LoginFail::AskPlain(_))), "rule 4: ask first");
            for source in [PinSource::Saved, PinSource::Invite, PinSource::Manual] {
                let pinned = TlsPlan { pin: Some(pin), source, plain_ok: true };
                assert!(matches!(login_at(&old, &creds, false, &none, &pinned).await, Err(LoginFail::Msg(m)) if m == tls::NO_TLS_FOR_PINNED), "rules 1, 2, 5");
            }
            assert!(logins.lock().unwrap().is_empty(), "nothing reached the plain server");
            assert!(login_at(&old, &creds, false, &none, &plan(None, PinSource::Saved)).await.is_err(), "(the stand-in refuses every login)");
            assert_eq!(logins.lock().unwrap().len(), 1, "rule 3: a saved entry without a pin goes plain");

            // Saved data tied to a certificate goes only to that certificate.
            let tied = |p: Pin| Guard { token: Some(Binding { home: a.clone(), server_id: None, tls_pin: Some(p) }), password: None, confirmed: Some(a.clone()) };
            let with_token = Creds { device_token: Some("token-salvo".into()), ..creds.clone() };
            assert!(matches!(login_at(&a, &with_token, false, &tied(other), &TlsPlan::default()).await, Err(LoginFail::NotSameServer(_))));
            assert_eq!(tied(other).check_pin(None), Check::Mismatch, "never in the clear");
            assert_eq!(tied(pin).check_pin(Some(pin)), Check::Ok);
            assert_eq!(Guard::default().check_pin(None), Check::Ok, "nothing saved");

            hub.shutdown().await;
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn the_guard_verdicts() {
        let id = "0123456789abcdef0123456789abcdef";
        let home = "http://192.0.2.10:7890";
        let pinned = Binding { home: home.into(), server_id: Some(id.into()), tls_pin: None };
        assert_eq!(pinned.check("http://198.51.100.1:7890", Some(id)), Check::Ok, "same server, another address");
        assert_eq!(pinned.check(home, Some("ffffffffffffffffffffffffffffffff")), Check::Mismatch);
        assert_eq!(pinned.check(home, None), Check::Mismatch, "an identity is known: no answer is not enough");
        let legacy = Binding { home: home.into(), server_id: None, tls_pin: None };
        assert_eq!(legacy.check("192.0.2.10", None), Check::Ok, "the address it was saved under");
        assert_eq!(legacy.check("http://198.51.100.1:7890", Some(id)), Check::Unconfirmed);
        let g = Guard { token: Some(pinned), password: Some(legacy), confirmed: None };
        assert_eq!(g.check(home, Some(id)), Check::Ok);
        assert_eq!(g.check("http://198.51.100.1:7890", Some(id)), Check::Unconfirmed, "the worst of the two");
        assert_eq!(Guard::default().check("http://anything:1", None), Check::Ok, "nothing saved: nothing to keep back");
        assert!(!Check::Mismatch.message().is_empty() && Check::Mismatch.message().starts_with("Este endereço não é o mesmo servidor que você salvou"));
    }
}
