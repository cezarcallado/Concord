//! v1.1 I6 "Interface mais responsiva" (a child module of `autotest`): every screen at several
//! window sizes and zoom levels, one process, the window resized and zoomed at run time.
//!
//! - `resp-a` (the owner, "dono"): the home screen first (before logging in), then a text channel
//!   with the member list, a DM, an empty voice room, the call stage (grid, only the cards with
//!   video, one and three pinned, the chat docked, the chat with the call on, the stage
//!   fullscreen), a card's and a person's menus, the Go Live picker, the soundboard, every
//!   settings page, "Configurar câmera", a profile card, the server window's pages, a
//!   confirmation, and the TLS dialogs ("Segurança da conexão", "A identidade do servidor mudou"
//!   and its second step). `--mode host`: the host panel and "Parar servidor" instead (a server
//!   hosted in the app, the gate's host-mode environment).
//! - `resp-b`: a partner. Joins the first voice room and, with `--hwnd`, streams that window
//!   (720p30) or, with `--mode cam`, turns its camera on; stays until the owner's "fim" cue.
//!
//! The matrix: every scene at every combination of window size (logical pixels) and zoom (egui's
//! Ctrl+= zoom: the window keeps its size, everything is drawn bigger, so fewer points fit). The
//! default list is 760×440, 1024×640, 1280×720 and 1920×1080 at 1.0 / 1.25 / 1.5, plus 2560×1440
//! at 2.0. `VOX_RESP_COMBOS=760x440@1.25,1280x720@1` and `VOX_RESP_SCENES=canal,dm` narrow it.
//! With `VOX_AUTOTEST_SHOT_DIR` each one is saved as `<scene>__<w>x<h>-z<zoom>.png`.
//!
//! The responsive rules are checked at every stop (see [`crate::layout`]): every window, menu and
//! popup inside the app's window; a header's title never under its buttons; the member list off
//! when the chat would get narrower than its minimum; the docked chat and the stage both inside
//! the window with room for the cards; the chat in the center never narrower than its minimum
//! while the window allows it. `--mode gate` runs the few combinations and scenes the release
//! gate needs and fails on any break; the full matrix only logs them (`AUTOTEST resp DEFECT`).
//!
//! Background test mode; silent (`VOX_NO_PLAYBACK=1`, `VOX_NO_MIC=1`).

use std::time::{Duration, Instant};

use eframe::egui;
use vox_proto::{ChannelKind, PrivateAccess, User, UserId};

use super::{shots, Auto};
use crate::app::{App, Pick, Screen, SettingsPage};
use crate::call::TileKey;
use crate::context_menu::{Lead, Origin};
use crate::layout::{self, CenterMode};
use crate::roles::{AccessDraft, AdminCmd, ServerTab};

/// Test values for this throwaway hub.
const OWNER_PW: &str = "resp-dono-123";
const SALA2: &str = "Sala 2";
const JOGATINA: &str = "jogatina-de-sexta";
const ADM: &str = "administração";
const FIM: &str = "fim-resp";
/// The partners: three streamers and a camera (the full matrix).
const STREAMERS: [&str; 3] = ["lua", "sol", "mar"];
const CAMERA: &str = "rio";
const TEST_WINDOW_TITLE: &str = "vox test window";

/// How many of [`STREAMERS`] the run waits for (`VOX_RESP_STREAMERS`, 0 to 3; 3 by default):
/// `tools/shots.ps1 -Resp` starts that many.
fn streamers() -> usize {
    std::env::var("VOX_RESP_STREAMERS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(STREAMERS.len()).min(STREAMERS.len())
}

/// A window size (logical pixels, what Windows calls the window's size) and egui's zoom.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Combo {
    pub w: f32,
    pub h: f32,
    pub zoom: f32,
}

impl Combo {
    /// The window in points (what the layout gets).
    pub fn points(self) -> egui::Vec2 {
        egui::vec2(self.w / self.zoom, self.h / self.zoom)
    }

    fn tag(self) -> String {
        format!("{}x{}-z{:03}", self.w.round() as i32, self.h.round() as i32, (self.zoom * 100.0).round() as i32)
    }
}

const FULL: [(f32, f32, f32); 13] = [
    (760.0, 440.0, 1.0),
    (760.0, 440.0, 1.25),
    (760.0, 440.0, 1.5),
    (1024.0, 640.0, 1.0),
    (1024.0, 640.0, 1.25),
    (1024.0, 640.0, 1.5),
    (1280.0, 720.0, 1.0),
    (1280.0, 720.0, 1.25),
    (1280.0, 720.0, 1.5),
    (1920.0, 1080.0, 1.0),
    (1920.0, 1080.0, 1.25),
    (1920.0, 1080.0, 1.5),
    (2560.0, 1440.0, 2.0),
];
/// The gate's: the smallest window zoomed in (the hardest), the smallest at 1.0, a typical one.
const GATE: [(f32, f32, f32); 3] = [(760.0, 440.0, 1.25), (760.0, 440.0, 1.0), (1280.0, 720.0, 1.0)];

/// Before logging in.
const PRE: [&str; 1] = ["inicio"];
/// Logged in, in this order (the call scenes need the room).
const POST: [&str; 38] = [
    "canal",
    "canal-membros",
    "dm",
    "sala-vazia",
    "sala-grade",
    "sala-videos",
    "sala-fixado",
    "sala-fixados",
    "sala-dock",
    "sala-chat",
    "sala-tela-cheia",
    "menu-card",
    "menu-pessoa",
    "perfil",
    "golive",
    "soundboard",
    "config-conta",
    "config-voz",
    "config-atalhos",
    "config-video",
    "config-notificacoes",
    "config-chat",
    "config-sons",
    "config-privacidade",
    "config-sobre",
    "camera",
    "admin-membros",
    "admin-canais",
    "admin-acesso",
    "admin-convites",
    "admin-registro",
    "admin-config",
    "dialogo-apagar",
    "tls-seguranca",
    "tls-mudou",
    "tls-confirmar",
    "sala-fixado-dock",
    "fim",
];
const GATE_SCENES: [&str; 11] = ["canal", "canal-membros", "dm", "sala-dock", "menu-pessoa", "golive", "config-atalhos", "admin-membros", "dialogo-apagar", "tls-mudou", "fim"];
const HOST_SCENES: [&str; 2] = ["host-painel", "host-parar"];

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Phase {
    #[default]
    Setup,
    Zoom,
    Resize,
    Settle,
    Shot,
}

#[derive(Default)]
pub struct State {
    at: Option<Instant>,
    combos: Vec<Combo>,
    scenes: Vec<&'static str>,
    scene: usize,
    combo: usize,
    phase: Phase,
    resent: u8,
    /// The extra wait of this stop before its shot.
    wait: f32,
    /// The scene's setup already did its actions (it is called every tick until ready).
    started: bool,
    /// The zooms a once-per-zoom scene was already taken at.
    zooms_done: Vec<u32>,
    defects: Vec<String>,
    checks: u32,
    created: bool,
    geral: i64,
    sala: i64,
    sala2: i64,
    adm: i64,
    jogatina: i64,
    peers: Vec<User>,
    streams: Vec<UserId>,
    cam: Option<UserId>,
    uploads: usize,
    dir: Option<std::path::PathBuf>,
    sent: bool,
}

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST resp: {what}");
}

fn channel_id(app: &App, name: &str, kind: ChannelKind) -> Option<i64> {
    app.channels.iter().find(|c| c.name == name && c.kind == kind).map(|c| c.id)
}

fn env_combos(gate: bool) -> Vec<Combo> {
    let parse = |s: &str| -> Option<Combo> {
        let (size, zoom) = s.split_once('@').unwrap_or((s, "1"));
        let (w, h) = size.split_once('x')?;
        Some(Combo { w: w.trim().parse().ok()?, h: h.trim().parse().ok()?, zoom: zoom.trim().parse().ok()? })
    };
    if let Ok(list) = std::env::var("VOX_RESP_COMBOS") {
        let v: Vec<Combo> = list.split(',').filter_map(parse).collect();
        if !v.is_empty() {
            return v;
        }
    }
    let base: &[(f32, f32, f32)] = if gate { &GATE } else { &FULL };
    base.iter().map(|&(w, h, zoom)| Combo { w, h, zoom }).collect()
}

fn env_scenes(all: &[&'static str]) -> Vec<&'static str> {
    match std::env::var("VOX_RESP_SCENES") {
        Ok(list) if !list.trim().is_empty() => {
            let want: Vec<&str> = list.split(',').map(str::trim).collect();
            // "fim" always closes the run.
            all.iter().copied().filter(|s| want.contains(s) || *s == "fim").collect()
        }
        _ => all.to_vec(),
    }
}

/// Scenes taken once per zoom (the window's size does not matter: the stage fullscreen fills the
/// monitor).
fn once_per_zoom(scene: &str) -> bool {
    scene == "sala-tela-cheia"
}

/// How long a stop waits for the layout (and the video windows) to settle.
fn settle_secs(scene: &str) -> f32 {
    if scene.starts_with("sala") || scene.starts_with("menu-card") || scene == "golive" || scene == "camera" {
        1.8
    } else {
        0.9
    }
}

impl Auto {
    fn rs_waited(&self, secs: f32) -> bool {
        self.resp.at.is_some_and(|t| t.elapsed() >= Duration::from_secs_f32(secs))
    }

    fn rs_mark(&mut self) {
        self.resp.at = Some(Instant::now());
    }

    fn rs_gate(&self) -> bool {
        self.args.mode.as_deref() == Some("gate")
    }

    fn rs_defect(&mut self, what: String) {
        tracing::warn!(target: "autotest", "AUTOTEST resp DEFECT {what}");
        self.resp.defects.push(what);
    }

    fn rs_peer(&self, name: &str) -> Option<User> {
        self.resp.peers.iter().find(|u| u.username == name).cloned()
    }

    // ------------------------------------------------------------ the matrix

    /// One tick of the matrix over `self.resp.scenes`: `Ok(true)` once every scene was taken.
    fn rs_matrix(&mut self, app: &mut App) -> Result<bool, String> {
        let ctx = app.repaint_ctx();
        let Some(&scene) = self.resp.scenes.get(self.resp.scene) else { return Ok(true) };
        match self.resp.phase {
            Phase::Setup => {
                if self.resp.at.is_none() {
                    self.rs_mark();
                    self.resp.started = false;
                }
                let first = !self.resp.started;
                self.resp.started = true;
                if self.rs_setup(app, scene, first)? {
                    self.resp.combo = 0;
                    self.resp.phase = Phase::Zoom;
                } else if self.rs_waited(90.0) {
                    return Err(format!("{scene}: never got ready"));
                }
            }
            Phase::Zoom => {
                // The next combination this scene is taken at.
                loop {
                    let Some(c) = self.resp.combos.get(self.resp.combo).copied() else {
                        self.rs_teardown(app, scene);
                        self.resp.scene += 1;
                        self.resp.phase = Phase::Setup;
                        self.resp.at = None;
                        return Ok(false);
                    };
                    let z = (c.zoom * 100.0).round() as u32;
                    if once_per_zoom(scene) && self.resp.zooms_done.contains(&z) {
                        self.resp.combo += 1;
                        continue;
                    }
                    break;
                }
                let c = self.resp.combos[self.resp.combo];
                ctx.set_zoom_factor(c.zoom);
                self.resp.phase = Phase::Resize;
                self.rs_mark();
            }
            Phase::Resize => {
                let c = self.resp.combos[self.resp.combo];
                if !app.stage_fullscreen {
                    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(c.points()));
                }
                self.resp.resent = 0;
                self.resp.phase = Phase::Settle;
                self.rs_mark();
            }
            Phase::Settle => {
                let c = self.resp.combos[self.resp.combo];
                let got = ctx.content_rect().size();
                let zoom_ok = (ctx.zoom_factor() - c.zoom).abs() < 0.01;
                let size_ok = app.stage_fullscreen || (got - c.points()).abs().max_elem() < 2.0;
                if zoom_ok && size_ok && self.rs_waited(settle_secs(scene)) || self.rs_waited(8.0) {
                    if !(zoom_ok && size_ok) {
                        log(&format!("{scene} {}: the window is {got:?} at zoom {:.2}, not {:?} at {:.2}", c.tag(), ctx.zoom_factor(), c.points(), c.zoom));
                    }
                    self.resp.wait = self.rs_per_combo(app, scene, c)?;
                    self.resp.phase = Phase::Shot;
                    self.rs_mark();
                } else if !size_ok && self.rs_waited(1.5 + self.resp.resent as f32 * 1.5) && self.resp.resent < 3 {
                    self.resp.resent += 1;
                    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(c.points()));
                }
            }
            Phase::Shot => {
                if !self.rs_waited(self.resp.wait) {
                    return Ok(false);
                }
                let c = self.resp.combos[self.resp.combo];
                self.rs_check(app, scene, c);
                if scene != "fim" {
                    shots::save(app, &format!("{scene}__{}", c.tag()));
                }
                self.rs_after(app, scene);
                if once_per_zoom(scene) {
                    self.resp.zooms_done.push((c.zoom * 100.0).round() as u32);
                }
                self.resp.combo += 1;
                self.resp.phase = Phase::Zoom;
                self.rs_mark();
            }
        }
        Ok(false)
    }

    /// The rules, at this stop.
    fn rs_check(&mut self, app: &App, scene: &str, c: Combo) {
        let ctx = app.repaint_ctx();
        let screen = ctx.content_rect();
        let at = format!("{scene} {}", c.tag());
        self.resp.checks += 1;
        // 1. Every window, menu and popup inside the app's window.
        let layers = ctx.memory(|m| m.areas().visible_layer_ids());
        for l in layers {
            if !matches!(l.order, egui::Order::Middle | egui::Order::Foreground) {
                continue;
            }
            let Some(r) = ctx.memory(|m| m.area_rect(l.id)) else { continue };
            if r.width() < 2.0 || r.height() < 2.0 {
                continue;
            }
            if !screen.expand(1.5).contains_rect(r) {
                self.rs_defect(format!("{at}: a window/popup {:?} at {r:?} goes past the app's window {screen:?}", l.order));
            }
        }
        // 2.-5. The layout's own probes (`layout::probe`), when drawn this frame.
        let frame = ctx.cumulative_frame_nr();
        let fresh = |key: &str| layout::probed(&ctx, key).filter(|(f, _)| frame.saturating_sub(*f) <= 3).map(|(_, r)| r);
        for (title, tools) in [("center_title", "center_tools"), ("dock_title", "dock_tools"), ("stage_title", "stage_tools")] {
            if let (Some(t), Some(b)) = (fresh(title), fresh(tools)) {
                if t.right() > b.left() + 0.5 && t.y_range().intersects(b.y_range()) {
                    self.rs_defect(format!("{at}: the header's title {t:?} runs under its buttons {b:?}"));
                }
            }
        }
        if let Some(chat) = fresh("center_chat") {
            let members = fresh("members");
            if chat.width() + 0.5 < layout::CHAT_MIN_W && members.is_some() {
                self.rs_defect(format!("{at}: the chat is {:.0} wide (min {}) with the member list docked", chat.width(), layout::CHAT_MIN_W));
            }
            if !screen.expand(1.0).contains_rect(chat) {
                self.rs_defect(format!("{at}: the chat {chat:?} goes past the window {screen:?}"));
            }
        }
        if let Some(dock) = fresh("dock") {
            if !screen.expand(1.0).contains_rect(dock) {
                self.rs_defect(format!("{at}: the docked chat {dock:?} goes past the window {screen:?}"));
            }
            if let Some(stage) = fresh("stage") {
                if stage.width() + 0.5 < layout::STAGE_MIN_W {
                    self.rs_defect(format!("{at}: the stage is {:.0} wide beside the docked chat (min {})", stage.width(), layout::STAGE_MIN_W));
                }
            }
        }
        // The gate's named rules.
        if scene == "canal" && c.points().x < layout::MEMBERS_AUTO_W && fresh("members").is_some() {
            self.rs_defect(format!("{at}: the member list is docked in a {:.0}-point window (it collapses below {})", c.points().x, layout::MEMBERS_AUTO_W));
        }
        if scene == "sala-dock" && c.points().x >= 759.0 && fresh("dock").is_none() {
            self.rs_defect(format!("{at}: no docked chat in a {:.0}-point window (it fits from the smallest window up)", c.points().x));
        }
        if scene == "sala-dock" && c.points().x < 700.0 && fresh("dock").is_some() {
            self.rs_defect(format!("{at}: the chat is docked in a {:.0}-point window (no room beside the stage)", c.points().x));
        }
        if scene == "canal-membros" && fresh("members").is_none() && fresh("members_peek").is_none() {
            self.rs_defect(format!("{at}: the member list did not come back with its button"));
        }
    }

    /// Get `scene` ready (once, before its combinations): `Ok(true)` when it is.
    fn rs_setup(&mut self, app: &mut App, scene: &str, first: bool) -> Result<bool, String> {
        let ctx = app.repaint_ctx();
        let lua = self.rs_peer(STREAMERS[0]).or_else(|| self.resp.peers.first().cloned());
        Ok(match scene {
            "inicio" => {
                app.screen == Screen::Connect
            }
            "canal" | "canal-membros" => {
                app.select_text(self.resp.geral);
                app.set_center_mode(CenterMode::Chat);
                app.settings.layout.show_members = true;
                layout::set_members_peek(&ctx, (scene == "canal-membros").then_some(self.resp.geral));
                true
            }
            "dm" => {
                let Some(p) = lua else { return Err("no partner for the DM".into()) };
                if app.dms.selected.is_none() {
                    app.open_dm(p.id);
                }
                if !self.resp.sent && app.dms.selected.is_some() {
                    self.resp.sent = true;
                    app.dms.input = "oi! você vem hoje? trouxe o placar de ontem e a lista da jogatina de sexta".into();
                    app.send_dm();
                }
                app.dms.selected.is_some() && self.rs_waited(2.5)
            }
            "sala-vazia" => {
                if app.voice.channel != Some(self.resp.sala2) && app.voice.joining.is_none() {
                    app.join_voice(self.resp.sala2);
                }
                if app.voice.channel == Some(self.resp.sala2) {
                    app.show_call();
                    app.set_center_mode(CenterMode::Cards);
                    self.rs_waited(2.0)
                } else {
                    false
                }
            }
            "sala-grade" => {
                if app.voice.channel != Some(self.resp.sala) && app.voice.joining.is_none() {
                    app.join_voice(self.resp.sala);
                }
                if app.voice.channel != Some(self.resp.sala) {
                    return Ok(false);
                }
                app.show_call();
                app.set_center_mode(CenterMode::Cards);
                let me = app.my_id();
                let mut live: Vec<UserId> = app.members_here().iter().filter(|m| m.streaming && Some(m.user.id) != me).map(|m| m.user.id).collect();
                live.sort();
                let cam = app.voice.cameras.iter().filter_map(|i| i.parse::<UserId>().ok()).find(|u| !live.contains(u));
                let want = if self.rs_gate() { 0 } else { streamers() };
                if (live.len() >= want && (want == 0 || cam.is_some())) || self.rs_waited(60.0) {
                    if live.len() < want {
                        log(&format!("only {} lives and camera {cam:?} came; going on", live.len()));
                    }
                    self.resp.streams = live.clone();
                    self.resp.cam = cam;
                    for u in &live {
                        app.watch(&u.to_string());
                    }
                    app.show_hidden_cards();
                    app.pin_only(None);
                    let watching = live.iter().all(|u| app.is_watching(&u.to_string()));
                    watching && self.rs_waited(4.0)
                } else {
                    false
                }
            }
            "sala-videos" => {
                app.pin_only(None);
                app.hide_cards_without_video();
                true
            }
            "sala-fixado" => {
                app.show_hidden_cards();
                app.pin_only(self.resp.streams.first().map(|u| TileKey::Stream(*u)));
                true
            }
            "sala-fixados" => {
                app.pin_only(None);
                for u in self.resp.streams.iter().take(2) {
                    app.pin(TileKey::Stream(*u));
                }
                if let Some(c) = self.resp.cam {
                    app.pin(TileKey::Member(c));
                }
                true
            }
            "sala-dock" | "sala-fixado-dock" => {
                if app.voice.channel.is_none() {
                    if let Some(vc) = Some(self.resp.sala).filter(|v| *v != 0) {
                        if app.voice.joining.is_none() {
                            app.join_voice(vc);
                        }
                    }
                    return Ok(false);
                }
                app.pin_only(if scene == "sala-dock" { None } else { self.resp.streams.first().map(|u| TileKey::Stream(*u)) });
                app.show_call();
                app.dock_channel(self.resp.geral);
                app.set_center_mode(CenterMode::CardsWithChat);
                true
            }
            "sala-chat" => {
                app.pin_only(None);
                app.select_text(self.resp.geral);
                app.set_center_mode(CenterMode::Chat);
                true
            }
            "sala-tela-cheia" => {
                app.show_call();
                app.set_center_mode(CenterMode::Cards);
                app.pin_only(self.resp.streams.first().map(|u| TileKey::Stream(*u)));
                true
            }
            "menu-card" | "menu-pessoa" | "perfil" => {
                app.show_call();
                app.set_center_mode(if scene == "menu-card" { CenterMode::Cards } else { CenterMode::Chat });
                app.pin_only(None);
                true
            }
            "golive" => {
                if !app.golive.open {
                    app.open_golive();
                    return Ok(false);
                }
                if app.golive.loading {
                    return Ok(false);
                }
                if app.golive.pick.is_none() {
                    // Only the test windows: never a picture of the person's own windows.
                    app.golive.windows.retain(|w| w.title == TEST_WINDOW_TITLE);
                    app.golive.pick = Some(app.golive.windows.first().map(|w| Pick::Window(w.hwnd)).unwrap_or(Pick::AllSound));
                    let repaint = app.repaint.clone();
                    app.golive.picker.release();
                    app.golive.picker.sources_listed(&app.golive.windows, &[], std::sync::Arc::new(move || repaint.wake()), false);
                    self.rs_mark();
                    return Ok(false);
                }
                self.rs_waited(4.0)
            }
            "soundboard" => {
                app.sb.open = true;
                true
            }
            s if s.starts_with("config-") => {
                if !first {
                    return Ok(self.rs_waited(1.0));
                }
                let slug = &s["config-".len()..];
                let Some(page) = SettingsPage::ALL.iter().copied().find(|p| p.slug() == slug) else { return Err(format!("no settings page {slug}")) };
                app.open_settings_at(page);
                false
            }
            "camera" => {
                if first {
                    app.open_camera_window();
                }
                app.settings_ui.cam.live.is_some() && self.rs_waited(3.0) || self.rs_waited(20.0)
            }
            "admin-membros" | "admin-canais" | "admin-acesso" | "admin-convites" | "admin-registro" | "admin-config" | "dialogo-apagar" => {
                if !first {
                    // (The invites page shows a fresh code: the longest line it has.)
                    let code = scene != "admin-convites" || app.host.inv.last.is_some() || self.rs_waited(10.0);
                    return Ok(self.rs_waited(1.5) && code);
                }
                if scene == "admin-convites" && app.host.inv.last.is_none() {
                    app.create_invite();
                }
                app.select_text(self.resp.geral);
                app.set_center_mode(CenterMode::Chat);
                app.open_server_settings();
                app.roles.editing_access = None;
                app.roles.confirm_delete = None;
                app.roles.tab = match scene {
                    "admin-membros" => ServerTab::Members,
                    "admin-convites" => ServerTab::Invites,
                    "admin-registro" => ServerTab::Audit,
                    "admin-config" => ServerTab::Settings,
                    _ => ServerTab::Channels,
                };
                if scene == "admin-acesso" {
                    let draft = AccessDraft::of(app.channels.iter().find(|c| c.id == self.resp.adm).and_then(|c| c.private.as_ref()));
                    app.roles.editing_access = Some((self.resp.adm, draft));
                }
                if scene == "dialogo-apagar" {
                    app.roles.confirm_delete = Some(self.resp.jogatina);
                }
                if scene == "admin-registro" {
                    app.admin(AdminCmd::Audit { before: None });
                }
                false
            }
            "tls-seguranca" => {
                app.tls.security_open = true;
                true
            }
            "tls-mudou" | "tls-confirmar" => {
                app.tls.mismatch = Some(crate::app::tls_ui::Mismatch {
                    url: "https://192.0.2.10:7996".into(),
                    user: "dono".into(),
                    expected: [0x5a; 32],
                    got: [0xc3; 32],
                    confirming: scene == "tls-confirmar",
                });
                true
            }
            "host-painel" => {
                app.host.panel_open = true;
                app.host.inv.last.is_some() || self.rs_waited(15.0)
            }
            "host-parar" => {
                app.host.panel_open = true;
                app.host.confirm_stop = true;
                true
            }
            "fim" => true,
            other => return Err(format!("unknown scene {other}")),
        })
    }

    /// What a stop needs at its own size (menus open where the layout puts their target):
    /// the extra seconds to wait before the shot.
    fn rs_per_combo(&mut self, app: &mut App, scene: &str, _c: Combo) -> Result<f32, String> {
        let ctx = app.repaint_ctx();
        let screen = ctx.content_rect();
        Ok(match scene {
            "sala-tela-cheia" => {
                app.set_stage_fullscreen(&ctx, true);
                3.0
            }
            // (At this size: a window with room for the list drops the peek, as it should.)
            "canal-membros" => {
                layout::set_members_peek(&ctx, Some(self.resp.geral));
                0.6
            }
            "menu-card" => {
                let Some(u) = self.resp.streams.first().copied().or_else(|| self.resp.peers.first().map(|p| p.id)) else { return Err("no card for the menu".into()) };
                let key = if self.resp.streams.is_empty() { TileKey::Member(u) } else { TileKey::Stream(u) };
                let Some(card) = app.call.drawn.iter().find(|d| d.key == key).map(|d| d.rect) else {
                    log("menu-card: the card is not drawn");
                    return Ok(0.5);
                };
                app.open_card_menu(&ctx, key, egui::pos2(card.center().x, card.top() + 14.0));
                1.0
            }
            "menu-pessoa" => {
                let Some(p) = self.rs_peer(STREAMERS[0]).or_else(|| self.resp.peers.first().cloned()) else { return Err("no partner".into()) };
                app.open_person_menu(&ctx, &p, Origin::Sidebar, Lead::None, egui::pos2(150.0_f32.min(screen.width() * 0.3), screen.height() * 0.55));
                1.0
            }
            "perfil" => {
                let Some(p) = self.rs_peer(STREAMERS[0]).or_else(|| self.resp.peers.first().cloned()) else { return Err("no partner".into()) };
                app.open_profile_at(&p, egui::pos2(screen.width() * 0.4, 90.0));
                1.5
            }
            _ => 0.0,
        })
    }

    /// After a stop's shot.
    fn rs_after(&mut self, app: &mut App, scene: &str) {
        let ctx = app.repaint_ctx();
        match scene {
            "sala-tela-cheia" => app.set_stage_fullscreen(&ctx, false),
            "menu-card" | "menu-pessoa" => {
                app.cmenu.open = None;
                egui::Popup::close_all(&ctx);
            }
            "perfil" => app.close_profile(),
            _ => {}
        }
    }

    /// Leave `scene` as it was found.
    fn rs_teardown(&mut self, app: &mut App, scene: &str) {
        let ctx = app.repaint_ctx();
        match scene {
            "canal-membros" => layout::set_members_peek(&ctx, None),
            "sala-videos" => app.show_hidden_cards(),
            "golive" => {
                app.golive.open = false;
                app.golive.pick = None;
            }
            "soundboard" => app.sb.open = false,
            s if s.starts_with("config-") => app.settings_ui.open = false,
            "camera" => app.close_camera_window(),
            s if s.starts_with("admin-") || s == "dialogo-apagar" => {
                app.roles.editing_access = None;
                app.roles.confirm_delete = None;
                app.roles.server_open = false;
            }
            "tls-seguranca" => app.tls.security_open = false,
            "tls-mudou" | "tls-confirmar" => app.tls.mismatch = None,
            "host-parar" => app.host.confirm_stop = false,
            _ => {}
        }
    }

    // ------------------------------------------------------------ the owner

    pub(super) fn run_resp_a(&mut self, app: &mut App) -> Option<i32> {
        if self.args.mode.as_deref() == Some("host") {
            return self.run_resp_host(app);
        }
        let gate = self.rs_gate();
        match self.step {
            // The home screen with a few saved servers, before logging in.
            0 => {
                layout::set_probing(true);
                self.resp.combos = env_combos(gate);
                log(&format!("{} combinations: {:?}", self.resp.combos.len(), self.resp.combos.iter().map(|c| c.tag()).collect::<Vec<_>>()));
                if !gate {
                    for (i, (name, nick, url)) in [("Turma da Lua", "", "http://192.0.2.10:7996"), ("Clã do Sábado", "sábado", "http://192.0.2.11:7996"), ("Estúdio", "", "http://192.0.2.12:7996")].into_iter().enumerate() {
                        app.settings.servers.servers.push(crate::servers::SavedServer {
                            id: 900 + i as u64,
                            url: url.into(),
                            username: "dono".into(),
                            server_name: name.into(),
                            nickname: nick.into(),
                            ..Default::default()
                        });
                    }
                }
                self.resp.scenes = if gate { Vec::new() } else { env_scenes(&PRE) };
                self.resp.scene = 0;
                self.resp.phase = Phase::Setup;
                self.goto(1, "the home screen");
            }
            1 => match self.rs_matrix(app) {
                Ok(true) => {
                    app.settings.servers.servers.retain(|s| s.id < 900);
                    app.form.personal = OWNER_PW.into();
                    self.connect(app);
                    self.goto(2, "connecting");
                }
                Ok(false) => {}
                Err(e) => return self.fail(&e),
            },
            2 => {
                if !self.logged_in(app) {
                    return if self.waited(30) { self.fail(&format!("could not log in: {:?}", app.form.error)) } else { None };
                }
                if app.roles.account.is_some_and(|a| a.must_set_password) {
                    if !self.resp.created {
                        self.resp.created = true;
                        app.set_personal_password(None, Some(OWNER_PW.into()));
                    }
                    return if self.waited(20) { self.fail("the personal password was not set") } else { None };
                }
                if !self.resp.sent {
                    self.resp.sent = true;
                    for (name, kind, private) in [(JOGATINA, ChannelKind::Text, None), (ADM, ChannelKind::Text, Some(PrivateAccess::default())), (SALA2, ChannelKind::Voice, None)] {
                        if channel_id(app, name, kind).is_none() {
                            app.admin(AdminCmd::CreateChannel { name: name.into(), kind, private });
                        }
                    }
                }
                let ids = (Self::first_text(app), Self::first_voice(app), channel_id(app, SALA2, ChannelKind::Voice), channel_id(app, ADM, ChannelKind::Text), channel_id(app, JOGATINA, ChannelKind::Text));
                match ids {
                    (Some(g), Some(s), Some(s2), Some(a), Some(j)) => {
                        (self.resp.geral, self.resp.sala, self.resp.sala2, self.resp.adm, self.resp.jogatina) = (g, s, s2, a, j);
                        self.resp.sent = false;
                        app.select_text(g);
                        for text in [
                            "boa noite! alguém topa a partida depois do jantar?",
                            "vou abrir a sala de voz daqui a pouco — quem chegar primeiro escolhe o mapa, e quem perder paga a pizza da próxima sexta",
                            "a nova interface ficou bem mais calma de ler à noite",
                            "https://example.com/concord/placar-de-ontem",
                        ] {
                            Self::say(app, text);
                        }
                        self.goto(3, "channels ready; waiting for the partners");
                    }
                    _ if self.waited(30) => return self.fail("could not create the channels"),
                    _ => {}
                }
            }
            3 => {
                let want: Vec<&str> = if gate { Vec::new() } else { STREAMERS[..streamers()].iter().copied().chain([CAMERA]).collect() };
                let peers: Vec<User> = app.online.iter().filter(|u| u.username != self.args.name).cloned().collect();
                let all = want.iter().all(|n| peers.iter().any(|u| u.username == *n));
                if (all && !peers.is_empty()) || self.waited(90) {
                    log(&format!("partners online: {:?}", peers.iter().map(|u| u.username.as_str()).collect::<Vec<_>>()));
                    self.resp.peers = peers;
                    if gate || !app.sb.supported {
                        self.goto(5, "the scenes");
                    } else {
                        let dir = std::env::temp_dir().join(format!("concord-resp-{}", self.args.run_id));
                        let _ = std::fs::create_dir_all(&dir);
                        self.resp.dir = Some(dir);
                        self.goto(4, "uploading a few sounds");
                    }
                }
            }
            // A few sounds for the soundboard (one upload at a time).
            4 => {
                if app.sb.busy || app.sb.sounds.len() < self.resp.uploads || !self.waited(1) {
                    return if self.waited(40) { self.fail("an upload never finished") } else { None };
                }
                const SOUNDS: [(&str, &str, f32); 4] = [("aplausos", "👏", 0.6), ("buzina", "🚨", 0.5), ("risada", "😂", 0.7), ("tambor", "🥁", 0.4)];
                match SOUNDS.get(self.resp.uploads) {
                    Some((name, emoji, secs)) => {
                        let path = self.resp.dir.clone().unwrap_or_default().join(format!("{name}.wav"));
                        let _ = std::fs::write(&path, super::sound::test_wav(*secs));
                        if let Err(e) = app.add_sound(path, name, emoji) {
                            return self.fail(&format!("could not add a sound: {e}"));
                        }
                        self.resp.uploads += 1;
                        self.goto(4, "uploading");
                    }
                    None => self.goto(5, "the scenes"),
                }
            }
            5 => {
                self.resp.scenes = env_scenes(if gate { &GATE_SCENES } else { &POST });
                self.resp.scene = 0;
                self.resp.phase = Phase::Setup;
                self.resp.at = None;
                self.goto(6, "the matrix");
            }
            6 => match self.rs_matrix(app) {
                Ok(true) => {
                    log(&format!("{} stops checked, {} breaks of the rules", self.resp.checks, self.resp.defects.len()));
                    for d in &self.resp.defects {
                        log(&format!("  - {d}"));
                    }
                    // Leave the hub as it was found, and let the partners go.
                    let ids: Vec<_> = app.sb.sounds.iter().map(|s| s.id).collect();
                    for id in ids {
                        app.backend.send(crate::backend::Cmd::Sound(crate::soundboard::SoundCmd::Delete(id)));
                    }
                    if let Some(d) = self.resp.dir.take() {
                        let _ = std::fs::remove_dir_all(d);
                    }
                    app.backend.send(crate::backend::Cmd::SendMessage { channel: self.resp.geral, content: FIM.into(), extras: Default::default() });
                    app.repaint_ctx().set_zoom_factor(1.0);
                    self.goto(7, "done");
                }
                Ok(false) => {}
                Err(e) => return self.fail(&e),
            },
            7 if self.waited(2) => {
                if gate && !self.resp.defects.is_empty() {
                    return self.fail(&format!("{} breaks of the responsive rules: {}", self.resp.defects.len(), self.resp.defects.join(" | ")));
                }
                if self.resp.checks == 0 {
                    return self.fail("nothing was checked");
                }
                return self.pass();
            }
            _ => {}
        }
        None
    }

    /// `--mode host`: "Meu servidor" and "Parar servidor" across the matrix.
    fn run_resp_host(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => {
                layout::set_probing(true);
                self.resp.combos = env_combos(false);
                app.form.user = self.args.name.clone();
                app.host.tab = crate::host::HomeTab::Host;
                app.host_create("Turma da Lua");
                if let Some(e) = &app.host.error {
                    return self.fail(&format!("could not create the server: {e}"));
                }
                self.goto(1, "server created, opening it");
            }
            1 => {
                if !self.logged_in(app) {
                    return if self.waited(90) { self.fail(&format!("the hosted server did not come up: {:?}", app.form.error)) } else { None };
                }
                if app.roles.account.is_some_and(|a| a.must_set_password) {
                    if !self.resp.created {
                        self.resp.created = true;
                        app.set_personal_password(None, Some(OWNER_PW.into()));
                    }
                    return if self.waited(20) { self.fail("the personal password was not set") } else { None };
                }
                app.create_invite();
                self.resp.scenes = env_scenes(&HOST_SCENES);
                self.resp.scene = 0;
                self.resp.phase = Phase::Setup;
                self.goto(2, "the host panel");
            }
            2 => match self.rs_matrix(app) {
                Ok(true) => {
                    app.host.panel_open = false;
                    app.host_stop();
                    app.repaint_ctx().set_zoom_factor(1.0);
                    self.goto(3, "stopping");
                }
                Ok(false) => {}
                Err(e) => return self.fail(&e),
            },
            3 => {
                if app.screen == Screen::Connect && !app.host.hosting() || self.waited(30) {
                    log(&format!("{} stops checked, {} breaks of the rules", self.resp.checks, self.resp.defects.len()));
                    return self.pass();
                }
            }
            _ => {}
        }
        None
    }

    // ------------------------------------------------------------ a partner

    pub(super) fn run_resp_b(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => self.connect(app),
            1 => {
                if self.logged_in(app) {
                    let (Some(vc), Some(t)) = (Self::first_voice(app), Self::first_text(app)) else { return self.fail("no channels") };
                    app.select_text(t);
                    app.join_voice(vc);
                    self.goto(2, "joining voice");
                } else if self.waited(30) || app.form.error.is_some() {
                    return self.fail(&format!("could not log in: {:?}", app.form.error));
                }
            }
            2 => {
                if app.voice.channel.is_some() && self.waited(2) {
                    if let Some(hwnd) = self.args.hwnd {
                        app.start_share(Pick::Window(hwnd), crate::settings::Preset::P720p30, false);
                    } else if self.args.mode.as_deref() == Some("cam") {
                        app.toggle_camera();
                    }
                    self.goto(3, "in the room; waiting for the owner");
                } else if self.waited(60) {
                    return self.fail("voice join timed out");
                }
            }
            3 => {
                if Self::saw_message(app, FIM) {
                    return self.pass();
                }
                if self.waited(3000) {
                    return self.fail("the owner never finished");
                }
            }
            _ => {}
        }
        None
    }
}
