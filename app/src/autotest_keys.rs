//! v1.1 keyboard shortcuts (card K1), a child module of `autotest`: `keys-a` joins the first
//! voice room, sets up shortcuts and presses them with synthetic key events handed straight to
//! the shortcut matcher (`keybinds::feed`): never through the system, so nothing reaches the
//! person's other programs, and the test app installs no keyboard hook at all (checked).
//!
//! 1. Ctrl+D deafens and undeafens; the auto-repeat of a held key fires once.
//! 2. A plain key does nothing while a concord text field has the focus; Ctrl+D still does.
//! 3. M mutes and unmutes (the dock is told it has a microphone; nothing is captured).
//! 4. Push-to-talk on V: held talks, released mutes again, the repeats change nothing.
//! 5. A soundboard shortcut to a sound that does not exist: a notice, no crash, nothing played.
//! 6. F9 opens the Go Live picker (closed again at once; nothing is shared).
//! 7. With `VOX_AUTOTEST_SHOT_DIR`: the "Atalhos" page with these shortcuts, a conflict and the
//!    missing sound (`atalhos.png`).
//! 8. Ctrl+L leaves the voice room.
//!
//! Silent: `VOX_NO_PLAYBACK=1`, `VOX_NO_MIC=1`.

use super::Auto;
use crate::app::{App, SettingsPage};
use crate::keybinds::{self, Action, Combo, Keybind, SoundRef, MOD_ALT, MOD_CTRL};

const KEY_D: u32 = 0x44;
const KEY_L: u32 = 0x4C;
const KEY_M: u32 = 0x4D;
const KEY_V: u32 = 0x56;
const KEY_1: u32 = 0x31;
const KEY_F9: u32 = 0x78;
/// A sound id no test hub has.
const MISSING_SOUND: i64 = 987_654_321;

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST keys: {what}");
}

fn kb(action: Action, vk: u32, mods: u8, name: &str) -> Keybind {
    Keybind { action, key: Combo::new(vk, mods), key_name: name.to_string() }
}

/// A press with `repeats` auto-repeats, then the release.
fn tap(vk: u32, mods: u8, repeats: u32) {
    keybinds::feed(vk, true, mods);
    for _ in 0..repeats {
        keybinds::feed(vk, true, mods);
    }
    keybinds::feed(vk, false, mods);
}

impl Auto {
    fn fired(app: &App) -> usize {
        app.keys.fired.len()
    }

    pub(super) fn run_keys_a(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => self.connect(app),
            1 => {
                if self.logged_in(app) {
                    let Some(vc) = Self::first_voice(app) else { return self.fail("no voice channel") };
                    app.join_voice(vc);
                    self.goto(2, "joining voice");
                } else if self.waited(30) || app.form.error.is_some() {
                    return self.fail(&format!("could not log in: {:?}", app.form.error));
                }
            }
            2 if app.voice.channel.is_some() && self.waited(2) => {
                let (server_id, url) = (app.session_server_id.clone(), app.session_url.to_lowercase());
                app.settings.keybinds = vec![
                    kb(Action::ToggleDeafen, KEY_D, MOD_CTRL, "D"),
                    kb(Action::ToggleMute, KEY_M, 0, "M"),
                    kb(Action::PushToTalk, KEY_V, 0, "V"),
                    kb(Action::LeaveVoice, KEY_L, MOD_CTRL, "L"),
                    kb(Action::ToggleGoLive, KEY_F9, 0, "F9"),
                    kb(
                        Action::PlaySound(SoundRef {
                            server_id,
                            server_url: url,
                            server_name: app.server_name.clone(),
                            sound: MISSING_SOUND,
                            name: "buzina".into(),
                            emoji: "🚨".into(),
                        }),
                        KEY_1,
                        MOD_ALT,
                        "1",
                    ),
                ];
                app.sync_keybinds();
                if app.keys.os_hook() {
                    return self.fail("a test app installed the system keyboard hook");
                }
                log(&format!("{} shortcuts set; no system hook (synthetic keys only)", app.settings.keybinds.len()));
                self.keys_base = Self::fired(app);
                tap(KEY_D, MOD_CTRL, 4);
                self.goto(3, "Ctrl+D (with 4 auto-repeats)");
            }
            2 if app.voice.error.is_some() || self.waited(60) => return self.fail(&format!("could not join voice: {:?}", app.voice.error)),
            3 => {
                if Self::fired(app) != self.keys_base + 1 {
                    return self.fail(&format!("Ctrl+D with auto-repeat ran {} shortcuts (wanted 1)", Self::fired(app) - self.keys_base));
                }
                if app.voice.deafened {
                    log("Ctrl+D deafened, once");
                    tap(KEY_D, MOD_CTRL, 0);
                    self.goto(4, "Ctrl+D again");
                } else if self.waited(10) {
                    return self.fail("Ctrl+D did not deafen");
                }
            }
            4 => {
                if !app.voice.deafened {
                    log("Ctrl+D undeafened");
                    // Typing in a concord text field: the plain key is paused, Ctrl+D is not. (Each
                    // pump also sets the typing flag back from the real window, which is not typing.)
                    let before = Self::fired(app);
                    keybinds::set_typing(true);
                    tap(KEY_M, 0, 0);
                    app.keybinds_pump_now();
                    let plain = Self::fired(app) - before;
                    keybinds::set_typing(true);
                    tap(KEY_D, MOD_CTRL, 0);
                    app.keybinds_pump_now();
                    let total = Self::fired(app) - before;
                    if plain != 0 || total != 1 {
                        return self.fail(&format!("while typing: plain key ran {plain}, Ctrl+D ran {} (wanted 0 and 1)", total - plain));
                    }
                    self.goto(5, "typing: M paused, Ctrl+D deafens");
                } else if self.waited(10) {
                    return self.fail("Ctrl+D did not undeafen");
                }
            }
            5 => {
                if app.voice.deafened {
                    tap(KEY_D, MOD_CTRL, 0);
                    self.goto(6, "undeafen again");
                } else if self.waited(10) {
                    return self.fail("Ctrl+D while typing did not deafen");
                }
            }
            6 => {
                if !app.voice.deafened {
                    // The dock is told it has a microphone (nothing is captured: VOX_NO_MIC).
                    app.voice.no_mic = false;
                    tap(KEY_M, 0, 2);
                    self.goto(7, "M");
                } else if self.waited(10) {
                    return self.fail("could not undeafen");
                }
            }
            7 => {
                if app.voice.muted {
                    log("M muted");
                    tap(KEY_M, 0, 0);
                    self.goto(8, "M again");
                } else if self.waited(10) {
                    return self.fail("M did not mute");
                }
            }
            8 => {
                if !app.voice.muted && !crate::platform::global_ptt() {
                    // macOS: no push-to-talk yet (phase M4 of docs/PLANO-macOS.md); on to Alt+1.
                    log("M unmuted; no push-to-talk on this system, skipped");
                    app.notice = None;
                    self.keys_base = Self::fired(app);
                    tap(KEY_1, MOD_ALT, 0);
                    self.goto(12, "Alt+1 (a sound that does not exist)");
                } else if !app.voice.muted {
                    log("M unmuted");
                    app.settings.ptt_enabled = true;
                    app.keybinds_changed();
                    self.goto(9, "push-to-talk on");
                } else if self.waited(10) {
                    return self.fail("M did not unmute");
                }
            }
            9 => {
                if app.voice.muted && self.waited(1) {
                    log("push-to-talk on: muted until the key");
                    keybinds::feed(KEY_V, true, 0);
                    keybinds::feed(KEY_V, true, 0);
                    self.goto(10, "V held");
                } else if self.waited(10) {
                    return self.fail("push-to-talk on did not mute");
                }
            }
            10 => {
                if !app.voice.muted {
                    log("V held: talking");
                    keybinds::feed(KEY_V, true, 0);
                    keybinds::feed(KEY_V, false, 0);
                    self.goto(11, "V released");
                } else if self.waited(10) {
                    return self.fail("holding V did not open the microphone");
                }
            }
            11 => {
                if app.voice.muted {
                    log("V released: muted again");
                    app.settings.ptt_enabled = false;
                    app.keybinds_changed();
                    app.notice = None;
                    self.keys_base = Self::fired(app);
                    tap(KEY_1, MOD_ALT, 0);
                    self.goto(12, "Alt+1 (a sound that does not exist)");
                } else if self.waited(10) {
                    return self.fail("releasing V did not mute");
                }
            }
            12 if self.waited(1) => {
                app.keybinds_pump_now();
                if Self::fired(app) != self.keys_base + 1 {
                    return self.fail("Alt+1 did not run");
                }
                if app.sb.supported {
                    match &app.notice {
                        Some(n) if n.contains("não existe mais") => log("the missing sound: a notice, nothing played"),
                        other => return self.fail(&format!("no notice for the missing sound: {other:?}")),
                    }
                } else {
                    log("this hub has no soundboard: the sound shortcut does nothing");
                }
                app.notice = None;
                tap(KEY_F9, 0, 0);
                self.goto(13, "F9 (Go Live)");
            }
            13 => {
                if app.golive.open {
                    log("F9 opened the Go Live picker");
                    app.golive.open = false;
                    // The page as a person sees it: one conflict, the missing sound.
                    app.settings.keybinds.push(kb(Action::ToggleMute, KEY_D, MOD_CTRL, "D"));
                    app.sync_keybinds();
                    app.open_settings_at(SettingsPage::Keybinds);
                    self.goto(14, "the Atalhos page");
                } else if self.waited(10) {
                    return self.fail("F9 did not open the Go Live picker");
                }
            }
            14 if self.waited(2) => {
                super::shots::save(app, "atalhos");
                app.settings_ui.scroll_end = true;
                self.goto(15, "the page's end");
            }
            15 if self.waited(2) => {
                super::shots::save(app, "atalhos-2");
                app.settings_ui.open = false;
                app.settings.keybinds.pop();
                app.sync_keybinds();
                tap(KEY_L, MOD_CTRL, 0);
                self.goto(16, "Ctrl+L");
            }
            16 => {
                if app.voice.channel.is_none() && app.voice.joining.is_none() {
                    log(&format!("Ctrl+L left the voice room; shortcuts run: {:?}", app.keys.fired));
                    return self.pass();
                } else if self.waited(15) {
                    return self.fail("Ctrl+L did not leave the voice room");
                }
            }
            _ => {}
        }
        None
    }
}
