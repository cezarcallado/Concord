//! v1.1 (E1): end-to-end encrypted calls, end to end in real apps (a child module of `autotest`).
//! The processes share flag files in `--mode <dir>`. Silent: `VOX_NO_PLAYBACK=1`, `VOX_NO_MIC=1`;
//! the microphones are synthetic tones (`VOX_TEST_MIC_TONE`), the camera the synthetic one
//! (`VOX_FAKE_CAMERA=1`), the Go Live a pre-encoded H.264 file (`VOX_TEST_H264`, the path the
//! NVENC / Media Foundation output takes), with `--hwnd` the test window, or with
//! `E2V_SOURCE=pattern` the moving test pattern.
//!
//! - `e2v-a` (the owner): sets its personal password, turns "Chamada criptografada de ponta a
//!   ponta" on for the first voice room (picture `e2v-canal`: the setting, with
//!   `VOX_AUTOTEST_SHOT_DIR`), joins it (it must say encrypted), turns the camera on and goes live,
//!   then holds until B is done.
//! - `e2v-b`: joins, watches A, and checks over a few seconds that A's voice decodes (~100 blocks
//!   of 10 ms a second, with sound), A's camera and A's live arrive (frames a second), A's media
//!   state is "ok" and A shows as speaking (picture `e2v-sala`: the call with the header's
//!   "Criptografada de ponta a ponta"). Then it lets C come and go (`e2v-join`) and measures the
//!   longest stretch without A's tone meanwhile (keys rotate twice): at most [`MAX_GAP_MS`].
//! - `e2v-c` (the newcomer): joins when told, must hear A within a few seconds (it only gets
//!   A's keys after A rotated), stays a little and leaves.
//! - `e2v-admin` (the owner, for `e2v-old`): turns the encryption on and watches that nobody gets
//!   into the room while the released v1.0.2 app (role `voice`) tries.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use vox_client_core::golive::{EncoderChoice, H264FileSource, ShareOptions, ShareSource};
use vox_client_core::MediaE2eeState;
use vox_proto::{ChannelId, UserId};

use super::Auto;
use crate::app::{App, CameraUi, Share};
use crate::backend::e2ee::E2eeStatus;
use crate::backend::Cmd;
use crate::roles::{AdminCmd, E2eeEdit, ServerTab};

/// The owner's personal password in these runs (a fresh dev hub each time).
const E2V_PW: &str = "e2v-senha-dono";
/// The longest stretch without A's tone B may hear while C comes and goes.
pub const MAX_GAP_MS: u64 = 500;

/// `E2V_PLAIN=1`: the same run in a room WITHOUT end-to-end encryption (a baseline for the
/// numbers; the encryption checks are skipped, everything else is the same).
fn plain() -> bool {
    std::env::var("E2V_PLAIN").as_deref() == Ok("1")
}

/// `E2V_RAMP_SECS=<n>` (v1.1 G5): B also checks that A's live, sent next to A's camera, reaches
/// [`RAMP_FPS`] (over a ~2 s window) within `n` seconds of B watching it. Run it with a camera that
/// spends its bitrate like a real one (`VOX_FAKE_CAMERA_NOISE`): the flat synthetic picture alone
/// sends a third of its share, and the bandwidth estimate then holds the live at 15 fps for good.
fn ramp_secs() -> Option<f64> {
    std::env::var("E2V_RAMP_SECS").ok().and_then(|v| v.parse::<f64>().ok()).filter(|s| *s > 0.0)
}

/// The frame rate the ramp check wants (of the live's 30).
pub const RAMP_FPS: f64 = 27.0;

/// `E2V_INVITE=1` (the cross-machine Windows <-> Mac run): A, once it is the owner, makes an invite
/// (it carries the hub's certificate) and writes it to `invite.txt` in the flag folder; B and C
/// join with only that code (pinned TLS, no server password), its address replaced by 127.0.0.1
/// (the hub listens on loopback; the other computer reaches it through an SSH forward).
fn invite_mode() -> bool {
    std::env::var("E2V_INVITE").as_deref() == Ok("1")
}

/// A's media state as the run expects it (Ok when encrypted; anything in the plain baseline).
fn state_ok(s: Option<MediaE2eeState>) -> bool {
    plain() || s == Some(MediaE2eeState::Ok)
}

#[derive(Default)]
pub struct State {
    /// A's identity (user id).
    a: Option<String>,
    chunks: u64,
    cam: u64,
    live: u64,
    at: Option<Instant>,
    spoke: bool,
    max_gap: u64,
    reports: u32,
    joined_at: Option<Instant>,
    last_log: Option<Instant>,
    /// The ramp check: when B could first watch, the last sample (time, live frames), the first
    /// time a window reached [`RAMP_FPS`] (s after the start) and the best window so far.
    ramp_t0: Option<Instant>,
    ramp_mark: Option<(Instant, u64)>,
    ramp_at: Option<f64>,
    ramp_best: f64,
    /// `E2V_INVITE`: A asked for the invite / wrote it; B and C: the pin of the code they used.
    invite_asked: Option<Instant>,
    invite_written: bool,
    pin: Option<[u8; 32]>,
}

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST e2v: {what}");
}

impl Auto {
    fn v_dir(&self) -> PathBuf {
        PathBuf::from(self.args.mode.clone().unwrap_or_else(|| ".".into()))
    }

    fn v_flag(&self, name: &str) -> bool {
        self.v_dir().join(name).exists()
    }

    fn v_set(&self, name: &str) {
        if let Err(e) = std::fs::write(self.v_dir().join(name), "ok") {
            tracing::warn!(error = %e, file = name, "AUTOTEST e2v: cannot write a flag file");
        }
    }

    /// Logged in and this computer registered on the hub's device list (it can encrypt).
    fn v_ready(&self, app: &App) -> bool {
        self.logged_in(app) && app.privacy.view.status == E2eeStatus::Active
    }

    /// B and C: log in, with only A's invite code under `E2V_INVITE` (waiting for it), else with
    /// the typed address. `Some(code)`: the run failed.
    fn v_connect(&mut self, app: &mut App) -> Option<i32> {
        if !invite_mode() {
            self.connect(app);
            return None;
        }
        let Ok(code) = std::fs::read_to_string(self.v_dir().join("invite.txt")) else {
            return if self.waited(240) { self.fail("no invite code from A") } else { None };
        };
        let decoded = match vox_proto::InviteCode::decode(code.trim()) {
            Ok(c) => c,
            Err(e) => return self.fail(&format!("the code does not decode: {e}")),
        };
        let Some(pin) = decoded.tls_pin else { return self.fail("the invite has no certificate (layout 1)") };
        let code = vox_proto::InviteCode { addrs: vec![vox_proto::InviteAddr::Lan(std::net::Ipv4Addr::LOCALHOST, 8)], ..decoded }.encode();
        log(&format!("joining with only A's invite (pinned TLS, server code {})", vox_proto::tls_server_code(&pin)));
        self.e2v.pin = Some(pin);
        app.form.url = code;
        app.form.user = self.args.name.clone();
        app.form.password.clear();
        app.connect();
        self.goto(1, "connecting with the invite");
        None
    }

    /// B and C under `E2V_INVITE`: the session is pinned to the invite's certificate.
    fn v_pinned(&mut self, app: &App) -> Option<i32> {
        if invite_mode() && app.tls.session != self.e2v.pin {
            return self.fail("the session is not pinned to the invite's certificate");
        }
        None
    }

    /// A under `E2V_INVITE`: make the invite and write it for B and C. `Some(Ok)`: written.
    fn v_write_invite(&mut self, app: &mut App) -> Option<Result<(), i32>> {
        if !invite_mode() || self.e2v.invite_written {
            return Some(Ok(()));
        }
        let Some(asked) = self.e2v.invite_asked else {
            app.create_invite();
            self.e2v.invite_asked = Some(Instant::now());
            return None;
        };
        if let Some(e) = app.host.inv.error.clone() {
            return Some(Err(self.fail(&format!("invite failed: {e}")).unwrap_or(1)));
        }
        if let Some((_, code)) = app.host.inv.last.clone() {
            let pin = vox_proto::InviteCode::decode(&code).ok().and_then(|c| c.tls_pin);
            if pin.is_none() || pin != app.tls.session {
                return Some(Err(self.fail("the invite does not carry this session's certificate").unwrap_or(1)));
            }
            if let Err(e) = std::fs::write(self.v_dir().join("invite.txt"), &code) {
                return Some(Err(self.fail(&format!("cannot write the code: {e}")).unwrap_or(1)));
            }
            log(&format!("invite with the hub's certificate written ({} chars)", code.len()));
            self.e2v.invite_written = true;
            return Some(Ok(()));
        }
        if asked.elapsed() > Duration::from_secs(20) {
            return Some(Err(self.fail("no invite came back").unwrap_or(1)));
        }
        None
    }

    fn v_member(app: &App, name: &str) -> Option<UserId> {
        app.members_here().iter().find(|m| m.user.username == name).map(|m| m.user.id)
    }

    /// The owner: logged in with its personal password set (so its Admin rights count).
    fn v_owner_ready(&mut self, app: &mut App) -> Option<Result<(), i32>> {
        match self.step {
            0 => {
                app.form.personal = E2V_PW.into();
                self.connect(app);
                None
            }
            1 => {
                if !self.logged_in(app) {
                    return if self.waited(30) { Some(Err(self.fail(&format!("could not log in: {:?}", app.form.error)).unwrap_or(1))) } else { None };
                }
                if app.me.as_ref().map(|u| u.role) != Some(vox_proto::Role::Owner) {
                    return Some(Err(self.fail("this name is not the hub's owner").unwrap_or(1)));
                }
                if app.roles.account.is_some_and(|a| a.must_set_password) {
                    app.set_personal_password(None, Some(E2V_PW.into()));
                    self.goto(2, "setting the owner's personal password");
                } else {
                    self.goto(3, "owner already protected");
                }
                None
            }
            2 => {
                if app.roles.account.is_some_and(|a| a.has_password && !a.must_set_password) {
                    self.goto(3, "personal password set");
                } else if self.waited(15) {
                    return Some(Err(self.fail(&format!("the personal password was not set: {:?}", app.roles.pw_msg)).unwrap_or(1)));
                }
                None
            }
            _ => Some(Ok(())),
        }
    }

    /// Turn the encryption of voice room `vc` on through the server window (the setting is shown,
    /// so it can be pictured). True once the channel list says so and the panel has its answer.
    fn v_turn_on(&mut self, app: &mut App, vc: ChannelId) -> bool {
        let on = app.channels.iter().any(|c| c.id == vc && c.e2ee);
        let info = app.roles.editing_e2ee.as_ref().and_then(|e| e.info.clone());
        on && info.is_some_and(|i| i.on)
    }

    // ------------------------------------------------------------ A

    pub(super) fn run_e2v_a(&mut self, app: &mut App) -> Option<i32> {
        app.repaint_ctx().request_repaint_after(Duration::from_millis(100));
        match self.v_owner_ready(app) {
            Some(Err(code)) => return Some(code),
            None => return None,
            Some(Ok(())) => {}
        }
        let vc = Self::first_voice(app)?;
        match self.step {
            3 => {
                match self.v_write_invite(app) {
                    Some(Err(code)) => return Some(code),
                    None => return None,
                    Some(Ok(())) => {}
                }
                if !self.v_ready(app) {
                    return if self.waited(45) { self.fail(&format!("e2ee not active here: {:?} {:?}", app.privacy.view.status, app.privacy.view.error)) } else { None };
                }
                if plain() {
                    self.v_set("e2ee-on");
                    app.join_voice(vc);
                    self.goto(5, "joining the room WITHOUT encryption (E2V_PLAIN baseline)");
                    return None;
                }
                app.open_server_settings();
                app.roles.tab = ServerTab::Channels;
                app.roles.editing_e2ee = Some(E2eeEdit { id: vc, busy: true, ..Default::default() });
                app.admin(AdminCmd::SetChannelE2ee { id: vc, on: true });
                self.goto(4, "turning the room's end-to-end encryption on (server window)");
            }
            4 => {
                if self.v_turn_on(app, vc) && self.waited(2) {
                    let outdated = app.roles.editing_e2ee.as_ref().and_then(|e| e.info.as_ref()).map(|i| i.outdated.len()).unwrap_or(0);
                    log(&format!("the room is encrypted; {outdated} people would be left out"));
                    super::shots::save(app, "e2v-canal");
                    app.roles.server_open = false;
                    app.roles.editing_e2ee = None;
                    self.v_set("e2ee-on");
                    app.join_voice(vc);
                    self.goto(5, "joining the encrypted room");
                } else if self.waited(20) {
                    return self.fail(&format!("the room did not turn encrypted: {:?}", app.roles.editing_e2ee.as_ref().map(|e| (&e.error, e.busy))));
                }
            }
            5 => {
                if app.voice.channel.is_some() {
                    if app.voice.e2ee == plain() {
                        return self.fail("joined, but the room is not end-to-end encrypted here");
                    }
                    self.goto(6, "in the encrypted room; waiting for B");
                } else if self.waited(60) || app.voice.error.is_some() {
                    return self.fail(&format!("could not join: {:?}", app.voice.error));
                }
            }
            6 => {
                if app.members_here().len() >= 2 && self.waited(2) {
                    app.toggle_camera();
                    self.goto(7, "camera on");
                } else if self.waited(120) {
                    return self.fail("B never joined");
                }
            }
            7 => match app.voice.camera.clone() {
                CameraUi::On { encoder, .. } => {
                    log(&format!("camera on, encoder {encoder}"));
                    if let Some(hwnd) = self.args.hwnd {
                        app.start_share(crate::app::Pick::Window(hwnd), crate::settings::Preset::P720p30, false);
                    } else if std::env::var("E2V_SOURCE").as_deref() == Ok("pattern") {
                        // The moving test pattern through the regular encoder (it follows the
                        // bandwidth estimate, unlike a pre-encoded file): the cross-machine run's
                        // live on the Mac, which has no test window to capture.
                        let opts = ShareOptions { fps: 30, audio: false, encoder: EncoderChoice::Auto, ..ShareOptions::default() };
                        app.voice.share = Share::Starting;
                        app.backend.send(Cmd::StartShare { source: ShareSource::TestPattern, opts });
                    } else {
                        let Some(path) = std::env::var_os("VOX_TEST_H264") else { return self.fail("VOX_TEST_H264 is not set") };
                        let source = ShareSource::H264File(H264FileSource { path: PathBuf::from(path), fps: 30, looped: true });
                        let opts = ShareOptions { fps: 30, audio: false, encoder: EncoderChoice::Auto, ..ShareOptions::default() };
                        app.voice.share = Share::Starting;
                        app.backend.send(Cmd::StartShare { source, opts });
                    }
                    self.goto(8, "going live");
                }
                CameraUi::Off => return self.fail(&format!("camera did not start: {:?}", app.notice)),
                CameraUi::Starting if self.waited(20) => return self.fail("camera still starting after 20 s"),
                CameraUi::Starting => {}
            },
            8 => {
                if let Share::Live { encoder } = &app.voice.share {
                    log(&format!("live (pre-encoded path: {encoder})"));
                    self.v_set("a-live");
                    self.goto(9, "live; holding until B is done");
                } else if app.voice.share == Share::Off && self.waited(3) {
                    return self.fail(&format!("Go Live did not start: {:?}", app.notice));
                } else if self.waited(30) {
                    return self.fail("Go Live start timed out");
                }
            }
            9 => {
                // What I send, every few seconds (the gate compares it with what B receives).
                if self.e2v.last_log.is_none_or(|t| t.elapsed() >= Duration::from_secs(5)) && self.fresh_stats(app) {
                    let now = Instant::now();
                    let live = Self::stat_count(app, "share-video").unwrap_or(0);
                    let cam = Self::stat_count(app, "camera").unwrap_or(0);
                    if let Some(at) = self.e2v.at {
                        let secs = now.duration_since(at).as_secs_f64();
                        log(&format!("A sends: live {:.1} fps, camera {:.1} fps", live.saturating_sub(self.e2v.live) as f64 / secs, cam.saturating_sub(self.e2v.cam) as f64 / secs));
                    }
                    (self.e2v.live, self.e2v.cam, self.e2v.at, self.e2v.last_log) = (live, cam, Some(now), Some(now));
                }
                if let Some(s) = app.voice.e2ee_states.values().find(|s| **s == MediaE2eeState::EncryptionFailed) {
                    return self.fail(&format!("my media did not encrypt: {s:?}"));
                }
                if !matches!(app.voice.share, Share::Live { .. }) {
                    return self.fail(&format!("the live ended by itself: {:?}", app.notice));
                }
                if self.v_flag("b-done") {
                    app.stop_share();
                    app.toggle_camera();
                    self.goto(10, "B is done; stopping");
                } else if self.waited(300) {
                    return self.fail("B never finished");
                }
            }
            10 if self.waited(2) => {
                app.leave_voice();
                self.goto(11, "leaving");
            }
            11 if self.waited(2) => return self.pass(),
            _ => {}
        }
        None
    }

    // ------------------------------------------------------------ B

    pub(super) fn run_e2v_b(&mut self, app: &mut App) -> Option<i32> {
        app.repaint_ctx().request_repaint_after(Duration::from_millis(100));
        if let Some(a) = self.e2v.a.clone() {
            if app.voice.speaking.contains(&a) {
                self.e2v.spoke = true;
            }
        }
        match self.step {
            0 => return self.v_connect(app),
            1 => {
                if self.v_ready(app) && self.v_flag("e2ee-on") {
                    if let Some(code) = self.v_pinned(app) {
                        return Some(code);
                    }
                    let vc = Self::first_voice(app)?;
                    if !plain() && !app.channels.iter().any(|c| c.id == vc && c.e2ee) {
                        return self.fail("the hub's channel list does not say the room is encrypted");
                    }
                    app.join_voice(vc);
                    self.goto(2, "joining the encrypted room");
                } else if self.waited(150) {
                    return self.fail(&format!("not ready: logged in {}, e2ee {:?}, flag {}", self.logged_in(app), app.privacy.view.status, self.v_flag("e2ee-on")));
                }
            }
            2 => {
                if app.voice.channel.is_some() {
                    if app.voice.e2ee == plain() {
                        return self.fail("joined, but the room is not end-to-end encrypted here");
                    }
                    self.goto(3, "in the encrypted room; waiting for A's live");
                } else if self.waited(60) || app.voice.error.is_some() {
                    return self.fail(&format!("could not join: {:?}", app.voice.error));
                }
            }
            3 => {
                let a = Self::v_member(app, "dono");
                let live = app.members_here().iter().any(|m| Some(m.user.id) == a && m.streaming);
                if let (Some(a), true, true) = (a, live, self.v_flag("a-live")) {
                    let a = a.to_string();
                    if !app.watch(&a) {
                        return self.fail("Assistir was refused");
                    }
                    self.e2v.a = Some(a);
                    // (So the header's badge notes where it is drawn, checked below.)
                    crate::layout::set_probing(true);
                    self.goto(4, "watching A");
                } else if self.waited(120) {
                    return self.fail("A's live never showed");
                }
            }
            4 => {
                let a = self.e2v.a.clone()?;
                let ready = app.is_watching(&a) && app.voice.cameras.contains(&a);
                if let (true, Some(limit)) = (ready, ramp_secs()) {
                    // The ramp check (E2V_RAMP_SECS): a stats sample every ~2 s until the live reaches
                    // RAMP_FPS, at most `limit` seconds.
                    let now = Instant::now();
                    let t0 = *self.e2v.ramp_t0.get_or_insert(now);
                    let due = self.e2v.ramp_mark.is_none_or(|(t, _)| now.duration_since(t) >= Duration::from_secs(2));
                    if self.e2v.ramp_at.is_none() && due && self.fresh_stats(app) {
                        let live = Self::stat_count(app, &format!("watch-video:{a}")).unwrap_or(0);
                        if let Some((t, n)) = self.e2v.ramp_mark {
                            let fps = live.saturating_sub(n) as f64 / now.duration_since(t).as_secs_f64();
                            self.e2v.ramp_best = self.e2v.ramp_best.max(fps);
                            if fps >= RAMP_FPS {
                                let at = now.duration_since(t0).as_secs_f64();
                                self.e2v.ramp_at = Some(at);
                                log(&format!("live ramp: {fps:.1} fps (>= {RAMP_FPS}) {at:.1} s after B started watching (limit {limit:.0} s)"));
                            }
                        }
                        self.e2v.ramp_mark = Some((now, live));
                    }
                    if self.e2v.ramp_at.is_none() {
                        if now.duration_since(t0).as_secs_f64() > limit {
                            log(&format!("live ramp: never reached {RAMP_FPS} fps within {limit:.0} s (best {:.1} fps)", self.e2v.ramp_best));
                            return self.fail(&format!("A's live (with A's camera on) never reached {RAMP_FPS} fps within {limit:.0} s (best {:.1} fps)", self.e2v.ramp_best));
                        }
                        return None;
                    }
                }
                // (12 s: the first seconds of a live are WebRTC's bandwidth ramp-up, when it drops
                // pre-encoded frames, and a file source cannot answer with a recovery keyframe the
                // way NVENC / Media Foundation do: its picture breaks until the file's next one.
                // Measured after that, as a viewer arriving later would see it.)
                if ready && self.waited(12) && self.fresh_stats(app) {
                    self.e2v.chunks = Self::ao_audio(app, &format!("voice:{a}")).map(|x| x.chunks).unwrap_or(0);
                    self.e2v.cam = Self::stat_count(app, &format!("cam-video:{a}")).unwrap_or(0);
                    self.e2v.live = Self::stat_count(app, &format!("watch-video:{a}")).unwrap_or(0);
                    self.e2v.at = Some(Instant::now());
                    self.goto(5, "first sample");
                } else if self.waited(60) {
                    return self.fail(&format!("A's camera and live never both arrived (watching {}, camera {})", app.is_watching(&a), app.voice.cameras.contains(&a)));
                }
            }
            5 if self.waited(8) => {
                if !self.fresh_stats(app) {
                    return None;
                }
                let a = self.e2v.a.clone()?;
                let secs = self.e2v.at.map(|t| t.elapsed().as_secs_f64()).unwrap_or(8.0);
                let Some(voice) = Self::ao_audio(app, &format!("voice:{a}")).cloned() else { return self.fail("A's voice is not subscribed") };
                let blocks = voice.chunks.saturating_sub(self.e2v.chunks) as f64 / secs;
                let cam = Self::stat_count(app, &format!("cam-video:{a}")).unwrap_or(0).saturating_sub(self.e2v.cam) as f64 / secs;
                let live = Self::stat_count(app, &format!("watch-video:{a}")).unwrap_or(0).saturating_sub(self.e2v.live) as f64 / secs;
                let state = app.voice.e2ee_states.get(&a).copied();
                log(&format!(
                    "B receives from A: voice {blocks:.0} blocks/s at {:?} dBFS, camera {cam:.1} fps, live {live:.1} fps; A's media state {state:?}; A seen speaking {}",
                    voice.rms_dbfs, self.e2v.spoke
                ));
                if blocks < 85.0 || !voice.rms_dbfs.is_some_and(|d| d > -40.0) {
                    return self.fail(&format!("A's voice does not decode here ({blocks:.0} blocks/s, {:?} dBFS)", voice.rms_dbfs));
                }
                if cam < 20.0 {
                    return self.fail(&format!("A's camera does not arrive ({cam:.1} fps)"));
                }
                if live < 24.0 {
                    return self.fail(&format!("A's live does not arrive at its 30 fps ({live:.1} fps)"));
                }
                if !state_ok(state) {
                    return self.fail(&format!("A's media state is {state:?}, not Ok"));
                }
                if !self.e2v.spoke {
                    return self.fail("A never showed as speaking");
                }
                super::shots::save(app, "e2v-sala");
                let ctx = app.repaint_ctx();
                let badge = crate::layout::probed(&ctx, "call_e2ee_badge").filter(|(frame, _)| ctx.cumulative_frame_nr().saturating_sub(*frame) < 5);
                if !plain() && (!app.voice.e2ee || badge.is_none()) {
                    return self.fail("the call header does not show \"Criptografada de ponta a ponta\"");
                }
                // C comes now: from here on the longest stretch without A's tone counts.
                self.e2v.max_gap = 0;
                self.e2v.reports = 0;
                self.v_set("c-go");
                self.goto(6, "checks passed; C may come (keys rotate)");
            }
            6 => {
                let a = self.e2v.a.clone()?;
                if self.fresh_stats(app) {
                    if let Some(v) = Self::ao_audio(app, &format!("voice:{a}")) {
                        self.e2v.max_gap = self.e2v.max_gap.max(v.max_quiet_ms);
                        self.e2v.reports += 1;
                    }
                }
                if let Some(s) = app.voice.e2ee_states.get(&a).filter(|s| **s != MediaE2eeState::Ok) {
                    log(&format!("A's media state now {s:?}"));
                }
                if self.v_flag("c-left") && self.waited(6) {
                    self.goto(7, "C came and left");
                } else if self.waited(150) {
                    return self.fail("C never came and left");
                }
            }
            7 => {
                if !self.fresh_stats(app) {
                    return None;
                }
                let a = self.e2v.a.clone()?;
                if let Some(v) = Self::ao_audio(app, &format!("voice:{a}")) {
                    self.e2v.max_gap = self.e2v.max_gap.max(v.max_quiet_ms);
                }
                let state = app.voice.e2ee_states.get(&a).copied();
                log(&format!("while C came and left: longest stretch without A's tone {} ms ({} reports); A's media state now {state:?}", self.e2v.max_gap, self.e2v.reports));
                if self.e2v.max_gap > MAX_GAP_MS {
                    return self.fail(&format!("A's voice stopped for {} ms while the keys rotated", self.e2v.max_gap));
                }
                if !state_ok(state) {
                    return self.fail(&format!("A's media state is {state:?} after the rotation"));
                }
                if !app.is_watching(&a) {
                    return self.fail("A's live stopped while the keys rotated");
                }
                self.v_set("b-done");
                self.goto(8, "done; telling A");
            }
            8 if self.waited(3) => {
                app.leave_voice();
                self.goto(9, "leaving");
            }
            9 if self.waited(2) => return self.pass(),
            _ => {}
        }
        None
    }

    // ------------------------------------------------------------ C

    pub(super) fn run_e2v_c(&mut self, app: &mut App) -> Option<i32> {
        app.repaint_ctx().request_repaint_after(Duration::from_millis(100));
        match self.step {
            0 => return self.v_connect(app),
            1 => {
                if self.v_ready(app) && self.v_flag("c-go") {
                    if let Some(code) = self.v_pinned(app) {
                        return Some(code);
                    }
                    let vc = Self::first_voice(app)?;
                    app.join_voice(vc);
                    self.e2v.joined_at = Some(Instant::now());
                    self.goto(2, "joining the encrypted room (a newcomer)");
                } else if self.waited(240) {
                    return self.fail("never told to come");
                }
            }
            2 => {
                if app.voice.channel.is_some() {
                    if app.voice.e2ee == plain() {
                        return self.fail("joined, but the room is not end-to-end encrypted here");
                    }
                    self.e2v.a = Self::v_member(app, "dono").map(|u| u.to_string());
                    self.goto(3, "in; waiting to hear A");
                } else if self.waited(60) || app.voice.error.is_some() {
                    return self.fail(&format!("could not join: {:?}", app.voice.error));
                }
            }
            3 => {
                let Some(a) = self.e2v.a.clone() else { return self.fail("A is not in the room") };
                if self.fresh_stats(app) {
                    let voice = Self::ao_audio(app, &format!("voice:{a}")).cloned();
                    let state = app.voice.e2ee_states.get(&a).copied();
                    if voice.as_ref().is_some_and(|v| v.rms_dbfs.is_some_and(|d| d > -40.0)) && state_ok(state) {
                        let ms = self.e2v.joined_at.map(|t| t.elapsed().as_millis()).unwrap_or(0);
                        log(&format!("C hears A {ms} ms after pressing join ({:?} dBFS)", voice.and_then(|v| v.rms_dbfs)));
                        self.goto(4, "hearing A; staying a little");
                        return None;
                    }
                }
                if self.waited(15) {
                    return self.fail(&format!("A's voice never decoded here (state {:?})", app.voice.e2ee_states.get(&a)));
                }
            }
            4 if self.waited(5) => {
                app.leave_voice();
                self.goto(5, "leaving");
            }
            5 => {
                if app.voice.channel.is_none() && self.waited(1) {
                    self.v_set("c-left");
                    return self.pass();
                } else if self.waited(15) {
                    return self.fail("could not leave");
                }
            }
            _ => {}
        }
        None
    }

    // ------------------------------------------------------------ the owner of `e2v-old`

    pub(super) fn run_e2v_admin(&mut self, app: &mut App) -> Option<i32> {
        app.repaint_ctx().request_repaint_after(Duration::from_millis(200));
        match self.v_owner_ready(app) {
            Some(Err(code)) => return Some(code),
            None => return None,
            Some(Ok(())) => {}
        }
        let vc = Self::first_voice(app)?;
        match self.step {
            3 => {
                if !self.v_ready(app) {
                    return if self.waited(45) { self.fail(&format!("e2ee not active here: {:?}", app.privacy.view.status)) } else { None };
                }
                app.roles.editing_e2ee = Some(E2eeEdit { id: vc, busy: true, ..Default::default() });
                app.admin(AdminCmd::SetChannelE2ee { id: vc, on: true });
                self.goto(4, "turning the room's end-to-end encryption on");
            }
            4 => {
                if self.v_turn_on(app, vc) {
                    let outdated: Vec<String> = app.roles.editing_e2ee.as_ref().and_then(|e| e.info.as_ref()).map(|i| i.outdated.iter().map(|u| u.username.clone()).collect()).unwrap_or_default();
                    log(&format!("the room is encrypted; left out: {outdated:?}"));
                    self.v_set("e2ee-on");
                    self.goto(5, "watching the room while the old app tries");
                } else if self.waited(20) {
                    return self.fail("the room did not turn encrypted");
                }
            }
            5 => {
                if app.voice_members.values().any(|m| m.channel_id == Some(vc)) {
                    return self.fail("somebody got into the encrypted room");
                }
                if self.waited(60) {
                    return self.pass();
                }
            }
            _ => {}
        }
        None
    }
}
