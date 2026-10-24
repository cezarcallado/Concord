//! Right-click menu autotest (a child module of `autotest`): `menu-a` / `menu-b` (v0.6).
//!
//! - `menu-b` (name `menuB...`): joins the first voice channel, turns the synthetic camera on
//!   (`VOX_FAKE_CAMERA=1`), goes live with a test tone ("Só áudio") and says "ready"; waits for A.
//! - `menu-a` (name `menuA...`, same suffix): joins the same channel, goes live with the whole
//!   screen (1080p30; the preview of my own live is what the last part measures), then:
//!   1. opens B's menu from the sidebar and checks the rows the app drew match the model, and
//!      that Esc closes it;
//!   2. runs "Volume" (the slider's code path), "Ver perfil" and "Enviar mensagem" and checks the
//!      state (user preference, profile card, the DM opened);
//!   3. B's stream card: the menu leads with the stream; Assistir, volume/mute, Parar;
//!   4. B's camera: Ampliar, Ocultar (no tile) and Mostrar;
//!   5. Bloquear / Desbloquear, with "Ligar" disabled while blocked;
//!   6. my own entry: the status switch, Ensurdecer, Ampliar prévia, and "Ocultar prévia": the
//!      preview must be gone from the tile and the capture must hand its GPU objects back (the
//!      process-wide count of preview taps holding GPU objects goes to zero), then "Mostrar
//!      prévia" brings it back.
//!
//! `--show menu --hold N`: for a look at the real thing, A keeps the stream card's menu of B open
//! for N s (at most 60) before the checks go on, and B (usually the owner) keeps the menu of A's
//! stream card open meanwhile (the moderation section).
//!
//! Silent: `VOX_NO_PLAYBACK=1`, `VOX_NO_MIC=1`.

use std::time::Instant;

use eframe::egui;
use vox_proto::{Presence, UserId};

use super::Auto;
use crate::app::{App, Center, Pick, Share};
use crate::call::TileKey;
use crate::context_menu::{disabled_reason, ids, Act, Id, Lead, Origin};
use crate::settings::Preset;

#[derive(Default)]
pub struct State {
    peer: Option<UserId>,
    cam: bool,
    expect: Vec<Id>,
    sidebar: Vec<Id>,
    holders_on: usize,
    /// `--show menu`: the second menu / the watch has been done.
    shown_second: bool,
    unblocked_first: bool,
    at: Option<Instant>,
}

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST menu: {what}");
}

fn fake_camera() -> bool {
    std::env::var("VOX_FAKE_CAMERA").is_ok_and(|v| !v.is_empty() && v != "0")
}

impl Auto {
    fn menu_tag(&self, what: &str) -> String {
        format!("menutest {what} {}", self.args.run_id)
    }

    /// `--show menu`: seconds to keep a menu open for a screenshot.
    fn menu_hold(&self) -> u64 {
        if self.args.show.as_deref() == Some("menu") { self.args.hold_secs.min(60) } else { 0 }
    }

    fn menu_peer_name(&self) -> String {
        let n = &self.args.name;
        if let Some(rest) = n.strip_prefix("menuA") {
            format!("menuB{rest}")
        } else if let Some(rest) = n.strip_prefix("menuB") {
            format!("menuA{rest}")
        } else {
            String::new()
        }
    }

    pub(super) fn run_menu_b(&mut self, app: &mut App) -> Option<i32> {
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
                    if fake_camera() {
                        app.start_camera();
                    }
                    app.start_audio_share_source(vox_client_core::golive::ShareSource::AudioTone, None, false);
                    self.goto(3, "in voice; camera and tone live requested");
                } else if self.waited(60) {
                    return self.fail("voice join timed out");
                }
            }
            3 => {
                if matches!(app.voice.share, Share::Live { .. }) && (!fake_camera() || app.camera_on()) {
                    let text = self.menu_tag("ready");
                    Self::say(app, &text);
                    self.goto(4, "live; told A");
                } else if self.waited(40) {
                    return self.fail(&format!("could not go live: {:?}", app.notice));
                }
            }
            4 => {
                if self.menu_hold() > 0 && self.menu.at.is_none() {
                    // A is sharing the screen: show A's stream card menu as the owner sees it.
                    let a = app.members_here().into_iter().find(|m| m.streaming && Some(m.user.id) != app.my_id()).map(|m| m.user.clone());
                    if let Some(a) = a {
                        app.center = Center::Call;
                        // Show the owner's view: the personal password prompt was put off, which
                        // makes the app treat the owner as a member until it is set.
                        if let Some(acc) = app.roles.account.as_mut() {
                            acc.must_set_password = false;
                        }
                        let ctx = app.repaint_ctx();
                        app.open_person_menu(&ctx, &a, Origin::Tile, Lead::Stream, egui::pos2(520.0, 140.0));
                        self.menu.at = Some(Instant::now());
                    }
                }
                // Some seconds later A's picture is flowing: watch it, the open menu follows.
                if let Some(at) = self.menu.at.filter(|t| t.elapsed().as_secs() >= 6 && !self.menu.shown_second) {
                    let _ = at;
                    self.menu.shown_second = true;
                    if let Some(a) = app.members_here().into_iter().find(|m| m.streaming && Some(m.user.id) != app.my_id()).map(|m| m.user.id) {
                        app.watch_focused(&a.to_string());
                    }
                }
                if Self::saw_message(app, &self.menu_tag("done")) {
                    return self.pass();
                } else if Self::saw_message(app, &self.menu_tag("failed")) {
                    return self.fail("A's menu checks failed");
                } else if self.waited(300) {
                    return self.fail("A never finished");
                }
            }
            _ => {}
        }
        None
    }

    pub(super) fn run_menu_a(&mut self, app: &mut App) -> Option<i32> {
        let ctx = app.repaint_ctx();
        macro_rules! check {
            ($cond:expr, $($why:tt)*) => {
                if !$cond {
                    let why = format!($($why)*);
                    let text = self.menu_tag("failed");
                    Self::say(app, &text);
                    return self.fail(&why);
                }
            };
        }
        match self.step {
            0 => self.connect(app),
            1 => {
                if self.logged_in(app) {
                    let name = self.menu_peer_name();
                    match app.online.iter().find(|u| u.username == name) {
                        Some(u) => {
                            self.menu.peer = Some(u.id);
                            let Some(vc) = Self::first_voice(app) else { return self.fail("no voice channel") };
                            app.join_voice(vc);
                            self.goto(2, "joining voice");
                        }
                        None if self.waited(90) => return self.fail(&format!("{name} never came online")),
                        None => {}
                    }
                } else if self.waited(30) || app.form.error.is_some() {
                    return self.fail(&format!("could not log in: {}", app.form.error.clone().unwrap_or_default()));
                }
            }
            2 => {
                let peer = self.menu.peer?;
                let together = app.voice.channel.is_some() && app.voice_members.get(&peer).is_some_and(|m| m.channel_id == app.voice.channel && m.streaming);
                if together && Self::saw_message(app, &self.menu_tag("ready")) {
                    self.menu.cam = fake_camera() && app.voice.cameras.contains(&peer.to_string());
                    app.center = Center::Call;
                    // (For a screenshot the live is this window; otherwise the gate's test window
                    // (`--hwnd`). Never the person's desktop.)
                    let pick = match (self.menu_hold() > 0, app.video.parent, self.args.hwnd) {
                        (true, Some(hwnd), _) => Pick::Window(hwnd),
                        (_, _, Some(hwnd)) => Pick::Window(hwnd),
                        _ => return self.fail("no test window to share (--hwnd)"),
                    };
                    app.start_share(pick, Preset::P1080p30, false);
                    self.goto(3, "B is live in my room; sharing the screen");
                } else if self.waited(120) {
                    return self.fail("B never showed up live in my room");
                }
            }
            3 => {
                let peer = self.menu.peer?;
                if app.dms.is_blocked(peer) && !self.menu.unblocked_first {
                    // Left over from an earlier run: start clean.
                    self.menu.unblocked_first = true;
                    let b = app.known_user(peer)?;
                    app.run_menu_act(&ctx, &b, Act::Item(Id::Unblock), egui::pos2(300.0, 200.0));
                    log("B was blocked from an earlier run: unblocked");
                    return None;
                }
                if matches!(app.voice.share, Share::Live { .. }) && self.waited(if self.menu.unblocked_first { 8 } else { 4 }) {
                    let holders = vox_capture::preview_gpu_holders();
                    log(&format!("live; preview tiles placed={:?}; preview taps holding GPU objects: {holders}", app.video.placed.keys().collect::<Vec<_>>()));
                    self.menu.holders_on = holders;
                    if self.args.mode.as_deref() == Some("measure") {
                        // Measurement run (see the docs): 20 s each with the preview on, off, on.
                        log("PHASE preview on (tile in the call view)");
                        self.goto(20, "measuring with the preview on");
                        return None;
                    }
                    self.goto(4, "opening B's menu from the sidebar");
                    let b = app.known_user(self.menu.peer?)?;
                    let rows = app.menu_rows(&b, Origin::Sidebar, Lead::None);
                    let sidebar = ids(&rows);
                    log(&format!("B's rows: {sidebar:?}"));
                    for id in [Id::ViewProfile, Id::SendMessage, Id::Call, Id::CopyUsername, Id::Volume, Id::MuteLocal, Id::StreamWatch, Id::Block] {
                        check!(sidebar.contains(&id), "B's menu lacks {id:?}");
                    }
                    if self.menu.cam {
                        check!(sidebar.contains(&Id::CamFocus) && sidebar.contains(&Id::CamHide), "B's camera items are missing");
                    }
                    self.menu.sidebar = sidebar;
                    // First B's stream card (the same items, the stream first)...
                    let card = ids(&app.menu_rows(&b, Origin::Tile, Lead::Stream));
                    check!(card.first() == Some(&Id::StreamWatch), "the stream card does not lead with the stream: {card:?}");
                    self.menu.expect = card;
                    app.open_person_menu(&ctx, &b, Origin::Tile, Lead::Stream, egui::pos2(520.0, 140.0));
                    self.goto(4, "opened B's stream card");
                } else if matches!(app.voice.share, Share::Off) && self.waited(40) {
                    return self.fail(&format!("could not go live: {:?}", app.notice));
                }
            }
            4 if self.waited(1) => {
                check!(app.cmenu.open.is_some(), "the menu did not open");
                if self.menu_hold() > 0 {
                    // A look at the real thing: B's stream card (same person, other context).
                    let b = app.known_user(self.menu.peer?)?;
                    if self.menu.at.is_none() {
                        self.menu.at = Some(Instant::now());
                        app.center = Center::Call;
                    }
                    if self.menu.at.is_some_and(|t| t.elapsed().as_secs() >= self.menu_hold() / 2) && !self.menu.shown_second {
                        // Second half: my own stream card.
                        self.menu.shown_second = true;
                        let me = app.me.clone()?;
                        app.open_person_menu(&ctx, &me, Origin::Tile, Lead::Stream, egui::pos2(520.0, 140.0));
                    }
                    if self.menu.at.is_some_and(|t| t.elapsed().as_secs() < self.menu_hold()) {
                        return None;
                    }
                    self.menu.at = None;
                    self.args.show = None;
                    let _ = b;
                }
                check!(ids(&app.cmenu.last_rows) == self.menu.expect, "the stream card drew {:?}, the model says {:?}", ids(&app.cmenu.last_rows), self.menu.expect);
                // ...then, with that menu still open, a right click on B in the sidebar replaces it.
                let b = app.known_user(self.menu.peer?)?;
                self.menu.expect = self.menu.sidebar.clone();
                app.open_person_menu(&ctx, &b, Origin::Sidebar, Lead::None, egui::pos2(120.0, 160.0));
                self.goto(41, "stream card as modelled; replaced it with the sidebar menu");
            }
            41 if self.waited(1) => {
                check!(app.cmenu.open.is_some(), "the second menu did not open");
                check!(ids(&app.cmenu.last_rows) == self.menu.expect, "the replacing menu drew {:?}, the model says {:?}", ids(&app.cmenu.last_rows), self.menu.expect);
                // Esc closes it.
                ctx.input_mut(|i| {
                    i.events.push(egui::Event::Key { key: egui::Key::Escape, physical_key: Some(egui::Key::Escape), pressed: true, repeat: false, modifiers: egui::Modifiers::NONE })
                });
                self.goto(5, "menu open and as modelled; pressed Esc");
            }
            5 if self.waited(1) => {
                check!(app.cmenu.open.is_none(), "Esc did not close the menu");
                let b = app.known_user(self.menu.peer?)?;
                // Volume: the slider's code path.
                let mut pref = app.settings.user_audio(&b.username);
                pref.volume_pct = 150;
                app.set_user_pref(&b, pref, true);
                check!(app.settings.user_audio(&b.username).volume_pct == 150, "the volume did not stick");
                // Ver perfil.
                app.run_menu_act(&ctx, &b, Act::Item(Id::ViewProfile), egui::pos2(300.0, 200.0));
                check!(app.profile.popout.as_ref().is_some_and(|p| p.user.id == b.id), "Ver perfil did not open B's card");
                app.close_profile();
                // Enviar mensagem.
                app.run_menu_act(&ctx, &b, Act::Item(Id::SendMessage), egui::pos2(300.0, 200.0));
                self.goto(6, "volume, Ver perfil done; waiting for the DM");
            }
            6 => {
                let peer = self.menu.peer?;
                if app.dms.selected.is_some_and(|d| app.dms.conv(d).is_some_and(|c| c.peer.id == peer)) {
                    app.center = Center::Call;
                    let b = app.known_user(peer)?;
                    // The stream card leads with the stream, then the person's items.
                    let rows = app.menu_rows(&b, Origin::Tile, Lead::Stream);
                    check!(ids(&rows).first() == Some(&Id::StreamWatch), "the stream card does not lead with Assistir: {:?}", ids(&rows));
                    app.run_menu_act(&ctx, &b, Act::Item(Id::StreamWatch), egui::pos2(300.0, 200.0));
                    self.goto(7, "Enviar mensagem opened the DM; pressed Ouvir on B's card");
                } else if self.waited(20) {
                    check!(false, "Enviar mensagem did not open the DM ({:?})", app.notice);
                }
            }
            7 => {
                let peer = self.menu.peer?;
                if app.is_watching(&peer.to_string()) {
                    let b = app.known_user(peer)?;
                    let rows = app.menu_rows(&b, Origin::Tile, Lead::Stream);
                    let got = ids(&rows);
                    check!(got.first() == Some(&Id::StreamStop), "watching: the card leads with {:?}", got.first());
                    check!(got.contains(&Id::StreamVolume) && got.contains(&Id::StreamMute), "stream volume/mute missing");
                    check!(!got.contains(&Id::StreamFocus), "an audio-only live offers Ampliar");
                    let mut pref = app.settings.user_audio(&b.username);
                    pref.stream_volume_pct = 40;
                    pref.stream_muted = true;
                    app.set_user_pref(&b, pref, true);
                    check!(app.settings.user_audio(&b.username).stream_muted, "the stream mute did not stick");
                    let mut pref = app.settings.user_audio(&b.username);
                    pref.stream_volume_pct = 100;
                    pref.stream_muted = false;
                    app.set_user_pref(&b, pref, true);
                    app.run_menu_act(&ctx, &b, Act::Item(Id::StreamStop), egui::pos2(300.0, 200.0));
                    self.goto(8, "Parar de ouvir pressed");
                } else if self.waited(20) {
                    check!(false, "Assistir/Ouvir from the menu did not start the stream");
                }
            }
            8 => {
                let peer = self.menu.peer?;
                if !app.is_watching(&peer.to_string()) {
                    if self.menu.cam {
                        self.goto(9, "stopped; camera checks");
                    } else {
                        log("no synthetic camera (VOX_FAKE_CAMERA=1 not set): camera checks skipped");
                        self.goto(11, "stopped; block checks");
                    }
                } else if self.waited(20) {
                    check!(false, "Parar de ouvir did not stop the stream");
                }
            }
            9 => {
                let peer = self.menu.peer?;
                let b = app.known_user(peer)?;
                let rows = app.menu_rows(&b, Origin::Tile, Lead::Camera);
                check!(ids(&rows).first() == Some(&Id::CamFocus), "the camera card does not lead with Fixar câmera: {:?}", ids(&rows));
                app.run_menu_act(&ctx, &b, Act::Item(Id::CamFocus), egui::pos2(300.0, 200.0));
                check!(app.is_pinned(TileKey::Member(peer)), "Fixar câmera did not pin the camera card");
                app.run_menu_act(&ctx, &b, Act::Item(Id::CamFocus), egui::pos2(300.0, 200.0));
                check!(!app.is_pinned(TileKey::Member(peer)), "the second Fixar câmera did not unpin it");
                check!(app.camera_key_of(peer).is_some(), "B's camera has no tile");
                app.run_menu_act(&ctx, &b, Act::Item(Id::CamHide), egui::pos2(300.0, 200.0));
                let rows = ids(&app.menu_rows(&b, Origin::Tile, Lead::Camera));
                check!(app.camera_key_of(peer).is_none() && rows.contains(&Id::CamShow) && !rows.contains(&Id::CamHide), "Ocultar câmera did not hide it: {rows:?}");
                self.menu.at = Some(Instant::now());
                self.goto(10, "camera hidden for me; checking it comes back");
            }
            10 if self.waited(2) => {
                let peer = self.menu.peer?;
                let b = app.known_user(peer)?;
                app.run_menu_act(&ctx, &b, Act::Item(Id::CamShow), egui::pos2(300.0, 200.0));
                check!(app.camera_key_of(peer).is_some(), "Mostrar câmera did not bring the tile back");
                self.goto(11, "camera shown again; block checks");
            }
            11 => {
                let peer = self.menu.peer?;
                let b = app.known_user(peer)?;
                app.run_menu_act(&ctx, &b, Act::Item(Id::Block), egui::pos2(300.0, 200.0));
                let rows = app.menu_rows(&b, Origin::Sidebar, Lead::None);
                check!(app.dms.is_blocked(peer) && ids(&rows).contains(&Id::Unblock) && !ids(&rows).contains(&Id::Block), "Bloquear did not show Desbloquear: {:?}", ids(&rows));
                check!(disabled_reason(&rows, Id::Call).is_some(), "Ligar is enabled for a blocked person");
                self.goto(111, "blocked; unblocking in a moment (the hub takes the two requests in order)");
            }
            111 if self.waited(2) => {
                let peer = self.menu.peer?;
                let b = app.known_user(peer)?;
                app.run_menu_act(&ctx, &b, Act::Item(Id::Unblock), egui::pos2(300.0, 200.0));
                let rows = app.menu_rows(&b, Origin::Sidebar, Lead::None);
                check!(!app.dms.is_blocked(peer) && ids(&rows).contains(&Id::Block) && disabled_reason(&rows, Id::Call).is_none(), "Desbloquear did not restore the menu");
                self.goto(12, "block/unblock done; my own entry");
            }
            12 => {
                let me = app.me.clone()?;
                let rows = app.menu_rows(&me, Origin::Tile, Lead::Stream);
                let got = ids(&rows);
                log(&format!("my rows: {got:?}"));
                check!(got.first() == Some(&Id::SwapScreen), "my stream card does not lead with Trocar tela: {got:?}");
                for id in [Id::StopShare, Id::SelfPreviewFocus, Id::SelfPreviewToggle, Id::Deafen, Id::CameraToggle, Id::ViewProfile, Id::CopyUsername] {
                    check!(got.contains(&id), "my menu lacks {id:?}");
                }
                check!(!got.contains(&Id::Block) && !got.contains(&Id::Volume) && !got.contains(&Id::SendMessage), "my menu has items for other people: {got:?}");
                // Status quick switch (when the hub has profiles).
                if got.contains(&Id::Presence) {
                    app.run_menu_act(&ctx, &me, Act::Presence(Presence::Dnd), egui::pos2(300.0, 200.0));
                }
                app.run_menu_act(&ctx, &me, Act::Item(Id::Deafen), egui::pos2(300.0, 200.0));
                self.goto(13, "status and Ensurdecer pressed");
            }
            13 => {
                let me = app.me.clone()?;
                let has_presence = ids(&app.menu_rows(&me, Origin::Tile, Lead::Stream)).contains(&Id::Presence);
                let dnd = !has_presence || app.my_presence() == Presence::Dnd;
                if dnd && app.voice.deafened {
                    app.run_menu_act(&ctx, &me, Act::Item(Id::Deafen), egui::pos2(300.0, 200.0));
                    if has_presence {
                        app.run_menu_act(&ctx, &me, Act::Presence(Presence::Online), egui::pos2(300.0, 200.0));
                    }
                    self.goto(14, "status and deafen applied; undoing");
                } else if self.waited(20) {
                    check!(false, "status {:?} / deafened {} did not follow the menu", app.my_presence(), app.voice.deafened);
                }
            }
            14 => {
                let me = app.me.clone()?;
                let back = app.my_presence() == Presence::Online && !app.voice.deafened;
                if back || self.waited(20) {
                    check!(back, "status/deafen did not come back");
                    // Ampliar prévia, then Ocultar prévia (the tile must lose the preview and the capture
                    // must let go of its GPU objects).
                    app.center = Center::Call;
                    app.run_menu_act(&ctx, &me, Act::Item(Id::SelfPreviewFocus), egui::pos2(300.0, 200.0));
                    check!(app.self_preview_focused(), "Fixar prévia did not pin my stream card");
                    self.goto(15, "preview enlarged; waiting a moment for frames");
                }
            }
            15 if self.waited(3) => {
                let me = app.me.clone()?;
                let holders = vox_capture::preview_gpu_holders();
                log(&format!("preview ON, focused: preview taps holding GPU objects = {holders}"));
                check!(holders >= 1, "the preview is on but no tap holds GPU objects ({holders})");
                self.menu.holders_on = holders;
                app.run_menu_act(&ctx, &me, Act::Item(Id::SelfPreviewToggle), egui::pos2(300.0, 200.0));
                check!(!app.settings.show_self_preview && !app.my_preview_live() && !app.self_preview_focused(), "Ocultar prévia did not turn the preview off");
                check!(ids(&app.menu_rows(&me, Origin::Tile, Lead::Stream)).contains(&Id::SelfPreviewToggle), "no way back");
                self.goto(16, "Ocultar prévia pressed; waiting for the capture to let go");
            }
            16 => {
                let holders = vox_capture::preview_gpu_holders();
                if holders == 0 {
                    log(&format!("preview OFF: the capture gave its GPU objects back after {:.1}s", self.step_at.elapsed().as_secs_f32()));
                    check!(!app.video.placed.contains_key(vox_client_core::golive::SELF_SHARE_KEY), "the preview tile is still placed");
                    let me = app.me.clone()?;
                    app.run_menu_act(&ctx, &me, Act::Item(Id::SelfPreviewToggle), egui::pos2(300.0, 200.0));
                    check!(app.settings.show_self_preview && app.my_preview_live(), "Mostrar prévia did not turn it on");
                    self.goto(17, "Mostrar prévia pressed");
                } else if self.waited(12) {
                    check!(false, "{holders} preview tap(s) still hold GPU objects 12 s after turning the preview off");
                }
            }
            17 => {
                let holders = vox_capture::preview_gpu_holders();
                if holders >= 1 && app.video.placed.contains_key(vox_client_core::golive::SELF_SHARE_KEY) {
                    log(&format!("preview ON again: taps holding GPU objects = {holders}, tile placed"));
                    app.stop_share();
                    let text = self.menu_tag("done");
                    Self::say(app, &text);
                    self.goto(18, "preview is back; stopped the live");
                } else if self.waited(15) {
                    check!(false, "the preview did not come back (taps {holders}, placed {:?})", app.video.placed.keys().collect::<Vec<_>>());
                }
            }
            18 if self.waited(2) => return self.pass(),
            20 if self.waited(20) => {
                log(&format!("PHASE preview off (preview taps holding GPU objects: {})", vox_capture::preview_gpu_holders()));
                app.set_self_preview(false);
                self.goto(21, "measuring with the preview off");
            }
            21 if self.waited(20) => {
                log(&format!("PHASE preview on again (preview taps holding GPU objects: {})", vox_capture::preview_gpu_holders()));
                app.set_self_preview(true);
                self.goto(22, "measuring with the preview back on");
            }
            22 if self.waited(20) => {
                log(&format!("PHASE end (preview taps holding GPU objects: {})", vox_capture::preview_gpu_holders()));
                app.stop_share();
                let text = self.menu_tag("done");
                Self::say(app, &text);
                self.goto(18, "measurement over");
            }
            _ => {}
        }
        None
    }
}
