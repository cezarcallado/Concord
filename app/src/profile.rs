//! User profiles in the app (v0.6): display name, "Sobre mim", profile colour, custom status and
//! presence, the Discord-style card that opens on a left click ("popout"), and the "Perfil"
//! section of the settings. The wire contract is `vox_proto::profile`; the hub enforces every
//! limit and everything about invisibility, this module only shows and sends.
//!
//! On a hub that predates profiles (`GET /api/users/{id}/profile` answers `404`) the popout still
//! opens, with what the app already knows (name, picture, role), and the settings section is not
//! shown.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Pos2, RichText, Sense, Vec2};
use vox_client_core::hub::{HttpStatusError, HubClient, ProfileReply};
use vox_proto::*;

use crate::app::{local_offset_min, name_color, now_ms, App, GREEN, RED};
use crate::avatars::Surface;
use crate::backend::Cmd;
use crate::theme::{self, ph, space};
use crate::ui_kit::{self, Kind};

/// How often idle time is looked at (the UI wakes itself this often while connected).
const IDLE_CHECK: Duration = Duration::from_secs(20);
/// A cached profile is shown right away when a card opens, and fetched again anyway.
const POPOUT_WIDTH: f32 = 320.0;
const BANNER_H: f32 = 72.0;
const AVATAR_R: f32 = 34.0;
/// Default minutes of inactivity before "Ausente" (0 = never).
pub const DEFAULT_IDLE_AWAY_MINUTES: u32 = 10;
pub const OFFLINE_COLOR: Color32 = theme::DESLIGADO;

/// The accent colours offered next to the picker.
const SWATCHES: [[u8; 3]; 10] = [
    [88, 101, 242],
    [87, 242, 135],
    [254, 231, 92],
    [235, 69, 158],
    [237, 66, 69],
    [240, 140, 50],
    [26, 188, 156],
    [52, 152, 219],
    [155, 89, 182],
    [149, 165, 166],
];

/// A few emoji to pick from for a status (the font has no picker; any emoji can be typed or pasted).
const QUICK_EMOJI: [&str; 12] = ["🎮", "💤", "📚", "🎧", "📺", "🔥", "❤", "😀", "😴", "🏠", "✈", "🎵"];

// ---------------------------------------------------------------- settings

/// Profile-related settings kept on this computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ProfileSettings {
    /// Minutes without keyboard or mouse before "Ausente" (0 = never automatically).
    pub idle_away_minutes: u32,
}

impl Default for ProfileSettings {
    fn default() -> Self {
        ProfileSettings { idle_away_minutes: DEFAULT_IDLE_AWAY_MINUTES }
    }
}

// ---------------------------------------------------------------- pure helpers

fn decorate_hit(resp: egui::Response, user: &User) -> egui::Response {
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    if user.has_display_name() {
        resp.on_hover_text(format!("@{}", user.username))
    } else {
        resp
    }
}

/// [`App::profile_hit`] for places that cannot borrow the app (the chat list): pointer cursor,
/// "@username" on hover, and whether it was clicked.
pub fn hit(resp: egui::Response, user: &User) -> bool {
    decorate_hit(resp, user).clicked()
}

pub fn presence_label(p: Presence) -> &'static str {
    match p {
        Presence::Online => "Online",
        Presence::Away => "Ausente",
        Presence::Dnd => "Não perturbe",
        Presence::Invisible => "Invisível",
    }
}

pub fn presence_hint(p: Presence) -> &'static str {
    match p {
        Presence::Online => "As pessoas veem você online.",
        Presence::Away => "As pessoas veem que você está ausente.",
        Presence::Dnd => "Sons de alerta e avisos de mensagens diretas ficam em silêncio neste computador.",
        Presence::Invisible => "As pessoas veem você offline, mas tudo continua funcionando.",
    }
}

pub fn presence_color(p: Presence) -> Color32 {
    match p {
        Presence::Online => GREEN,
        Presence::Away => theme::NEVOA,
        Presence::Dnd => RED,
        Presence::Invisible => OFFLINE_COLOR,
    }
}

/// The status to show now: set, and not past its time.
pub fn live_status(status: Option<&CustomStatus>, now_ms: i64) -> Option<&CustomStatus> {
    status.and_then(|s| s.live(now_ms))
}

/// "🎮 jogando", or just one of the two.
pub fn status_line(s: &CustomStatus) -> String {
    match (s.emoji.is_empty(), s.text.is_empty()) {
        (false, false) => format!("{} {}", s.emoji, s.text),
        (false, true) => s.emoji.clone(),
        _ => s.text.clone(),
    }
}

/// "Limpar em": when a status clears itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClearAfter {
    Min30,
    Hour1,
    Hours4,
    Today,
    #[default]
    Never,
}

impl ClearAfter {
    pub const ALL: [ClearAfter; 5] = [ClearAfter::Min30, ClearAfter::Hour1, ClearAfter::Hours4, ClearAfter::Today, ClearAfter::Never];

    pub fn label(self) -> &'static str {
        match self {
            ClearAfter::Min30 => "30 minutos",
            ClearAfter::Hour1 => "1 hora",
            ClearAfter::Hours4 => "4 horas",
            ClearAfter::Today => "Hoje",
            ClearAfter::Never => "Nunca",
        }
    }

    /// Seconds from now (`None` = never). "Hoje" is until local midnight (at least a minute).
    pub fn secs(self, now_ms: i64, offset_min: i32) -> Option<u64> {
        match self {
            ClearAfter::Min30 => Some(30 * 60),
            ClearAfter::Hour1 => Some(3600),
            ClearAfter::Hours4 => Some(4 * 3600),
            ClearAfter::Today => Some(secs_until_local_midnight(now_ms, offset_min).max(60)),
            ClearAfter::Never => None,
        }
    }
}

/// Seconds until the next local midnight.
pub fn secs_until_local_midnight(now_ms: i64, offset_min: i32) -> u64 {
    let local = now_ms.div_euclid(1000) + i64::from(offset_min) * 60;
    (86_400 - local.rem_euclid(86_400)) as u64
}

/// Days since 1970-01-01 -> (year, month 1-12, day 1-31). (Howard Hinnant's algorithm.)
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// "12 de outubro de 2026".
pub fn format_date_pt(ms: i64, offset_min: i32) -> String {
    const MONTHS: [&str; 12] = ["janeiro", "fevereiro", "março", "abril", "maio", "junho", "julho", "agosto", "setembro", "outubro", "novembro", "dezembro"];
    let local = ms.div_euclid(1000) + i64::from(offset_min) * 60;
    let (y, m, d) = civil_from_days(local.div_euclid(86_400));
    format!("{d} de {} de {y}", MONTHS[(m - 1) as usize])
}

/// The profile colour as an egui colour.
pub fn accent_color(accent: Option<&str>) -> Option<Color32> {
    accent.and_then(accent_rgb).map(|[r, g, b]| Color32::from_rgb(r, g, b))
}

pub fn hex_of(rgb: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])
}

/// What to do about "Ausente" now: `Some(true)` = tell the hub I am idle, `Some(false)` = tell it
/// I am back, `None` = nothing. Idle detection only ever moves between Online and Away: a
/// presence the person chose is left alone, and so is a call (someone talking is not away).
pub fn idle_action(idle_secs: u64, threshold_min: u32, in_voice: bool, my_presence: Presence, auto_sent: bool) -> Option<bool> {
    let want_away = threshold_min > 0 && !in_voice && idle_secs >= u64::from(threshold_min) * 60;
    match (auto_sent, want_away) {
        (false, true) if my_presence == Presence::Online => Some(true),
        (true, false) => Some(false),
        _ => None,
    }
}

static FAKE_IDLE: AtomicI64 = AtomicI64::new(-1);

/// Pretend the keyboard and mouse have been idle for this long (`None`: the real thing). The
/// autotest uses it to see "Ausente" without waiting ten minutes.
pub fn set_fake_idle(secs: Option<u64>) {
    FAKE_IDLE.store(secs.map(|s| s as i64).unwrap_or(-1), Ordering::Relaxed);
}

/// Seconds since the last keyboard or mouse input anywhere in Windows.
pub fn system_idle_secs() -> u64 {
    let fake = FAKE_IDLE.load(Ordering::Relaxed);
    if fake >= 0 {
        return fake as u64;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::GetTickCount;
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
        // SAFETY: plain query into a zeroed out-struct with its size set.
        unsafe {
            let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
            if GetLastInputInfo(&mut info) == 0 {
                return 0;
            }
            u64::from(GetTickCount().wrapping_sub(info.dwTime)) / 1000
        }
    }
    #[cfg(not(windows))]
    {
        0
    }
}

// ---------------------------------------------------------------- backend side

/// The profile calls the backend runs ([`Cmd::Profile`]); the answer is a [`ProfileEv`].
#[derive(Debug, Clone)]
pub enum ProfileCmd {
    Fetch { user: UserId },
    Update(ProfileUpdate),
    SetPresence { presence: Presence, auto: bool },
    SetStatus(StatusUpdate),
    /// A moderator clears parts of somebody else's profile.
    ModClear { user: UserId, what: ModClearProfileRequest },
}

#[derive(Debug)]
pub enum ProfileEv {
    Fetched { user: UserId, reply: ProfileReply },
    Updated(Result<UserProfile, String>),
    PresenceSet { asked: Presence, auto: bool, result: Result<Presence, String> },
    StatusSet(Result<Option<CustomStatus>, String>),
    ModCleared { user: UserId, result: Result<(), String> },
}

impl ProfileCmd {
    /// The event for a command that cannot run (no hub connection).
    pub fn offline(self) -> ProfileEv {
        let msg = "Sem conexão com o servidor.".to_string();
        match self {
            ProfileCmd::Fetch { user } => ProfileEv::Fetched { user, reply: ProfileReply::Failed("offline".into()) },
            ProfileCmd::Update(_) => ProfileEv::Updated(Err(msg)),
            ProfileCmd::SetPresence { presence, auto } => ProfileEv::PresenceSet { asked: presence, auto, result: Err(msg) },
            ProfileCmd::SetStatus(_) => ProfileEv::StatusSet(Err(msg)),
            ProfileCmd::ModClear { user, .. } => ProfileEv::ModCleared { user, result: Err(msg) },
        }
    }
}

pub async fn run(hub: &HubClient, cmd: ProfileCmd) -> ProfileEv {
    match cmd {
        ProfileCmd::Fetch { user } => ProfileEv::Fetched { user, reply: hub.profile(user).await },
        ProfileCmd::Update(u) => ProfileEv::Updated(hub.update_profile(&u).await.map_err(|e| error_text(&e))),
        ProfileCmd::SetPresence { presence, auto } => {
            ProfileEv::PresenceSet { asked: presence, auto, result: hub.set_presence(presence, auto).await.map_err(|e| error_text(&e)) }
        }
        ProfileCmd::SetStatus(u) => ProfileEv::StatusSet(hub.set_status(&u).await.map_err(|e| error_text(&e))),
        ProfileCmd::ModClear { user, what } => ProfileEv::ModCleared { user, result: hub.mod_clear_profile(user, what).await.map_err(|e| error_text(&e)) },
    }
}

/// The hub's refusals, for the people.
pub fn error_text(e: &anyhow::Error) -> String {
    tracing::warn!(error = %format!("{e:#}"), "profile call failed");
    let Some(st) = e.downcast_ref::<HttpStatusError>() else { return crate::backend::friendly(e) };
    let m = st.message.to_lowercase();
    match st.status {
        429 => match m.split("wait ").nth(1).and_then(|r| r.split_whitespace().next()).and_then(|n| n.parse::<u64>().ok()) {
            Some(s) if s >= 60 => format!("Muitas alterações seguidas. Tente de novo em {} min.", s.div_ceil(60)),
            Some(s) => format!("Muitas alterações seguidas. Tente de novo em {s} s."),
            None => "Muitas alterações seguidas. Tente de novo daqui a pouco.".into(),
        },
        400 if m.contains("display name must be") => format!("O nome de exibição precisa ter de 1 a {DISPLAY_NAME_MAX} caracteres."),
        400 if m.contains("display name has") => "O nome de exibição tem caracteres que não são permitidos.".into(),
        400 if m.contains("bio must be") => format!("O \"Sobre mim\" pode ter até {BIO_MAX} caracteres e {BIO_MAX_LINES} quebras de linha."),
        400 if m.contains("bio has") => "O \"Sobre mim\" tem caracteres que não são permitidos.".into(),
        400 if m.contains("colour") => "A cor precisa ser no formato #rrggbb.".into(),
        400 if m.contains("status text must") => format!("O texto do status pode ter até {STATUS_TEXT_MAX} caracteres."),
        400 if m.contains("status text has") => "O texto do status tem caracteres que não são permitidos.".into(),
        400 if m.contains("status emoji") => "O emoji do status precisa ser um único emoji.".into(),
        400 if m.contains("a status can clear") => "O status pode se apagar sozinho em até 7 dias.".into(),
        409 => "Esse nome é o nome de usuário de outra pessoa. Escolha outro.".into(),
        403 => "Seu cargo não permite isso.".into(),
        404 | 405 => "Este servidor é de uma versão antiga e ainda não tem perfis.".into(),
        _ => crate::backend::friendly(e),
    }
}

// ---------------------------------------------------------------- UI state

/// Presence and status of one person, as the hub last said (people who are plain "online, no
/// status" have no entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresenceInfo {
    pub presence: Presence,
    pub status: Option<CustomStatus>,
}

struct Cached {
    profile: UserProfile,
}

/// The open profile card.
#[derive(Clone)]
pub struct Popout {
    pub user: User,
    pub anchor: Pos2,
    /// Drawn for the first time this frame: the click that opened it must not close it.
    just_opened: bool,
    /// "Volume" is open (the slider under the buttons).
    volume: bool,
}

#[derive(Default)]
pub struct Form {
    pub display_name: String,
    pub bio: String,
    pub accent: Option<[u8; 3]>,
    pub status_emoji: String,
    pub status_text: String,
    pub clear_after: ClearAfter,
    /// What the fields were filled from (to know whether they changed).
    pub loaded: Option<UserProfile>,
    pub busy: bool,
    pub status_busy: bool,
    pub msg: Option<(bool, String)>,
    pub status_msg: Option<(bool, String)>,
}

impl Form {
    /// Fill the profile fields from the hub's copy of mine.
    fn fill(&mut self, p: &UserProfile) {
        self.display_name = p.user.display_name.clone().unwrap_or_default();
        self.bio = p.bio.clone();
        self.accent = p.accent.as_deref().and_then(accent_rgb);
        self.loaded = Some(p.clone());
    }

    /// The edit to send: only what differs from what the hub has; `None` when nothing does.
    pub fn changes(&self) -> Option<ProfileUpdate> {
        let base = self.loaded.as_ref()?;
        let mut u = ProfileUpdate::default();
        let name = self.display_name.trim();
        if name != base.user.display_name.as_deref().unwrap_or("") {
            u.display_name = Some(name.to_string());
        }
        let bio = self.bio.trim();
        if bio != base.bio {
            u.bio = Some(bio.to_string());
        }
        let accent = self.accent.map(hex_of);
        if accent != base.accent {
            u.accent = Some(accent.unwrap_or_default());
        }
        (u != ProfileUpdate::default()).then_some(u)
    }
}

#[derive(Default)]
pub struct ProfileUi {
    /// Does the hub know profiles? `None` until the first answer.
    pub supported: Option<bool>,
    /// Presence and status of the people it is news about (me included, even when invisible).
    pub presence: HashMap<UserId, PresenceInfo>,
    cache: HashMap<UserId, Cached>,
    /// People whose profile is being fetched.
    loading: Vec<UserId>,
    failed: Vec<UserId>,
    pub popout: Option<Popout>,
    /// The card was drawn for this person, with their loaded profile or without (the autotest).
    pub popout_drawn: Option<(UserId, bool)>,
    pub form: Form,
    /// "Ausente" was set by idle detection (and so is undone by it).
    pub auto_away_sent: bool,
    last_idle_check: Option<Instant>,
}

impl ProfileUi {
    pub fn reset(&mut self) {
        *self = ProfileUi::default();
    }
}

impl App {
    // ------------------------------------------------------------ what the app knows

    /// My presence as the hub has it.
    pub fn my_presence(&self) -> Presence {
        self.my_id().and_then(|id| self.profile.presence.get(&id)).map(|i| i.presence).unwrap_or_default()
    }

    /// "Não perturbe": alert sounds and direct-message notifications are silent on this computer.
    pub fn notifications_muted(&self) -> bool {
        self.my_presence() == Presence::Dnd
    }

    /// The presence to draw next to someone in a list (only called for people listed as online).
    pub fn presence_of(&self, id: UserId) -> Presence {
        self.profile.presence.get(&id).map(|i| i.presence).unwrap_or_default()
    }

    /// Their live custom status, if they have one.
    pub fn status_of(&self, id: UserId) -> Option<CustomStatus> {
        let now = now_ms();
        live_status(self.profile.presence.get(&id).and_then(|i| i.status.as_ref()), now).cloned()
    }

    /// The freshest `User` the app has for `id`.
    pub fn known_user(&self, id: UserId) -> Option<User> {
        if let Some(me) = self.me.as_ref().filter(|m| m.id == id) {
            return Some(me.clone());
        }
        self.online
            .iter()
            .find(|u| u.id == id)
            .cloned()
            .or_else(|| self.voice_members.get(&id).map(|m| m.user.clone()))
            .or_else(|| self.logs.values().flat_map(|l| l.msgs.iter().rev()).find(|m| m.author.id == id).map(|m| m.author.clone()))
    }

    /// The profile the hub last gave for this person.
    pub fn cached_profile(&self, id: UserId) -> Option<&UserProfile> {
        self.profile.cache.get(&id).map(|c| &c.profile)
    }

    /// Forget the loaded copy of a profile (the next card opens with a fresh fetch).
    pub fn forget_profile(&mut self, id: UserId) {
        self.profile.cache.remove(&id);
    }

    /// May I clear parts of this person's profile (a moderator above their role)?
    /// (has an "about me", has a status) of a person's cached card; `None` while not loaded.
    pub fn profile_clear_parts(&self, user: &User) -> Option<(bool, bool)> {
        self.profile.cache.get(&user.id).map(|c| (!c.profile.bio.is_empty(), c.profile.status.is_some()))
    }

    pub fn can_clear_profile_of(&self, id: UserId) -> bool {
        self.roles.supported && self.profile.supported == Some(true) && Some(id) != self.my_id() && self.my_role().outranks(self.role_of(id))
    }

    // ------------------------------------------------------------ hub events

    /// A fresh `Ready`: forget presence, check whether this hub knows profiles, load mine.
    pub fn profile_on_ready(&mut self) {
        let popout = self.profile.popout.take();
        let (supported, form) = (self.profile.supported, std::mem::take(&mut self.profile.form));
        self.profile.reset();
        // A reconnect to the same hub keeps what was learnt about it and what is being edited.
        self.profile.supported = supported;
        self.profile.form = Form { busy: false, status_busy: false, ..form };
        self.profile.popout = popout;
        if let Some(id) = self.my_id() {
            self.fetch_profile(id);
        }
    }

    pub fn on_presence_changed(&mut self, user_id: UserId, presence: Presence, status: Option<CustomStatus>) {
        self.profile.supported.get_or_insert(true);
        if presence == Presence::Online && status.is_none() {
            self.profile.presence.remove(&user_id);
        } else {
            self.profile.presence.insert(user_id, PresenceInfo { presence, status: status.clone() });
        }
        if let Some(c) = self.profile.cache.get_mut(&user_id) {
            c.profile.presence = presence;
            c.profile.status = status;
        }
        if Some(user_id) == self.my_id() && presence != Presence::Away {
            self.profile.auto_away_sent = false;
        }
    }

    /// Someone went offline (or invisible).
    pub fn profile_on_offline(&mut self, user_id: UserId) {
        if Some(user_id) != self.my_id() {
            self.profile.presence.remove(&user_id);
        }
    }

    pub fn on_profile_event(&mut self, ev: ProfileEv) {
        match ev {
            ProfileEv::Fetched { user, reply } => {
                self.profile.loading.retain(|u| *u != user);
                match reply {
                    ProfileReply::Ready(p) => {
                        self.profile.supported = Some(true);
                        self.profile.failed.retain(|u| *u != user);
                        if Some(user) == self.my_id() && self.profile.form.changes().is_none() {
                            self.profile.form.fill(&p);
                        }
                        self.profile.cache.insert(user, Cached { profile: *p });
                    }
                    ProfileReply::Unsupported => {
                        tracing::info!("this hub has no profiles (older server): profile features are hidden");
                        self.profile.supported = Some(false);
                    }
                    ProfileReply::Failed(e) => {
                        tracing::warn!(error = %e, user, "loading a profile failed");
                        if !self.profile.failed.contains(&user) {
                            self.profile.failed.push(user);
                        }
                    }
                }
            }
            ProfileEv::Updated(result) => {
                let f = &mut self.profile.form;
                f.busy = false;
                match result {
                    Ok(p) => {
                        f.fill(&p);
                        f.msg = Some((true, "Perfil salvo.".into()));
                        if let Some(me) = self.me.as_mut() {
                            me.display_name = p.user.display_name.clone();
                        }
                        self.profile.cache.insert(p.user.id, Cached { profile: p });
                    }
                    Err(e) => f.msg = Some((false, e)),
                }
            }
            ProfileEv::PresenceSet { asked, auto, result } => match result {
                Ok(p) => {
                    tracing::info!(?asked, auto, now = ?p, "presence set");
                    if let Some(id) = self.my_id() {
                        // The hub's PresenceChanged says the same; this keeps my own screen right if it is late.
                        let status = self.profile.presence.get(&id).and_then(|i| i.status.clone());
                        self.on_presence_changed(id, p, status);
                    }
                }
                Err(e) => {
                    if auto {
                        self.profile.auto_away_sent = false;
                    } else {
                        self.notice = Some(e);
                    }
                }
            },
            ProfileEv::StatusSet(result) => {
                let f = &mut self.profile.form;
                f.status_busy = false;
                match result {
                    Ok(status) => {
                        f.status_msg = Some((true, if status.is_some() { "Status definido." } else { "Status limpo." }.into()));
                        if status.is_none() {
                            f.status_emoji.clear();
                            f.status_text.clear();
                        }
                        if let Some(id) = self.my_id() {
                            let presence = self.my_presence();
                            self.on_presence_changed(id, presence, status);
                        }
                    }
                    Err(e) => f.status_msg = Some((false, e)),
                }
            }
            ProfileEv::ModCleared { user, result } => match result {
                Ok(()) => {
                    self.toast("Perfil limpo (fica no registro de moderação).");
                    self.fetch_profile(user);
                }
                Err(e) => self.notice = Some(e),
            },
        }
    }

    pub fn fetch_profile(&mut self, user: UserId) {
        if self.profile.supported == Some(false) || self.profile.loading.contains(&user) {
            return;
        }
        self.profile.loading.push(user);
        self.backend.send(Cmd::Profile(ProfileCmd::Fetch { user }));
    }

    // ------------------------------------------------------------ my own profile (actions)

    /// Choose my presence (what the buttons do). The hub persists it.
    pub fn set_presence(&mut self, presence: Presence) {
        self.profile.auto_away_sent = false;
        self.backend.send(Cmd::Profile(ProfileCmd::SetPresence { presence, auto: false }));
    }

    /// "Salvar perfil": send what changed.
    pub fn save_profile(&mut self) {
        let f = &mut self.profile.form;
        let Some(update) = f.changes().filter(|_| !f.busy) else { return };
        // The same rules as the hub, so the mistake is explained before the round trip.
        let checked = update
            .display_name
            .as_deref()
            .map(clean_display_name)
            .transpose()
            .map(|_| ())
            .and(update.bio.as_deref().map(clean_bio).transpose().map(|_| ()))
            .and(update.accent.as_deref().map(clean_accent).transpose().map(|_| ()));
        if let Err(e) = checked {
            f.msg = Some((false, error_text(&anyhow::Error::new(HttpStatusError { status: 400, message: e.to_string() }))));
            return;
        }
        f.busy = true;
        f.msg = None;
        self.backend.send(Cmd::Profile(ProfileCmd::Update(update)));
    }

    /// "Definir status" with the form's fields.
    pub fn save_status(&mut self) {
        let (offset, now) = (local_offset_min(), now_ms());
        let f = &mut self.profile.form;
        if f.status_busy {
            return;
        }
        let expires_in_secs = f.clear_after.secs(now, offset);
        let update = StatusUpdate { emoji: f.status_emoji.trim().to_string(), text: f.status_text.trim().to_string(), expires_in_secs };
        if let Err(e) = clean_status(&update.emoji, &update.text, update.expires_in_secs, now) {
            f.status_msg = Some((false, error_text(&anyhow::Error::new(HttpStatusError { status: 400, message: e.to_string() }))));
            return;
        }
        f.status_busy = true;
        f.status_msg = None;
        self.backend.send(Cmd::Profile(ProfileCmd::SetStatus(update)));
    }

    pub fn clear_status(&mut self) {
        let f = &mut self.profile.form;
        if f.status_busy {
            return;
        }
        f.status_busy = true;
        f.status_msg = None;
        self.backend.send(Cmd::Profile(ProfileCmd::SetStatus(StatusUpdate::default())));
    }

    // ------------------------------------------------------------ idle -> "Ausente"

    /// Called every frame: after the configured idle time the hub is told "Ausente" (unless I
    /// chose another presence or I am in a call), and told I am back at the first input.
    pub fn profile_tick(&mut self, ctx: &egui::Context) {
        if !self.ready || self.profile.supported != Some(true) {
            return;
        }
        let minutes = self.settings.profile.idle_away_minutes;
        if minutes == 0 && !self.profile.auto_away_sent {
            return;
        }
        let now = Instant::now();
        if self.profile.last_idle_check.is_some_and(|t| now.duration_since(t) < Duration::from_secs(5)) {
            ctx.request_repaint_after(IDLE_CHECK);
            return;
        }
        self.profile.last_idle_check = Some(now);
        let in_voice = self.voice.channel.is_some() || self.voice.joining.is_some();
        if let Some(away) = idle_action(system_idle_secs(), minutes, in_voice, self.my_presence(), self.profile.auto_away_sent) {
            tracing::info!(away, "idle detection: presence");
            self.profile.auto_away_sent = away;
            let presence = if away { Presence::Away } else { Presence::Online };
            self.backend.send(Cmd::Profile(ProfileCmd::SetPresence { presence, auto: true }));
        }
        ctx.request_repaint_after(IDLE_CHECK);
    }

    // ------------------------------------------------------------ the popout

    /// Left click on a person: open their card next to the pointer.
    pub fn open_profile(&mut self, ctx: &egui::Context, user: &User) {
        let anchor = ctx.input(|i| i.pointer.interact_pos().or(i.pointer.latest_pos())).unwrap_or(Pos2::new(320.0, 180.0));
        self.open_profile_at(user, anchor);
    }

    pub fn open_profile_at(&mut self, user: &User, anchor: Pos2) {
        if self.screen != crate::app::Screen::Main {
            return;
        }
        self.profile.popout = Some(Popout { user: user.clone(), anchor, just_opened: true, volume: false });
        self.fetch_profile(user.id);
    }

    pub fn close_profile(&mut self) {
        self.profile.popout = None;
    }

    // "Enviar mensagem" of the card calls `App::open_dm` (direct messages, `dms.rs`).

    /// A clickable hit on a person's name or picture: pointer cursor, "@username" on hover, and
    /// the card on click.
    pub fn profile_hit(&mut self, ui: &egui::Ui, resp: egui::Response, user: &User) -> egui::Response {
        let resp = decorate_hit(resp, user);
        if resp.clicked() {
            self.open_profile(ui.ctx(), user);
        }
        resp
    }

    /// The person's card (Discord-like, v1.0): a banner in their profile colour, the picture over
    /// its edge with the presence dot, the name (in the role's colour) and badge, the username,
    /// presence and status; then "Sobre mim", the role and "Membro desde" on an inner card; and
    /// the actions: "Mensagem", "Ligar" and "Volume" (somebody in my room), or "Editar perfil" on
    /// my own; "⋯" opens the right-click menu.
    pub fn profile_popout(&mut self, ctx: &egui::Context) {
        let Some(pop) = self.profile.popout.clone() else { return };
        if self.screen != crate::app::Screen::Main {
            self.profile.popout = None;
            return;
        }
        let user = self.known_user(pop.user.id).unwrap_or_else(|| pop.user.clone());
        let me = self.my_id() == Some(user.id);
        let now = now_ms();
        let supported = self.profile.supported != Some(false);
        let cached = self.profile.cache.get(&user.id).map(|c| c.profile.clone());
        let loading = supported && cached.is_none() && !self.profile.failed.contains(&user.id);
        let failed = supported && cached.is_none() && self.profile.failed.contains(&user.id);
        let online = me || self.online.iter().any(|u| u.id == user.id);
        let presence = if me { self.my_presence() } else { self.presence_of(user.id) };
        // An online person's live status; an offline one's is whatever the hub last served.
        let status = if online { self.status_of(user.id) } else { cached.as_ref().and_then(|c| live_status(c.status.as_ref(), now).cloned()) };
        let accent = cached.as_ref().and_then(|c| accent_color(c.accent.as_deref()));
        let banner_color = accent.unwrap_or_else(|| name_color(user.id).gamma_multiply(0.55));
        let roles_on = self.roles.supported;
        let role = if roles_on { user.role } else { Role::Member };
        let can_clear = self.can_clear_profile_of(user.id);
        let offset = local_offset_min();
        let facts = self.menu_facts(&user, crate::context_menu::Origin::Popout, crate::context_menu::Lead::None);
        let can_call = facts.dms_ok && !facts.in_call_with;
        let call_why = if !facts.hub_up {
            Some("Sem conexão com o servidor")
        } else if facts.blocked {
            Some("Você bloqueou esta pessoa: desbloqueie para ligar")
        } else {
            None
        };

        let screen = ctx.content_rect();
        let mut pos = pop.anchor + Vec2::new(14.0, -10.0);
        if pos.x + POPOUT_WIDTH > screen.right() - 6.0 {
            pos.x = (pop.anchor.x - POPOUT_WIDTH - 14.0).max(6.0);
        }
        // (Kept inside the window with the card's height from the last frame: a long card moves up;
        // v1.0 phase 4, the card ran off the bottom of a 600-point window.)
        let win_id = egui::Id::new(("profile_popout", user.id));
        // (The height the card wants, not the one it got: a card squeezed by the window's bottom
        // edge would otherwise never move up.)
        let natural_id = win_id.with("natural_h");
        let card_h = ctx.data(|d| d.get_temp::<f32>(natural_id)).unwrap_or(420.0);
        pos.y = pos.y.clamp(6.0, (screen.bottom() - card_h - 6.0).max(6.0));

        let mut close = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let (mut dm, mut edit, mut call) = (false, false, false);
        let mut volume_open = pop.volume;
        let mut menu_at: Option<Pos2> = None;
        let mut clear = ModClearProfileRequest::default();
        let frame = egui::Frame::new()
            .fill(theme::PAREDE)
            .stroke(egui::Stroke::new(theme::stroke::HAIRLINE, theme::RISCO))
            .corner_radius(theme::radius::POPUP)
            .shadow(ctx.global_style().visuals.popup_shadow);
        let shown = egui::Window::new("profile_popout")
            .id(win_id)
            .title_bar(false)
            .resizable(false)
            .collapsible(false)
            .fixed_pos(pos)
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_width(POPOUT_WIDTH);
                ui.spacing_mut().item_spacing.y = 4.0;
                // Banner, and the picture overlapping its lower edge (a ring in the card's colour).
                let r = theme::radius::POPUP;
                let (banner, _) = ui.allocate_exact_size(Vec2::new(POPOUT_WIDTH, BANNER_H), Sense::hover());
                ui.painter().rect_filled(banner, egui::CornerRadius { nw: r, ne: r, sw: 0, se: 0 }, banner_color);
                let center = Pos2::new(banner.left() + 18.0 + AVATAR_R, banner.bottom());
                ui.painter().circle_filled(center, AVATAR_R + 5.0, theme::PAREDE);
                crate::avatars::draw_avatar(&mut self.avatars.store, ui.painter(), ui.ctx(), center, AVATAR_R, &user, Surface::Popout);
                let dot = center + Vec2::new(AVATAR_R * 0.72, AVATAR_R * 0.72);
                let dot_color = if online { presence_color(presence) } else { OFFLINE_COLOR };
                ui.painter().circle_filled(dot, 10.0, theme::PAREDE);
                ui.painter().circle_filled(dot, 7.0, dot_color);
                // "⋯" on the banner: the same menu as a right click anywhere else.
                let more_rect = egui::Rect::from_min_size(Pos2::new(banner.right() - 38.0, banner.top() + 8.0), Vec2::splat(30.0));
                // (In a child: `put` would move the layout's cursor back up to the banner.)
                let mut corner = ui.new_child(egui::UiBuilder::new().max_rect(more_rect));
                let more = ui_kit::icon_button(&mut corner, ph::DOTS_THREE, "Mais opções (o mesmo menu do botão direito)", ui_kit::IconState::Normal);
                if !more.hovered() {
                    // Readable over any banner colour.
                    ui.painter().rect_filled(more.rect, theme::radius::ROW, theme::ETIQUETA);
                    ui.painter().text(more.rect.center(), egui::Align2::CENTER_CENTER, ph::DOTS_THREE, theme::body(18.0), theme::GIZ);
                }
                if more.clicked() {
                    menu_at = Some(more.rect.left_bottom());
                }
                ui.add_space(AVATAR_R + 8.0);
                // (I6: the details scroll when the app's window is shorter than the card.)
                let body_h = (screen.bottom() - 6.0 - pos.y - BANNER_H - AVATAR_R - 8.0).max(80.0);
                let body = egui::ScrollArea::vertical().id_salt("profile_popout_body").max_height(body_h).auto_shrink([false, true]).show(ui, |ui| {
                egui::Frame::new().inner_margin(egui::Margin { left: 16, right: 16, top: 0, bottom: 14 }).show(ui, |ui| {
                    ui.set_width(POPOUT_WIDTH - 32.0);
                    // Name, badge, username.
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui.label(crate::roles::role_name(user.shown_name(), role, Some(theme::GIZ)).font(theme::display(19.0)));
                        crate::roles::role_badge(ui, role);
                    });
                    ui.label(RichText::new(format!("@{}", user.username)).font(theme::body(13.0)).color(theme::NEVOA));
                    // Presence, then the custom status.
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                        ui.painter().circle_filled(r.center(), 4.0, dot_color);
                        let text = match (online, me, presence) {
                            (false, _, _) => "Offline".to_string(),
                            (true, true, Presence::Invisible) => "Invisível (só você sabe)".to_string(),
                            (true, _, p) => presence_label(p).to_string(),
                        };
                        ui.label(RichText::new(text).font(theme::body(12.5)).color(theme::NEVOA));
                    });
                    if let Some(s) = &status {
                        ui.add_space(2.0);
                        egui::Frame::new().fill(theme::MOVEL).corner_radius(theme::radius::BUTTON).inner_margin(egui::Margin::symmetric(10, 6)).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.add(egui::Label::new(RichText::new(status_line(s)).color(theme::TEXTO)).wrap());
                        });
                    }
                    ui.add_space(space::M);
                    // The inner card: about, role, member since.
                    egui::Frame::new().fill(theme::NOITE).corner_radius(theme::radius::BUTTON).inner_margin(egui::Margin::symmetric(12, 10)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 4.0;
                        let heading = |ui: &mut egui::Ui, t: &str| {
                            ui.label(RichText::new(t).font(theme::semibold(12.0)).color(theme::NEVOA));
                        };
                        if loading {
                            ui_kit::hint(ui, "Carregando perfil...");
                        } else if failed {
                            ui.label(RichText::new("Não foi possível carregar o perfil.").font(theme::body(12.5)).color(theme::AVISO));
                        }
                        if let Some(p) = &cached {
                            if !p.bio.is_empty() {
                                heading(ui, "Sobre mim");
                                crate::links::message(ui, &p.bio);
                                ui.add_space(space::S);
                            } else if me {
                                heading(ui, "Sobre mim");
                                ui_kit::hint(ui, "Você ainda não escreveu nada em \"Sobre mim\".");
                                ui.add_space(space::S);
                            }
                        }
                        if roles_on {
                            heading(ui, "Cargo");
                            ui.horizontal(|ui| {
                                let color = crate::roles::role_color(role).unwrap_or(theme::NEVOA);
                                let icon = crate::roles::role_icon(role).map(|i| format!("{i}  ")).unwrap_or_default();
                                ui_kit::pill(ui, &format!("{icon}{}", crate::roles::role_label(role)), theme::MOVEL, color);
                            });
                            ui.add_space(space::S);
                        }
                        if let Some(p) = cached.as_ref().filter(|p| p.member_since_ms > 0) {
                            heading(ui, "Membro desde");
                            ui.label(RichText::new(format_date_pt(p.member_since_ms, offset)).font(theme::body(13.0)).color(theme::TEXTO));
                        }
                    });
                    ui.add_space(space::L);
                    // The actions.
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
                        if me {
                            if ui_kit::button(ui, Kind::Primary, Some(ph::PENCIL_SIMPLE), "Editar perfil").clicked() {
                                edit = true;
                            }
                            return;
                        }
                        if facts.dms_ok && ui_kit::button(ui, Kind::Primary, Some(ph::CHAT_CIRCLE), "Mensagem").on_hover_text("Enviar uma mensagem direta").clicked() {
                            dm = true;
                        }
                        if can_call {
                            let r = ui.add_enabled_ui(call_why.is_none(), |ui| ui_kit::button(ui, Kind::Secondary, Some(ph::PHONE), "Ligar")).inner;
                            let r = match call_why {
                                Some(w) => r.on_disabled_hover_text(w),
                                None => r.on_hover_text("Uma chamada só entre vocês dois"),
                            };
                            if r.clicked() {
                                call = true;
                            }
                        }
                        if facts.same_room {
                            let kind = if volume_open { Kind::Secondary } else { Kind::Ghost };
                            if ui_kit::button(ui, kind, Some(ph::SPEAKER_HIGH), "Volume").on_hover_text("O volume desta pessoa, só para você").clicked() {
                                volume_open = !volume_open;
                            }
                        }
                    });
                    if volume_open && facts.same_room {
                        self.popout_volume(ui, &user);
                    }
                    if can_clear {
                        if let Some(p) = &cached {
                            let (has_bio, has_status, has_name) = (!p.bio.is_empty(), status.is_some(), user.display_name.is_some());
                            if has_bio || has_status || has_name {
                                ui.add_space(space::M);
                                ui.label(RichText::new("Moderação").font(theme::semibold(12.0)).color(theme::NEVOA));
                                ui.horizontal_wrapped(|ui| {
                                    ui.spacing_mut().item_spacing = Vec2::new(4.0, 4.0);
                                    let b = |ui: &mut egui::Ui, text: &str| ui_kit::button_sized(ui, Kind::Ghost, ui_kit::Size::Small, Some(ph::TRASH), text).clicked();
                                    if has_bio && b(ui, "Limpar sobre mim") {
                                        clear.bio = true;
                                    }
                                    if has_status && b(ui, "Limpar status") {
                                        clear.status = true;
                                    }
                                    if has_name && b(ui, "Limpar nome") {
                                        clear.display_name = true;
                                    }
                                });
                            }
                        }
                    }
                });
                });
                // The height the card wants: everything laid out, also what the window clips (and
                // what its details scroll: the card moves up for them first).
                let natural = ui.min_rect().height() - body.inner_rect.height() + body.content_size.y;
                if ui.ctx().data(|d| d.get_temp::<f32>(natural_id)) != Some(natural) {
                    ui.ctx().data_mut(|d| d.insert_temp(natural_id, natural));
                    ui.ctx().request_repaint();
                }
            });
        if let Some(r) = shown {
            self.profile.popout_drawn = Some((user.id, cached.is_some()));
            let rect = r.response.rect;
            let p = self.profile.popout.as_mut().expect("open");
            p.volume = volume_open;
            let just_opened = std::mem::take(&mut p.just_opened);
            if !just_opened && self.cmenu.open.is_none() && ctx.input(|i| i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|p| !rect.contains(p))) {
                close = true;
            }
        }
        if let Some(p) = menu_at {
            self.open_person_menu(ctx, &user, crate::context_menu::Origin::Popout, crate::context_menu::Lead::None, p);
        }
        if !clear.is_empty() {
            self.backend.send(Cmd::Profile(ProfileCmd::ModClear { user: user.id, what: clear }));
        }
        if dm {
            close = true;
            self.open_dm(user.id);
        }
        if call {
            close = true;
            self.call_user(user.id);
        }
        if edit {
            close = true;
            self.open_settings();
        }
        if close {
            self.close_profile();
        }
    }

    /// The card's "Volume": the person's volume for me (0-200%) and "Silenciar para mim".
    fn popout_volume(&mut self, ui: &mut egui::Ui, user: &User) {
        let before = self.settings.user_audio(&user.username);
        let mut pref = before;
        ui.add_space(space::S);
        let slider = ui
            .horizontal(|ui| {
                ui.label(RichText::new(ph::SPEAKER_HIGH).font(theme::body(16.0)).color(theme::NEVOA));
                ui.spacing_mut().slider_width = (ui.available_width() - 64.0).max(100.0);
                ui.add(egui::Slider::new(&mut pref.volume_pct, 0..=200).suffix("%"))
            })
            .inner;
        let muted = ui.checkbox(&mut pref.muted, "Silenciar para mim").on_hover_text("Só você deixa de ouvir esta pessoa").changed();
        if pref != before {
            self.set_user_pref(user, pref, muted || !slider.dragged());
        }
        if slider.drag_stopped() {
            self.save_settings();
        }
    }

    // ------------------------------------------------------------ the settings section

    /// "Perfil" in the settings ("Conta e perfil"), in the forms' rows: the display name, "Sobre
    /// mim", the profile colour, the custom status and the presence. Returns whether it was shown
    /// (an older hub has no profiles).
    pub fn profile_section(&mut self, ui: &mut egui::Ui) -> bool {
        use crate::forms;
        if self.profile.supported != Some(true) {
            return false;
        }
        let me = self.me.clone();
        forms::group(ui, "Perfil");
        if let Some(me) = &me {
            forms::note(ui, format!("Seu nome de usuário (para entrar) é @{}. O resto só muda o que as pessoas veem.", me.username), false);
            ui.add_space(space::S);
        }
        let dirty = self.profile.form.changes().is_some();
        let mut save = false;
        let mut see = false;
        let busy = self.profile.form.busy;
        let field_w = 300.0;
        {
            let f = &mut self.profile.form;
            forms::row(ui, "Nome de exibição", |ui| {
                ui.add(ui_kit::field(&mut f.display_name).char_limit(DISPLAY_NAME_MAX).hint_text("opcional").desired_width(field_w));
            });
            forms::row(ui, "Sobre mim", |ui| {
                ui.vertical(|ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut f.bio)
                            .char_limit(BIO_MAX)
                            .desired_rows(3)
                            .desired_width(field_w)
                            .margin(egui::Margin::symmetric(10, 7))
                            .hint_text("conte algo sobre você (links funcionam)"),
                    );
                    ui.label(RichText::new(format!("{}/{BIO_MAX}", f.bio.chars().count())).font(theme::body(12.0)).color(theme::NEVOA));
                });
            });
            forms::row(ui, "Cor do perfil", |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let mut rgb = f.accent.unwrap_or([88, 101, 242]);
                    if ui.color_edit_button_srgb(&mut rgb).on_hover_text("Outra cor").changed() {
                        f.accent = Some(rgb);
                    }
                    for s in SWATCHES {
                        let (r, resp) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::click());
                        ui.painter().rect_filled(r, theme::radius::TAG, Color32::from_rgb(s[0], s[1], s[2]));
                        if f.accent == Some(s) {
                            ui.painter().rect_stroke(r.expand(1.5), theme::radius::TAG, egui::Stroke::new(2.0, theme::GIZ), egui::StrokeKind::Outside);
                        }
                        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            f.accent = Some(s);
                        }
                    }
                    if ui_kit::button_sized(ui, Kind::Ghost, ui_kit::Size::Small, None, "Sem cor").clicked() {
                        f.accent = None;
                    }
                });
            });
        }
        forms::indented(ui, |ui| {
            let label = if busy { "Salvando..." } else { "Salvar perfil" };
            save = ui.add_enabled_ui(dirty && !busy, |ui| ui_kit::button(ui, Kind::Primary, None, label)).inner.clicked();
            see = ui_kit::button(ui, Kind::Secondary, Some(ph::USER), "Ver meu cartão").clicked();
            if dirty && !busy {
                ui.label(RichText::new("há mudanças não salvas").font(theme::body(12.5)).color(theme::AVISO));
            }
        });
        if let Some((ok, text)) = &self.profile.form.msg {
            forms::indented(ui, |ui| ui_kit::result_text(ui, *ok, text));
        }
        if let Some(p) = self.profile.cache.get(&self.my_id().unwrap_or_default()) {
            if p.profile.member_since_ms > 0 {
                forms::note(ui, format!("Membro desde {}", format_date_pt(p.profile.member_since_ms, local_offset_min())), true);
            }
        }

        // Custom status.
        forms::divider(ui);
        forms::group(ui, "Status personalizado");
        let current = self.my_id().and_then(|id| self.status_of(id));
        if let Some(s) = &current {
            forms::note(ui, format!("No ar agora: {}", status_line(s)), false);
            ui.add_space(space::S);
        }
        let (mut set, mut clear) = (false, false);
        {
            let f = &mut self.profile.form;
            forms::row(ui, "Status", |ui| {
                ui.add(ui_kit::field(&mut f.status_emoji).char_limit(STATUS_EMOJI_MAX_CHARS).hint_text("emoji").desired_width(56.0));
                ui.add(ui_kit::field(&mut f.status_text).char_limit(STATUS_TEXT_MAX).hint_text("o que você está fazendo").desired_width(field_w - 64.0));
            });
            forms::indented(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    for e in QUICK_EMOJI {
                        if ui_kit::button_sized(ui, Kind::Ghost, ui_kit::Size::Small, None, e).clicked() {
                            f.status_emoji = e.to_string();
                        }
                    }
                });
            });
            forms::row(ui, "Limpar em", |ui| {
                egui::ComboBox::from_id_salt("status_clear_after").width(180.0).selected_text(f.clear_after.label()).show_ui(ui, |ui| {
                    for c in ClearAfter::ALL {
                        ui.selectable_value(&mut f.clear_after, c, c.label());
                    }
                });
            });
            forms::indented(ui, |ui| {
                let can = !f.status_busy && !(f.status_emoji.trim().is_empty() && f.status_text.trim().is_empty());
                set = ui.add_enabled_ui(can, |ui| ui_kit::button(ui, Kind::Secondary, None, "Definir status")).inner.clicked();
                clear = ui.add_enabled_ui(!f.status_busy && current.is_some(), |ui| ui_kit::button(ui, Kind::Ghost, None, "Limpar status")).inner.clicked();
            });
            if let Some((ok, text)) = &f.status_msg {
                forms::indented(ui, |ui| ui_kit::result_text(ui, *ok, text));
            }
        }

        // Presence.
        forms::divider(ui);
        forms::group(ui, "Presença");
        let current = self.my_presence();
        let mut choose = None;
        forms::row(ui, "Aparecer como", |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = space::S;
                for p in Presence::ALL {
                    let r = presence_chip(ui, current == p, p).on_hover_text(presence_hint(p));
                    if r.clicked() {
                        choose = Some(p);
                    }
                }
            });
        });
        forms::note(ui, presence_hint(current), true);
        let mut minutes = self.settings.profile.idle_away_minutes;
        forms::row(ui, "Ausente automático", |ui| {
            ui.add(egui::DragValue::new(&mut minutes).range(0..=240).suffix(" min"));
        });
        forms::note(ui, "Depois desse tempo sem mexer no PC (0 = nunca; em chamada você não fica ausente).", true);
        if minutes != self.settings.profile.idle_away_minutes {
            self.settings.profile.idle_away_minutes = minutes;
            self.save_settings();
        }

        if save {
            self.save_profile();
        }
        if set {
            self.save_status();
        }
        if clear {
            self.clear_status();
        }
        if let Some(p) = choose.filter(|p| *p != current) {
            self.set_presence(p);
        }
        if see {
            if let Some(me) = me {
                let center = ui.ctx().content_rect().center();
                self.open_profile_at(&me, center - Vec2::new(POPOUT_WIDTH / 2.0, 120.0));
            }
        }
        ui.add_space(space::M);
        true
    }
}

/// A presence choice: a pill with the presence's dot (the chosen one raised, like the forms'
/// chips).
fn presence_chip(ui: &mut egui::Ui, selected: bool, p: Presence) -> egui::Response {
    let text = presence_label(p);
    let g = ui.painter().layout_no_wrap(text.to_owned(), theme::semibold(theme::size::BUTTON), Color32::PLACEHOLDER);
    let s = Vec2::new(g.size().x + 2.0 * space::L + 14.0, 28.0);
    let (rect, resp) = ui.allocate_exact_size(s, Sense::click());
    if ui.is_rect_visible(rect) {
        let (fill, fg, line) = if selected {
            (theme::MOVEL, theme::GIZ, theme::GIZ)
        } else if resp.hovered() {
            (theme::ROW_HOVER, theme::GIZ, theme::RISCO)
        } else {
            (Color32::TRANSPARENT, theme::NEVOA, theme::RISCO)
        };
        let painter = ui.painter();
        painter.rect(rect, theme::radius::PILL, fill, egui::Stroke::new(theme::stroke::HAIRLINE, line), egui::StrokeKind::Inside);
        painter.circle_filled(Pos2::new(rect.left() + space::L + 3.0, rect.center().y), 4.0, presence_color(p));
        painter.galley(Pos2::new(rect.left() + space::L + 14.0, rect.center().y - g.size().y / 2.0), g, fg);
    }
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, text));
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: i64, name: &str, display: Option<&str>) -> User {
        User { display_name: display.map(Into::into), ..User::new(id, name) }
    }

    #[test]
    fn people_are_named_by_display_name_with_the_username_as_fallback() {
        assert_eq!(user(1, "ana", Some("Ana Maria")).shown_name(), "Ana Maria");
        assert_eq!(user(1, "ana", None).shown_name(), "ana");
        assert_eq!(user(1, "ana", Some("  ")).shown_name(), "ana", "a blank display name is no name");
        assert!(user(1, "ana", Some("Ana")).has_display_name() && !user(1, "ana", None).has_display_name());
        // The username is what the hover and the card's "@" show, whatever the display name is.
        assert_eq!(format!("@{}", user(1, "ana", Some("Ana")).username), "@ana");
    }

    #[test]
    fn statuses_expire_on_the_clock() {
        let s = CustomStatus { emoji: "🎮".into(), text: "jogando".into(), expires_at_ms: Some(10_000) };
        assert!(live_status(Some(&s), 9_999).is_some());
        assert!(live_status(Some(&s), 10_000).is_none(), "gone at its time");
        assert!(live_status(Some(&s), 99_999).is_none());
        let forever = CustomStatus { text: "x".into(), ..Default::default() };
        assert!(live_status(Some(&forever), i64::MAX).is_some());
        assert!(live_status(Some(&CustomStatus::default()), 0).is_none(), "an empty status is none");
        assert!(live_status(None, 0).is_none());
        assert_eq!(status_line(&s), "🎮 jogando");
        assert_eq!(status_line(&CustomStatus { emoji: "🎮".into(), ..Default::default() }), "🎮");
        assert_eq!(status_line(&forever), "x");
    }

    #[test]
    fn clear_after_choices() {
        // Wednesday 2026-10-14 15:30:00 UTC; at UTC-3 it is 12:30, so 11.5 h to local midnight.
        let now = 1_791_991_800_000;
        assert_eq!(ClearAfter::Never.secs(now, -180), None);
        assert_eq!(ClearAfter::Min30.secs(now, -180), Some(1800));
        assert_eq!(ClearAfter::Hour1.secs(now, -180), Some(3600));
        assert_eq!(ClearAfter::Hours4.secs(now, -180), Some(14_400));
        assert_eq!(ClearAfter::Today.secs(now, -180), Some(11 * 3600 + 30 * 60));
        assert_eq!(secs_until_local_midnight(now, 0), 8 * 3600 + 30 * 60);
        // 23:59:30 local: "Hoje" still lasts at least a minute.
        let late = 1_791_936_000_000 + 86_400_000 + (2 * 3600 + 59 * 60 + 30) * 1000; // 02:59:30 UTC on the 15th
        let left = secs_until_local_midnight(late, -180);
        assert_eq!(left, 30);
        assert_eq!(ClearAfter::Today.secs(late, -180), Some(60));
        assert_eq!(ClearAfter::ALL.len(), 5);
    }

    #[test]
    fn dates_are_written_in_portuguese() {
        assert_eq!(format_date_pt(0, 0), "1 de janeiro de 1970");
        assert_eq!(format_date_pt(1_791_991_800_000, 0), "14 de outubro de 2026");
        // Local time can be the day before UTC.
        assert_eq!(format_date_pt(1_791_936_000_000 + 60_000, -180), "13 de outubro de 2026");
        assert_eq!(format_date_pt(1_791_936_000_000 + 60_000, 0), "14 de outubro de 2026");
        assert_eq!(format_date_pt(951_782_400_000, 0), "29 de fevereiro de 2000");
    }

    #[test]
    fn idle_makes_you_away_and_activity_brings_you_back() {
        let min10 = 10 * 60;
        // Below the threshold: nothing. At it: away.
        assert_eq!(idle_action(min10 - 1, 10, false, Presence::Online, false), None);
        assert_eq!(idle_action(min10, 10, false, Presence::Online, false), Some(true));
        // Already sent: nothing more, until activity.
        assert_eq!(idle_action(min10 * 3, 10, false, Presence::Away, true), None);
        assert_eq!(idle_action(2, 10, false, Presence::Away, true), Some(false));
        // Never away: setting 0, or a call.
        assert_eq!(idle_action(100_000, 0, false, Presence::Online, false), None);
        assert_eq!(idle_action(100_000, 10, true, Presence::Online, false), None);
        // Joining a call while auto-away brings you back at once.
        assert_eq!(idle_action(100_000, 10, true, Presence::Away, true), Some(false));
        // A presence I chose is never overridden: not dnd, not invisible, not a manual away.
        for p in [Presence::Dnd, Presence::Invisible, Presence::Away] {
            assert_eq!(idle_action(100_000, 10, false, p, false), None, "{p:?}");
        }
        // Turning the feature off while away undoes it.
        assert_eq!(idle_action(100_000, 0, false, Presence::Away, true), Some(false));
    }

    #[test]
    fn the_form_sends_only_what_changed() {
        let mut f = Form::default();
        assert!(f.changes().is_none(), "nothing loaded: nothing to send");
        let base = UserProfile {
            user: user(1, "ana", Some("Ana")),
            bio: "oi".into(),
            accent: Some("#112233".into()),
            status: None,
            presence: Presence::Online,
            online: true,
            member_since_ms: 5,
        };
        f.fill(&base);
        assert_eq!((f.display_name.as_str(), f.bio.as_str(), f.accent), ("Ana", "oi", Some([0x11, 0x22, 0x33])));
        assert!(f.changes().is_none());
        f.display_name = "  Aninha ".into();
        assert_eq!(f.changes(), Some(ProfileUpdate { display_name: Some("Aninha".into()), ..Default::default() }));
        f.bio.clear();
        f.accent = None;
        f.display_name = "Ana".into();
        assert_eq!(f.changes(), Some(ProfileUpdate { bio: Some(String::new()), accent: Some(String::new()), ..Default::default() }));
        f.accent = Some([255, 128, 0]);
        f.bio = "oi".into();
        assert_eq!(f.changes().unwrap().accent.as_deref(), Some("#ff8000"));
        f.display_name.clear();
        assert_eq!(f.changes().unwrap().display_name, Some(String::new()), "clearing the display name is a change");
    }

    #[test]
    fn colours() {
        assert_eq!(accent_color(Some("#ff8800")), Some(Color32::from_rgb(255, 136, 0)));
        assert_eq!(accent_color(Some("nope")), None);
        assert_eq!(accent_color(None), None);
        assert_eq!(hex_of([1, 2, 255]), "#0102ff");
        assert!(SWATCHES.iter().all(|s| accent_rgb(&hex_of(*s)) == Some(*s)));
    }

    #[test]
    fn hub_refusals_are_explained_in_portuguese() {
        let err = |status: u16, message: &str| error_text(&anyhow::Error::new(HttpStatusError { status, message: message.into() }));
        assert!(err(400, "display name must be 1-32 characters").contains("1 a 32"));
        assert!(err(400, "bio must be at most 190 characters and 8 lines").contains("190"));
        assert!(err(400, "colour must look like #rrggbb").contains("#rrggbb"));
        assert!(err(400, "status emoji must be a single emoji").contains("emoji"));
        assert!(err(409, "that name is somebody else's username").contains("outra pessoa"));
        assert!(err(429, "too many profile changes, wait 20 s").contains("20 s"));
        assert!(err(429, "too many profile changes, wait 90 s").contains("2 min"));
        assert!(err(404, "").contains("versão antiga"));
        assert!(err(403, "").contains("cargo"));
    }

    #[test]
    fn every_presence_has_a_label_and_a_distinct_colour() {
        let colours: std::collections::HashSet<_> = Presence::ALL.iter().map(|p| presence_color(*p)).collect();
        assert_eq!(colours.len(), 4);
        assert_eq!(Presence::ALL.map(presence_label), ["Online", "Ausente", "Não perturbe", "Invisível"]);
        assert!(Presence::ALL.iter().all(|p| !presence_hint(*p).is_empty()));
    }

    #[test]
    fn the_settings_default_to_ten_minutes_and_old_files_get_it() {
        assert_eq!(ProfileSettings::default().idle_away_minutes, 10);
        let s: ProfileSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.idle_away_minutes, 10);
        let s: ProfileSettings = serde_json::from_str(r#"{"idle_away_minutes":0}"#).unwrap();
        assert_eq!(s.idle_away_minutes, 0);
    }
}
