//! Soundboard autotest (a child module of `autotest`, so it drives the same `Auto` steps).
//!
//! - `sound-a`: joins the first voice channel, uploads a generated WAV (44.1 kHz stereo, silence
//!   around a sine) and the Vorbis fixture the way the "Adicionar som" dialog does, checks that a
//!   text file and a 6 s clip are refused with a reason, then plays the first sound, plays it again
//!   while B is deafened, again while B has the soundboard muted, renames it and deletes the second.
//! - `sound-b`: in the same voice channel. Sees both sounds appear, checks that A's play reaches
//!   its playback mixer at about the expected level (the headless mixer is pulled by hand and the
//!   RMS is logged), that being deafened or having the soundboard muted plays nothing, that it sees
//!   the rename, and that it is not allowed to delete A's sound.
//!
//! - v1.1, both: the ring and the stop. A plays the remaining sound; A's own ring and B's ring
//!   for A stay lit for about the clip's length (nobody has a microphone here: only the
//!   soundboard can light them). Then A plays it again and presses "Parar" 300 ms in: A's ring and
//!   B's go off at once, and both mixers (pulled in real time) stop well before the clip's end.
//!   Last, A opens the "Sons" panel: a click on it keeps it open, one on the chat beside it
//!   closes it (it lies over the chat and takes its mouse wheel).
//!
//! - `sound-trim`: one process, the trim editor. It generates a 20 s file of tone bursts, opens the
//!   "Adicionar som" dialog on it, checks the editor's read (duration, the default selection is
//!   the loudest 5 s), selects 3.2 s inside a burst (5 s is the most a selection can ever be),
//!   asks for "Ouvir trecho" (silent: it only reports the clip's length), uploads the selection
//!   and checks the hub stored a clip of about 3.2 s; then an 11 minute file is refused with a
//!   friendly message. It refuses to run without `VOX_NO_PLAYBACK`: the preview would be heard.
//!
//! Silent: no sound (`VOX_NO_PLAYBACK=1`), no microphone (`VOX_NO_MIC=1`); each instance needs its
//! own `VOX_SOUND_CACHE` so B really downloads the clips. Without `VOX_NO_PLAYBACK` the sounds go
//! through the real output device (use the inaudible "Steam Streaming Speakers" with
//! `--output-device`): then there is no mixer probe and the level checks are skipped.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;
use vox_proto::SoundId;

use super::Auto;
use crate::app::{App, Screen};
use crate::backend::Cmd;
use crate::soundboard::{Heard, SoundCmd};

/// The Vorbis fixture of the client-core tests (0.8 s of 440 Hz, 22.05 kHz mono).
const OGG: &[u8] = include_bytes!("../../crates/vox-client-core/tests/fixtures/tone-22k-mono.ogg");

/// Level A and B must hear: -16 LUFS clips at the default 50 % soundboard volume (-6 dB), as an
/// RMS over the clip including its quiet edges.
const RMS_RANGE: std::ops::Range<f32> = -28.0..-17.0;

#[derive(Default)]
pub struct State {
    dir: Option<PathBuf>,
    /// A: the uploaded sounds' ids (WAV, OGG).
    ids: Vec<SoundId>,
    last_play: Option<Instant>,
    /// B: how many plays and mixer measurements it had seen before the step it is waiting in.
    plays_before: usize,
    mix_before: usize,
    /// `--show panel`: the "Adicionar som" dialog has been opened for a look.
    add_shown: bool,
    /// `sound-trim`: how many sounds the hub had before the upload.
    base: usize,
    /// v1.1 ring / stop phase: when the ring (A's own, or A's as B sees it) came on.
    lit_at: Option<Instant>,
}

/// The ring of a 0.9 s clip stays lit for about that long (100 ms ticks, event delivery).
const RING_MS: std::ops::RangeInclusive<u128> = 600..=2000;
/// A clip stopped 300 ms in: the ring and the mix end well before its 0.9 s.
const STOPPED_MS: u32 = 750;

/// The mixer has no device: the backend measures what it mixes (`SoundEv::MixerHeard`).
fn headless() -> bool {
    std::env::var("VOX_NO_PLAYBACK").is_ok_and(|v| !v.is_empty() && v != "0")
}

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST soundboard: {what}");
}

/// A primary click at `at`, handed to egui on its next pass as if the window had it: never
/// through the system, so the user's real pointer and the foreground are untouched.
#[derive(Default)]
struct Clicks(Vec<egui::Event>);

impl egui::Plugin for Clicks {
    fn debug_name(&self) -> &'static str {
        "sound_clicks"
    }

    fn input_hook(&mut self, _ctx: &egui::Context, input: &mut egui::RawInput) {
        input.events.append(&mut self.0);
    }
}

fn click(ctx: &egui::Context, at: egui::Pos2) {
    let button = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
    // (Added once: a second add is ignored.)
    ctx.add_plugin(Clicks::default());
    ctx.with_plugin(|p: &mut Clicks| p.0.extend([egui::Event::PointerMoved(at), button(true), button(false)]));
    ctx.request_repaint();
}

/// 0.4 s of silence, 0.9 s of a 440 Hz sine, 0.4 s of silence; 44.1 kHz, stereo with different
/// channels (so it stays stereo), 16-bit.
pub(super) fn test_wav(tone_s: f32) -> Vec<u8> {
    let rate = 44_100u32;
    let n = |s: f32| (s * rate as f32) as usize;
    let total = n(0.4) + n(tone_s) + n(0.4);
    let mut data = Vec::with_capacity(total * 4);
    for i in 0..total {
        let (l, r) = if i >= n(0.4) && i < n(0.4) + n(tone_s) {
            let t = (i - n(0.4)) as f32 / rate as f32;
            ((t * 440.0 * std::f32::consts::TAU).sin() * 0.2, (t * 660.0 * std::f32::consts::TAU).sin() * 0.2)
        } else {
            (0.0, 0.0)
        };
        data.extend_from_slice(&((l * 32767.0) as i16).to_le_bytes());
        data.extend_from_slice(&((r * 32767.0) as i16).to_le_bytes());
    }
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * 4).to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(&data);
    v
}

impl Auto {
    fn snd_tag(&self, what: &str) -> String {
        format!("sndtest {what} {}", self.args.run_id)
    }

    fn snd_say(&self, app: &mut App, what: &str) {
        let tag = self.snd_tag(what);
        Self::say(app, &tag);
    }

    fn snd_saw(&self, app: &App, what: &str) -> bool {
        Self::saw_message(app, &self.snd_tag(what))
    }

    fn snd_login(&mut self, app: &mut App) -> Option<Option<i32>> {
        if app.screen == Screen::Main && app.ready && app.hub_up {
            return Some(None);
        }
        if self.waited(30) || (app.form.error.is_some() && !app.form.busy) {
            return Some(self.fail(&format!("could not log in: {}", app.form.error.clone().unwrap_or_default())));
        }
        None
    }

    fn snd_join(&mut self, app: &mut App) -> Option<i32> {
        match self.snd_login(app) {
            Some(Some(code)) => return Some(code),
            Some(None) => {}
            None => return None,
        }
        let Some(vc) = Self::first_voice(app) else { return self.fail("no voice channel") };
        app.join_voice(vc);
        self.goto(2, "joining voice");
        None
    }

    fn snd_message(app: &App) -> Option<(bool, String)> {
        app.sb.message.clone()
    }

    fn snd_dir(&mut self) -> PathBuf {
        self.sound.dir.get_or_insert_with(|| {
            let d = std::env::temp_dir().join(format!("concord-sound-autotest-{}", self.args.run_id));
            let _ = std::fs::create_dir_all(&d);
            d
        })
        .clone()
    }

    fn snd_play(&mut self, app: &mut App, id: SoundId) {
        app.play_sound(id);
        self.sound.last_play = Some(Instant::now());
    }

    /// Two seconds between plays (the hub's gap per person, and the app's own click gap).
    fn snd_can_play(&self) -> bool {
        self.sound.last_play.is_none_or(|t| t.elapsed() > Duration::from_millis(2400))
    }

    pub(super) fn run_sound_a(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => self.connect(app),
            1 => return self.snd_join(app),
            2 => {
                if app.voice.channel.is_none() {
                    if let Some(e) = app.voice.error.clone() {
                        return self.fail(&format!("voice join failed: {e}"));
                    }
                    return if self.waited(60) { self.fail("voice join took too long") } else { None };
                }
                if !app.sb.supported {
                    return if self.waited(20) { self.fail("the hub reports no soundboard") } else { None };
                }
                let dir = self.snd_dir();
                let wav = dir.join("bipe.wav");
                std::fs::write(&wav, test_wav(0.9)).ok();
                std::fs::write(dir.join("vorbis.ogg"), OGG).ok();
                std::fs::write(dir.join("nao-e-audio.mp3"), b"this is plain text, not an mp3 file at all").ok();
                std::fs::write(dir.join("longo.wav"), test_wav(6.0)).ok();
                if let Err(e) = app.add_sound(wav, "bipe", "🎺") {
                    return self.fail(&format!("could not start the upload: {e}"));
                }
                self.goto(3, "uploading the WAV");
            }
            3 => {
                if app.sb.uploaded_ok >= 1 && app.sb.sounds.len() == 1 {
                    let s = app.sb.sounds[0].clone();
                    log(&format!("WAV uploaded: {:?} {} ms, {} bytes, by {}", s.name, s.duration_ms, s.size, s.uploader.username));
                    // 0.9 s of tone plus the 10 ms pads, 44.1 kHz stereo (different channels) kept stereo.
                    if !(900..=950).contains(&s.duration_ms) || s.size < 150_000 || s.emoji != "🎺" {
                        return self.fail(&format!("unexpected stored sound: {s:?}"));
                    }
                    self.sound.ids.push(s.id);
                    let ogg = self.snd_dir().join("vorbis.ogg");
                    if let Err(e) = app.add_sound(ogg, "vorbis", "🥁") {
                        return self.fail(&format!("could not start the second upload: {e}"));
                    }
                    self.goto(4, "uploading the OGG");
                } else if let Some((false, e)) = Self::snd_message(app) {
                    return self.fail(&format!("upload failed: {e}"));
                } else if self.waited(40) {
                    return self.fail("the first upload never showed in the list");
                }
            }
            4 => {
                if app.sb.uploaded_ok >= 2 && app.sb.sounds.len() == 2 {
                    let ogg = app.sb.sounds.iter().find(|s| s.name == "vorbis").cloned();
                    let Some(ogg) = ogg else { return self.fail("the OGG sound is not in the list") };
                    log(&format!("OGG uploaded: {} ms, {} bytes", ogg.duration_ms, ogg.size));
                    if !(780..=900).contains(&ogg.duration_ms) {
                        return self.fail(&format!("unexpected OGG duration {}", ogg.duration_ms));
                    }
                    self.sound.ids.push(ogg.id);
                    app.sb.message = None;
                    let bad = self.snd_dir().join("nao-e-audio.mp3");
                    let _ = app.add_sound(bad, "ruim", "");
                    self.goto(5, "uploading a text file");
                } else if let Some((false, e)) = Self::snd_message(app) {
                    return self.fail(&format!("upload failed: {e}"));
                } else if self.waited(40) {
                    return self.fail("the second upload never showed in the list");
                }
            }
            5 => {
                if let Some((false, e)) = Self::snd_message(app) {
                    if !e.contains("Não consegui ler") {
                        return self.fail(&format!("unexpected refusal for a text file: {e}"));
                    }
                    log(&format!("text file refused: {e}"));
                    app.sb.message = None;
                    let long = self.snd_dir().join("longo.wav");
                    let _ = app.add_sound(long, "longo", "");
                    self.goto(6, "uploading a 6 s clip");
                } else if self.waited(20) {
                    return self.fail("the text file was not refused");
                }
            }
            6 => {
                if let Some((false, e)) = Self::snd_message(app) {
                    if !e.contains("máximo") {
                        return self.fail(&format!("unexpected refusal for a long clip: {e}"));
                    }
                    log(&format!("6 s clip refused: {e}"));
                    if app.sb.sounds.len() != 2 {
                        return self.fail("a refused upload changed the list");
                    }
                    if self.args.show.as_deref() == Some("panel") {
                        self.goto(20, "`--show panel`: uploading more sounds for a look at the panel");
                        return None;
                    }
                    self.snd_say(app, "a-ready");
                    self.goto(7, "sounds uploaded; waiting for B to see them");
                } else if self.waited(20) {
                    return self.fail("the long clip was not refused");
                }
            }
            7 => {
                if self.snd_saw(app, "b-listed") && self.snd_can_play() {
                    let id = self.sound.ids[0];
                    self.snd_play(app, id);
                    self.goto(8, "played the first sound");
                } else if self.waited(60) {
                    return self.fail("B never listed the sounds");
                }
            }
            8 => {
                let mine = app.sb.mixer_heard.iter().find(|h| h.0 == self.sound.ids[0]).cloned();
                let mine_ok = if headless() { mine.is_some() } else { app.sb.played.iter().any(|p| p.0 == self.sound.ids[0] && p.2 == Heard::Playing) };
                if mine_ok && self.snd_saw(app, "b-heard") {
                    if let Some(h) = mine {
                        log(&format!("my own mixer got it: rms {:.1} dBFS, peak {:.3}, {} ms", h.1, h.2, h.3));
                        if !RMS_RANGE.contains(&h.1) {
                            return self.fail(&format!("my mixer level {:.1} dBFS is not in {RMS_RANGE:?}", h.1));
                        }
                    }
                    self.snd_say(app, "a-deaf-ask");
                    self.goto(9, "asked B to deafen");
                } else if self.waited(40) {
                    return self.fail("the sound did not reach both mixers");
                }
            }
            9 => {
                let b_deaf = app.voice_members.values().any(|m| Some(m.user.id) != app.my_id() && m.deafened);
                if self.snd_saw(app, "b-deaf") && b_deaf && self.snd_can_play() {
                    let id = self.sound.ids[0];
                    self.snd_play(app, id);
                    self.goto(10, "played while B is deafened");
                } else if self.waited(40) {
                    return self.fail("B did not deafen");
                }
            }
            10 => {
                if self.snd_saw(app, "b-gated") {
                    self.snd_say(app, "a-unmute-ask");
                    self.goto(11, "B heard nothing while deafened; asked B to mute the soundboard");
                } else if self.waited(40) {
                    return self.fail("B did not confirm the deafen gate");
                }
            }
            11 => {
                if self.snd_saw(app, "b-muted-ready") && self.snd_can_play() {
                    let id = self.sound.ids[0];
                    self.snd_play(app, id);
                    self.goto(12, "played while B has the soundboard muted");
                } else if self.waited(40) {
                    return self.fail("B did not mute the soundboard");
                }
            }
            12 => {
                if self.snd_saw(app, "b-muted-ok") {
                    app.sb.message = None;
                    app.backend.send(Cmd::Sound(SoundCmd::Rename { id: self.sound.ids[0], name: "bipe novo".into(), emoji: "🎺".into() }));
                    self.goto(13, "renamed the first sound");
                } else if self.waited(40) {
                    return self.fail("B did not confirm the soundboard mute");
                }
            }
            13 => {
                if self.snd_saw(app, "b-forbidden") {
                    let id = self.sound.ids[1];
                    app.backend.send(Cmd::Sound(SoundCmd::Delete(id)));
                    self.goto(14, "B could not delete my sound; deleting the second sound");
                } else if self.waited(40) {
                    return self.fail("B's delete attempt was not reported");
                }
            }
            14 => {
                if self.snd_saw(app, "b-sees-delete") && app.sb.sounds.len() == 1 {
                    let name = app.sb.sounds[0].name.clone();
                    if name != "bipe novo" {
                        return self.fail(&format!("the remaining sound is {name:?}"));
                    }
                    self.goto(30, "ring and stop phase");
                } else if self.waited(40) {
                    return self.fail("B never saw the delete");
                }
            }
            30 => {
                if self.snd_saw(app, "b-ring-ready") && self.snd_can_play() {
                    let id = self.sound.ids[0];
                    self.snd_play(app, id);
                    self.sound.lit_at = None;
                    self.goto(31, "played; my ring must stay lit for the whole sound");
                } else if self.waited(40) {
                    return self.fail("B never got ready for the ring check");
                }
            }
            31 => {
                let lit = app.sending_voice();
                match (lit, self.sound.lit_at) {
                    (true, None) => self.sound.lit_at = Some(Instant::now()),
                    (false, Some(t)) => {
                        let ms = t.elapsed().as_millis();
                        log(&format!("my own ring was lit for {ms} ms (clip 0.9 s)"));
                        if !RING_MS.contains(&ms) {
                            return self.fail(&format!("my ring was lit for {ms} ms, expected {RING_MS:?}"));
                        }
                        self.goto(32, "own ring ok");
                    }
                    _ => {}
                }
                if self.waited(10) {
                    return self.fail(&format!("my ring did not follow my sound (lit at {:?})", self.sound.lit_at));
                }
            }
            32 => {
                if self.snd_saw(app, "b-ring-ok") && self.snd_can_play() {
                    self.sound.mix_before = app.sb.mixer_heard.len();
                    let id = self.sound.ids[0];
                    self.snd_play(app, id);
                    self.sound.lit_at = None;
                    self.goto(33, "played again; stopping it 300 ms in");
                } else if self.waited(40) {
                    return self.fail("B never confirmed my ring");
                }
            }
            33 => {
                if app.sending_voice() && self.step_at.elapsed() >= Duration::from_millis(300) {
                    app.stop_sounds();
                    self.sound.lit_at = Some(Instant::now());
                    self.goto(34, "pressed Parar");
                } else if self.waited(5) {
                    return self.fail("my ring did not light for the second play");
                }
            }
            34 => {
                let off = !app.sending_voice();
                let mine = app.sb.mixer_heard.get(self.sound.mix_before).copied();
                let mix_done = !headless() || mine.is_some();
                if off && mix_done && self.snd_saw(app, "b-stopped") {
                    if let Some(h) = mine {
                        log(&format!("my mixer played {} ms of the stopped 0.9 s clip", h.3));
                        if h.3 > STOPPED_MS {
                            return self.fail(&format!("the stopped clip played {} ms here", h.3));
                        }
                    }
                    if app.sb.playing_now().next().is_some() {
                        return self.fail("the panel still lists a playing sound after Parar");
                    }
                    app.sb.open = true;
                    self.goto(35, "panel open; a click inside keeps it, one beside it closes it");
                } else if self.waited(20) {
                    return self.fail(&format!("stop not confirmed: ring off {off}, my mix {mine:?}, B stopped {}", self.snd_saw(app, "b-stopped")));
                }
            }
            35 | 36 if self.step_at.elapsed() >= Duration::from_millis(500) => {
                let ctx = app.repaint_ctx();
                let Some(panel) = ctx.memory(|m| m.area_rect(egui::Id::new("soundboard_panel"))) else {
                    return self.fail("the panel is not on screen");
                };
                if self.step == 35 {
                    // On its title (no button there), then the chat just left of it.
                    click(&ctx, panel.left_top() + egui::vec2(30.0, 22.0));
                    self.goto(36, "clicked inside the panel");
                } else if !app.sb.open {
                    return self.fail("a click inside the panel closed it");
                } else {
                    click(&ctx, egui::pos2(panel.left() - 24.0, panel.center().y));
                    self.goto(37, "clicked beside the panel");
                }
            }
            37 => {
                if !app.sb.open {
                    log("a click beside the soundboard panel closed it; one inside did not");
                    self.snd_say(app, "a-done");
                    self.goto(15, "done");
                } else if self.waited(5) {
                    return self.fail("a click beside the soundboard panel did not close it");
                }
            }
            // `--show panel`: five sounds in the panel, open, with a play every few seconds (the
            // tile flashes); no B.
            20 => {
                const EXTRA: [(&str, &str); 3] = [("buzina", "📯"), ("aplausos", "👏"), ("risada", "😂")];
                let k = (app.sb.uploaded_ok as usize).saturating_sub(2);
                if k < EXTRA.len() && app.sb.sounds.len() == 2 + k && !app.sb.busy {
                    let ogg = self.snd_dir().join("vorbis.ogg");
                    let _ = app.add_sound(ogg, EXTRA[k].0, EXTRA[k].1);
                } else if k >= EXTRA.len() && app.sb.sounds.len() == 2 + EXTRA.len() {
                    app.sb.open = true;
                    self.goto(21, "panel open");
                }
                if self.waited(60) {
                    return self.fail("could not fill the panel");
                }
            }
            21 => {
                if self.waited(14) && !self.sound.add_shown {
                    self.sound.add_shown = true;
                    app.open_add_sound();
                    if let Some(f) = app.sb.add.as_mut() {
                        f.path = self.sound.dir.clone().unwrap_or_default().join("vorbis.ogg").display().to_string();
                        f.name = "novo som".into();
                        f.emoji = "🎉".into();
                    }
                }
                if self.snd_can_play() && self.waited(3) {
                    let id = app.sb.sounds[0].id;
                    self.snd_play(app, id);
                }
                if self.waited(60) {
                    return self.pass();
                }
            }
            15 if self.waited(2) => {
                if let Some(d) = self.sound.dir.take() {
                    let _ = std::fs::remove_dir_all(d);
                }
                return self.pass();
            }
            _ => {}
        }
        None
    }

    pub(super) fn run_sound_b(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => self.connect(app),
            1 => return self.snd_join(app),
            2 => {
                if app.voice.channel.is_none() {
                    if let Some(e) = app.voice.error.clone() {
                        return self.fail(&format!("voice join failed: {e}"));
                    }
                    return if self.waited(60) { self.fail("voice join took too long") } else { None };
                }
                if !app.sb.supported {
                    return if self.waited(20) { self.fail("the hub reports no soundboard") } else { None };
                }
                self.goto(3, "in voice; waiting for A's sounds");
            }
            3 => {
                if app.sb.sounds.len() == 2 && self.snd_saw(app, "a-ready") {
                    let names: Vec<_> = app.sb.sounds.iter().map(|s| s.name.clone()).collect();
                    log(&format!("B sees the sounds {names:?}"));
                    if !names.contains(&"bipe".to_string()) || !names.contains(&"vorbis".to_string()) {
                        return self.fail(&format!("unexpected sounds {names:?}"));
                    }
                    if app.sb.sounds.iter().any(|s| Some(s.uploader.id) == app.my_id()) {
                        return self.fail("the uploader should be A");
                    }
                    self.snd_say(app, "b-listed");
                    self.goto(4, "listed; waiting for A to play");
                } else if self.waited(90) {
                    return self.fail("A's sounds never showed");
                }
            }
            4 => {
                let probe = app.sb.mixer_heard.first().cloned();
                if let (Some(p), true) = (app.sb.played.first().cloned(), probe.is_some() || !headless()) {
                    log(&format!("heard {p:?}; mixer probe {probe:?}"));
                    if !matches!(p.2, Heard::Playing | Heard::Downloading) {
                        return self.fail(&format!("expected the sound to play, got {:?}", p.2));
                    }
                    if let Some(h) = probe.filter(|h| !RMS_RANGE.contains(&h.1)) {
                        return self.fail(&format!("the mixer level {:.1} dBFS is not in {RMS_RANGE:?}", h.1));
                    }
                    self.snd_say(app, "b-heard");
                    self.goto(5, "heard the first sound");
                } else if self.waited(60) {
                    return self.fail("the sound never reached my mixer");
                }
            }
            5 => {
                if self.snd_saw(app, "a-deaf-ask") {
                    app.toggle_deafen();
                    self.goto(6, "deafening");
                } else if self.waited(60) {
                    return self.fail("A never asked");
                }
            }
            6 => {
                if app.voice.deafened {
                    self.sound.plays_before = app.sb.played.len();
                    self.sound.mix_before = app.sb.mixer_heard.len();
                    self.snd_say(app, "b-deaf");
                    self.goto(7, "deafened; waiting for A's second play");
                } else if self.waited(20) {
                    return self.fail("could not deafen");
                }
            }
            7 => {
                if app.sb.played.len() > self.sound.plays_before {
                    let p = app.sb.played[self.sound.plays_before];
                    if p.2 != Heard::Deafened {
                        return self.fail(&format!("deafened, but the play was {:?}", p.2));
                    }
                    self.goto(8, "play seen while deafened; giving a wrongly queued sound time to reach the mixer");
                } else if self.waited(60) {
                    return self.fail("A's second play never arrived");
                }
            }
            8 if self.waited(2) => {
                if app.sb.mixer_heard.len() != self.sound.mix_before {
                    return self.fail("a sound reached the mixer while deafened");
                }
                log("deafened: the play was seen, nothing reached the mixer");
                self.snd_say(app, "b-gated");
                self.goto(9, "gate confirmed");
            }
            9 => {
                if self.snd_saw(app, "a-unmute-ask") {
                    app.toggle_deafen();
                    app.settings.soundboard.muted = true;
                    self.goto(10, "undeafening and muting the soundboard");
                } else if self.waited(60) {
                    return self.fail("A never asked to mute");
                }
            }
            10 => {
                if !app.voice.deafened {
                    self.sound.plays_before = app.sb.played.len();
                    self.sound.mix_before = app.sb.mixer_heard.len();
                    self.snd_say(app, "b-muted-ready");
                    self.goto(11, "waiting for A's third play");
                } else if self.waited(20) {
                    return self.fail("could not undeafen");
                }
            }
            11 => {
                if app.sb.played.len() > self.sound.plays_before {
                    let p = app.sb.played[self.sound.plays_before];
                    if p.2 != Heard::Muted {
                        return self.fail(&format!("soundboard muted, but the play was {:?}", p.2));
                    }
                    self.goto(12, "play seen while muted");
                } else if self.waited(60) {
                    return self.fail("A's third play never arrived");
                }
            }
            12 if self.waited(2) => {
                if app.sb.mixer_heard.len() != self.sound.mix_before {
                    return self.fail("a sound reached the mixer while the soundboard was muted");
                }
                log("soundboard muted: the play was seen, nothing reached the mixer");
                app.settings.soundboard.muted = false;
                self.snd_say(app, "b-muted-ok");
                self.goto(13, "mute confirmed; waiting for the rename");
            }
            13 => {
                if app.sb.sounds.iter().any(|s| s.name == "bipe novo") {
                    // Not mine: the delete must be refused.
                    let Some(other) = app.sb.sounds.iter().find(|s| s.name == "vorbis").map(|s| s.id) else {
                        return self.fail("the second sound is gone too early");
                    };
                    app.sb.message = None;
                    app.backend.send(Cmd::Sound(SoundCmd::Delete(other)));
                    self.goto(14, "saw the rename; trying to delete A's sound");
                } else if self.waited(60) {
                    return self.fail("the rename never arrived");
                }
            }
            14 => {
                if let Some((false, e)) = Self::snd_message(app) {
                    if !e.contains("Só quem enviou") {
                        return self.fail(&format!("unexpected refusal: {e}"));
                    }
                    log(&format!("delete of A's sound refused: {e}"));
                    if app.sb.sounds.len() != 2 {
                        return self.fail("the refused delete changed the list");
                    }
                    self.snd_say(app, "b-forbidden");
                    self.goto(15, "refused; waiting for A to delete it");
                } else if self.waited(20) {
                    return self.fail("the delete was not refused");
                }
            }
            15 => {
                if app.sb.sounds.len() == 1 {
                    self.snd_say(app, "b-sees-delete");
                    self.goto(30, "saw the delete; the ring and stop phase");
                } else if self.waited(60) {
                    return self.fail("A's delete never arrived");
                }
            }
            30 => {
                self.sound.lit_at = None;
                self.snd_say(app, "b-ring-ready");
                self.goto(31, "waiting for A's sound to light A's ring");
            }
            31 | 33 => {
                let a = app.sb.sounds.first().map(|s| s.uploader.id.to_string()).unwrap_or_default();
                let lit = app.voice.speaking.contains(&a);
                match (lit, self.sound.lit_at) {
                    (true, None) => {
                        self.sound.lit_at = Some(Instant::now());
                        // The measurement of the first play is long done: what comes next is this one's.
                        self.sound.mix_before = app.sb.mixer_heard.len();
                    }
                    (false, Some(t)) => {
                        let ms = t.elapsed().as_millis();
                        if self.step == 31 {
                            log(&format!("A's ring was lit here for {ms} ms (clip 0.9 s)"));
                            if !RING_MS.contains(&ms) {
                                return self.fail(&format!("A's ring was lit for {ms} ms, expected {RING_MS:?}"));
                            }
                            self.sound.lit_at = None;
                            self.snd_say(app, "b-ring-ok");
                            self.goto(33, "A's ring ok; waiting for the play A stops");
                        } else {
                            log(&format!("A's stopped sound: ring lit here for {ms} ms"));
                            if ms > STOPPED_MS as u128 {
                                return self.fail(&format!("A stopped the sound but the ring stayed {ms} ms"));
                            }
                            self.goto(34, "A's ring went off with the stop");
                        }
                    }
                    _ => {}
                }
                if self.waited(20) {
                    return self.fail(&format!("A's ring did not follow A's sound (step {}, lit at {:?})", self.step, self.sound.lit_at));
                }
            }
            34 => {
                // The measurement of the stopped clip (the first one's ended long ago).
                let last = app.sb.mixer_heard.last().copied().filter(|_| app.sb.mixer_heard.len() > self.sound.mix_before);
                if !headless() || last.is_some() {
                    if let Some(h) = last {
                        log(&format!("my mixer played {} ms of A's stopped 0.9 s clip", h.3));
                        if h.3 > STOPPED_MS {
                            return self.fail(&format!("A stopped the sound but {} ms of it played here", h.3));
                        }
                    }
                    self.snd_say(app, "b-stopped");
                    self.goto(16, "the stop reached me; waiting for A");
                } else if self.waited(20) {
                    return self.fail("the stopped sound was never measured");
                }
            }
            16 => {
                if self.snd_saw(app, "a-done") {
                    app.leave_voice();
                    self.goto(17, "A is done");
                } else if self.waited(60) {
                    return self.fail("A never finished");
                }
            }
            17 if self.waited(2) => return self.pass(),
            _ => {}
        }
        None
    }
}

/// A mono 16-bit WAV of `secs` seconds at `rate`, `f(t)` in -1..1.
fn mono_wav(rate: u32, secs: f32, f: impl Fn(f32) -> f32) -> Vec<u8> {
    let n = (secs * rate as f32) as usize;
    let mut data = Vec::with_capacity(n * 2);
    for i in 0..n {
        data.extend_from_slice(&((f(i as f32 / rate as f32).clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    let mut v = Vec::with_capacity(44 + data.len());
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * 2).to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(&data);
    v
}

impl Auto {
    pub(super) fn run_sound_trim(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => {
                if !headless() {
                    return self.fail("sound-trim previews its clip: run it with VOX_NO_PLAYBACK=1 (nothing may play on real speakers)");
                }
                self.connect(app);
            }
            1 => return self.snd_join(app),
            2 => {
                if app.voice.channel.is_none() {
                    if let Some(e) = app.voice.error.clone() {
                        return self.fail(&format!("voice join failed: {e}"));
                    }
                    return if self.waited(60) { self.fail("voice join took too long") } else { None };
                }
                if !app.sb.supported {
                    return if self.waited(20) { self.fail("the hub reports no soundboard") } else { None };
                }
                self.sound.base = app.sb.sounds.len();
                let dir = self.snd_dir();
                // 20 s: tone bursts at 1-5 s, 8-12 s (both quiet) and 15-19 s (loud).
                let burst = |t: f32| match t {
                    t if (1.0..5.0).contains(&t) || (8.0..12.0).contains(&t) => (t * 440.0 * std::f32::consts::TAU).sin() * 0.15,
                    t if (15.0..19.0).contains(&t) => (t * 523.0 * std::f32::consts::TAU).sin() * 0.6,
                    _ => 0.0,
                };
                std::fs::write(dir.join("rajadas.wav"), mono_wav(16_000, 20.0, burst)).ok();
                // 11 minutes (8 kHz mono, 10 MB): over the editor's 10 minute limit.
                std::fs::write(dir.join("longo.wav"), mono_wav(8_000, 660.0, |t| (t * 200.0 * std::f32::consts::TAU).sin() * 0.1)).ok();
                app.open_add_sound();
                app.trim_choose(dir.join("rajadas.wav"));
                self.goto(3, "reading the 20 s file in the add dialog");
            }
            3 => {
                let Some(ed) = app.trim_editor() else { return self.fail("the add dialog is gone") };
                if let Some(e) = ed.error.clone() {
                    return self.fail(&format!("the editor could not read the file: {e}"));
                }
                if ed.ready() {
                    let dur = ed.duration();
                    let default = ed.sel;
                    log(&format!("editor read the file: {dur:.2} s; default selection {:.2}..{:.2} s", default.start, default.end));
                    if !(19.9..=20.1).contains(&dur) {
                        return self.fail(&format!("unexpected duration {dur}"));
                    }
                    // The loud burst (15-19 s) must be inside the default 5 s.
                    if (default.len() - 5.0).abs() > 0.01 || default.start > 15.05 || default.end < 18.95 {
                        return self.fail(&format!("the default selection {default:?} does not hold the loudest burst"));
                    }
                    // However it is asked, the selection stays within 5 s and the file.
                    ed.set_selection(8.0, 20.0);
                    if ed.sel.len() > 5.0 + 1e-9 || ed.sel.end > dur {
                        return self.fail(&format!("the selection {:?} broke the limits", ed.sel));
                    }
                    ed.set_selection(8.5, 11.7);
                    ed.fade_in = true;
                    ed.fade_out = true;
                    log(&format!("selected {:.2}..{:.2} s", ed.sel.start, ed.sel.end));
                    app.trim_preview();
                    self.goto(4, "asked for the preview (silent)");
                } else if self.waited(40) {
                    return self.fail("the editor never finished reading the file");
                }
            }
            4 => {
                let Some(ed) = app.trim_editor() else { return self.fail("the add dialog is gone") };
                if let Some(e) = ed.preview_error.clone() {
                    return self.fail(&format!("preview failed: {e}"));
                }
                if ed.previews >= 1 {
                    let ms = ed.last_preview_ms;
                    log(&format!("preview clip is {ms} ms"));
                    if !(3_150..=3_250).contains(&ms) {
                        return self.fail(&format!("the previewed clip is {ms} ms, expected about 3200"));
                    }
                    if let Err(e) = app.trim_submit(&format!("trecho {}", self.args.run_id), "🎺") {
                        return self.fail(&format!("could not upload the selection: {e}"));
                    }
                    self.goto(5, "uploading the selection");
                } else if self.waited(30) {
                    return self.fail("no preview answer");
                }
            }
            5 => {
                if app.sb.uploaded_ok >= 1 && app.sb.sounds.len() == self.sound.base + 1 {
                    let Some(s) = app.sb.sounds.iter().filter(|s| s.name == format!("trecho {}", self.args.run_id)).max_by_key(|s| s.id).cloned() else { return self.fail("the uploaded sound is not in the list") };
                    log(&format!("selection uploaded: {:?} {} ms, {} bytes", s.name, s.duration_ms, s.size));
                    if !(3_100..=3_300).contains(&s.duration_ms) || s.size > 400_000 {
                        return self.fail(&format!("the hub stored {} ms / {} bytes, expected about 3.2 s", s.duration_ms, s.size));
                    }
                    if app.sb.add.is_some() {
                        return self.fail("the dialog should have closed after the upload");
                    }
                    app.open_add_sound();
                    app.trim_choose(self.snd_dir().join("longo.wav"));
                    self.goto(6, "choosing the 11 minute file");
                } else if let Some((false, e)) = Self::snd_message(app) {
                    return self.fail(&format!("upload failed: {e}"));
                } else if let Some(e) = app.sb.add.as_ref().and_then(|f| f.error.clone()) {
                    return self.fail(&format!("upload failed: {e}"));
                } else if self.waited(40) {
                    return self.fail("the selection never showed in the list");
                }
            }
            6 => {
                let Some(ed) = app.trim_editor() else { return self.fail("the add dialog is gone") };
                if let Some(e) = ed.error.clone() {
                    log(&format!("11 minute file refused: {e}"));
                    if !e.contains("10 minutos") {
                        return self.fail(&format!("unexpected refusal: {e}"));
                    }
                    if self.args.show.as_deref() == Some("trim") {
                        // A look at the editor: the short file again, then hold the window open.
                        let p = self.snd_dir().join("rajadas.wav");
                        app.trim_choose(p);
                        self.goto(7, "`--show trim`: the editor on the 20 s file");
                        return None;
                    }
                    if let Some(d) = self.sound.dir.take() {
                        let _ = std::fs::remove_dir_all(d);
                    }
                    return self.pass();
                } else if ed.ready() {
                    return self.fail("an 11 minute file was accepted");
                } else if self.waited(60) {
                    return self.fail("the 11 minute file was neither read nor refused");
                }
            }
            7 => {
                if let Some(ed) = app.trim_editor() {
                    if ed.ready() && !self.sound.add_shown {
                        self.sound.add_shown = true;
                        ed.set_selection(8.5, 11.7);
                        if let Some(f) = app.sb.add.as_mut() {
                            f.name = "trecho".into();
                        }
                    }
                }
                if self.waited(60) {
                    return self.pass();
                }
            }
            _ => {}
        }
        None
    }
}
