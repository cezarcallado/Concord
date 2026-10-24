//! v1.1 interface fixes (a child module of `autotest`): `uix-a` (the owner, "dono") checks,
//! `uix-b` sits in the first voice room and waits for the end. With `VOX_AUTOTEST_SHOT_DIR`, A
//! also saves the screens (`uix-*.png`).
//!
//! 1. A makes many text channels and a few voice rooms: in the smallest window the sidebar's list
//!    is taller than its column, scrolls to the end and back (shot `uix-sidebar-rolada`).
//! 2. B's lamp ring in the sidebar (B is made to "speak": the ring is drawn whole, nothing cut,
//!    shot `uix-anel-sidebar`).
//! 3. The chat in the smallest window (760 × 440): a channel full of messages scrolls (up, stays
//!    there, back down; shot `uix-chat-pequeno`); the same in a DM and in the chat docked beside
//!    the cards of a call. Lists 1 and 3 are scrolled with the mouse wheel over their middle,
//!    handed to egui as the window's own input (`Inject`): it only scrolls what is under the
//!    pointer, so this also catches anything drawn over a list.
//! 4. A message's right-click menu, opened on a link (shot `uix-menu-mensagem`): its rows.
//! 5. The text box's right-click menu (shot `uix-menu-caixa`): Recortar / Copiar on with a
//!    selection, Selecionar tudo; "Colar" and "Selecionar tudo" done through the menu's path
//!    (never the real clipboard: a test must not change the user's).
//!
//! Silent: `VOX_NO_PLAYBACK=1`, `VOX_NO_MIC=1`.

use std::time::{Duration, Instant};

use eframe::egui;
use vox_proto::ChannelKind;

use super::Auto;
use crate::app::chat_extras;
use crate::app::{chat_view, App};
use crate::layout::{CenterMode, Open};
use crate::roles::AdminCmd;
use crate::ui_kit::{self, ScrollProbe};

/// The owner's personal password (test value; the hub is a throwaway one).
const UIX_PW: &str = "uix-dono-123";
/// Text channels and voice rooms made (more than the smallest window's sidebar can show).
const TEXTS: usize = 16;
const VOICES: usize = 3;
/// Messages written in the channel (several lines each) and in the DM.
const MESSAGES: u32 = 14;
const DM_MESSAGES: u32 = 12;
const LINK: &str = "https://example.com/concord/teste";
const DONE: &str = "uixdone";
/// The smallest window the app allows (points).
const SMALL: [f32; 2] = [760.0, 440.0];

#[derive(Default)]
pub struct State {
    geral: i64,
    voice: i64,
    peer: i64,
    dm: i64,
    sent: u32,
    asked: bool,
    probe: Option<ScrollProbe>,
    at: std::cell::Cell<Option<Instant>>,
}

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST uix: {what}");
}

fn text_name(i: usize) -> String {
    format!("canal-{i:02}")
}

fn voice_name(i: usize) -> String {
    format!("sala-{i}")
}

/// Input events handed to egui as if the window had them (the pointer over a spot, the mouse
/// wheel): never through the system, so the user's real pointer and the foreground are untouched.
#[derive(Default)]
struct Inject {
    events: Vec<egui::Event>,
}

impl egui::Plugin for Inject {
    fn debug_name(&self) -> &'static str {
        "uix_inject"
    }

    fn input_hook(&mut self, _ctx: &egui::Context, input: &mut egui::RawInput) {
        input.events.append(&mut self.events);
    }
}

/// Hand `events` to egui on its next pass ([`Inject`]).
fn inject(ctx: &egui::Context, events: impl IntoIterator<Item = egui::Event>) {
    // (Added once: a second add is ignored.)
    ctx.add_plugin(Inject::default());
    ctx.with_plugin(|p: &mut Inject| p.events.extend(events));
    ctx.request_repaint();
}

/// The pointer to `at`, then the mouse wheel by `dy` points (positive: the content moves down,
/// towards the top), in notches of at most 100.
fn wheel(ctx: &egui::Context, at: egui::Pos2, dy: f32) {
    let notches = (dy.abs() / 100.0).ceil().max(1.0);
    let step = egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: egui::vec2(0.0, dy / notches), phase: egui::TouchPhase::Move, modifiers: egui::Modifiers::NONE };
    inject(ctx, std::iter::once(egui::Event::PointerMoved(at)).chain(std::iter::repeat_n(step, notches as usize)));
}

fn channel_id(app: &App, name: &str, kind: ChannelKind) -> Option<i64> {
    app.channels.iter().find(|c| c.name == name && c.kind == kind).map(|c| c.id)
}

impl Auto {
    fn uix_lingered(&self, secs: f32) -> bool {
        let at = self.uix.at.get().unwrap_or_else(Instant::now);
        self.uix.at.set(Some(at));
        at.elapsed() >= Duration::from_secs_f32(secs)
    }

    fn uix_next(&mut self, step: u32, what: &str) {
        self.uix.at.set(None);
        self.uix.asked = false;
        self.goto(step, what);
    }

    /// Scroll the chat (or the sidebar) up from the end with the mouse wheel over its middle, check
    /// it moved and stays: `Ok(true)` once done, `Ok(false)` while going on. (The real wheel's
    /// path: egui gives it to the scroll area only when that area is what is under the pointer, so
    /// anything drawn over the list, or a list that does not know its height, fails here.)
    fn uix_scroll_check(&mut self, app: &App, key: &str, what: &str) -> Result<bool, String> {
        let ctx = app.repaint_ctx();
        let Some(p) = ui_kit::scroll_probe(&ctx, key) else { return Err(format!("{what}: never drawn")) };
        if !self.uix.asked {
            if !p.scrolls() {
                return Err(format!("{what}: the content ({:.0}) is not taller than what shows ({:.0}): nothing to scroll", p.content_h, p.view_h));
            }
            log(&format!("{what}: content {:.0}, showing {:.0}, at {:.0} of {:.0}; the wheel at {:?}", p.content_h, p.view_h, p.offset, p.max_offset(), p.rect.center()));
            // Chats start at the end; the sidebar at the top: go the other way (a positive wheel
            // moves the content down, towards the top).
            let up = p.offset > p.max_offset() / 2.0;
            wheel(&ctx, p.rect.center(), if up { p.max_offset() } else { -p.max_offset() });
            self.uix.asked = true;
            self.uix.probe = Some(p);
            // (The caller lingers again before the result is read.)
            self.uix.at.set(None);
            return Ok(false);
        }
        let Some(before) = self.uix.probe else { return Ok(false) };
        if !self.uix_lingered(1.5) {
            ctx.request_repaint();
            return Ok(false);
        }
        // (The pointer leaves: no hover in the shots.)
        inject(&ctx, [egui::Event::PointerGone]);
        let moved = (p.offset - before.offset).abs();
        if moved < before.max_offset() * 0.8 {
            return Err(format!("{what}: the wheel over it asked for {:.0} but it went from {:.0} to {:.0} (and stayed)", before.max_offset(), before.offset, p.offset));
        }
        log(&format!("{what}: scrolled from {:.0} to {:.0} and stayed there", before.offset, p.offset));
        Ok(true)
    }

    // ------------------------------------------------------------ A

    pub(super) fn run_uix_a(&mut self, app: &mut App) -> Option<i32> {
        let ctx = app.repaint_ctx();
        match self.step {
            0 => {
                app.form.personal = UIX_PW.into();
                self.connect(app);
            }
            1 => {
                if !self.logged_in(app) {
                    return if self.waited(30) { self.fail("could not log in") } else { None };
                }
                if app.roles.account.is_some_and(|a| a.must_set_password) {
                    if !self.uix.asked {
                        self.uix.asked = true;
                        app.set_personal_password(None, Some(UIX_PW.into()));
                    }
                    return if self.waited(20) { self.fail("the personal password was not set") } else { None };
                }
                let (Some(geral), Some(voice)) = (Self::first_text(app), Self::first_voice(app)) else { return self.fail("no channels") };
                (self.uix.geral, self.uix.voice) = (geral, voice);
                if self.uix.sent == 0 {
                    self.uix.sent = 1;
                    for i in 1..=TEXTS {
                        if channel_id(app, &text_name(i), ChannelKind::Text).is_none() {
                            app.admin(AdminCmd::CreateChannel { name: text_name(i), kind: ChannelKind::Text, private: None });
                        }
                    }
                    for i in 1..=VOICES {
                        if channel_id(app, &voice_name(i), ChannelKind::Voice).is_none() {
                            app.admin(AdminCmd::CreateChannel { name: voice_name(i), kind: ChannelKind::Voice, private: None });
                        }
                    }
                }
                let all = (1..=TEXTS).all(|i| channel_id(app, &text_name(i), ChannelKind::Text).is_some()) && (1..=VOICES).all(|i| channel_id(app, &voice_name(i), ChannelKind::Voice).is_some());
                if all {
                    app.select_text(geral);
                    app.notify.focus_override = Some(true);
                    // The smallest window the app allows.
                    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(SMALL[0], SMALL[1])));
                    self.uix.sent = 0;
                    self.uix_next(2, "channels made; writing in #geral");
                } else if self.waited(30) {
                    return self.fail("could not make the channels");
                }
            }
            2 => {
                // A few messages a second (the hub's limit is 5), several lines each.
                if self.uix.sent < MESSAGES && self.uix_lingered(0.35) {
                    self.uix.sent += 1;
                    let n = self.uix.sent;
                    let text = if n == MESSAGES - 2 {
                        format!("o link do teste: {LINK} (clique com o botão direito)")
                    } else {
                        format!("mensagem {n} do teste de rolagem\nsegunda linha da mensagem {n}\nterceira linha, para ocupar espaço")
                    };
                    Self::say(app, &text);
                    self.uix.at.set(None);
                }
                let arrived = app.logs.get(&self.uix.geral).map_or(0, |l| l.msgs.iter().filter(|m| m.content.contains("teste de rolagem") || m.content.contains(LINK)).count());
                let b_in = app.voice_members.values().find(|m| m.user.username != "dono" && m.channel_id == Some(self.uix.voice)).map(|m| m.user.id);
                if arrived as u32 >= MESSAGES && b_in.is_some() {
                    self.uix.peer = b_in.unwrap_or_default();
                    self.uix_next(3, "messages in, B in the voice room: the sidebar");
                } else if self.waited(90) {
                    return self.fail(&format!("{arrived} of {MESSAGES} messages arrived, B in the room: {}", b_in.is_some()));
                }
            }
            3 if self.uix_lingered(2.0) => {
                let size = ctx.content_rect().size();
                log(&format!("window content {:.0} × {:.0}", size.x, size.y));
                match self.uix_scroll_check(app, ui_kit::SIDEBAR_SCROLL, "sidebar") {
                    Ok(true) => {
                        // (Left at the end: the voice rooms, B's among them, are there.)
                        super::shots::save(app, "uix-sidebar-rolada");
                        self.uix_next(4, "sidebar scrolls; B's ring");
                    }
                    Ok(false) => {}
                    Err(e) => return self.fail(&e),
                }
            }
            4 => {
                // B "speaks" (its id in the speaking set, as the room reports it).
                app.voice.speaking.insert(self.uix.peer.to_string());
                ctx.request_repaint();
                if self.uix_lingered(1.5) {
                    super::shots::save(app, "uix-anel-sidebar");
                    app.voice.speaking.remove(&self.uix.peer.to_string());
                    log("B's speaking ring drawn in the sidebar");
                    self.uix_next(5, "the chat in the smallest window");
                }
            }
            5 if self.uix_lingered(1.0) => match self.uix_scroll_check(app, chat_view::CHAT_SCROLL, "chat (#geral, smallest window)") {
                Ok(true) => {
                    super::shots::save(app, "uix-chat-pequeno");
                    ui_kit::scroll_request(&ctx, chat_view::CHAT_SCROLL, 100_000.0);
                    self.uix_next(6, "the message menu");
                }
                Ok(false) => {}
                Err(e) => return self.fail(&e),
            },
            6 if self.uix_lingered(1.0) => {
                let Some(m) = app.logs.get(&self.uix.geral).and_then(|l| l.msgs.iter().find(|m| m.content.contains(LINK))) else { return self.fail("the message with the link is gone") };
                let id = m.id;
                let at = ctx.content_rect().center() - egui::vec2(60.0, 120.0);
                chat_extras::force_menu(&ctx, id, Some(LINK.to_string()), at);
                ctx.request_repaint();
                let a = chat_extras::MsgActions { features: app.chatx.features, social: true, delete: true, own: true };
                let info = chat_extras::MenuInfo { caption: &m.content, reactions: &m.reactions, me: app.my_id(), recent: &app.settings.recent_reactions };
                let rows = chat_extras::menu_labels(a, info, Some(LINK));
                log(&format!("message menu rows: {rows:?}"));
                for want in ["Responder", "Copiar texto", "Copiar link", "Abrir link", "Copiar ID da mensagem", "Apagar mensagem"] {
                    if !rows.contains(&want) {
                        return self.fail(&format!("the message menu has no \"{want}\": {rows:?}"));
                    }
                }
                self.uix_next(7, "message menu open");
            }
            7 => {
                ctx.request_repaint();
                if self.uix_lingered(1.5) {
                    if !egui::Popup::is_any_open(&ctx) {
                        return self.fail("the message menu did not open");
                    }
                    super::shots::save(app, "uix-menu-mensagem");
                    chat_extras::force_menu_off(&ctx);
                    // The text box: some text, all of it selected through the menu's path.
                    app.input = "texto do teste".into();
                    chat_view::queue_box_paste(&ctx, egui::Id::new("chat_input_box"), " colado");
                    self.uix_next(8, "the text box: Colar");
                }
            }
            8 if self.uix_lingered(1.0) => {
                if app.input != "texto do teste colado" && app.input != " coladotexto do teste" {
                    return self.fail(&format!("\"Colar\" left the box as {:?}", app.input));
                }
                log(&format!("Colar through the menu's path: {:?}", app.input));
                chat_view::queue_box_select_all(&ctx, egui::Id::new("chat_input_box"));
                ctx.request_repaint();
                self.uix_next(9, "the text box: Selecionar tudo");
            }
            9 if self.uix_lingered(1.0) => {
                let id = egui::Id::new("chat_input_box");
                let Some(r) = ctx.read_response(id) else { return self.fail("the text box was not drawn") };
                chat_view::force_box_menu(&ctx, id, r.rect.left_top() + egui::vec2(40.0, -150.0));
                ctx.request_repaint();
                let state = chat_view::box_menu_state(&ctx, id, &app.input);
                log(&format!("text box menu (Recortar, Copiar, Colar, Selecionar tudo) on: {state:?}"));
                if !(state[0] && state[1] && state[3]) {
                    return self.fail(&format!("with everything selected, Recortar / Copiar / Selecionar tudo must be on: {state:?}"));
                }
                self.uix_next(10, "text box menu open");
            }
            10 => {
                ctx.request_repaint();
                if self.uix_lingered(1.5) {
                    if !egui::Popup::is_any_open(&ctx) {
                        return self.fail("the text box menu did not open");
                    }
                    super::shots::save(app, "uix-menu-caixa");
                    chat_view::force_box_menu_off(&ctx);
                    app.input.clear();
                    // An empty box: nothing to cut, copy or select.
                    let state = chat_view::box_menu_state(&ctx, egui::Id::new("chat_input_box"), "");
                    if state[3] {
                        return self.fail("an empty box offers Selecionar tudo");
                    }
                    app.open_dm(self.uix.peer);
                    self.uix.sent = 0;
                    self.uix_next(11, "a DM in the smallest window");
                }
            }
            11 => {
                let Some(dm) = app.dms.selected else {
                    return if self.waited(20) { self.fail("the DM did not open") } else { None };
                };
                self.uix.dm = dm;
                if self.uix.sent < DM_MESSAGES && self.uix_lingered(0.4) {
                    self.uix.sent += 1;
                    app.dms.input = format!("dm {} do teste de rolagem\nsegunda linha\nterceira linha", self.uix.sent);
                    app.send_dm();
                    self.uix.at.set(None);
                }
                let arrived = app.dms.logs.get(&dm).map_or(0, |l| l.msgs.iter().filter(|m| m.content.contains("teste de rolagem")).count());
                if arrived as u32 >= DM_MESSAGES {
                    self.uix_next(12, "DM full");
                } else if self.waited(60) {
                    return self.fail(&format!("{arrived} of {DM_MESSAGES} DM lines arrived"));
                }
            }
            12 if self.uix_lingered(1.5) => {
                if app.open_in_center() != Open::Dm(self.uix.dm) {
                    return self.fail(&format!("the DM is not open: {:?}", app.open_in_center()));
                }
                match self.uix_scroll_check(app, chat_view::CHAT_SCROLL, "chat (DM, smallest window)") {
                    Ok(true) => {
                        super::shots::save(app, "uix-dm-pequeno");
                        app.join_voice(self.uix.voice);
                        self.uix_next(13, "the chat docked beside the cards");
                    }
                    Ok(false) => {}
                    Err(e) => return self.fail(&e),
                }
            }
            13 => {
                if app.voice.channel != Some(self.uix.voice) {
                    return if self.waited(60) { self.fail("could not join the voice room") } else { None };
                }
                if !self.uix.asked {
                    self.uix.asked = true;
                    app.dock_channel(self.uix.geral);
                    app.set_center_mode(CenterMode::CardsWithChat);
                }
                if self.uix_lingered(3.0) {
                    if app.chat_on_screen() != Some(self.uix.geral) {
                        return self.fail(&format!("the docked chat is not #geral: {:?}", app.chat_on_screen()));
                    }
                    self.uix_next(14, "docked chat");
                }
            }
            14 => match self.uix_scroll_check(app, chat_view::CHAT_SCROLL, "chat (docked beside the cards, smallest window)") {
                Ok(true) => {
                    super::shots::save(app, "uix-chat-ao-lado");
                    app.leave_voice();
                    app.set_center_mode(CenterMode::Chat);
                    app.select_text(self.uix.geral);
                    Self::say(app, DONE);
                    self.uix_next(15, "done; telling B");
                }
                Ok(false) => {}
                Err(e) => return self.fail(&e),
            },
            15 if self.uix_lingered(2.0) => return self.pass(),
            _ => {}
        }
        None
    }

    // ------------------------------------------------------------ B

    pub(super) fn run_uix_b(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => self.connect(app),
            1 => {
                if !self.logged_in(app) {
                    return if self.waited(30) { self.fail("could not log in") } else { None };
                }
                let (Some(geral), Some(voice)) = (Self::first_text(app), Self::first_voice(app)) else { return self.fail("no channels") };
                (self.uix.geral, self.uix.voice) = (geral, voice);
                app.select_text(geral);
                app.join_voice(voice);
                self.uix_next(2, "in the voice room; waiting for A");
            }
            2 => {
                if app.logs.get(&self.uix.geral).is_some_and(|l| l.msgs.iter().any(|m| m.content == DONE)) {
                    return self.pass();
                }
                if self.waited(280) {
                    return self.fail("A never finished");
                }
            }
            _ => {}
        }
        None
    }
}
