//! `%APPDATA%\concord\settings.json`. The server password is only stored when the user ticks
//! "Lembrar senha", and then only encrypted with Windows DPAPI (current user) or, on macOS, kept in
//! a user-only file, secrets.json (settings.json holds only a reference, see `filestore`). The saved server
//! list (v0.9.1) is `servers.json` next to it (see `servers.rs`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use vox_client_core::{GateConfig, NoiseLevel, UserAudio, VoiceFxConfig};

use crate::alerts::AlertSettings;

/// Go Live quality presets. The bitrates come from `docs/quality-bench.md` (hard synthetic
/// motion through the real hardware encoders): past these the quality gain per Mbps flattens out.
/// The host's upload carries one copy per viewer (the SFU forwards, it doesn't transcode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Preset {
    P720p30,
    #[default]
    P1080p30,
    P1080p60,
    /// "Máxima": 1080p60 with more bits, for fast games / 3D on a good connection.
    Max1080p60,
}

impl Preset {
    pub const ALL: [Preset; 4] = [Preset::P720p30, Preset::P1080p30, Preset::P1080p60, Preset::Max1080p60];

    pub fn label(self) -> &'static str {
        match self {
            Preset::P720p30 => "720p 30 fps",
            Preset::P1080p30 => "1080p 30 fps",
            Preset::P1080p60 => "1080p 60 fps",
            Preset::Max1080p60 => "Máxima (1080p 60 fps)",
        }
    }

    /// (width, height, fps)
    pub fn dims(self) -> (u32, u32, u32) {
        match self {
            Preset::P720p30 => (1280, 720, 30),
            Preset::P1080p30 => (1920, 1080, 30),
            Preset::P1080p60 | Preset::Max1080p60 => (1920, 1080, 60),
        }
    }

    /// Video bitrate cap (bps).
    pub fn bitrate(self) -> u64 {
        match self {
            Preset::P720p30 => 4_000_000,
            Preset::P1080p30 => 8_000_000,
            Preset::P1080p60 => 12_000_000,
            Preset::Max1080p60 => 16_000_000,
        }
    }
}

/// Webcam quality ("Qualidade da câmera"), all at 30 fps. The bitrate follows the size (about
/// 1.2 Mbps at 720p, see `vox_client_core::golive::camera::camera_bitrate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CameraPreset {
    P360,
    P540,
    #[default]
    P720,
    P1080,
}

impl CameraPreset {
    pub const ALL: [CameraPreset; 4] = [CameraPreset::P360, CameraPreset::P540, CameraPreset::P720, CameraPreset::P1080];

    pub fn label(self) -> &'static str {
        match self {
            CameraPreset::P360 => "360p (economiza internet)",
            CameraPreset::P540 => "540p",
            CameraPreset::P720 => "720p (recomendado)",
            CameraPreset::P1080 => "1080p",
        }
    }

    pub fn to_core(self) -> vox_client_core::golive::camera::CameraQuality {
        use vox_client_core::golive::camera::CameraQuality as Q;
        match self {
            CameraPreset::P360 => Q::P360,
            CameraPreset::P540 => Q::P540,
            CameraPreset::P720 => Q::P720,
            CameraPreset::P1080 => Q::P1080,
        }
    }
}

/// One person's local listening settings (what *I* hear), in percent. Only non-default entries
/// are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserAudioPref {
    /// Their microphone volume, 0..=200.
    pub volume_pct: u32,
    pub muted: bool,
    /// Their screenshare audio volume, 0..=200.
    pub stream_volume_pct: u32,
    pub stream_muted: bool,
}

impl Default for UserAudioPref {
    fn default() -> Self {
        Self { volume_pct: 100, muted: false, stream_volume_pct: 100, stream_muted: false }
    }
}

impl UserAudioPref {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn to_core(self) -> UserAudio {
        UserAudio {
            volume: self.volume_pct.min(200) as f32 / 100.0,
            muted: self.muted,
            stream_volume: self.stream_volume_pct.min(200) as f32 / 100.0,
            stream_muted: self.stream_muted,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub server: String,
    pub username: String,
    pub remember_password: bool,
    /// DPAPI-encrypted server password, hex encoded. Only present with `remember_password`.
    pub password_enc: Option<String>,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    /// Output volume in percent (0..=200).
    pub volume_pct: u32,
    pub preset: Preset,
    pub share_audio: bool,
    /// "Priorizar fluidez": keep the frame rate under congestion (default: drop fps first).
    pub share_smooth: bool,
    pub ptt_enabled: bool,
    /// Push-to-talk's key before v1.1, still written (from the first push-to-talk binding of
    /// `keybinds`) so an older app keeps its key; read once into `keybinds` at start
    /// (`keybinds::adopt_legacy_ptt`). 0 when that binding is a combination.
    pub ptt_vk: u32,
    /// The name shown for push-to-talk's key (the binding's label).
    pub ptt_name: String,
    /// "Atalhos de teclado" (v1.1): mute, deafen, push-to-talk, leave, Go Live, a sound. Older
    /// files have none (their push-to-talk key is adopted from `ptt_vk`); an entry this version
    /// does not understand is skipped, never the whole file.
    #[serde(default, deserialize_with = "crate::keybinds::de_list")]
    pub keybinds: Vec<crate::keybinds::Keybind>,
    /// Input sensitivity ("Sensibilidade de entrada"): automatic or manual voice-activity gate.
    pub input_gate: GateConfig,
    /// "Supressão de ruído": Padrão (WebRTC), IA leve (RNNoise, the default) or IA máxima
    /// (DeepFilterNet3).
    pub noise_suppression: NoiseLevel,
    /// "Modificador de voz": effect and intensity (older files get "Desligado").
    pub voice_fx: VoiceFxConfig,
    /// Local volume / mute per person, keyed by username (identities are per-session ids).
    pub user_audio: BTreeMap<String, UserAudioPref>,
    /// Alert sounds (only I hear them).
    pub alerts: AlertSettings,
    /// "Abrir transmissões em janela separada": watched streams open in their own window
    /// instead of inside the call view.
    pub stream_popout: bool,
    /// "Ouvir só as transmissões fixadas": with several watched streams, only the pinned ones (or
    /// the fullscreen one) play sound. Off by default: every watched stream is heard.
    pub stream_focus_audio: bool,
    /// "Mostrar prévia da minha transmissão" (item 8a): while I am live, my stream tile shows a
    /// small picture of what I share. Off tears the preview tap down completely (no second
    /// video processor, no staging textures). Older files get the default (on).
    pub show_self_preview: bool,
    /// "Prévia da minha transmissão ao passar o mouse" (v1.1): while I am live, a small still
    /// picture of what I share (and the app's name and icon) goes to the server for the hover
    /// cards. Off: nothing is sent, and a picture already there is dropped. Older files get the
    /// default (on, as in Discord).
    pub share_hover_preview: bool,
    /// Camera to use (`vox_capture::CameraInfo::id`); `None` = the first camera.
    pub camera_device: Option<String>,
    /// Its name, to find it again if the id changed (plugged into another USB port).
    pub camera_name: Option<String>,
    pub camera_preset: CameraPreset,
    /// Per camera (by device link, else name): native format, image controls, mirror / rotation /
    /// zoom (v0.6). Only cameras with non-default choices are stored.
    pub camera_profiles: crate::camera_cfg::CameraProfiles,
    /// "Aceleração de hardware" (v0.7): off = the processor encodes the screen share and the
    /// camera. Older files get on.
    pub hw_accel: bool,
    /// "Codificador": a specific encoder (`vox_capture::EncoderCandidate::id`, stable across
    /// reboots); `None` = "Automático (recomendado)".
    pub video_encoder: Option<String>,
    /// "Mostrar prévia de links": cards with title and thumbnail under chat links (the hub
    /// fetches them). Added with item 10, so older settings.json files get the default (on).
    pub show_link_previews: bool,
    /// v1.1 (E2): "Prévias de links nas conversas criptografadas": off by default (asking the hub
    /// for a card tells it which link was in an end-to-end encrypted DM).
    pub e2e_link_previews: bool,
    /// "Qualidade das imagens enviadas" (chat images): Normal (1920 px), Alta (2560) or Original
    /// (up to 4096). Older files get Normal.
    pub image_quality: vox_client_core::images::Quality,
    /// "Mostrar imagens automaticamente": chat images load by themselves (off: click to load).
    pub auto_show_images: bool,
    /// Item 12: per-device login tokens ("Lembrar este computador"), keyed by
    /// `server|username` (lowercase), each DPAPI-encrypted and hex encoded like the password.
    pub device_tokens: BTreeMap<String, String>,
    /// v1.0.1: the identity (`HealthResponse::server_id`) of the server that issued each login
    /// token above, same keys. A token is sent only to an address that answers with it (see
    /// `backend::Guard`); a token without one (saved before v1.0.1, or by an older hub) gets it
    /// at its next login. Not a secret.
    pub device_token_servers: BTreeMap<String, String>,
    /// v1.1 (T0): the TLS certificate pin (64 hex digits) of the server that issued each login
    /// token above, same keys. A token with one is sent only over TLS with that certificate
    /// (`backend::Guard::check_pin`). Not a secret.
    pub device_token_pins: BTreeMap<String, String>,
    /// Soundboard volume, mute and silenced sounds (added after v0.4.1: older files get the defaults).
    pub soundboard: crate::soundboard::SoundboardSettings,
    /// Profile settings of this computer (idle time before "Ausente"); older files get the defaults.
    pub profile: crate::profile::ProfileSettings,
    /// Windows notifications and the tray ("Notificações"); older files get the defaults.
    pub notify: crate::app::notify::NotifySettings,
    /// Reactions I used last, newest first (the picker's "Recentes"; v0.8).
    pub recent_reactions: Vec<String>,
    /// The v1.0 shell's remembered choices (center mode, docked chat width, member list).
    pub layout: crate::layout::LayoutSettings,
    /// "Apoie o concord" (v1.1, `donate.rs`): how many of the two popups this user got. Older
    /// files get 0 (the first comes at the next login).
    pub donate_popups_shown: u32,
    /// Seconds logged in to a server, tray included (for the second popup). Older files start at 0.
    pub usage_secs: u64,
    /// "Seus servidores salvos" (v0.9.1). Kept in `servers.json` next to this file, not in it: an
    /// older app rewrites settings.json without it (see `servers.rs`). `server`, `username`,
    /// `remember_password` and `password_enc` above mirror its most recently used entry.
    #[serde(skip)]
    pub servers: crate::servers::ServerList,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server: String::new(),
            username: String::new(),
            remember_password: false,
            password_enc: None,
            input_device: None,
            output_device: None,
            volume_pct: 100,
            preset: Preset::default(),
            share_audio: true,
            share_smooth: false,
            ptt_enabled: false,
            ptt_vk: 0,
            ptt_name: String::new(),
            keybinds: Vec::new(),
            input_gate: GateConfig::default(),
            noise_suppression: NoiseLevel::default(),
            voice_fx: VoiceFxConfig::default(),
            user_audio: BTreeMap::new(),
            alerts: AlertSettings::default(),
            stream_popout: false,
            stream_focus_audio: false,
            show_self_preview: true,
            share_hover_preview: true,
            camera_device: None,
            camera_name: None,
            camera_preset: CameraPreset::default(),
            camera_profiles: Default::default(),
            hw_accel: true,
            video_encoder: None,
            show_link_previews: true,
            e2e_link_previews: false,
            image_quality: Default::default(),
            auto_show_images: true,
            device_tokens: BTreeMap::new(),
            device_token_servers: BTreeMap::new(),
            device_token_pins: BTreeMap::new(),
            soundboard: Default::default(),
            profile: Default::default(),
            notify: Default::default(),
            recent_reactions: Vec::new(),
            layout: Default::default(),
            donate_popups_shown: 0,
            usage_secs: 0,
            servers: Default::default(),
        }
    }
}

/// Autotests (`--settings-dir`): settings.json and servers.json live in this folder.
static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Autotests only: keep the settings in `dir` (read at start, written on every change) instead of
/// `%APPDATA%\concord`.
pub fn set_dir(dir: PathBuf) {
    let _ = DIR.set(dir);
}

/// The settings live in an autotest's own folder.
pub fn dir_overridden() -> bool {
    DIR.get().is_some()
}

fn path() -> Option<PathBuf> {
    if let Some(d) = DIR.get() {
        return Some(d.join("settings.json"));
    }
    if let Some(d) = std::env::var_os("APPDATA") {
        return Some(PathBuf::from(d).join("concord").join("settings.json"));
    }
    // macOS: ~/Library/Application Support/concord (there is no %APPDATA%).
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return Some(PathBuf::from(home).join("Library").join("Application Support").join("concord").join("settings.json"));
    }
    None
}

/// Where settings.json is (the autotest reads the files back).
pub fn file_path() -> Option<PathBuf> {
    path()
}

/// The saved server list (v0.9.1), next to settings.json.
pub fn servers_path(settings: &Path) -> PathBuf {
    settings.with_file_name("servers.json")
}

/// v1.1 (E0): this computer's identity keys and the device keys it has seen (`e2ee_store.rs`),
/// next to settings.json.
pub fn e2ee_path() -> Option<PathBuf> {
    path().map(|p| p.with_file_name("e2ee.json"))
}

/// Write through a temporary file, so a crash never leaves half a file.
fn write_atomic(p: &Path, text: &str) -> Result<(), String> {
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, p).map_err(|e| e.to_string())
}

/// The video encoder self-test results (v0.7), next to settings.json.
pub fn encoder_cache_path() -> Option<PathBuf> {
    path().map(|p| p.with_file_name("encoders.json"))
}

impl Settings {
    pub fn load() -> Self {
        let Some(p) = path() else { return Self::default() };
        Self::load_at(&p)
    }

    /// settings.json at `p`, and the saved server list next to it. The single saved server of
    /// v0.9.0 (or of an older app run since) joins the list (`servers.rs`).
    pub fn load_at(p: &Path) -> Self {
        let mut s: Settings = match std::fs::read_to_string(p) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                warn!(error = %e, "settings.json is invalid; using defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        };
        let list = std::fs::read_to_string(servers_path(p)).ok();
        s.servers = list.as_deref().and_then(crate::servers::ServerList::from_json).unwrap_or_default();
        if list.is_some() && s.servers.servers.is_empty() && s.servers.next_id == 0 {
            warn!("servers.json is invalid; starting a new list");
        }
        if s.merge_legacy_server(crate::app::now_ms()) {
            info!(entries = s.servers.servers.len(), "server list: the saved server of an older version joined the list");
        }
        s
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        self.save_at(&p);
    }

    /// settings.json at `p`, and servers.json next to it.
    pub fn save_at(&self, p: &Path) {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let res = serde_json::to_string_pretty(self).map_err(|e| e.to_string()).and_then(|text| write_atomic(p, &text));
        if let Err(e) = res {
            warn!(error = %e, "cannot save settings");
        }
        if let Err(e) = write_atomic(&servers_path(p), &self.servers.to_json()) {
            warn!(error = %e, "cannot save the server list");
        }
    }

    /// The remembered server password, if any (decrypted).
    pub fn saved_password(&self) -> Option<String> {
        if !self.remember_password {
            return None;
        }
        unprotect(self.password_enc.as_deref()?)
    }

    fn device_key(server: &str, username: &str) -> String {
        format!("{}|{}", server.trim().to_ascii_lowercase(), username.trim().to_ascii_lowercase())
    }

    /// This computer's login token for `username` on `server` (item 12), decrypted.
    pub fn device_token(&self, server: &str, username: &str) -> Option<String> {
        unprotect(self.device_tokens.get(&Self::device_key(server, username))?)
    }

    /// Remember (DPAPI-encrypted) or forget this computer's login token for a name. A new token
    /// starts without a server identity ([`Settings::set_device_token_server`]).
    pub fn set_device_token(&mut self, server: &str, username: &str, token: Option<&str>) {
        let key = Self::device_key(server, username);
        self.device_token_servers.remove(&key);
        self.device_token_pins.remove(&key);
        match token.and_then(protect) {
            Some(enc) => {
                self.device_tokens.insert(key, enc);
            }
            None => {
                self.device_tokens.remove(&key);
            }
        }
    }

    /// v1.0.1: the identity of the server that issued this computer's login token for `username`
    /// on `server`, when known.
    pub fn device_token_server(&self, server: &str, username: &str) -> Option<String> {
        self.device_token_servers.get(&Self::device_key(server, username)).filter(|id| vox_proto::is_server_id(id)).cloned()
    }

    /// v1.1 (T0): the certificate of the server that issued this computer's login token for
    /// `username` on `server`, when known.
    pub fn device_token_pin(&self, server: &str, username: &str) -> Option<[u8; 32]> {
        vox_proto::tls_pin_from_hex(self.device_token_pins.get(&Self::device_key(server, username))?)
    }

    /// v1.1 (T0): tie the login token saved for `username` on `server` (if there is one) to a
    /// certificate (`None`: unknown).
    pub fn set_device_token_pin(&mut self, server: &str, username: &str, pin: Option<[u8; 32]>) {
        let key = Self::device_key(server, username);
        match pin.filter(|_| self.device_tokens.contains_key(&key)) {
            Some(pin) => {
                self.device_token_pins.insert(key, vox_proto::tls_pin_hex(&pin));
            }
            None => {
                self.device_token_pins.remove(&key);
            }
        }
    }

    /// v1.0.1: tie the login token saved for `username` on `server` (if there is one) to a server
    /// identity (`None`: unknown).
    pub fn set_device_token_server(&mut self, server: &str, username: &str, id: Option<&str>) {
        let key = Self::device_key(server, username);
        match id.filter(|id| vox_proto::is_server_id(id) && self.device_tokens.contains_key(&key)) {
            Some(id) => {
                self.device_token_servers.insert(key, id.to_string());
            }
            None => {
                self.device_token_servers.remove(&key);
            }
        }
    }
    /// The video encoder choice for vox-client-core (Settings → Vídeo).
    pub fn encoder_pref(&self) -> vox_client_core::golive::encoders::EncoderPref {
        vox_client_core::golive::encoders::EncoderPref { hardware: self.hw_accel, encoder: self.video_encoder.clone() }
    }

    /// Saved local audio settings of `username` (defaults when none).
    pub fn user_audio(&self, username: &str) -> UserAudioPref {
        self.user_audio.get(username).copied().unwrap_or_default()
    }

    /// Remember `username`'s local audio settings; default values are not stored.
    pub fn set_user_audio(&mut self, username: &str, mut pref: UserAudioPref) {
        pref.volume_pct = pref.volume_pct.min(200);
        pref.stream_volume_pct = pref.stream_volume_pct.min(200);
        if pref.is_default() {
            self.user_audio.remove(username);
        } else {
            self.user_audio.insert(username.to_string(), pref);
        }
    }

}

// ---------------------------------------------------------------- DPAPI

/// Kept from before the rename to concord (v1.0.1): the passwords and tokens already saved were
/// encrypted with it, and the migrated settings.json must still decrypt.
#[cfg_attr(not(windows), allow(dead_code))]
const ENTROPY: &[u8] = b"voxhub-settings-v1";

#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg_attr(not(windows), allow(dead_code))]
fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(windows)]
pub(crate) fn protect(plain: &str) -> Option<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};
    let input = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr() as *mut u8,
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: ENTROPY.len() as u32,
        pbData: ENTROPY.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: the blobs point at live buffers for the duration of the call; the output buffer is
    // allocated by DPAPI and released with LocalFree after copying it out.
    unsafe {
        if CryptProtectData(&input, std::ptr::null(), &entropy, std::ptr::null(), std::ptr::null(), 0, &mut out) == 0 {
            return None;
        }
        let hex = to_hex(std::slice::from_raw_parts(out.pbData, out.cbData as usize));
        LocalFree(out.pbData as *mut core::ffi::c_void);
        Some(hex)
    }
}

#[cfg(windows)]
pub(crate) fn unprotect(hex: &str) -> Option<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};
    let mut bytes = from_hex(hex)?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_mut_ptr(),
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: ENTROPY.len() as u32,
        pbData: ENTROPY.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: as in `protect`.
    unsafe {
        if CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut out,
        ) == 0
        {
            return None;
        }
        let plain = String::from_utf8(std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec()).ok();
        LocalFree(out.pbData as *mut core::ffi::c_void);
        plain
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn protect(_plain: &str) -> Option<String> {
    None
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn unprotect(_hex: &str) -> Option<String> {
    None
}

// ---------------------------------------------------------------- macOS: a user-only file

/// macOS: the secret goes to secrets.json next to settings.json (readable only by this user);
/// settings.json keeps only an opaque reference. See `filestore`.
#[cfg(target_os = "macos")]
pub(crate) fn protect(plain: &str) -> Option<String> {
    filestore::protect_in(filestore::Vault::Secrets, plain)
}

#[cfg(target_os = "macos")]
pub(crate) fn unprotect(reference: &str) -> Option<String> {
    filestore::unprotect(reference)
}

/// v1.1 (E0): this computer's end-to-end identity keys (e2ee.json). Same protection as
/// [`protect`] on Windows; on macOS their own map in secrets.json (`kck:` references).
#[cfg(not(target_os = "macos"))]
pub(crate) fn protect_key(plain: &str) -> Option<String> {
    protect(plain)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn unprotect_key(stored: &str) -> Option<String> {
    unprotect(stored)
}

#[cfg(target_os = "macos")]
pub(crate) fn protect_key(plain: &str) -> Option<String> {
    filestore::protect_in(filestore::Vault::Keys, plain)
}

#[cfg(target_os = "macos")]
pub(crate) fn unprotect_key(reference: &str) -> Option<String> {
    filestore::unprotect(reference)
}

/// Saved passwords, login tokens and the end-to-end identity keys on macOS (v1.1.0).
///
/// - Everything lives in `secrets.json` next to settings.json (~/Library/Application
///   Support/concord), mode 0600 (this user only), always replaced atomically (a 0600 temp file,
///   then a rename). settings.json, servers.json and e2ee.json hold only `kc1:<id>` (passwords and
///   tokens) or `kck:<id>` (identity keys): 32 random hex digits, no secret.
/// - The Keychain is no longer used: the app is signed ad hoc, so every update made macOS ask for
///   the Keychain password again. The prefixes are the ones the Keychain versions wrote, so their
///   references keep working once migrated.
/// - Migration, once: when secrets.json has not been migrated yet and a settings file mentions a
///   `kc1:`/`kck:` id it lacks, the old Keychain item of that kind (`secrets` / `device-keys`) is
///   read ONE time and the mentioned entries are copied here. A read that fails or is refused is
///   not retried: that saved password is simply gone (typed again once) and the identity key goes
///   the existing "chave nova" way (`e2ee_store::UNREADABLE_KEY_TEXT`). The Keychain items are left
///   as they are (deleting them could ask again; an older concord still finds them).
/// - [`secrets_gc`] (at startup) drops the ids no file mentions any more.
/// - Autotests with their own settings folder get their own secrets.json there. Unit tests keep
///   the map in memory only (the file functions have their own tests) and never read the Keychain.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod filestore {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard};

    use serde::{Deserialize, Serialize};

    /// Which map a secret lives in.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Vault {
        /// Server passwords and login tokens (`kc1:`).
        Secrets,
        /// v1.1: the end-to-end identity keys (`kck:`).
        Keys,
    }

    impl Vault {
        pub(super) const ALL: [Vault; 2] = [Vault::Secrets, Vault::Keys];

        pub(super) fn prefix(self) -> &'static str {
            match self {
                Vault::Secrets => "kc1:",
                Vault::Keys => "kck:",
            }
        }

        /// The Keychain account the Keychain versions used (read by the migration only).
        #[cfg_attr(not(all(target_os = "macos", not(test))), allow(dead_code))]
        fn keychain_account(self) -> &'static str {
            match self {
                Vault::Secrets => "secrets",
                Vault::Keys => "device-keys",
            }
        }

        /// The vault and id of a reference (`None`: not one of ours).
        pub(super) fn of(reference: &str) -> Option<(Vault, String)> {
            Vault::ALL.into_iter().find_map(|v| {
                let id = reference.strip_prefix(v.prefix())?;
                (id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit())).then(|| (v, id.to_ascii_lowercase()))
            })
        }
    }

    /// secrets.json.
    #[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
    pub(super) struct Data {
        #[serde(default)]
        pub(super) secrets: HashMap<String, String>,
        #[serde(default)]
        pub(super) keys: HashMap<String, String>,
        /// The one-time copy from the Keychain was done (or tried): never read it again.
        #[serde(default)]
        pub(super) keychain_migrated: bool,
    }

    impl Data {
        pub(super) fn map(&mut self, vault: Vault) -> &mut HashMap<String, String> {
            match vault {
                Vault::Secrets => &mut self.secrets,
                Vault::Keys => &mut self.keys,
            }
        }
    }

    /// secrets.json as last read or written (`None`: not read yet).
    static CACHE: Mutex<Option<Data>> = Mutex::new(None);

    /// A reference id: 16 random bytes, hex.
    fn new_id() -> Option<String> {
        let mut b = [0u8; 16];
        #[cfg(unix)]
        {
            use std::io::Read;
            std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).ok()?;
        }
        // Only the unit tests get here (Windows keeps DPAPI): distinct ids are enough.
        #[cfg(not(unix))]
        for c in b.chunks_mut(8) {
            use std::hash::{BuildHasher, Hasher};
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u64(0);
            c.copy_from_slice(&h.finish().to_le_bytes());
        }
        Some(super::to_hex(&b))
    }

    #[cfg(test)]
    pub(super) fn is_reference(s: &str) -> bool {
        Vault::of(s).is_some()
    }

    /// The file (`None` in unit tests: memory only).
    #[cfg(not(test))]
    fn file() -> Option<PathBuf> {
        super::path().map(|p| p.with_file_name("secrets.json"))
    }

    #[cfg(test)]
    fn file() -> Option<PathBuf> {
        None
    }

    /// Read secrets.json (`Ok(None)`: there is none yet). Group/other permissions are taken away.
    pub(super) fn read_file(p: &Path) -> Result<Option<Data>, String> {
        let bytes = match std::fs::read(p) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(m) = std::fs::metadata(p) {
                if m.permissions().mode() & 0o077 != 0 {
                    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
                }
            }
        }
        serde_json::from_slice(&bytes).map(Some).map_err(|e| format!("damaged file: {e}"))
    }

    /// Replace secrets.json atomically: a new 0600 file next to it, then a rename over it.
    pub(super) fn write_file(p: &Path, data: &Data) -> Result<(), String> {
        use std::io::Write;
        let dir = p.parent().ok_or("no folder")?;
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let json = serde_json::to_vec(data).map_err(|e| e.to_string())?;
        let tmp = dir.join(format!(".secrets.json.{}.tmp", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let done = o
            .open(&tmp)
            .and_then(|mut f| {
                f.write_all(&json)?;
                f.sync_all()
            })
            .and_then(|_| std::fs::rename(&tmp, p));
        if done.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        done.map_err(|e| e.to_string())
    }

    /// The one-time copy from the Keychain: for each vault whose ids `text` (the settings files)
    /// mentions but `data` lacks, `read` that Keychain item once and keep the mentioned entries.
    /// Returns whether the Keychain was asked at all (then `data` must be written).
    pub(super) fn migrate(data: &mut Data, text: &str, read: impl Fn(Vault) -> Result<Option<HashMap<String, String>>, String>) -> bool {
        let mut asked = false;
        for vault in Vault::ALL {
            let prefix = vault.prefix();
            let map = data.map(vault);
            let wanted: Vec<String> = text
                .match_indices(prefix)
                .filter_map(|(i, _)| text.get(i..i + prefix.len() + 32))
                .filter_map(|r| Vault::of(r).map(|(_, id)| id))
                .filter(|id| !map.contains_key(id))
                .collect();
            if wanted.is_empty() {
                continue;
            }
            asked = true;
            match read(vault) {
                Ok(Some(old)) => {
                    let mut copied = 0;
                    for id in &wanted {
                        if let Some(v) = old.get(id) {
                            map.insert(id.clone(), v.clone());
                            copied += 1;
                        }
                    }
                    tracing::info!(?vault, copied, missing = wanted.len() - copied, "secrets: moved out of the Keychain into secrets.json");
                }
                Ok(None) => tracing::info!(?vault, "secrets: no Keychain item to move"),
                // Refused or unreadable: never asked again; those saved secrets are gone.
                Err(e) => tracing::warn!(error = %e, ?vault, "secrets: the old Keychain item cannot be read; those saved secrets are dropped"),
            }
        }
        data.keychain_migrated = true;
        asked
    }

    /// The old Keychain item of `vault` (migration only).
    #[cfg(all(target_os = "macos", not(test)))]
    fn keychain_read(vault: Vault) -> Result<Option<HashMap<String, String>>, String> {
        /// errSecItemNotFound
        const NOT_FOUND: i32 = -25300;
        let account = match super::DIR.get() {
            // Autotests of the Keychain versions used an account per settings folder.
            Some(d) => {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                d.hash(&mut h);
                format!("{}-test-{:016x}", vault.keychain_account(), h.finish())
            }
            None => vault.keychain_account().to_string(),
        };
        match security_framework::passwords::get_generic_password("concord", &account) {
            Ok(b) => serde_json::from_slice(&b).map(Some).map_err(|e| format!("damaged item: {e}")),
            Err(e) if e.code() == NOT_FOUND => Ok(None),
            Err(e) => Err(format!("{e} ({})", e.code())),
        }
    }

    // Unit tests and other systems never read a Keychain.
    #[cfg(not(all(target_os = "macos", not(test))))]
    fn keychain_read(_vault: Vault) -> Result<Option<HashMap<String, String>>, String> {
        Ok(None)
    }

    fn persist(data: &Data) -> Result<(), String> {
        match file() {
            Some(p) => write_file(&p, data),
            None if cfg!(test) => Ok(()),
            None => Err("no settings folder".into()),
        }
    }

    /// The cache, read from secrets.json on first use (and migrated, once) (`None`: unreadable).
    fn loaded() -> Option<MutexGuard<'static, Option<Data>>> {
        let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
        if cache.is_none() {
            let mut data = match file().map(|p| read_file(&p)) {
                Some(Ok(d)) => d.unwrap_or_default(),
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "secrets: cannot read secrets.json");
                    return None;
                }
                None => Data::default(),
            };
            if !data.keychain_migrated && file().is_some() {
                if let Some(text) = super::path().and_then(|p| super::secret_files_text(&p)) {
                    if migrate(&mut data, &text, keychain_read) {
                        if let Err(e) = persist(&data) {
                            tracing::warn!(error = %e, "secrets: cannot write secrets.json");
                        }
                    }
                }
            }
            *cache = Some(data);
        }
        Some(cache)
    }

    pub(crate) fn protect_in(vault: Vault, plain: &str) -> Option<String> {
        let mut cache = loaded()?;
        let data = cache.as_mut()?;
        let prefix = vault.prefix();
        let found = data.map(vault).iter().find(|(_, v)| v.as_str() == plain).map(|(k, _)| k.clone());
        if let Some(id) = found {
            return Some(format!("{prefix}{id}"));
        }
        let id = new_id()?;
        data.map(vault).insert(id.clone(), plain.to_string());
        match persist(data) {
            Ok(()) => Some(format!("{prefix}{id}")),
            Err(e) => {
                data.map(vault).remove(&id);
                tracing::warn!(error = %e, ?vault, "secrets: cannot save a secret");
                None
            }
        }
    }

    /// A reference of either vault.
    pub(super) fn unprotect(reference: &str) -> Option<String> {
        let (vault, id) = Vault::of(reference)?;
        loaded()?.as_mut()?.map(vault).get(&id).cloned()
    }

    /// Drop the entries whose reference (with the vault's prefix) is not in `text`; returns how
    /// many went.
    pub(super) fn keep_referenced_in(vault: Vault, map: &mut HashMap<String, String>, text: &str) -> usize {
        let before = map.len();
        let prefix = vault.prefix();
        map.retain(|id, _| text.contains(&format!("{prefix}{id}")));
        before - map.len()
    }

    /// Forget every secret whose reference does not appear in `text` (the settings files).
    pub(super) fn gc(text: &str) {
        let Some(mut cache) = loaded() else { return };
        let Some(data) = cache.as_mut() else { return };
        let dropped: usize = Vault::ALL.into_iter().map(|v| keep_referenced_in(v, data.map(v), text)).sum();
        if dropped == 0 {
            return;
        }
        match persist(data) {
            Ok(()) => tracing::info!(dropped, "secrets: unused saved secrets removed"),
            Err(e) => tracing::warn!(error = %e, "secrets: cannot clean up"),
        }
    }
}

/// macOS, at startup: move the Keychain secrets out once (see `filestore`) and drop the saved
/// secrets that neither settings.json, servers.json nor e2ee.json (the identity keys) mention any
/// more (a password saved again gets a new reference). An unreadable file: nothing is dropped.
#[cfg(target_os = "macos")]
pub fn secrets_gc() {
    let Some(p) = path() else { return };
    let Some(text) = secret_files_text(&p) else { return };
    filestore::gc(&text);
}

/// Every file that may hold a `kc1:`/`kck:` reference, next to settings.json at `settings` (what
/// [`secrets_gc`] keeps). A missing file counts as empty; `None` when one cannot be read.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn secret_files_text(settings: &Path) -> Option<String> {
    let read = |p: &Path| match std::fs::read_to_string(p) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(String::new()),
        Err(_) => None,
    };
    let mut text = String::new();
    let e2ee = settings.with_file_name("e2ee.json");
    for f in [settings.to_path_buf(), servers_path(settings), crate::e2ee_store::bad_path(&e2ee), e2ee] {
        text.push_str(&read(&f)?);
        text.push('\n');
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_files_start_the_donation_popups_and_the_usage_counter_at_zero() {
        // A file written before v1.1's donation popups.
        let s: Settings = serde_json::from_str(r#"{"server":"http://x","volume_pct":80}"#).unwrap();
        assert_eq!((s.donate_popups_shown, s.usage_secs, s.volume_pct), (0, 0, 80));
        let back: Settings = serde_json::from_str(&serde_json::to_string(&Settings { donate_popups_shown: 2, usage_secs: 7, ..s }).unwrap()).unwrap();
        assert_eq!((back.donate_popups_shown, back.usage_secs), (2, 7));
    }

    #[test]
    fn the_preview_of_my_own_live_is_on_by_default_and_old_files_get_that() {
        assert!(Settings::default().show_self_preview);
        // A file written before the switch existed.
        let old: Settings = serde_json::from_str(r#"{"server":"h","username":"ana"}"#).unwrap();
        assert!(old.show_self_preview);
        // Off survives the file.
        let off = Settings { show_self_preview: false, ..Default::default() };
        let back: Settings = serde_json::from_str(&serde_json::to_string(&off).unwrap()).unwrap();
        assert!(!back.show_self_preview);
    }

    /// v1.1 shortcuts: an old file (push-to-talk key only) loads with no bindings and its key in
    /// `ptt_vk`; the bindings survive the file; an older app's reader ignores the new field.
    #[test]
    fn shortcuts_load_from_old_files_and_survive_the_file() {
        use crate::keybinds::{Action, Combo, Keybind, MOD_CTRL};
        let old: Settings = serde_json::from_str(r#"{"server":"h","ptt_enabled":true,"ptt_vk":86,"ptt_name":"V"}"#).unwrap();
        assert!(old.keybinds.is_empty());
        assert_eq!(old.ptt_vk, 86);
        let mut s = Settings { ptt_enabled: true, ..Default::default() };
        s.keybinds.push(Keybind { action: Action::ToggleMute, key: Combo::new(0x4D, MOD_CTRL), key_name: "M".into() });
        s.keybinds.push(Keybind { action: Action::PushToTalk, key: Combo::new(0x56, 0), key_name: "V".into() });
        let text = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(back.keybinds, s.keybinds);
        // What an app without the field sees: the same file, read by a struct that does not know it.
        #[derive(serde::Deserialize)]
        struct OldApp {
            ptt_enabled: bool,
        }
        assert!(serde_json::from_str::<OldApp>(&text).unwrap().ptt_enabled);
        // A broken shortcut list does not cost the rest of the settings.
        let bad: Settings = serde_json::from_str(r#"{"username":"ana","keybinds":[{"action":{"kind":"from_the_future"}}]}"#).unwrap();
        assert_eq!(bad.username, "ana");
        assert!(bad.keybinds.is_empty());
    }

    /// The secret store of macOS (secrets.json): opaque references, the same secret keeps its
    /// reference, two vaults, and the startup clean-up keeps exactly what the files mention. Runs
    /// on every system (memory only in unit tests; the file functions are tested below).
    #[test]
    fn secret_store_references_and_clean_up() {
        let p = |v: filestore::Vault, s: &str| filestore::protect_in(v, s).unwrap();
        let (sec, keys) = (filestore::Vault::Secrets, filestore::Vault::Keys);
        let r = p(sec, "senha do Mac");
        assert!(filestore::is_reference(&r) && r.starts_with("kc1:") && !r.contains("senha"), "{r}");
        assert_eq!(p(sec, "senha do Mac"), r, "the same secret keeps its reference");
        assert_ne!(p(sec, "outra senha"), r);
        assert_eq!(filestore::unprotect(&r).as_deref(), Some("senha do Mac"));
        assert_eq!(filestore::unprotect("kc1:00000000000000000000000000000000"), None);
        assert_eq!(filestore::unprotect("01000000d08c9ddf"), None, "a Windows DPAPI value means nothing here");
        let k = p(keys, "chave do aparelho");
        assert!(k.starts_with("kck:") && !k.contains("chave"), "{k}");
        assert_eq!(filestore::unprotect(&k).as_deref(), Some("chave do aparelho"));
        assert_ne!(p(keys, "senha do Mac"), r, "same secret, other vault: another reference");
        let mut m: std::collections::HashMap<String, String> = [("a".repeat(32), "1".into()), ("b".repeat(32), "2".into())].into_iter().collect();
        let text = format!("{{\"password_enc\":\"kc1:{}\"}}", "a".repeat(32));
        assert_eq!(filestore::keep_referenced_in(sec, &mut m, &text), 1);
        assert_eq!(m.keys().cloned().collect::<Vec<_>>(), vec!["a".repeat(32)]);
        // A kc1: mention does not keep a kck: key of the same id.
        let mut m: std::collections::HashMap<String, String> = [("a".repeat(32), "x".into())].into_iter().collect();
        assert_eq!(filestore::keep_referenced_in(keys, &mut m, &text), 1);
    }

    /// v1.1.0 (macOS): secrets.json is written atomically, readable only by this user, and read
    /// back; the one-time Keychain migration copies only the mentioned entries, is never retried,
    /// and a refused read just drops them.
    #[test]
    fn secret_file_and_the_one_time_keychain_migration() {
        use filestore::{migrate, read_file, write_file, Data, Vault};
        let dir = std::env::temp_dir().join(format!("concord-secrets-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let f = dir.join("secrets.json");
        assert_eq!(read_file(&f).unwrap(), None, "no file yet");
        let mut d = Data::default();
        d.secrets.insert("a".repeat(32), "senha".into());
        d.keys.insert("b".repeat(32), "chave".into());
        write_file(&f, &d).unwrap();
        write_file(&f, &d).unwrap(); // replacing it works too
        assert_eq!(read_file(&f).unwrap(), Some(d.clone()));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no temp file left behind");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
            read_file(&f).unwrap();
            assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600, "made user-only again");
        }
        std::fs::write(&f, b"{\"secrets\":").unwrap();
        assert!(read_file(&f).is_err(), "a damaged file is an error, not an empty store");
        let _ = std::fs::remove_dir_all(&dir);

        // Migration: settings mention two passwords and one key; the Keychain has them plus junk.
        let (pa, pb, kc, junk) = ("a".repeat(32), "b".repeat(32), "c".repeat(32), "d".repeat(32));
        let text = format!(r#"{{"password_enc":"kc1:{pa}"}} {{"servers":[{{"password_enc":"kc1:{}"}}]}} {{"secret":"kck:{kc}"}}"#, pb.to_uppercase());
        let keychain = |v: Vault| -> Result<Option<std::collections::HashMap<String, String>>, String> {
            Ok(Some(match v {
                Vault::Secrets => [(pa.clone(), "s1".to_string()), (pb.clone(), "s2".into()), (junk.clone(), "x".into())].into_iter().collect(),
                Vault::Keys => [(kc.clone(), "k".to_string())].into_iter().collect(),
            }))
        };
        let mut d = Data::default();
        assert!(migrate(&mut d, &text, keychain));
        assert!(d.keychain_migrated);
        assert_eq!((d.secrets.len(), d.secrets.get(&pb).map(String::as_str)), (2, Some("s2")), "only what the files mention");
        assert_eq!(d.keys.get(&kc).map(String::as_str), Some("k"));
        // Nothing missing: the Keychain is not asked at all.
        let never = |_: Vault| -> Result<Option<std::collections::HashMap<String, String>>, String> { panic!("the Keychain was read again") };
        assert!(!migrate(&mut d, &text, never));
        // A refused read: nothing copied, no error for the caller, marked done (not asked again).
        let mut d = Data::default();
        assert!(migrate(&mut d, &text, |_| Err("refused (-128)".into())));
        assert!(d.keychain_migrated && d.secrets.is_empty() && d.keys.is_empty());
        // No reference at all (a new install): the Keychain is never asked.
        let mut d = Data::default();
        assert!(!migrate(&mut d, "{\"username\":\"ana\"}", never));
    }

    /// v1.1 (E0): the startup clean-up reads e2ee.json too, or every Mac would lose its identity
    /// key at the next start (its reference is only in that file).
    #[test]
    fn secrets_gc_keeps_the_identity_keys_of_e2ee_json() {
        let dir = std::env::temp_dir().join(format!("concord-gc-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let settings = dir.join("settings.json");
        let (pw, srv) = (format!("kc1:{}", "a".repeat(32)), format!("kc1:{}", "b".repeat(32)));
        std::fs::write(&settings, format!(r#"{{"password_enc":"{pw}"}}"#)).unwrap();
        std::fs::write(servers_path(&settings), format!(r#"{{"servers":[{{"password_enc":"{srv}"}}]}}"#)).unwrap();
        // The identity's reference is only in e2ee.json (written by the store, as in the app).
        fn store_like(_: &str) -> Option<String> {
            Some(format!("kck:{}", "c".repeat(32)))
        }
        let key = store_like("").unwrap();
        assert!(!secret_files_text(&settings).unwrap().contains(&key));
        let mut store = crate::e2ee_store::Store::at(dir.join("e2ee.json"), store_like, |_| None);
        let _id = store.identity("00112233445566778899aabbccddeeff", 3, 1).unwrap();
        assert_eq!(Some(dir.join("e2ee.json")), Some(settings.with_file_name("e2ee.json")));
        let text = secret_files_text(&settings).unwrap();
        for r in [&pw, &srv, &key] {
            assert!(text.contains(r.as_str()), "{r} must be kept");
        }
        // What the clean-up does with it (the maps of saved secrets, by id).
        let mut m: std::collections::HashMap<String, String> = ["a", "b", "c", "d"].iter().map(|c| (c.repeat(32), "x".to_string())).collect();
        assert_eq!(filestore::keep_referenced_in(filestore::Vault::Secrets, &mut m, &text), 2, "the key and the unreferenced one are not passwords");
        let mut k: std::collections::HashMap<String, String> = ["c", "d"].iter().map(|c| (c.repeat(32), "x".to_string())).collect();
        assert_eq!(filestore::keep_referenced_in(filestore::Vault::Keys, &mut k, &text), 1);
        assert!(k.contains_key(&"c".repeat(32)), "the identity key stays");
        // Without e2ee.json (no identity yet) the rest is still read.
        std::fs::remove_file(dir.join("e2ee.json")).unwrap();
        assert!(secret_files_text(&settings).unwrap().contains(&pw));
        // A damaged e2ee.json kept aside (e2ee_store::bad_path): its references stay too.
        let old = format!("kck:{}", "e".repeat(32));
        std::fs::write(crate::e2ee_store::bad_path(&dir.join("e2ee.json")), format!("{{\"identities\":[{{\"secret\":\"{old}\"")).unwrap();
        assert!(secret_files_text(&settings).unwrap().contains(&old));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn password_roundtrips_and_is_not_plain() {
        let enc = protect("s3nha secreta").expect("dpapi");
        assert!(!enc.contains("s3nha"));
        assert_eq!(unprotect(&enc).as_deref(), Some("s3nha secreta"));
        assert_eq!(unprotect("zz"), None);
    }

    #[test]
    fn settings_roundtrip_through_the_file_without_plaintext_password() {
        let dir = std::env::temp_dir().join(format!("concord-settings-test-{}", std::process::id()));
        // Only this test touches APPDATA.
        std::env::set_var("APPDATA", &dir);
        let s = Settings {
            server: "http://h:7890".into(),
            username: "ana".into(),
            remember_password: true,
            password_enc: protect("segredo-123"),
            ..Default::default()
        };
        s.save();
        let raw = std::fs::read_to_string(dir.join("concord").join("settings.json")).unwrap();
        assert!(!raw.contains("segredo-123"));
        let back = Settings::load();
        assert_eq!(back.username, "ana");
        assert_eq!(back.saved_password().as_deref(), Some("segredo-123"));
        // The single saved server became the list's entry, and its file has no plain password.
        assert_eq!(back.servers.servers.len(), 1);
        let list = std::fs::read_to_string(dir.join("concord").join("servers.json")).unwrap();
        assert!(!list.contains("segredo-123"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn defaults_when_fields_are_missing() {
        let s: Settings = serde_json::from_str(r#"{"server":"http://x:7890"}"#).unwrap();
        assert_eq!(s.server, "http://x:7890");
        assert_eq!(s.volume_pct, 100);
        assert!(s.share_audio);
        assert!(s.share_hover_preview, "hover previews of my live are on in older files");
        assert!(!serde_json::from_str::<Settings>(r#"{"share_hover_preview":false}"#).unwrap().share_hover_preview);
        assert!(!s.stream_popout, "streams open inside the app by default");
        let s: Settings = serde_json::from_str(r#"{"stream_popout":true}"#).unwrap();
        assert!(s.stream_popout);
    }
    #[test]
    fn user_audio_is_kept_per_username_and_defaults_are_not_stored() {
        let mut s = Settings::default();
        assert!(s.user_audio("ana").is_default());
        let pref = UserAudioPref { volume_pct: 150, muted: true, stream_volume_pct: 40, stream_muted: false };
        s.set_user_audio("ana", pref);
        s.set_user_audio("bob", UserAudioPref { volume_pct: 30, ..Default::default() });
        assert_eq!(s.user_audio("ana"), pref);
        assert_eq!(s.user_audio("bob").volume_pct, 30);
        assert!(s.user_audio("cat").is_default());
        // Survives a save/load cycle through JSON, keyed by name.
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"ana\""));
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.user_audio("ana"), pref);
        assert_eq!(back.user_audio("bob").volume_pct, 30);
        // Back to defaults removes the entry; huge values are clamped to 200 %.
        s.set_user_audio("ana", UserAudioPref::default());
        assert!(!s.user_audio.contains_key("ana"));
        s.set_user_audio("bob", UserAudioPref { volume_pct: 9000, stream_volume_pct: 201, ..Default::default() });
        assert_eq!((s.user_audio("bob").volume_pct, s.user_audio("bob").stream_volume_pct), (200, 200));
    }

    #[test]
    fn alert_settings_default_on_except_presence_and_roundtrip() {
        let s: Settings = serde_json::from_str(r#"{"server":"x"}"#).unwrap();
        assert_eq!(s.alerts, AlertSettings::default());
        assert!(s.alerts.enabled && s.alerts.room && s.alerts.streams && s.alerts.own_state && !s.alerts.presence);
        let mut s = s;
        s.alerts.volume_pct = 35;
        s.alerts.presence = true;
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!((back.alerts.volume_pct, back.alerts.presence), (35, true));
        let s: Settings = serde_json::from_str(r#"{"alerts":{"enabled":false}}"#).unwrap();
        assert!(!s.alerts.enabled && s.alerts.volume_pct == 60);
    }

    #[test]
    fn old_settings_files_and_partial_entries_load() {
        let s: Settings = serde_json::from_str(r#"{"server":"x"}"#).unwrap();
        assert!(s.user_audio.is_empty());
        let s: Settings = serde_json::from_str(r#"{"user_audio":{"ana":{"muted":true}}}"#).unwrap();
        let a = s.user_audio("ana");
        assert!(a.muted);
        assert_eq!((a.volume_pct, a.stream_volume_pct), (100, 100));
    }

    #[test]
    fn input_gate_defaults_to_automatic_and_roundtrips() {
        use vox_client_core::GateMode;
        let s: Settings = serde_json::from_str(r#"{"server":"x"}"#).unwrap();
        assert_eq!(s.input_gate, GateConfig::default());
        assert_eq!(s.input_gate.mode, GateMode::Auto);
        let mut s = s;
        s.input_gate = GateConfig { mode: GateMode::Manual, threshold_db: -38.0, hold_ms: 500 };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"manual\""));
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.input_gate, s.input_gate);
        // A hand-edited file with silly values is clamped when applied.
        let s: Settings = serde_json::from_str(r#"{"input_gate":{"mode":"manual","threshold_db":-400.0,"hold_ms":0}}"#).unwrap();
        assert_eq!(s.input_gate.sanitized().threshold_db, vox_client_core::audio::gate::THRESHOLD_MIN_DB);
    }

    #[test]
    fn voice_fx_defaults_to_off_and_roundtrips() {
        use vox_client_core::VoiceEffect;
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.voice_fx, VoiceFxConfig { effect: VoiceEffect::Off, intensity_pct: 50 });
        let s = Settings { voice_fx: VoiceFxConfig { effect: VoiceEffect::Cave, intensity_pct: 80 }, ..Settings::default() };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""voice_fx":{"effect":"cave","intensity_pct":80}"#), "{json}");
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.voice_fx, s.voice_fx);
        // An effect this version does not know: off.
        let s: Settings = serde_json::from_str(r#"{"voice_fx":{"effect":"vocoder"}}"#).unwrap();
        assert_eq!(s.voice_fx.effect, VoiceEffect::Off);
    }

    #[test]
    fn noise_suppression_defaults_to_ai_light_and_roundtrips() {
        let s: Settings = serde_json::from_str(r#"{"server":"x"}"#).unwrap();
        assert_eq!(s.noise_suppression, NoiseLevel::AiLight);
        let mut s = s;
        s.noise_suppression = NoiseLevel::Standard;
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"noise_suppression\":\"standard\""), "{json}");
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.noise_suppression, NoiseLevel::Standard);
        // IA máxima persists as "ai_max".
        let s: Settings = serde_json::from_str(r#"{"noise_suppression":"ai_max"}"#).unwrap();
        assert_eq!(s.noise_suppression, NoiseLevel::AiMax);
        assert!(serde_json::to_string(&s).unwrap().contains("\"noise_suppression\":\"ai_max\""));
        // A level this version does not know (a newer app's file) falls back to the default.
        let s: Settings = serde_json::from_str(r#"{"noise_suppression":"ai_ultra"}"#).unwrap();
        assert_eq!(s.noise_suppression, NoiseLevel::AiLight);
    }

    #[test]
    fn camera_settings_default_and_roundtrip() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!((s.camera_device.clone(), s.camera_preset), (None, CameraPreset::P720));
        let s = Settings {
            camera_device: Some(r"\\?\usb#vid_046d&pid_082d#{e5323777}\global".into()),
            camera_name: Some("HD Pro Webcam C920".into()),
            camera_preset: CameraPreset::P360,
            ..Default::default()
        };
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.camera_device, s.camera_device);
        assert_eq!(back.camera_name, s.camera_name);
        assert_eq!(back.camera_preset, CameraPreset::P360);
        assert_eq!(CameraPreset::P1080.to_core().dims(), (1920, 1080, 30));
    }

    #[test]
    fn video_encoder_choice_defaults_to_automatic_hardware_and_roundtrips() {
        let s: Settings = serde_json::from_str(r#"{"server":"x"}"#).unwrap();
        assert!(s.hw_accel && s.video_encoder.is_none());
        assert_eq!(s.encoder_pref(), Default::default());
        let s = Settings { hw_accel: false, video_encoder: Some("nvenc:10de:1f99:00001234:0".into()), ..Default::default() };
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(!back.hw_accel);
        assert_eq!(back.encoder_pref().encoder.as_deref(), Some("nvenc:10de:1f99:00001234:0"));
        assert_eq!(encoder_cache_path().map(|p| p.file_name().unwrap().to_owned()), Some("encoders.json".into()));
    }

    #[test]
    fn pref_converts_to_core_gains() {
        let c = UserAudioPref { volume_pct: 200, muted: true, stream_volume_pct: 50, stream_muted: false }.to_core();
        assert_eq!((c.volume, c.muted, c.stream_volume, c.stream_muted), (2.0, true, 0.5, false));
        assert!(UserAudioPref::default().to_core().is_default());
    }

    #[test]
    fn device_tokens_are_per_server_and_name_and_encrypted() {
        let mut s = Settings::default();
        s.set_device_token("http://1.2.3.4:7890", "Ana", Some("tok-ana"));
        s.set_device_token("http://1.2.3.4:7890", "Bia", Some("tok-bia"));
        assert_eq!(s.device_token("HTTP://1.2.3.4:7890 ", "ana").as_deref(), Some("tok-ana"));
        assert_eq!(s.device_token("http://other:7890", "Ana"), None);
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("tok-ana"), "stored encrypted");
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.device_token("http://1.2.3.4:7890", "Bia").as_deref(), Some("tok-bia"));
        s.set_device_token("http://1.2.3.4:7890", "Ana", None);
        assert_eq!(s.device_token("http://1.2.3.4:7890", "Ana"), None);
        // Settings files from before item 12 load with none.
        let old: Settings = serde_json::from_str(r#"{"server":"x","username":"y"}"#).unwrap();
        assert!(old.device_tokens.is_empty());
    }
}
