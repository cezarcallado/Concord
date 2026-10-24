//! v1.1 (E0): per-device identity keys, end to end in real apps (a child module of `autotest`).
//! The three processes share files in `--mode <dir>`.
//!
//! - `e2ee-a` (with its own `--settings-dir`: the real e2ee.json, DPAPI): logs in, its computer is
//!   registered ("este computador"), e2ee.json holds the identity with the secret protected, and
//!   writes `a-ready.txt`. When a second device of its account appears it must get the alert ("Um
//!   aparelho novo foi adicionado à sua conta: ..."), see it marked in Configurações → Privacidade
//!   → "Seus aparelhos" (with `VOX_AUTOTEST_SHOT_DIR`: `seus-aparelhos.png`), and remove it there
//!   ("Remover"); then the list has only this computer (`seus-aparelhos-depois.png`).
//! - `e2ee-a2`: the same name on "another computer" (memory-only store, its own label); waits for
//!   `a-ready.txt`, logs in, registers, and must then learn that it was removed (no alert of its
//!   own: A's computer was there first).
//! - `e2ee-b`: another member; must see A's device on the list, then A's second one, then only
//!   one again, and never get an alert (those are not its devices).
//!
//! Silent: `VOX_NO_PLAYBACK=1`, `VOX_NO_MIC=1`.

use std::path::PathBuf;
use std::time::Duration;

use eframe::egui;

use super::Auto;
use crate::app::settings_ui::SettingsPage;
use crate::app::App;
use crate::backend::e2ee::E2eeStatus;

/// The second device's name on the list.
const A2_LABEL: &str = "Notebook de teste";

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST e2ee: {what}");
}

/// Ask egui for a picture of the window (only with `VOX_AUTOTEST_SHOT_DIR`; never the screen).
pub(super) fn shot(ctx: &egui::Context, name: &str) {
    if std::env::var_os("VOX_AUTOTEST_SHOT_DIR").is_some() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(name.to_string())));
    }
}

/// Write the window pictures egui handed back this pass.
pub(super) fn save_shots(ctx: &egui::Context) {
    let Some(dir) = std::env::var_os("VOX_AUTOTEST_SHOT_DIR") else { return };
    let shots: Vec<(String, std::sync::Arc<egui::ColorImage>)> = ctx.input(|i| {
        i.raw
            .events
            .iter()
            .filter_map(|e| match e {
                egui::Event::Screenshot { user_data, image, .. } => Some((user_data.data.as_ref()?.downcast_ref::<String>()?.clone(), image.clone())),
                _ => None,
            })
            .collect()
    });
    for (name, img) in shots {
        let path = std::path::Path::new(&dir).join(format!("{name}.png"));
        let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
        let res = image::RgbaImage::from_raw(img.size[0] as u32, img.size[1] as u32, rgba).map(|i| i.save(&path));
        log(&format!("window picture {} ({}x{}, saved: {:?})", path.display(), img.size[0], img.size[1], res.map(|r| r.is_ok())));
    }
}

impl Auto {
    fn e2_dir(&self) -> PathBuf {
        PathBuf::from(self.args.mode.clone().unwrap_or_else(|| ".".into()))
    }

    fn e2_waited(&self, secs: f32) -> bool {
        self.step_at.elapsed() >= Duration::from_secs_f32(secs)
    }

    /// e2ee.json of this run's own settings folder: the identity is there, its secret is a DPAPI
    /// blob (never the key), and the device id is this computer's.
    fn e2_check_file(&self, this_device: &str) -> Result<(), String> {
        let path = crate::settings::e2ee_path().ok_or("no settings folder")?;
        if !crate::settings::dir_overridden() {
            return Err("e2ee-a needs --settings-dir".into());
        }
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let file: crate::e2ee_store::E2eeFile = serde_json::from_str(&text).map_err(|e| format!("e2ee.json: {e}"))?;
        let [id] = file.identities.as_slice() else { return Err(format!("{} identities in e2ee.json", file.identities.len())) };
        if id.device_id != this_device {
            return Err("e2ee.json names another device".into());
        }
        // DPAPI output (CRYPTPROTECT blob, hex): version 1 and the provider's GUID.
        if cfg!(windows) && (!id.secret.starts_with("01000000d08c9ddf0115d1118c7a00c04fc297eb") || id.secret.len() < 200) {
            return Err("the secret in e2ee.json is not a DPAPI blob".into());
        }
        if file.pins.values().flat_map(|m| m.values()).flatten().all(|p| p.device_id != this_device) {
            return Err("this computer's key is not among the pins".into());
        }
        log(&format!("e2ee.json: 1 identity (protected secret, {} chars), {} pinned keys", id.secret.len(), file.pins.values().flat_map(|m| m.values()).map(Vec::len).sum::<usize>()));
        Ok(())
    }

    pub(super) fn run_e2ee_a(&mut self, app: &mut App) -> Option<i32> {
        let ctx = app.repaint_ctx();
        save_shots(&ctx);
        ctx.request_repaint_after(Duration::from_millis(100));
        let v = app.privacy.view.clone();
        match self.step {
            0 => self.connect(app),
            1 => {
                if self.logged_in(app) && v.status == E2eeStatus::Active && v.mine.len() == 1 {
                    let Some(this) = v.this_device.clone() else { return self.fail("active without a device id") };
                    if v.mine[0].device_id != this {
                        return self.fail("the only device of my account is not this computer");
                    }
                    if let Err(e) = self.e2_check_file(&this) {
                        return self.fail(&e);
                    }
                    log(&format!("registered: \"{}\"", v.mine[0].label));
                    if let Err(e) = std::fs::write(self.e2_dir().join("a-ready.txt"), &this) {
                        return self.fail(&format!("cannot write a-ready.txt: {e}"));
                    }
                    self.goto(2, "waiting for a second device of my account");
                } else if self.waited(40) {
                    return self.fail(&format!("not registered: {:?} {:?}, {} devices", v.status, v.error, v.mine.len()));
                }
            }
            2 => {
                let this = v.this_device.clone().unwrap_or_default();
                let other = v.mine.iter().find(|d| d.device_id != this).cloned();
                match other {
                    Some(d) if !app.privacy.alerts.is_empty() => {
                        let want = crate::app::privacy_ui::new_device_alert(A2_LABEL);
                        if app.privacy.alerts != [want.clone()] || d.label != A2_LABEL {
                            return self.fail(&format!("alerts {:?}, device \"{}\"", app.privacy.alerts, d.label));
                        }
                        if !app.notice.as_deref().is_some_and(|n| n.contains(&want)) {
                            return self.fail(&format!("the notice bar says {:?}", app.notice));
                        }
                        log(&format!("alert: {want}"));
                        app.open_settings_at(SettingsPage::Privacy);
                        self.goto(3, "Configurações → Privacidade");
                    }
                    _ if self.waited(120) => return self.fail(&format!("no second device / alert: {} devices, alerts {:?}", v.mine.len(), app.privacy.alerts)),
                    _ => {}
                }
            }
            3 if self.e2_waited(1.5) => {
                if !app.settings_ui.open || app.settings_ui.page != SettingsPage::Privacy {
                    return self.fail("the Privacy page is not open");
                }
                shot(&ctx, "seus-aparelhos");
                self.goto(4, "picture of \"Seus aparelhos\"");
            }
            4 if self.e2_waited(1.0) => {
                let this = v.this_device.clone().unwrap_or_default();
                let Some(d) = v.mine.iter().find(|d| d.device_id != this).cloned() else { return self.fail("the other device is gone already") };
                if !app.privacy.new_devices.contains(&d.device_id) {
                    return self.fail("the new device is not marked \"novo\"");
                }
                // What "Remover" does.
                app.remove_e2ee_device(&d.device_id);
                self.goto(5, "removing the other device");
            }
            5 => {
                if v.mine.len() == 1 && v.removing.is_none() && v.mine[0].device_id == v.this_device.clone().unwrap_or_default() {
                    if v.error.is_some() {
                        return self.fail(&format!("error: {:?}", v.error));
                    }
                    let _ = std::fs::write(self.e2_dir().join("a-removed.txt"), "ok");
                    log("the other device is gone; only this computer is left");
                    self.goto(6, "removed");
                } else if self.waited(30) {
                    return self.fail(&format!("still {} devices ({:?})", v.mine.len(), v.error));
                }
            }
            6 if self.e2_waited(1.5) => {
                shot(&ctx, "seus-aparelhos-depois");
                self.goto(7, "picture after");
            }
            7 if self.e2_waited(1.5) => {
                if !self.e2_dir().join("b-done.txt").exists() && !self.waited(60) {
                    return None;
                }
                return self.pass();
            }
            _ => {}
        }
        None
    }

    pub(super) fn run_e2ee_a2(&mut self, app: &mut App) -> Option<i32> {
        let v = app.privacy.view.clone();
        match self.step {
            0 => {
                crate::backend::e2ee::set_label_override(A2_LABEL.into());
                if self.e2_dir().join("a-ready.txt").exists() {
                    self.connect(app);
                } else if self.waited(90) {
                    return self.fail("A never registered");
                }
            }
            1 => {
                if self.logged_in(app) && v.status == E2eeStatus::Active && v.mine.len() == 2 {
                    let mine = v.mine.iter().find(|d| Some(&d.device_id) == v.this_device.as_ref());
                    if mine.map(|d| d.label.as_str()) != Some(A2_LABEL) {
                        return self.fail(&format!("this device is listed as {:?}", mine.map(|d| &d.label)));
                    }
                    if !app.privacy.alerts.is_empty() {
                        return self.fail(&format!("an alert about A's computer, which was there first: {:?}", app.privacy.alerts));
                    }
                    log("registered as the second device of the account");
                    self.goto(2, "waiting to be removed");
                } else if self.waited(40) {
                    return self.fail(&format!("not registered: {:?} {:?}, {} devices", v.status, v.error, v.mine.len()));
                }
            }
            2 => {
                if v.status == E2eeStatus::Removed {
                    if v.this_device.is_some() {
                        return self.fail("removed but still has an identity");
                    }
                    if !app.notice.as_deref().is_some_and(|n| n.contains("removido")) {
                        return self.fail(&format!("the notice bar says {:?}", app.notice));
                    }
                    log("removed from the account on the other computer");
                    return self.pass();
                } else if self.waited(90) {
                    return self.fail(&format!("never removed: {:?}", v.status));
                }
            }
            _ => {}
        }
        None
    }

    pub(super) fn run_e2ee_b(&mut self, app: &mut App) -> Option<i32> {
        let v = app.privacy.view.clone();
        let a = app.online.iter().find(|u| u.username == "e2A1").map(|u| u.id);
        let a_devices = a.and_then(|id| v.devices_of.get(&id)).map_or(0, Vec::len);
        if !app.privacy.alerts.is_empty() {
            return self.fail(&format!("an alert about somebody else's device: {:?}", app.privacy.alerts));
        }
        match self.step {
            0 => self.connect(app),
            1 => {
                if self.logged_in(app) && v.status == E2eeStatus::Active && v.mine.len() == 1 {
                    log("registered");
                    self.goto(2, "waiting for A's device");
                } else if self.waited(40) {
                    return self.fail(&format!("not registered: {:?} {:?}", v.status, v.error));
                }
            }
            2 => {
                if a_devices == 1 {
                    log("sees A's device on the list");
                    self.goto(3, "waiting for A's second device");
                } else if self.waited(90) {
                    return self.fail(&format!("A's devices: {a_devices}"));
                }
            }
            3 => {
                if a_devices == 2 {
                    log("sees A's second device");
                    self.goto(4, "waiting for A to remove it");
                } else if self.waited(90) {
                    return self.fail(&format!("A's devices: {a_devices}"));
                }
            }
            4 => {
                if a_devices == 1 {
                    log("A's second device is gone from the list");
                    let _ = std::fs::write(self.e2_dir().join("b-done.txt"), "ok");
                    return self.pass();
                } else if self.waited(90) {
                    return self.fail(&format!("A's devices: {a_devices}"));
                }
            }
            _ => {}
        }
        None
    }
}
