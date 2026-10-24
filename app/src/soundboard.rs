//! Server soundboard UI: the "Sons" panel of the voice bar, adding / renaming / deleting sounds,
//! the settings section, and the brief flash on the tile of whoever played a sound.
//!
//! v1.1: while a sound plays, the panel shows "Parar" (and the tile of a sound I am playing
//! shows a stop sign: clicking it again stops it). Stopping cuts every sound playing here, with a
//! short fade, and the ones I played for the whole room. A new sound while another plays is
//! mixed with it, as before.
//!
//! The network and audio side lives in `backend/sounds.rs`: it keeps the list, downloads and
//! caches the clips, and plays them through the voice session's mixer. This module only sends
//! [`SoundCmd`]s and reacts to [`SoundEv`]s. The feature is hidden on a hub that predates it
//! (`GET /api/sounds` answered 404, see [`SoundboardUi::supported`]).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, FontId, Margin, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use serde::{Deserialize, Serialize};
use vox_client_core::soundboard::Trim;
use vox_proto::{clean_sound_emoji, clean_sound_name, may_edit_sound, Sound, SoundId, User, UserId, SOUND_MAX_COUNT, SOUND_NAME_MAX};

use crate::app::App;
use crate::backend::Cmd;
use crate::forms;
use crate::soundtrim::{self, TrimEditor};
use crate::theme::{self, ph, radius, size, space};
use crate::ui_kit::{self, IconState, Kind, Size};

/// Shown on a sound that has no emoji.
pub(crate) const DEFAULT_EMOJI: &str = "🔊";
/// A few emojis to pick from when adding or renaming (any emoji can be typed or pasted too).
pub(crate) const QUICK_EMOJI: [&str; 20] = ["🎺", "🥁", "🎸", "😂", "🤣", "😎", "🔥", "💩", "👏", "😱", "🤔", "🙈", "🚨", "🔔", "💥", "👀", "🎉", "😭", "🐔", "🦆"];
/// The tile of whoever played a sound lights up for this long.
const FLASH_SECS: f32 = 2.5;
/// Clicking sounds faster than this does nothing (the hub would refuse it anyway).
const CLICK_GAP: Duration = Duration::from_millis(2000);
/// Width of the soundboard popup's content.
const PANEL_W: f32 = 404.0;
/// The sound buttons of the popup: at least this wide, this far apart, this tall.
const TILE_MIN_W: f32 = 120.0;
const TILE_GAP: f32 = 6.0;
const TILE_H: f32 = 40.0;

// ---------------------------------------------------------------- settings

/// "Soundboard" in Configurações. Saved in settings.json (older files get the defaults).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundboardSettings {
    /// 0..=100, default 50.
    pub volume_pct: u32,
    /// "Silenciar soundboard": nobody's sounds are heard (the panel still works).
    pub muted: bool,
    /// Sounds silenced one by one, per server (ids are per server).
    pub muted_sounds: BTreeMap<String, Vec<SoundId>>,
}

impl Default for SoundboardSettings {
    fn default() -> Self {
        Self { volume_pct: 50, muted: false, muted_sounds: BTreeMap::new() }
    }
}

// ---------------------------------------------------------------- UI <-> backend

#[derive(Debug, Clone)]
pub enum SoundCmd {
    /// What the backend needs to decide whether and how loud to play (sent whenever it changes).
    Prefs { volume_pct: u32, muted: bool, muted_sounds: Vec<SoundId> },
    /// Ask the hub to play a sound for the room (it also starts here at once).
    Play(SoundId),
    /// "Parar": stop every sound playing here, and mine for the whole room.
    Stop,
    /// Hear a sound only here.
    Preview(SoundId),
    /// Decode, trim, normalise and upload an audio file. `trim`: only that part of the file (the
    /// trim editor's selection); `None`: the whole file, which must fit the 5 s limit.
    Upload { path: PathBuf, name: String, emoji: String, trim: Option<Trim> },
    /// "Ouvir trecho": cut the clip the way an upload would and play it here only.
    PreviewClip { path: PathBuf, trim: Trim, output_device: Option<String> },
    Rename { id: SoundId, name: String, emoji: String },
    Delete(SoundId),
}

/// What the backend did with a `SoundPlayed` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heard {
    /// Queued in the playback mixer.
    Playing,
    /// Not downloaded yet: it starts as soon as it arrives.
    Downloading,
    /// "Silenciar soundboard".
    Muted,
    /// This one sound is silenced.
    SoundMuted,
    /// Deafened: sounds are not played.
    Deafened,
    /// No output device (or too many sounds at once).
    NoOutput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundOp {
    Upload,
    Rename,
    Delete,
}

#[derive(Debug)]
pub enum SoundEv {
    /// The whole soundboard state after a login, a reconnect or a change. `supported: false` is a
    /// hub without a soundboard.
    List { supported: bool, sounds: Vec<Sound> },
    Played { sound_id: SoundId, by: User, heard: Heard },
    /// Sounds stopped: those `by` played (`None`: everything playing here).
    Stopped { by: Option<UserId> },
    /// An upload / rename / delete finished; the error is Portuguese, ready to show.
    Done { op: SoundOp, result: Result<(), String> },
    /// "Ouvir trecho" started (its length in ms) or could not (Portuguese text). In headless runs
    /// nothing is played and this is the only sign of it.
    ClipPreview(Result<u32, String>),
    /// Headless runs (`VOX_NO_PLAYBACK`): what the playback mixer produced for a sound.
    MixerHeard { sound_id: SoundId, rms_dbfs: f32, peak: f32, ms: u32 },
}

/// Is this hub error the refusal of a `PlaySound`?
pub fn is_play_refusal(error: &str) -> bool {
    use vox_proto::*;
    [ERR_SOUND_RATE_LIMITED, ERR_SOUND_NOT_IN_VOICE, ERR_SOUND_UNKNOWN, ERR_SOUND_SERVER_MUTED].contains(&error)
}

/// A hub error text for a sound action, in Portuguese (anything else keeps the generic form).
pub fn server_error_text(error: &str) -> String {
    use vox_proto::*;
    match error {
        ERR_SOUND_RATE_LIMITED => "Calma: espere um instante para tocar outro som.".into(),
        ERR_SOUND_NOT_IN_VOICE => "Entre em um canal de voz para tocar sons.".into(),
        ERR_SOUND_UNKNOWN => "Esse som não existe mais.".into(),
        ERR_SOUND_SERVER_MUTED => "Você está silenciado pelo servidor: os sons também ficam desligados.".into(),
        other => format!("Servidor: {other}"),
    }
}

// ---------------------------------------------------------------- state

pub struct Flash {
    at: Instant,
    emoji: String,
    name: String,
}

/// The "Adicionar som" dialog.
pub struct AddForm {
    pub path: String,
    pub name: String,
    pub emoji: String,
    pub error: Option<String>,
    /// The file dialog is open on its own thread.
    picking: Option<std_mpsc::Receiver<Option<PathBuf>>>,
    /// The name was typed by the person, so a newly chosen file does not overwrite it.
    name_edited: bool,
    /// The trim editor of the chosen file (reads it in the background, then shows the waveform).
    pub trim: Option<TrimEditor>,
    /// The path the editor was started for, and whether a new one is waiting to be started.
    pub analyzed: String,
    pub want_analyze: bool,
}

pub struct EditForm {
    pub id: SoundId,
    pub name: String,
    pub emoji: String,
}

#[derive(Default)]
pub struct SoundboardUi {
    /// The hub has a soundboard (false: the whole feature is hidden).
    pub supported: bool,
    /// By name.
    pub sounds: Vec<Sound>,
    pub open: bool,
    /// The frame the popup was first drawn on, this time it is open (the click that opened it
    /// does not close it again).
    opened_frame: Option<u64>,
    /// The popup's search (cleared when it closes).
    pub search: String,
    pub add: Option<AddForm>,
    pub edit: Option<EditForm>,
    pub confirm_delete: Option<(SoundId, String)>,
    /// An upload / rename / delete is in flight.
    pub busy: bool,
    pub message: Option<(bool, String)>,
    flashes: HashMap<UserId, Flash>,
    /// Sounds playing now: (sound, player, until when).
    playing: Vec<(SoundId, UserId, Instant)>,
    last_click: Option<Instant>,
    prefs_sent: Option<(u32, bool, Vec<SoundId>)>,
    /// Probes for the autotest.
    pub played: Vec<(SoundId, UserId, Heard)>,
    pub mixer_heard: Vec<(SoundId, f32, f32, u32)>,
    pub uploaded_ok: u32,
}

impl SoundboardUi {
    pub fn find(&self, id: SoundId) -> Option<&Sound> {
        self.sounds.iter().find(|s| s.id == id)
    }

    /// Sounds playing right now (by anyone).
    pub fn playing_now(&self) -> impl Iterator<Item = (SoundId, UserId)> + '_ {
        let now = Instant::now();
        self.playing.iter().filter(move |p| p.2 > now).map(|p| (p.0, p.1))
    }

    /// Is `id` playing, played by `by`?
    pub fn is_playing(&self, id: SoundId, by: Option<UserId>) -> bool {
        self.playing_now().any(|(s, u)| s == id && by.is_none_or(|b| b == u))
    }

    /// Keep the list of what plays from the backend's events.
    fn track_playing(&mut self, ev: &SoundEv) {
        let now = Instant::now();
        self.playing.retain(|p| p.2 > now);
        match ev {
            SoundEv::Played { sound_id, by, heard: Heard::Playing | Heard::Downloading } => {
                let ms = self.find(*sound_id).map_or(0, |s| s.duration_ms);
                self.playing.retain(|p| !(p.0 == *sound_id && p.1 == by.id));
                self.playing.push((*sound_id, by.id, now + Duration::from_millis(ms as u64)));
            }
            SoundEv::Stopped { by: Some(by) } => self.playing.retain(|p| p.1 != *by),
            SoundEv::Stopped { by: None } => self.playing.clear(),
            _ => {}
        }
    }
}

fn dur_text(ms: u32) -> String {
    format!("{:.1} s", ms as f32 / 1000.0)
}

impl App {
    /// The settings key of the server we are on (sound ids are per server).
    fn sound_server_key(&self) -> String {
        crate::backend::normalize_url(&self.form.url).to_lowercase()
    }

    fn sound_is_muted(&self, id: SoundId) -> bool {
        self.settings.soundboard.muted_sounds.get(&self.sound_server_key()).is_some_and(|l| l.contains(&id))
    }

    fn set_sound_muted(&mut self, id: SoundId, muted: bool) {
        let key = self.sound_server_key();
        let list = self.settings.soundboard.muted_sounds.entry(key.clone()).or_default();
        list.retain(|s| *s != id);
        if muted {
            list.push(id);
        }
        if list.is_empty() {
            self.settings.soundboard.muted_sounds.remove(&key);
        }
        self.save_settings();
    }

    /// Tell the backend the soundboard settings when they changed. Cheap: called every frame.
    pub fn sound_tick(&mut self) {
        let mut muted_list = self.settings.soundboard.muted_sounds.get(&self.sound_server_key()).cloned().unwrap_or_default();
        muted_list.sort_unstable();
        let now = (self.settings.soundboard.volume_pct.min(100), self.settings.soundboard.muted, muted_list);
        if self.sb.prefs_sent.as_ref() != Some(&now) {
            self.backend.send(Cmd::Sound(SoundCmd::Prefs { volume_pct: now.0, muted: now.1, muted_sounds: now.2.clone() }));
            self.sb.prefs_sent = Some(now);
        }
    }

    pub fn reset_soundboard(&mut self) {
        self.sb = SoundboardUi::default();
    }

    pub fn on_sound_event(&mut self, ev: SoundEv) {
        self.sb.track_playing(&ev);
        match ev {
            SoundEv::List { supported, mut sounds } => {
                sounds.sort_by_key(|s| (s.name.to_lowercase(), s.id));
                tracing::info!(supported, sounds = sounds.len(), "soundboard list");
                self.sb.supported = supported;
                self.sb.sounds = sounds;
                if !supported {
                    self.sb.open = false;
                }
            }
            SoundEv::Played { sound_id, by, heard } => {
                tracing::info!(sound_id, by = %by.username, ?heard, "sound played in my room");
                self.sb.played.push((sound_id, by.id, heard));
                let found = self.sb.find(sound_id).map(|s| (s.emoji.clone(), s.name.clone()));
                let (emoji, name) = match (found, self.repaint.ctx()) {
                    (Some((e, n)), Some(ctx)) => (shown_emoji(&ctx, &e), n),
                    (Some((e, n)), None) => (if e.is_empty() { DEFAULT_EMOJI.to_string() } else { e }, n),
                    (None, _) => (DEFAULT_EMOJI.to_string(), String::new()),
                };
                self.sb.flashes.insert(by.id, Flash { at: Instant::now(), emoji, name });
                self.repaint.wake();
            }
            SoundEv::Stopped { by } => {
                tracing::info!(?by, "soundboard: sounds stopped");
                self.repaint.wake();
            }
            SoundEv::Done { op, result } => {
                self.sb.busy = false;
                match result {
                    Ok(()) => {
                        if op == SoundOp::Upload {
                            self.sb.uploaded_ok += 1;
                            self.sb.add = None;
                            self.sb.message = Some((true, "Som adicionado.".into()));
                        } else {
                            self.sb.edit = None;
                            self.sb.message = None;
                        }
                    }
                    Err(e) => {
                        if op == SoundOp::Upload {
                            if let Some(add) = self.sb.add.as_mut() {
                                add.error = Some(e);
                                return;
                            }
                        }
                        self.sb.message = Some((false, e));
                    }
                }
            }
            SoundEv::ClipPreview(r) => {
                tracing::info!(target: "autotest", "AUTOTEST soundboard: clip preview {r:?}");
                if let Some(ed) = self.trim_editor() {
                    ed.preview_result(r);
                }
            }
            SoundEv::MixerHeard { sound_id, rms_dbfs, peak, ms } => {
                tracing::info!(target: "autotest", "AUTOTEST soundboard: mixer got sound {sound_id}: rms {rms_dbfs:.1} dBFS, peak {peak:.3}, {ms} ms");
                self.sb.mixer_heard.push((sound_id, rms_dbfs, peak, ms));
            }
        }
    }

    // ------------------------------------------------------------ actions

    /// A click on a sound: ask the hub to play it for the room; a click on the sound I am
    /// playing stops it.
    pub fn play_sound(&mut self, id: SoundId) {
        if self.voice.channel.is_none() {
            return;
        }
        if self.my_id().is_some_and(|me| self.sb.is_playing(id, Some(me))) {
            self.stop_sounds();
            return;
        }
        if self.sb.last_click.is_some_and(|t| t.elapsed() < CLICK_GAP) {
            self.sb.message = Some((false, "Calma: espere um instante para tocar outro som.".into()));
            return;
        }
        self.sb.last_click = Some(Instant::now());
        self.sb.message = None;
        self.backend.send(Cmd::Sound(SoundCmd::Play(id)));
    }

    /// "Parar": every sound playing here stops, and mine for the whole room.
    pub fn stop_sounds(&mut self) {
        self.sb.message = None;
        self.backend.send(Cmd::Sound(SoundCmd::Stop));
    }

    pub fn preview_sound(&mut self, id: SoundId) {
        self.backend.send(Cmd::Sound(SoundCmd::Preview(id)));
    }

    /// Upload the whole file `path` as `name` (it must fit the 5 s limit once its silence is cut).
    pub fn add_sound(&mut self, path: PathBuf, name: &str, emoji: &str) -> Result<(), String> {
        self.add_sound_trimmed(path, name, emoji, None)
    }

    /// The dialog's "Adicionar": upload `path` (or just the `trim` part of it) as `name`.
    pub fn add_sound_trimmed(&mut self, path: PathBuf, name: &str, emoji: &str, trim: Option<Trim>) -> Result<(), String> {
        let name = clean_sound_name(name).ok_or_else(|| format!("O nome precisa ter de 1 a {SOUND_NAME_MAX} letras."))?;
        let emoji = clean_sound_emoji(emoji).ok_or("O emoji é inválido (só um emoji, sem espaços).")?;
        if self.sb.sounds.len() >= SOUND_MAX_COUNT {
            return Err(format!("O soundboard está cheio ({SOUND_MAX_COUNT} sons). Apague algum antes."));
        }
        self.sb.busy = true;
        self.backend.send(Cmd::Sound(SoundCmd::Upload { path, name, emoji, trim }));
        Ok(())
    }

    pub fn open_add_sound(&mut self) {
        self.sb.add = Some(AddForm {
            path: String::new(),
            name: String::new(),
            emoji: String::new(),
            error: None,
            picking: None,
            name_edited: false,
            trim: None,
            analyzed: String::new(),
            want_analyze: false,
        });
    }

    fn can_edit_sound(&self, s: &Sound) -> bool {
        self.my_id().is_some_and(|me| may_edit_sound(self.my_role(), me, s.uploader.id))
    }

    // ------------------------------------------------------------ windows

    pub fn soundboard_windows(&mut self, ctx: &egui::Context) {
        if self.voice.channel.is_none() || !self.sb.supported {
            self.sb.open = false;
        }
        self.soundboard_panel(ctx);
        self.add_sound_window(ctx);
        self.edit_sound_window(ctx);
        self.delete_sound_window(ctx);
    }

    /// The soundboard popup above the voice dock: a search, the grid of sounds, the volume and
    /// "Adicionar".
    fn soundboard_panel(&mut self, ctx: &egui::Context) {
        if !self.sb.open {
            self.sb.opened_frame = None;
            return;
        }
        let opened = *self.sb.opened_frame.get_or_insert(ctx.cumulative_frame_nr());
        let mut close = false;
        let mut prefs_changed = false;
        let mut play = None;
        let mut preview = None;
        let mut mute_toggle: Option<(SoundId, bool)> = None;
        let mut edit = None;
        let mut delete = None;
        let mut want_add = false;
        let mut stop = false;
        let me = self.my_id();
        let any_playing = self.sb.playing_now().next().is_some();
        let screen = ctx.content_rect();
        // In a small window at most half its width (and a shorter grid, below): the chat beside
        // it stays usable.
        let w = PANEL_W.min((screen.width() * 0.5).max(300.0)).min(screen.width() - 32.0);
        let style = ctx.global_style();
        let area = egui::Area::new(egui::Id::new("soundboard_panel"))
            .order(egui::Order::Middle)
            .anchor(Align2::RIGHT_BOTTOM, [-16.0, -72.0])
            .constrain(true)
            .show(ctx, |ui| {
                // A popup (the shadow), on the wall colour so the sound tiles stand out in theirs.
                egui::Frame::new()
                    .fill(theme::PAREDE)
                    .stroke(Stroke::new(theme::stroke::HAIRLINE, theme::RISCO))
                    .corner_radius(radius::POPUP)
                    .inner_margin(Margin::same(12))
                    .shadow(style.visuals.popup_shadow)
                    .show(ui, |ui| {
                    ui.set_width(w);
                    ui.spacing_mut().item_spacing.y = 8.0;
                    // Header: the title, how many sounds, the ✕.
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Sons").font(theme::display(size::TITLE)).color(theme::GIZ));
                        ui.label(RichText::new(format!("{}/{SOUND_MAX_COUNT}", self.sb.sounds.len())).font(theme::body(size::SMALL)).color(theme::NEVOA));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui_kit::icon_button(ui, ph::X, "Fechar", IconState::Normal).clicked() {
                                close = true;
                            }
                            if any_playing
                                && ui_kit::button_sized(ui, Kind::Secondary, Size::Small, Some(ph::STOP), "Parar")
                                    .on_hover_text("Para os sons que estão tocando (os seus param para todos na sala)")
                                    .clicked()
                            {
                                stop = true;
                            }
                        });
                    });
                    if !self.sb.sounds.is_empty() {
                        ui.add(ui_kit::field(&mut self.sb.search).hint_text(format!("{}  Buscar som", ph::MAGNIFYING_GLASS)).desired_width(f32::INFINITY));
                    }
                    if let Some((ok, text)) = self.sb.message.clone() {
                        let (icon, color) = if ok { (ph::CHECK, theme::ONLINE) } else { (ph::WARNING, theme::AO_VIVO) };
                        forms::note_colored(ui, format!("{icon} {text}"), false, color);
                    }
                    if self.sb.sounds.is_empty() {
                        ui.add(
                            egui::Label::new(
                                RichText::new("Nenhum som ainda. Adicione o primeiro: um arquivo WAV, MP3, OGG, OPUS, M4A, WEBM ou FLAC (até 10 minutos); você escolhe o trecho de até 5 segundos.")
                                    .color(theme::NEVOA),
                            )
                            .wrap(),
                        );
                    }
                    let query = self.sb.search.trim().to_lowercase();
                    let shown: Vec<usize> = (0..self.sb.sounds.len()).filter(|&i| query.is_empty() || self.sb.sounds[i].name.to_lowercase().contains(&query)).collect();
                    if shown.is_empty() && !self.sb.sounds.is_empty() {
                        ui.label(RichText::new("Nenhum som com esse nome.").color(theme::NEVOA));
                    }
                    egui::ScrollArea::vertical().id_salt("soundboard_grid").auto_shrink([false, true]).max_height((screen.height() - 300.0).clamp(96.0, 330.0)).show(ui, |ui| {
                        let ctx = ui.ctx().clone();
                        let (cols, tile_w) = crate::sources::grid_layout(ui.available_width(), TILE_MIN_W, TILE_GAP);
                        ui.spacing_mut().item_spacing = Vec2::splat(TILE_GAP);
                        for row in shown.chunks(cols) {
                            ui.horizontal(|ui| {
                                for &i in row {
                                    let s = &self.sb.sounds[i];
                                    let muted_here = self.sound_is_muted(s.id);
                                    let can_edit = self.can_edit_sound(s);
                                    let emoji = shown_emoji(&ctx, &s.emoji);
                                    let mine_playing = me.is_some_and(|me| self.sb.is_playing(s.id, Some(me)));
                                    let (tile, listen) = sound_tile(ui, tile_w, &emoji, &s.name, muted_here, mine_playing);
                                    let tile = tile.on_hover_text(format!(
                                        "{}\npor {} · {}{}\n{}",
                                        s.name,
                                        s.uploader.shown_name(),
                                        dur_text(s.duration_ms),
                                        if muted_here { "\n(silenciado só para você)" } else { "" },
                                        if mine_playing { "Tocando: clique para parar" } else { "Clique: toca para a sala · botão direito: mais opções" }
                                    ));
                                    if listen {
                                        preview = Some(s.id);
                                    } else if tile.clicked() {
                                        play = Some(s.id);
                                    }
                                    tile.context_menu(|ui| {
                                        ui.set_min_width(220.0);
                                        ui.label(RichText::new(&s.name).font(theme::semibold(size::BUTTON)).color(theme::GIZ));
                                        if ui_kit::menu_row(ui, ph::HEADPHONES, "Ouvir só aqui", false).clicked() {
                                            preview = Some(s.id);
                                            ui.close();
                                        }
                                        let (icon, text) = if muted_here { (ph::SPEAKER_HIGH, "Reativar este som") } else { (ph::SPEAKER_SLASH, "Silenciar este som (só para mim)") };
                                        if ui_kit::menu_row(ui, icon, text, false).clicked() {
                                            mute_toggle = Some((s.id, !muted_here));
                                            ui.close();
                                        }
                                        if can_edit {
                                            ui.separator();
                                            if ui_kit::menu_row(ui, ph::PENCIL_SIMPLE, "Renomear…", false).clicked() {
                                                edit = Some(s.clone());
                                                ui.close();
                                            }
                                            if ui_kit::menu_row(ui, ph::TRASH, "Apagar", true).clicked() {
                                                delete = Some((s.id, s.name.clone()));
                                                ui.close();
                                            }
                                        }
                                    });
                                }
                            });
                        }
                    });
                    // Footer: my volume for everybody's sounds, and "Adicionar".
                    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), egui::Sense::hover());
                    ui.painter().hline(r.x_range(), r.center().y, Stroke::new(theme::stroke::HAIRLINE, theme::RISCO));
                    ui.horizontal(|ui| {
                        let muted = self.settings.soundboard.muted;
                        let (icon, hover, state) = if muted {
                            (ph::SPEAKER_SLASH, "Ouvir o soundboard de novo", IconState::Off)
                        } else {
                            (ph::SPEAKER_HIGH, "Silenciar soundboard: só você deixa de ouvir os sons; os outros continuam ouvindo", IconState::Normal)
                        };
                        if ui_kit::icon_button(ui, icon, hover, state).clicked() {
                            self.settings.soundboard.muted = !muted;
                            prefs_changed = true;
                        }
                        // (I6: narrower in a narrow panel, so "Adicionar" keeps its room.)
                        ui.spacing_mut().slider_width = (ui.available_width() - 190.0).clamp(48.0, 110.0);
                        ui.add_enabled_ui(!muted, |ui| {
                            prefs_changed |= ui.add(egui::Slider::new(&mut self.settings.soundboard.volume_pct, 0..=100).suffix("%")).on_hover_text("Volume do soundboard (só para você)").changed();
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let full = self.sb.sounds.len() >= SOUND_MAX_COUNT;
                            let r = ui.add_enabled_ui(!full && !self.sb.busy, |ui| ui_kit::button_sized(ui, Kind::Secondary, Size::Small, Some(ph::UPLOAD_SIMPLE), "Adicionar")).inner;
                            if r.on_disabled_hover_text(if full { "O soundboard está cheio: apague algum som antes" } else { "Enviando..." }).clicked() {
                                want_add = true;
                            }
                        });
                    });
                });
            });
        // Esc closes it (not while a dialog of its own, or a menu, is open over it).
        let dialogs = self.sb.add.is_some() || self.sb.edit.is_some() || self.sb.confirm_delete.is_some();
        if !dialogs && ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !egui::Popup::is_any_open(ctx) {
            close = true;
        }
        // So does a click anywhere else (the chat under it, the channel list), like the reaction
        // picker: it is a layer over the chat that also takes the mouse wheel over its whole rect.
        let click = ctx.input(|i| i.pointer.any_click()).then(|| ctx.pointer_interact_pos()).flatten();
        if let Some(at) = click {
            let layer = ctx.layer_id_at(at).map(|l| l.order);
            close |= closes_panel(at, area.response.rect, layer, ctx.cumulative_frame_nr() > opened, dialogs);
        }
        if want_add {
            self.open_add_sound();
        }
        if stop {
            self.stop_sounds();
        }
        // "Parar" and the tiles' stop sign go away when the sounds end.
        let now = Instant::now();
        if let Some(next) = self.sb.playing.iter().map(|p| p.2).filter(|t| *t > now).min() {
            ctx.request_repaint_after(next - now + Duration::from_millis(20));
        }
        if close {
            self.sb.open = false;
            self.sb.search.clear();
        }
        if prefs_changed {
            self.save_settings();
        }
        if let Some(id) = play {
            self.play_sound(id);
        }
        if let Some(id) = preview {
            self.preview_sound(id);
        }
        if let Some((id, m)) = mute_toggle {
            self.set_sound_muted(id, m);
        }
        if let Some(s) = edit {
            self.sb.edit = Some(EditForm { id: s.id, name: s.name, emoji: s.emoji });
        }
        if let Some(d) = delete {
            self.sb.confirm_delete = Some(d);
        }
    }

    fn add_sound_window(&mut self, ctx: &egui::Context) {
        let Some(mut form) = self.sb.add.take() else { return };
        let mut submit = false;
        let mut cancel = false;
        let mut pick = false;
        let mut preview = false;
        let busy = self.sb.busy;
        // A file dropped on the window.
        let dropped = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).next_back());
        if let Some(p) = dropped {
            set_path(&mut form, p);
        }
        if let Some(rx) = &form.picking {
            match rx.try_recv() {
                Ok(Some(p)) => {
                    set_path(&mut form, p);
                    form.picking = None;
                }
                Ok(None) | Err(std_mpsc::TryRecvError::Disconnected) => form.picking = None,
                Err(std_mpsc::TryRecvError::Empty) => {}
            }
        }
        // A new file: start reading it (the previous editor stops when dropped).
        if form.want_analyze {
            form.want_analyze = false;
            form.analyzed = form.path.clone();
            form.trim = Some(TrimEditor::start(PathBuf::from(form.path.trim().trim_matches('"')), &self.repaint));
        }
        if let Some(ed) = form.trim.as_mut() {
            ed.poll();
        }
        // Esc stays with the text fields (the ✕ and "Cancelar" close it).
        let mut dialog = forms::Dialog::new("sound_add", "Adicionar som").icon(ph::UPLOAD_SIMPLE).width(620.0);
        dialog.esc_closes = false;
        let open = dialog.show(ctx, |ui| {
            forms::note(ui, "Escolha um arquivo de áudio (WAV, MP3, OGG, OPUS, M4A, WEBM ou FLAC) de até 10 minutos. Você marca o trecho de até 5 segundos que vira o som; o app ajusta o volume e envia.", false);
            ui.add_space(space::S);
            ui.horizontal(|ui| {
                let field_w = ui.available_width() - 150.0;
                let r = ui.add_enabled(!busy, ui_kit::field(&mut form.path).hint_text("Caminho do arquivo (ou arraste o arquivo para cá)").desired_width(field_w));
                // A typed path is read when the box is left, not on every keystroke.
                if r.lost_focus() && !form.path.trim().is_empty() && form.path != form.analyzed {
                    form.want_analyze = true;
                }
                if ui.add_enabled_ui(!busy && form.picking.is_none(), |ui| ui_kit::button(ui, Kind::Secondary, Some(ph::FOLDER_OPEN), "Escolher…")).inner.clicked() {
                    pick = true;
                }
            });
            if let Some(ed) = form.trim.as_mut() {
                ui.add_space(space::S);
                if ed.loading() {
                    soundtrim::loading_ui(ui, ed);
                } else if let Some(e) = ed.error.clone() {
                    forms::note_colored(ui, format!("{} {e}", ph::WARNING), false, theme::AO_VIVO);
                } else if ed.ready() && ed.ui(ui, !busy) {
                    preview = true;
                }
            }
            forms::divider(ui);
            forms::row(ui, "Nome e emoji", |ui| {
                if ui.add_enabled(!busy, ui_kit::field(&mut form.name).char_limit(SOUND_NAME_MAX).desired_width(240.0)).changed() {
                    form.name_edited = true;
                }
                ui.add_enabled(!busy, ui_kit::field(&mut form.emoji).char_limit(vox_proto::SOUND_EMOJI_MAX_CHARS).hint_text("emoji").desired_width(64.0));
            });
            forms::indented(ui, |ui| emoji_row(ui, &mut form.emoji, busy));
            if let Some(e) = &form.error {
                forms::note_colored(ui, format!("{} {e}", ph::WARNING), false, theme::AO_VIVO);
            }
            forms::footer(ui, |ui| {
                let ready = form.trim.as_ref().is_some_and(|t| t.ready()) && !form.name.trim().is_empty();
                let r = ui.add_enabled_ui(ready && !busy, |ui| ui_kit::button(ui, Kind::Primary, Some(ph::UPLOAD_SIMPLE), "Adicionar")).inner;
                if r.on_disabled_hover_text(if busy { "Enviando..." } else { "Escolha um arquivo e um nome" }).clicked() {
                    submit = true;
                }
                if ui_kit::button(ui, Kind::Ghost, None, "Cancelar").clicked() {
                    cancel = true;
                }
                if busy {
                    ui.spinner();
                    ui.label(RichText::new("Processando e enviando...").color(theme::NEVOA));
                }
            });
        });
        let open = open && !cancel;
        if pick {
            let (tx, rx) = std_mpsc::channel();
            let repaint = self.repaint.clone();
            let owner = self.video.parent;
            std::thread::spawn(move || {
                let _ = tx.send(open_file_dialog(owner));
                repaint.wake();
            });
            form.picking = Some(rx);
        }
        if submit {
            form.error = None;
            self.sb.add = Some(form);
            let (name, emoji) = self.sb.add.as_ref().map(|f| (f.name.clone(), f.emoji.clone())).unwrap_or_default();
            let r = self.trim_submit(&name, &emoji);
            if let (Err(e), Some(f)) = (r, self.sb.add.as_mut()) {
                f.error = Some(e);
            }
            if !open {
                self.sb.add = None;
            }
            return;
        }
        if open {
            self.sb.add = Some(form);
            if preview {
                self.trim_preview();
            }
        }
    }

    fn edit_sound_window(&mut self, ctx: &egui::Context) {
        let Some(mut form) = self.sb.edit.take() else { return };
        let mut save = false;
        let mut cancel = false;
        let busy = self.sb.busy;
        let open = forms::Dialog::new("sound_edit", "Renomear som").icon(ph::PENCIL_SIMPLE).width(460.0).show(ctx, |ui| {
            forms::row(ui, "Nome", |ui| {
                ui.add(ui_kit::field(&mut form.name).char_limit(SOUND_NAME_MAX).desired_width(220.0));
            });
            forms::row(ui, "Emoji", |ui| {
                ui.add(ui_kit::field(&mut form.emoji).char_limit(vox_proto::SOUND_EMOJI_MAX_CHARS).desired_width(56.0));
            });
            forms::indented(ui, |ui| emoji_row(ui, &mut form.emoji, false));
            if clean_sound_name(&form.name).is_none() {
                forms::note_colored(ui, format!("O nome precisa ter de 1 a {SOUND_NAME_MAX} letras."), true, theme::AVISO);
            }
            forms::footer(ui, |ui| {
                let ok = clean_sound_name(&form.name).is_some() && clean_sound_emoji(&form.emoji).is_some() && !busy;
                if ui.add_enabled_ui(ok, |ui| ui_kit::button(ui, Kind::Primary, None, "Salvar")).inner.clicked() {
                    save = true;
                }
                if ui_kit::button(ui, Kind::Ghost, None, "Cancelar").clicked() {
                    cancel = true;
                }
            });
        });
        if save {
            self.sb.busy = true;
            self.backend.send(Cmd::Sound(SoundCmd::Rename { id: form.id, name: form.name.trim().to_string(), emoji: form.emoji.trim().to_string() }));
        }
        if open && !cancel {
            self.sb.edit = Some(form);
        }
    }

    fn delete_sound_window(&mut self, ctx: &egui::Context) {
        let Some((id, name)) = self.sb.confirm_delete.clone() else { return };
        let mut done = false;
        let open = forms::Dialog::new("sound_delete", "Apagar som").icon(ph::TRASH).width(400.0).show(ctx, |ui| {
            ui.add(egui::Label::new(RichText::new(format!("Apagar o som \"{name}\" para todo mundo? Não dá para desfazer.")).color(theme::TEXTO)).wrap());
            forms::footer(ui, |ui| {
                if ui_kit::button(ui, Kind::Danger, Some(ph::TRASH), "Apagar").clicked() {
                    self.sb.busy = true;
                    self.backend.send(Cmd::Sound(SoundCmd::Delete(id)));
                    done = true;
                }
                if ui_kit::button(ui, Kind::Ghost, None, "Cancelar").clicked() {
                    done = true;
                }
            });
        });
        if done || !open {
            self.sb.confirm_delete = None;
        }
    }

    // ------------------------------------------------------------ settings section

    /// Configurações → Sons. Returns whether a setting changed.
    pub fn soundboard_settings(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        forms::group(ui, "Soundboard do servidor");
        if !self.sb.supported {
            let text = if self.me.is_none() {
                "Entre num servidor para usar o soundboard: os sons ficam guardados em cada servidor."
            } else {
                "Este servidor é de uma versão antiga: não tem soundboard."
            };
            forms::note(ui, text, false);
            ui.add_space(space::M);
        }
        forms::row(ui, "Ouvir", |ui| {
            changed |= ui.checkbox(&mut self.settings.soundboard.muted, "Silenciar soundboard").on_hover_text("Você deixa de ouvir os sons que os outros tocam").changed();
        });
        ui.add_enabled_ui(!self.settings.soundboard.muted, |ui| {
            forms::row(ui, "Volume do soundboard", |ui| {
                changed |= ui.add(egui::Slider::new(&mut self.settings.soundboard.volume_pct, 0..=100).suffix("%")).changed();
            });
        });
        forms::note(ui, "Só para você: os outros ouvem no volume deles.", true);
        let key = self.sound_server_key();
        let n = self.settings.soundboard.muted_sounds.get(&key).map_or(0, Vec::len);
        if n > 0 {
            forms::row(ui, "Sons silenciados", |ui| {
                ui.label(RichText::new(format!("{n} som(ns) silenciado(s) só para você")).color(theme::TEXTO));
                if ui_kit::button_sized(ui, Kind::Secondary, Size::Small, Some(ph::SPEAKER_HIGH), "Reativar todos").clicked() {
                    self.settings.soundboard.muted_sounds.remove(&key);
                    changed = true;
                }
            });
        }
        forms::note(ui, "Para tocar, adicionar ou silenciar um som, use o botão Sons na barra da chamada (botão direito num som: mais opções).", false);
        changed
    }

    // ------------------------------------------------------------ tile flash

    /// Light up the tile of someone who just played a sound (drawn under the tile's content).
    pub fn paint_sound_flash(&self, painter: &egui::Painter, rect: Rect, user: UserId, with_picture_space: bool) {
        let Some(f) = self.sb.flashes.get(&user) else { return };
        let age = f.at.elapsed().as_secs_f32();
        if age >= FLASH_SECS {
            return;
        }
        let fade = 1.0 - age / FLASH_SECS;
        let alpha = (255.0 * fade.powf(0.7)) as u8;
        let accent = theme::GIZ.gamma_multiply(alpha as f32 / 255.0);
        painter.rect_stroke(rect.shrink(1.0), 8.0, Stroke::new(4.0, accent), egui::StrokeKind::Inside);
        let size = (rect.height() * 0.22).clamp(14.0, 40.0);
        let white = Color32::from_rgba_unmultiplied(255, 255, 255, alpha);
        if with_picture_space {
            painter.text(Pos2::new(rect.center().x, rect.top() + size * 0.9), Align2::CENTER_CENTER, &f.emoji, FontId::proportional(size), white);
        }
        if !f.name.is_empty() && rect.width() > 110.0 {
            let tag = format!("{} {}", f.emoji, f.name);
            painter.text(rect.right_bottom() - Vec2::new(8.0, 6.0), Align2::RIGHT_BOTTOM, tag, FontId::proportional(12.0), white);
        }
        painter.ctx().request_repaint_after(Duration::from_millis(40));
    }
}

/// Can the UI draw all of `s`? Yes for an emoji with a colour picture (`theme::emoji`); otherwise
/// the font must have it (egui's bundled emoji font is older than some emojis, and its
/// `has_glyph` cannot tell: a glyph the font lacks is the one with no width). Joiners and
/// variation selectors have no width by design.
fn has_glyphs(ctx: &egui::Context, s: &str) -> bool {
    if theme::emoji::has_image(s) {
        return true;
    }
    ctx.fonts_mut(|f| {
        let font = FontId::proportional(14.0);
        s.chars().all(|c| matches!(c, '\u{200d}' | '\u{fe0f}' | '\u{fe0e}') || f.glyph_width(&font, c) > 0.0)
    })
}

/// The emoji of a sound as the UI can draw it: its own, or the speaker when there is none or the
/// font has no glyph for it (so there is never an empty box).
fn shown_emoji(ctx: &egui::Context, emoji: &str) -> String {
    if emoji.is_empty() || !has_glyphs(ctx, emoji) {
        DEFAULT_EMOJI.to_string()
    } else {
        emoji.to_string()
    }
}

pub(crate) fn set_path(form: &mut AddForm, p: PathBuf) {
    form.path = p.display().to_string();
    form.error = None;
    form.want_analyze = true;
    if !form.name_edited || form.name.trim().is_empty() {
        let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        form.name = stem.chars().take(SOUND_NAME_MAX).collect();
        form.name_edited = false;
    }
}

/// The quick emoji picks: small square cells, the chosen one filled.
fn emoji_row(ui: &mut egui::Ui, emoji: &mut String, disabled: bool) {
    ui.add_enabled_ui(!disabled, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::splat(2.0);
            let ctx = ui.ctx().clone();
            for e in QUICK_EMOJI.iter().filter(|e| has_glyphs(&ctx, e)) {
                let (rect, r) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
                if ui.is_rect_visible(rect) {
                    let chosen = emoji.as_str() == *e;
                    if chosen {
                        ui.painter().rect_filled(rect, radius::ROW, theme::MOVEL);
                    } else if r.hovered() && ui.is_enabled() {
                        ui.painter().rect_filled(rect, radius::ROW, theme::ROW_HOVER);
                    }
                    let fg = if ui.is_enabled() { theme::GIZ } else { theme::NEVOA };
                    ui.painter().text(rect.center(), Align2::CENTER_CENTER, *e, FontId::proportional(16.0), fg);
                }
                if r.clicked() {
                    *emoji = e.to_string();
                }
            }
        });
    });
}

/// Does a click at `at` close the popup drawn at `panel`? Only outside it, on whatever is under it
/// (`layer`: the order of the layer there; a menu or tooltip in front, such as a tile's
/// right-click menu, does not count), after the frame it opened on (`after_open`), and while no
/// dialog of its own is open.
fn closes_panel(at: Pos2, panel: Rect, layer: Option<egui::Order>, after_open: bool, dialogs: bool) -> bool {
    after_open && !dialogs && !panel.contains(at) && layer.is_none_or(|o| o < egui::Order::Foreground)
}

/// One sound of the popup's grid: its emoji and name on [`theme::MOVEL`] (dimmed and struck
/// through when silenced for me). On hover a small headphones button appears on the right ("Ouvir
/// só aqui"). Returns the tile's response and whether that button was clicked.
/// While `playing` (a sound I played, still going), a stop sign replaces the emoji and the tile
/// is outlined: a click stops it.
fn sound_tile(ui: &mut egui::Ui, w: f32, emoji: &str, name: &str, muted: bool, playing: bool) -> (egui::Response, bool) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, TILE_H), Sense::click());
    let listen_rect = Rect::from_center_size(Pos2::new(rect.right() - 18.0, rect.center().y), Vec2::splat(26.0));
    let hovered = resp.hovered() || ui.rect_contains_pointer(listen_rect);
    let mut listen = false;
    if ui.is_rect_visible(rect) {
        let pressed = resp.is_pointer_button_down_on();
        let fill = if pressed {
            theme::MOVEL_PRESSED
        } else if hovered {
            theme::MOVEL_HOVER
        } else {
            theme::MOVEL
        };
        let p = ui.painter();
        p.rect_filled(rect, radius::BUTTON, fill);
        let fg = if muted { theme::NEVOA } else { theme::GIZ };
        if playing {
            p.rect_stroke(rect, radius::BUTTON, Stroke::new(theme::stroke::SPEAKING, theme::ABAJUR), egui::StrokeKind::Inside);
            p.text(Pos2::new(rect.left() + 12.0, rect.center().y), Align2::LEFT_CENTER, ph::STOP, theme::body(17.0), theme::ABAJUR);
        } else {
            p.text(Pos2::new(rect.left() + 12.0, rect.center().y), Align2::LEFT_CENTER, emoji, FontId::proportional(17.0), fg);
        }
        let name_x = rect.left() + 38.0;
        let right = if hovered { listen_rect.left() - 4.0 } else { rect.right() - 8.0 };
        let mut job = egui::text::LayoutJob::single_section(name.to_string(), egui::TextFormat::simple(theme::semibold(size::BUTTON), fg));
        job.wrap.max_width = (right - name_x).max(10.0);
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        job.wrap.overflow_character = Some('…');
        let g = p.layout_job(job);
        let gy = rect.center().y - g.size().y / 2.0;
        let gw = g.size().x;
        p.galley(Pos2::new(name_x, gy), g, fg);
        if muted {
            p.hline(name_x..=name_x + gw, rect.center().y, Stroke::new(1.0, theme::NEVOA));
        }
    }
    if hovered {
        // A child over the tile (it does not move the row's cursor); added last, it takes the click.
        let mut c = ui.new_child(egui::UiBuilder::new().id_salt(("sound_listen", name)).max_rect(listen_rect));
        let (r, b) = c.allocate_exact_size(listen_rect.size(), Sense::click());
        let fill = if b.hovered() { theme::MOVEL_PRESSED } else { Color32::TRANSPARENT };
        c.painter().rect_filled(r, radius::ROW, fill);
        c.painter().text(r.center(), Align2::CENTER_CENTER, ph::HEADPHONES, theme::body(15.0), if b.hovered() { theme::GIZ } else { theme::NEVOA });
        listen = b.on_hover_text("Ouvir só aqui (a sala não ouve)").clicked();
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), name));
    (resp, listen)
}

/// The Windows "open file" dialog (blocks: run it on its own thread). `None` = cancelled.
#[cfg(windows)]
fn open_file_dialog(owner: Option<isize>) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::UI::Controls::Dialogs::{GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW};
    let mut file = [0u16; 2048];
    let filter: Vec<u16> = "Áudio (wav, mp3, ogg, flac)\0*.wav;*.mp3;*.ogg;*.flac\0Todos os arquivos\0*.*\0\0".encode_utf16().collect();
    let title: Vec<u16> = "Escolher o som\0".encode_utf16().collect();
    // SAFETY: a zeroed OPENFILENAMEW is the documented starting point; every pointer set below
    // outlives the call, and the dialog only writes into `file` (its length is passed in).
    let ok = unsafe {
        let mut ofn: OPENFILENAMEW = std::mem::zeroed();
        ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
        ofn.hwndOwner = owner.unwrap_or(0) as _;
        ofn.lpstrFilter = filter.as_ptr();
        ofn.nFilterIndex = 1;
        ofn.lpstrFile = file.as_mut_ptr();
        ofn.nMaxFile = file.len() as u32;
        ofn.lpstrTitle = title.as_ptr();
        ofn.Flags = OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
        GetOpenFileNameW(&mut ofn) != 0
    };
    if !ok {
        return None;
    }
    let len = file.iter().position(|&c| c == 0).unwrap_or(file.len());
    Some(PathBuf::from(std::ffi::OsString::from_wide(&file[..len])))
}

#[cfg(target_os = "macos")]
fn open_file_dialog(owner: Option<isize>) -> Option<PathBuf> {
    crate::filedlg::open(owner, "Escolher o som", ("Áudio", "*.wav;*.mp3;*.ogg;*.flac"), false).ok()?.into_iter().next()
}

#[cfg(not(any(windows, target_os = "macos")))]
fn open_file_dialog(_owner: Option<isize>) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_half_volume_and_unmuted() {
        let s = SoundboardSettings::default();
        assert_eq!((s.volume_pct, s.muted), (50, false));
        // An old settings.json without the section gets the defaults.
        let old: crate::settings::Settings = serde_json::from_str(r#"{"server":"x"}"#).unwrap();
        assert_eq!((old.soundboard.volume_pct, old.soundboard.muted), (50, false));
        let round: SoundboardSettings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(round.volume_pct, 50);
    }

    fn user(id: UserId) -> User {
        User::new(id, "x")
    }

    #[test]
    fn the_playing_list_follows_plays_stops_and_the_clip_length() {
        let mut sb = SoundboardUi::default();
        let s = |id, ms| Sound { id, name: "s".into(), emoji: String::new(), uploader: user(1), created_at_ms: 0, duration_ms: ms, size: 1, hash: "h".into() };
        sb.sounds = vec![s(1, 5000), s(2, 1)];
        sb.track_playing(&SoundEv::Played { sound_id: 1, by: user(7), heard: Heard::Playing });
        sb.track_playing(&SoundEv::Played { sound_id: 1, by: user(8), heard: Heard::Downloading });
        sb.track_playing(&SoundEv::Played { sound_id: 2, by: user(7), heard: Heard::Playing });
        sb.track_playing(&SoundEv::Played { sound_id: 1, by: user(9), heard: Heard::Muted });
        std::thread::sleep(Duration::from_millis(5));
        assert!(sb.is_playing(1, Some(7)) && sb.is_playing(1, Some(8)) && !sb.is_playing(1, Some(9)), "a muted play is not playing here");
        assert!(!sb.is_playing(2, None), "a 1 ms clip has ended");
        sb.track_playing(&SoundEv::Stopped { by: Some(7) });
        assert!(!sb.is_playing(1, Some(7)) && sb.is_playing(1, None));
        sb.track_playing(&SoundEv::Stopped { by: None });
        assert_eq!(sb.playing_now().count(), 0);
    }

    #[test]
    fn a_click_elsewhere_closes_the_panel() {
        use egui::Order;
        let panel = Rect::from_min_size(Pos2::new(340.0, 60.0), Vec2::new(404.0, 300.0));
        let chat = Pos2::new(200.0, 200.0);
        assert!(closes_panel(chat, panel, Some(Order::Background), true, false), "on the chat beside it");
        assert!(closes_panel(chat, panel, None, true, false));
        assert!(closes_panel(chat, panel, Some(Order::Middle), true, false), "on another window");
        assert!(!closes_panel(Pos2::new(500.0, 200.0), panel, Some(Order::Middle), true, false), "inside it (a tile, Parar)");
        assert!(!closes_panel(chat, panel, Some(Order::Background), false, false), "the click that opened it");
        assert!(!closes_panel(chat, panel, Some(Order::Background), true, true), "a dialog of its own is open");
        assert!(!closes_panel(chat, panel, Some(Order::Foreground), true, false), "a tile's menu over the chat");
        assert!(!closes_panel(chat, panel, Some(Order::Tooltip), true, false));
    }

    #[test]
    fn play_refusals_are_recognised() {
        assert!(is_play_refusal(vox_proto::ERR_SOUND_RATE_LIMITED) && is_play_refusal(vox_proto::ERR_SOUND_SERVER_MUTED));
        assert!(!is_play_refusal("invalid event"));
    }

    #[test]
    fn hub_errors_are_translated() {
        assert!(server_error_text(vox_proto::ERR_SOUND_RATE_LIMITED).starts_with("Calma"));
        assert!(server_error_text(vox_proto::ERR_SOUND_NOT_IN_VOICE).contains("voz"));
        assert_eq!(server_error_text("boom"), "Servidor: boom");
    }

    #[test]
    fn a_chosen_file_names_the_sound_unless_the_person_typed_one() {
        let mut f = AddForm {
            path: String::new(),
            name: String::new(),
            emoji: String::new(),
            error: Some("x".into()),
            picking: None,
            name_edited: false,
            trim: None,
            analyzed: String::new(),
            want_analyze: false,
        };
        set_path(&mut f, PathBuf::from("sons/buzina de carro.mp3"));
        assert_eq!((f.name.as_str(), f.error.is_none()), ("buzina de carro", true));
        set_path(&mut f, PathBuf::from("sons/outra.wav"));
        assert_eq!(f.name, "outra", "still the file's name: nobody typed one");
        f.name = "meu nome".into();
        f.name_edited = true;
        set_path(&mut f, PathBuf::from("sons/terceira.ogg"));
        assert_eq!(f.name, "meu nome");
        set_path(&mut f, PathBuf::from(format!("sons/{}.wav", "x".repeat(60))));
        assert_eq!(f.name, "meu nome");
        f.name_edited = false;
        set_path(&mut f, PathBuf::from(format!("sons/{}.wav", "x".repeat(60))));
        assert_eq!(f.name.chars().count(), SOUND_NAME_MAX);
    }
}
