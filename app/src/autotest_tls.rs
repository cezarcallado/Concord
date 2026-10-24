//! v1.1 (T0): TLS between the app and the hub, end to end in real apps (a child module of
//! `autotest`). The clients exchange what they need through files in `--mode <dir>`.
//!
//! - `tls-a` (the owner, "dono", on the gate's dev hub): logs in by typing the address (TLS on
//!   first use: the lock is closed), sets its personal password, makes an invite and checks that
//!   the code carries the session's certificate (layout 2), writes it to `invite.txt`; once B
//!   chatted and joined voice, joins too (LiveKit through the local tunnel and the hub's port).
//! - `tls-b`: has only the code: joins with it (pinned TLS, rule 1), checks the lock, the saved
//!   entry's pin, chats, joins voice and sees A there.
//! - `tls-mitm`: logs in to hub A saving the password, leaves; the gate then puts hub B (another
//!   certificate) at the same address (`swapped.txt`). One click on the saved server must give "A
//!   identidade do servidor mudou" with nothing sent (`mismatch.txt`; the gate checks hub B's log
//!   and answers in `checked.txt`). Then "Confiar no novo código" logs in to B and the entry takes
//!   B's certificate.
//! - `tls-oldhub`: against the v1.0.2 hub (no TLS): typing the address asks before going plain
//!   (rule 4), then the open lock, chat and voice (LiveKit directly, as before); a click on the
//!   saved entry then goes plain without asking (rule 3).

use std::path::PathBuf;

use vox_proto::InviteCode;

use super::Auto;
use crate::app::tls_ui::{LOCK_OFF, LOCK_ON};
use crate::app::{App, Screen};

const OWNER_PW: &str = "autotest-tls-dono";

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST tls: {what}");
}

#[derive(Default)]
pub struct State {
    pin: Option<[u8; 32]>,
    entry: Option<u64>,
}

impl Auto {
    fn tls_dir(&self) -> PathBuf {
        PathBuf::from(self.args.mode.clone().unwrap_or_else(|| ".".into()))
    }

    fn tls_tag(&self, what: &str) -> String {
        format!("tlstest {what} {}", self.args.run_id)
    }

    /// The lock text of the server header (`None` before a login).
    fn lock_text(app: &App) -> Option<&'static str> {
        app.tls_lock().map(|(_, t, _)| t)
    }

    pub(super) fn run_tls_a(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => self.connect(app),
            1 => {
                if self.logged_in(app) {
                    let Some(pin) = app.tls.session else { return self.fail("a typed address of a 1.1 hub did not use TLS") };
                    if Self::lock_text(app) != Some(LOCK_ON) {
                        return self.fail(&format!("the lock says {:?}", Self::lock_text(app)));
                    }
                    log(&format!("logged in over TLS, server code {}", vox_proto::tls_server_code(&pin)));
                    self.tls.pin = Some(pin);
                    if app.roles.account.is_some_and(|a| a.must_set_password) {
                        app.set_personal_password(None, Some(OWNER_PW.into()));
                    }
                    self.goto(2, "making sure the Dono has a personal password");
                } else if self.waited(40) || (app.form.error.is_some() && !app.form.busy) {
                    return self.fail(&format!("could not log in: {:?}", app.form.error));
                }
            }
            2 => {
                if let Some((false, e)) = &app.roles.pw_msg {
                    return self.fail(&format!("personal password failed: {e}"));
                }
                if app.roles.account.is_some_and(|a| !a.must_set_password) && app.my_role().can_admin() {
                    app.create_invite();
                    self.goto(3, "creating an invite");
                } else if self.waited(20) {
                    return self.fail("the Dono's powers did not come on");
                }
            }
            3 => {
                if let Some(e) = &app.host.inv.error {
                    return self.fail(&format!("invite failed: {e}"));
                }
                if let Some((_, code)) = app.host.inv.last.clone() {
                    let decoded = match InviteCode::decode(&code) {
                        Ok(c) => c,
                        Err(e) => return self.fail(&format!("the new code does not decode: {e}")),
                    };
                    if decoded.tls_pin != self.tls.pin {
                        return self.fail("the invite does not carry the server's certificate");
                    }
                    if let Err(e) = std::fs::write(self.tls_dir().join("invite.txt"), &code) {
                        return self.fail(&format!("cannot write the code: {e}"));
                    }
                    log(&format!("invite with the pin: {} chars", code.len()));
                    self.goto(4, "waiting for B (chat and voice)");
                } else if self.waited(15) {
                    return self.fail("no invite came back");
                }
            }
            4 => {
                let me = app.my_id();
                let b = app.voice_members.values().find(|m| Some(m.user.id) != me && m.channel_id.is_some()).cloned();
                if let Some(b) = b.filter(|_| Self::saw_message(app, &self.tls_tag("hello"))) {
                    log(&format!("{} chatted and is in voice", b.user.username));
                    app.join_voice(b.channel_id.unwrap_or_default());
                    self.goto(5, "joining B in voice (through the tunnel)");
                } else if self.waited(150) {
                    return self.fail("B never chatted and joined voice");
                }
            }
            5 => {
                if app.voice.channel.is_some() {
                    Self::say(app, &self.tls_tag("ok"));
                    self.goto(6, "both in voice");
                } else if app.voice.joining.is_none() && app.voice.error.is_some() {
                    return self.fail(&format!("voice over TLS failed: {:?}", app.voice.error));
                } else if self.waited(45) {
                    return self.fail(&format!("could not join voice: {:?}", app.voice.error));
                }
            }
            6 => {
                if Self::saw_message(app, &self.tls_tag("bye")) {
                    app.leave_voice();
                    return self.pass();
                } else if self.waited(90) {
                    return self.fail("B never said bye");
                }
            }
            _ => {}
        }
        None
    }

    pub(super) fn run_tls_b(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => {
                let Ok(code) = std::fs::read_to_string(self.tls_dir().join("invite.txt")) else {
                    if self.waited(180) {
                        return self.fail("no invite code from A");
                    }
                    return None;
                };
                let decoded = match InviteCode::decode(code.trim()) {
                    Ok(c) => c,
                    Err(e) => return self.fail(&format!("the code does not decode: {e}")),
                };
                let Some(pin) = decoded.tls_pin else { return self.fail("the code has no pin (layout 1)") };
                // The gate's dev hub listens on 127.0.0.1 only, while the code names this PC's LAN
                // address: the same code (token and pin) with the loopback address instead.
                let code = InviteCode { addrs: vec![vox_proto::InviteAddr::Lan(std::net::Ipv4Addr::LOCALHOST, 8)], ..decoded }.encode();
                self.tls.pin = Some(pin);
                log(&format!("code with pin {}", vox_proto::tls_server_code(&pin)));
                app.form.url = code;
                app.form.user = self.args.name.clone();
                app.form.password.clear();
                app.connect();
                self.goto(1, "joining with only the invite");
            }
            1 => {
                if self.logged_in(app) {
                    if app.tls.session != self.tls.pin {
                        return self.fail("the session is not pinned to the invite's certificate");
                    }
                    if Self::lock_text(app) != Some(LOCK_ON) {
                        return self.fail(&format!("the lock says {:?}", Self::lock_text(app)));
                    }
                    let url = crate::backend::normalize_url(&app.session_url);
                    if !url.starts_with("http://") {
                        return self.fail(&format!("the saved address should stay http:// ({url})"));
                    }
                    let entry = app.settings.servers.servers.iter().find(|e| crate::backend::normalize_url(&e.url) == url).cloned();
                    if entry.as_ref().and_then(|e| e.tls_pin()) != self.tls.pin {
                        return self.fail("the saved server did not keep the pin");
                    }
                    if app.settings.device_token_pin(&url, &self.args.name) != self.tls.pin {
                        return self.fail("the login token is not tied to the certificate");
                    }
                    Self::say(app, &self.tls_tag("hello"));
                    let Some(vc) = Self::first_voice(app) else { return self.fail("no voice channel") };
                    app.join_voice(vc);
                    self.goto(2, "chatted; joining voice (through the tunnel)");
                } else if app.tls.mismatch.is_some() {
                    return self.fail("the invite's certificate was refused");
                } else if self.waited(40) || (app.form.error.is_some() && !app.form.busy) {
                    return self.fail(&format!("could not join with the invite: {:?}", app.form.error));
                }
            }
            2 => {
                if app.voice.channel.is_some() && Self::saw_message(app, &self.tls_tag("ok")) {
                    let here = app.voice_members.values().filter(|m| m.channel_id == app.voice.channel).count();
                    log(&format!("in voice with {here} people"));
                    if here < 2 {
                        return self.fail("A is not in the voice channel");
                    }
                    self.goto(3, "A joined");
                } else if app.voice.channel.is_none() && app.voice.joining.is_none() && app.voice.error.is_some() {
                    return self.fail(&format!("voice over TLS failed: {:?}", app.voice.error));
                } else if self.waited(120) {
                    return self.fail(&format!("no voice together with A: {:?}", app.voice.error));
                }
            }
            3 if self.waited(4) => {
                if app.voice.channel.is_none() {
                    return self.fail("voice dropped");
                }
                Self::say(app, &self.tls_tag("bye"));
                app.leave_voice();
                self.goto(4, "said bye");
            }
            4 if self.waited(2) => return self.pass(),
            _ => {}
        }
        None
    }

    pub(super) fn run_tls_mitm(&mut self, app: &mut App) -> Option<i32> {
        let dir = self.tls_dir();
        match self.step {
            0 => {
                app.form.remember = true;
                self.connect(app);
            }
            1 => {
                if self.logged_in(app) {
                    let Some(pin) = app.tls.session else { return self.fail("hub A not over TLS") };
                    let url = crate::backend::normalize_url(&app.session_url);
                    let Some(e) = app.settings.servers.servers.iter().find(|e| crate::backend::normalize_url(&e.url) == url).cloned() else {
                        return self.fail("the server was not saved");
                    };
                    if e.tls_pin() != Some(pin) || app.settings.server_password(&e).is_none() {
                        return self.fail("the entry has no pin or no saved password");
                    }
                    self.tls.pin = Some(pin);
                    self.tls.entry = Some(e.id);
                    log(&format!("hub A saved with code {}", vox_proto::tls_server_code(&pin)));
                    app.switch_server();
                    let _ = std::fs::write(dir.join("saved.txt"), vox_proto::tls_pin_hex(&pin));
                    self.goto(2, "saved; waiting for the gate to put hub B at the address");
                } else if self.waited(40) || (app.form.error.is_some() && !app.form.busy) {
                    return self.fail(&format!("could not log in to A: {:?}", app.form.error));
                }
            }
            2 => {
                if dir.join("swapped.txt").exists() {
                    let Some(id) = self.tls.entry else { return self.fail("no entry") };
                    app.join_saved(id);
                    self.goto(3, "one click on the saved server (hub B answers)");
                } else if self.waited(120) {
                    return self.fail("the gate never swapped the hubs");
                }
            }
            3 => {
                if self.logged_in(app) {
                    return self.fail("logged in to another certificate");
                }
                if let Some(m) = app.tls.mismatch.clone() {
                    if Some(m.expected) != self.tls.pin || Some(m.got) == self.tls.pin {
                        return self.fail("the mismatch names the wrong certificates");
                    }
                    if app.form.busy {
                        return self.fail("the form is still busy");
                    }
                    log(&format!("mismatch shown: saved {} / answered {}", vox_proto::tls_server_code(&m.expected), vox_proto::tls_server_code(&m.got)));
                    let _ = std::fs::write(dir.join("mismatch.txt"), vox_proto::tls_pin_hex(&m.got));
                    self.goto(4, "waiting for the gate's look at hub B's log");
                } else if self.waited(30) || (app.form.error.is_some() && !app.form.busy) {
                    return self.fail(&format!("no mismatch dialog: {:?}", app.form.error));
                }
            }
            4 => {
                if let Ok(verdict) = std::fs::read_to_string(dir.join("checked.txt")) {
                    if verdict.trim() != "ok" {
                        return self.fail(&format!("hub B saw something: {}", verdict.trim()));
                    }
                    log("hub B got no login and no request at all");
                    // "Confiar no novo código", then its second confirmation.
                    if let Some(m) = app.tls.mismatch.as_mut() {
                        m.confirming = true;
                    }
                    self.goto(5, "trusting the new code (second confirmation)");
                } else if self.waited(60) {
                    return self.fail("the gate never checked hub B");
                }
            }
            5 if self.waited(1) => {
                app.tls_trust_new();
                self.goto(6, "logging in to hub B with its new code");
            }
            6 | 7 => {
                if self.logged_in(app) {
                    let got = app.tls.session;
                    let e = self.tls.entry.and_then(|id| app.settings.server_entry(id)).cloned();
                    if got.is_none() || got == self.tls.pin || e.and_then(|e| e.tls_pin()) != got {
                        return self.fail("the entry did not take the new certificate");
                    }
                    log("logged in to hub B after trusting its code");
                    return self.pass();
                }
                let err = app.form.error.clone().filter(|_| !app.form.busy);
                match err {
                    // B is another server (v1.0.1's identity check): the saved password stays
                    // back too, and the form asks for it to be typed.
                    Some(e) if self.step == 6 && e.contains("não é o mesmo servidor") => {
                        log("the saved password stayed back too (another server); typing it");
                        app.form.password = self.args.password.clone();
                        app.connect();
                        self.goto(7, "logging in to hub B with the typed password");
                    }
                    Some(e) => return self.fail(&format!("could not log in after trusting: {e}")),
                    None if self.waited(40) => return self.fail("no answer after trusting"),
                    None => {}
                }
            }
            _ => {}
        }
        None
    }

    pub(super) fn run_tls_oldhub(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => {
                // The password is saved, so the later click on the entry logs in by itself.
                app.form.remember = true;
                self.connect(app);
            }
            1 => {
                if self.logged_in(app) {
                    return self.fail("went plain without asking (rule 4)");
                }
                if app.tls.plain_prompt.is_some() {
                    log("the plain-HTTP prompt is up; answering \"Conectar sem proteção\"");
                    app.tls_accept_plain();
                    self.goto(2, "connecting without protection");
                } else if self.waited(30) || (app.form.error.is_some() && !app.form.busy) {
                    return self.fail(&format!("no plain-HTTP prompt: {:?}", app.form.error));
                }
            }
            2 => {
                if self.logged_in(app) {
                    if app.tls.session.is_some() || Self::lock_text(app) != Some(LOCK_OFF) {
                        return self.fail(&format!("the lock says {:?}", Self::lock_text(app)));
                    }
                    let url = crate::backend::normalize_url(&app.session_url);
                    let e = app.settings.servers.servers.iter().find(|e| crate::backend::normalize_url(&e.url) == url).cloned();
                    let Some(e) = e else { return self.fail("not saved") };
                    if e.tls_pin().is_some() {
                        return self.fail("a pin was saved for a server without TLS");
                    }
                    self.tls.entry = Some(e.id);
                    Self::say(app, &self.tls_tag("plain"));
                    let Some(vc) = Self::first_voice(app) else { return self.fail("no voice channel") };
                    app.join_voice(vc);
                    self.goto(3, "open lock; chat and voice (LiveKit directly)");
                } else if self.waited(40) || (app.form.error.is_some() && !app.form.busy) {
                    return self.fail(&format!("could not log in plain: {:?}", app.form.error));
                }
            }
            3 => {
                if app.voice.channel.is_some() && Self::saw_message(app, &self.tls_tag("plain")) {
                    app.leave_voice();
                    self.goto(4, "chat and voice work; leaving");
                } else if app.voice.channel.is_none() && app.voice.joining.is_none() && app.voice.error.is_some() {
                    return self.fail(&format!("voice failed: {:?}", app.voice.error));
                } else if self.waited(60) {
                    return self.fail(&format!("no chat or voice: {:?}", app.voice.error));
                }
            }
            4 if self.waited(2) => {
                app.switch_server();
                self.goto(5, "back home");
            }
            5 if self.waited(2) => {
                let Some(id) = self.tls.entry else { return self.fail("no entry") };
                app.join_saved(id);
                self.goto(6, "one click on the saved server: plain without asking (rule 3)");
            }
            6 => {
                if app.tls.plain_prompt.is_some() {
                    return self.fail("asked again for a saved server");
                }
                if self.logged_in(app) && app.screen == Screen::Main {
                    if Self::lock_text(app) != Some(LOCK_OFF) {
                        return self.fail("the lock is not open");
                    }
                    return self.pass();
                } else if self.waited(40) || (app.form.error.is_some() && !app.form.busy) {
                    return self.fail(&format!("the saved server did not log in: {:?}", app.form.error));
                }
            }
            _ => {}
        }
        None
    }
}
