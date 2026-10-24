//! The Pix donation (v1.1, D1), a child module of `autotest`: role `donate`, alone on a dev hub.
//! Only with the `donate-test` feature, the one build that has the test key: a normal build has
//! neither this role nor the key.
//!
//! First login: the popup, once (shots at 1060 × 660 and 760 × 440, inside the window both times).
//! Logging in again: nothing. Configurações → "Apoiar o projeto" (shots at both sizes). 72 hours
//! reached in the tray (the counter runs there): nothing until the window is back; in voice:
//! nothing until the call ends, then the second popup. Then never a third.
//!
//! Silent: `VOX_NO_PLAYBACK=1`, `VOX_NO_MIC=1`.

use std::time::Duration;

use eframe::egui;

use super::shots::save;
use super::Auto;
use crate::app::settings_ui::SettingsPage;
use crate::app::App;
use crate::donate::{self, UsageClock};

/// The smallest window the app allows, and the normal one (points).
const SMALL: [f32; 2] = [760.0, 440.0];
const NORMAL: [f32; 2] = [1060.0, 660.0];
/// Long enough for a popup that should not come: past the calm wait and then some.
const NOTHING_SECS: u64 = 7;

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST donate: {what}");
}

fn resize(app: &App, size: [f32; 2]) {
    if let Some(c) = app.repaint.ctx() {
        c.send_viewport_cmd(egui::ViewportCommand::InnerSize(size.into()));
    }
}

/// The window has reached `size`, and the frames have settled.
fn sized(app: &App, size: [f32; 2]) -> bool {
    app.repaint.ctx().is_some_and(|c| (c.content_rect().size() - egui::Vec2::from(size)).abs().max_elem() < 2.0)
}

/// The popup is all inside the window.
fn popup_inside(app: &App) -> bool {
    app.repaint.ctx().is_some_and(|c| c.memory(|m| m.area_rect(egui::Id::new("donate_popup"))).is_some_and(|r| c.content_rect().contains_rect(r)))
}

/// What settings.json says now.
fn saved() -> Option<u32> {
    let text = std::fs::read_to_string(crate::settings::file_path()?).ok()?;
    serde_json::from_str::<serde_json::Value>(&text).ok()?.get("donate_popups_shown")?.as_u64().map(|n| n as u32)
}

impl Auto {
    pub(super) fn run_donate(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => {
                donate::use_test_pix();
                app.donate.enabled = true;
                // (These windows never have the focus: a person would be looking at it.)
                app.notify.focus_override = Some(true);
                self.connect(app);
            }
            1 => {
                if app.donate.open {
                    return self.fail("the popup came before the login");
                }
                if self.logged_in(app) {
                    self.goto(2, "logged in: the first popup");
                } else if self.waited(40) {
                    return self.fail(&format!("could not log in: {:?}", app.form.error));
                }
            }
            2 => {
                if app.donate.open {
                    if app.settings.donate_popups_shown != 1 || saved() != Some(1) {
                        return self.fail("the first popup was not counted and saved");
                    }
                    self.goto(20, "the first popup is up");
                } else if self.waited(20) {
                    return self.fail("no popup after the first login");
                }
            }
            // (A step of its own: egui shows a window from its second pass on.)
            20 if self.waited(1) => {
                save(app, "donate-popup-1060x660");
                if !popup_inside(app) {
                    return self.fail("the popup is out of the normal window");
                }
                resize(app, SMALL);
                self.goto(3, "the smallest window");
            }
            3 if sized(app, SMALL) && self.waited(2) => {
                save(app, "donate-popup-760x440");
                if !popup_inside(app) {
                    return self.fail("the popup is out of the smallest window");
                }
                if let Some(c) = app.repaint.ctx() {
                    app.donate_copy(&c, donate::TEST_PIX.key.to_string(), "Chave Pix copiada.");
                }
                if app.donate.copied.as_deref() != Some(donate::TEST_PIX.key) {
                    return self.fail("\"Copiar chave Pix\" did not give the key");
                }
                app.donate.open = false;
                resize(app, NORMAL);
                app.logout();
                self.goto(4, "dismissed; logging in again");
            }
            3 if self.waited(10) => return self.fail("the window did not reach the smallest size"),
            4 if self.waited(2) => {
                app.form.url = self.args.server.clone();
                app.form.user = self.args.name.clone();
                app.form.password = self.args.password.clone();
                app.connect();
                self.goto(5, "the second login");
            }
            5 if self.logged_in(app) => self.goto(6, "logged in again: no popup"),
            5 if self.waited(40) => return self.fail("could not log in again"),
            6 if app.donate.open => return self.fail("a second popup at the second login"),
            6 if self.waited(NOTHING_SECS) => {
                app.open_settings_at(SettingsPage::Donate);
                self.goto(7, "the settings page");
            }
            7 if sized(app, NORMAL) && self.waited(2) => {
                save(app, "donate-settings-1060x660");
                resize(app, SMALL);
                self.goto(8, "the settings page, small");
            }
            8 if sized(app, SMALL) && self.waited(2) => {
                save(app, "donate-settings-760x440");
                resize(app, NORMAL);
                app.settings_ui.open = false;
                // A clock of seconds, the first look a moment from now: 72 hours run out in the tray.
                app.donate.clock = UsageClock::with_period(Duration::from_secs(1));
                app.settings.usage_secs = donate::SECOND_POPUP_SECS - 2;
                app.hide_for_test();
                self.goto(9, "hidden: the counter runs, no popup");
            }
            9 if app.donate.open => return self.fail("a popup with the window in the tray"),
            9 if self.waited(NOTHING_SECS) => {
                if app.settings.usage_secs < donate::SECOND_POPUP_SECS {
                    return self.fail(&format!("the counter stood still in the tray: {}", app.settings.usage_secs));
                }
                log(&format!("in the tray the counter reached {}", app.settings.usage_secs));
                if let Some(c) = app.repaint.ctx() {
                    app.show_window(&c);
                }
                let Some(vc) = Self::first_voice(app) else { return self.fail("no voice channel") };
                app.join_voice(vc);
                self.goto(10, "72 hours reached: into a call");
            }
            10 if app.voice.channel.is_some() => self.goto(11, "in the call: no popup"),
            10 if self.waited(40) => return self.fail(&format!("could not join voice: {:?}", app.voice.error)),
            11 if app.donate.open => return self.fail("a popup during a call"),
            11 if self.waited(NOTHING_SECS) => {
                if app.settings.donate_popups_shown != 1 {
                    return self.fail("the second popup was counted during the call");
                }
                app.leave_voice();
                self.goto(12, "the call ended: the second popup");
            }
            12 => {
                if app.donate.open {
                    if app.settings.donate_popups_shown != 2 || saved() != Some(2) {
                        return self.fail("the second popup was not counted and saved");
                    }
                    self.goto(21, "the second popup is up");
                } else if self.waited(20) {
                    return self.fail("no second popup after the call");
                }
            }
            21 if self.waited(1) => {
                save(app, "donate-popup-2");
                app.donate.open = false;
                app.settings.usage_secs = 10 * donate::SECOND_POPUP_SECS;
                self.goto(13, "dismissed: never a third");
            }
            13 if app.donate.open => return self.fail("a third popup"),
            13 if self.waited(NOTHING_SECS) => return self.pass(),
            _ => {}
        }
        None
    }
}
