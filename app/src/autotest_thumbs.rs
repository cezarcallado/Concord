//! Go Live hover cards autotest (v1.1, a child module of `autotest`): `thumbs-a` / `thumbs-b` /
//! `thumbs-c`. A is the hub's owner (`dono`) and streams the gate's test window; B (`thB<n>`) is
//! given access to a private voice room; C (`thC<n>`) is not. Neither B nor C is in A's voice room
//! for the first part. Every step waits a second first (the hub's message rate limit). The three
//! talk in the first (public) text channel.
//!
//! 1. A makes a private voice room for B, joins the public voice room and shares the test window.
//! 2. B and C rest the pointer on A's "ao vivo" tag in the sidebar (the pointer handed to egui as
//!    the window's own input, never the system's): after the debounce the card asks the hub and
//!    shows A's still picture and the app's name ("test_window": the exe has no version resource)
//!    without being in the room. B saves the card (shot `card-hover-sidebar`) and the picture.
//! 3. A moves its live to the private room. B (access) gets the new picture from the sidebar, then
//!    joins the room: the stream card with "Assistir transmissão" (shot `botao-assistir`) and the
//!    hover card over it (shot `card-hover-palco`). C (no access) asks the hub directly for A's
//!    preview and gets nothing (`404`, the same as no live).
//!
//! Silent (`VOX_NO_PLAYBACK=1`, `VOX_NO_MIC=1`).

use std::time::Instant;

use eframe::egui;
use vox_proto::{ChannelKind, PrivateAccess, UserId};

use super::Auto;
use crate::app::{App, Pick, Share};
use crate::backend::Cmd;
use crate::roles::AdminCmd;
use crate::settings::Preset;

/// The owner's personal password in these runs (a throwaway dev hub).
const TH_PW: &str = "autotest-dono-thumbs";
const VOICE_NAME: &str = "th-cofre";
/// What the gate's test window is called (its exe has no version resource).
const TEST_APP: &str = "test_window";

#[derive(Default)]
pub struct State {
    geral: i64,
    a: Option<UserId>,
    b: Option<UserId>,
    private: Option<i64>,
    hovered_at: Option<Instant>,
}

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST thumbs: {what}");
}

/// The pointer handed to egui as the window's own input (never the system's pointer).
#[derive(Default)]
struct Pointer {
    events: Vec<egui::Event>,
}

impl egui::Plugin for Pointer {
    fn debug_name(&self) -> &'static str {
        "thumbs_pointer"
    }

    fn input_hook(&mut self, _ctx: &egui::Context, input: &mut egui::RawInput) {
        input.events.append(&mut self.events);
    }
}

fn point(ctx: &egui::Context, ev: egui::Event) {
    ctx.add_plugin(Pointer::default());
    ctx.with_plugin(|p: &mut Pointer| p.events.push(ev));
    ctx.request_repaint();
}

impl Auto {
    fn th_tag(&self, what: &str) -> String {
        format!("thtest {what} {}", self.args.run_id)
    }

    /// Rest the pointer on `user`'s live in `place`; `Some(true)` once the card has a picture and
    /// a name, `Some(false)` once it says there is none, `None` while waiting.
    fn th_hover(&mut self, app: &mut App, user: UserId, place: &'static str) -> Option<bool> {
        let ctx = app.repaint_ctx();
        let frame = ctx.cumulative_frame_nr();
        let Some(r) = app.stream_cards.anchor(user, place, frame) else {
            ctx.request_repaint();
            return None;
        };
        // On the tag itself in the sidebar; on a stream card, its top left (the middle is the
        // "Assistir" button).
        let at = if place == "card" { egui::pos2(r.left() + 24.0, r.top() + 18.0) } else { r.center() };
        point(&ctx, egui::Event::PointerMoved(at));
        self.th.hovered_at.get_or_insert_with(Instant::now);
        if let Some((size, name, game, icon)) = app.stream_cards.known(user) {
            if size.is_some() && name.is_some() {
                log(&format!("{place}: card for {user}: picture {size:?}, app {name:?}, game {game}, icon {icon}, after {} ms, {} lookups so far", self.th.hovered_at.map_or(0, |t| t.elapsed().as_millis()), app.stream_cards.asked));
                return Some(true);
            }
        }
        if app.stream_cards.missing(user) {
            return Some(false);
        }
        None
    }

    fn th_save_picture(app: &App, user: UserId, name: &str) {
        let Some(dir) = std::env::var_os("VOX_AUTOTEST_SHOT_DIR") else { return };
        if let Some(j) = app.stream_cards.picture(user) {
            let path = std::path::Path::new(&dir).join(name);
            match std::fs::write(&path, j) {
                Ok(()) => log(&format!("saved the picture ({} bytes) as {}", j.len(), path.display())),
                Err(e) => log(&format!("could not save {}: {e}", path.display())),
            }
        }
    }

    fn th_check_card(app: &App, user: UserId) -> Result<(), String> {
        let Some((size, name, game, _)) = app.stream_cards.known(user) else { return Err("no card".into()) };
        let (w, h) = size.ok_or("no picture")?;
        if !(16..=640).contains(&w) || !(16..=640).contains(&h) {
            return Err(format!("picture of {w}x{h}"));
        }
        if name.as_deref() != Some(TEST_APP) {
            return Err(format!("the app is {name:?}, not {TEST_APP:?}"));
        }
        if game {
            return Err("the test window passed for a game".into());
        }
        Ok(())
    }

    // ------------------------------------------------------------ A (the owner, streaming)

    pub(super) fn run_thumbs_a(&mut self, app: &mut App) -> Option<i32> {
        if self.step >= 3 && !self.waited(1) {
            return None;
        }
        match self.step {
            0 => {
                app.form.personal = TH_PW.into();
                self.connect(app);
            }
            1 => {
                if !self.logged_in(app) {
                    return if self.waited(30) { self.fail(&format!("could not log in: {:?}", app.form.error)) } else { None };
                }
                if !app.stream_cards.supported() {
                    return if self.waited(10) { self.fail("the hub does not announce hover previews") } else { None };
                }
                if app.roles.account.is_some_and(|a| a.must_set_password) {
                    app.set_personal_password(None, Some(TH_PW.into()));
                }
                self.goto(2, "logged in; the hub keeps hover previews");
            }
            2 => {
                let b = app.online.iter().find(|u| u.username.starts_with("thB")).map(|u| u.id);
                let c = app.online.iter().any(|u| u.username.starts_with("thC"));
                let pw = app.roles.account.is_some_and(|a| a.has_password && !a.must_set_password);
                match (Self::first_text(app), b, c) {
                    (Some(ch), Some(b), true) if pw => {
                        (self.th.geral, self.th.b) = (ch, Some(b));
                        app.select_text(ch);
                        let access = Some(PrivateAccess { roles: vec![], users: vec![b] });
                        app.admin(AdminCmd::CreateChannel { name: VOICE_NAME.into(), kind: ChannelKind::Voice, private: access });
                        self.goto(3, "B and C are online; making the private voice room for B");
                    }
                    _ if self.waited(60) => return self.fail("B or C never came online (or the password was not set)"),
                    _ => {}
                }
            }
            3 => {
                let Some(v) = app.channels.iter().find(|c| c.name == VOICE_NAME && c.kind == ChannelKind::Voice) else {
                    return if self.waited(20) { self.fail("the private voice room never showed") } else { None };
                };
                self.th.private = Some(v.id);
                let Some(public) = app.channels.iter().find(|c| c.kind == ChannelKind::Voice && c.private.is_none()).map(|c| c.id) else { return self.fail("no public voice room") };
                app.join_voice(public);
                self.goto(4, "joining the public voice room");
            }
            4 => {
                if app.voice.channel.is_some() {
                    let Some(hwnd) = self.args.hwnd else { return self.fail("no --hwnd given") };
                    app.start_share(Pick::Window(hwnd), Preset::P720p30, false);
                    self.goto(5, "in voice; sharing the test window");
                } else if self.waited(60) {
                    return self.fail(&format!("voice join failed: {:?}", app.voice.error));
                }
            }
            5 => {
                if matches!(app.voice.share, Share::Live { .. }) {
                    if !self.waited(4) {
                        return None; // the first picture goes up ~1.5 s after the start
                    }
                    Self::say(app, &self.th_tag("live"));
                    self.goto(6, "live; waiting for B and C to see the card from outside the room");
                } else if self.waited(30) {
                    return self.fail(&format!("Go Live did not start: {:?}", app.notice));
                }
            }
            6 => {
                if !Self::saw_message(app, &self.th_tag("b-public")) || !Self::saw_message(app, &self.th_tag("c-public")) {
                    return if self.waited(120) { self.fail("B or C never saw the card") } else { None };
                }
                let Some(v) = self.th.private else { return self.fail("no private room") };
                app.join_voice(v);
                self.goto(7, "part 2: moving to the private room");
            }
            7 => {
                if app.voice.channel == self.th.private && app.voice.joining.is_none() {
                    let Some(hwnd) = self.args.hwnd else { return self.fail("no --hwnd given") };
                    if !matches!(app.voice.share, Share::Live { .. } | Share::Starting) {
                        app.start_share(Pick::Window(hwnd), Preset::P720p30, false);
                    }
                    self.goto(8, "in the private room; live again");
                } else if self.waited(60) {
                    return self.fail("could not move to the private room");
                }
            }
            8 => {
                if matches!(app.voice.share, Share::Live { .. }) {
                    if !self.waited(4) {
                        return None;
                    }
                    Self::say(app, &self.th_tag("private"));
                    self.goto(9, "live in the private room; waiting for B and C");
                } else if self.waited(30) {
                    return self.fail(&format!("Go Live did not start again: {:?}", app.notice));
                }
            }
            9 => {
                if !Self::saw_message(app, &self.th_tag("b-private")) || !Self::saw_message(app, &self.th_tag("c-private")) {
                    return if self.waited(150) { self.fail("B or C never finished part 2") } else { None };
                }
                app.stop_share();
                self.goto(10, "done; stopping");
            }
            10 if self.waited(2) => {
                app.leave_voice();
                self.goto(11, "leaving voice");
            }
            11 if self.waited(2) => return self.pass(),
            _ => {}
        }
        None
    }

    // ------------------------------------------------------------ B (access to the private room)

    pub(super) fn run_thumbs_b(&mut self, app: &mut App) -> Option<i32> {
        if (self.step == 2 || self.step >= 6) && !self.waited(1) {
            return None;
        }
        match self.step {
            0 => self.connect(app),
            1 => {
                if let Some(code) = self.th_wait_live(app) {
                    return Some(code);
                }
            }
            2 => {
                let Some(a) = self.th.a else { return self.fail("no A") };
                match self.th_hover(app, a, "sidebar") {
                    Some(true) => {
                        if let Err(e) = Self::th_check_card(app, a) {
                            return self.fail(&format!("the sidebar card: {e}"));
                        }
                        if app.voice.channel.is_some() {
                            return self.fail("B is in a voice room (it must look from outside)");
                        }
                        self.goto(3, "the card shows A's picture and app from outside the room");
                    }
                    Some(false) => return if self.waited(20) { self.fail("the hub has no preview of A's live") } else { None },
                    None if self.waited(30) => return self.fail("no card after resting the pointer on A's live"),
                    None => {}
                }
            }
            3 => {
                if !self.waited(1) {
                    return None; // the card on screen for the shot
                }
                super::shots::save(app, "card-hover-sidebar");
                let a = self.th.a.unwrap_or_default();
                Self::th_save_picture(app, a, "preview-recebida.jpg");
                point(&app.repaint_ctx(), egui::Event::PointerGone);
                Self::say(app, &self.th_tag("b-public"));
                self.goto(4, "waiting for A to move to the private room");
            }
            4 => {
                let (Some(a), true) = (self.th.a, Self::saw_message(app, &self.th_tag("private"))) else {
                    return if self.waited(150) { self.fail("A never moved") } else { None };
                };
                let private = app.channels.iter().find(|c| c.name == VOICE_NAME).map(|c| c.id);
                if private.is_none() || app.voice_members.get(&a).and_then(|m| m.channel_id) != private {
                    return if self.waited(20) { self.fail("A is not in the private room for me (with access)") } else { None };
                }
                self.th.private = private;
                app.stream_cards.forget(a);
                self.th.hovered_at = None;
                self.goto(5, "A is live in the private room (I have access)");
            }
            5 => {
                let a = self.th.a.unwrap_or_default();
                match self.th_hover(app, a, "sidebar") {
                    Some(true) => {
                        if let Err(e) = Self::th_check_card(app, a) {
                            return self.fail(&format!("the private room's card: {e}"));
                        }
                        point(&app.repaint_ctx(), egui::Event::PointerGone);
                        let Some(v) = self.th.private else { return self.fail("no private room") };
                        app.join_voice(v);
                        self.goto(6, "the card works for the private room; joining it for the stage");
                    }
                    Some(false) => return if self.waited(20) { self.fail("no preview of A's private live for me") } else { None },
                    None if self.waited(30) => return self.fail("no card for the private live"),
                    None => {}
                }
            }
            6 => {
                let a = self.th.a.unwrap_or_default();
                if app.voice.channel != self.th.private || app.stream_cards.anchor(a, "card", app.repaint_ctx().cumulative_frame_nr()).is_none() {
                    app.repaint_ctx().request_repaint();
                    return if self.waited(40) { self.fail("A's stream card never showed on the stage") } else { None };
                }
                if !self.waited(3) {
                    return None;
                }
                super::shots::save(app, "botao-assistir");
                app.stream_cards.forget(a);
                self.th.hovered_at = None;
                self.goto(7, "the stage shows A's card with \"Assistir transmissão\"; hovering it");
            }
            7 => {
                let a = self.th.a.unwrap_or_default();
                match self.th_hover(app, a, "card") {
                    Some(true) => self.goto(8, "the hover card on the stream card"),
                    Some(false) => return self.fail("no preview on the stream card"),
                    None if self.waited(30) => return self.fail("no hover card on the stream card"),
                    None => {}
                }
            }
            8 => {
                super::shots::save(app, "card-hover-palco");
                point(&app.repaint_ctx(), egui::Event::PointerGone);
                app.leave_voice();
                Self::say(app, &self.th_tag("b-private"));
                self.goto(9, "done");
            }
            9 if self.waited(2) => return self.pass(),
            _ => {}
        }
        None
    }

    /// B and C: logged in, in the public text channel, until A says it is live (and is listed live
    /// in a voice room I am not in).
    fn th_wait_live(&mut self, app: &mut App) -> Option<i32> {
        if !self.logged_in(app) {
            return if self.waited(30) { self.fail(&format!("could not log in: {:?}", app.form.error)) } else { None };
        }
        if self.th.geral == 0 {
            if let Some(ch) = Self::first_text(app) {
                self.th.geral = ch;
                app.select_text(ch);
            }
        }
        if !app.stream_cards.supported() {
            return if self.waited(10) { self.fail("the hub does not announce hover previews") } else { None };
        }
        if !Self::saw_message(app, &self.th_tag("live")) {
            return if self.waited(150) { self.fail("A never went live") } else { None };
        }
        let a = app.voice_members.values().find(|m| m.user.username == "dono" && m.streaming).map(|m| m.user.id);
        match a {
            Some(a) => {
                self.th.a = Some(a);
                self.goto(2, "A is live in a room I am not in; resting the pointer on its live tag");
            }
            None if self.waited(20) => return self.fail("A is not listed live"),
            None => {}
        }
        None
    }

    // ------------------------------------------------------------ C (no access to the private room)

    pub(super) fn run_thumbs_c(&mut self, app: &mut App) -> Option<i32> {
        if self.step >= 3 && !self.waited(1) {
            return None;
        }
        match self.step {
            0 => self.connect(app),
            1 => {
                if let Some(code) = self.th_wait_live(app) {
                    return Some(code);
                }
            }
            2 => {
                let Some(a) = self.th.a else { return self.fail("no A") };
                match self.th_hover(app, a, "sidebar") {
                    Some(true) => {
                        if let Err(e) = Self::th_check_card(app, a) {
                            return self.fail(&format!("the sidebar card: {e}"));
                        }
                        point(&app.repaint_ctx(), egui::Event::PointerGone);
                        Self::say(app, &self.th_tag("c-public"));
                        self.goto(3, "the public live's card works for me too");
                    }
                    Some(false) => return if self.waited(20) { self.fail("the hub has no preview of A's live") } else { None },
                    None if self.waited(30) => return self.fail("no card after resting the pointer on A's live"),
                    None => {}
                }
            }
            3 => {
                let Some(a) = self.th.a else { return self.fail("no A") };
                if !Self::saw_message(app, &self.th_tag("private")) {
                    return if self.waited(150) { self.fail("A never moved") } else { None };
                }
                if app.voice_members.get(&a).is_some_and(|m| m.channel_id.is_some()) {
                    return if self.waited(10) { self.fail("A's private room shows for me") } else { None };
                }
                // Not listed anywhere for me: ask the hub directly, as a curious app would.
                app.stream_cards.forget(a);
                let r = app.stream_cards.probe(a);
                app.backend.send(Cmd::StreamPreview { user: r.user, have_icon: r.have_icon });
                self.goto(4, "A left my view (private room); asking the hub for its preview anyway");
            }
            4 => {
                let a = self.th.a.unwrap_or_default();
                if app.stream_cards.missing(a) {
                    log("the hub answered 404 for the private live's preview (no access)");
                    Self::say(app, &self.th_tag("c-private"));
                    self.goto(5, "refused, as it must be");
                } else if app.stream_cards.known(a).is_some() {
                    return self.fail("I got the preview of a live in a private room I may not see");
                } else if self.waited(15) {
                    return self.fail("no answer from the hub");
                }
            }
            5 if self.waited(2) => return self.pass(),
            _ => {}
        }
        None
    }
}
