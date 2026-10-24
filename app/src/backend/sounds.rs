//! Soundboard in the backend: the sound list, the download cache, the rules for playing a sound
//! someone else triggered, and the upload pipeline.
//!
//! Playing never goes through LiveKit. The hub says `SoundPlayed` to the members of one voice
//! channel; this client then (1) checks it is for the room it is in, (2) applies the personal
//! rules (soundboard muted, this sound muted, deafened: nothing is played), (3) finds the clip in
//! the cache, downloading it first when it has not been seen yet, and (4) queues it in the voice
//! session's playback mixer, next to the voices, so it reaches the same speakers and the echo
//! canceller's reference. With `VOX_NO_PLAYBACK` (the silent autotests) the mixer has no device;
//! the clip is queued all the same and the mix is pulled by hand, in real time, to measure what
//! B would have heard (`SoundEv::MixerHeard`).
//!
//! v1.1, no delay: while in a voice channel every clip of the server is kept decoded in memory
//! (bounded, see `SoundCache`), so a play never waits for the disk; and the player's own app
//! starts the clip at the click instead of waiting for the hub's echo (a refusal from the hub
//! stops it again). The others start it when the hub's `SoundPlayed` reaches them.
//!
//! v1.1, stop: "Parar" (or a click on the sound that is playing) stops, with a 5 ms fade, every
//! sound playing here, and asks the hub (`StopSound`) to stop the ones we played for the whole
//! room; the hub relays `SoundStopped` to the room. A hub older than that (its `SoundPlayed` has
//! no `stoppable`) is never sent `StopSound`: the sound then stops only here. A new sound while
//! one plays is mixed with it (unchanged since v0.5; the hub's per-person gap and room limit
//! keep that from piling up).
//!
//! v1.1, speaking ring: whoever played a sound counts as speaking until it ends or is stopped
//! (`VoiceSession::sound_speaking`), on every app of the room: the clip never travels on their
//! microphone track, so the voice level could not show it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use vox_client_core::audio::MIX_10MS;
use vox_client_core::hub::{HttpStatusError, SoundsReply};
use vox_client_core::soundboard::{prepare_file, ProcessError, SoundCache, SoundPcm, Trim};
use vox_proto::{Sound, SoundId, User, UserId};

use super::*;
use crate::soundboard::{Heard, SoundCmd, SoundEv, SoundOp};

/// A clip that arrives later than this after the hub announced it is not played any more (the
/// moment has passed).
const MAX_LATE: Duration = Duration::from_millis(3000);
/// The hub's echo of our own play is matched to the click within this long.
const OWN_ECHO: Duration = Duration::from_millis(5000);
/// After our last play, "Parar" also asks the hub to stop it for the room for this long (the
/// longest clip plus a margin).
const OWN_STOPPABLE: Duration = Duration::from_millis(vox_proto::SOUND_MAX_MS as u64 + 2000);

/// Our own last play, from the click until the hub's echo (and a while after, for "Parar").
#[derive(Debug, Clone)]
struct OwnPlay {
    sound_id: SoundId,
    at: Instant,
    /// The clip started here at the click (the echo then plays nothing more; a refusal stops
    /// exactly this one).
    local: Option<Arc<SoundPcm>>,
    echoed: bool,
    /// "Parar" came before the echo, while the hub's support for stops was not known yet.
    stop_after_echo: bool,
}

pub(super) struct SoundState {
    /// The current list (shared with the prefetch task).
    list: Arc<Mutex<Vec<Sound>>>,
    /// `None` until the hub answered once on this connection.
    supported: Option<bool>,
    cache: Option<Arc<SoundCache>>,
    volume_pct: u32,
    muted: bool,
    muted_sounds: HashSet<SoundId>,
    prefetching: Arc<AtomicBool>,
    /// Who we are on this hub (from its `Ready`).
    me: Option<User>,
    /// This hub relays stops (`SoundPlayed::stoppable`); `None` until it sent a play.
    hub_stops: Option<bool>,
    own: Option<OwnPlay>,
    /// When each player last stopped their sounds (a clip still downloading is then dropped).
    stopped: HashMap<UserId, Instant>,
    /// When "Parar" was last pressed here (it stops the clips still downloading too).
    stopped_here: Option<Instant>,
    /// A headless measurement of the mix is running (one at a time pulls the mixer).
    measuring: Arc<AtomicBool>,
}

impl Default for SoundState {
    fn default() -> Self {
        Self {
            list: Arc::default(),
            supported: None,
            cache: None,
            volume_pct: 50,
            muted: false,
            muted_sounds: HashSet::new(),
            prefetching: Arc::default(),
            me: None,
            hub_stops: None,
            own: None,
            stopped: HashMap::new(),
            stopped_here: None,
            measuring: Arc::default(),
        }
    }
}

impl SoundState {
    fn gain(&self) -> f32 {
        self.volume_pct.min(100) as f32 / 100.0
    }
}

/// A clip that was not cached when its play event came: it was downloaded and is ready.
pub(super) struct LateSound {
    pub voice_gen: u64,
    pub sound_id: SoundId,
    pub by: User,
    pub pcm: Result<Arc<SoundPcm>, String>,
    pub asked: Instant,
    /// A preview that cannot go through a voice session plays on this output device (`None`: the
    /// default one). Only the trim editor's "Ouvir trecho" (`sound_id` < 0) uses it.
    pub device: Option<String>,
}

/// The identity of a user in the voice room (the hub makes LiveKit tokens with the user id).
fn identity(id: UserId) -> String {
    id.to_string()
}

/// Where downloaded clips live: `VOX_SOUND_CACHE` (tests), else `%LOCALAPPDATA%\concord\sounds`.
/// An autotest without `VOX_SOUND_CACHE` uses a temporary folder of its own: tests never write
/// into the real profile (a `sounds` folder there would also count as "already migrated").
fn cache_root() -> PathBuf {
    if let Some(p) = std::env::var_os("VOX_SOUND_CACHE").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    if vox_client_core::testmode::background() {
        return std::env::temp_dir().join(format!("concord-autotest-sounds-{}", std::process::id()));
    }
    let base = std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("APPDATA")).map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("concord").join("sounds")
}

/// One directory per server (ids are per server): its address with the odd characters replaced.
fn server_dir(base: &Path, host: &str, port: u16) -> PathBuf {
    let host = format!("{host}_{port}");
    base.join(host.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect::<String>())
}

/// A failed hub call for a person: what happened, in Portuguese.
pub(super) fn friendly_error(e: &anyhow::Error) -> String {
    if let Some(h) = e.downcast_ref::<HttpStatusError>() {
        return match h.status {
            400 => format!("O servidor recusou o áudio: {}.", h.message),
            401 => "Sua sessão expirou. Entre de novo.".into(),
            403 => "Só quem enviou o som (ou um moderador) pode mudar ou apagar.".into(),
            404 => "Esse som não existe mais.".into(),
            409 => format!("O soundboard está cheio ({} sons). Apague algum antes.", vox_proto::SOUND_MAX_COUNT),
            413 => "O som passou de 1 MB.".into(),
            429 => "Muitos envios seguidos. Espere um pouco e tente de novo.".into(),
            s => format!("O servidor respondeu com erro {s}."),
        };
    }
    "Não foi possível falar com o servidor.".into()
}

impl Controller {
    fn sound_cache(&mut self) -> Option<Arc<SoundCache>> {
        if self.sb.cache.is_none() {
            let hub = self.hub.as_ref()?;
            let url = hub.base_url();
            self.sb.cache = Some(Arc::new(SoundCache::new(server_dir(&cache_root(), url.host_str().unwrap_or("hub"), url.port_or_known_default().unwrap_or(0)))));
        }
        self.sb.cache.clone()
    }

    fn sb_list(&self) -> Vec<Sound> {
        self.sb.list.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The hub snapshot arrived (login, reconnect): ask for the sound list.
    pub(super) fn sounds_on_ready(&mut self, me: User) {
        self.sb.me = Some(me);
        // Another connection, maybe another hub version: learn again whether it relays stops.
        self.sb.hub_stops = None;
        self.sb.own = None;
        let Some(hub) = self.hub.clone() else { return };
        let (gen, int_tx) = (self.hub_gen, self.int_tx.clone());
        tokio::spawn(async move {
            let _ = int_tx.send(Internal::Sounds(gen, hub.sounds().await));
        });
    }

    pub(super) fn sounds_listed(&mut self, reply: SoundsReply) {
        match reply {
            SoundsReply::Ready(list) => {
                self.sb.supported = Some(true);
                self.sounds_changed(list);
            }
            SoundsReply::Unsupported => {
                info!("the hub has no soundboard (older server): the Sons button stays hidden");
                self.sb.supported = Some(false);
                self.sb.list.lock().unwrap_or_else(|e| e.into_inner()).clear();
                self.out.send(Ev::Sound(SoundEv::List { supported: false, sounds: Vec::new() }));
            }
            SoundsReply::Failed(why) => warn!(%why, "could not load the sound list"),
        }
    }

    /// A new list (from the hub's `SoundsChanged`, or the answer to `GET /api/sounds`).
    pub(super) fn sounds_changed(&mut self, list: Vec<Sound>) {
        if self.sb.supported.is_none() {
            self.sb.supported = Some(true);
        }
        *self.sb.list.lock().unwrap_or_else(|e| e.into_inner()) = list.clone();
        if let Some(cache) = self.sound_cache() {
            let keep: Vec<_> = list.iter().map(|s| (s.id, s.hash.clone())).collect();
            tokio::task::spawn_blocking(move || cache.retain(&keep));
        }
        self.out.send(Ev::Sound(SoundEv::List { supported: true, sounds: list }));
        self.prefetch_sounds();
    }

    /// Forget everything about the sounds (logout / another server).
    pub(super) fn sounds_reset(&mut self) {
        self.sb.supported = None;
        self.sb.cache = None;
        self.sb.me = None;
        self.sb.hub_stops = None;
        self.sb.own = None;
        // A prefetch still running for the old server keeps the old (now empty) list and its own
        // flag: it ends, instead of fetching the next server's clips from the old one and
        // keeping that server's prefetch from starting.
        self.sb.list.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.sb.list = Arc::default();
        self.sb.prefetching = Arc::default();
        self.out.send(Ev::Sound(SoundEv::List { supported: false, sounds: Vec::new() }));
    }

    /// We are in a voice channel now: apply the volume / mute and fetch the clips we do not have.
    pub(super) fn sounds_voice_joined(&mut self) {
        self.apply_sound_gain();
        self.prefetch_sounds();
    }

    /// We left the voice channel: the decoded clips are not needed until the next one (the files
    /// stay on disk).
    pub(super) fn sounds_voice_left(&mut self) {
        self.sb.own = None;
        self.sb.stopped.clear();
        self.sb.stopped_here = None;
        if let Some(c) = &self.sb.cache {
            c.clear_mem();
        }
    }

    fn apply_sound_gain(&self) {
        if let Some(v) = &self.voice {
            v.set_soundboard(self.sb.gain(), self.sb.muted);
        }
    }

    /// Download, in the background and one at a time, the clips of the list that are not cached
    /// (only while in a voice channel: that is the only place they are ever played), then decode
    /// them all into memory (bounded by the cache's budget), so a play starts at once. A clip that
    /// is played before its turn is fetched on demand instead.
    fn prefetch_sounds(&mut self) {
        if self.voice_channel.is_none() || self.sb.supported != Some(true) {
            return;
        }
        let (Some(hub), Some(cache)) = (self.hub.clone(), self.sound_cache()) else { return };
        if self.sb.prefetching.swap(true, Ordering::AcqRel) {
            return; // the running task re-reads the list when it finishes an item
        }
        let (list, flag) = (self.sb.list.clone(), self.sb.prefetching.clone());
        tokio::spawn(async move {
            let mut failed: HashSet<SoundId> = HashSet::new();
            let missing = |failed: &HashSet<SoundId>| list.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|s| !failed.contains(&s.id) && !cache.has(s.id, &s.hash)).cloned();
            loop {
                while let Some(s) = missing(&failed) {
                    match hub.sound_audio(s.id, &s.hash).await.and_then(|wav| cache.put(s.id, &s.hash, &wav).map(|_| ())) {
                        Ok(()) => tracing::debug!(id = s.id, name = %s.name, "sound downloaded"),
                        Err(e) => {
                            // Skip it (a later list change or play tries again); the others still come.
                            warn!(id = s.id, error = %format!("{e:#}"), "sound download failed");
                            failed.insert(s.id);
                        }
                    }
                }
                let keys: Vec<(SoundId, String)> = list.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|s| (s.id, s.hash.clone())).collect();
                let t = Instant::now();
                let warm = cache.clone();
                if let Ok(n) = tokio::task::spawn_blocking(move || warm.warm(&keys)).await {
                    info!(decoded = n, mem_kb = cache.mem_bytes() / 1024, ms = t.elapsed().as_millis() as u64, "soundboard clips ready in memory");
                }
                flag.store(false, Ordering::Release);
                // The list changed while the clips were decoded (no other task started then): go on.
                if missing(&failed).is_none() || flag.swap(true, Ordering::AcqRel) {
                    break;
                }
            }
        });
    }

    pub(super) fn sound_cmd(&mut self, cmd: SoundCmd) {
        match cmd {
            SoundCmd::Prefs { volume_pct, muted, muted_sounds } => {
                self.sb.volume_pct = volume_pct.min(100);
                self.sb.muted = muted;
                self.sb.muted_sounds = muted_sounds.into_iter().collect();
                self.apply_sound_gain();
            }
            SoundCmd::Play(id) => {
                let sent = self.hub_tx.as_ref().map(|tx| tx.send(ClientEvent::PlaySound { sound_id: id }));
                if !matches!(sent, Some(Ok(()))) {
                    self.out.send(Ev::Notice("Som não enviado: sem conexão com o servidor.".into()));
                    return;
                }
                self.sound_play_local(id);
            }
            SoundCmd::Stop => self.sound_stop(),
            SoundCmd::Preview(id) => self.sound_preview(id),
            SoundCmd::Upload { path, name, emoji, trim } => self.sound_upload(path, name, emoji, trim),
            SoundCmd::PreviewClip { path, trim, output_device } => self.sound_preview_clip(path, trim, output_device),
            SoundCmd::Rename { id, name, emoji } => {
                let (Some(hub), out) = (self.hub.clone(), self.out.clone()) else { return };
                tokio::spawn(async move {
                    let r = hub.rename_sound(id, &name, Some(&emoji)).await;
                    done(&out, SoundOp::Rename, r.map(|_| ()));
                });
            }
            SoundCmd::Delete(id) => {
                let (Some(hub), out) = (self.hub.clone(), self.out.clone()) else { return };
                tokio::spawn(async move {
                    let r = hub.delete_sound(id).await;
                    done(&out, SoundOp::Delete, r);
                });
            }
        }
    }

    /// Decode, normalise and upload a file (or just the `trim` part of it, streamed from disk);
    /// the new list arrives by the hub's broadcast.
    fn sound_upload(&mut self, path: PathBuf, name: String, emoji: String, trim: Option<Trim>) {
        let out = self.out.clone();
        let cache = self.sound_cache();
        let Some(hub) = self.hub.clone() else {
            return done(&out, SoundOp::Upload, Err(anyhow::anyhow!("offline")));
        };
        tokio::spawn(async move {
            let fail = |msg: String| out.send(Ev::Sound(SoundEv::Done { op: SoundOp::Upload, result: Err(msg) }));
            let size = tokio::fs::metadata(&path).await.map(|m| m.len()).map_err(|_| ());
            match size {
                Err(()) => return fail("Não consegui abrir esse arquivo. Confira o caminho.".into()),
                Ok(n) if n > vox_client_core::soundboard::process::MAX_SOURCE_BYTES as u64 => return fail(ProcessError::TooBig.to_string()),
                Ok(_) => {}
            }
            let t = Instant::now();
            let prepared = match tokio::task::spawn_blocking(move || prepare_file(&path, trim.as_ref())).await {
                Ok(Ok(p)) => p,
                Ok(Err(e)) => return fail(e.to_string()),
                Err(_) => return fail("Não foi possível processar o áudio.".into()),
            };
            info!(
                ms = t.elapsed().as_millis() as u64,
                duration_ms = prepared.duration_ms,
                channels = prepared.channels,
                source_rate = prepared.source_rate,
                lufs_before = prepared.loudness_before,
                gain_db = prepared.gain_db,
                trimmed_start_ms = prepared.trimmed_start_ms,
                trimmed_end_ms = prepared.trimmed_end_ms,
                selection = ?trim.map(|t| (t.start_ms, t.end_ms)),
                bytes = prepared.wav.len(),
                "sound prepared"
            );
            let wav = prepared.wav.clone();
            match hub.upload_sound(&name, &emoji, prepared.wav).await {
                Ok(sound) => {
                    // The hub stores exactly these samples: keep them, nothing to download.
                    if let Some(cache) = cache {
                        let _ = tokio::task::spawn_blocking(move || cache.put(sound.id, &sound.hash, &wav)).await;
                    }
                    out.send(Ev::Sound(SoundEv::Done { op: SoundOp::Upload, result: Ok(()) }));
                }
                Err(e) => {
                    warn!(error = %format!("{e:#}"), "sound upload failed");
                    fail(friendly_error(&e));
                }
            }
        });
    }

    /// "Ouvir trecho": cut the part of the file exactly as an upload would, then play it here only.
    /// In a voice channel it goes through the session's output (the person's speakers, and the echo
    /// canceller's reference), otherwise to `device`. Headless runs play nothing; they just say the
    /// clip is ready.
    fn sound_preview_clip(&mut self, path: PathBuf, trim: Trim, device: Option<String>) {
        let (out, int_tx, voice_gen, no_playback) = (self.out.clone(), self.int_tx.clone(), self.voice_gen, self.no_playback);
        tokio::spawn(async move {
            let say = |r: Result<u32, String>| out.send(Ev::Sound(SoundEv::ClipPreview(r)));
            let prepared = match tokio::task::spawn_blocking(move || prepare_file(&path, Some(&trim))).await {
                Ok(Ok(p)) => p,
                Ok(Err(e)) => return say(Err(e.to_string())),
                Err(_) => return say(Err("Não foi possível processar o áudio.".into())),
            };
            let pcm = match SoundPcm::from_wav(&prepared.wav) {
                Ok(p) => Arc::new(p),
                Err(e) => return say(Err(format!("{e:#}"))),
            };
            if no_playback {
                info!(ms = pcm.duration_ms(), "clip preview suppressed (VOX_NO_PLAYBACK)");
                return say(Ok(pcm.duration_ms()));
            }
            let late = LateSound { voice_gen, sound_id: -1, by: User::new(-1, ""), pcm: Ok(pcm), asked: Instant::now(), device };
            let _ = int_tx.send(Internal::LateSound(late));
        });
    }

    /// "Ouvir só aqui".
    fn sound_preview(&mut self, id: SoundId) {
        if self.no_playback {
            tracing::debug!(id, "sound preview suppressed (VOX_NO_PLAYBACK)");
            return;
        }
        let Some(sound) = self.sb_list().into_iter().find(|s| s.id == id) else { return };
        let gain = self.sb.gain();
        match self.sound_pcm(&sound) {
            Some(pcm) => {
                if let Some(v) = &self.voice {
                    v.preview_sound(&pcm, gain);
                }
            }
            None => {
                // Download first; the result comes back as a late sound with no `by`: preview.
                self.fetch_late(sound, None);
            }
        }
    }

    /// The clip if it is decoded in memory (never blocks on the disk; a miss goes through
    /// [`Controller::fetch_late`]).
    fn sound_pcm(&mut self, s: &Sound) -> Option<Arc<SoundPcm>> {
        self.sound_cache()?.get_mem(s.id, &s.hash)
    }

    /// Why this app would not play `sound_id` now (`None`: it would).
    fn sound_rule(&self, sound_id: SoundId) -> Option<Heard> {
        if self.sb.muted {
            Some(Heard::Muted)
        } else if self.sb.muted_sounds.contains(&sound_id) {
            Some(Heard::SoundMuted)
        } else if self.voice.as_ref().is_some_and(|v| v.is_deafened()) {
            Some(Heard::Deafened)
        } else {
            None
        }
    }

    /// Our own click (already sent to the hub): light our ring and, when the clip is in memory
    /// and the personal rules allow, start it here at once instead of waiting for the hub's echo.
    fn sound_play_local(&mut self, sound_id: SoundId) {
        let at = Instant::now();
        let mut local = None;
        let me = self.sb.me.clone();
        let sound = self.sb_list().into_iter().find(|s| s.id == sound_id);
        if let (Some(me), Some(sound), true) = (me, sound, self.voice_channel.is_some()) {
            if let Some(v) = &self.voice {
                v.sound_speaking(&identity(me.id), Some(Duration::from_millis(sound.duration_ms as u64)));
            }
            if self.sound_rule(sound_id).is_none() {
                if let Some(pcm) = self.sound_pcm(&sound) {
                    local = self.queue_sound(sound_id, me.id, &pcm).then_some(pcm);
                }
            }
            if local.is_some() {
                info!(sound_id, click_to_queue_us = at.elapsed().as_micros() as u64, "my sound started here at the click");
                self.out.send(Ev::Sound(SoundEv::Played { sound_id, by: me, heard: Heard::Playing }));
            }
        }
        self.sb.own = Some(OwnPlay { sound_id, at, local, echoed: false, stop_after_echo: false });
    }

    /// "Parar": every sound playing here stops (5 ms fade), and the ones we played stop for the
    /// whole room too (on a hub that relays it).
    fn sound_stop(&mut self) {
        self.sb.stopped_here = Some(Instant::now());
        let mut n = 0;
        if let Some(v) = &self.voice {
            n = v.stop_sounds(None) + v.stop_previews();
            if let Some(me) = &self.sb.me {
                v.sound_speaking(&identity(me.id), None);
            }
        }
        let mut ask_hub = false;
        if let Some(own) = self.sb.own.as_mut().filter(|o| o.at.elapsed() < OWN_STOPPABLE) {
            match self.sb.hub_stops {
                Some(true) => ask_hub = true,
                // Pressed before the echo told us what this hub can do: decide when it comes.
                None if !own.echoed => own.stop_after_echo = true,
                _ => {}
            }
        }
        if ask_hub {
            self.send_stop();
        }
        info!(stopped_here = n, for_room = ask_hub, "soundboard stop");
        self.out.send(Ev::Sound(SoundEv::Stopped { by: None }));
    }

    fn send_stop(&mut self) {
        if let Some(tx) = &self.hub_tx {
            let _ = tx.send(ClientEvent::StopSound);
        }
        self.sb.own = None;
    }

    /// `SoundStopped` from the hub: `by` stopped their sounds in our room.
    pub(super) fn sound_stopped(&mut self, by: UserId, channel_id: ChannelId) {
        if self.voice_channel != Some(channel_id) {
            return;
        }
        self.sb.stopped.insert(by, Instant::now());
        let Some(v) = &self.voice else { return };
        let n = v.stop_sounds(Some(by));
        v.sound_speaking(&identity(by), None);
        info!(by, stopped_here = n, "sound stopped by its player");
        self.out.send(Ev::Sound(SoundEv::Stopped { by: Some(by) }));
    }

    /// The hub refused our last play (rate limit, server mute...): the clip started at the click
    /// stops again, and so does our ring.
    pub(super) fn sound_refused(&mut self) {
        let Some(own) = self.sb.own.take().filter(|o| !o.echoed && o.at.elapsed() < OWN_ECHO) else { return };
        let Some(me) = self.sb.me.as_ref().map(|m| m.id) else { return };
        if let Some(v) = &self.voice {
            // Only this play: an earlier sound of ours the hub accepted goes on (for the room
            // too), and so does our ring while it lasts.
            let stopped = own.local.as_ref().is_some_and(|pcm| v.stop_clip(pcm, me));
            v.sound_speaking(&identity(me), v.sound_left(me));
            info!(sound_id = own.sound_id, stopped, "the hub refused my sound; stopped it here");
        }
        self.out.send(Ev::Sound(SoundEv::Stopped { by: Some(me) }));
    }

    /// A clip that is not in memory: read it from disk (off this thread) or, when it was never
    /// downloaded, from the hub. `by`: who played it (`None`: a local preview).
    fn fetch_late(&mut self, sound: Sound, by: Option<User>) {
        let (Some(hub), Some(cache)) = (self.hub.clone(), self.sound_cache()) else { return };
        let (int_tx, voice_gen, asked) = (self.int_tx.clone(), self.voice_gen, Instant::now());
        tokio::spawn(async move {
            let disk = cache.clone();
            let (id, hash) = (sound.id, sound.hash.clone());
            let pcm = match tokio::task::spawn_blocking(move || disk.get(id, &hash)).await.ok().flatten() {
                Some(pcm) => Ok(pcm),
                None => match hub.sound_audio(sound.id, &sound.hash).await {
                    Ok(wav) => cache.put(sound.id, &sound.hash, &wav).map_err(|e| format!("{e:#}")),
                    Err(e) => Err(format!("{e:#}")),
                },
            };
            let by = by.unwrap_or_else(|| User::new(-1, ""));
            let _ = int_tx.send(Internal::LateSound(LateSound { voice_gen, sound_id: sound.id, by, pcm, asked, device: None }));
        });
    }

    /// A clip that had to be downloaded first is here.
    pub(super) fn late_sound_ready(&mut self, late: LateSound) {
        if late.voice_gen != self.voice_gen {
            return;
        }
        let pcm = match late.pcm {
            Ok(p) => p,
            Err(e) => return warn!(id = late.sound_id, error = %e, "sound download failed"),
        };
        if late.by.id < 0 {
            // A preview that waited for its download, or the trim editor's clip.
            let gain = self.sb.gain();
            let played = match &self.voice {
                Some(v) => v.preview_sound(&pcm, gain),
                // Not in a voice channel: the output device is opened just for it.
                None if late.sound_id < 0 => self.ui_player.play_pcm(pcm.to_stereo(), gain, late.device.as_deref()),
                None => false,
            };
            if late.sound_id < 0 {
                let r = if played { Ok(pcm.duration_ms()) } else { Err("Não consegui tocar: a saída de áudio não está disponível.".to_string()) };
                self.out.send(Ev::Sound(SoundEv::ClipPreview(r)));
            }
            return;
        }
        if late.asked.elapsed() > MAX_LATE {
            return info!(id = late.sound_id, "downloaded sound arrived too late; not played");
        }
        // Stopped by its player, or by our "Parar", while it was on its way.
        if self.sb.stopped.get(&late.by.id).into_iter().chain(&self.sb.stopped_here).any(|t| *t >= late.asked) {
            return info!(id = late.sound_id, "sound stopped before it was ready; not played");
        }
        if self.sb.me.as_ref().is_some_and(|m| m.id == late.by.id) && self.sb.own.as_ref().is_some_and(|o| o.stop_after_echo) {
            return;
        }
        // The personal rules may have changed while it downloaded.
        if self.sound_rule(late.sound_id).is_some() || self.voice.is_none() {
            return;
        }
        info!(id = late.sound_id, wait_ms = late.asked.elapsed().as_millis() as u64, "sound ready after a disk read or download");
        self.queue_sound(late.sound_id, late.by.id, &pcm);
    }

    /// `SoundPlayed` from the hub. Always tells the UI (the tile flashes even when nothing is
    /// played here); plays the clip when the personal rules allow.
    pub(super) fn sound_played(&mut self, sound_id: SoundId, by: User, channel_id: ChannelId, stoppable: bool) {
        let t = Instant::now();
        let heard = self.sound_played_inner(sound_id, &by, channel_id, stoppable);
        if let Some(heard) = heard {
            info!(sound_id, by = %by.username, ?heard, event_to_queue_us = t.elapsed().as_micros() as u64, "sound played in my room");
            self.out.send(Ev::Sound(SoundEv::Played { sound_id, by, heard }));
        }
    }

    fn sound_played_inner(&mut self, sound_id: SoundId, by: &User, channel_id: ChannelId, stoppable: bool) -> Option<Heard> {
        // Only the room we are in (a stale event after leaving or moving is ignored).
        if self.voice_channel != Some(channel_id) || self.voice.is_none() {
            return None;
        }
        self.sb.hub_stops = Some(stoppable);
        let sound = self.sb_list().into_iter().find(|s| s.id == sound_id);
        // Our own play coming back: it was started (or refused locally) at the click.
        if self.sb.me.as_ref().is_some_and(|m| m.id == by.id) {
            if let Some(own) = self.sb.own.as_mut().filter(|o| o.sound_id == sound_id && !o.echoed && o.at.elapsed() < OWN_ECHO) {
                own.echoed = true;
                let (local, stop) = (own.local.is_some(), own.stop_after_echo);
                // What the click would have waited for before v1.1 (the hub's round trip).
                info!(sound_id, click_to_echo_ms = own.at.elapsed().as_secs_f64() * 1000.0, started_at_click = local, "my sound came back from the hub");
                if stop {
                    if stoppable {
                        self.send_stop();
                    }
                    return None;
                }
                if local {
                    return None;
                }
            }
        }
        // The player's ring stays lit for the whole sound, whatever this app plays.
        if let (Some(v), Some(s)) = (&self.voice, &sound) {
            v.sound_speaking(&identity(by.id), Some(Duration::from_millis(s.duration_ms as u64)));
        }
        let heard = match self.sound_rule(sound_id) {
            Some(h) => h,
            None => {
                let Some(sound) = sound else {
                    warn!(sound_id, "played sound is not in the list (yet); ignored");
                    return None;
                };
                match self.sound_pcm(&sound) {
                    Some(pcm) => {
                        if self.queue_sound(sound_id, by.id, &pcm) {
                            Heard::Playing
                        } else {
                            Heard::NoOutput
                        }
                    }
                    None => {
                        self.fetch_late(sound, Some(by.clone()));
                        Heard::Downloading
                    }
                }
            }
        };
        Some(heard)
    }

    /// Hand a clip to the voice session's mixer (tagged with its player, for a stop); headless
    /// runs also measure the mix.
    fn queue_sound(&mut self, sound_id: SoundId, by: UserId, pcm: &SoundPcm) -> bool {
        let Some(v) = &self.voice else { return false };
        v.set_soundboard(self.sb.gain(), self.sb.muted);
        if !v.play_sound_by(pcm, by) {
            return false;
        }
        if self.no_playback && !self.sb.measuring.swap(true, Ordering::AcqRel) {
            measure_headless(v.playback_mixer(), sound_id, pcm.duration_ms(), self.out.clone(), self.sb.measuring.clone());
        }
        true
    }
}

fn done(out: &Out, op: SoundOp, r: anyhow::Result<()>) {
    let result = r.map_err(|e| {
        warn!(?op, error = %format!("{e:#}"), "soundboard call failed");
        friendly_error(&e)
    });
    out.send(Ev::Sound(SoundEv::Done { op, result }));
}

/// No output device exists in a silent run: pull the playback mix by hand, at the pace a sound
/// card would (10 ms blocks in real time, so a stop arriving mid-clip cuts it as it would for a
/// person), until the clips are done, and report how loud it was and when the first sample came
/// out. Nothing is ever played.
fn measure_headless(mixer: Arc<vox_client_core::audio::Mixer>, sound_id: SoundId, clip_ms: u32, out: Out, busy: Arc<AtomicBool>) {
    let queued = Instant::now();
    tokio::task::spawn_blocking(move || {
        let mut block = vec![0i16; MIX_10MS];
        let (mut sum, mut n, mut peak) = (0f64, 0u64, 0i16);
        let mut first_us = None;
        let mut blocks = 0u64;
        while mixer.sounds_active() > 0 && queued.elapsed() < Duration::from_secs(10) {
            // The device would ask for block k at k * 10 ms.
            let due = Duration::from_millis(blocks * 10);
            if let Some(wait) = due.checked_sub(queued.elapsed()) {
                std::thread::sleep(wait);
            }
            mixer.mix(&mut block);
            blocks += 1;
            if first_us.is_none() && block.iter().any(|&s| s != 0) {
                first_us = Some(queued.elapsed().as_micros() as u64);
            }
            for &s in &block {
                sum += (s as f64 / 32768.0).powi(2);
                peak = peak.max(s.saturating_abs());
            }
            n += block.len() as u64;
        }
        busy.store(false, Ordering::Release);
        let rms = if n > 0 { (sum / n as f64).sqrt() } else { 0.0 };
        let rms_dbfs = if rms > 0.0 { 20.0 * rms.log10() as f32 } else { f32::NEG_INFINITY };
        let ms = (n / 2 * 1000 / 48_000) as u32;
        info!(sound_id, clip_ms, ms, queue_to_first_sample_us = ?first_us, "headless mix measured");
        out.send(Ev::Sound(SoundEv::MixerHeard { sound_id, rms_dbfs, peak: peak as f32 / 32768.0, ms }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(status: u16) -> anyhow::Error {
        anyhow::Error::new(HttpStatusError { status, message: "boom".into() }).context("sound upload failed")
    }

    #[test]
    fn hub_refusals_become_portuguese_sentences() {
        assert!(friendly_error(&http(403)).starts_with("Só quem enviou"));
        assert!(friendly_error(&http(409)).contains("cheio") && friendly_error(&http(409)).contains("50"));
        assert!(friendly_error(&http(413)).contains("1 MB"));
        assert!(friendly_error(&http(429)).contains("Espere"));
        assert!(friendly_error(&http(404)).contains("não existe"));
        assert!(friendly_error(&http(400)).contains("boom"), "the hub's reason is passed on");
        assert!(friendly_error(&http(500)).contains("500"));
        assert!(friendly_error(&anyhow::anyhow!("connection refused")).contains("servidor"));
    }

    #[test]
    fn each_server_has_its_own_safe_cache_directory() {
        let base = Path::new("cache");
        assert_eq!(server_dir(base, "192.168.1.50", 7890), base.join("192.168.1.50_7890"));
        assert_ne!(server_dir(base, "a.example.com", 7890), server_dir(base, "a.example.com", 7891));
        let odd = server_dir(base, "../../x/y:z", 1);
        assert_eq!(odd.parent(), Some(base), "never leaves the cache directory: {odd:?}");
    }

    #[test]
    fn the_default_volume_is_half() {
        let s = SoundState::default();
        assert_eq!((s.volume_pct, s.muted, s.gain()), (50, false, 0.5));
        let loud = SoundState { volume_pct: 400, ..SoundState::default() };
        assert_eq!(loud.gain(), 1.0, "capped at 100 %");
    }
}
