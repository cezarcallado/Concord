//! Screenshots for design review (v1.0), a child module of `autotest`.
//!
//! - `kit`: no server; the `ui_kit` gallery fills the window (saved twice: the top, then
//!   scrolled to the end), then Configurações → Notificações (`settings-notificacoes`), and the
//!   gallery again, held for `--hold` seconds. Also `--autotest idle --show kit` (connected, the gallery over it).
//! - `shots`: the main screens one after the other: the home screen, the call view (with whoever
//!   else is in the first voice room, e.g. a `cam-a` partner with its camera on), a text channel's
//!   chat with a few messages, and the settings dialog. Passes once it has gone through them.
//!
//! With `VOX_AUTOTEST_SHOT_DIR=<dir>`, each screen is saved as `<dir>/<name>.png`: the window as
//! Windows renders it (`PrintWindow`, so the native video windows of the call are in it too). The
//! window stays behind the others the whole time (background test mode).

use std::time::Instant;

use eframe::egui;

use super::Auto;
use crate::app::App;

#[derive(Default)]
pub struct State {
    at: Option<Instant>,
    /// `shots3`: the settings section being shown, and where the test sounds are written.
    page: usize,
    dir: Option<std::path::PathBuf>,
}

/// `shots3`: the title of `examples/test_window.exe`'s windows, the only ones the Go Live picker
/// is left with for its screenshots (never a picture of the person's own windows).
const TEST_WINDOW_TITLE: &str = "vox test window";
/// `shots3`: the sounds put on the soundboard for its screenshot (name, emoji, tone length); the
/// hub takes at most 5 uploads in a row from one person.
const SHOT_SOUNDS: [(&str, &str, f32); 5] =
    [("aplausos", "👏", 0.6), ("buzina", "🚨", 0.5), ("risada", "😂", 0.7), ("tambor", "🥁", 0.4), ("trombeta", "🎺", 0.6)];

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST shots: {what}");
}

/// Save the window as `<VOX_AUTOTEST_SHOT_DIR>/<name>.png` (nothing without the variable).
/// (`VOX_AUTOTEST_SHOT_PREFIX`, e.g. "small-" or "dpi150-", goes in front of every name: the same
/// screens at another window size or display scale, see [`window`].)
pub(super) fn save(app: &App, name: &str) {
    let Some(dir) = std::env::var_os("VOX_AUTOTEST_SHOT_DIR") else { return };
    let prefix = std::env::var("VOX_AUTOTEST_SHOT_PREFIX").unwrap_or_default();
    let path = std::path::Path::new(&dir).join(format!("{prefix}{name}.png"));
    #[cfg(windows)]
    {
        let Some(hwnd) = app.video.parent else { return log("no window yet") };
        match super::cliptest::capture::client(hwnd, false) {
            Some(img) => match img.save(&path) {
                Ok(()) => log(&format!("saved {}", path.display())),
                Err(e) => log(&format!("could not save {}: {e}", path.display())),
            },
            None => log(&format!("{name}: the capture failed")),
        }
    }
    // macOS: `screencapture` needs the Screen Recording permission (a prompt), so the app keeps
    // egui's picture of its own last frame ([`collect`]): what is on the screen now, before this
    // pass changes anything (as the Windows capture sees it).
    #[cfg(not(windows))]
    {
        let _ = app;
        let last = LAST_FRAME.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let Some(image) = last else { return log(&format!("{name}: no window picture yet")) };
        let [w, h] = image.size;
        let bytes: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
        match image::RgbaImage::from_raw(w as u32, h as u32, bytes).map(|img| img.save(&path)) {
            Some(Ok(())) => log(&format!("saved {}", path.display())),
            Some(Err(e)) => log(&format!("could not save {}: {e}", path.display())),
            None => log(&format!("{name}: the picture has the wrong size")),
        }
    }
}

/// macOS: the tag of the every-frame window picture (other modules' pictures carry a name).
#[cfg(not(windows))]
struct FrameShot;

/// macOS: the window's last frame, as egui handed it back.
#[cfg(not(windows))]
static LAST_FRAME: std::sync::Mutex<Option<std::sync::Arc<egui::ColorImage>>> = std::sync::Mutex::new(None);

/// macOS, every pass of an autotest with `VOX_AUTOTEST_SHOT_DIR`: keep the picture of the frame
/// just shown and ask for the next one (see [`save`]).
#[cfg(not(windows))]
pub(super) fn collect(ctx: &egui::Context) {
    if std::env::var_os("VOX_AUTOTEST_SHOT_DIR").is_none() {
        return;
    }
    let got = ctx.input(|i| {
        i.raw.events.iter().rev().find_map(|e| match e {
            egui::Event::Screenshot { user_data, image, .. } if user_data.data.as_ref().is_some_and(|d| d.downcast_ref::<FrameShot>().is_some()) => Some(image.clone()),
            _ => None,
        })
    });
    if let Some(image) = got {
        *LAST_FRAME.lock().unwrap_or_else(|p| p.into_inner()) = Some(image);
    }
    ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(FrameShot)));
}

/// The window for design-review screenshots: its inner size in points (`VOX_AUTOTEST_WINDOW=900x600`,
/// default 1060 × 660) and a zoom that stands in for Windows' display scale (`VOX_AUTOTEST_ZOOM=1.25`:
/// everything 25% bigger, in a window 25% bigger in pixels, like a 125% screen). Only with
/// `VOX_AUTOTEST_SHOT_DIR`; otherwise the normal window.
pub fn window() -> ([f32; 2], f32) {
    let normal = ([1060.0, 660.0], 1.0);
    if std::env::var_os("VOX_AUTOTEST_SHOT_DIR").is_none() {
        return normal;
    }
    let zoom = std::env::var("VOX_AUTOTEST_ZOOM").ok().and_then(|z| z.trim().parse::<f32>().ok()).filter(|z| (0.5..=3.0).contains(z)).unwrap_or(1.0);
    let size = std::env::var("VOX_AUTOTEST_WINDOW")
        .ok()
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some([w.trim().parse::<f32>().ok()?, h.trim().parse::<f32>().ok()?])
        })
        .unwrap_or(normal.0);
    ([size[0] * zoom, size[1] * zoom], zoom)
}

impl Auto {
    fn shot_waited(&self, secs: f32) -> bool {
        self.shots.at.is_some_and(|t| t.elapsed().as_secs_f32() >= secs)
    }

    fn shot_next(&mut self, step: u32, what: &str) {
        self.shots.at = Some(Instant::now());
        self.goto(step, what);
    }

    /// `kit`: the component gallery, for a look and a screenshot.
    pub(super) fn run_kit(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => {
                app.kit = Some(Default::default());
                self.shot_next(1, "showing the ui_kit gallery");
            }
            1 if self.shot_waited(3.0) => {
                save(app, "kit");
                if let Some(g) = app.kit.as_mut() {
                    g.to_end = true;
                }
                self.shot_next(2, "the gallery's second half");
            }
            2 if self.shot_waited(2.0) => {
                save(app, "kit-2");
                // v1.1: Configurações → Notificações too ("Som dos avisos"; nothing is clicked).
                app.kit = None;
                app.open_settings_at(crate::app::SettingsPage::Notifications);
                self.shot_next(3, "settings: Notificações");
            }
            3 if self.shot_waited(2.0) => {
                save(app, "settings-notificacoes");
                app.settings_ui.open = false;
                app.kit = Some(Default::default());
                self.shot_next(4, "holding the gallery");
            }
            4 if self.shot_waited(self.args.hold_secs as f32) => return self.pass(),
            _ => {}
        }
        None
    }

    /// `shots`: home, call, chat and settings, each saved.
    pub(super) fn run_shots(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => self.shot_next(1, "home screen"),
            1 if self.shot_waited(3.0) => {
                save(app, "home");
                self.connect(app);
                self.shot_next(2, "connecting");
            }
            2 => {
                if self.logged_in(app) {
                    let Some(vc) = Self::first_voice(app) else { return self.fail("no voice channel") };
                    app.join_voice(vc);
                    self.shot_next(3, "joining voice");
                } else if self.shot_waited(30.0) || app.form.error.is_some() {
                    return self.fail(&format!("could not log in: {:?}", app.form.error));
                }
            }
            // Wait for the other(s) in the room and their camera, if any, to settle.
            3 if app.voice.channel.is_some() && app.members_here().len() >= 2 && self.shot_waited(8.0) => {
                save(app, "call");
                let Some(ch) = Self::first_text(app) else { return self.fail("no text channel") };
                app.select_text(ch);
                for text in [
                    "boa noite! alguém topa a partida depois do jantar?",
                    "vou abrir a sala de voz daqui a pouco",
                    "a nova interface ficou bem mais calma de ler à noite",
                ] {
                    Self::say(app, text);
                }
                self.shot_next(4, "chat with a few messages");
            }
            3 if self.shot_waited(90.0) => return self.fail("nobody else came to the voice room"),
            4 if self.shot_waited(4.0) => {
                save(app, "chat");
                app.open_settings();
                self.shot_next(5, "settings");
            }
            5 if self.shot_waited(3.0) => {
                save(app, "settings");
                app.settings_ui.open = false;
                self.shot_next(6, "done");
            }
            6 if self.shot_waited(1.0) => return self.pass(),
            _ => {}
        }
        None
    }

    /// `shots3` (v1.0 phase 3a): every settings section, "Configurar câmera" (the synthetic camera),
    /// the soundboard popup with a few sounds, the trim editor, the voice changer popup and the Go
    /// Live picker (left with the test windows only), each saved.
    pub(super) fn run_shots3(&mut self, app: &mut App) -> Option<i32> {
        use crate::app::SettingsPage;
        match self.step {
            0 => {
                self.connect(app);
                self.shot_next(1, "connecting");
            }
            1 => {
                if self.logged_in(app) {
                    let Some(vc) = Self::first_voice(app) else { return self.fail("no voice channel") };
                    app.join_voice(vc);
                    self.shot_next(2, "joining voice");
                } else if self.shot_waited(30.0) || app.form.error.is_some() {
                    return self.fail(&format!("could not log in: {:?}", app.form.error));
                }
            }
            2 if app.voice.channel.is_some() && self.shot_waited(3.0) => {
                self.shots.page = 0;
                app.open_settings_at(SettingsPage::ALL[0]);
                self.shot_next(3, "settings sections");
            }
            2 if self.shot_waited(60.0) => return self.fail("could not join voice"),
            // Each section; the long ones also scrolled to their end.
            3 if self.shot_waited(2.0) => {
                let page = SettingsPage::ALL[self.shots.page];
                save(app, &format!("settings-{}", page.slug()));
                if matches!(page, SettingsPage::Voice | SettingsPage::Video) {
                    app.settings_ui.scroll_end = true;
                    self.shot_next(4, "the section's end");
                } else {
                    self.shot_next(5, "next section");
                }
            }
            4 if self.shot_waited(1.5) => {
                let page = SettingsPage::ALL[self.shots.page];
                save(app, &format!("settings-{}-2", page.slug()));
                self.shot_next(5, "next section");
            }
            5 => {
                self.shots.page += 1;
                match SettingsPage::ALL.get(self.shots.page) {
                    Some(p) => {
                        app.open_settings_at(*p);
                        self.shot_next(3, p.title());
                    }
                    None => {
                        app.settings_ui.open = false;
                        app.open_camera_window();
                        self.shot_next(6, "Configurar câmera");
                    }
                }
            }
            6 if app.settings_ui.cam.live.is_some() && self.shot_waited(4.0) => {
                save(app, "camera");
                app.close_camera_window();
                let dir = std::env::temp_dir().join(format!("concord-shots3-{}", self.args.run_id));
                let _ = std::fs::create_dir_all(&dir);
                self.shots.dir = Some(dir);
                self.shots.page = 0;
                self.shot_next(7, "adding sounds");
            }
            6 if self.shot_waited(20.0) => return self.fail("the camera never reported"),
            // The sounds, one upload at a time.
            // (`page` counts the uploads started; the next one waits for the last to be listed.)
            7 if !app.sb.busy && app.sb.sounds.len() >= self.shots.page && self.shot_waited(0.5) => {
                if !app.sb.supported {
                    return if self.shot_waited(20.0) { self.fail("the hub reports no soundboard") } else { None };
                }
                if let Some((false, e)) = app.sb.message.clone() {
                    return self.fail(&format!("an upload failed: {e}"));
                }
                match SHOT_SOUNDS.get(self.shots.page) {
                    Some((name, emoji, secs)) => {
                        let path = self.shots.dir.clone().unwrap_or_default().join(format!("{name}.wav"));
                        let _ = std::fs::write(&path, super::sound::test_wav(*secs));
                        if let Err(e) = app.add_sound(path, name, emoji) {
                            return self.fail(&format!("could not add a sound: {e}"));
                        }
                        self.shots.page += 1;
                        self.shot_next(7, "uploading");
                    }
                    None => {
                        app.sb.message = None;
                        app.sb.open = true;
                        self.shot_next(8, "the soundboard popup");
                    }
                }
            }
            7 if self.shot_waited(30.0) => return self.fail("an upload never finished"),
            8 if self.shot_waited(2.0) => {
                save(app, "soundboard");
                // The trim editor over a longer recording.
                let path = self.shots.dir.clone().unwrap_or_default().join("gravacao-longa.wav");
                let _ = std::fs::write(&path, super::sound::test_wav(9.0));
                app.trim_choose(path);
                self.shot_next(9, "the trim editor");
            }
            9 if app.trim_editor().is_some_and(|e| e.ready()) && self.shot_waited(2.0) => {
                save(app, "trim");
                app.sb.add = None;
                app.sb.open = false;
                // The dock shows the voice changer only with a microphone; the screenshot run has
                // none (VOX_NO_MIC), so the dock is told it has one (nothing is captured).
                app.voice.no_mic = false;
                app.settings.voice_fx.effect = vox_client_core::VoiceEffect::Robot;
                egui::Popup::open_id(&app.repaint_ctx(), egui::Id::new(crate::call::dock::FX_MENU));
                self.shot_next(10, "the voice changer popup");
            }
            9 if self.shot_waited(20.0) => return self.fail("the trim editor never got ready"),
            10 if self.shot_waited(2.0) => {
                save(app, "voice-fx");
                egui::Popup::close_id(&app.repaint_ctx(), egui::Id::new(crate::call::dock::FX_MENU));
                app.settings.voice_fx.effect = vox_client_core::VoiceEffect::Off;
                app.voice.no_mic = true;
                app.open_golive();
                self.shot_next(11, "the Go Live picker");
            }
            11 if !app.golive.loading && self.shot_waited(1.0) => {
                // Only the test windows: never a picture of the person's own windows.
                app.golive.windows.retain(|w| w.title == TEST_WINDOW_TITLE);
                if app.golive.windows.is_empty() {
                    return self.fail("no test window to show (start examples/test_window.exe)");
                }
                app.golive.pick = app.golive.windows.first().map(|w| crate::app::Pick::Window(w.hwnd));
                let repaint = app.repaint.clone();
                app.golive.picker.release();
                app.golive.picker.sources_listed(&app.golive.windows, &[], std::sync::Arc::new(move || repaint.wake()), false);
                self.shot_next(12, "thumbnails of the test windows");
            }
            11 if self.shot_waited(20.0) => return self.fail("the sources never came"),
            12 if self.shot_waited(5.0) => {
                save(app, "golive");
                app.golive.mode = crate::app::ShareMode::AudioOnly;
                app.golive.pick = Some(crate::app::Pick::AllSound);
                let repaint = app.repaint.clone();
                app.golive.picker.release();
                app.golive.picker.sources_listed(&app.golive.windows, &[], std::sync::Arc::new(move || repaint.wake()), true);
                self.shot_next(13, "the audio-only picker");
            }
            13 if self.shot_waited(3.0) => {
                save(app, "golive-audio");
                app.golive.open = false;
                app.golive.mode = crate::app::ShareMode::ScreenAudio;
                // Leave the soundboard as it was found.
                let ids: Vec<_> = app.sb.sounds.iter().map(|s| s.id).collect();
                for id in ids {
                    app.backend.send(crate::backend::Cmd::Sound(crate::soundboard::SoundCmd::Delete(id)));
                }
                if let Some(d) = self.shots.dir.take() {
                    let _ = std::fs::remove_dir_all(d);
                }
                self.shot_next(14, "done");
            }
            14 if self.shot_waited(2.0) => return self.pass(),
            _ => {}
        }
        None
    }

    /// `calladex` (v1.0.1): the concord home screen ("home"), "calladex" typed there (the credits
    /// card, "calladex-creditos"), then in a text channel an ordinary message that contains the word
    /// (sent) and the exact `/calladex` (not sent; the members' card, "calladex"), and Configurações
    /// → Sobre ("sobre"). Fails if `/calladex` ever reaches the hub or the plain message does not.
    pub(super) fn run_calladex(&mut self, app: &mut App) -> Option<i32> {
        use crate::app::calladex::Flavour;
        const PLAIN: &str = "o calladex é segredo";
        match self.step {
            0 => self.shot_next(1, "home screen"),
            1 if self.shot_waited(3.0) => {
                save(app, "home");
                // The same path as the keyboard on the home screen (no field focused).
                app.calladex_home_typed("xcalla");
                app.calladex_home_typed("dex");
                self.shot_next(2, "the credits card");
            }
            2 if self.shot_waited(2.0) => {
                if app.calladex.as_ref().map(|c| c.flavour) != Some(Flavour::Credits) {
                    return self.fail("\"calladex\" typed on the home screen did not open the credits");
                }
                save(app, "calladex-creditos");
                app.calladex = None;
                app.form.url = self.args.server.clone();
                app.form.user = self.args.name.clone();
                app.form.password = self.args.password.clone();
                app.connect();
                self.shot_next(3, "connecting");
            }
            3 => {
                // Logged in, and the member list has arrived (the partners are online first).
                if self.logged_in(app) && (app.chatx.people.len() >= 3 || self.shot_waited(25.0)) {
                    let Some(ch) = Self::first_text(app) else { return self.fail("no text channel") };
                    app.select_text(ch);
                    Self::say(app, PLAIN);
                    Self::say(app, "/calladex");
                    if !app.input.is_empty() {
                        return self.fail("the input still holds /calladex");
                    }
                    match app.calladex.as_ref() {
                        Some(c) if c.flavour == Flavour::Members => log(&format!("the card lists {} members", c.entries.len())),
                        _ => return self.fail("/calladex did not open the card"),
                    }
                    self.shot_next(4, "the members' card");
                } else if self.shot_waited(40.0) || app.form.error.is_some() {
                    return self.fail(&format!("could not log in: {:?}", app.form.error));
                }
            }
            4 if Self::saw_message(app, PLAIN) && self.shot_waited(4.0) => {
                if Self::saw_message(app, "/calladex") {
                    return self.fail("/calladex was sent to the channel");
                }
                if app.calladex.as_ref().is_none_or(|c| c.entries.len() < 2) {
                    return self.fail("the card does not list the members");
                }
                save(app, "calladex");
                app.calladex = None;
                app.open_settings_at(crate::app::SettingsPage::About);
                self.shot_next(5, "Configurações → Sobre");
            }
            4 if self.shot_waited(20.0) => return self.fail("the ordinary message with the word was not sent"),
            5 if self.shot_waited(2.0) => {
                save(app, "sobre");
                app.settings_ui.open = false;
                self.shot_next(6, "done");
            }
            6 if self.shot_waited(1.0) => {
                if Self::saw_message(app, "/calladex") {
                    return self.fail("/calladex was sent to the channel");
                }
                return self.pass();
            }
            _ => {}
        }
        None
    }
}
