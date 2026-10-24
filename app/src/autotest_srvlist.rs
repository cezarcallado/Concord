//! The saved server list, end to end in real apps (v0.9.1; a child module of `autotest`):
//! `srvlist --mode phase1` then, as a second process with the same `--settings-dir`,
//! `srvlist --mode phase2` (the restart).
//!
//! Two dev hubs: hub 1 is `--server` / `--password` (named "gate1"), hub 2 is [`HUB2`] (named
//! "gate2", password [`HUB2_PW`]). The launcher writes a v0.9.0-shaped settings.json first: hub 1
//! as the single saved server, with its password DPAPI-encrypted the way v0.9.0 did it.
//!
//! - phase 1: the old single server became the list's only entry (migration). "+ Adicionar
//!   servidor", hub 2's address and password typed ("Lembrar a senha") -> saved as a second
//!   entry; a personal password set there gives this computer a login token. "Trocar de servidor",
//!   then one click on hub 1 logs in with its migrated password, no typing. No health check runs
//!   while logged in. The files on disk hold no plain password.
//! - phase 2 (restarted app): both entries show online; one click on hub 2 logs in with its saved
//!   password and the login token in place of the personal password, one on hub 1 with its saved
//!   password, switching through "Trocar de servidor".
//!   Hub 1's password is forgotten: a click opens the form filled in, asking only for it. Hub 2 is
//!   removed: its entry and its login token are gone, from memory and from the files.
//!
//! Silent (`VOX_NO_PLAYBACK=1`, `VOX_NO_MIC=1`); the app never takes the foreground.

use super::Auto;
use crate::app::{App, Screen};
use crate::backend::normalize_url;
use crate::servers::{plan, server_key, Plan};

/// The second dev hub (the launcher starts it with LiveKit on 7973-7975).
const HUB2: &str = "127.0.0.1:7998";
const HUB2_PW: &str = "srv-dois-456";
/// The personal password set on hub 2 (fresh data dir every run).
const PERSONAL_PW: &str = "autotest-srvlist-pessoal";

#[derive(Default)]
pub struct State {
    rounds: u32,
}

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST srvlist: {what}");
}

impl Auto {
    fn sl_hub1(&self) -> String {
        normalize_url(&self.args.server)
    }

    fn sl_id(&self, app: &App, url: &str) -> Option<u64> {
        let key = server_key(url, &self.args.name);
        app.settings.servers.servers.iter().find(|e| e.key() == key).map(|e| e.id)
    }

    /// Back on the home screen after "Trocar de servidor", with the list in front.
    fn sl_home(&self, app: &App) -> bool {
        app.screen == Screen::Connect && app.me.is_none() && !app.form.busy && app.session_url.is_empty()
    }

    /// One click on a saved entry, with the form emptied first (nothing is typed).
    fn sl_click(app: &mut App, id: u64) {
        app.form.url.clear();
        app.form.user.clear();
        app.form.password.clear();
        app.join_saved(id);
    }

    /// settings.json and servers.json as they are on disk.
    fn sl_files() -> Option<(String, String)> {
        let p = crate::settings::file_path()?;
        let settings = std::fs::read_to_string(&p).ok()?;
        let servers = std::fs::read_to_string(crate::settings::servers_path(&p)).ok()?;
        Some((settings, servers))
    }

    /// No password in plain text in either file.
    fn sl_no_plain_secrets(&self) -> Result<(), String> {
        let (a, b) = Self::sl_files().ok_or("the settings files are not on disk")?;
        for secret in [self.args.password.as_str(), HUB2_PW, PERSONAL_PW] {
            if a.contains(secret) || b.contains(secret) {
                return Err(format!("a password is in plain text on disk ({} chars)", secret.len()));
            }
        }
        Ok(())
    }

    fn sl_logged_into(&self, app: &App, name: &str) -> Result<bool, String> {
        if self.logged_in(app) {
            return if app.server_name == name { Ok(true) } else { Err(format!("logged into {:?}, wanted {name:?}", app.server_name)) };
        }
        if let Some(e) = app.form.error.clone().filter(|_| !app.form.busy) {
            return Err(format!("login failed: {e}"));
        }
        if self.waited(40) {
            return Err("no login after 40 s".into());
        }
        Ok(false)
    }

    pub(super) fn run_srvlist(&mut self, app: &mut App) -> Option<i32> {
        if self.args.mode.as_deref() == Some("phase2") {
            self.run_srvlist_2(app)
        } else {
            self.run_srvlist_1(app)
        }
    }

    fn run_srvlist_1(&mut self, app: &mut App) -> Option<i32> {
        let hub1 = self.sl_hub1();
        let hub2 = normalize_url(HUB2);
        match self.step {
            0 => {
                // Migration: v0.9.0's single saved server is the list's first (only) entry.
                let list = app.settings.servers.servers.clone();
                log(&format!("start: {} entries", list.len()));
                let [e] = list.as_slice() else { return self.fail(&format!("migration gave {} entries, wanted 1", list.len())) };
                if e.url != hub1 || !e.username.eq_ignore_ascii_case(&self.args.name) {
                    return self.fail(&format!("migrated entry is {} / {}", e.url, e.username));
                }
                // macOS: v0.9.0's DPAPI-encrypted password means nothing here: the app saves it
                // into this run's own secrets.json now, the way "Lembrar a senha" does. Phase 2 (a
                // new process) must find it again.
                #[cfg(target_os = "macos")]
                if app.settings.server_password(e).is_none() {
                    let id = e.id;
                    let Some(enc) = crate::settings::protect(&self.args.password) else { return self.fail("macOS: secrets.json refused to save the password") };
                    if let Some(x) = app.settings.servers.servers.iter_mut().find(|s| s.id == id) {
                        x.password_enc = Some(enc);
                        x.needs_password = true;
                    }
                    app.settings.sync_legacy_server();
                    app.settings.save();
                    log("macOS: hub 1's password saved in this run's secrets.json");
                }
                let list = app.settings.servers.servers.clone();
                let [e] = list.as_slice() else { return self.fail("the entry went away") };
                if app.settings.server_password(e).as_deref() != Some(self.args.password.as_str()) {
                    return self.fail("the migrated entry lost its saved password");
                }
                if app.servers.form_open {
                    return self.fail("with a saved server the list should lead, not the form");
                }
                if app.settings.server != hub1 || app.settings.saved_password().is_none() {
                    return self.fail("v0.9.0's own fields changed after loading");
                }
                // "+ Adicionar servidor", hub 2's address and password typed, "Lembrar a senha" on.
                app.servers_add_new();
                if !app.servers.form_open || !app.form.url.is_empty() {
                    return self.fail("\"+ Adicionar servidor\" did not open an empty form");
                }
                app.form.url = HUB2.into();
                app.form.user = self.args.name.clone();
                app.form.password = HUB2_PW.into();
                app.form.remember = true;
                app.form.save_server = true;
                app.connect();
                self.goto(1, "typed hub 2's address");
            }
            1 => match self.sl_logged_into(app, "gate2") {
                Err(e) => return self.fail(&e),
                Ok(false) => {}
                Ok(true) => {
                    let Some(id) = self.sl_id(app, &hub2) else { return self.fail("hub 2 was not saved after logging in") };
                    let e = app.settings.server_entry(id).cloned().unwrap_or_default();
                    log(&format!("hub 2 saved as #{id}: {} \"{}\", {} entries", e.url, e.server_name, app.settings.servers.servers.len()));
                    if app.settings.servers.servers.len() != 2 || e.server_name != "gate2" || e.url != hub2 {
                        return self.fail("the new entry is not right");
                    }
                    if app.settings.server_password(&e).as_deref() != Some(HUB2_PW) || !e.needs_password {
                        return self.fail("hub 2's password was not saved (\"Lembrar a senha\")");
                    }
                    if app.settings.server != hub2 {
                        return self.fail("v0.9.0's fields do not follow the last used server");
                    }
                    app.set_personal_password(None, Some(PERSONAL_PW.into()));
                    self.goto(2, "setting a personal password on hub 2");
                }
            },
            2 => {
                if let Some((false, e)) = &app.roles.pw_msg {
                    return self.fail(&format!("personal password failed: {e}"));
                }
                if app.roles.account.is_some_and(|a| a.has_password) {
                    let device = app.settings.device_token(&hub2, &self.args.name).is_some();
                    let personal = self.sl_id(app, &hub2).and_then(|id| app.settings.server_entry(id)).is_some_and(|e| e.personal);
                    log(&format!("personal password set; login token kept={device}, entry protected={personal}"));
                    if !device || !personal {
                        return self.fail("no login token for hub 2's entry");
                    }
                    app.switch_server();
                    self.goto(3, "Trocar de servidor");
                } else if self.waited(15) {
                    return self.fail("the personal password was not confirmed");
                }
            }
            3 if self.waited(2) => {
                if !self.sl_home(app) {
                    return self.fail("\"Trocar de servidor\" did not go back to the home screen");
                }
                if app.servers.form_open {
                    return self.fail("the list does not lead after switching");
                }
                let Some(id) = self.sl_id(app, &hub1) else { return self.fail("hub 1's entry is gone") };
                Self::sl_click(app, id);
                self.goto(4, "one click on hub 1 (migrated password)");
            }
            4 => match self.sl_logged_into(app, "gate1") {
                Err(e) => return self.fail(&e),
                Ok(false) => {}
                Ok(true) => {
                    if app.session_url != hub1 {
                        return self.fail(&format!("session at {}", app.session_url));
                    }
                    self.srv.rounds = app.servers.pinger.rounds;
                    log(&format!("in hub 1 with one click; {} health rounds so far", self.srv.rounds));
                    self.goto(5, "logged in; no health checks now");
                }
            },
            5 if self.waited(4) => {
                if app.servers.pinger.rounds != self.srv.rounds || app.servers.pinger.busy() {
                    return self.fail("health checks went on while logged in");
                }
                app.switch_server();
                self.goto(6, "Trocar de servidor");
            }
            6 if self.waited(2) => {
                if !self.sl_home(app) {
                    return self.fail("not back home");
                }
                if let Err(e) = self.sl_no_plain_secrets() {
                    return self.fail(&e);
                }
                let (settings, servers) = Self::sl_files().unwrap_or_default();
                let v: serde_json::Value = serde_json::from_str(&settings).unwrap_or_default();
                if v["server"].as_str() != Some(hub1.as_str()) || v["password_enc"].as_str().is_none() {
                    return self.fail("settings.json does not keep v0.9.0's fields for the last used server");
                }
                if !servers.contains("gate2") || !servers.contains("7998") {
                    return self.fail("servers.json does not have hub 2");
                }
                log("phase 1 done: both servers saved, files clean");
                return self.pass();
            }
            _ => {}
        }
        None
    }

    fn run_srvlist_2(&mut self, app: &mut App) -> Option<i32> {
        let hub1 = self.sl_hub1();
        let hub2 = normalize_url(HUB2);
        let (id1, id2) = (self.sl_id(app, &hub1), self.sl_id(app, &hub2));
        if self.step < 7 && (id1.is_none() || id2.is_none()) {
            return self.fail("the restarted app does not list both servers");
        }
        let (id1, id2) = (id1.unwrap_or_default(), id2.unwrap_or_default());
        match self.step {
            0 => {
                if app.servers.form_open {
                    return self.fail("the list does not lead after the restart");
                }
                let on = |id| app.servers.pinger.online.get(&id).copied();
                if on(id1) == Some(true) && on(id2) == Some(true) {
                    log(&format!("both online after {} round(s)", app.servers.pinger.rounds));
                    let e2 = app.settings.server_entry(id2).cloned().unwrap_or_default();
                    let has_pw = app.settings.server_password(&e2).is_some();
                    let device = app.settings.server_device_token(&e2).is_some();
                    if !has_pw || !device || !e2.personal || plan(&e2, has_pw, device, false) != Plan::Join {
                        return self.fail("hub 2 should go in with its saved password and this computer's login token");
                    }
                    Self::sl_click(app, id2);
                    self.goto(1, "one click on hub 2 (saved password + login token)");
                } else if self.waited(20) {
                    return self.fail(&format!("not both online: {:?} / {:?}", on(id1), on(id2)));
                }
            }
            1 => match self.sl_logged_into(app, "gate2") {
                Err(e) => return self.fail(&e),
                Ok(false) => {}
                Ok(true) => {
                    // The name is protected and nothing was typed: the login token proved it.
                    if app.roles.account.is_some_and(|a| !a.has_password) {
                        return self.fail("hub 2's name should have its personal password");
                    }
                    log("in hub 2 with one click (saved password + login token, no personal password typed)");
                    app.switch_server();
                    self.goto(2, "in hub 2; Trocar de servidor");
                }
            },
            2 if self.waited(2) => {
                if !self.sl_home(app) {
                    return self.fail("not back home");
                }
                Self::sl_click(app, id1);
                self.goto(3, "one click on hub 1 (saved password)");
            }
            3 => match self.sl_logged_into(app, "gate1") {
                Err(e) => return self.fail(&e),
                Ok(false) => {}
                Ok(true) => {
                    app.switch_server();
                    self.goto(4, "in hub 1; Trocar de servidor");
                }
            },
            4 if self.waited(2) => {
                if !self.sl_home(app) {
                    return self.fail("not back home");
                }
                // A missing credential: the form opens filled in and asks only for it.
                app.servers_forget_password(id1);
                if app.settings.server_entry(id1).is_some_and(|e| e.password_enc.is_some()) {
                    return self.fail("\"Esquecer senha\" kept the password");
                }
                Self::sl_click(app, id1);
                let asks = app.form.error.clone().unwrap_or_default();
                log(&format!("click without the password: form open={}, busy={}, asks: {asks}", app.servers.form_open, app.form.busy));
                if app.form.busy || !app.servers.form_open || !asks.contains("senha do servidor") {
                    return self.fail("the form did not open asking for the server password");
                }
                if app.form.url != hub1 || !app.form.user.eq_ignore_ascii_case(&self.args.name) || !app.form.password.is_empty() || app.form.need_personal {
                    return self.fail("the form is not filled in with the saved server");
                }
                app.form.password = self.args.password.clone();
                app.form.remember = true;
                app.connect();
                self.goto(5, "typed only the password");
            }
            5 => match self.sl_logged_into(app, "gate1") {
                Err(e) => return self.fail(&e),
                Ok(false) => {}
                Ok(true) => {
                    if app.settings.server_entry(id1).is_none_or(|e| e.password_enc.is_none()) {
                        return self.fail("the password typed with \"Lembrar a senha\" was not saved again");
                    }
                    if app.settings.servers.servers.len() != 2 {
                        return self.fail("the login made a duplicate entry");
                    }
                    app.switch_server();
                    self.goto(6, "Trocar de servidor");
                }
            },
            6 if self.waited(2) => {
                if !self.sl_home(app) {
                    return self.fail("not back home");
                }
                // "Remover da lista" (confirmed).
                app.servers_remove(id2);
                self.goto(7, "removed hub 2");
            }
            7 => {
                if self.sl_id(app, &hub2).is_some() || app.settings.servers.servers.len() != 1 {
                    return self.fail("hub 2 is still listed");
                }
                if app.settings.device_token(&hub2, &self.args.name).is_some() {
                    return self.fail("hub 2's login token survived the removal");
                }
                if let Err(e) = self.sl_no_plain_secrets() {
                    return self.fail(&e);
                }
                let (settings, servers) = Self::sl_files().unwrap_or_default();
                if settings.contains("7998") || servers.contains("7998") {
                    return self.fail("hub 2 is still in the files on disk");
                }
                log("hub 2 removed with its secrets");
                return self.pass();
            }
            _ => {}
        }
        None
    }
}
