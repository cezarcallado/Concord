//! Hidden `--autotest <role>` mode: a script that presses the same buttons (the `App` action
//! methods) a person would, so the whole stack can be exercised unattended. Every step is
//! logged as `AUTOTEST ...`; the process exits 0 on success and 1 on failure.
//!
//! Roles: `a` (talks, joins voice, shares a window), `b` (answers, joins, watches A),
//! `idle` (connect and wait), `voice` (connect, join the first voice channel and wait),
//! `cam-a` / `cam-b` (item 11: A turns the camera on, B sees and checks the tile),
//! `multi-a` (twice: A1 and A2 each share a window) / `multi-b` (item 9: B watches both at once),
//! `audio-a` / `audio-b` (audio-only Go Live: A shares just a tone or an app's sound, B listens),
//! `menu-a` / `menu-b` (v0.6: the right-click menus of people and streams),
//! `audio-c` (a third client that never joins voice and must still see A's live as audio-only).

use std::time::{Duration, Instant};

use eframe::egui;
use vox_proto::ChannelKind;

use crate::app::{App, Center, Pick, Screen, Share};
use crate::backend::Cmd;
use crate::call::TileKey;
use crate::settings::Preset;

/// How long role A (`a` / `cam-a`) keeps going past its hold while waiting for B to finish
/// its checks (B's "done" signal), before the run fails.
const B_DONE_GRACE_SECS: u64 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    A,
    B,
    Idle,
    Voice,
    /// Mic test (alone, then while in voice) and the input meter / gate, with the real mic.
    MicTest,
    /// Webcam: A turns the camera on (and off), B checks the tile, pause / resume and clearing.
    CamA,
    CamB,
    /// Item 9: several streams. Two `multi-a` instances (A1, A2) each share a window; `multi-b`
    /// watches both at once.
    MultiA,
    MultiB,
    /// Audio-only Go Live ("Só áudio"): A shares just audio, B listens (see `AoState`).
    AudioA,
    AudioB,
    /// Audio-only Go Live seen from outside the voice room: logged in, never joins voice.
    AudioC,
    /// Item 12: the owner A sets its personal password, server-mutes, moves and unmutes member B;
    /// B checks that its own UI follows (see `autotest_roles.rs`).
    ModA,
    ModB,
    /// Profile pictures: A sets a generated picture and removes it again, B checks that it shows
    /// on the call tile, the sidebar, the online list and the chat (see `autotest_avatars.rs`).
    AvatarA,
    AvatarB,
    /// Profiles: A edits its profile and status, goes away (idle), invisible and back; B checks A's
    /// card and presence (see `autotest_profiles.rs`).
    ProfileA,
    ProfileB,
    /// Changing the microphone mid-call, with fake devices: A switches, unplugs and replugs, B
    /// must keep hearing it (see `autotest_micswitch.rs`).
    MicSwitchA,
    MicSwitchB,
    /// Soundboard: A uploads sounds and plays them, B checks its own mixer, the deafen and mute
    /// gates and the permissions (see `autotest_sounds.rs`).
    SoundA,
    SoundB,
    /// Soundboard trim editor: one process picks a 3.2 s part of a generated 20 s file, previews it
    /// (silently) and uploads it; a too long file is refused (see `autotest_sounds.rs`).
    SoundTrim,
    /// Direct messages and a private call: A writes to B, B replies, A calls, B answers, both are
    /// in the private room, B hears A's tone live, A hangs up (see `autotest_dms.rs`).
    DmA,
    DmB,
    /// The right-click menu of people and streams: A opens B's menus (sidebar, stream card, camera
    /// card), runs the actions and checks the state, then its own entry and the preview switch
    /// (see `autotest_menu.rs`).
    MenuA,
    MenuB,
    /// Chat images: A pastes a generated screenshot and drops a generated photo, sends them; B
    /// sees both thumbnails and opens each in the viewer (see `autotest_images.rs`).
    ImgA,
    ImgB,
    /// Chat notifications and the tray: A writes in a channel and in a DM, B counts, sounds,
    /// notifies and hides in the tray, C is a bystander (see `autotest_notify.rs`).
    NotifyA,
    NotifyB,
    NotifyC,
    /// Mentions, replies and reactions (v0.8): A mentions B from the member list, B replies, both
    /// react, in a channel and a DM; deletes show in the quotes (see `autotest_chatx.rs`).
    ChatxA,
    ChatxB,
    /// Host mode and invites (v0.8): A hosts a server inside its app and makes an invite, B joins
    /// with only the code, chats and joins voice, A revokes, C is refused (see `autotest_host.rs`).
    HostA,
    HostB,
    HostC,
    /// "Configurar câmera" (v0.8.1): the window with the synthetic camera, a control changed,
    /// applied and saved, "Priorizar fluidez", "Restaurar padrão" (see `autotest_camwin.rs`).
    CamWin,
    /// v0.8.2 fixes: deleting my own messages (with images, channel and DM), a new picture after
    /// the newest was deleted shows its own thumbnail, a deleted and recreated channel starts
    /// empty, and the sidebar badges never cover the name (see `autotest_fix.rs`).
    FixA,
    FixB,
    /// Private channels (v0.9): the owner A makes a text and a voice channel private for member B;
    /// member C sees nothing of them and is refused their voice room; B chats and joins voice; A
    /// takes B off and B loses both live (see `autotest_private.rs`).
    PrivA,
    PrivB,
    PrivC,
    /// The saved server list (v0.9.1): migration, saving, one click, "Trocar de servidor", the
    /// form asking only for what is missing, removal (see `autotest_srvlist.rs`). Two runs of the
    /// same role (`--mode phase1` / `phase2`) share a `--settings-dir`.
    SrvList,
    /// v1.0 design review: the `ui_kit` gallery alone (`kit`) and the main screens one after the
    /// other (`shots`), saved with `VOX_AUTOTEST_SHOT_DIR` (see `autotest_shots.rs`).
    Kit,
    Shots,
    /// v1.0 phase 3a: the secondary windows (each settings section, Configurar câmera, the
    /// soundboard and its trim editor, the voice changer, the Go Live picker), saved the same way.
    Shots3,
    /// v1.0 navigation and the center modes (see `autotest_nav.rs`): `nav-a` (the owner) checks the
    /// sidebar highlight, the three modes and the docked chat's read marking; `nav-b` answers.
    NavA,
    NavB,
    /// v1.0 call stage (see `autotest_stage.rs`): B pins two streams and a camera, hides a card
    /// without video, enters and leaves the stage fullscreen; the video windows must follow the
    /// cards and keep playing. Partners: two `multi-a` and one `cam-a`.
    StageB,
    /// v1.0 phase 3b: the server window's pages, the menus, the profile card and editor, the DM
    /// header, the incoming call, a toast and the dialogs (`shots3b-a` with `shots3b-b`), and the
    /// host panel (`shots3b-host`), saved the same way (see `autotest_shots3b.rs`).
    Shots3bA,
    Shots3bB,
    Shots3bHost,
    /// v1.0.1: the concord home screen and the Calladex easter egg (see `autotest_shots.rs`):
    /// "calladex" typed on the home screen opens the credits; in a channel, `/calladex` opens the
    /// card and is not sent, while a message that only contains the word is; then Configurações →
    /// Sobre. Partners: one or more `idle` clients (the members on the card).
    Calladex,
    /// v1.1 interface fixes (see `autotest_uix.rs`): A checks that the sidebar and every chat
    /// scroll in the smallest window, the speaking ring, the message and text box menus; B sits
    /// in the voice room.
    UixA,
    UixB,
    /// v1.1 D1: the Pix donation popups and page (see `autotest_donate.rs`; only in a build with
    /// the `donate-test` feature).
    #[cfg(feature = "donate-test")]
    Donate,
    /// v1.1 keyboard shortcuts (see `autotest_keys.rs`): synthetic key events for deafen, mute,
    /// push-to-talk, a missing sound, Go Live and leave, straight into the shortcut matcher.
    KeysA,
    /// v1.1 Go Live hover cards (see `autotest_thumbs.rs`): A streams the test window, B and C
    /// see its still picture and app from outside the room; in a private room only B does.
    ThumbsA,
    ThumbsB,
    ThumbsC,
    /// v1.1 colour emoji (see `autotest_emoji.rs`): message text, chips, quick reactions, picker.
    EmojiA,
    /// v1.1 T0 TLS (see `autotest_tls.rs`): invite with the pin, chat and voice through the tunnel
    /// (`tls-a` / `tls-b`), another certificate at a saved address (`tls-mitm`), an older hub
    /// (`tls-oldhub`).
    TlsA,
    TlsB,
    TlsMitm,
    TlsOldHub,
    /// v1.1 E0 per-device identity keys (see `autotest_e2ee.rs`): A and B register, a second
    /// device of A appears, A gets the alert and removes it from "Seus aparelhos".
    E2eeA,
    E2eeA2,
    E2eeB,
    /// v1.1 E2 end-to-end encrypted DMs (see `autotest_e2dm.rs`): A sends text and a photo, B
    /// opens and draws them and replies, A's second computer reads A's own lines, a new computer
    /// of A sees "antes deste aparelho existir", B verifies the safety code and sees it change.
    E2dmA,
    E2dmB,
    E2dmA2,
    E2dmNew,
    /// v1.1 E1 end-to-end encrypted calls (see `autotest_e2v.rs`): A turns the room's encryption
    /// on, joins with tone, camera and a pre-encoded live; B checks everything decodes and that the
    /// keys rotate without a gap while C comes and goes; `e2v-admin` turns it on for `e2v-old`.
    E2vA,
    E2vB,
    E2vC,
    E2vAdmin,
    /// v1.1 macOS (M2): watch a stream drawn by the app itself, pinned, fullscreen, popped out and
    /// back (see `autotest_macview.rs`). Partner: anyone live (`vox-cli share h264file`).
    MacView,
    /// v1.1 I6 responsive layout (see `autotest_resp.rs`): every screen at several window sizes
    /// and zoom levels, checking the layout rules (`resp-a`); `resp-b` streams or shows a camera.
    RespA,
    RespB,
}

#[path = "autotest_roles.rs"]
mod item12;

#[path = "autotest_avatars.rs"]
mod avatar;

#[path = "autotest_profiles.rs"]
mod profiles;

#[path = "autotest_sounds.rs"]
mod sound;

#[path = "autotest_dms.rs"]
mod dmtest;

#[path = "autotest_menu.rs"]
mod menu;

#[path = "autotest_camcfg.rs"]
mod camcfg;

#[path = "autotest_clip.rs"]
mod cliptest;

#[path = "autotest_images.rs"]
mod imgtest;

#[path = "autotest_notify.rs"]
mod nttest;

#[path = "autotest_chatx.rs"]
mod chatx;

#[path = "autotest_host.rs"]
mod hosttest;

#[path = "autotest_camwin.rs"]
mod camwin;
#[path = "autotest_fix.rs"]
mod fixtest;
#[path = "autotest_private.rs"]
mod privtest;
#[path = "autotest_srvlist.rs"]
mod srvlist;
#[path = "autotest_shots.rs"]
pub mod shots;
#[path = "autotest_nav.rs"]
mod navtest;
#[path = "autotest_stage.rs"]
mod stagetest;
#[path = "autotest_shots3b.rs"]
mod shots3b;
#[path = "autotest_macview.rs"]
mod macview;
#[path = "autotest_uix.rs"]
mod uixtest;
#[cfg(feature = "donate-test")]
#[path = "autotest_donate.rs"]
mod donatetest;
#[path = "autotest_keys.rs"]
mod keystest;
#[path = "autotest_thumbs.rs"]
mod thumbstest;
#[path = "autotest_tls.rs"]
mod tlstest;
#[path = "autotest_e2ee.rs"]
mod e2eetest;
#[path = "autotest_e2dm.rs"]
mod e2dmtest;
#[path = "autotest_e2v.rs"]
mod e2vtest;
#[path = "autotest_emoji.rs"]
mod emojitest;
#[path = "autotest_micswitch.rs"]
mod micswitch;
#[path = "autotest_resp.rs"]
mod resp;

/// Command-line parameters of an autotest run.
#[derive(Default)]
pub struct AutoArgs {
    pub server: String,
    pub name: String,
    pub password: String,
    /// Window to share (role `a`).
    pub hwnd: Option<isize>,
    /// Role `a`: halfway through the hold, switch the live to this window ("Trocar tela").
    pub switch_hwnd: Option<isize>,
    /// Seconds role `a` keeps the Go Live running.
    pub hold_secs: u64,
    /// Makes the chat messages of one test run unique (old runs stay in the hub's history).
    pub run_id: String,
    /// `golive` / `settings`: open that dialog once connected (for layout checks). Role `b`:
    /// `settings` opens the settings dialog over the inline stream (airspace check).
    pub show: Option<String>,
    /// Role `b`: the "Abrir transmissões em janela separada" setting is on.
    pub popout: bool,
    /// Role `b`: seconds to keep watching after the checks (CPU / RAM measurements).
    pub watch_secs: u64,
    /// Role `a`: run the item-8a checks on the preview of my own live (needs `--switch-hwnd`
    /// for the source-switch part; ignores `--hold`).
    pub preview: bool,
    /// Role `a`: CPU measurement run: `chat` (tile hidden, preview off), `grid` (5 fps) or
    /// `focus` (15 fps) for `--hold` seconds, no checks.
    pub pv_mode: Option<String>,
    /// Role `multi-b`: `checks` (default) or a measurement run of `--watch-secs` seconds: `one`
    /// (watch only A1), `two` (both, grid), `strip` (both, A1 focused) or `chat` (both, chat
    /// view shown).
    pub mode: Option<String>,
    /// Roles `audio-a` / `audio-b`: the process whose sound A shares (`--mode app`), the one it
    /// switches to halfway ("Trocar app"), Alta (256 kbps) instead of Normal, and whether A
    /// also turns into a screen share and back (with `--switch-hwnd`).
    pub audio_pid: Option<u32>,
    pub switch_pid: Option<u32>,
    pub audio_high: bool,
    pub kinds: bool,
}

pub struct Auto {
    pub role: Role,
    args: AutoArgs,
    step: u32,
    step_at: Instant,
    started: Instant,
    seq_seen: u64,
    frames_first: u64,
    finished: bool,
    /// v1.0 idle check: until then the autotest does not tick (the app is measured at rest), and
    /// the frame count when it started.
    quiet_until: Option<Instant>,
    quiet_frames: u64,
    /// Mic test role: loudest level / gate-open seen in the current phase.
    lvl_max: f32,
    lvl_min: f32,
    thr_last: f32,
    opened: bool,
    last_line: Option<Instant>,
    change_checked: bool,
    /// "Trocar tela" requested / confirmed by the app (role `a`).
    switch_sent: bool,
    switch_done: bool,
    /// Role `a`: B showed up in my viewer list / left it again (item 7).
    viewer_seen: bool,
    viewer_cleared: bool,
    /// Webcam roles.
    stats_req: bool,
    cam_of: Option<String>,
    cam_frames: Vec<u64>,
    grid_rect: Option<(i32, i32, i32, i32)>,
    self_view_checked: bool,
    cam_stats_logged: bool,
    /// Preview checks (role `a --preview`): last frame count / time / tile video size seen.
    pv_frames: u64,
    pv_at: Instant,
    pv_video: (u32, u32),
    pv_sizes: Vec<(u32, u32)>,
    /// Role `b`, fullscreen checks: the app window / pop-out before fullscreen.
    win_rect: Option<win::Rect>,
    /// Role `multi-b`: the two streamers (by username) and per-stream baselines.
    mb_ids: Vec<String>,
    mb_frames: Vec<u64>,
    mb_audio: Vec<u64>,
    mb_presented: Vec<u64>,
    mb_at: Instant,
    /// Roles `audio-a` / `audio-b`.
    ao: AoState,
    /// Roles `mod-a` / `mod-b` (item 12).
    item12: item12::State,
    /// Roles `avatar-a` / `avatar-b`.
    avatar: avatar::State,
    /// Roles `profile-a` / `profile-b`.
    profile: profiles::State,
    /// Roles `sound-a` / `sound-b`.
    sound: sound::State,
    /// Roles `dm-a` / `dm-b`.
    dm: dmtest::State,
    /// Roles `menu-a` / `menu-b`.
    menu: menu::State,
    /// Webcam configuration run inside `cam-a` / `cam-b` (see `autotest_camcfg.rs`).
    camcfg: camcfg::State,
    /// Airspace checks inside `cam-b` (see `autotest_clip.rs`).
    clip: cliptest::State,
    /// Roles `img-a` / `img-b`.
    img: imgtest::State,
    /// Roles `notify-a` / `notify-b` / `notify-c`.
    nt: nttest::State,
    /// Roles `chatx-a` / `chatx-b`.
    cx: chatx::State,
    /// Roles `host-a` / `host-b` / `host-c`.
    host: hosttest::State,
    /// Role `camwin`.
    camwin: camwin::State,
    /// Roles `fix-a` / `fix-b`.
    fx: fixtest::State,
    /// Roles `priv-a` / `priv-b` / `priv-c`.
    pv: privtest::State,
    /// Role `srvlist`.
    srv: srvlist::State,
    /// Roles `kit` / `shots`.
    shots: shots::State,
    /// Roles `nav-a` / `nav-b`.
    nav: navtest::State,
    /// Role `stage-b`.
    stage: stagetest::State,
    s3b: shots3b::State,
    /// Roles `uix-a` / `uix-b`.
    uix: uixtest::State,
    /// Role `keys-a`: the shortcuts run before the last presses.
    keys_base: usize,
    /// Roles `thumbs-a` / `thumbs-b` / `thumbs-c`.
    th: thumbstest::State,
    /// Roles `tls-a` / `tls-b` / `tls-mitm` / `tls-oldhub`.
    tls: tlstest::State,
    /// Roles `e2dm-a` / `e2dm-b` / `e2dm-a2` / `e2dm-new`.
    e2dm: e2dmtest::State,
    /// Roles `resp-a` / `resp-b`.
    resp: resp::State,
    /// Roles `e2v-a` / `e2v-b` / `e2v-c` / `e2v-admin`.
    e2v: e2vtest::State,
    /// Roles `micswitch-a` / `micswitch-b`.
    micsw: micswitch::State,
}

impl Auto {
    pub fn new(role: Role, args: AutoArgs) -> Self {
        Auto {
            role,
            args,
            step: 0,
            step_at: Instant::now(),
            started: Instant::now(),
            seq_seen: 0,
            frames_first: 0,
            finished: false,
            quiet_until: None,
            quiet_frames: 0,
            lvl_max: -100.0,
            lvl_min: 0.0,
            thr_last: 0.0,
            opened: false,
            last_line: None,
            change_checked: false,
            switch_sent: false,
            switch_done: false,
            viewer_seen: false,
            viewer_cleared: false,
            stats_req: false,
            cam_of: None,
            cam_frames: Vec::new(),
            grid_rect: None,
            self_view_checked: false,
            cam_stats_logged: false,
            camcfg: Default::default(),
            clip: Default::default(),
            pv_frames: 0,
            pv_at: Instant::now(),
            pv_video: (0, 0),
            pv_sizes: Vec::new(),
            win_rect: None,
            mb_ids: Vec::new(),
            mb_frames: Vec::new(),
            mb_audio: Vec::new(),
            mb_presented: Vec::new(),
            mb_at: Instant::now(),
            ao: AoState::default(),
            item12: item12::State::default(),
            avatar: avatar::State::default(),
            profile: profiles::State::default(),
            sound: sound::State::default(),
            dm: dmtest::State::default(),
            menu: menu::State::default(),
            img: imgtest::State::default(),
            nt: nttest::State::default(),
            cx: chatx::State::default(),
            host: hosttest::State::default(),
            camwin: camwin::State::default(),
            fx: fixtest::State::default(),
            pv: privtest::State::default(),
            srv: srvlist::State::default(),
            shots: shots::State::default(),
            nav: navtest::State::default(),
            stage: stagetest::State::default(),
            s3b: shots3b::State::default(),
            uix: uixtest::State::default(),
            keys_base: 0,
            th: thumbstest::State::default(),
            tls: tlstest::State::default(),
            e2dm: e2dmtest::State::default(),
            resp: resp::State::default(),
            e2v: e2vtest::State::default(),
            micsw: micswitch::State::default(),
        }
    }

    /// True once a stay-connected role has nothing left to script.
    fn resting(&self) -> bool {
        match self.role {
            // The picker scripts (item 8c) keep ticking until they are done (step 9).
            Role::Idle => self.step >= 2 && !(matches!(self.args.show.as_deref(), Some("golive-cycle") | Some("golive-pick") | Some("previews")) && self.step < 9),
            Role::Voice => self.step >= 8 && self.waited(3),
            // Hidden in the tray, idling: nothing ticks until the hub says something (CPU measurement).
            Role::NotifyB => self.step == nttest::REST_STEP,
            _ => false,
        }
    }

    fn goto(&mut self, step: u32, what: &str) {
        tracing::info!(target: "autotest", "AUTOTEST {:?} step {} -> {}: {} (t+{:.1}s)", self.role, self.step, step, what, self.started.elapsed().as_secs_f32());
        self.step = step;
        self.step_at = Instant::now();
    }

    /// v1.0 idle check: a quiet window of `secs` starts (the autotest stops ticking; one wake-up
    /// at its end). Anything else that draws a frame meanwhile is the app's own doing.
    fn quiet_start(&mut self, app: &App, secs: u64) {
        self.quiet_frames = app.repaint_ctx().cumulative_frame_nr();
        self.quiet_until = Some(Instant::now() + Duration::from_secs(secs));
    }

    /// The quiet window is over: `Some(frames drawn during it)`.
    fn quiet_done(&mut self, app: &App) -> Option<u64> {
        if self.quiet_until.is_some_and(|t| Instant::now() >= t) {
            self.quiet_until = None;
            Some(app.repaint_ctx().cumulative_frame_nr().saturating_sub(self.quiet_frames))
        } else {
            None
        }
    }

    fn waited(&self, secs: u64) -> bool {
        self.step_at.elapsed() >= Duration::from_secs(secs)
    }

    fn fail(&self, reason: &str) -> Option<i32> {
        tracing::error!(target: "autotest", "AUTOTEST {:?} RESULT: FAIL at step {}: {}", self.role, self.step, reason);
        Some(1)
    }

    fn pass(&self) -> Option<i32> {
        tracing::info!(target: "autotest", "AUTOTEST {:?} RESULT: PASS (t+{:.1}s)", self.role, self.started.elapsed().as_secs_f32());
        Some(0)
    }

    /// Returns `Some(exit code)` when finished.
    fn run(&mut self, app: &mut App) -> Option<i32> {
        if self.finished {
            return None;
        }
        self.answer_password_prompt(app);
        let done = self.run_role(app);
        self.finished = done.is_some();
        done
    }

    /// The hub's owner (the release gate's hubs name `dono` as `owner`, or, with none configured,
    /// the first login from 127.0.0.1 is the owner) gets "Defina sua senha
    /// pessoal" right after logging in. It is a centred window, so it covers the middle of the
    /// call view and the native video tiles under it are hidden on purpose. Roles that are not
    /// about item 12 answer it the way a user does, with "Agora não"; `mod-a` sets the password
    /// itself (and `--show prompt` keeps the prompt up for a look).
    fn answer_password_prompt(&mut self, app: &mut App) {
        if self.role == Role::ModA || app.roles.prompt_later || app.me.is_none() {
            return;
        }
        if app.roles.supported && app.roles.account.is_some_and(|a| a.must_set_password) {
            app.roles.prompt_later = true;
            tracing::info!(target: "autotest", "AUTOTEST {:?}: answered the owner's password prompt with \"Agora não\"", self.role);
        }
    }

    fn run_role(&mut self, app: &mut App) -> Option<i32> {
        match self.role {
            Role::A => self.run_a(app),
            Role::B => self.run_b(app),
            Role::Idle | Role::Voice => self.run_stay(app),
            Role::MicTest => self.run_mictest(app),
            Role::CamA => self.run_cam_a(app),
            Role::CamB => self.run_cam_b(app),
            Role::MultiA => self.run_multi_a(app),
            Role::MultiB => self.run_multi_b(app),
            Role::AudioA => self.run_audio_a(app),
            Role::AudioB => self.run_audio_b(app),
            Role::AudioC => self.run_audio_c(app),
            Role::ModA => self.run_mod_a(app),
            Role::ModB => self.run_mod_b(app),
            Role::AvatarA => self.run_avatar_a(app),
            Role::AvatarB => self.run_avatar_b(app),
            Role::ProfileA => self.run_profile_a(app),
            Role::ProfileB => self.run_profile_b(app),
            Role::MicSwitchA => self.run_micswitch_a(app),
            Role::MicSwitchB => self.run_micswitch_b(app),
            Role::SoundA => self.run_sound_a(app),
            Role::SoundB => self.run_sound_b(app),
            Role::SoundTrim => self.run_sound_trim(app),
            Role::DmA => self.run_dm_a(app),
            Role::DmB => self.run_dm_b(app),
            Role::MenuA => self.run_menu_a(app),
            Role::MenuB => self.run_menu_b(app),
            Role::ImgA => self.run_img_a(app),
            Role::ImgB => self.run_img_b(app),
            Role::NotifyA => self.run_notify_a(app),
            Role::NotifyB => self.run_notify_b(app),
            Role::NotifyC => self.run_notify_c(app),
            Role::ChatxA => self.run_chatx_a(app),
            Role::ChatxB => self.run_chatx_b(app),
            Role::HostA => self.run_host_a(app),
            Role::HostB => self.run_host_b(app),
            Role::HostC => self.run_host_c(app),
            Role::CamWin => self.run_camwin(app),
            Role::FixA => self.run_fix_a(app),
            Role::FixB => self.run_fix_b(app),
            Role::PrivA => self.run_priv_a(app),
            Role::PrivB => self.run_priv_b(app),
            Role::PrivC => self.run_priv_c(app),
            Role::SrvList => self.run_srvlist(app),
            Role::Kit => self.run_kit(app),
            Role::Shots => self.run_shots(app),
            Role::Shots3 => self.run_shots3(app),
            Role::NavA => self.run_nav_a(app),
            Role::NavB => self.run_nav_b(app),
            Role::StageB => self.run_stage_b(app),
            Role::Shots3bA => self.run_shots3b_a(app),
            Role::Shots3bB => self.run_shots3b_b(app),
            Role::Shots3bHost => self.run_shots3b_host(app),
            Role::Calladex => self.run_calladex(app),
            Role::MacView => self.run_mac_view(app),
            Role::UixA => self.run_uix_a(app),
            Role::UixB => self.run_uix_b(app),
            #[cfg(feature = "donate-test")]
            Role::Donate => self.run_donate(app),
            Role::KeysA => self.run_keys_a(app),
            Role::ThumbsA => self.run_thumbs_a(app),
            Role::ThumbsB => self.run_thumbs_b(app),
            Role::ThumbsC => self.run_thumbs_c(app),
            Role::TlsA => self.run_tls_a(app),
            Role::TlsB => self.run_tls_b(app),
            Role::TlsMitm => self.run_tls_mitm(app),
            Role::TlsOldHub => self.run_tls_oldhub(app),
            Role::E2eeA => self.run_e2ee_a(app),
            Role::E2eeA2 => self.run_e2ee_a2(app),
            Role::E2eeB => self.run_e2ee_b(app),
            Role::E2dmA => self.run_e2dm_a(app),
            Role::E2dmB => self.run_e2dm_b(app),
            Role::E2dmA2 => self.run_e2dm_a2(app),
            Role::E2dmNew => self.run_e2dm_new(app),
            Role::E2vA => self.run_e2v_a(app),
            Role::E2vB => self.run_e2v_b(app),
            Role::E2vC => self.run_e2v_c(app),
            Role::E2vAdmin => self.run_e2v_admin(app),
            Role::EmojiA => self.run_emoji_a(app),
            Role::RespA => self.run_resp_a(app),
            Role::RespB => self.run_resp_b(app),
        }
    }

    fn sample(&mut self, g: &vox_client_core::VoiceGate) {
        self.lvl_max = self.lvl_max.max(g.level_db());
        self.lvl_min = self.lvl_min.min(g.level_db());
        self.thr_last = g.threshold_db();
        self.opened |= g.is_open();
        if self.last_line.is_none_or(|t| t.elapsed() >= Duration::from_millis(500)) {
            self.last_line = Some(Instant::now());
            tracing::info!(target: "autotest", "AUTOTEST sample: level {:.1} dBFS, threshold {:.1}, open={}, ptt={}", g.level_db(), g.threshold_db(), g.is_open(), g.ptt());
        }
    }

    fn reset_samples(&mut self) {
        self.lvl_max = -100.0;
        self.lvl_min = 0.0;
        self.opened = false;
    }

    /// Mic test alone, then together with a voice session; the meter must move with the real
    /// microphone and everything must close again afterwards.
    fn run_mictest(&mut self, app: &mut App) -> Option<i32> {
        use crate::app::MicTestUi;
        use vox_client_core::{GateConfig, GateMode};
        match self.step {
            0 => self.connect(app),
            1 => {
                if self.logged_in(app) {
                    app.open_settings();
                    app.start_mic_test();
                    self.goto(2, "mic test requested");
                } else if self.waited(30) || app.form.error.is_some() {
                    return self.fail(&format!("could not log in: {}", app.form.error.clone().unwrap_or_default()));
                }
            }
            2 => match app.settings_ui.mic_test.clone() {
                MicTestUi::On(warning) => {
                    tracing::info!(target: "autotest", "AUTOTEST mic test running; warning={warning:?}");
                    self.reset_samples();
                    self.goto(3, "sampling the meter outside voice");
                }
                MicTestUi::Off => {
                    return self.fail(&format!("mic test did not start: {:?}", app.settings_ui.mic_test_error));
                }
                MicTestUi::Starting if self.waited(15) => return self.fail("mic test still opening after 15 s"),
                MicTestUi::Starting => {}
            },
            3 => {
                let g = app.gate_test.clone();
                self.sample(&g);
                let secs = std::env::var("VOX_AUTOTEST_METER_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(6);
                if self.waited(secs) {
                    tracing::info!(target: "autotest", "AUTOTEST meter (test, no voice): level {:.1}..{:.1} dBFS, threshold {:.1}, gate opened={}", self.lvl_min, self.lvl_max, self.thr_last, self.opened);
                    if self.lvl_max <= -99.0 {
                        return self.fail("the meter never moved: no signal from the microphone");
                    }
                    let Some(vc) = Self::first_voice(app) else {
                        return self.fail("no voice channel");
                    };
                    app.join_voice(vc);
                    self.goto(4, "joining voice with the mic test still running");
                }
            }
            4 => {
                if app.voice.channel.is_some() {
                    self.reset_samples();
                    self.goto(5, "in voice; sampling both meters");
                } else if let Some(e) = app.voice.error.clone() {
                    return self.fail(&format!("voice join failed: {e}"));
                } else if self.waited(90) {
                    return self.fail("voice join took more than 90 s");
                }
            }
            5 => {
                let (gv, gt) = (app.gate_voice.clone(), app.gate_test.clone());
                self.sample(&gv);
                if self.waited(3) && gv.config().mode == GateMode::Auto {
                    // Change the sensitivity mid-call: the very next chunks must use it.
                    app.settings.input_gate = GateConfig { mode: GateMode::Manual, threshold_db: -33.0, hold_ms: 500 };
                    app.apply_gate_config();
                }
                if !self.change_checked && gv.config().mode == GateMode::Manual && self.step_at.elapsed() > Duration::from_millis(3500) {
                    self.change_checked = true;
                    let (t1, t2) = (gv.threshold_db(), gt.threshold_db());
                    tracing::info!(target: "autotest", "AUTOTEST mid-call change: voice threshold {t1:.1}, test threshold {t2:.1} (wanted -33)");
                    if (t1 + 33.0).abs() > 0.1 || (t2 + 33.0).abs() > 0.1 {
                        return self.fail("the new threshold did not reach the running gates");
                    }
                }
                if self.waited(6) {
                    tracing::info!(target: "autotest", "AUTOTEST meter (voice): level {:.1}..{:.1} dBFS, gate opened={}, speaking-indicator={}", self.lvl_min, self.lvl_max, self.opened, app.sending_voice());
                    app.stop_mic_test();
                    self.goto(6, "mic test stopped");
                }
            }
            6 if self.waited(2) => {
                let g = app.gate_test.clone();
                tracing::info!(target: "autotest", "AUTOTEST after stop: test meter level {:.1}, open={}", g.level_db(), g.is_open());
                if g.level_db() > -99.0 || g.is_open() {
                    return self.fail("the test meter did not clear after the test stopped");
                }
                app.leave_voice();
                self.goto(7, "left voice");
            }
            7 if self.waited(2) => return self.pass(),
            _ => {}
        }
        None
    }

    fn connect(&mut self, app: &mut App) {
        app.form.url = self.args.server.clone();
        app.form.user = self.args.name.clone();
        app.form.password = self.args.password.clone();
        app.connect();
        self.goto(1, "connecting");
    }

    fn logged_in(&self, app: &App) -> bool {
        app.screen == Screen::Main && app.ready && app.hub_up
    }

    /// One line about the picker's memory: egui textures alive and whether it holds anything.
    fn log_picker(app: &App, what: &str) {
        let tex = app.repaint.ctx().map(|c| c.tex_manager().read().num_allocated()).unwrap_or(0);
        tracing::info!(target: "autotest", "AUTOTEST {what}: egui textures={tex} picker_idle={} dialog_open={}", app.golive.picker.is_idle(), app.golive.open);
    }

    fn first_text(app: &App) -> Option<i64> {
        app.channels.iter().find(|c| c.kind == ChannelKind::Text).map(|c| c.id)
    }

    fn first_voice(app: &App) -> Option<i64> {
        app.channels.iter().find(|c| c.kind == ChannelKind::Voice).map(|c| c.id)
    }

    fn saw_message(app: &App, prefix: &str) -> bool {
        app.logs.values().any(|l| l.msgs.iter().any(|m| m.content.starts_with(prefix)))
    }

    fn say(app: &mut App, text: &str) {
        app.input = text.to_string();
        app.send_message();
    }

    fn run_stay(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => self.connect(app),
            1 => {
                if self.logged_in(app) {
                    self.goto(2, "connected; staying idle");
                    if self.role == Role::Voice {
                        if let Some(vc) = Self::first_voice(app) {
                            app.join_voice(vc);
                            self.goto(3, "joining voice");
                        }
                    }
                    match self.args.show.as_deref() {
                        Some("golive") | Some("golive-cycle") => app.open_golive(),
                        // "Só áudio" picker: apps with their icons and "Todo o som do PC".
                        Some("golive-audio") => {
                            app.golive.mode = crate::app::ShareMode::AudioOnly;
                            app.open_golive();
                        }
                        // Item 8c layout checks: the "Telas" tab with a screen picked, and the
                        // "Aplicativos" tab with the first window picked (step 2 below).
                        Some("golive-screens") => {
                            app.open_golive();
                            app.golive.picker.tab = crate::sources::Tab::Screens;
                            app.golive.pick = Some(Pick::Monitor(0));
                        }
                        Some("golive-pick") => app.open_golive(),
                        // "Trocar tela" picker over a pretend live on screen 1 (the "transmitindo
                        // agora" mark); nothing is sent anywhere.
                        Some("golive-switch") => {
                            app.voice.share = Share::Live { encoder: "test".into() };
                            app.voice.share_pick = Some(Pick::Monitor(0));
                            app.open_golive_switch();
                        }
                        Some("settings") => app.open_settings(),
                        // v1.0: the component gallery over the app (`ui_kit`).
                        Some("kit") => app.kit = Some(Default::default()),
                        // Item 10: link-preview cards under real public links (needs internet;
                        // the hub refuses private addresses, so this cannot use a local site).
                        Some("previews") => {
                            if let Some(ch) = Self::first_text(app) {
                                app.select_text(ch);
                                for text in [
                                    "olha esse vídeo https://www.youtube.com/watch?v=dQw4w9WgXcQ",
                                    "wiki https://en.wikipedia.org/wiki/Rust_(programming_language) e o código https://github.com/rust-lang/rust",
                                    "reel https://www.instagram.com/reel/DeRslJageXF/?dlrf=abc",
                                    "interno http://192.168.1.1/admin e http://localhost:7890/api/health não geram cartão",
                                    "só texto, sem link",
                                ] {
                                    Self::say(app, text);
                                }
                            }
                        }
                        // "Configurar câmera" with its preview (layout / airspace check);
                        // `camera-ctl` opens it from Settings (Settings waits behind it).
                        Some("camera") => app.open_camera_window(),
                        Some("camera-ctl") => {
                            app.open_settings();
                            app.open_camera_window();
                        }
                        _ => {}
                    }
                } else if self.waited(30) || app.form.error.is_some() {
                    return self.fail(&format!("could not log in: {}", app.form.error.clone().unwrap_or_default()));
                }
            }
            2 if self.args.show.as_deref() == Some("previews") && self.waited(25) => {
                let (entries, ready, textures) = app.previews.stats();
                tracing::info!(target: "autotest", "AUTOTEST previews: entries={entries} cards={ready} textures={textures} unsupported={}", app.previews.unsupported());
                if app.previews.unsupported() {
                    // An older hub (no /api/link-preview): quietly no cards, which is the pass.
                    if ready != 0 || entries != 0 {
                        return self.fail("an old hub must leave no cards and no pending lookups");
                    }
                } else if ready == 0 {
                    return self.fail("no link-preview card was loaded");
                }
                self.goto(9, "cards loaded; staying open for a look");
            }
            // Item 8c: the Go Live picker's thumbnails (and their textures) go away with the dialog.
            2 if self.args.show.as_deref() == Some("golive-pick") && !app.golive.windows.is_empty() && self.waited(2) => {
                app.golive.pick = app.golive.windows.first().map(|w| Pick::Window(w.hwnd));
                self.goto(9, "picked the first window in the picker");
            }
            2 if self.args.show.as_deref() == Some("golive-cycle") && self.waited(6) => {
                Self::log_picker(app, "picker open, thumbnails loaded");
                app.golive.open = false;
                if let Some(c) = app.repaint.ctx() {
                    c.request_repaint();
                }
                self.goto(3, "closed the picker");
            }
            3 if self.args.show.as_deref() == Some("golive-cycle") && self.waited(3) => {
                Self::log_picker(app, "picker closed");
                app.open_golive();
                self.goto(4, "reopened the picker");
            }
            4 if self.args.show.as_deref() == Some("golive-cycle") && self.waited(6) => {
                Self::log_picker(app, "picker reopened");
                app.golive.open = false;
                if let Some(c) = app.repaint.ctx() {
                    c.request_repaint();
                }
                self.goto(5, "closed again");
            }
            5 if self.args.show.as_deref() == Some("golive-cycle") && self.waited(3) => {
                Self::log_picker(app, "picker closed again");
                self.goto(9, "cycle done");
            }
            // Role::Voice: report how the join ended, as the user would see it.
            3 if self.role == Role::Voice => {
                if app.voice.channel.is_some() {
                    tracing::info!(target: "autotest", "AUTOTEST voice joined; warning={:?} no_mic={}", app.voice.warning, app.voice.no_mic);
                    self.goto(4, "joined");
                } else if let Some(e) = app.voice.error.clone() {
                    tracing::info!(target: "autotest", "AUTOTEST voice failed (retry={:?}, joining={:?}): {e}", app.voice.retry, app.voice.joining);
                    self.goto(4, "join failed");
                } else if self.waited(90) {
                    return self.fail("voice join neither succeeded nor failed within 90 s");
                }
            }
            // Role::Voice, once joined: press mute / unmute / deafen / undeafen like a person (the
            // alert sounds for them are logged, or silent with VOX_NO_PLAYBACK).
            4..=7 if self.role == Role::Voice && self.waited(1) => {
                if app.voice.channel.is_none() {
                    self.goto(8, "not in voice; skipping the mute/deafen presses");
                } else {
                    match self.step {
                        4 | 5 => app.toggle_mute(),
                        _ => app.toggle_deafen(),
                    }
                    self.goto(self.step + 1, "pressed mute/deafen");
                }
            }
            _ => {}
        }
        None
    }

    fn run_a(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => self.connect(app),
            1 => {
                if self.logged_in(app) {
                    let Some(ch) = Self::first_text(app) else {
                        return self.fail("no text channel");
                    };
                    app.select_text(ch);
                    let text = format!("autotest-A {}: oi do A", self.args.run_id);
                    Self::say(app, &text);
                    self.goto(2, "logged in, message sent, waiting for B's message");
                } else if self.waited(30) || app.form.error.is_some() {
                    return self.fail(&format!("could not log in: {}", app.form.error.clone().unwrap_or_default()));
                }
            }
            2 => {
                if Self::saw_message(app, &format!("autotest-B {}", self.args.run_id)) {
                    let Some(vc) = Self::first_voice(app) else {
                        return self.fail("no voice channel");
                    };
                    app.join_voice(vc);
                    self.goto(3, "A saw B's message; joining voice");
                } else if self.waited(120) {
                    return self.fail("never saw B's message");
                }
            }
            3 => {
                if app.voice.channel.is_some() {
                    self.goto(4, "in voice; waiting for B");
                } else if let Some(e) = &app.voice.error {
                    let e = e.clone();
                    return self.fail(&format!("voice join failed: {e}"));
                } else if self.waited(30) {
                    return self.fail("voice join timed out");
                }
            }
            4 => {
                if app.members_here().len() >= 2 {
                    let names: Vec<_> = app.members_here().iter().map(|m| m.user.username.clone()).collect();
                    let Some(hwnd) = self.args.hwnd else {
                        return self.fail("no --hwnd given");
                    };
                    app.start_share(Pick::Window(hwnd), Preset::P1080p30, true);
                    self.goto(5, &format!("voice members {names:?}; starting Go Live 1080p30"));
                } else if self.waited(120) {
                    return self.fail("B never joined voice");
                }
            }
            5 => {
                if let Share::Live { encoder } = &app.voice.share {
                    let enc = encoder.clone();
                    let hold = self.args.hold_secs;
                    self.goto(6, &format!("Go Live running, encoder={enc}; holding {hold}s"));
                } else if app.voice.share == Share::Off && self.waited(2) {
                    let msg = app.notice.clone().unwrap_or_default();
                    return self.fail(&format!("Go Live did not start: {msg}"));
                } else if self.waited(30) {
                    return self.fail("Go Live start timed out");
                }
            }
            6 if self.args.preview => self.goto(30, "Go Live running; starting the preview checks"),
            6 if self.args.pv_mode.is_some() => {
                let mode = self.args.pv_mode.clone().unwrap_or_default();
                match mode.as_str() {
                    "chat" => {
                        if let Some(ch) = Self::first_text(app) {
                            app.select_text(ch);
                        }
                    }
                    "focus" => app.pin_only(app.my_id().map(TileKey::Stream)),
                    _ => {}
                }
                self.goto(60, &format!("preview measurement mode {mode}"));
            }
            60 => {
                if self.waited(self.args.hold_secs) {
                    app.stop_share();
                    self.goto(7, "stopping Go Live");
                }
            }
            30..=59 => return self.run_a_preview(app),
            6 => {
                let viewers = app.my_id().map(|me| app.viewers_of(me)).unwrap_or_default();
                if !self.viewer_seen && !viewers.is_empty() {
                    self.viewer_seen = true;
                    tracing::info!(target: "autotest", "AUTOTEST A: viewers of my live = {viewers:?} (ui hover/bar text: {})", App::viewers_text(&viewers));
                } else if self.viewer_seen && !self.viewer_cleared && viewers.is_empty() {
                    self.viewer_cleared = true;
                    tracing::info!(target: "autotest", "AUTOTEST A: viewer list cleared again ({})", App::viewers_text(&viewers));
                }
                if let Some(h2) = self.args.switch_hwnd {
                    if !self.switch_sent && self.waited(self.args.hold_secs / 2) {
                        app.switch_share(Pick::Window(h2), true);
                        self.switch_sent = true;
                        tracing::info!(target: "autotest", "AUTOTEST A: Trocar tela -> window {h2}");
                    } else if self.switch_sent && !self.switch_done && app.voice.switching_to.is_none() {
                        if !matches!(app.voice.share, Share::Live { .. }) {
                            return self.fail("the live ended during the switch");
                        }
                        if let Some(n) = app.notice.clone() {
                            return self.fail(&format!("switch reported: {n}"));
                        }
                        self.switch_done = true;
                        tracing::info!(target: "autotest", "AUTOTEST A: switch done, still live, share_pick={:?}", app.voice.share_pick);
                    }
                }
                // Hold at least `hold_secs`, and keep the live running while B is still watching:
                // B's inline checks (steps 20..29) take ~30 s and must not race the hold. B
                // stopping (viewer list cleared) is the signal; after a generous bound the
                // checks below fail as before.
                let b_watching = self.viewer_seen && !self.viewer_cleared;
                if self.waited(self.args.hold_secs) && (!b_watching || self.waited(self.args.hold_secs + B_DONE_GRACE_SECS)) {
                    if self.args.switch_hwnd.is_some() && !self.switch_done {
                        return self.fail("the switch never completed");
                    }
                    if app.viewers_supported && !self.viewer_seen {
                        return self.fail("B watched my live but never showed up in the viewer list");
                    }
                    if self.viewer_seen && !self.viewer_cleared {
                        return self.fail("B stopped watching but stayed in the viewer list");
                    }
                    app.stop_share();
                    self.goto(7, "stopping Go Live");
                }
            }
            7 => {
                if self.waited(3) {
                    app.leave_voice();
                    self.goto(8, "leaving voice");
                }
            }
            8 if self.waited(3) => return self.pass(),
            _ => {}
        }
        None
    }

    fn run_b(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => self.connect(app),
            1 => {
                if self.logged_in(app) {
                    let Some(ch) = Self::first_text(app) else {
                        return self.fail("no text channel");
                    };
                    app.select_text(ch);
                    self.goto(2, "logged in; waiting for A's message");
                } else if self.waited(30) || app.form.error.is_some() {
                    return self.fail(&format!("could not log in: {}", app.form.error.clone().unwrap_or_default()));
                }
            }
            2 => {
                if Self::saw_message(app, &format!("autotest-A {}", self.args.run_id)) {
                    let text = format!("autotest-B {}: oi do B", self.args.run_id);
                    Self::say(app, &text);
                    let Some(vc) = Self::first_voice(app) else {
                        return self.fail("no voice channel");
                    };
                    app.join_voice(vc);
                    self.goto(3, "B saw A's message; replied; joining voice");
                } else if self.waited(120) {
                    return self.fail("never saw A's message");
                }
            }
            3 => {
                if app.voice.channel.is_some() {
                    self.goto(4, "in voice; waiting for A's Go Live");
                } else if let Some(e) = &app.voice.error {
                    let e = e.clone();
                    return self.fail(&format!("voice join failed: {e}"));
                } else if self.waited(30) {
                    return self.fail("voice join timed out");
                }
            }
            4 => {
                let me = app.my_id();
                let live = app
                    .members_here()
                    .iter()
                    .find(|m| m.streaming && Some(m.user.id) != me)
                    .map(|m| (m.user.id, m.user.username.clone()));
                // Not within the first seconds after the hub snapshot: those alerts are muted on purpose,
                // and A's "someone started watching" sound should be heard (logged) in this run.
                if let Some((id, name)) = live.filter(|_| self.waited(5)) {
                    // The sidebar's "Assistir": call view, focused on the stream.
                    app.watch_focused(&id.to_string());
                    self.goto(5, &format!("{name} is live; pressing Assistir (sidebar)"));
                } else if self.waited(150) {
                    return self.fail("A never went live");
                }
            }
            5 => {
                if app.voice.watch_started_count >= 1 {
                    self.goto(6, "viewer opened; letting frames flow");
                } else if self.waited(20) {
                    return self.fail("watch never started");
                }
            }
            6 => {
                if self.waited(4) {
                    self.seq_seen = app.stream_info_seq;
                    app.backend.send(Cmd::Stats);
                    self.goto(7, "sampling frames (1/2)");
                }
            }
            7 => {
                if app.stream_info_seq != self.seq_seen {
                    self.seq_seen = app.stream_info_seq;
                    self.frames_first = app
                        .stream_info
                        .iter()
                        .find(|s| s.0.starts_with("watch-video"))
                        .map(|s| s.1)
                        .unwrap_or(0);
                    let first = self.frames_first;
                    self.goto(8, &format!("frames so far: {first}"));
                } else if self.waited(10) {
                    return self.fail("no stats reply");
                }
            }
            8 => {
                if self.waited(5) {
                    app.backend.send(Cmd::Stats);
                    self.goto(9, "sampling frames (2/2)");
                }
            }
            9 => {
                if app.stream_info_seq != self.seq_seen {
                    let (frames, w, h) = app
                        .stream_info
                        .iter()
                        .find(|s| s.0.starts_with("watch-video"))
                        .map(|s| (s.1, s.2, s.3))
                        .unwrap_or((0, 0, 0));
                    if frames > self.frames_first + 10 && w > 0 {
                        tracing::info!(target: "autotest", "AUTOTEST viewer got frames: {} -> {} ({}x{})", self.frames_first, frames, w, h);
                        if self.args.popout {
                            let st = Self::view_state(app);
                            tracing::info!(target: "autotest", "AUTOTEST B popout mode: state {st:?}");
                            if !st.is_some_and(|s| s.popped_out && s.popout_hwnd != 0) {
                                return self.fail("popout setting on, but the stream is not in its own window");
                            }
                            self.goto(29, "popout mode verified; holding");
                        } else {
                            self.goto(20, "frames verified; checking the inline view");
                        }
                    } else {
                        return self.fail(&format!("viewer got no frames ({} -> {frames}, {w}x{h})", self.frames_first));
                    }
                } else if self.waited(10) {
                    return self.fail("no stats reply");
                }
            }
            10 => {
                if app.voice.watch_ended_count >= 1 {
                    self.goto(11, "unwatched; waiting for A to stop");
                } else if self.waited(10) {
                    return self.fail("watch did not end");
                }
            }
            11 => {
                let me = app.my_id();
                let any_live = app.members_here().iter().any(|m| m.streaming && Some(m.user.id) != me);
                if !any_live {
                    app.leave_voice();
                    self.goto(12, "A stopped Go Live; leaving voice");
                } else if self.waited(60) {
                    return self.fail("A never stopped");
                }
            }
            12 if self.waited(3) => return self.pass(),
            20..=29 => return self.run_b_inline(app),
            30..=41 => return self.run_b_fullscreen(app),
            _ => {}
        }
        None
    }

    /// The stream role `b` checks: the focused one if watched, else the first watched one.
    fn watched_key(app: &App) -> Option<String> {
        app.focused_stream().or_else(|| app.voice.watching.first().cloned())
    }

    /// "Abrir em janela" / "Trazer de volta" on the checked stream.
    fn pop_out(app: &mut App, out: bool) {
        if let Some(k) = Self::watched_key(app) {
            app.pop_out_stream(&k, out);
        }
    }

    fn view_state(app: &App) -> Option<vox_client_core::golive::StreamState> {
        let key = Self::watched_key(app)?;
        vox_client_core::golive::Viewer::get().ok()?.state(&key)
    }

    fn placed(app: &App) -> Option<(i32, i32, i32, i32)> {
        let key = Self::watched_key(app)?;
        app.video.placed.get(&key).copied()
    }

    /// Item 8: the stream inside the call view (role `b`, steps 20..29).
    fn run_b_inline(&mut self, app: &mut App) -> Option<i32> {
        if app.voice.watching.is_empty() && self.step < 29 {
            return self.fail("stopped watching during the inline checks");
        }
        match self.step {
            20 => {
                let st = Self::view_state(app);
                let placed = Self::placed(app);
                tracing::info!(target: "autotest", "AUTOTEST B inline focused: center={:?} focus={:?} placed={placed:?} state={st:?}", app.center, app.focused_stream());
                if app.center != Center::Call || app.focused_stream().is_none() {
                    return self.fail("Assistir did not open the focused call view");
                }
                if placed.is_none() || !st.is_some_and(|s| !s.popped_out && s.presented > 0) {
                    return self.fail("the inline video is not placed / presenting");
                }
                let Some(ch) = Self::first_text(app) else { return self.fail("no text channel") };
                app.select_text(ch);
                self.goto(21, "switched to the text channel (stream keeps running)");
            }
            21 if self.waited(2) => {
                let placed = Self::placed(app);
                tracing::info!(target: "autotest", "AUTOTEST B chat shown: center={:?} placed={placed:?} watching={:?}", app.center, app.voice.watching);
                if placed.is_some() {
                    return self.fail("the video stayed visible over the chat");
                }
                app.show_call();
                app.pin_only(None);
                self.goto(22, "back to the call view, grid");
            }
            22 if self.waited(2) => {
                let placed = Self::placed(app);
                tracing::info!(target: "autotest", "AUTOTEST B grid: placed={placed:?}");
                if placed.is_none() {
                    return self.fail("no video in the grid tile");
                }
                app.pin_only(app.voice.watching.first().and_then(|k| k.parse().ok()).map(TileKey::Stream));
                self.goto(23, "focused again");
            }
            23 if self.waited(2) => {
                let placed = Self::placed(app);
                tracing::info!(target: "autotest", "AUTOTEST B focused: placed={placed:?}");
                if self.args.show.as_deref() == Some("settings") {
                    app.open_settings();
                    self.goto(24, "settings dialog opened over the stream");
                } else {
                    self.goto(25, "no dialog check");
                }
            }
            24 if self.waited(4) => {
                let placed = Self::placed(app);
                let clip = Self::watched_key(app).and_then(|k| app.video.clips.get(&k).cloned());
                tracing::info!(target: "autotest", "AUTOTEST B with settings open: covered={} placed={placed:?} clip={clip:?}", app.video.covered);
                // Over part of the video the dialog is cut out of it; over all of it, it hides it.
                if placed.is_some() && !matches!(clip, Some(crate::call::clip::Clip::Cut(_))) {
                    return self.fail("the video stayed on top of the settings dialog");
                }
                app.settings_ui.open = false;
                self.goto(25, "settings closed");
            }
            25 if self.waited(2) => {
                if Self::placed(app).is_none() {
                    return self.fail("the video did not come back after the dialog closed");
                }
                Self::pop_out(app, true);
                self.goto(26, "pressed Abrir em janela");
            }
            26 if self.waited(3) => {
                let st = Self::view_state(app);
                tracing::info!(target: "autotest", "AUTOTEST B popped out: state={st:?} placed={:?}", Self::placed(app));
                let Some(st) = st.filter(|s| s.popped_out && s.popout_hwnd != 0) else {
                    return self.fail("Abrir em janela did not open the window");
                };
                close_window(st.popout_hwnd);
                self.goto(27, "closed the pop-out window like a user (WM_CLOSE)");
            }
            27 if self.waited(2) => {
                let st = Self::view_state(app);
                let placed = Self::placed(app);
                tracing::info!(target: "autotest", "AUTOTEST B after closing the window: state={st:?} placed={placed:?} watching={:?}", app.voice.watching);
                if !st.is_some_and(|s| !s.popped_out) || placed.is_none() {
                    return self.fail("closing the pop-out did not bring the stream back inline");
                }
                // The focus toolbar's "Tela cheia" button.
                self.win_rect = app.video.parent.and_then(win::window_rect);
                let ctx = app.repaint_ctx();
                app.set_fullscreen(&ctx, true);
                self.goto(30, &format!("pressed Tela cheia (window was {:?})", self.win_rect));
            }
            29 if self.waited(self.args.watch_secs) => {
                if let Some(st) = Self::view_state(app) {
                    tracing::info!(target: "autotest", "AUTOTEST B before unwatch: state={st:?} placed={:?}", Self::placed(app));
                }
                if let Some(k) = Self::watched_key(app) {
                    app.unwatch(&k);
                }
                self.goto(10, "pressing Parar de assistir");
            }
            _ => {}
        }
        None
    }
}

impl Auto {
    /// The app's window covers its monitor and the stream video fills it.
    fn check_app_fullscreen(&self, app: &App, how: &str) -> Result<(), String> {
        let parent = app.video.parent.ok_or("no app window")?;
        let (wr, mr) = (win::window_rect(parent), win::monitor_rect(parent));
        let placed = Self::placed(app);
        tracing::info!(target: "autotest", "AUTOTEST B fullscreen ({how}): fullscreen={} window={wr:?} monitor={mr:?} placed={placed:?}", app.call.fullscreen);
        let full = mr.is_some_and(|m| wr == Some(m) && placed.is_some_and(|p| p.2 == m.2 - m.0 && p.3 == m.3 - m.1));
        if !app.call.fullscreen || !full {
            return Err(format!("{how} did not put the stream fullscreen over the monitor"));
        }
        Ok(())
    }

    /// Back from fullscreen: the window where it was and the stream in the focused view again.
    fn check_app_restored(&self, app: &App, how: &str) -> Result<(), String> {
        let wr = app.video.parent.and_then(win::window_rect);
        let placed = Self::placed(app);
        tracing::info!(target: "autotest", "AUTOTEST B after leaving fullscreen ({how}): fullscreen={} window={wr:?} (was {:?}) placed={placed:?} focus={:?}", app.call.fullscreen, self.win_rect, app.focused_stream());
        if app.call.fullscreen || wr != self.win_rect || placed.is_none() || app.focused_stream().as_ref() != app.voice.watching.first() {
            return Err(format!("{how} did not bring back the focused view in the same window"));
        }
        Ok(())
    }

    fn check_popout(&self, app: &App, fullscreen: bool, how: &str) -> Result<isize, String> {
        let st = Self::view_state(app).filter(|s| s.popped_out && s.popout_hwnd != 0).ok_or("the stream is not in its own window")?;
        let (wr, mr) = (win::window_rect(st.popout_hwnd), win::monitor_rect(st.popout_hwnd));
        tracing::info!(target: "autotest", "AUTOTEST B pop-out ({how}): fullscreen={} window={wr:?} monitor={mr:?} (was {:?})", st.fullscreen, self.win_rect);
        let ok = if fullscreen { st.fullscreen && wr.is_some() && wr == mr } else { !st.fullscreen && wr == self.win_rect };
        if !ok {
            return Err(format!("pop-out: {how} {}", if fullscreen { "did not cover the monitor" } else { "did not restore the window" }));
        }
        Ok(st.popout_hwnd)
    }

    /// Fullscreen checks (role `b`, steps 30..41): the "Tela cheia" button, a double-click on
    /// the video and F11 enter it, Esc leaves it; then the same in the pop-out window
    /// (double-click / F11 toggle, Esc leaves). Every time the window must cover its monitor,
    /// and come back exactly where it was.
    fn run_b_fullscreen(&mut self, app: &mut App) -> Option<i32> {
        if app.voice.watching.is_empty() {
            return self.fail("stopped watching during the fullscreen checks");
        }
        let parent = app.video.parent.unwrap_or_default();
        let res: Result<(), String> = match self.step {
            30 if self.waited(2) => self.check_app_fullscreen(app, "Tela cheia button").map(|_| {
                win::key(parent, win::VK_ESCAPE);
                self.goto(31, "pressed Esc");
            }),
            31 if self.waited(2) => self.check_app_restored(app, "Esc").and_then(|_| {
                let (x, y, w, h) = Self::placed(app).ok_or("no video")?;
                win::double_click(parent, x + w / 2, y + h / 2);
                self.goto(32, "double-clicked the stream video");
                Ok(())
            }),
            32 if self.waited(2) => self.check_app_fullscreen(app, "double-click").map(|_| {
                win::key(parent, win::VK_ESCAPE);
                self.goto(33, "pressed Esc");
            }),
            33 if self.waited(2) => self.check_app_restored(app, "Esc").map(|_| {
                win::key(parent, win::VK_F11);
                self.goto(34, "pressed F11");
            }),
            34 if self.waited(2) => self.check_app_fullscreen(app, "F11").map(|_| {
                win::key(parent, win::VK_ESCAPE);
                self.goto(35, "pressed Esc");
            }),
            35 if self.waited(2) => self.check_app_restored(app, "Esc").map(|_| {
                Self::pop_out(app, true);
                self.goto(36, "pressed Abrir em janela (pop-out fullscreen checks)");
            }),
            36 if self.waited(2) => {
                let st = Self::view_state(app).filter(|s| s.popped_out && s.popout_hwnd != 0);
                match st {
                    Some(st) => {
                        self.win_rect = win::window_rect(st.popout_hwnd);
                        win::native_double_click(st.popout_hwnd);
                        self.goto(37, &format!("double-clicked the pop-out (window {:?})", self.win_rect));
                        Ok(())
                    }
                    None => Err("Abrir em janela did not open the window".into()),
                }
            }
            37 if self.waited(2) => self.check_popout(app, true, "double-click").map(|h| {
                win::key(h, win::VK_ESCAPE);
                self.goto(38, "pressed Esc in the pop-out");
            }),
            38 if self.waited(2) => self.check_popout(app, false, "Esc").map(|h| {
                win::key(h, win::VK_F11);
                self.goto(39, "pressed F11 in the pop-out");
            }),
            39 if self.waited(2) => self.check_popout(app, true, "F11").map(|h| {
                win::key(h, win::VK_F11);
                self.goto(40, "pressed F11 again in the pop-out");
            }),
            40 if self.waited(2) => self.check_popout(app, false, "F11 again").map(|_| {
                Self::pop_out(app, false);
                self.goto(41, "pressed Trazer de volta");
            }),
            41 if self.waited(2) => {
                let st = Self::view_state(app);
                if st.is_some_and(|s| !s.popped_out) && Self::placed(app).is_some() {
                    let secs = self.args.watch_secs;
                    self.goto(29, &format!("fullscreen checks done; watching {secs}s more"));
                    Ok(())
                } else {
                    Err(format!("the stream did not come back inline: {st:?}"))
                }
            }
            _ => Ok(()),
        };
        match res {
            Ok(()) => None,
            Err(e) => self.fail(&e),
        }
    }
}

fn close_window(hwnd: isize) {
    win::post(hwnd, win::WM_CLOSE, 0, 0);
}

/// Window queries and synthetic input for the autotest (posted to this process's own windows,
/// never global input).
#[cfg(windows)]
mod win {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    pub use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VK_ESCAPE, VK_F11};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, VIRTUAL_KEY, MAPVK_VK_TO_VSC};
    pub use windows_sys::Win32::UI::WindowsAndMessaging::WM_CLOSE;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowRect, PostMessageW, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE};

    const MK_LBUTTON: usize = 0x0001;

    /// left, top, right, bottom (screen pixels).
    pub type Rect = (i32, i32, i32, i32);

    pub fn post(hwnd: isize, msg: u32, wparam: usize, lparam: isize) {
        // SAFETY: posting to a window of this process; the message carries no pointers.
        unsafe { PostMessageW(hwnd as _, msg, wparam, lparam) };
    }

    pub fn window_rect(hwnd: isize) -> Option<Rect> {
        let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: valid out-pointer.
        (unsafe { GetWindowRect(hwnd as _, &mut r) } != 0).then_some((r.left, r.top, r.right, r.bottom))
    }

    /// The monitor the window is (mostly) on.
    pub fn monitor_rect(hwnd: isize) -> Option<Rect> {
        // SAFETY: plain queries with a properly sized out-struct.
        unsafe {
            let mut mi: MONITORINFO = std::mem::zeroed();
            mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            let r = (GetMonitorInfoW(MonitorFromWindow(hwnd as _, MONITOR_DEFAULTTONEAREST), &mut mi) != 0).then_some(mi.rcMonitor)?;
            Some((r.left, r.top, r.right, r.bottom))
        }
    }

    fn xy(x: i32, y: i32) -> isize {
        (((y as u32 & 0xFFFF) << 16) | (x as u32 & 0xFFFF)) as isize
    }

    /// Two left clicks at client pixel (x, y), as the mouse would send them.
    pub fn double_click(hwnd: isize, x: i32, y: i32) {
        post(hwnd, WM_MOUSEMOVE, 0, xy(x, y));
        for _ in 0..2 {
            post(hwnd, WM_LBUTTONDOWN, MK_LBUTTON, xy(x, y));
            post(hwnd, WM_LBUTTONUP, 0, xy(x, y));
        }
    }

    /// One left click at client pixel (x, y).
    pub fn click(hwnd: isize, x: i32, y: i32) {
        post(hwnd, WM_MOUSEMOVE, 0, xy(x, y));
        post(hwnd, WM_LBUTTONDOWN, MK_LBUTTON, xy(x, y));
        post(hwnd, WM_LBUTTONUP, 0, xy(x, y));
    }

    /// A double-click as a `CS_DBLCLKS` window receives it.
    pub fn native_double_click(hwnd: isize) {
        post(hwnd, WM_LBUTTONDBLCLK, MK_LBUTTON, xy(20, 20));
        post(hwnd, WM_LBUTTONUP, 0, xy(20, 20));
    }

    pub fn key(hwnd: isize, vk: VIRTUAL_KEY) {
        // SAFETY: pure table lookup.
        let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as isize;
        post(hwnd, WM_KEYDOWN, vk as usize, 1 | (scan << 16));
        post(hwnd, WM_KEYUP, vk as usize, 1 | (scan << 16) | (0b11 << 30));
    }
}

#[cfg(not(windows))]
mod win {
    pub type Rect = (i32, i32, i32, i32);
    pub const VK_ESCAPE: u16 = 0x1B;
    pub const VK_F11: u16 = 0x7A;
    pub const WM_CLOSE: u32 = 0x10;
    pub fn post(_hwnd: isize, _msg: u32, _wparam: usize, _lparam: isize) {}
    pub fn window_rect(_hwnd: isize) -> Option<Rect> {
        None
    }
    pub fn monitor_rect(_hwnd: isize) -> Option<Rect> {
        None
    }
    pub fn double_click(_hwnd: isize, _x: i32, _y: i32) {}
    pub fn click(_hwnd: isize, _x: i32, _y: i32) {}
    pub fn native_double_click(_hwnd: isize) {}
    pub fn key(_hwnd: isize, _vk: u16) {}
}

impl App {
    pub fn auto_tick(&mut self, ctx: &egui::Context) {
        let Some(mut auto) = self.auto.take() else { return };
        #[cfg(not(windows))]
        shots::collect(ctx);
        let done = auto.run(self);
        let auto_done_ticking = auto.resting();
        let quiet = auto.quiet_until.map(|t| t.saturating_duration_since(Instant::now()));
        self.auto = Some(auto);
        match done {
            Some(code) => {
                crate::EXIT_CODE.store(code, std::sync::atomic::Ordering::Relaxed);
                self.notify.allow_quit();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            // Idle/voice roles have nothing left to do once connected: stop ticking so the app
            // can be measured at rest.
            None if auto_done_ticking => crate::repaint_watch::set_expected_fps(0.0),
            // An idle check: one wake-up at its end, nothing before (`Auto::quiet`).
            None if quiet.is_some_and(|q| !q.is_zero()) => {
                crate::repaint_watch::set_expected_fps(0.0);
                ctx.request_repaint_after(quiet.unwrap_or_default());
            }
            None => {
                // (The repaint watch counts this tick as declared, not as a loop.)
                crate::repaint_watch::set_expected_fps(10.0);
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }
    }
}

// ---------------------------------------------------------------- item 11: webcam

impl Auto {
    /// Frames of the stream whose label starts with `prefix` in a fresh stats report: `None` while
    /// waiting for the reply (the first call asks for it).
    fn stats_frames(&mut self, app: &mut App, prefix: &str) -> Option<u64> {
        if !self.stats_req {
            self.stats_req = true;
            self.seq_seen = app.stream_info_seq;
            app.backend.send(Cmd::Stats);
            return None;
        }
        if app.stream_info_seq == self.seq_seen {
            return None;
        }
        self.stats_req = false;
        Some(app.stream_info.iter().find(|s| s.0.starts_with(prefix)).map(|s| s.1).unwrap_or(0))
    }

    /// The hub's `camera` flag of voice member `id` and what the sidebar / tile icon rule makes of
    /// it (`None`: not in a voice channel we can see).
    fn hub_camera(app: &App, id: i64) -> Option<(bool, bool)> {
        app.voice_members.get(&id).map(|m| (m.camera, app.camera_flag_of(m)))
    }

    fn tile_state(key: &str) -> Option<vox_client_core::golive::StreamState> {
        vox_client_core::golive::Viewer::get().ok()?.state(key)
    }

    /// A: joins, turns the camera on once B is there (the synthetic camera with
    /// `VOX_FAKE_CAMERA=1`, else the default camera), optionally goes live at the same time
    /// (`--hwnd`), checks the mirrored self-view, holds, turns the camera off.
    pub(crate) fn run_cam_a(&mut self, app: &mut App) -> Option<i32> {
        use crate::app::CameraUi;
        use vox_client_core::voice::SELF_CAMERA_KEY;
        match self.step {
            0 => self.connect(app),
            1 => {
                if self.logged_in(app) {
                    let Some(vc) = Self::first_voice(app) else { return self.fail("no voice channel") };
                    app.join_voice(vc);
                    self.goto(2, "joining voice");
                } else if self.waited(30) || app.form.error.is_some() {
                    return self.fail(&format!("could not log in: {}", app.form.error.clone().unwrap_or_default()));
                }
            }
            2 => {
                if app.voice.channel.is_some() {
                    self.goto(3, "in voice; waiting for B");
                } else if self.waited(60) {
                    return self.fail("voice join timed out");
                }
            }
            3 => {
                if app.members_here().len() >= 2 && self.waited(2) {
                    app.toggle_camera();
                    self.goto(4, "pressed Câmera");
                } else if self.waited(120) {
                    return self.fail("B never joined voice");
                }
            }
            4 => match app.voice.camera.clone() {
                CameraUi::On { encoder, info } => {
                    tracing::info!(target: "autotest", "AUTOTEST A camera on: encoder={encoder} {info}");
                    if let Some(hwnd) = self.args.hwnd {
                        app.start_share(Pick::Window(hwnd), Preset::P720p30, false);
                        self.goto(5, "camera on; starting Go Live too");
                    } else {
                        self.goto(6, "camera on; holding");
                    }
                }
                CameraUi::Off => return self.fail(&format!("camera did not start: {:?}", app.notice)),
                CameraUi::Starting if self.waited(20) => return self.fail("camera still starting after 20 s"),
                CameraUi::Starting => {}
            },
            5 => {
                if matches!(app.voice.share, Share::Live { .. }) {
                    tracing::info!(target: "autotest", "AUTOTEST A camera and Go Live running together (camera={:?})", app.voice.camera);
                    self.goto(6, "camera + Go Live; holding");
                } else if app.voice.share == Share::Off && self.waited(2) {
                    return self.fail(&format!("Go Live did not start next to the camera: {:?}", app.notice));
                } else if self.waited(30) {
                    return self.fail("Go Live start timed out");
                }
            }
            6 => {
                if !self.self_view_checked && self.waited(4) {
                    let st = Self::tile_state(SELF_CAMERA_KEY);
                    let placed = app.video.placed.get(SELF_CAMERA_KEY).copied();
                    tracing::info!(target: "autotest", "AUTOTEST A self-view: center={:?} placed={placed:?} state={st:?}", app.center);
                    if placed.is_none() || !st.is_some_and(|s| s.presented > 0) {
                        return self.fail("the self-view tile is not placed / presenting");
                    }
                    // The hub echoes our own camera flag back (the server is the dev hub, which
                    // knows it) and the icon rule agrees.
                    let mine = app.my_id().and_then(|me| Self::hub_camera(app, me));
                    tracing::info!(target: "autotest", "AUTOTEST A own camera flag from the hub (flag, icon): {mine:?}");
                    if mine != Some((true, true)) {
                        return self.fail(&format!("the hub did not report my camera as on: {mine:?}"));
                    }
                    self.self_view_checked = true;
                    self.stats_req = false;
                }
                if self.self_view_checked && !self.cam_stats_logged && self.waited(self.args.hold_secs / 2) {
                    if let Some(f) = self.stats_frames(app, "camera") {
                        let preview = app.stream_info.iter().find(|s| s.0 == "camera-preview").map(|s| s.1).unwrap_or(0);
                        tracing::info!(target: "autotest", "AUTOTEST A camera frames sent so far: {f}, self-view frames: {preview}");
                        self.cam_stats_logged = true;
                    }
                }
                // With the synthetic camera, B may ask for the configuration run (it mutes itself).
                if camcfg::fake_camera() {
                    self.camcfg_a(app);
                }
                // Hold at least `hold_secs`, then until B signals that its checks are done (it
                // deafens itself): B's checks take ~50 s and must not race the hold.
                let me = app.my_id();
                let b_done = app.members_here().iter().any(|m| Some(m.user.id) != me && m.deafened);
                if self.waited(self.args.hold_secs) && (b_done || self.waited(self.args.hold_secs + B_DONE_GRACE_SECS)) {
                    if !b_done {
                        tracing::warn!(target: "autotest", "AUTOTEST A: B never signalled the end of its checks; turning the camera off anyway");
                    }
                    app.toggle_camera();
                    self.goto(7, "pressed Câmera again (off)");
                }
            }
            7 if self.waited(2) => {
                let st = Self::tile_state(SELF_CAMERA_KEY);
                let placed = app.video.placed.get(SELF_CAMERA_KEY).copied();
                tracing::info!(target: "autotest", "AUTOTEST A camera off: camera={:?} placed={placed:?} state={st:?} share={:?}", app.voice.camera, app.voice.share);
                if app.voice.camera != CameraUi::Off || placed.is_some() || st.is_some() {
                    return self.fail("the camera / self-view did not go away");
                }
                let mine = app.my_id().and_then(|me| Self::hub_camera(app, me));
                tracing::info!(target: "autotest", "AUTOTEST A own camera flag after off (flag, icon): {mine:?}");
                if mine != Some((false, false)) {
                    return self.fail(&format!("the hub still reports my camera as on: {mine:?}"));
                }
                if self.args.hwnd.is_some() {
                    if !matches!(app.voice.share, Share::Live { .. }) {
                        return self.fail("turning the camera off ended Go Live");
                    }
                    app.stop_share();
                }
                self.goto(8, "camera off verified");
            }
            8 if self.waited(4) => {
                app.leave_voice();
                self.goto(9, "leaving voice");
            }
            9 if self.waited(2) => return self.pass(),
            _ => {}
        }
        None
    }

    /// B: joins, sees A's camera tile with frames flowing, checks that hiding the call view
    /// (chat, minimized) pauses it and showing it resumes it, focuses it, opens the soundboard and
    /// a menu over part of it (cut out of the video, which keeps playing), and that A turning the
    /// camera off clears the tile.
    pub(crate) fn run_cam_b(&mut self, app: &mut App) -> Option<i32> {
        use vox_client_core::voice::camera_key;
        let key = self.cam_of.as_deref().map(camera_key);
        match self.step {
            0 => self.connect(app),
            1 => {
                if self.logged_in(app) {
                    let Some(vc) = Self::first_voice(app) else { return self.fail("no voice channel") };
                    app.join_voice(vc);
                    self.goto(2, "joining voice");
                } else if self.waited(30) || app.form.error.is_some() {
                    return self.fail(&format!("could not log in: {}", app.form.error.clone().unwrap_or_default()));
                }
            }
            2 => {
                if app.voice.channel.is_some() {
                    self.goto(3, "in voice; waiting for A's camera");
                } else if self.waited(60) {
                    return self.fail("voice join timed out");
                }
            }
            3 => {
                if let Some(id) = app.voice.cameras.iter().next().cloned() {
                    self.cam_of = Some(id.clone());
                    self.goto(4, &format!("camera of {id} is on; center={:?}", app.center));
                } else if self.waited(120) {
                    return self.fail("A's camera never came on");
                }
            }
            4 if self.waited(4) => {
                if let Some(f) = self.stats_frames(app, "cam-video") {
                    let key = key.expect("set in step 3");
                    let st = Self::tile_state(&key);
                    let placed = app.video.placed.get(&key).copied();
                    tracing::info!(target: "autotest", "AUTOTEST B camera tile (grid): frames={f} placed={placed:?} state={st:?}");
                    if placed.is_none() || !st.is_some_and(|s| s.presented > 0 && s.video.0 > 0) {
                        return self.fail("A's camera tile is not placed / presenting");
                    }
                    self.cam_frames = vec![f];
                    self.grid_rect = placed;
                    self.goto(5, "grid verified; holding (screenshot window)");
                }
            }
            5 if self.waited(5) => {
                if let Some(f) = self.stats_frames(app, "cam-video") {
                    let d = f - self.cam_frames[0];
                    tracing::info!(target: "autotest", "AUTOTEST B camera frames in the grid: +{d} in ~5 s");
                    // Real cameras drop to ~15 fps in dim light.
                    if d < 30 {
                        return self.fail(&format!("camera frames not flowing (+{d})"));
                    }
                    // The hub reports A's camera (flag) and the icon shows (sidebar / tile rule).
                    let a = self.cam_of.as_deref().and_then(|i| i.parse::<i64>().ok()).unwrap_or(-1);
                    let flag = Self::hub_camera(app, a);
                    tracing::info!(target: "autotest", "AUTOTEST B A's camera flag from the hub (flag, icon): {flag:?}");
                    if flag != Some((true, true)) {
                        return self.fail(&format!("the hub did not report A's camera as on: {flag:?}"));
                    }
                    let Some(ch) = Self::first_text(app) else { return self.fail("no text channel") };
                    app.select_text(ch);
                    self.goto(6, "switched to the chat (tiles hidden)");
                }
            }
            6 if self.waited(2) => {
                if let Some(f) = self.stats_frames(app, "cam-video") {
                    let placed = app.video.placed.get(key.as_deref().unwrap_or_default()).copied();
                    if placed.is_some() {
                        return self.fail("the camera tile stayed visible over the chat");
                    }
                    self.cam_frames = vec![f];
                    self.goto(7, "chat shown; measuring the paused camera");
                }
            }
            7 if self.waited(4) => {
                if let Some(f) = self.stats_frames(app, "cam-video") {
                    let d = f - self.cam_frames[0];
                    tracing::info!(target: "autotest", "AUTOTEST B camera frames while the chat is shown: +{d} in ~4 s");
                    if d > 15 {
                        return self.fail(&format!("the hidden camera kept streaming (+{d})"));
                    }
                    app.show_call();
                    self.goto(8, "back to the call view");
                }
            }
            8 if self.waited(3) => {
                if let Some(f) = self.stats_frames(app, "cam-video") {
                    self.cam_frames = vec![f];
                    self.goto(9, "measuring the resumed camera");
                }
            }
            9 if self.waited(3) => {
                if let Some(f) = self.stats_frames(app, "cam-video") {
                    let d = f - self.cam_frames[0];
                    let placed = app.video.placed.get(key.as_deref().unwrap_or_default()).copied();
                    tracing::info!(target: "autotest", "AUTOTEST B camera resumed: +{d} in ~3 s, placed={placed:?}");
                    if d < 20 || placed.is_none() {
                        return self.fail(&format!("the camera did not resume (+{d}, placed={placed:?})"));
                    }
                    app.pin_only(self.cam_of.as_deref().and_then(|i| i.parse().ok()).map(TileKey::Member));
                    self.goto(10, "focused the camera tile");
                }
            }
            10 if self.waited(5) => {
                let placed = app.video.placed.get(key.as_deref().unwrap_or_default()).copied();
                tracing::info!(target: "autotest", "AUTOTEST B camera focused: placed={placed:?} (grid was {:?})", self.grid_rect);
                let big = |r: Option<(i32, i32, i32, i32)>| r.map(|r| r.2 * r.3).unwrap_or(0);
                if big(placed) <= big(self.grid_rect) {
                    return self.fail("the focused camera tile is not bigger than in the grid");
                }
                self.goto(35, "airspace: an overlay over part of the camera tile");
            }
            35 => match self.clip_b(app, key.as_deref().unwrap_or_default()) {
                Some(Ok(())) => {
                    crate::app::simulate_minimized(true);
                    self.goto(11, "minimized the window");
                }
                Some(Err(e)) => return self.fail(&e),
                None => {}
            },
            11 if self.waited(2) => {
                if let Some(f) = self.stats_frames(app, "cam-video") {
                    self.cam_frames = vec![f];
                    self.goto(12, "measuring while minimized");
                }
            }
            12 if self.waited(4) => {
                if let Some(f) = self.stats_frames(app, "cam-video") {
                    let d = f - self.cam_frames[0];
                    tracing::info!(target: "autotest", "AUTOTEST B camera frames while minimized: +{d} in ~4 s");
                    if d > 15 {
                        return self.fail(&format!("the camera kept streaming while minimized (+{d})"));
                    }
                    crate::app::simulate_minimized(false);
                    self.goto(13, "restored the window");
                }
            }
            13 if self.waited(3) => {
                if let Some(f) = self.stats_frames(app, "cam-video") {
                    self.cam_frames = vec![f];
                    self.goto(14, "measuring after restore");
                }
            }
            14 if self.waited(3) => {
                if let Some(f) = self.stats_frames(app, "cam-video") {
                    let d = f - self.cam_frames[0];
                    tracing::info!(target: "autotest", "AUTOTEST B camera after restore: +{d} in ~3 s");
                    if d < 20 {
                        return self.fail(&format!("the camera did not resume after restore (+{d})"));
                    }
                    let secs = self.args.watch_secs;
                    self.goto(15, &format!("watching the camera {secs}s more (measurements)"));
                }
            }
            15 if self.waited(self.args.watch_secs) => {
                if vox_client_core::golive::camera::probe_enabled() {
                    // Webcam configuration run: muting is A's cue to start changing its settings.
                    app.toggle_mute();
                    self.goto(30, "asked A to mirror / rotate / zoom (muted: A's cue); watching its picture");
                } else {
                    // Tell A the checks are done (A turns the camera off once it sees it).
                    app.toggle_deafen();
                    self.goto(16, "checks done (deafened: A's signal); waiting for A to turn the camera off");
                }
            }
            30 => match self.camcfg_b(app) {
                Some(Ok(())) => {
                    tracing::info!(target: "autotest", "AUTOTEST B camera configuration verified");
                    app.toggle_deafen();
                    self.goto(16, "checks done (deafened: A's signal); waiting for A to turn the camera off");
                }
                Some(Err(e)) => return self.fail(&e),
                None => {}
            },
            16 => {
                let a = self.cam_of.as_deref().and_then(|i| i.parse::<i64>().ok()).unwrap_or(-1);
                if app.voice.cameras.is_empty() && Self::hub_camera(app, a).is_some_and(|(flag, _)| !flag) {
                    self.goto(17, "A's camera is off (room and hub agree)");
                } else if self.waited(90) {
                    return self.fail("A never turned the camera off");
                }
            }
            17 if self.waited(2) => {
                let k = key.unwrap_or_default();
                let placed = app.video.placed.get(&k).copied();
                let st = Self::tile_state(&k);
                tracing::info!(target: "autotest", "AUTOTEST B after camera off: placed={placed:?} state={st:?} pins={:?}", app.call.pins);
                if placed.is_some() || st.is_some() || app.call.pins.iter().any(|p| matches!(p, TileKey::Member(_))) {
                    return self.fail("the camera tile was not cleared");
                }
                let a = self.cam_of.as_deref().and_then(|i| i.parse::<i64>().ok()).unwrap_or(-1);
                let flag = Self::hub_camera(app, a);
                tracing::info!(target: "autotest", "AUTOTEST B A's camera flag after off (flag, icon): {flag:?}");
                if flag != Some((false, false)) {
                    return self.fail(&format!("A's camera icon is still on: {flag:?}"));
                }
                self.goto(18, "tile cleared; waiting for A to leave");
            }
            18 => {
                if app.members_here().len() <= 1 || self.waited(30) {
                    app.leave_voice();
                    self.goto(19, "leaving voice");
                }
            }
            19 if self.waited(2) => return self.pass(),
            _ => {}
        }
        None
    }
}

// ---------------------------------------------------------------- item 8a: preview of my own live

impl Auto {
    /// A fresh frame count of my preview and the seconds since the previous sample (`None` while
    /// waiting for the stats reply).
    fn pv_take(&mut self, app: &mut App) -> Option<(u64, f64)> {
        let f = self.stats_frames(app, "share-preview")?;
        let dt = self.pv_at.elapsed().as_secs_f64();
        let d = f.saturating_sub(self.pv_frames);
        self.pv_frames = f;
        self.pv_at = Instant::now();
        let _ = dt;
        Some((d, dt))
    }

    fn pv_state() -> Option<vox_client_core::golive::StreamState> {
        Self::tile_state(vox_client_core::golive::SELF_SHARE_KEY)
    }

    fn pv_placed(app: &App) -> Option<(i32, i32, i32, i32)> {
        app.video.placed.get(vox_client_core::golive::SELF_SHARE_KEY).copied()
    }

    fn note_video(&mut self) {
        if let Some(s) = Self::pv_state() {
            if s.video != (0, 0) && self.pv_sizes.last() != Some(&s.video) {
                self.pv_sizes.push(s.video);
            }
        }
    }

    /// Role `a --preview`, steps 30..=59: the preview of my own live in my stream tile follows
    /// the tile (grid rate, focus rate, paused with the chat view / minimized / covered), keeps
    /// working across a source switch, and never plays my own share's audio.
    fn run_a_preview(&mut self, app: &mut App) -> Option<i32> {
        use vox_client_core::golive::SELF_SHARE_KEY;
        if !matches!(app.voice.share, Share::Live { .. }) {
            return self.fail("the live ended during the preview checks");
        }
        self.note_video();
        let log = |what: &str| tracing::info!(target: "autotest", "AUTOTEST A preview: {what}");
        match self.step {
            // Frames arrive in my own tile.
            30 if self.waited(3) => {
                self.pv_take(app)?;
                let st = Self::pv_state();
                let placed = Self::pv_placed(app);
                let viewers = app.my_id().map(|me| app.viewers_of(me)).unwrap_or_default();
                log(&format!("tile placed={placed:?} state={st:?} viewers={viewers:?} center={:?}", app.center));
                if placed.is_none() || !st.is_some_and(|s| s.presented > 0 && s.video.0 > 0) {
                    return self.fail("my stream tile is not placed / presenting the preview");
                }
                self.grid_rect = placed;
                self.goto(31, "preview shown; measuring the grid rate");
            }
            31 if self.waited(6) => {
                let (d, dt) = self.pv_take(app)?;
                let rate = d as f64 / dt;
                let st = Self::pv_state();
                log(&format!("grid: {d} frames in {dt:.1}s = {rate:.1} fps, video {:?}", st.map(|s| s.video)));
                if !(2.5..=8.0).contains(&rate) {
                    return self.fail(&format!("grid preview rate {rate:.1} fps, expected about 5"));
                }
                self.pv_video = st.map(|s| s.video).unwrap_or_default();
                app.pin_only(app.my_id().map(TileKey::Stream));
                self.goto(32, "focused my stream tile");
            }
            32 if self.waited(2) => {
                if self.pv_take(app).is_some() {
                    self.goto(33, "focused; measuring the focus rate");
                }
            }
            33 if self.waited(5) => {
                let (d, dt) = self.pv_take(app)?;
                let rate = d as f64 / dt;
                let st = Self::pv_state();
                let placed = Self::pv_placed(app);
                let area = |r: Option<(i32, i32, i32, i32)>| r.map(|r| r.2 * r.3).unwrap_or(0);
                log(&format!("focused: {d} frames in {dt:.1}s = {rate:.1} fps, video {:?} (grid {:?}), tile {placed:?} (grid {:?})", st.map(|s| s.video), self.pv_video, self.grid_rect));
                if rate < 10.5 {
                    return self.fail(&format!("focused preview rate {rate:.1} fps, expected about 15"));
                }
                if area(placed) <= area(self.grid_rect) || !st.is_some_and(|s| s.video.0 > self.pv_video.0) {
                    return self.fail("the focused preview is not bigger than in the grid");
                }
                app.pin_only(None);
                self.goto(34, "unfocused");
            }
            34 if self.waited(2) => {
                if self.pv_take(app).is_some() {
                    self.goto(35, "back in the grid; measuring");
                }
            }
            35 if self.waited(4) => {
                let (d, dt) = self.pv_take(app)?;
                let rate = d as f64 / dt;
                log(&format!("unfocused again: {rate:.1} fps"));
                if rate > 8.0 {
                    return self.fail(&format!("the preview stayed fast after unfocusing ({rate:.1} fps)"));
                }
                let Some(ch) = Self::first_text(app) else { return self.fail("no text channel") };
                app.select_text(ch);
                self.goto(36, "switched to the chat (tile hidden)");
            }
            // Chat view: the preview stops completely.
            36 if self.waited(2) => {
                if self.pv_take(app).is_some() {
                    if Self::pv_placed(app).is_some() {
                        return self.fail("my preview stayed visible over the chat");
                    }
                    self.goto(37, "chat shown; measuring the paused preview");
                }
            }
            37 if self.waited(5) => {
                let (d, dt) = self.pv_take(app)?;
                log(&format!("chat view: +{d} frames in {dt:.1}s"));
                if d > 1 {
                    return self.fail(&format!("the hidden preview kept producing frames (+{d})"));
                }
                app.show_call();
                self.goto(38, "back to the call view");
            }
            38 if self.waited(2) => {
                if self.pv_take(app).is_some() {
                    self.goto(39, "call view; measuring the resumed preview");
                }
            }
            39 if self.waited(4) => {
                let (d, dt) = self.pv_take(app)?;
                log(&format!("resumed: +{d} frames in {dt:.1}s"));
                if d < 8 || Self::pv_placed(app).is_none() {
                    return self.fail(&format!("the preview did not resume (+{d})"));
                }
                crate::app::simulate_minimized(true);
                self.goto(40, "minimized the window");
            }
            // Minimized: the preview stops completely.
            40 if self.waited(2) => {
                if self.pv_take(app).is_some() {
                    self.goto(41, "minimized; measuring");
                }
            }
            41 if self.waited(4) => {
                let (d, dt) = self.pv_take(app)?;
                log(&format!("minimized: +{d} frames in {dt:.1}s"));
                if d > 1 {
                    return self.fail(&format!("the preview kept producing frames while minimized (+{d})"));
                }
                crate::app::simulate_minimized(false);
                self.goto(42, "restored the window");
            }
            42 if self.waited(2) => {
                if self.pv_take(app).is_some() {
                    self.goto(43, "restored; measuring");
                }
            }
            43 if self.waited(4) => {
                let (d, dt) = self.pv_take(app)?;
                log(&format!("restored: +{d} frames in {dt:.1}s"));
                if d < 8 {
                    return self.fail(&format!("the preview did not resume after restore (+{d})"));
                }
                app.open_settings();
                self.goto(44, "opened the settings dialog (may cover the tile)");
            }
            // A dialog over the whole tile: the video child window hides, so the preview stops (over
            // part of it the part is cut out and the preview keeps running).
            44 if self.waited(2) => {
                if self.pv_take(app).is_some() {
                    self.goto(45, "settings open; measuring");
                }
            }
            45 if self.waited(4) => {
                let (d, dt) = self.pv_take(app)?;
                let clip = app.video.clips.get(vox_client_core::golive::SELF_SHARE_KEY).cloned();
                let covered = clip == Some(crate::call::clip::Clip::Hidden) && Self::pv_placed(app).is_none();
                log(&format!("settings open: covered={covered} clip={clip:?} +{d} frames in {dt:.1}s"));
                if covered && d > 1 {
                    return self.fail(&format!("the covered preview kept producing frames (+{d})"));
                }
                if matches!(clip, Some(crate::call::clip::Clip::Cut(_))) && d < 2 {
                    return self.fail(&format!("the partly covered preview stopped (+{d})"));
                }
                if clip.is_none() {
                    log("the dialog did not overlap my tile; the covered case is not exercised in this layout");
                }
                app.settings_ui.open = false;
                self.goto(46, "settings closed");
            }
            46 if self.waited(2) => {
                if self.pv_take(app).is_some() {
                    self.goto(47, "dialog closed; measuring");
                }
            }
            47 if self.waited(3) => {
                let (d, dt) = self.pv_take(app)?;
                log(&format!("after the dialog: +{d} frames in {dt:.1}s"));
                if d < 6 || Self::pv_placed(app).is_none() {
                    return self.fail(&format!("the preview did not come back after the dialog (+{d})"));
                }
                match self.args.switch_hwnd {
                    Some(h2) => {
                        self.pv_video = Self::pv_state().map(|s| s.video).unwrap_or_default();
                        app.switch_share(Pick::Window(h2), true);
                        self.goto(48, &format!("Trocar tela -> window {h2} (preview was {:?})", self.pv_video));
                    }
                    None => self.goto(52, "no --switch-hwnd: skipping the switch part"),
                }
            }
            // Trocar tela: the preview follows the new source.
            48 => {
                if app.voice.switching_to.is_none() {
                    if let Some(n) = app.notice.clone() {
                        return self.fail(&format!("switch reported: {n}"));
                    }
                    self.goto(49, "switch done");
                } else if self.waited(20) {
                    return self.fail("the switch never completed");
                }
            }
            49 if self.waited(2) => {
                if self.pv_take(app).is_some() {
                    self.goto(50, "switched; measuring");
                }
            }
            50 if self.waited(4) => {
                let (d, dt) = self.pv_take(app)?;
                let video = Self::pv_state().map(|s| s.video).unwrap_or_default();
                log(&format!("after the switch: +{d} frames in {dt:.1}s, video {video:?} (before {:?}), sizes seen {:?}", self.pv_video, self.pv_sizes));
                if d < 8 {
                    return self.fail(&format!("the preview stopped after the source switch (+{d})"));
                }
                if video == self.pv_video {
                    return self.fail("the preview picture did not change size with the new source");
                }
                self.goto(51, "the preview follows the switch");
            }
            // Optional pause for a screenshot of the tile (`--hold` beyond 20 s).
            51 => {
                if self.waited(self.args.hold_secs.saturating_sub(20)) {
                    self.goto(52, "checking the subscriptions");
                }
            }
            // My own share's audio is never subscribed or played.
            52 if self.waited(1) => {
                self.stats_frames(app, "share-video")?;
                let me = app.my_id().map(|i| i.to_string()).unwrap_or_default();
                let labels: Vec<&str> = app.stream_info.iter().map(|s| s.0.as_str()).collect();
                log(&format!("streams: {labels:?}"));
                let own_recv = labels.iter().any(|l| (l.starts_with("watch-audio:") || l.starts_with("voice:") || l.starts_with("watch-video:")) && l.ends_with(&format!(":{me}")));
                if own_recv || app.is_watching(&me) {
                    return self.fail("my own share is subscribed (audio or video)");
                }
                if !labels.contains(&"share-video") || !labels.contains(&"share-preview") {
                    return self.fail("the stats lack share-video / share-preview");
                }
                log(&format!("no subscription to my own share; tile sizes seen {:?}; PASS", self.pv_sizes));
                let _ = SELF_SHARE_KEY;
                app.stop_share();
                self.goto(7, "stopping Go Live");
            }
            _ => {}
        }
        None
    }
}

// ---------------------------------------------------------------- item 9: several streams

/// Seconds a `multi-a` keeps its live past the start waiting for B to finish, before failing.
const MULTI_A_LIMIT_SECS: u64 = 300;

impl Auto {
    /// True once a fresh stats reply arrived (the first call asks for it).
    fn fresh_stats(&mut self, app: &mut App) -> bool {
        if !self.stats_req {
            self.stats_req = true;
            self.seq_seen = app.stream_info_seq;
            app.backend.send(Cmd::Stats);
            return false;
        }
        if app.stream_info_seq == self.seq_seen {
            return false;
        }
        self.stats_req = false;
        true
    }

    fn stat_count(app: &App, label: &str) -> Option<u64> {
        app.stream_info.iter().find(|s| s.0 == label).map(|s| s.1)
    }

    /// Video frames received per watched streamer (`mb_ids` order); `None` when one has no
    /// subscribed video in the stats.
    fn mb_video(&self, app: &App) -> Vec<Option<u64>> {
        self.mb_ids.iter().map(|id| Self::stat_count(app, &format!("watch-video:{id}"))).collect()
    }

    fn mb_audio_now(&self, app: &App) -> Vec<Option<u64>> {
        self.mb_ids.iter().map(|id| Self::stat_count(app, &format!("watch-audio:{id}"))).collect()
    }

    fn mb_presented_now(&self) -> Vec<u64> {
        self.mb_ids.iter().map(|id| Self::tile_state(id).map(|s| s.presented).unwrap_or(0)).collect()
    }

    fn mb_placed(&self, app: &App) -> Vec<Option<(i32, i32, i32, i32)>> {
        self.mb_ids.iter().map(|id| app.video.placed.get(id).copied()).collect()
    }

    fn deltas(now: &[Option<u64>], before: &[u64]) -> Vec<u64> {
        now.iter().zip(before).map(|(n, b)| n.unwrap_or(0).saturating_sub(*b)).collect()
    }

    fn area(r: Option<(i32, i32, i32, i32)>) -> i64 {
        r.map(|r| r.2 as i64 * r.3 as i64).unwrap_or(0)
    }

    /// A1 / A2: join, go live with `--hwnd` (1080p30) once three people are in the room, keep it
    /// running until B is done with this stream (B left my viewer list, or deafened itself: its
    /// "all checks done" signal), then stop and leave. B must have shown up in my viewer list.
    pub(crate) fn run_multi_a(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => self.connect(app),
            1 => {
                if self.logged_in(app) {
                    let Some(vc) = Self::first_voice(app) else { return self.fail("no voice channel") };
                    app.join_voice(vc);
                    self.goto(2, "joining voice");
                } else if self.waited(30) || app.form.error.is_some() {
                    return self.fail(&format!("could not log in: {}", app.form.error.clone().unwrap_or_default()));
                }
            }
            2 => {
                if app.voice.channel.is_some() {
                    self.goto(3, "in voice; waiting for the other streamer and B");
                } else if let Some(e) = app.voice.error.clone() {
                    return self.fail(&format!("voice join failed: {e}"));
                } else if self.waited(60) {
                    return self.fail("voice join timed out");
                }
            }
            3 => {
                if app.members_here().len() >= 3 {
                    let Some(hwnd) = self.args.hwnd else { return self.fail("no --hwnd given") };
                    let names: Vec<_> = app.members_here().iter().map(|m| m.user.username.clone()).collect();
                    app.start_share(Pick::Window(hwnd), Preset::P1080p30, true);
                    self.goto(4, &format!("voice members {names:?}; starting Go Live 1080p30"));
                } else if self.waited(120) {
                    return self.fail("the others never joined voice");
                }
            }
            4 => {
                if let Share::Live { encoder } = &app.voice.share {
                    let enc = encoder.clone();
                    self.goto(5, &format!("Go Live running, encoder={enc}; waiting for B"));
                } else if app.voice.share == Share::Off && self.waited(2) {
                    return self.fail(&format!("Go Live did not start: {:?}", app.notice));
                } else if self.waited(30) {
                    return self.fail("Go Live start timed out");
                }
            }
            5 => {
                let viewers = app.my_id().map(|me| app.viewers_of(me)).unwrap_or_default();
                if !self.viewer_seen && !viewers.is_empty() {
                    self.viewer_seen = true;
                    tracing::info!(target: "autotest", "AUTOTEST multi-A: viewers of my live = {viewers:?} ({})", App::viewers_text(&viewers));
                } else if self.viewer_seen && !self.viewer_cleared && viewers.is_empty() {
                    self.viewer_cleared = true;
                    tracing::info!(target: "autotest", "AUTOTEST multi-A: B left my viewer list ({})", App::viewers_text(&viewers));
                }
                let me = app.my_id();
                let b_done = app.members_here().iter().any(|m| Some(m.user.id) != me && m.deafened);
                if (self.viewer_seen && (self.viewer_cleared || b_done)) || (b_done && self.waited(10)) || self.waited(MULTI_A_LIMIT_SECS) {
                    tracing::info!(target: "autotest", "AUTOTEST multi-A: done (viewer seen={} cleared={} B done={b_done})", self.viewer_seen, self.viewer_cleared);
                    if app.viewers_supported && !self.viewer_seen {
                        return self.fail("B never showed up in my viewer list");
                    }
                    if !b_done && !self.viewer_cleared {
                        return self.fail("B never finished");
                    }
                    app.stop_share();
                    self.goto(6, "stopping Go Live");
                }
            }
            6 if self.waited(3) => {
                app.leave_voice();
                self.goto(7, "leaving voice");
            }
            7 if self.waited(2) => return self.pass(),
            _ => {}
        }
        None
    }

    /// B: watches A1 and A2 at the same time. Checks: frames on both tiles, focus + strip (the
    /// strip one still plays, drawn at the strip rate), the chat view pauses both at the SFU (audio
    /// keeps flowing) and the call view resumes them, fullscreen of the double-clicked strip
    /// tile, one pop-out window per stream (still playing with the chat shown), unwatching one
    /// keeps the other, and the end of a watched live closes it with the message. With
    /// `--mode one|two|strip|chat` it only watches for `--watch-secs` (CPU / RAM measurements).
    pub(crate) fn run_multi_b(&mut self, app: &mut App) -> Option<i32> {
        let mode = self.args.mode.clone().unwrap_or_else(|| "checks".into());
        let log = |what: &str| tracing::info!(target: "autotest", "AUTOTEST multi-B: {what}");
        match self.step {
            0 => self.connect(app),
            1 => {
                if self.logged_in(app) {
                    let Some(vc) = Self::first_voice(app) else { return self.fail("no voice channel") };
                    app.join_voice(vc);
                    self.goto(2, "joining voice");
                } else if self.waited(30) || app.form.error.is_some() {
                    return self.fail(&format!("could not log in: {}", app.form.error.clone().unwrap_or_default()));
                }
            }
            2 => {
                if app.voice.channel.is_some() {
                    self.goto(3, "in voice; waiting for both lives");
                } else if self.waited(60) {
                    return self.fail("voice join timed out");
                }
            }
            3 => {
                let me = app.my_id();
                let mut live: Vec<(String, String)> =
                    app.members_here().iter().filter(|m| m.streaming && Some(m.user.id) != me).map(|m| (m.user.username.clone(), m.user.id.to_string())).collect();
                live.sort();
                if live.len() >= 2 && self.waited(5) {
                    self.mb_ids = live.iter().take(2).map(|(_, id)| id.clone()).collect();
                    app.show_call();
                    let n = if mode == "one" { 1 } else { 2 };
                    for id in self.mb_ids.clone().iter().take(n) {
                        // The tile's "Assistir transmissão".
                        if !app.watch(id) {
                            return self.fail("watch refused");
                        }
                    }
                    if n == 1 {
                        self.mb_ids.truncate(1);
                    }
                    self.goto(4, &format!("{live:?} are live; pressed Assistir transmissão on {n}"));
                } else if self.waited(150) {
                    return self.fail("the two lives never started");
                }
            }
            4 => {
                if self.mb_ids.iter().all(|id| app.is_watching(id)) {
                    log(&format!("watching {:?} (focus={:?}, center={:?})", app.voice.watching, app.focused_stream(), app.center));
                    if mode == "checks" {
                        self.goto(5, "both watched; letting frames flow");
                    } else {
                        match mode.as_str() {
                            "strip" => app.pin_only(self.mb_ids.first().and_then(|k| k.parse().ok()).map(TileKey::Stream)),
                            "chat" => {
                                if let Some(ch) = Self::first_text(app) {
                                    app.select_text(ch);
                                }
                            }
                            _ => {}
                        }
                        let secs = self.args.watch_secs;
                        self.goto(90, &format!("measurement mode {mode}: holding {secs}s"));
                    }
                } else if self.waited(30) {
                    return self.fail(&format!("watch did not start for both: {:?}", app.voice.watching));
                }
            }
            5 if self.waited(4) => {
                if self.fresh_stats(app) {
                    self.mb_frames = self.mb_video(app).iter().map(|f| f.unwrap_or(0)).collect();
                    self.goto(6, &format!("frames so far {:?}", self.mb_frames));
                }
            }
            6 if self.waited(4) => {
                if !self.fresh_stats(app) {
                    return None;
                }
                let now = self.mb_video(app);
                let d = Self::deltas(&now, &self.mb_frames);
                let audio = self.mb_audio_now(app);
                let placed = self.mb_placed(app);
                let states: Vec<_> = self.mb_ids.iter().map(|id| Self::tile_state(id)).collect();
                log(&format!("grid: frames +{d:?} in ~4 s, audio chunks {audio:?}, placed {placed:?}, states {states:?}"));
                if d.iter().any(|&x| x <= 10) {
                    return self.fail(&format!("frames are not flowing on both tiles (+{d:?})"));
                }
                if audio.iter().any(|a| a.is_none()) {
                    return self.fail("a watched stream's audio is not subscribed");
                }
                if placed.iter().any(|p| p.is_none()) || !states.iter().all(|s| s.is_some_and(|s| !s.popped_out && s.presented > 0 && s.video.0 > 0)) {
                    return self.fail("both tiles must be placed and presenting");
                }
                let (s0, s1) = (Self::area(placed[0]), Self::area(placed[1]));
                if (s0 - s1).abs() * 50 > s0.max(s1) {
                    return self.fail("the two stream tiles of the grid differ in size");
                }
                let my_name = app.me.as_ref().map(|u| u.username.clone()).unwrap_or_default();
                for id in &self.mb_ids {
                    let viewers = id.parse::<i64>().map(|i| app.viewers_of(i)).unwrap_or_default();
                    log(&format!("viewers of {id} as the hub says: {viewers:?}"));
                    if app.viewers_supported && !viewers.contains(&my_name) {
                        return self.fail(&format!("I am not in {id}'s viewer list"));
                    }
                }
                app.pin_only(self.mb_ids.first().and_then(|k| k.parse().ok()).map(TileKey::Stream));
                self.goto(7, "focused A1 (A2 goes to the strip)");
            }
            7 if self.waited(3) => {
                let placed = self.mb_placed(app);
                let states: Vec<_> = self.mb_ids.iter().map(|id| Self::tile_state(id)).collect();
                log(&format!("focused: placed {placed:?}, states {states:?}"));
                if Self::area(placed[0]) <= 2 * Self::area(placed[1]) || placed[1].is_none() {
                    return self.fail("the focused stream is not big with the other in the strip");
                }
                let caps: Vec<u32> = states.iter().map(|s| s.map(|s| s.max_fps).unwrap_or(99)).collect();
                if caps != vec![0, crate::call::STRIP_FPS] {
                    return self.fail(&format!("drawing caps {caps:?}, expected [0, {}]", crate::call::STRIP_FPS));
                }
                self.mb_presented = self.mb_presented_now();
                self.mb_at = Instant::now();
                self.stats_req = false;
                self.goto(8, "measuring the focused / strip drawing rates");
            }
            8 if self.waited(4) => {
                if !self.fresh_stats(app) {
                    return None;
                }
                let dt = self.mb_at.elapsed().as_secs_f64();
                let pres: Vec<f64> = self.mb_presented_now().iter().zip(&self.mb_presented).map(|(n, b)| n.saturating_sub(*b) as f64 / dt).collect();
                let frames = self.mb_video(app);
                log(&format!("drawn fps focused={:.1} strip={:.1} over {dt:.1}s; frames received {frames:?}", pres[0], pres[1]));
                if pres[1] < 2.0 || pres[1] > crate::call::STRIP_FPS as f64 + 3.0 {
                    return self.fail(&format!("the strip stream is drawn at {:.1} fps, expected about {}", pres[1], crate::call::STRIP_FPS));
                }
                if pres[0] < pres[1] + 5.0 {
                    return self.fail(&format!("the focused stream is drawn at {:.1} fps, not faster than the strip", pres[0]));
                }
                let Some(ch) = Self::first_text(app) else { return self.fail("no text channel") };
                app.select_text(ch);
                self.goto(9, "chat shown (both tiles hidden)");
            }
            // Hidden: both are paused at the SFU after the grace period; the audio goes on.
            9 if self.waited(3) => {
                if self.fresh_stats(app) {
                    if self.mb_placed(app).iter().any(|p| p.is_some()) {
                        return self.fail("a stream stayed visible over the chat");
                    }
                    self.mb_frames = self.mb_video(app).iter().map(|f| f.unwrap_or(0)).collect();
                    self.mb_audio = self.mb_audio_now(app).iter().map(|f| f.unwrap_or(0)).collect();
                    self.goto(10, "chat; measuring the paused streams");
                }
            }
            10 if self.waited(4) => {
                if !self.fresh_stats(app) {
                    return None;
                }
                let d = Self::deltas(&self.mb_video(app), &self.mb_frames);
                let a = Self::deltas(&self.mb_audio_now(app), &self.mb_audio);
                log(&format!("chat view: video frames +{d:?}, audio chunks +{a:?} in ~4 s"));
                if d.iter().any(|&x| x > 10) {
                    return self.fail(&format!("a hidden stream kept receiving video (+{d:?})"));
                }
                if a.iter().any(|&x| x < 100) {
                    return self.fail(&format!("a hidden stream's audio stopped (+{a:?})"));
                }
                app.show_call();
                self.goto(11, "back to the call view");
            }
            11 if self.waited(1) => {
                if self.fresh_stats(app) {
                    self.mb_frames = self.mb_video(app).iter().map(|f| f.unwrap_or(0)).collect();
                    self.goto(12, "measuring the resumed streams");
                }
            }
            12 if self.waited(4) => {
                if !self.fresh_stats(app) {
                    return None;
                }
                let d = Self::deltas(&self.mb_video(app), &self.mb_frames);
                let placed = self.mb_placed(app);
                log(&format!("resumed: frames +{d:?} in ~4 s, placed {placed:?}, focus {:?}", app.focused_stream()));
                if d.iter().any(|&x| x <= 10) || placed.iter().any(|p| p.is_none()) {
                    return self.fail(&format!("the streams did not resume (+{d:?})"));
                }
                crate::app::simulate_minimized(true);
                self.goto(30, "minimized the window");
            }
            // Minimized: both paused at the SFU too; restored: both back.
            30 if self.waited(3) => {
                if self.fresh_stats(app) {
                    self.mb_frames = self.mb_video(app).iter().map(|f| f.unwrap_or(0)).collect();
                    self.goto(31, "minimized; measuring");
                }
            }
            31 if self.waited(4) => {
                if !self.fresh_stats(app) {
                    return None;
                }
                let d = Self::deltas(&self.mb_video(app), &self.mb_frames);
                log(&format!("minimized: frames +{d:?} in ~4 s"));
                if d.iter().any(|&x| x > 10) {
                    return self.fail(&format!("a stream kept receiving video while minimized (+{d:?})"));
                }
                crate::app::simulate_minimized(false);
                self.goto(32, "restored the window");
            }
            32 if self.waited(2) => {
                if self.fresh_stats(app) {
                    self.mb_frames = self.mb_video(app).iter().map(|f| f.unwrap_or(0)).collect();
                    self.goto(33, "restored; measuring");
                }
            }
            33 if self.waited(4) => {
                if !self.fresh_stats(app) {
                    return None;
                }
                let d = Self::deltas(&self.mb_video(app), &self.mb_frames);
                let placed = self.mb_placed(app);
                log(&format!("restored: frames +{d:?} in ~4 s, placed {placed:?}"));
                if d.iter().any(|&x| x <= 10) || placed.iter().any(|p| p.is_none()) {
                    return self.fail(&format!("the streams did not resume after the restore (+{d:?})"));
                }
                // Double-click on the strip tile: fullscreen of that one.
                let parent = app.video.parent.unwrap_or_default();
                let (x, y, w, h) = placed[1].expect("checked");
                self.win_rect = app.video.parent.and_then(win::window_rect);
                win::double_click(parent, x + w / 2, y + h / 2);
                self.goto(13, "double-clicked A2 in the strip");
            }
            13 if self.waited(2) => {
                let parent = app.video.parent.unwrap_or_default();
                let (wr, mr) = (win::window_rect(parent), win::monitor_rect(parent));
                let placed = self.mb_placed(app);
                log(&format!("fullscreen: on={} key={:?} window={wr:?} monitor={mr:?} placed={placed:?}", app.call.fullscreen, app.call.fullscreen_key));
                let covers = mr.is_some_and(|m| wr == Some(m) && placed[1].is_some_and(|p| p.2 == m.2 - m.0 && p.3 == m.3 - m.1));
                if !app.call.fullscreen || app.call.fullscreen_key.as_ref() != self.mb_ids.get(1) || !covers || placed[0].is_some() {
                    return self.fail("the double-clicked strip stream did not go fullscreen alone");
                }
                win::key(parent, win::VK_ESCAPE);
                self.goto(14, "pressed Esc");
            }
            14 if self.waited(2) => {
                let placed = self.mb_placed(app);
                let wr = app.video.parent.and_then(win::window_rect);
                log(&format!("after fullscreen: on={} focus={:?} window={wr:?} (was {:?}) placed={placed:?}", app.call.fullscreen, app.focused_stream(), self.win_rect));
                if app.call.fullscreen || app.focused_stream().as_ref() != self.mb_ids.first() || placed.iter().any(|p| p.is_none()) || wr != self.win_rect {
                    return self.fail("leaving fullscreen did not bring back the focused view");
                }
                let a2 = self.mb_ids[1].clone();
                app.pop_out_stream(&a2, true);
                self.goto(15, "Abrir em janela on A2");
            }
            15 if self.waited(2) => {
                let a2 = Self::tile_state(&self.mb_ids[1]);
                let placed = self.mb_placed(app);
                log(&format!("A2 popped out: state {a2:?}, placed {placed:?}"));
                if !a2.is_some_and(|s| s.popped_out && s.popout_hwnd != 0 && s.max_fps == 0) || placed[1].is_some() || placed[0].is_none() {
                    return self.fail("A2 did not move to its own window (with A1 still inside)");
                }
                let a1 = self.mb_ids[0].clone();
                app.pop_out_stream(&a1, true);
                self.goto(16, "Abrir em janela on A1 too");
            }
            16 if self.waited(2) => {
                let states: Vec<_> = self.mb_ids.iter().map(|id| Self::tile_state(id)).collect();
                log(&format!("both popped out: {states:?}"));
                let hwnds: Vec<isize> = states.iter().map(|s| s.filter(|s| s.popped_out).map(|s| s.popout_hwnd).unwrap_or(0)).collect();
                if hwnds.contains(&0) || hwnds[0] == hwnds[1] {
                    return self.fail("each stream needs its own window");
                }
                let Some(ch) = Self::first_text(app) else { return self.fail("no text channel") };
                app.select_text(ch);
                self.goto(17, "chat shown with both in their own windows");
            }
            17 if self.waited(3) => {
                if self.fresh_stats(app) {
                    self.mb_frames = self.mb_video(app).iter().map(|f| f.unwrap_or(0)).collect();
                    self.goto(18, "measuring the popped-out streams");
                }
            }
            18 if self.waited(4) => {
                if !self.fresh_stats(app) {
                    return None;
                }
                let d = Self::deltas(&self.mb_video(app), &self.mb_frames);
                log(&format!("popped out, chat shown: frames +{d:?} in ~4 s"));
                if d.iter().any(|&x| x <= 10) {
                    return self.fail(&format!("a stream in its own window was paused (+{d:?})"));
                }
                for id in self.mb_ids.clone() {
                    app.pop_out_stream(&id, false);
                }
                app.show_call();
                self.goto(19, "brought both back; call view");
            }
            19 if self.waited(3) => {
                let states: Vec<_> = self.mb_ids.iter().map(|id| Self::tile_state(id)).collect();
                let placed = self.mb_placed(app);
                log(&format!("back inline: placed {placed:?}, states {states:?}"));
                if placed.iter().any(|p| p.is_none()) || states.iter().any(|s| !s.is_some_and(|s| !s.popped_out)) {
                    return self.fail("the streams did not come back inline");
                }
                let a1 = self.mb_ids[0].clone();
                app.unwatch(&a1);
                self.goto(20, "Parar de assistir on A1 (A2 must go on)");
            }
            20 if self.waited(3) => {
                if !self.fresh_stats(app) {
                    return None;
                }
                let (a1, a2) = (self.mb_ids[0].clone(), self.mb_ids[1].clone());
                let labels: Vec<&str> = app.stream_info.iter().map(|s| s.0.as_str()).collect();
                log(&format!("after unwatching A1: watching {:?}, focus {:?}, A1 state {:?}, placed {:?}, streams {labels:?}", app.voice.watching, app.focused_stream(), Self::tile_state(&a1), self.mb_placed(app)));
                if app.voice.watching != vec![a2.clone()] || app.focused_stream().is_some() || app.is_pinned(TileKey::Stream(a1.parse().unwrap_or(-1))) {
                    return self.fail("unwatching A1 did not leave exactly A2 (and the focus on A1 should go)");
                }
                if Self::tile_state(&a1).is_some() || app.video.placed.contains_key(&a1) || labels.iter().any(|l| l.ends_with(&format!(":{a1}")) && l.starts_with("watch-")) {
                    return self.fail("A1's viewer / subscriptions are still there");
                }
                self.mb_frames = vec![Self::stat_count(app, &format!("watch-video:{a2}")).unwrap_or(0)];
                self.goto(21, "A2 alone; measuring");
            }
            21 if self.waited(4) => {
                if !self.fresh_stats(app) {
                    return None;
                }
                let a2 = self.mb_ids[1].clone();
                let f = Self::stat_count(app, &format!("watch-video:{a2}")).unwrap_or(0);
                let d = f.saturating_sub(self.mb_frames[0]);
                log(&format!("A2 still playing: +{d} frames in ~4 s, placed {:?}", app.video.placed.get(&a2)));
                if d <= 10 || !app.video.placed.contains_key(&a2) {
                    return self.fail(&format!("A2 stopped when A1 was unwatched (+{d})"));
                }
                self.goto(22, "waiting for A1 to end its live (it saw me leave its viewer list)");
            }
            22 => {
                let a1 = self.mb_ids[0].parse::<i64>().unwrap_or(-1);
                if !app.voice_members.get(&a1).is_some_and(|m| m.streaming) {
                    log(&format!("A1 ended its live; watching {:?}, toast {:?}", app.voice.watching, app.call.toast.as_ref().map(|t| &t.0)));
                    if app.voice.watching != vec![self.mb_ids[1].clone()] {
                        return self.fail("A1's end changed the other stream");
                    }
                    app.toggle_deafen();
                    self.goto(23, "deafened (A2's signal to end its live)");
                } else if self.waited(60) {
                    return self.fail("A1 never ended its live");
                }
            }
            23 => {
                if app.voice.watching.is_empty() {
                    let toast = app.call.toast.as_ref().map(|t| t.0.clone()).unwrap_or_default();
                    let a2 = self.mb_ids[1].parse::<i64>().map(|i| app.name_of(i)).unwrap_or_default();
                    log(&format!("A2 ended its live: toast {toast:?}"));
                    if !toast.contains("foi encerrada") || !toast.contains(&a2) {
                        return self.fail("no 'transmissão encerrada' message for A2");
                    }
                    self.goto(24, "the watched live ended with the message");
                } else if self.waited(90) {
                    return self.fail("A2 never ended its live");
                }
            }
            24 if self.waited(2) => {
                app.leave_voice();
                self.goto(25, "leaving voice");
            }
            25 if self.waited(3) => return self.pass(),
            // Measurement runs: hold, one stats line now and then, then signal the end.
            90 => {
                if self.waited(self.args.watch_secs) {
                    if !self.fresh_stats(app) {
                        return None;
                    }
                    log(&format!("measurement {mode} done: frames {:?}, watching {:?}", self.mb_video(app), app.voice.watching));
                    app.toggle_deafen();
                    self.goto(91, "deafened (the streamers' signal to end)");
                }
            }
            91 => {
                let me = app.my_id();
                let any_live = app.members_here().iter().any(|m| m.streaming && Some(m.user.id) != me);
                if !any_live || self.waited(60) {
                    app.leave_voice();
                    self.goto(25, "leaving voice");
                }
            }
            _ => {}
        }
        None
    }
}

// ---------------------------------------------------------------- audio-only Go Live

/// State of the `audio-a` / `audio-b` roles.
#[derive(Default)]
pub struct AoState {
    /// Audio chunks of the stream under test at the last sample, and when.
    chunks: u64,
    at: Option<Instant>,
    /// Role `audio-b`: the streamer's identity.
    streamer: String,
    /// Role `audio-a`: the kind-change legs are done / which one runs.
    kind_leg: u32,
}

/// Audio below this counts as silence (the test tones sit near -15 dBFS).
const AO_SILENCE_DBFS: f64 = -50.0;
/// The streamer waits this long for the listener before failing.
const AO_A_LIMIT_SECS: u64 = 300;

impl Auto {
    fn ao_log(&self, what: &str) {
        tracing::info!(target: "autotest", "AUTOTEST {:?}: {what}", self.role);
    }

    /// The stats entry of the audio stream `label` in the last report.
    fn ao_audio<'a>(app: &'a App, label: &str) -> Option<&'a crate::backend::AudioInfo> {
        app.audio_info.iter().find(|a| a.label == label)
    }

    fn ao_labels(app: &App) -> Vec<&str> {
        app.stream_info.iter().map(|s| s.0.as_str()).collect()
    }

    /// A: "Só áudio" streamer. Goes live with nothing but audio (a stereo test tone, or the
    /// sound of the process `--audio-pid`), checks there is no video anywhere, holds while B
    /// listens, optionally switches to another app (`--switch-pid`, "Trocar app") and becomes a
    /// screen share and back (`--kinds` with `--switch-hwnd`), then stops when B is done.
    pub(crate) fn run_audio_a(&mut self, app: &mut App) -> Option<i32> {
        use vox_client_core::golive::SELF_SHARE_KEY;
        let mode = self.args.mode.clone().unwrap_or_else(|| "tone".into());
        match self.step {
            0 => self.connect(app),
            1 => {
                if self.logged_in(app) {
                    let Some(vc) = Self::first_voice(app) else { return self.fail("no voice channel") };
                    app.join_voice(vc);
                    self.goto(2, "joining voice");
                } else if self.waited(30) || app.form.error.is_some() {
                    return self.fail(&format!("could not log in: {}", app.form.error.clone().unwrap_or_default()));
                }
            }
            2 => {
                if app.voice.channel.is_some() {
                    self.goto(3, "in voice; waiting for B");
                } else if let Some(e) = app.voice.error.clone() {
                    return self.fail(&format!("voice join failed: {e}"));
                } else if self.waited(60) {
                    return self.fail("voice join timed out");
                }
            }
            3 => {
                if app.members_here().len() >= 2 && self.waited(2) {
                    let high = self.args.audio_high;
                    // Go Live > "Só áudio" > the app (or, in the autotest, a synthetic tone).
                    match mode.as_str() {
                        "app" => {
                            let Some(pid) = self.args.audio_pid else { return self.fail("no --audio-pid given") };
                            app.start_audio_share(Pick::App(pid), high);
                        }
                        _ => app.start_audio_share_source(vox_client_core::golive::ShareSource::AudioTone, None, high),
                    }
                    self.goto(4, &format!("starting an audio-only Go Live ({mode}, {})", if high { "Alta 256 kbps" } else { "Normal 128 kbps" }));
                } else if self.waited(120) {
                    return self.fail("B never joined voice");
                }
            }
            4 => {
                if matches!(app.voice.share, Share::Live { .. }) {
                    if !app.voice.share_audio_only {
                        return self.fail("the live is not marked audio-only");
                    }
                    self.goto(5, "audio-only Go Live running");
                } else if app.voice.share == Share::Off && self.waited(2) {
                    return self.fail(&format!("Go Live did not start: {:?}", app.notice));
                } else if self.waited(30) {
                    return self.fail("Go Live start timed out");
                }
            }
            // My side of the wire: no video track, no encoder, no preview; just the audio.
            5 if self.waited(5) => {
                if !self.fresh_stats(app) {
                    return None;
                }
                let labels: Vec<String> = Self::ao_labels(app).iter().map(|s| s.to_string()).collect();
                let audio = Self::ao_audio(app, "share-audio").cloned();
                self.ao_log(&format!("my streams {labels:?}; share-audio {audio:?}"));
                if labels.iter().any(|l| l == "share-video" || l == "share-preview") {
                    return self.fail("an audio-only live must have no video stream or preview");
                }
                let Some(audio) = audio else { return self.fail("no share-audio stream") };
                if audio.chunks < 200 {
                    return self.fail(&format!("only {} audio chunks sent", audio.chunks));
                }
                if Self::tile_state(SELF_SHARE_KEY).is_some() {
                    return self.fail("an audio-only live opened a preview tile");
                }
                let mine = app.my_id().and_then(|me| app.voice_members.get(&me)).map(|m| m.streaming);
                self.ao_log(&format!("hub says my streaming flag is {mine:?}"));
                if mine != Some(true) {
                    return self.fail("the hub does not show me as streaming");
                }
                let hub_flag = app.my_id().and_then(|me| app.voice_members.get(&me)).map(|m| m.audio_only);
                self.ao_log(&format!("hub says my audio_only flag is {hub_flag:?}"));
                if hub_flag != Some(true) {
                    return self.fail("the hub does not show my live as audio-only");
                }
                self.goto(6, "no video anywhere; waiting for B to listen");
            }
            6 => {
                let viewers = app.my_id().map(|me| app.viewers_of(me)).unwrap_or_default();
                if !self.viewer_seen && !viewers.is_empty() {
                    self.viewer_seen = true;
                    self.ao_log(&format!("viewers of my live = {viewers:?} ({})", App::viewers_text(&viewers)));
                }
                if self.viewer_seen && self.waited(6) {
                    self.goto(if self.args.switch_pid.is_some() { 7 } else { 9 }, "B is listening");
                } else if !app.viewers_supported && self.waited(30) {
                    self.goto(9, "viewer lists unsupported; carrying on");
                } else if self.waited(AO_A_LIMIT_SECS) {
                    return self.fail("B never showed up in my viewer list");
                }
            }
            // "Trocar app": the live goes on with another app's sound.
            7 => {
                let Some(pid2) = self.args.switch_pid else { return self.fail("no --switch-pid") };
                app.switch_share(Pick::App(pid2), true);
                self.goto(8, &format!("Trocar app -> pid {pid2}"));
            }
            8 => {
                if app.voice.switching_to.is_none() && self.waited(1) {
                    if let Some(n) = app.notice.clone() {
                        return self.fail(&format!("the switch reported: {n}"));
                    }
                    if !matches!(app.voice.share, Share::Live { .. }) || !app.voice.share_audio_only {
                        return self.fail("the live did not survive the switch as audio-only");
                    }
                    self.ao_log(&format!("switched; share_pick={:?}", app.voice.share_pick));
                    self.goto(9, "switch done; still live, audio-only");
                } else if self.waited(20) {
                    return self.fail("the switch never completed");
                }
            }
            // The live becomes a screen share and back (viewers stay in the room and follow).
            9 if self.args.kinds && self.args.switch_hwnd.is_some() => {
                let Some(hwnd) = self.args.switch_hwnd else { return self.fail("no --switch-hwnd") };
                if self.waited(4) {
                    app.start_share(Pick::Window(hwnd), Preset::P720p30, false);
                    self.ao.kind_leg = 1;
                    self.goto(10, "restarting as a screen share (Tela + áudio)");
                }
            }
            10 => {
                let is_screen = matches!(app.voice.share, Share::Live { .. }) && !app.voice.share_audio_only;
                if is_screen && self.waited(14) {
                    app.start_audio_share_source(vox_client_core::golive::ShareSource::AudioTone, None, self.args.audio_high);
                    self.ao.kind_leg = 2;
                    self.goto(11, "restarting as audio-only again");
                } else if app.voice.share == Share::Off && self.waited(3) {
                    return self.fail(&format!("the screen share did not start: {:?}", app.notice));
                } else if self.waited(40) {
                    return self.fail("the screen share restart timed out");
                }
            }
            11 => {
                let back = matches!(app.voice.share, Share::Live { .. }) && app.voice.share_audio_only;
                if back && self.waited(12) {
                    self.goto(12, "audio-only again; waiting for B");
                } else if app.voice.share == Share::Off && self.waited(3) {
                    return self.fail(&format!("the audio-only restart failed: {:?}", app.notice));
                } else if self.waited(40) {
                    return self.fail("the audio-only restart timed out");
                }
            }
            // Hold until B says it is done (it deafens itself) or leaves my viewer list.
            9 | 12 => {
                let viewers = app.my_id().map(|me| app.viewers_of(me)).unwrap_or_default();
                let me = app.my_id();
                let b_done = app.members_here().iter().any(|m| Some(m.user.id) != me && m.deafened);
                if self.viewer_seen && viewers.is_empty() {
                    self.viewer_cleared = true;
                }
                // (B leaves my viewer list and comes back in its own checks, so only its deafen
                // ends my live.)
                if (b_done || self.waited(AO_A_LIMIT_SECS)) && self.waited(self.args.hold_secs.min(30)) {
                    self.ao_log(&format!("done (B done={b_done}, viewer cleared={})", self.viewer_cleared));
                    if app.viewers_supported && !self.viewer_seen {
                        return self.fail("B never showed up in my viewer list");
                    }
                    app.stop_share();
                    self.goto(13, "stopping Go Live");
                }
            }
            13 if self.waited(3) => {
                app.leave_voice();
                self.goto(14, "leaving voice");
            }
            14 if self.waited(2) => return self.pass(),
            _ => {}
        }
        None
    }

    /// B: listener. Sees the audio-only tile (not "Assistir"), presses "Ouvir", checks that no
    /// video is subscribed or drawn, that stereo audio arrives with sound in it (a tone), that it
    /// is in A's viewer list, that the sound survives A's source switch and (with `--kinds`) A
    /// turning into a screen share and back.
    pub(crate) fn run_audio_b(&mut self, app: &mut App) -> Option<i32> {
        let mode = self.args.mode.clone().unwrap_or_else(|| "tone".into());
        match self.step {
            0 => self.connect(app),
            1 => {
                if self.logged_in(app) {
                    let Some(vc) = Self::first_voice(app) else { return self.fail("no voice channel") };
                    app.join_voice(vc);
                    self.goto(2, "joining voice");
                } else if self.waited(30) || app.form.error.is_some() {
                    return self.fail(&format!("could not log in: {}", app.form.error.clone().unwrap_or_default()));
                }
            }
            2 => {
                if app.voice.channel.is_some() {
                    self.goto(3, "in voice; waiting for A's audio-only live");
                } else if let Some(e) = app.voice.error.clone() {
                    return self.fail(&format!("voice join failed: {e}"));
                } else if self.waited(60) {
                    return self.fail("voice join timed out");
                }
            }
            3 => {
                let me = app.my_id();
                let live = app.members_here().iter().find(|m| m.streaming && Some(m.user.id) != me).map(|m| (m.user.id, m.user.username.clone()));
                if let Some((id, name)) = live {
                    let key = id.to_string();
                    if app.voice.audio_only.contains(&key) && self.waited(3) {
                        let member = app.voice_members.get(&id).cloned();
                        let tile_audio_only = member.as_ref().is_some_and(|m| app.stream_is_audio_only(m));
                        self.ao_log(&format!("{name} is live, audio-only={tile_audio_only}, hub flag {:?}; pressing Ouvir", member.as_ref().map(|m| m.audio_only)));
                        if !member.as_ref().is_some_and(|m| m.audio_only) {
                            return self.fail("the hub does not flag the live as audio-only");
                        }
                        if !tile_audio_only {
                            return self.fail("the tile does not know the live is audio-only");
                        }
                        self.ao.streamer = key.clone();
                        app.show_call();
                        // The tile's "Ouvir" button.
                        if !app.watch(&key) {
                            return self.fail("watch refused");
                        }
                        self.goto(4, "pressed Ouvir");
                    } else if self.waited(60) && !app.voice.audio_only.contains(&key) {
                        return self.fail("A is live but the room never said the live is audio-only");
                    }
                } else if self.waited(150) {
                    return self.fail("A never went live");
                }
            }
            4 => {
                if app.is_watching(&self.ao.streamer) {
                    self.goto(5, "listening; letting audio flow");
                } else if self.waited(20) {
                    return self.fail("listening never started");
                }
            }
            5 if self.waited(4) => {
                if self.fresh_stats(app) {
                    let label = format!("watch-audio:{}", self.ao.streamer);
                    self.ao.chunks = Self::ao_audio(app, &label).map(|a| a.chunks).unwrap_or(0);
                    self.ao.at = Some(Instant::now());
                    self.goto(6, &format!("first sample: {} chunks", self.ao.chunks));
                }
            }
            6 if self.waited(4) => {
                if !self.fresh_stats(app) {
                    return None;
                }
                let id = self.ao.streamer.clone();
                let label = format!("watch-audio:{id}");
                let labels: Vec<String> = Self::ao_labels(app).iter().map(|s| s.to_string()).collect();
                let info = Self::ao_audio(app, &label).cloned();
                self.ao_log(&format!("streams {labels:?}; {label}: {info:?}"));
                // No video subscription, no surface, no tile window.
                if labels.iter().any(|l| l.starts_with("watch-video")) {
                    return self.fail("a video stream is subscribed to an audio-only live");
                }
                if app.video.placed.contains_key(&id) || Self::tile_state(&id).is_some() {
                    return self.fail("an audio-only live got a video surface");
                }
                let Some(info) = info else { return self.fail("the live's audio is not subscribed") };
                let secs = self.ao.at.map(|t| t.elapsed().as_secs_f64()).unwrap_or(4.0);
                let rate = (info.chunks.saturating_sub(self.ao.chunks)) as f64 / secs;
                self.ao_log(&format!("audio chunks/s {rate:.0}, rms {:?} dBFS, side {:?} dBFS, {:?} kbps, codec [{}]", info.rms_dbfs, info.side_dbfs, info.kbps, info.codec));
                if rate < 80.0 {
                    return self.fail(&format!("audio is not flowing ({rate:.0} chunks/s)"));
                }
                if !info.rms_dbfs.is_some_and(|d| d > AO_SILENCE_DBFS) {
                    return self.fail(&format!("the audio is silent ({:?} dBFS)", info.rms_dbfs));
                }
                if !info.codec.to_ascii_lowercase().contains("opus") {
                    return self.fail(&format!("not Opus: {}", info.codec));
                }
                let stereo_flag = info.codec.contains("stereo=1");
                self.ao_log(&format!("stereo: negotiated flag {stereo_flag}, side energy {:?}", info.side_dbfs));
                if mode == "tone" {
                    // Left and right differ (440 / 660 Hz), so a real stereo path has side energy.
                    if !info.side_dbfs.is_some_and(|d| d > -40.0) {
                        return self.fail(&format!("the audio is not stereo on the wire (side {:?} dBFS, codec {})", info.side_dbfs, info.codec));
                    }
                }
                // Am I in A's viewer list (item 7)?
                let my_name = app.me.as_ref().map(|u| u.username.clone()).unwrap_or_default();
                let viewers = id.parse::<i64>().map(|i| app.viewers_of(i)).unwrap_or_default();
                self.ao_log(&format!("viewers of {id} as the hub says: {viewers:?}"));
                if app.viewers_supported && !viewers.contains(&my_name) {
                    return self.fail("I am not in the streamer's viewer list");
                }
                // The sound must survive A's switch of source: watch for another while.
                self.ao.chunks = info.chunks;
                self.ao.at = Some(Instant::now());
                let next = if self.args.switch_pid.is_some() { 7 } else if self.args.kinds { 8 } else { 20 };
                self.goto(next, "stereo audio verified");
            }
            // The sound goes on while A switches app (`--switch-pid`) for about 12 s.
            7 if self.waited(12) => {
                if !self.fresh_stats(app) {
                    return None;
                }
                let label = format!("watch-audio:{}", self.ao.streamer);
                let info = Self::ao_audio(app, &label).cloned();
                self.ao_log(&format!("after A's switch window: {label}: {info:?}"));
                let Some(info) = info else { return self.fail("the audio went away") };
                if info.chunks.saturating_sub(self.ao.chunks) < 500 || !info.rms_dbfs.is_some_and(|d| d > AO_SILENCE_DBFS) {
                    return self.fail("the audio stopped or went silent after the source switch");
                }
                self.goto(if self.args.kinds { 8 } else { 20 }, "audio still flowing after the switch");
            }
            // `--kinds`: A turns into a screen share. The listener's tile becomes a video one.
            8 => {
                let id = self.ao.streamer.clone();
                if !app.voice.audio_only.contains(&id) {
                    self.ao_log(&format!("A became a screen share: watching {:?}, placed {:?}", app.voice.watching, app.video.placed.get(&id)));
                    self.goto(9, "kind flipped to video");
                } else if self.waited(60) {
                    return self.fail("A's live never flipped to a screen share");
                }
            }
            9 if self.waited(5) => {
                if self.fresh_stats(app) {
                    let id = self.ao.streamer.clone();
                    self.ao.chunks = Self::stat_count(app, &format!("watch-video:{id}")).unwrap_or(0);
                    self.goto(10, &format!("video frames so far {}", self.ao.chunks));
                }
            }
            10 if self.waited(3) => {
                if !self.fresh_stats(app) {
                    return None;
                }
                let id = self.ao.streamer.clone();
                let frames = Self::stat_count(app, &format!("watch-video:{id}")).unwrap_or(0);
                let audio = Self::ao_audio(app, &format!("watch-audio:{id}")).cloned();
                let placed = app.video.placed.get(&id).copied();
                self.ao_log(&format!("screen phase: video frames {} -> {frames}, placed {placed:?}, audio {audio:?}", self.ao.chunks));
                if !app.is_watching(&id) {
                    return self.fail("I stopped watching when the live changed kind");
                }
                if app.streamer_hub_audio_only(&id) != Some(false) {
                    return self.fail("the hub still flags the screen share as audio-only");
                }
                if frames <= self.ao.chunks + 10 {
                    return self.fail("no video arrived after the live became a screen share");
                }
                if placed.is_none() {
                    return self.fail("the screen share has no video surface in the tile");
                }
                self.goto(11, "screen share verified; waiting for A to go audio-only again");
            }
            11 => {
                let id = self.ao.streamer.clone();
                if app.voice.audio_only.contains(&id) {
                    self.goto(12, "kind flipped back to audio-only");
                } else if self.waited(60) {
                    return self.fail("A's live never became audio-only again");
                }
            }
            12 if self.waited(5) => {
                if !self.fresh_stats(app) {
                    return None;
                }
                let id = self.ao.streamer.clone();
                let labels: Vec<String> = Self::ao_labels(app).iter().map(|s| s.to_string()).collect();
                let audio = Self::ao_audio(app, &format!("watch-audio:{id}")).cloned();
                self.ao_log(&format!("audio-only again: streams {labels:?}, placed {:?}, tile {:?}, audio {audio:?}", app.video.placed.get(&id), Self::tile_state(&id)));
                if !app.is_watching(&id) {
                    return self.fail("I stopped watching when the live went back to audio-only");
                }
                if app.streamer_hub_audio_only(&id) != Some(true) {
                    return self.fail("the hub does not flag the live as audio-only again");
                }
                if labels.iter().any(|l| l.starts_with("watch-video")) || app.video.placed.contains_key(&id) || Self::tile_state(&id).is_some() {
                    return self.fail("the video of the screen share was not released");
                }
                if !audio.is_some_and(|a| a.rms_dbfs.is_some_and(|d| d > AO_SILENCE_DBFS)) {
                    return self.fail("the audio is silent after going back to audio-only");
                }
                self.goto(20, "back to audio-only verified");
            }
            // `--watch-secs` longer than the default: keep listening (screenshots, CPU runs).
            20 if !self.waited(self.args.watch_secs.saturating_sub(9)) => {}
            20 => {
                // Per-stream volume and mute (item 2) apply to the music too.
                let id = self.ao.streamer.clone();
                if let Ok(uid) = id.parse::<i64>() {
                    let name = app.name_of(uid);
                    let mut pref = app.settings.user_audio(&name);
                    pref.stream_muted = true;
                    pref.stream_volume_pct = 50;
                    if let Some(u) = app.voice_members.get(&uid).map(|m| m.user.clone()) {
                        app.set_user_pref(&u, pref, true);
                    }
                }
                self.goto(21, "muted the stream and set 50% (item 2 controls)");
            }
            21 if self.waited(2) => {
                // "Parar de ouvir": gone from the viewer list and the stats.
                let id = self.ao.streamer.clone();
                app.unwatch(&id);
                self.goto(22, "Parar de ouvir");
            }
            22 if self.waited(3) => {
                let id = self.ao.streamer.clone();
                if app.is_watching(&id) {
                    return self.fail("still listening after Parar de ouvir");
                }
                if !self.fresh_stats(app) {
                    return None;
                }
                let labels: Vec<String> = Self::ao_labels(app).iter().map(|s| s.to_string()).collect();
                if labels.iter().any(|l| l.ends_with(&format!(":{id}")) && l.starts_with("watch-")) {
                    return self.fail("the stream's subscriptions are still there");
                }
                // The tile offers "Ouvir" again; pressing it works a second time.
                if !app.watch(&id) {
                    return self.fail("watch refused the second time");
                }
                self.goto(23, "pressed Ouvir again");
            }
            23 if self.waited(4) => {
                let id = self.ao.streamer.clone();
                if !app.is_watching(&id) {
                    return self.fail("the second Ouvir did not start");
                }
                app.toggle_deafen();
                self.goto(24, "listening again; deafened (A's signal to end its live)");
            }
            24 => {
                let id = self.ao.streamer.parse::<i64>().unwrap_or(-1);
                if !app.voice_members.get(&id).is_some_and(|m| m.streaming) {
                    let toast = app.call.toast.as_ref().map(|t| t.0.clone()).unwrap_or_default();
                    self.ao_log(&format!("A ended its live: watching {:?}, toast {toast:?}", app.voice.watching));
                    if !app.voice.watching.is_empty() {
                        return self.fail("still 'watching' after the live ended");
                    }
                    self.goto(25, "the live ended and the listening closed");
                } else if self.waited(90) {
                    return self.fail("A never ended its live");
                }
            }
            25 if self.waited(2) => {
                app.leave_voice();
                self.goto(26, "leaving voice");
            }
            26 if self.waited(3) => return self.pass(),
            _ => {}
        }
        None
    }
}

impl Auto {
    /// C: a third client that logs in and never joins voice. It must learn from the hub alone
    /// (`VoiceMember::audio_only`) that A's live is audio-only: the music note and "Ouvir"
    /// decision is right although the voice room (the LiveKit fallback) says nothing about A.
    /// With `--kinds` it follows A's live to a screen share and back; at the end the flag is
    /// cleared again.
    pub(crate) fn run_audio_c(&mut self, app: &mut App) -> Option<i32> {
        // The hub never lists the flag on someone who is not streaming.
        if let Some(m) = app.voice_members.values().find(|m| m.audio_only && !m.streaming) {
            return self.fail(&format!("{} has audio_only without streaming", m.user.username));
        }
        let me = app.my_id();
        let live = |app: &App| app.voice_members.values().find(|m| m.streaming && Some(m.user.id) != me).cloned();
        match self.step {
            0 => self.connect(app),
            1 => {
                if self.logged_in(app) {
                    self.goto(2, "logged in; staying out of voice, waiting for an audio-only live");
                } else if self.waited(30) || app.form.error.is_some() {
                    return self.fail(&format!("could not log in: {}", app.form.error.clone().unwrap_or_default()));
                }
            }
            2 => {
                if app.voice.channel.is_some() {
                    return self.fail("this client must stay out of voice");
                }
                if let Some(m) = live(app).filter(|m| m.audio_only) {
                    let key = m.user.id.to_string();
                    let room = app.voice.audio_only.contains(&key);
                    let decision = app.stream_is_audio_only(&m);
                    self.ao_log(&format!("{} is live: hub audio_only={}, my voice room says {room}, tile decision {decision}", m.user.username, m.audio_only));
                    if room {
                        return self.fail("the voice room knows about a live in a room I am not in (the test proves nothing)");
                    }
                    if !decision {
                        return self.fail("outside the room the tile ignored the hub's audio_only flag");
                    }
                    self.ao.streamer = key;
                    self.goto(if self.args.kinds { 3 } else { 5 }, "the hub's flag alone makes the live audio-only here");
                } else if self.waited(240) {
                    return self.fail("never saw an audio-only live");
                }
            }
            // `--kinds`: the live turns into a screen share and back.
            3 => {
                if let Some(m) = live(app).filter(|m| !m.audio_only) {
                    if app.stream_is_audio_only(&m) {
                        return self.fail("a screen share is still shown as audio-only");
                    }
                    self.goto(4, "the live became a screen share: flag cleared, tile follows");
                } else if live(app).is_none() {
                    return self.fail("the live ended before it became a screen share");
                } else if self.waited(120) {
                    return self.fail("the flag was never cleared by the switch to a screen share");
                }
            }
            4 => {
                if let Some(m) = live(app).filter(|m| m.audio_only) {
                    if !app.stream_is_audio_only(&m) {
                        return self.fail("back to audio-only, but the tile ignored the flag");
                    }
                    self.goto(5, "audio-only again, seen from outside");
                } else if live(app).is_none() {
                    return self.fail("the live ended before it went audio-only again");
                } else if self.waited(120) {
                    return self.fail("the flag never came back");
                }
            }
            // The live ends: streaming and the flag are cleared together.
            5 => {
                let id = self.ao.streamer.parse::<i64>().unwrap_or(-1);
                match app.voice_members.get(&id) {
                    Some(m) if m.streaming => {
                        if self.waited(300) {
                            return self.fail("A's live never ended");
                        }
                    }
                    other => {
                        let flag = other.map(|m| m.audio_only);
                        self.ao_log(&format!("A's live ended; member {} flag {flag:?}", if other.is_some() { "still in voice" } else { "gone" }));
                        if flag == Some(true) {
                            return self.fail("the audio_only flag outlived the live");
                        }
                        self.goto(6, "the flag went away with the live");
                    }
                }
            }
            6 if self.waited(1) => return self.pass(),
            _ => {}
        }
        None
    }
}
