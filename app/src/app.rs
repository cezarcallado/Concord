//! The egui application: state, event handling and screens.
//!
//! Every button calls one of the `App` action methods (`connect`, `join_voice`, `start_share`,
//! ...); `--autotest` drives those same methods (see `autotest.rs`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use eframe::egui::{self, Color32, RichText};
use vox_capture::{MonitorInfo, WindowInfo};
use vox_client_core::golive::{DegradationPreference, EncoderChoice, ShareOptions, ShareSource, AUDIO_ONLY_HIGH_BPS, AUDIO_ONLY_NORMAL_BPS};
use vox_client_core::hub::HubEvent;
use vox_client_core::{AlertKind, NoiseControl, VoiceEvent, VoiceGate};
use vox_proto::{Channel, ChannelId, ChannelKind, Message, ServerEvent, User, UserId, VoiceMember};

use crate::alerts::{self, AlertPolicy};
use crate::autotest::Auto;
use crate::call::{watch_allowed, watch_limit_text, CallUi, VideoUi, WATCH_PENDING_TIMEOUT};
use crate::backend::{Backend, Cmd, Ev, Repaint, VoiceOpts};
use crate::meter::MeterWatch;
use crate::previews::{self, PreviewStore};
use crate::settings::{Preset, Settings, UserAudioPref};
use crate::theme::ph;

/// Direct messages and private calls (a child module: it uses the app's private helpers).
#[path = "dms.rs"]
pub mod dms;

/// Chat notifications and the tray (a child module: it uses the app's private helpers).
#[path = "notify.rs"]
pub mod notify;

/// Mentions, replies and reactions in the chat (v0.8; a child module: it uses the app's private
/// helpers).
#[path = "chat_extras.rs"]
pub mod chat_extras;

/// Settings → Vídeo: hardware acceleration and the video encoder (v0.7).
#[path = "encoder_cfg.rs"]
mod encoder_cfg;

/// The v1.0 main screen: the sidebar and the center (`layout.rs` has the rules).
#[path = "shell.rs"]
mod shell;
#[path = "sidebar.rs"]
mod sidebar;
/// The v1.0 chat renderer (channels, the docked chat, DMs).
#[path = "chat_view.rs"]
pub mod chat_view;
/// The v1.0 home screen.
#[path = "home.rs"]
mod home;
/// Configurações (v1.0, phase 3a): the sections on the left, the chosen one on the right.
#[path = "settings_ui.rs"]
pub mod settings_ui;

// "Apoie o concord": the Pix popup and settings page (v1.1, `donate.rs` has the rules).
#[path = "donate_ui.rs"]
pub mod donate_ui;
pub(crate) use settings_ui::{set_log_dir, SettingsPage};
/// The Go Live source picker window (v1.0, phase 3a).
#[path = "golive_ui.rs"]
mod golive_ui;
/// "Atalhos de teclado" (v1.1): the settings page, the keyboard hook and what each shortcut does.
#[path = "keybinds_ui.rs"]
pub mod keybinds_ui;
/// The Calladex easter egg (v1.0.1): `/calladex` in a chat input, or "calladex" typed on the home screen.
#[path = "calladex.rs"]
pub mod calladex;
// v1.1 (T0): the connection's lock, "Segurança da conexão" and the TLS dialogs.
#[path = "tls_ui.rs"]
pub mod tls_ui;
// v1.1 (E0): Configurações → Privacidade → "Seus aparelhos".
#[path = "privacy_ui.rs"]
pub mod privacy_ui;

// Icons (Phosphor, `theme::ph`; v1.0).
pub(crate) const ICON_MUTED: &str = ph::MICROPHONE_SLASH;
pub(crate) const ICON_DEAF: &str = ph::HEADPHONES;
pub(crate) const ICON_LIVE: &str = ph::RECORD;
pub(crate) const ICON_VOICE: &str = ph::SPEAKER_HIGH;
/// Somebody's (or a stream's) sound switched off for me.
pub(crate) const ICON_SPEAKER_OFF: &str = ph::SPEAKER_SLASH;
/// Autotests: the window counts as minimized without really being minimized (background test
/// mode never minimizes or restores a window, `vox_client_core::testmode`).
static SIMULATED_MINIMIZED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Autotests: count the window as minimized (or not) from now on.
pub(crate) fn simulate_minimized(on: bool) {
    SIMULATED_MINIMIZED.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Design-review screenshots that may be shared (`tools\shots.ps1` sets `VOX_SHOT_PRIVACY=1`): the
/// machine's own device names (microphones, speakers, the video card) are replaced by generic
/// ones, so no picture shows what the person's PC has.
pub(crate) fn shot_privacy() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var_os("VOX_SHOT_PRIVACY").is_some_and(|v| v == "1"));
    *ON
}

/// [`shot_privacy`]'s stand-in for a video card's encoder label ("<card> (NVENC)" becomes
/// "Placa de vídeo (NVENC)"); the label itself otherwise.
pub(crate) fn shown_encoder_label(label: &str) -> String {
    if !shot_privacy() {
        return label.to_string();
    }
    match label.rfind(" (") {
        Some(i) => format!("Placa de vídeo{}", &label[i..]),
        None => "Placa de vídeo".to_string(),
    }
}

/// Is the main window minimized (nobody sees it)? Every "minimized" decision goes through here.
pub(crate) fn window_minimized(ctx: &egui::Context) -> bool {
    SIMULATED_MINIMIZED.load(std::sync::atomic::Ordering::Relaxed) || ctx.input(|i| i.viewport().minimized.unwrap_or(false))
}

/// A private channel's mark (v0.9), after its kind's icon.
pub(crate) const ICON_PRIVATE: &str = ph::LOCK_SIMPLE;
const PRIVATE_HOVER: &str = "Canal privado: só quem tem acesso vê";

/// How a channel reads in the lists: "# geral", "🔊 Sala 1", with 🔒 when private.
pub(crate) fn channel_label(c: &Channel, suffix: &str) -> String {
    let icon = if c.kind == ChannelKind::Voice { ICON_VOICE } else { "#" };
    let lock = if c.private.is_some() { format!("{ICON_PRIVATE} ") } else { String::new() };
    format!("{icon} {lock}{}{suffix}", c.name)
}
pub(crate) const ICON_TV: &str = ph::MONITOR;
pub(crate) const ICON_CAMERA: &str = ph::VIDEO_CAMERA;
const ICON_WARN: &str = ph::WARNING;
/// Marks an audio-only live ("Só áudio") next to "AO VIVO".
pub(crate) const AUDIO_ONLY_ICON: &str = ph::MUSIC_NOTES;
const ICON_VOL: &str = ph::SPEAKER_LOW;

/// The camera icon rule: the hub's flag, or the voice room's when both are in the same room.
pub(crate) fn camera_icon_on(hub_flag: bool, same_room: bool, room_says: bool) -> bool {
    hub_flag || (same_room && room_says)
}

/// Is a live audio-only ("Só áudio")? `hub_flag`: the hub says so (`VoiceMember::audio_only`,
/// reported for everybody, in any room). Fallback for older hubs, which never send it: the voice
/// room's reading of the LiveKit tracks (`room_says`), which only exists for people in the room
/// we are in (`same_room`).
pub(crate) fn audio_only_live_on(hub_flag: bool, same_room: bool, room_says: bool) -> bool {
    hub_flag || (same_room && room_says)
}

pub(crate) const GREEN: Color32 = crate::theme::ONLINE;
/// Last-resort UI limit for a join; the backend gives up on its own long before.
const JOIN_UI_LIMIT_SECS: u64 = 90;
pub(crate) const RED: Color32 = crate::theme::AO_VIVO;
const HISTORY_PAGE: usize = 50;
const MAX_KEPT_MESSAGES: usize = 1000;
/// "IA máxima" could not keep up in real time on this machine (see `NoiseControl`).
const NOISE_FALLBACK_TEXT: &str = "A supressão \"IA máxima\" pesou demais neste computador e voltou para \"IA leve\".";
/// `Settings::stream_focus_audio` (the stream audio menu and Configurações → Vídeo).
pub(crate) const FOCUS_AUDIO_LABEL: &str = "Ouvir só as transmissões fixadas";
pub(crate) const FOCUS_AUDIO_HOVER: &str = "Com várias transmissões abertas, só as fixadas (ou a que está em tela cheia) tocam som. Sem nenhuma fixada, todas tocam.";

/// What the center area shows: a text channel's chat, or the call view of our voice channel.
/// The call (and any stream we watch) keeps running while the chat is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Center {
    #[default]
    Chat,
    Call,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Connect,
    Main,
}

#[derive(Default)]
pub struct ConnectForm {
    pub url: String,
    pub user: String,
    pub password: String,
    pub remember: bool,
    pub busy: bool,
    pub error: Option<String>,
    /// Item 12: the name's personal password (shown once the hub asks for it).
    pub personal: String,
    pub need_personal: bool,
    /// "Lembrar este computador" (a device token instead of typing the personal password).
    pub remember_device: bool,
    /// "Salvar na lista de servidores" (v0.9.1, on by default).
    pub save_server: bool,
    /// v1.0.1: after "Este endereço não é o mesmo servidor que você salvou", the `server_key` of
    /// that address and name: the next login there goes only with what is typed now (no saved
    /// password, no saved login token) and, when it works, saves this server as the one.
    pub manual: Option<String>,
}

#[derive(Default)]
pub struct ChannelLog {
    pub msgs: Vec<Message>,
    pub requested: bool,
    pub loading_older: bool,
    pub no_more: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Link {
    #[default]
    Up,
    Reconnecting,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Share {
    #[default]
    Off,
    Starting,
    Live {
        encoder: String,
    },
}

/// Our camera, as the voice bar shows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CameraUi {
    #[default]
    Off,
    Starting,
    On {
        encoder: String,
        /// Device and mode, for the button's hover text.
        info: String,
    },
}

/// The camera preview of "Configurar câmera".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CameraPreviewUi {
    #[default]
    Off,
    Starting,
    On,
    Failed(String),
}

#[derive(Default)]
pub struct VoiceUi {
    pub channel: Option<ChannelId>,
    pub joining: Option<ChannelId>,
    pub link: Link,
    pub identity: String,
    pub muted: bool,
    pub deafened: bool,
    pub share: Share,
    pub share_preset: Preset,
    /// What the running Go Live captures, and whether it carries audio ("Trocar tela" starts from
    /// these).
    pub share_pick: Option<Pick>,
    pub share_audio: bool,
    /// The running Go Live is "Só áudio" (no video, no preview), and at which quality.
    pub share_audio_only: bool,
    pub share_audio_high: bool,
    /// Streamers (identities) whose live is audio-only, as the voice room reports it. Only
    /// people in our own voice channel are known (see `VoiceEvent::ShareKind`).
    pub audio_only: HashSet<String>,
    /// A "Trocar tela" request is in flight, towards this source.
    pub switching_to: Option<Pick>,
    /// Streams being watched (streamer identities), in the order they started (item 9: at
    /// most `MAX_WATCHED`).
    pub watching: Vec<String>,
    /// "Assistir" pressed, the core has not confirmed yet (identity, when).
    pub watch_pending: Vec<(String, std::time::Instant)>,
    pub speaking: HashSet<String>,
    pub error: Option<String>,
    /// Channel to retry after `error`.
    pub retry: Option<ChannelId>,
    /// When the current join started (drives the elapsed counter and the UI-side time limit).
    pub joining_since: Option<std::time::Instant>,
    /// Joined with limitations (no mic / no speakers); shown in the voice bar.
    pub warning: Option<String>,
    /// Listen-only: the mic could not be used.
    pub no_mic: bool,
    /// "Silenciar todos": every other participant is silenced locally (not saved).
    pub others_muted: bool,
    pub watch_started_count: u32,
    pub watch_ended_count: u32,
    pub camera: CameraUi,
    /// Other participants (identities) whose camera is on.
    pub cameras: HashSet<String>,
    /// v1.1 (E1): the room's media is end-to-end encrypted ("Criptografada de ponta a ponta").
    pub e2ee: bool,
    /// v1.1 (E1): how each other person's media decrypts here (only what is not "ok" is shown).
    pub e2ee_states: HashMap<String, vox_client_core::MediaE2eeState>,
    /// v1.1 (E1): a DM call that could not be encrypted because the other person's app is older.
    pub e2ee_plain_call: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Pick {
    Window(isize),
    Monitor(usize),
    /// "Só áudio": one app (its process) as the source of the sound.
    App(u32),
    /// "Só áudio": all the PC's sound except the app's own.
    AllSound,
}

/// The Go Live dialog's mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShareMode {
    /// "Tela + áudio": a window or screen (video), with the app's sound.
    #[default]
    ScreenAudio,
    /// "Só áudio": just an app's sound, e.g. to listen to music together.
    AudioOnly,
}

#[derive(Default)]
pub struct GoLive {
    pub open: bool,
    /// The dialog is "Trocar tela" (live already running: only the source and its audio change).
    pub switching: bool,
    pub loading: bool,
    pub windows: Vec<WindowInfo>,
    pub monitors: Vec<MonitorInfo>,
    pub pick: Option<Pick>,
    pub preset: Preset,
    pub audio: bool,
    /// "Tela + áudio" or "Só áudio"; kept while the app runs.
    pub mode: ShareMode,
    /// "Só áudio": Alta (256 kbps) instead of Normal (128 kbps).
    pub audio_high: bool,
    /// "Priorizar fluidez" instead of "Priorizar nitidez".
    pub smooth: bool,
    pub error: Option<String>,
    /// Tabs, thumbnails and their cache (item 8c); released when the dialog closes.
    pub picker: crate::sources::Picker,
}

/// The "Testar microfone" button's state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MicTestUi {
    #[default]
    Off,
    /// Devices are opening.
    Starting,
    /// Running; the text is a limitation to show (no speakers), if any.
    On(Option<String>),
}

#[derive(Default)]
pub struct SettingsUi {
    pub open: bool,
    pub mic_test: MicTestUi,
    pub mic_test_error: Option<String>,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub default_input: Option<String>,
    pub default_output: Option<String>,
    pub capturing_key: bool,
    pub cameras: Vec<vox_capture::CameraInfo>,
    pub cameras_error: Option<String>,
    /// The camera preview of "Configurar câmera" (`camera_cfg.rs`).
    pub camera_preview: CameraPreviewUi,
    /// Camera formats, image controls and the running camera's numbers (see `camera_cfg.rs`).
    pub cam: crate::camera_cfg::CamUi,
    /// "IA máxima" was too heavy for this machine and fell back to "IA leve" (note shown under
    /// "Supressão de ruído" until another level is picked).
    pub noise_fell_back: bool,
    /// The open section of the settings window (kept while the app runs).
    pub page: SettingsPage,
    /// Scroll the open section to its end on the next frame (the autotest's screenshots).
    pub scroll_end: bool,
}

pub struct App {
    pub backend: Backend,
    pub repaint: Repaint,
    pub settings: Settings,
    pub persist: bool,
    /// Keyboard shortcuts and push-to-talk (`keybinds_ui.rs`).
    pub keys: keybinds_ui::KeysUi,

    pub screen: Screen,
    pub form: ConnectForm,

    pub server_name: String,
    pub me: Option<User>,
    pub livekit_ok: bool,
    pub hub_up: bool,
    pub ready: bool,
    pub channels: Vec<Channel>,
    pub online: Vec<User>,
    pub voice_members: BTreeMap<UserId, VoiceMember>,
    /// False once the hub turned out to be too old to know who watches (no viewer UI then).
    pub viewers_supported: bool,
    pub logs: HashMap<ChannelId, ChannelLog>,
    pub selected: Option<ChannelId>,
    pub input: String,
    focus_input: bool,
    pub unread: HashMap<ChannelId, u32>,
    /// Bumped when a channel goes (v0.8.2): its message list starts from a fresh scroll state
    /// should the id ever come back.
    pub channel_epochs: HashMap<ChannelId, u32>,
    /// The sidebar rows drawn in the last frame (the autotest checks their layout).
    pub sidebar_rows: Vec<notify::DrawnRow>,
    /// Link-preview cards under chat messages (item 10).
    pub previews: PreviewStore,
    /// Go Live hover cards (v1.1, `stream_cards.rs`).
    pub stream_cards: crate::stream_cards::StreamCards,
    /// Profile pictures: the cache of what the hub sent, and the settings section.
    pub avatars: crate::avatars::Avatars,

    pub voice: VoiceUi,
    pub center: Center,
    pub call: CallUi,
    pub video: VideoUi,
    pub golive: GoLive,
    pub settings_ui: SettingsUi,
    pub notice: Option<String>,
    pub stream_info: Vec<(String, u64, u32, u32)>,
    /// Audio streams of the last stats report (autotest).
    pub audio_info: Vec<crate::backend::AudioInfo>,
    pub stream_info_seq: u64,
    pub auto: Option<Auto>,
    frames: u64,
    alert_policy: AlertPolicy,
    /// Gate + level meter of the voice session / of the mic test (two separate pipelines).
    pub gate_voice: Arc<VoiceGate>,
    pub gate_test: Arc<VoiceGate>,
    /// "Supressão de ruído", shared by the voice session and the mic test.
    pub noise: Arc<NoiseControl>,
    /// Runs while any level meter is visible (see `meter.rs`).
    meter_watch: Option<MeterWatch>,
    /// What the backend was last told about the camera tiles being on screen.
    cams_visible_sent: Option<bool>,
    /// What the backend was last told about the preview of our own Go Live.
    share_preview_sent: Option<vox_capture::PreviewConfig>,
    /// Item 12: roles, personal password, moderation, server settings.
    pub roles: crate::roles::RolesUi,
    /// The server soundboard (see `soundboard.rs`).
    pub sb: crate::soundboard::SoundboardUi,
    /// Profiles: presence, the card that opens on a click, the settings section.
    pub profile: crate::profile::ProfileUi,
    /// Direct messages and private 1:1 calls (see `dms.rs`).
    pub dms: dms::DmUi,
    /// The right-click menu of people and streams (see `context_menu.rs`).
    pub cmenu: crate::context_menu::CtxMenuUi,
    /// Images in the chat: composer, downloads, viewer (see `chat_images.rs`).
    pub images: crate::chat_images::ImagesUi,
    /// The tray icon, the hidden window, Windows notifications and the unread badge (`notify.rs`).
    pub notify: notify::NotifyUi,
    /// Mentions, replies and reactions (`chat_extras.rs`).
    pub chatx: chat_extras::ChatUi,
    /// "Criar / abrir servidor": the server I host in this app, and the invite tools (`host.rs`).
    pub host: crate::host::HostUi,
    /// "Seus servidores salvos" on the home screen (`servers.rs`, v0.9.1).
    pub servers: crate::servers::ServersUi,
    /// The address of the server I am logged into (normalized; empty on the home screen).
    pub session_url: String,
    /// v1.0.1: that server's identity (`None`: an older hub, or the home screen).
    pub session_server_id: Option<String>,
    /// `Cmd::Logout`s whose answer has not come back yet.
    logouts_pending: u32,
    /// The v1.0 component gallery over everything (`--autotest kit`; `ui_kit::gallery`).
    pub kit: Option<crate::ui_kit::Gallery>,
    /// v1.0 shell (`layout.rs`): the center's mode in a call (remembered in settings).
    pub center_mode: crate::layout::CenterMode,
    /// The text channel docked beside the cards (`CenterMode::CardsWithChat`); never the "open" one.
    pub docked_chat_channel: Option<ChannelId>,
    /// The call stage fills the whole window (nothing else is drawn).
    pub stage_fullscreen: bool,
    /// The Calladex card when open (`calladex.rs`; `None` costs nothing).
    pub calladex: Option<calladex::Calladex>,
    /// The last letters typed on the home screen with no field focused (the "calladex" sequence).
    pub calladex_keys: String,
    /// v1.1 (T0): the session's TLS (the lock) and its dialogs (`tls_ui.rs`).
    pub tls: tls_ui::TlsUi,
    /// v1.1 (E0): my devices on the server's list of encryption keys (`privacy_ui.rs`).
    pub privacy: privacy_ui::PrivacyUi,
    /// v1.1 (D1): the donation popup, its QR code and the usage counter (`donate_ui.rs`).
    pub donate: donate_ui::DonateUi,
}

impl App {
    pub fn new(backend: Backend, repaint: Repaint, settings: Settings, persist: bool, auto: Option<Auto>) -> Self {
        let mut form = ConnectForm {
            url: settings.server.clone(),
            user: settings.username.clone(),
            remember: settings.remember_password,
            remember_device: true,
            save_server: true,
            ..Default::default()
        };
        let servers = crate::servers::ServersUi::new(settings.servers.servers.is_empty());
        if let Some(p) = settings.saved_password() {
            form.password = p;
        }
        let golive = GoLive {
            preset: settings.preset,
            audio: settings.share_audio,
            smooth: settings.share_smooth,
            ..Default::default()
        };
        let noise = Arc::new(NoiseControl::new(settings.noise_suppression));
        noise.voice_fx().set(settings.voice_fx);
        let mut app = App {
            backend,
            repaint,
            settings,
            persist,
            keys: Default::default(),
            screen: Screen::Connect,
            form,
            server_name: String::new(),
            me: None,
            livekit_ok: true,
            hub_up: true,
            ready: false,
            channels: Vec::new(),
            online: Vec::new(),
            voice_members: BTreeMap::new(),
            viewers_supported: true,
            logs: HashMap::new(),
            selected: None,
            input: String::new(),
            focus_input: false,
            unread: HashMap::new(),
            channel_epochs: HashMap::new(),
            sidebar_rows: Vec::new(),
            previews: PreviewStore::default(),
            stream_cards: Default::default(),
            avatars: Default::default(),
            voice: VoiceUi::default(),
            center: Center::Chat,
            call: CallUi::default(),
            video: VideoUi::default(),
            golive,
            settings_ui: SettingsUi::default(),
            notice: None,
            stream_info: Vec::new(),
            audio_info: Vec::new(),
            stream_info_seq: 0,
            auto,
            frames: 0,
            alert_policy: AlertPolicy::default(),
            gate_voice: Arc::new(VoiceGate::default()),
            gate_test: Arc::new(VoiceGate::default()),
            noise,
            meter_watch: None,
            cams_visible_sent: None,
            share_preview_sent: None,
            roles: Default::default(),
            sb: Default::default(),
            profile: Default::default(),
            cmenu: Default::default(),
            dms: Default::default(),
            images: Default::default(),
            notify: notify::NotifyUi::new(persist),
            host: crate::host::HostUi::new(),
            chatx: Default::default(),
            servers,
            session_url: String::new(),
            session_server_id: None,
            logouts_pending: 0,
            kit: None,
            center_mode: crate::layout::CenterMode::default(),
            docked_chat_channel: None,
            stage_fullscreen: false,
            calladex: None,
            calladex_keys: String::new(),
            tls: Default::default(),
            privacy: Default::default(),
            donate: donate_ui::DonateUi::new(persist),
        };
        app.center_mode = app.settings.layout.center_mode;
        app.apply_gate_config();
        app.init_video_encoders();
        app.keybinds_init();
        // The camera list tells which camera "Padrão" is, so its saved setup is found even when
        // the settings were never opened.
        app.backend.send(Cmd::ListCameras);
        // Whether my live's still picture may go to the server (before any Go Live can start).
        app.backend.send(Cmd::StreamPreviewUpload(app.settings.share_hover_preview));
        // v1.1: the alert palette the user chose ("Som dos avisos"), for every sound from now on.
        alerts::apply_palette(&app.settings.alerts);
        app
    }

    // ------------------------------------------------------------ actions (buttons and autotest)

    pub fn save_settings(&self) {
        if self.persist || crate::settings::dir_overridden() {
            self.settings.save();
        }
    }

    pub fn connect(&mut self) {
        if self.form.busy {
            return;
        }
        let (url, user) = (self.form.url.trim().to_string(), self.form.user.trim().to_string());
        if url.is_empty() {
            self.form.error = Some("Informe o endereço do servidor.".into());
            return;
        }
        if user.chars().count() < 2 {
            self.form.error = Some("Informe seu nome (pelo menos 2 letras).".into());
            return;
        }
        // v1.0.1: re-typing the credentials after "não é o mesmo servidor": only what is typed.
        let manual = self.form.manual.as_deref() == Some(crate::servers::server_key(&url, &user).as_str());
        if manual {
            self.servers.joining = None;
        }
        // v0.9.1: a saved server's addresses (LAN first), when the form still names it.
        let urls = self.servers_before_connect(&url, &user);
        if vox_proto::looks_like_invite(&url) {
            // v0.8: a `vox1-` invite code instead of an address (see `host.rs`).
            self.connect_invite(&url, &user);
            return;
        }
        self.form.busy = true;
        self.form.error = None;
        // Item 12: a typed personal password wins; otherwise this computer's token for the name.
        let personal = Some(self.form.personal.clone()).filter(|p| !p.is_empty());
        let typed = crate::backend::normalize_url(&url);
        let token_at = if manual { None } else { std::iter::once(typed.clone()).chain(urls.iter().cloned()).find_map(|u| self.settings.device_token(&u, &user).map(|t| (u, t))) };
        let guard = self.saved_guard(&typed, &user, token_at.as_ref().map(|(u, _)| u.as_str()), manual);
        // v1.1 (T0): the certificate this server must have (saved entry, saved login token, my own
        // hosted server), and whether it is a saved address (the trust rules).
        let key = crate::servers::server_key(&typed, &user);
        let entry = self.servers.joining.and_then(|id| self.settings.server_entry(id)).or_else(|| self.settings.servers.servers.iter().find(|e| e.key() == key)).cloned();
        let token_pin = token_at.as_ref().and_then(|(u, _)| self.settings.device_token_pin(u, &user));
        let tls = self.tls_plan(&typed, entry.as_ref().and_then(|e| e.tls_pin()), token_pin, entry.is_some() || token_at.is_some());
        let device_token = token_at.map(|(_, t)| t);
        self.servers_login_started(device_token.is_some(), None);
        tracing::info!(url = %url, user = %user, personal = personal.is_some(), device_token = device_token.is_some(), saved_password = guard.password.is_some(), manual, addresses = urls.len().max(1), "connecting");
        self.backend.send(Cmd::Connect {
            url,
            user,
            password: self.form.password.clone(),
            personal,
            device_token,
            remember_device: self.form.remember_device,
            invite: (!urls.is_empty()).then_some(crate::host::InviteJoin { token: None, urls }),
            guard,
            tls,
        });
    }

    /// v1.0.1: what of this login was saved on this computer, and for which server (see
    /// `backend::Guard`). `typed`: the form's address, normalized; `token_at`: the address the
    /// login token about to be sent was saved under.
    fn saved_guard(&self, typed: &str, user: &str, token_at: Option<&str>, manual: bool) -> crate::backend::Guard {
        use crate::backend::{Binding, Guard};
        // The entry being joined, else the one saved for this address and name.
        let key = crate::servers::server_key(typed, user);
        let entry = self.servers.joining.and_then(|id| self.settings.server_entry(id)).or_else(|| self.settings.servers.servers.iter().find(|e| e.key() == key)).cloned();
        let entry_id = entry.as_ref().and_then(|e| e.server_id());
        // v1.1 (T0): and with which certificate (the token's own, else the entry's).
        let entry_pin = entry.as_ref().and_then(|e| e.tls_pin());
        let token = token_at.map(|at| Binding {
            home: at.to_string(),
            server_id: self.settings.device_token_server(at, user).or_else(|| entry_id.clone()),
            tls_pin: self.settings.device_token_pin(at, user).or(entry_pin),
        });
        // The password in the form is a saved one when it is the entry's (or v0.9.0's single one).
        let pw = &self.form.password;
        let legacy = self.settings.saved_password().filter(|_| crate::servers::server_key(&self.settings.server, &self.settings.username) == key);
        let saved_pw = !manual && !pw.is_empty() && (entry.as_ref().and_then(|e| self.settings.server_password(e)).as_ref() == Some(pw) || legacy.as_ref() == Some(pw));
        let password = saved_pw.then(|| Binding { home: entry.as_ref().map_or_else(|| typed.to_string(), |e| crate::backend::normalize_url(&e.url)), server_id: entry_id.clone(), tls_pin: entry_pin });
        Guard { token, password, confirmed: Some(typed.to_string()) }
    }

    pub fn logout(&mut self) {
        self.backend.send(Cmd::Logout);
        self.logouts_pending += 1;
        self.reset_session();
        self.screen = Screen::Connect;
    }

    fn reset_session(&mut self) {
        self.me = None;
        self.session_url.clear();
        self.session_server_id = None;
        self.tls.session = None;
        self.tls.security_open = false;
        self.ready = false;
        self.hub_up = true;
        self.channels.clear();
        self.online.clear();
        self.voice_members.clear();
        self.logs.clear();
        self.previews.reset();
        self.avatars.store.reset();
        self.selected = None;
        self.unread.clear();
        self.voice = VoiceUi::default();
        self.center = Center::Chat;
        self.call.leave_room();
        self.golive.open = false;
        self.stream_info.clear();
        self.form.busy = false;
        self.reset_roles();
        self.reset_soundboard();
        self.profile.reset();
        self.dm_reset();
        self.images.reset();
        self.chat_reset();
    }

    pub fn select_text(&mut self, id: ChannelId) {
        self.selected = Some(id);
        // The last text channel viewed is the one docked beside the cards (`layout.rs`).
        self.docked_chat_channel = Some(id);
        self.dms.selected = None;
        self.center = Center::Chat;
        self.channel_read(id);
        self.focus_input = true;
        let log = self.logs.entry(id).or_default();
        if !log.requested {
            log.requested = true;
            self.backend.send(Cmd::LoadHistory { channel: id, before: None });
        }
    }

    /// Send the input to the channel whose chat is on screen (the center's, or the docked one).
    pub fn send_message(&mut self) {
        let Some(channel) = self.chat_on_screen().or(self.selected) else { return };
        self.send_message_to(channel);
    }

    pub fn send_message_to(&mut self, channel: ChannelId) {
        if self.calladex_command(crate::chat_images::Target::Channel(channel)) {
            return;
        }
        let text = self.input.trim().to_string();
        let target = crate::chat_images::Target::Channel(channel);
        if self.send_with_images(target, text.clone()) {
            return;
        }
        if text.is_empty() {
            return;
        }
        let extras = self.take_send_extras(target, &text);
        self.input.clear();
        self.backend.send(Cmd::SendMessage { channel, content: text, extras });
    }

    pub fn join_voice(&mut self, channel: ChannelId) {
        // Clicking a voice channel (to join it, or again while in it) shows its call view.
        self.center = Center::Call;
        if self.voice.channel == Some(channel) || self.voice.joining == Some(channel) {
            return;
        }
        self.dm_before_voice_channel();
        self.voice.joining = Some(channel);
        self.voice.joining_since = Some(std::time::Instant::now());
        self.voice.error = None;
        self.voice.retry = None;
        self.voice.warning = None;
        let opts = VoiceOpts {
            input_device: self.settings.input_device.clone(),
            output_device: self.settings.output_device.clone(),
            volume_pct: self.settings.volume_pct,
            ptt: self.ptt_active(),
            user_audio: self.saved_audio_for_known_users(),
            others_muted: self.voice.others_muted,
            gate: self.gate_voice.clone(),
            noise: self.noise.clone(),
        };
        self.backend.send(Cmd::JoinVoice { channel, opts });
    }

    pub fn leave_voice(&mut self) {
        self.dm_leaving_voice();
        let was_in = self.voice.channel.is_some();
        self.backend.send(Cmd::LeaveVoice);
        self.voice.joining = None;
        self.voice_cleared();
        if was_in {
            self.own_alert(AlertKind::Leave, true);
        }
    }

    /// Play an alert for something the user did (or that happened to their own voice session).
    /// `standalone`: the voice session is going away, so use the output device directly.
    /// Not debounced: it is direct feedback.
    fn own_alert(&mut self, kind: AlertKind, standalone: bool) {
        if self.settings.alerts.allows(kind) {
            self.send_alert(kind, standalone);
        }
    }

    /// Alert for another person's doing; subject to the debounce rules.
    /// Returns whether the sound was asked for (settings and debounce allowed it).
    fn remote_alert(&mut self, kind: AlertKind, who: UserId) -> bool {
        if !self.settings.alerts.allows(kind) {
            return false;
        }
        if !self.alert_policy.admit(std::time::Instant::now(), kind, who) {
            tracing::debug!(alert = kind.name(), who, "alert sound skipped (debounce)");
            return false;
        }
        self.send_alert(kind, false);
        true
    }

    fn send_alert(&self, kind: AlertKind, standalone: bool) {
        if self.notifications_muted() {
            tracing::debug!(alert = kind.name(), "alert sound skipped (Não perturbe)");
            return;
        }
        tracing::debug!(alert = kind.name(), "alert sound requested");
        self.backend.send(Cmd::PlayAlert {
            kind,
            volume: self.settings.alerts.gain(),
            device: self.settings.output_device.clone(),
            standalone,
        });
    }

    /// The "Testar" button of the settings dialog: ignores the category switches.
    fn test_alert(&mut self) {
        if self.notifications_muted() {
            self.toast("Não perturbe está ligado: os sons de alerta ficam em silêncio.");
        } else if self.settings.alerts.enabled {
            self.send_alert(AlertKind::Join, false);
        }
    }

    fn voice_cleared(&mut self) {
        if !self.voice.watching.is_empty() {
            let what = if self.voice.watching.len() > 1 { "Transmissões encerradas" } else { "Transmissão encerrada" };
            self.toast(format!("{ICON_TV} {what} (você saiu da voz)."));
        }
        if self.center == Center::Call {
            self.center = Center::Chat;
        }
        // Pins, hidden cards and the strip's scroll are per room visit.
        self.call.leave_room();
        self.voice.channel = None;
        self.voice.share = Share::Off;
        self.voice.share_audio_only = false;
        self.voice.audio_only.clear();
        self.voice.switching_to = None;
        self.voice.watching.clear();
        self.voice.watch_pending.clear();
        self.call.fullscreen_key = None;
        self.voice.speaking.clear();
        self.voice.link = Link::Up;
        self.voice.warning = None;
        self.voice.no_mic = false;
        self.voice.others_muted = false;
        self.voice.camera = CameraUi::Off;
        self.voice.cameras.clear();
        self.voice.e2ee = false;
        self.voice.e2ee_states.clear();
        self.voice.e2ee_plain_call = false;
        self.gate_voice.clear_outputs();
    }

    /// Push the saved input-sensitivity settings into both gates; takes effect on the next audio
    /// chunk, mid-call included.
    pub fn apply_gate_config(&self) {
        let cfg = self.settings.input_gate.sanitized();
        self.gate_voice.set_config(cfg);
        self.gate_test.set_config(cfg);
    }

    /// The microphone in the settings changed: a call in progress follows it at once.
    pub fn apply_input_device(&self) {
        self.backend.send(Cmd::SetInputDevice(self.settings.input_device.clone()));
    }

    pub fn start_mic_test(&mut self) {
        self.settings_ui.mic_test = MicTestUi::Starting;
        self.settings_ui.mic_test_error = None;
        self.backend.send(Cmd::StartMicTest {
            input_device: self.settings.input_device.clone(),
            output_device: self.settings.output_device.clone(),
            volume_pct: self.settings.volume_pct,
            gate: self.gate_test.clone(),
            noise: self.noise.clone(),
        });
    }

    pub fn stop_mic_test(&mut self) {
        if self.settings_ui.mic_test != MicTestUi::Off {
            self.backend.send(Cmd::StopMicTest);
            self.settings_ui.mic_test = MicTestUi::Off;
        }
    }

    /// Whether a level meter is on screen: the voice dock's (drawn in the last frame: the stage is
    /// open, not only the chat) while in a call with a microphone, the settings dialog's while
    /// the mic test runs. v1.0: with only the chat in the center no meter is drawn, and its
    /// thread no longer wakes the window up to 15 times a second while you talk.
    fn meter_visible(&self) -> bool {
        let frame = self.repaint.ctx().map_or(0, |c| c.cumulative_frame_nr());
        let dock = self.call.meter_frame.is_some_and(|f| f + 1 >= frame);
        let in_call = self.screen == Screen::Main && self.voice.channel.is_some() && !self.voice.no_mic && dock;
        in_call || self.settings_ui.mic_test != MicTestUi::Off
    }

    /// The meters' repaint thread runs (the autotests' idle check).
    pub(crate) fn meter_watch_running(&self) -> bool {
        self.meter_watch.is_some()
    }

    /// Start or stop the thread that repaints for the meters, so an idle app has no timer at all.
    fn sync_meter_watch(&mut self) {
        let want = self.meter_visible();
        if want && self.meter_watch.is_none() {
            self.meter_watch = Some(MeterWatch::start(self.repaint.clone(), vec![self.gate_voice.clone(), self.gate_test.clone()]));
        } else if !want {
            self.meter_watch = None;
        }
    }

    /// Whether my own voice is going out right now (gate open or push-to-talk held), or one of
    /// my soundboard sounds is playing for the room (v1.1: the core puts my identity in the
    /// speaking set only for that); drives my own speaking highlight.
    pub fn sending_voice(&self) -> bool {
        self.voice.channel.is_some() && (self.gate_voice.is_open() || self.voice.speaking.contains(&self.voice.identity))
    }

    pub fn ptt_active(&self) -> bool {
        self.settings.ptt_enabled && crate::platform::global_ptt() && crate::keybinds::has_ptt(&self.settings.keybinds)
    }

    pub fn toggle_mute(&mut self) {
        let mute = !self.voice.muted;
        self.backend.send(Cmd::SetMuted(mute));
        if self.voice.channel.is_some() {
            self.own_alert(if mute { AlertKind::Mute } else { AlertKind::Unmute }, false);
        }
    }

    pub fn toggle_deafen(&mut self) {
        let deafen = !self.voice.deafened;
        self.backend.send(Cmd::SetDeafened(deafen));
        if self.voice.channel.is_some() {
            self.own_alert(if deafen { AlertKind::Deafen } else { AlertKind::Undeafen }, false);
        }
    }

    pub fn open_golive(&mut self) {
        self.golive.open = true;
        self.golive.switching = false;
        self.golive.loading = true;
        self.golive.error = None;
        self.golive.preset = self.settings.preset;
        self.golive.audio = self.settings.share_audio;
        self.golive.smooth = self.settings.share_smooth;
        self.backend.send(Cmd::ListSources);
    }

    /// Start Go Live on `pick` with the given quality (what the dialog's button does). The
    /// degradation preference comes from the dialog (`golive.smooth`).
    pub fn start_share(&mut self, pick: Pick, preset: Preset, audio: bool) {
        let (width, height, fps) = preset.dims();
        let smooth = self.golive.smooth;
        let source = match &pick {
            Pick::Window(h) => ShareSource::Window(*h),
            Pick::Monitor(i) => ShareSource::Monitor(*i),
            Pick::App(_) | Pick::AllSound => return self.start_audio_share(pick, self.golive.audio_high),
        };
        let opts = ShareOptions {
            width,
            height,
            fps,
            audio,
            audio_pid: None,
            audio_bitrate: AUDIO_ONLY_NORMAL_BPS,
            capture_cursor: true,
            // Autotest only: VOX_SHARE_ENCODER=software|nvenc|mf|auto picks the encoder path.
            encoder: (!self.persist).then(|| std::env::var("VOX_SHARE_ENCODER").ok()).flatten().and_then(|v| v.parse().ok()).unwrap_or(EncoderChoice::Auto),
            max_bitrate: Some(preset.bitrate()),
            degradation: if smooth { DegradationPreference::Smoothness } else { DegradationPreference::Sharpness },
        };
        self.settings.preset = preset;
        self.settings.share_audio = audio;
        self.settings.share_smooth = smooth;
        self.save_settings();
        self.voice.share = Share::Starting;
        self.voice.share_preset = preset;
        self.voice.share_pick = Some(pick);
        self.voice.share_audio = audio;
        self.voice.share_audio_only = false;
        self.voice.switching_to = None;
        self.golive.open = false;
        self.golive.switching = false;
        self.backend.send(Cmd::StartShare { source, opts });
    }

    /// Start (or restart as) an audio-only Go Live on `pick` (an app or all the sound): no
    /// video track, capture or encoder, stereo Opus at 128 kbps ("Normal") or 256 kbps ("Alta").
    /// A running share of any kind is replaced by the core; the room's viewers follow along.
    pub fn start_audio_share(&mut self, pick: Pick, high: bool) {
        let source = match pick {
            Pick::App(pid) => ShareSource::AudioApp { pid },
            Pick::AllSound => ShareSource::AudioSystem,
            Pick::Window(_) | Pick::Monitor(_) => return,
        };
        self.start_audio_share_source(source, Some(pick), high);
    }

    /// [`App::start_audio_share`] for any audio-only source (the autotest uses a tone).
    pub fn start_audio_share_source(&mut self, source: ShareSource, pick: Option<Pick>, high: bool) {
        let opts = ShareOptions {
            audio: true,
            audio_bitrate: if high { AUDIO_ONLY_HIGH_BPS } else { AUDIO_ONLY_NORMAL_BPS },
            ..ShareOptions::default()
        };
        self.golive.audio_high = high;
        self.voice.share = Share::Starting;
        self.voice.share_pick = pick;
        self.voice.share_audio = true;
        self.voice.share_audio_only = true;
        self.voice.share_audio_high = high;
        self.voice.switching_to = None;
        self.golive.open = false;
        self.golive.switching = false;
        self.backend.send(Cmd::StartShare { source, opts });
    }

    /// "Trocar tela": the same picker as Go Live, but choosing only changes what the running live
    /// captures (quality, bitrate and the fluidez/nitidez choice stay as they were).
    pub fn open_golive_switch(&mut self) {
        self.open_golive();
        self.golive.switching = true;
        self.golive.audio = self.voice.share_audio;
        self.golive.pick = None;
        self.golive.mode = if self.voice.share_audio_only { ShareMode::AudioOnly } else { ShareMode::ScreenAudio };
        self.golive.audio_high = self.voice.share_audio_high;
        self.golive.picker.tab = match self.voice.share_pick {
            Some(Pick::Monitor(_)) => crate::sources::Tab::Screens,
            _ => crate::sources::Tab::Apps,
        };
    }

    /// Switch the running live to `pick` without stopping it.
    pub fn switch_share(&mut self, pick: Pick, audio: bool) {
        let source = match pick {
            Pick::Window(h) => ShareSource::Window(h),
            Pick::Monitor(i) => ShareSource::Monitor(i),
            Pick::App(pid) => ShareSource::AudioApp { pid },
            Pick::AllSound => ShareSource::AudioSystem,
        };
        self.voice.switching_to = Some(pick);
        self.golive.open = false;
        self.golive.switching = false;
        self.backend.send(Cmd::SwitchShare { source, audio });
    }

    pub fn stop_share(&mut self) {
        self.backend.send(Cmd::StopShare);
        self.voice.share = Share::Off;
        self.voice.switching_to = None;
    }

    // ------------------------------------------------------------ camera

    /// The voice bar's "Câmera" button.
    pub fn toggle_camera(&mut self) {
        match self.voice.camera {
            CameraUi::Off => self.start_camera(),
            CameraUi::On { .. } => self.stop_camera(),
            CameraUi::Starting => {}
        }
    }

    pub fn start_camera(&mut self) {
        if self.voice.channel.is_none() {
            return;
        }
        self.voice.camera = CameraUi::Starting;
        self.settings_ui.camera_preview = CameraPreviewUi::Off;
        self.settings_ui.cam.low_light_noticed = false;
        self.send_camera_setup();
        self.backend.send(Cmd::StartCamera {
            id: self.settings.camera_device.clone(),
            name: self.settings.camera_name.clone(),
            preset: self.settings.camera_preset,
        });
    }

    pub fn stop_camera(&mut self) {
        self.backend.send(Cmd::StopCamera);
        self.voice.camera = CameraUi::Off;
    }

    pub fn camera_on(&self) -> bool {
        matches!(self.voice.camera, CameraUi::On { .. })
    }

    /// "Configurar câmera": start / stop the camera preview (with the chosen device and quality).
    pub fn set_camera_preview(&mut self, on: bool) {
        if on {
            self.settings_ui.camera_preview = CameraPreviewUi::Starting;
            self.send_camera_setup();
            self.backend.send(Cmd::CameraPreview(Some((self.settings.camera_device.clone(), self.settings.camera_name.clone(), self.settings.camera_preset))));
        } else if self.settings_ui.camera_preview != CameraPreviewUi::Off {
            self.settings_ui.camera_preview = CameraPreviewUi::Off;
            self.backend.send(Cmd::CameraPreview(None));
        }
    }

    /// Camera tiles are worth receiving: the call view is on screen (or Settings shows our own
    /// camera), the window is not minimized.
    fn cameras_visible(&self, ctx: &egui::Context) -> bool {
        let minimized = crate::app::window_minimized(ctx);
        let call_shown = self.center == Center::Call && !self.call.fullscreen;
        // ("Configurar câmera" shows our own camera while it is on.)
        let settings_self_view = self.camera_window_preview() && self.camera_on();
        self.screen == Screen::Main && self.voice.channel.is_some() && !minimized && (call_shown || settings_self_view)
    }

    /// Tell the backend when the camera tiles appear / disappear (subscriptions and self-view
    /// follow it).
    fn sync_cameras_visible(&mut self, ctx: &egui::Context) {
        let want = self.cameras_visible(ctx);
        if self.voice.channel.is_none() {
            self.cams_visible_sent = None;
            return;
        }
        if self.cams_visible_sent != Some(want) {
            tracing::info!(visible = want, "camera tiles visibility");
            self.cams_visible_sent = Some(want);
            self.backend.send(Cmd::CamerasVisible(want));
        }
    }

    /// Is our own Go Live running (so our stream tile can show the preview)?
    /// Is `m`'s live audio-only ("Só áudio")? Ours is known from what we started; the others' from
    /// the hub's flag (everybody, in any room), or - for an older hub - from the tracks seen in
    /// the voice room we are in.
    pub fn stream_is_audio_only(&self, m: &VoiceMember) -> bool {
        if self.my_id() == Some(m.user.id) {
            return self.voice.share_audio_only && !matches!(self.voice.share, Share::Off);
        }
        let same_room = m.channel_id.is_some() && m.channel_id == self.voice.channel;
        audio_only_live_on(m.streaming && m.audio_only, same_room, self.voice.audio_only.contains(&m.user.id.to_string()))
    }

    /// What the hub says about a streamer's `audio_only` (None: not a voice member we know).
    pub fn streamer_hub_audio_only(&self, identity: &str) -> Option<bool> {
        identity.parse::<UserId>().ok().and_then(|id| self.voice_members.get(&id)).map(|m| m.streaming && m.audio_only)
    }

    /// The same by streamer identity (the watched streams are always in our own room).
    pub fn identity_is_audio_only(&self, identity: &str) -> bool {
        match identity.parse::<UserId>().ok().and_then(|id| self.voice_members.get(&id)) {
            Some(m) => self.stream_is_audio_only(m),
            None => self.voice.audio_only.contains(identity),
        }
    }

    pub fn my_share_live(&self) -> bool {
        matches!(self.voice.share, Share::Live { .. })
    }

    /// Our live has a picture (so the tile can show the preview): not a "Só áudio" one.
    pub fn my_preview_live(&self) -> bool {
        self.my_share_live() && !self.voice.share_audio_only && self.settings.show_self_preview
    }

    /// "Ocultar prévia" / "Mostrar prévia" (my stream tile and my own menu): the same switch as
    /// Settings, saved.
    pub fn set_self_preview(&mut self, on: bool) {
        if self.settings.show_self_preview != on {
            self.settings.show_self_preview = on;
            if !on {
                self.unpin_self_stream();
            }
            self.save_settings();
        }
    }

    /// Tell the backend how the preview of our own live should run, from where our stream tile
    /// is this frame: off while it is not visible at all (chat view, minimized, a fullscreen
    /// stream, a dialog or menu covering it entirely), at the low rate in the grid, faster and
    /// bigger when focused. Called by `sync_video` with the tiles' placement and the ones hidden
    /// under an overlay (a menu over part of the tile only cuts that part out: it keeps running).
    pub(crate) fn sync_share_preview(&mut self, ctx: &egui::Context, covered: &[String]) {
        use vox_client_core::golive::SELF_SHARE_KEY;
        let rect = self.video.want.get(SELF_SHARE_KEY).copied();
        let minimized = crate::app::window_minimized(ctx);
        let visible = self.my_preview_live() && self.screen == Screen::Main && !minimized && !covered.iter().any(|k| k == SELF_SHARE_KEY) && rect.is_some();
        let last = self.share_preview_sent.unwrap_or_default();
        let want = match rect.filter(|_| visible) {
            Some(r) => {
                let (_, _, w, _) = crate::call::to_px(r, ctx.pixels_per_point());
                let (max_width, max_height) = crate::call::preview_size_for(w);
                let fps = if self.self_preview_focused() { crate::call::PREVIEW_FPS_FOCUSED } else { crate::call::PREVIEW_FPS };
                vox_capture::PreviewConfig { on: true, fps, max_width, max_height }
            }
            None => vox_capture::PreviewConfig { on: false, ..last },
        };
        if self.share_preview_sent != Some(want) {
            tracing::info!(on = want.on, fps = want.fps, max = format!("{}x{}", want.max_width, want.max_height), focused = self.self_preview_focused(), "share preview request");
            self.share_preview_sent = Some(want);
            self.backend.send(Cmd::SharePreview(want));
        }
    }

    /// A minimized window runs no `ui()` pass, so the preview is switched off from `logic()`
    /// (which still runs); the next `ui()` pass after the restore turns it back on.
    fn pause_share_preview_when_minimized(&mut self, ctx: &egui::Context) {
        let Some(sent) = self.share_preview_sent.filter(|c| c.on) else { return };
        if self.notify.hidden || crate::app::window_minimized(ctx) {
            let off = vox_capture::PreviewConfig { on: false, ..sent };
            tracing::info!("share preview request: off (window minimized)");
            self.share_preview_sent = Some(off);
            self.backend.send(Cmd::SharePreview(off));
        }
    }

    /// Is `identity`'s stream being watched (confirmed by the core)?
    pub fn is_watching(&self, identity: &str) -> bool {
        self.voice.watching.iter().any(|w| w == identity)
    }

    /// "Assistir": start watching `identity` next to the streams already watched. Refused, with
    /// a message, when `MAX_WATCHED` are already watched (or about to be). Returns whether the
    /// stream is watched or on its way.
    pub fn watch(&mut self, identity: &str) -> bool {
        if self.is_watching(identity) {
            return true;
        }
        self.voice.watch_pending.retain(|(_, at)| at.elapsed() < WATCH_PENDING_TIMEOUT);
        if self.voice.watch_pending.iter().any(|(w, _)| w == identity) {
            return true;
        }
        if !watch_allowed(self.voice.watching.len(), self.voice.watch_pending.len()) {
            tracing::info!(identity, watching = ?self.voice.watching, "watch refused: limit reached");
            self.toast(watch_limit_text());
            return false;
        }
        self.voice.watch_pending.push((identity.to_string(), std::time::Instant::now()));
        self.backend.send(Cmd::Watch(identity.to_string()));
        true
    }

    /// "Parar de assistir" one stream (the others keep playing).
    pub fn unwatch(&mut self, identity: &str) {
        self.voice.watch_pending.retain(|(w, _)| w != identity);
        self.backend.send(Cmd::Unwatch(identity.to_string()));
    }

    pub(crate) fn name_of(&self, id: UserId) -> String {
        self.online
            .iter()
            .find(|u| u.id == id)
            .map(|u| u.shown_name().to_string())
            .or_else(|| self.voice_members.get(&id).map(|m| m.user.shown_name().to_string()))
            .unwrap_or_else(|| format!("#{id}"))
    }

    pub fn my_id(&self) -> Option<UserId> {
        self.me.as_ref().map(|u| u.id)
    }

    /// Who is watching `streamer`'s live right now, by name (empty when they are not live).
    /// Only people in the streamer's own room count, whatever the hub said last.
    pub fn viewers_of(&self, streamer: UserId) -> Vec<String> {
        let Some(s) = self.voice_members.get(&streamer).filter(|s| s.streaming && s.channel_id.is_some()) else {
            return Vec::new();
        };
        let mut names: Vec<String> = self
            .voice_members
            .values()
            .filter(|m| m.user.id != streamer && m.channel_id == s.channel_id && m.watching.contains(&streamer))
            .map(|m| m.user.shown_name().to_string())
            .collect();
        names.sort_by_key(|n| n.to_lowercase());
        names
    }

    /// "Ana, Bruno" or "Ninguém assistindo ainda".
    pub fn viewers_text(names: &[String]) -> String {
        if names.is_empty() {
            "Ninguém assistindo ainda".to_string()
        } else {
            format!("Assistindo: {}", names.join(", "))
        }
    }

    /// Whether to show the camera icon for `m`: the hub says so, or - for someone in the room we
    /// are in - the voice room does (an older hub never reports it; the hub's word can lag, the
    /// room's is first-hand). Outside our room only the hub can tell.
    pub fn camera_flag_of(&self, m: &VoiceMember) -> bool {
        let same_room = self.voice.channel.is_some() && m.channel_id == self.voice.channel;
        let room_says = if self.my_id() == Some(m.user.id) { self.camera_on() } else { self.voice.cameras.contains(&m.user.id.to_string()) };
        camera_icon_on(m.camera, same_room, room_says)
    }

    /// Voice members of the channel we are in (including us).
    pub fn members_here(&self) -> Vec<&VoiceMember> {
        if self.voice.channel == Some(dms::DM_CALL_CHANNEL) {
            // A private call: me and the other person (the hub's voice state never has it).
            return self.dms.call_members.iter().collect();
        }
        match self.voice.channel {
            Some(ch) => self.voice_members.values().filter(|m| m.channel_id == Some(ch)).collect(),
            None => Vec::new(),
        }
    }

    // ------------------------------------------------------------ backend events

    pub fn pump_events(&mut self) {
        while let Ok(ev) = self.backend.rx.try_recv() {
            self.on_event(ev);
        }
        if self.noise.take_fallback_notice() {
            // The mic chain could not run "IA máxima" in real time and already switched to IA leve.
            self.settings.noise_suppression = self.noise.level();
            self.settings_ui.noise_fell_back = true;
            self.save_settings();
            self.notice = Some(format!("{ICON_WARN} {NOISE_FALLBACK_TEXT}"));
        }
        self.keybinds_pump();
    }

    fn on_event(&mut self, ev: Ev) {
        match ev {
            Ev::ConnectFailed(msg) => {
                self.form.busy = false;
                self.form.error = Some(msg);
                self.servers_login_failed();
            }
            Ev::NotSameServer(msg) => {
                // v1.0.1: nothing saved was sent. The form asks for the credentials again; the
                // next login at this address goes only with what is typed.
                self.form.busy = false;
                self.form.error = Some(msg);
                self.form.password.clear();
                self.form.manual = Some(crate::servers::server_key(&self.form.url, &self.form.user));
                self.servers.joining = None;
                self.servers_login_failed();
                self.servers.focus_password = true;
            }
            Ev::NeedPersonalPassword { message, forget_device } => {
                self.form.busy = false;
                self.form.need_personal = true;
                self.form.error = Some(message);
                self.servers_login_failed();
                if forget_device {
                    let url = crate::backend::normalize_url(&self.form.url);
                    let user = self.form.user.trim().to_string();
                    self.settings.set_device_token(&url, &user, None);
                    // A saved server joined at another of its addresses keeps the token there.
                    if let Some(id) = self.servers.joining {
                        self.settings.forget_server_device(id);
                    }
                    self.save_settings();
                }
            }
            Ev::TlsMismatch { url, expected, got } => self.on_tls_mismatch(url, expected, got),
            Ev::PlainPrompt { url } => self.on_plain_prompt(url),
            Ev::Admin { cmd, result } => self.on_admin_reply(cmd, result),
            Ev::Invite(ev) => self.on_invite_event(ev),
            Ev::LoggedIn {
                server_name,
                user,
                url,
                livekit_ok,
                device_token,
                server_id,
                pin,
                tls_pin,
                tls_learned,
            } => {
                self.form.busy = false;
                self.form.error = None;
                self.form.manual = None;
                self.server_name = server_name;
                self.me = Some(user.clone());
                self.livekit_ok = livekit_ok;
                self.screen = Screen::Main;
                // v1.1 (T0): the certificate is kept when it was the pinned one, or (first use) when
                // this login confirmed the server.
                let keep_pin = tls_pin.filter(|_| !tls_learned || pin);
                if let Some(t) = device_token {
                    // v1.0.1: a token belongs to the server that issued it (v1.1: and to the
                    // certificate it came through).
                    self.settings.set_device_token(&url, &user.username, Some(&t));
                    self.settings.set_device_token_server(&url, &user.username, server_id.as_deref());
                    self.settings.set_device_token_pin(&url, &user.username, tls_pin);
                } else if pin && self.settings.device_token_server(&url, &user.username).is_none() {
                    // A token saved before v1.0.1, just accepted here: it is this server's.
                    self.settings.set_device_token_server(&url, &user.username, server_id.as_deref());
                }
                if self.settings.device_token_pin(&url, &user.username).is_none() {
                    self.settings.set_device_token_pin(&url, &user.username, keep_pin);
                }
                self.tls.session = tls_pin;
                self.tls.plain_ok_for = None;
                tracing::info!(tls = tls_pin.is_some(), first_use = tls_learned, kept = keep_pin.is_some(), "logged in");
                let personal_typed = !self.form.personal.is_empty();
                self.form.personal.clear();
                self.form.need_personal = false;
                self.roles.prompt_later = false;
                // v0.9.1: the saved server list (which also keeps v0.9.0's single-server fields).
                let confirmed = server_id.as_deref().filter(|_| pin);
                self.servers_logged_in(&url, &user.username, &self.server_name.clone(), personal_typed, confirmed, keep_pin);
                self.session_url = url;
                self.session_server_id = server_id;
                self.save_settings();
            }
            // The answer to my own "Trocar de servidor": the app is already back home, and a login
            // started since (one click on the list) must not be disturbed.
            Ev::LoggedOut { reason: None } if self.logouts_pending > 0 => self.logouts_pending -= 1,
            Ev::LoggedOut { reason } => {
                self.reset_session();
                self.screen = Screen::Connect;
                self.form.error = reason;
            }
            Ev::Hub(ev) => self.on_hub(ev),
            Ev::Sound(ev) => self.on_sound_event(ev),
            Ev::ViewersSupported(ok) => self.viewers_supported = ok,
            // The channel went while the page was on its way.
            Ev::History { channel, .. } if !self.channels.iter().any(|c| c.id == channel) => {}
            Ev::History { channel, before, messages } => {
                let log = self.logs.entry(channel).or_default();
                log.loading_older = false;
                let complete = messages.len() < HISTORY_PAGE;
                if complete {
                    log.no_more = true;
                }
                // The page is the hub's truth for what it covers: deleted meanwhile = gone here
                // too, and an id an older hub gave to another message shows that message.
                let out = chat_extras::merge_page(&mut log.msgs, messages, before, complete, |m| m.id, same_message);
                let target = chat_extras::Target::Channel(channel);
                for m in out.deleted {
                    self.forget_message(target, m.id, &m.attachments);
                }
                for m in out.replaced {
                    self.forget_images(&m.attachments);
                }
                self.chat_on_history(target);
            }
            Ev::Avatar(ev) => self.on_avatar_event(ev),
            Ev::StreamCard(ev) => self.on_stream_card_event(ev),
            Ev::E2ee(ev) => self.on_e2ee_event(ev),
            Ev::Profile(ev) => self.on_profile_event(ev),
            Ev::Dm(ev) => self.on_dm_event(ev),
            Ev::Image(ev) => {
                self.on_image_event(ev);
                self.pump_images();
            }
            Ev::Preview { url, reply } => {
                self.previews.on_preview(&url, reply, std::time::Instant::now());
                self.pump_previews();
            }
            Ev::PreviewImage { url, bytes } => {
                self.previews.on_image(&url, bytes);
                self.pump_previews();
            }
            Ev::VoiceJoining(ch) => {
                self.voice.joining = Some(ch);
                self.voice.joining_since.get_or_insert_with(std::time::Instant::now);
            }
            Ev::VoiceJoined {
                channel,
                identity,
                warning,
                no_mic,
                e2ee,
            } => {
                self.voice.joining = None;
                self.voice.joining_since = None;
                self.voice.e2ee = e2ee;
                self.voice.e2ee_states.clear();
                // v1.1 (E1): a DM call this computer could have encrypted, but the other side's app
                // is older (the hub decided it plain).
                let plain_call = channel == dms::DM_CALL_CHANNEL && !e2ee && self.privacy.view.status == crate::backend::e2ee::E2eeStatus::Active;
                self.voice.e2ee_plain_call = plain_call;
                let warning = if plain_call {
                    let peer = self.dms.voice_dm.and_then(|dm| self.dms.conv(dm).map(|c| (c.peer.id, c.peer.shown_name().to_string())));
                    let peer_has_device = peer.as_ref().is_some_and(|(id, _)| self.privacy.view.devices_of.get(id).is_some_and(|d| !d.is_empty()));
                    let name = peer.map(|(_, n)| n).unwrap_or_else(|| "a outra pessoa".into());
                    // (Audit: "uses an old version" only when that is what we know.)
                    let note = crate::call::e2ee_plain_call_note(&name, self.privacy.view.directory_read, peer_has_device);
                    self.toast(format!("{} {note}", ph::LOCK_SIMPLE_OPEN));
                    Some(match warning {
                        Some(w) => format!("{w} · {note}"),
                        None => note,
                    })
                } else {
                    warning
                };
                self.voice.warning = warning;
                self.voice.no_mic = no_mic;
                self.voice.channel = Some(channel);
                self.voice.identity = identity;
                self.voice.link = Link::Up;
                self.voice.error = None;
                self.voice.share = Share::Off;
                self.voice.switching_to = None;
                self.voice.watching.clear();
                self.voice.watch_pending.clear();
                self.roles.session_server_muted = false;
                self.own_alert(AlertKind::Join, false);
            }
            Ev::VoiceFailed { channel, message } => {
                self.voice.joining = None;
                self.voice.joining_since = None;
                self.voice.retry = channel;
                self.voice.error = Some(message);
            }
            Ev::VoiceLeft => {
                self.voice.joining = None;
                self.voice.joining_since = None;
                self.voice_cleared();
            }
            Ev::Voice(ev) => self.on_voice(ev),
            Ev::CallNewDevice { identity } => {
                let who = identity.parse::<UserId>().map(|id| self.name_of(id)).unwrap_or(identity);
                self.toast(format!("{} {who} entrou de um aparelho novo", ph::SHIELD));
            }
            Ev::LocalVoice { muted, deafened } => {
                self.voice.muted = muted;
                self.voice.deafened = deafened;
            }
            Ev::Sources { windows, monitors, error } => {
                self.golive.loading = false;
                self.golive.windows = windows;
                self.golive.monitors = monitors;
                self.golive.error = error;
                if self.golive.open {
                    // Pictures of every listed window and screen, in the background (item 8c).
                    let repaint = self.repaint.clone();
                    let audio = self.golive.mode == ShareMode::AudioOnly;
                    self.golive.picker.sources_listed(&self.golive.windows, &self.golive.monitors, Arc::new(move || repaint.wake()), audio);
                }
                // Drop a selection that no longer exists.
                let still = match &self.golive.pick {
                    Some(Pick::Window(h)) => self.golive.windows.iter().any(|w| w.hwnd == *h),
                    Some(Pick::Monitor(i)) => self.golive.monitors.iter().any(|m| m.index == *i),
                    Some(Pick::App(pid)) => self.golive.windows.iter().any(|w| w.process_id == *pid),
                    Some(Pick::AllSound) | None => true,
                };
                if !still {
                    self.golive.pick = None;
                }
            }
            Ev::Devices {
                inputs,
                outputs,
                default_input,
                default_output,
            } => {
                self.settings_ui.inputs = inputs;
                self.settings_ui.outputs = outputs;
                self.settings_ui.default_input = default_input;
                self.settings_ui.default_output = default_output;
            }
            Ev::ShareStarted { encoder } => self.voice.share = Share::Live { encoder },
            Ev::ShareFailed(msg) => {
                self.voice.share = Share::Off;
                self.voice.switching_to = None;
                self.notice = Some(msg);
            }
            Ev::ShareStopped => {
                self.voice.share = Share::Off;
                self.voice.switching_to = None;
            }
            Ev::ShareSwitched { audio, audio_error } => {
                if let Some(pick) = self.voice.switching_to.take() {
                    self.voice.share_pick = Some(pick);
                }
                self.voice.share_audio = audio;
                if let Some(msg) = audio_error {
                    self.notice = Some(msg);
                }
            }
            Ev::SwitchFailed(msg) => {
                // The live carries on with the previous source.
                self.voice.switching_to = None;
                self.notice = Some(msg);
            }
            Ev::Notice(msg) => self.notice = Some(msg),
            Ev::MicTestStarted { warning } => {
                if self.settings_ui.mic_test == MicTestUi::Starting {
                    self.settings_ui.mic_test = MicTestUi::On(warning);
                } else {
                    self.backend.send(Cmd::StopMicTest);
                }
            }
            Ev::MicTestFailed(msg) => {
                self.settings_ui.mic_test = MicTestUi::Off;
                self.settings_ui.mic_test_error = Some(msg);
            }
            Ev::AudioInfo(info) => self.audio_info = info,
            Ev::StreamInfo(info) => {
                self.stream_info = info;
                self.stream_info_seq += 1;
            }
            Ev::CameraStarted { encoder, info } => {
                if self.voice.camera == CameraUi::Starting && self.voice.channel.is_some() {
                    self.voice.camera = CameraUi::On { encoder, info };
                } else {
                    // Turned off (or left) while it was starting.
                    self.backend.send(Cmd::StopCamera);
                }
            }
            Ev::CameraFailed(msg) => {
                self.voice.camera = CameraUi::Off;
                self.notice = Some(format!("{ICON_CAMERA} {msg}"));
            }
            Ev::CameraStopped => {
                if self.voice.camera != CameraUi::Starting {
                    self.voice.camera = CameraUi::Off;
                }
            }
            Ev::Cameras { list, error } => {
                self.settings_ui.cameras = list;
                self.settings_ui.cameras_error = error;
                // The saved setup of the camera the settings refer to, and (only while
                // "Configurar câmera" is open) its native formats.
                self.send_camera_setup();
                if self.settings_ui.cam.window {
                    self.request_camera_formats();
                }
            }
            Ev::CameraFormats { key, list } => {
                if key == self.settings_ui.cam.formats_for {
                    self.settings_ui.cam.formats = list;
                }
            }
            Ev::CameraLive(l) => self.camera_live(l),
            Ev::CameraLiveOff => {
                self.settings_ui.cam.live = None;
                self.settings_ui.cam.low_light_noticed = false;
            }
            Ev::CameraPreview(res) => {
                if self.settings_ui.camera_preview == CameraPreviewUi::Starting {
                    self.settings_ui.camera_preview = match res {
                        Ok(()) => CameraPreviewUi::On,
                        Err(e) => CameraPreviewUi::Failed(e),
                    };
                } else if res.is_ok() {
                    self.backend.send(Cmd::CameraPreview(None));
                }
            }
            Ev::CameraNativeClosed(res) => self.camera_native_closed(res),
        }
    }

    fn on_hub(&mut self, ev: HubEvent) {
        match ev {
            HubEvent::Connected => {
                tracing::info!("hub event socket connected");
                self.hub_up = true;
            }
            HubEvent::Disconnected { reason } => {
                tracing::warn!(%reason, "hub connection lost");
                self.hub_up = false;
            }
            HubEvent::AuthFailed => self.hub_up = false,
            HubEvent::Server(ev) => match ev {
                ServerEvent::Ready {
                    user,
                    mut channels,
                    online,
                    voice,
                    account,
                    chat,
                    ..
                } => {
                    channels.sort_by_key(|c| (c.position, c.id));
                    // No sounds for the snapshot, nor while everybody re-announces after it.
                    self.alert_policy.on_snapshot(std::time::Instant::now());
                    self.me = Some(user);
                    self.on_roles_ready(account);
                    self.profile_on_ready();
                    self.dm_on_ready();
                    self.images_on_ready();
                    self.chat_on_ready(chat, &online);
                    self.channels = channels;
                    self.online = online;
                    self.voice_members = voice
                        .into_iter()
                        .filter(|m| m.channel_id.is_some())
                        .map(|m| (m.user.id, m))
                        .collect();
                    self.hub_up = true;
                    self.ready = true;
                    // Channels deleted while we were away: their state goes before anything reloads.
                    self.prune_channels();
                    tracing::info!(channels = self.channels.len(), online = self.online.len(), "hub snapshot received");
                    // After a reconnect, pick up what was said while we were away.
                    let reload: Vec<ChannelId> = self.logs.iter().filter(|(_, l)| l.requested).map(|(id, _)| *id).collect();
                    for id in reload {
                        self.backend.send(Cmd::LoadHistory { channel: id, before: None });
                    }
                    let selected_ok = self
                        .selected
                        .is_some_and(|s| self.channels.iter().any(|c| c.id == s && c.kind == ChannelKind::Text));
                    if !selected_ok {
                        if let Some(first) = self.channels.iter().find(|c| c.kind == ChannelKind::Text).map(|c| c.id) {
                            self.select_text(first);
                        }
                    }
                }
                ServerEvent::MessageCreated { message } => {
                    let ch = message.channel_id;
                    self.chatx.note_user(&message.author);
                    // Counter, sound and Windows notification (see `notify.rs`).
                    self.channel_message_arrived(&message);
                    let log = self.logs.entry(ch).or_default();
                    let replaced = insert_message(&mut log.msgs, message);
                    if let Some(old) = replaced {
                        // An older hub gave a deleted message's id to this one.
                        self.forget_images(&old.attachments);
                    }
                    let log = self.logs.entry(ch).or_default();
                    if log.msgs.len() > MAX_KEPT_MESSAGES {
                        let extra = log.msgs.len() - MAX_KEPT_MESSAGES;
                        log.msgs.drain(..extra);
                    }
                }
                ServerEvent::UserOnline { user } => {
                    let already = self.online.iter().any(|u| u.id == user.id);
                    if let Some(kind) = alerts::online_alert(self.my_id(), &user, already) {
                        self.remote_alert(kind, user.id);
                    }
                    self.chatx.note_user(&user);
                    if !already {
                        self.online.push(user);
                    }
                }
                ServerEvent::UserOffline { user_id } => {
                    let was_online = self.online.iter().any(|u| u.id == user_id);
                    // Voice membership is the hub's `VoiceStateChanged`, which it sends before a real
                    // disconnect's `UserOffline`: someone who merely went invisible stays in their room.
                    if let Some(kind) = alerts::offline_alert(self.my_id(), self.voice.channel, user_id, was_online, None) {
                        self.remote_alert(kind, user_id);
                    }
                    self.profile_on_offline(user_id);
                    self.online.retain(|u| u.id != user_id);
                    self.check_watched_alive();
                }
                ServerEvent::VoiceStateChanged { member } => {
                    let prev = self.voice_members.get(&member.user.id).cloned();
                    let i_am_live = matches!(self.voice.share, Share::Live { .. });
                    let kind = alerts::voice_alert(self.my_id(), self.voice.channel, prev.as_ref(), &member)
                        .or_else(|| alerts::viewer_alert(self.my_id(), self.voice.channel, i_am_live, prev.as_ref(), &member));
                    if let Some(kind) = kind {
                        self.remote_alert(kind, member.user.id);
                    }
                    if member.channel_id.is_some() {
                        if member.channel_id == self.voice.channel && self.my_id() != Some(member.user.id) {
                            self.push_saved_audio(&member.user);
                        }
                        self.voice_members.insert(member.user.id, member);
                    } else {
                        self.voice_members.remove(&member.user.id);
                    }
                    self.check_watched_alive();
                }
                ServerEvent::PresenceChanged { user_id, presence, status } => self.on_presence_changed(user_id, presence, status),
                // Reactions (v0.8): channels, and my DMs (the hub sends those only to the two people).
                ServerEvent::ReactionChanged { channel_id, message_id, emoji, user_id, added } => {
                    self.on_reaction(chat_extras::Target::Channel(channel_id), message_id, &emoji, user_id, added)
                }
                ServerEvent::DmReactionChanged { dm_id, message_id, emoji, user_id, added } => self.on_reaction(chat_extras::Target::Dm(dm_id), message_id, &emoji, user_id, added),
                // Direct messages and private calls (only ever about my own conversations).
                ev @ (ServerEvent::DmMessageCreated { .. }
                | ServerEvent::DmMessageDeleted { .. }
                | ServerEvent::DmRead { .. }
                | ServerEvent::CallInvite { .. }
                | ServerEvent::CallAccepted { .. }
                | ServerEvent::CallDeclined { .. }
                | ServerEvent::CallEnded { .. }) => self.on_dm_server_event(ev),
                ServerEvent::Pong { .. } => {}
                ServerEvent::Error { error } => self.notice = Some(crate::soundboard::server_error_text(&error)),
                // The soundboard is handled by the backend, which reports it as `Ev::Sound`.
                ServerEvent::SoundPlayed { .. } | ServerEvent::SoundStopped { .. } | ServerEvent::SoundsChanged { .. } => {}
                // v1.1 (E0): the device directory is the backend's too (`Ev::E2ee`).
                ServerEvent::E2eeDevicesChanged { .. } => {}
                // Item 12: deleted messages, channel changes, roles, moderation.
                ev @ (ServerEvent::MessageDeleted { .. }
                | ServerEvent::ChannelsChanged { .. }
                | ServerEvent::UserUpdated { .. }
                | ServerEvent::Moderated { .. }) => self.on_roles_event(ev),
            },
        }
    }

    fn on_voice(&mut self, ev: VoiceEvent) {
        self.dm_on_voice(&ev);
        match ev {
            VoiceEvent::ActiveSpeakers { identities } => self.voice.speaking = identities.into_iter().collect(),
            VoiceEvent::Reconnecting => self.voice.link = Link::Reconnecting,
            VoiceEvent::Reconnected => self.voice.link = Link::Up,
            VoiceEvent::Disconnected { reason } => {
                let was_in = self.voice.channel.is_some();
                self.voice_cleared();
                if was_in {
                    self.own_alert(AlertKind::Leave, true);
                }
                self.voice.error = Some(format!(
                    "Você foi desconectado da voz ({reason}). Clique no canal para entrar de novo."
                ));
            }
            VoiceEvent::LocalShareEnded { reason } => {
                tracing::info!(%reason, "own Go Live ended");
                self.voice.share = Share::Off;
                self.voice.switching_to = None;
                self.notice = Some("Sua transmissão terminou (a janela foi fechada ou a captura parou).".into());
            }
            VoiceEvent::WatchStarted { identity } => {
                self.voice.watch_pending.retain(|(w, _)| *w != identity);
                if !self.is_watching(&identity) {
                    self.voice.watching.push(identity);
                }
                self.voice.watch_started_count += 1;
            }
            VoiceEvent::WatchEnded { identity, reason } => {
                self.unpin_stream(&identity);
                if self.call.fullscreen_key.as_deref() == Some(identity.as_str()) {
                    self.call.fullscreen_key = None;
                }
                self.voice.watch_pending.retain(|(w, _)| *w != identity);
                if self.is_watching(&identity) {
                    self.voice.watching.retain(|w| *w != identity);
                    // Not for the user's own doing (button, leaving, closing the stream's window).
                    if !matches!(reason.as_str(), "stopped" | "left room" | "viewer window closed") {
                        let who = identity.parse::<UserId>().map(|id| self.name_of(id)).unwrap_or(identity.clone());
                        self.toast(format!("{ICON_TV} A transmissão de {who} foi encerrada."));
                    }
                }
                self.voice.watch_ended_count += 1;
            }
            VoiceEvent::ShareKind { identity, audio_only } => {
                if audio_only {
                    self.voice.audio_only.insert(identity);
                } else {
                    self.voice.audio_only.remove(&identity);
                }
            }
            VoiceEvent::CameraStarted { identity } => {
                self.voice.cameras.insert(identity);
            }
            VoiceEvent::CameraStopped { identity } => {
                self.unpin_camera(&identity);
                self.voice.cameras.remove(&identity);
            }
            VoiceEvent::LocalCameraEnded { reason } => {
                tracing::info!(%reason, "own camera ended");
                self.voice.camera = CameraUi::Off;
                self.notice = Some(format!("{ICON_CAMERA} Sua câmera desligou: {reason}"));
            }
            VoiceEvent::ServerMuted { muted } => self.roles.session_server_muted = muted,
            VoiceEvent::Mic(status) => {
                tracing::info!(?status, "own microphone changed");
                match crate::backend::mic_status_text(&status) {
                    Some(text) => self.notice = Some(text),
                    // A new choice: what was said about the old one no longer applies.
                    None if self.notice.as_deref().is_some_and(crate::backend::is_mic_notice) => self.notice = None,
                    None => {}
                }
            }
            VoiceEvent::MicAvailable => {
                self.voice.no_mic = false;
                self.voice.warning = self.voice.warning.take().and_then(crate::backend::warning_without_mic);
            }
            VoiceEvent::EncoderSwitched { camera, label, backend } => {
                tracing::warn!(camera, %label, %backend, "own video switched encoder (unstable video)");
                if camera {
                    if let CameraUi::On { encoder, .. } = &mut self.voice.camera {
                        *encoder = backend;
                    }
                } else if let Share::Live { encoder } = &mut self.voice.share {
                    *encoder = backend;
                }
                self.notice = Some(format!("Vídeo instável: trocando para {label}"));
            }
            VoiceEvent::E2eeState { identity, state } => {
                if identity != self.voice.identity {
                    self.voice.e2ee_states.insert(identity, state);
                }
            }
            VoiceEvent::E2eePeerDevice { .. }
            | VoiceEvent::Connected { .. }
            | VoiceEvent::ParticipantJoined { .. }
            | VoiceEvent::ParticipantLeft { .. }
            | VoiceEvent::ShareStarted { .. }
            | VoiceEvent::ShareStopped { .. } => {}
        }
    }

    // ------------------------------------------------------------ push-to-talk

    /// Push-to-talk or a shortcut changed: the hook and the matcher follow (`keybinds_ui.rs`),
    /// the voice session hears about push-to-talk, and the settings are saved.
    fn ptt_settings_changed(&mut self) {
        self.sync_keybinds();
        if self.voice.channel.is_some() {
            let on = self.ptt_active();
            self.backend.send(Cmd::SetPtt(on));
            if !on {
                self.backend.send(Cmd::SetMuted(false));
            }
        }
        self.save_settings();
    }

    // ------------------------------------------------------------ screens

    // ------------------------------------------------------------ local volume / mute

    /// Saved local audio settings of everyone we know by name, keyed by voice identity (the
    /// user id), for the ones that differ from the defaults.
    fn saved_audio_for_known_users(&self) -> HashMap<String, vox_client_core::UserAudio> {
        let mut out = HashMap::new();
        let known = self.online.iter().chain(self.voice_members.values().map(|m| &m.user));
        for u in known {
            let pref = self.settings.user_audio(&u.username);
            if !pref.is_default() {
                out.insert(u.id.to_string(), pref.to_core());
            }
        }
        out
    }

    /// Hand a person's saved settings to the running voice session (when they show up in it).
    fn push_saved_audio(&self, user: &User) {
        let pref = self.settings.user_audio(&user.username);
        if !pref.is_default() {
            self.backend.send(Cmd::SetUserAudio { identity: user.id.to_string(), audio: pref.to_core() });
        }
    }

    /// Change (and remember) the local audio settings of `user`. `save` writes settings.json;
    /// sliders pass `false` while dragging and save once on release.
    pub fn set_user_pref(&mut self, user: &User, pref: UserAudioPref, save: bool) {
        self.settings.set_user_audio(&user.username, pref);
        self.backend.send(Cmd::SetUserAudio { identity: user.id.to_string(), audio: pref.to_core() });
        if save {
            self.save_settings();
        }
    }

    pub fn toggle_others_muted(&mut self) {
        self.voice.others_muted = !self.voice.others_muted;
        self.backend.send(Cmd::SetOthersMuted(self.voice.others_muted));
    }

    /// Button next to "Parar de assistir": volume and mute of the stream's audio.
    pub(crate) fn stream_audio_menu(&mut self, ui: &mut egui::Ui, user: &User) {
        let before = self.settings.user_audio(&user.username);
        let mut pref = before;
        let icon = if pref.stream_muted { ICON_SPEAKER_OFF } else { ICON_VOICE };
        let mut slider_state = (false, false);
        let several = self.voice.watching.len() > 1;
        let mut focus_audio = self.settings.stream_focus_audio;
        ui.menu_button(RichText::new(icon).small(), |ui| {
            ui.set_min_width(230.0);
            ui.label(RichText::new(format!("Áudio da transmissão de {}", user.shown_name())).strong());
            let slider = ui.add(egui::Slider::new(&mut pref.stream_volume_pct, 0..=200).suffix("%").text("Volume"));
            ui.checkbox(&mut pref.stream_muted, "Silenciar transmissão");
            if several {
                // Item 9: with several streams, optionally hear only the one shown big.
                ui.separator();
                ui.checkbox(&mut focus_audio, FOCUS_AUDIO_LABEL).on_hover_text(FOCUS_AUDIO_HOVER);
            }
            slider_state = (slider.dragged(), slider.drag_stopped());
        })
        .response
        .on_hover_text("Volume da transmissão (separado do microfone)");
        if pref != before {
            self.set_user_pref(user, pref, !slider_state.0);
        }
        if slider_state.1 {
            self.save_settings();
        }
        if focus_audio != self.settings.stream_focus_audio {
            self.settings.stream_focus_audio = focus_audio;
            self.save_settings();
        }
    }

    /// Send the backend whatever the preview store wants asked right now.
    fn pump_previews(&mut self) {
        for r in self.previews.take_requests() {
            match r {
                previews::Request::Preview(url) => self.backend.send(Cmd::LinkPreview { url }),
                previews::Request::Image(url) => self.backend.send(Cmd::LinkPreviewImage { url }),
            }
        }
    }

    // ------------------------------------------------------------ dialogs

    pub fn open_settings(&mut self) {
        self.settings_ui.open = true;
        self.backend.send(Cmd::ListDevices);
        self.backend.send(Cmd::ListCameras);
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        static STATS: vox_log::RateLimiter = vox_log::RateLimiter::new();
        self.frames += 1;
        if STATS.ready(vox_log::STATS_INTERVAL) {
            tracing::info!(frames = self.frames, "ui stats (frames since last line)");
            self.frames = 0;
        }
        crate::repaint_watch::frame(ctx);
        self.pump_events();
        self.avatars.store.begin_frame(std::time::Instant::now());
        self.images.store.begin_frame(std::time::Instant::now());
        self.images_tick(ctx);
        self.sync_meter_watch();
        self.sync_cameras_visible(ctx);
        self.sound_tick();
        self.profile_tick(ctx);
        self.dm_tick(ctx);
        // Before `notify_tick`: leaving the app while hosting asks first (`host.rs`).
        self.host_tick(ctx);
        self.servers_tick(ctx);
        self.notify_tick(ctx);
        self.sync_streams_visible(ctx, false);
        self.pause_share_preview_when_minimized(ctx);
        self.donate_tick(ctx);
        self.auto_tick(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if self.video.parent.is_none() {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            if let Ok(RawWindowHandle::Win32(h)) = frame.window_handle().map(|h| h.as_raw()) {
                self.video.parent = Some(h.hwnd.get());
                // The concord logo at the sizes the title bar and the taskbar use (`brand.rs`).
                #[cfg(windows)]
                crate::brand::set_window_icons(h.hwnd.get());
                // Ctrl+V of pictures (egui only reports pastes of text).
                crate::clipboard::hook(h.hwnd.get());
                if vox_client_core::testmode::background() {
                    // Autotests: never activates, behind every other window (`testmode`).
                    vox_client_core::testmode::keep_behind(h.hwnd.get(), true);
                }
            }
        }
        self.video.want.clear();
        // Shortcuts: whether a text field has the focus; the window's own keys without the hook.
        self.keybinds_frame(ui.ctx());
        // macOS: the new video frames go into their textures before anything is drawn.
        #[cfg(target_os = "macos")]
        self.mac_video_upload(ui.ctx(), frame);
        if let Some(g) = self.kit.as_mut() {
            egui::CentralPanel::default_margins().show(ui, |ui| crate::ui_kit::gallery(ui, g));
            return;
        }
        match self.screen {
            Screen::Connect => self.connect_screen(ui),
            Screen::Main => self.main_screen(ui),
        }
        let ctx = ui.ctx().clone();
        self.golive_window(&ctx);
        self.settings_window(&ctx);
        self.camera_window(&ctx);
        self.host_windows(&ctx);
        self.tls_windows(&ctx);
        // The easter egg (`calladex.rs`): nothing at all while it is closed.
        if self.calladex.is_some() {
            self.calladex_window(&ctx);
        }
        if self.screen == Screen::Main {
            self.server_settings_window(&ctx);
            self.password_prompt_window(&ctx);
            self.claim_owner_window(&ctx);
            self.soundboard_windows(&ctx);
            self.profile_popout(&ctx);
            self.dm_windows(&ctx);
            self.person_menu_layer(&ctx);
            self.menu_confirm_window(&ctx);
            self.image_viewer(&ctx);
            // Last: it looks at what the others left open.
            self.donate_window(&ctx);
        }
        self.sync_video(&ctx);
        self.pump_avatars();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.keybinds_exit();
        self.donate_on_exit();
        self.notify_on_exit();
        self.drop_video();
        self.backend.shutdown();
        self.host_on_exit();
    }
}

// ---------------------------------------------------------------- helpers

pub fn encoder_label(backend: &str) -> String {
    match backend {
        "mf" => "GPU (Media Foundation)".into(),
        "nvenc" => "GPU (NVENC)".into(),
        "software" => "CPU".into(),
        other => other.to_string(),
    }
}

/// Insert keeping the list ordered by id; a message already there is replaced by the hub's newer
/// copy. Returns the old one when it was another message under the same id (an older hub reuses
/// a deleted message's id), so its cached images can go.
fn insert_message(msgs: &mut Vec<Message>, m: Message) -> Option<Message> {
    match msgs.binary_search_by_key(&m.id, |x| x.id) {
        Ok(i) => {
            let old = std::mem::replace(&mut msgs[i], m);
            (!same_message(&old, &msgs[i])).then_some(old)
        }
        Err(pos) => {
            msgs.insert(pos, m);
            None
        }
    }
}

/// The same message (as opposed to another one under a reused id): same author, same moment.
fn same_message(a: &Message, b: &Message) -> bool {
    a.author.id == b.author.id && a.created_at_ms == b.created_at_ms
}

pub(crate) fn name_color(id: UserId) -> Color32 {
    const PALETTE: [Color32; 8] = [
        Color32::from_rgb(235, 130, 130),
        Color32::from_rgb(216, 195, 165),
        Color32::from_rgb(130, 200, 120),
        Color32::from_rgb(90, 200, 200),
        Color32::from_rgb(120, 160, 245),
        Color32::from_rgb(180, 140, 240),
        Color32::from_rgb(240, 140, 200),
        Color32::from_rgb(200, 200, 120),
    ];
    PALETTE[(id.unsigned_abs() as usize) % PALETTE.len()]
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Minutes to add to UTC to get local time (current rule, DST included).
#[cfg(windows)]
pub(crate) fn local_offset_min() -> i32 {
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    // SAFETY: plain query into a zeroed out-struct.
    unsafe {
        let mut tz: TIME_ZONE_INFORMATION = std::mem::zeroed();
        let id = GetTimeZoneInformation(&mut tz);
        let bias = tz.Bias + if id == 2 { tz.DaylightBias } else { tz.StandardBias };
        -bias
    }
}

#[cfg(not(windows))]
pub(crate) fn local_offset_min() -> i32 {
    0
}

/// "HH:MM" for today, "dd/mm HH:MM" otherwise.
pub fn format_time(ms: i64, now_ms: i64, offset_min: i32) -> String {
    let local = ms / 1000 + offset_min as i64 * 60;
    let today = (now_ms / 1000 + offset_min as i64 * 60).div_euclid(86_400);
    let days = local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    if days == today {
        return format!("{h:02}:{m:02}");
    }
    // civil date from days since 1970-01-01 (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{d:02}/{mo:02} {h:02}:{m:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_formatting() {
        // 2026-10-07 12:34:00 UTC
        let t = 1_791_376_440_000i64;
        assert_eq!(format_time(t, t + 1000, 0), "12:34");
        assert_eq!(format_time(t, t + 1000, -180), "09:34");
        assert_eq!(format_time(t, t + 2 * 86_400_000, 0), "07/10 12:34");
    }

    #[test]
    fn messages_stay_sorted_and_unique() {
        let mk = |id| Message {
            id,
            channel_id: 1,
            author: User::new(1, "a"),
            content: String::new(),
            created_at_ms: id,
            attachments: vec![],
            mentions: vec![],
            mention_everyone: false,
            reply: None,
            reactions: vec![],
        };
        let mut v = Vec::new();
        for id in [5, 3, 9, 3, 5] {
            insert_message(&mut v, mk(id));
        }
        assert_eq!(v.iter().map(|m| m.id).collect::<Vec<_>>(), vec![3, 5, 9]);
    }

    #[test]
    fn camera_icon_uses_the_hub_flag_or_the_room_in_the_same_room() {
        // The hub says so: always, wherever the person is.
        assert!(camera_icon_on(true, false, false));
        assert!(camera_icon_on(true, true, false));
        // An old hub (no flag): the room's word counts only in our own room.
        assert!(camera_icon_on(false, true, true));
        assert!(!camera_icon_on(false, false, true));
        // Nobody says so.
        assert!(!camera_icon_on(false, true, false));
        assert!(!camera_icon_on(false, false, false));
    }

    #[test]
    fn audio_only_uses_the_hub_flag_or_the_room_in_the_same_room() {
        // The hub says so: everybody sees it, in the room or not (what the 🎵 outside the room is).
        assert!(audio_only_live_on(true, false, false));
        assert!(audio_only_live_on(true, true, false));
        // An old hub (no flag): the room's reading of the tracks counts only in our own room.
        assert!(audio_only_live_on(false, true, true));
        assert!(!audio_only_live_on(false, false, true));
        // Nobody says so: a screen live.
        assert!(!audio_only_live_on(false, true, false));
        assert!(!audio_only_live_on(false, false, false));
    }
}
