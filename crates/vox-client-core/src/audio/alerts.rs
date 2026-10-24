//! Alert sounds ("ui sounds"): short synthesized chimes that only the local user hears. Nothing
//! is shipped as an audio file; each sound is a couple of decaying sine/triangle notes computed
//! once into a 48 kHz stereo buffer ([`AlertKind::samples`]) and mixed by the playback mixer
//! (see [`super::mixer::Mixer::play_ui`]) or by the standalone [`super::ui_player::UiPlayer`].
//!
//! Every sound peaks well below full scale (so mixing a few of them with voices never clips) and
//! starts and ends at exactly zero with smooth fades, so there is no click at the edges.
//!
//! v1.1 (card S6) adds [`Palette`]s: the same 15 alerts re-voiced lower (fundamentals about
//! 220..=780 Hz), rounder (soft attack, exponential decay, gentle low-pass, a body partial) and a
//! little longer (150..=450 ms), loudness-matched on a B-weighted short-term level instead of on
//! peaks. [`Palette::SinoSuave`] is the default; [`Palette::Atual`] keeps the v0.6..v1.0 chimes
//! (80..=250 ms) for whoever prefers them. The choice is process-wide ([`set_palette`]): every
//! playback path asks [`AlertKind::samples`], which reads it, so a change applies to the next
//! sound. `cargo run -p vox-client-core --example alert_palettes` renders every palette to WAV for
//! listening.

use std::f32::consts::{FRAC_PI_2, LN_10, TAU};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

use super::mixer::{MIX_CHANNELS, MIX_RATE};

/// A family of alert sounds. Every palette voices all of [`AlertKind::ALL`] with the same meaning
/// (join rises, leave falls, mention is the most insistent, a channel message the softest...).
///
/// [`Palette::ALL`] lists the variants in declaration order (the discriminants index it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Palette {
    /// The v0.6..v1.0 chimes: high (A4..G6, mostly E5 and up), short (80..=250 ms) sine bells and
    /// triangle blips. Shown as "Clássico (1.0)".
    Atual,
    /// Marimba / wood bar: a mallet-hard 3 ms attack, the bar's 4th and ~10th partials for the
    /// "tok", a fundamental that rings out. G-major pentatonic, B3..G5.
    Madeira,
    /// Round bubbly pops: near-sine notes that scoop up into pitch like a drop, with a soft 2nd
    /// harmonic for body. F-major pentatonic, D4..F5.
    Gota,
    /// Soft low bell: the current note family an octave down, with a sub-octave hum, longer ring
    /// and a slow 7 ms attack. A3..G5. The default since v1.1.
    #[default]
    SinoSuave,
}

/// The palette [`AlertKind::samples`] plays (a [`Palette`] discriminant).
static CURRENT: AtomicU8 = AtomicU8::new(Palette::SinoSuave as u8);

/// Choose the palette every alert of this process plays from now on (the app sets the user's
/// choice at startup and whenever it changes). Sounds already queued finish as they were.
pub fn set_palette(palette: Palette) {
    CURRENT.store(palette as u8, Ordering::Relaxed);
}

/// The palette alerts currently play ([`Palette::default`] until [`set_palette`]).
pub fn palette() -> Palette {
    Palette::ALL.get(CURRENT.load(Ordering::Relaxed) as usize).copied().unwrap_or_default()
}

/// Tests that change the process-wide palette hold this, one at a time; dropping it puts the
/// default back (also when the test fails).
#[cfg(test)]
pub(crate) fn palette_test_guard() -> PaletteTestGuard {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    PaletteTestGuard { _lock: LOCK.lock().unwrap_or_else(|e| e.into_inner()) }
}

#[cfg(test)]
pub(crate) struct PaletteTestGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for PaletteTestGuard {
    fn drop(&mut self) {
        set_palette(Palette::default());
    }
}

impl Palette {
    pub const ALL: [Palette; 4] = [Palette::Atual, Palette::Madeira, Palette::Gota, Palette::SinoSuave];

    fn index(self) -> usize {
        Self::ALL.iter().position(|p| *p == self).expect("listed in ALL")
    }

    /// Display name (pt-BR).
    pub fn label(self) -> &'static str {
        match self {
            Palette::Atual => "Clássico (1.0)",
            Palette::Madeira => "Madeira",
            Palette::Gota => "Gota",
            Palette::SinoSuave => "Sino suave",
        }
    }

    /// Stable name for settings files (never change these).
    pub fn key(self) -> &'static str {
        match self {
            Palette::Atual => "atual",
            Palette::Madeira => "madeira",
            Palette::Gota => "gota",
            Palette::SinoSuave => "sino_suave",
        }
    }

    /// The palette saved as `key` ([`Palette::key`]); `None` for a name this version does not know.
    pub fn from_key(key: &str) -> Option<Palette> {
        Self::ALL.into_iter().find(|p| p.key() == key)
    }
}

/// Every alert the client can play.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AlertKind {
    /// Someone entered my voice channel (also: I entered one). Two rising bell notes.
    Join,
    /// Someone left my voice channel (also: I left). Two falling bell notes.
    Leave,
    /// Someone came online on the server. One soft high tick, quiet.
    Online,
    /// Someone went offline. One soft lower tick, quiet.
    Offline,
    /// I muted my microphone. Two falling blips.
    Mute,
    /// I unmuted. Two rising blips.
    Unmute,
    /// I deafened myself. Three falling blips.
    Deafen,
    /// I undeafened. Three rising blips.
    Undeafen,
    /// Someone in my channel went live. Three rising bell notes.
    StreamStart,
    /// Someone in my channel stopped their live. Three falling bell notes.
    StreamStop,
    /// Someone started watching *my* live. Two identical bell pings ("knock knock").
    Viewer,
    /// A direct message arrived ("Mensagem direta"). A rising two-note pop, higher than a join.
    DirectMessage,
    /// Somebody is calling me from a DM: one short trill, repeated by the app while it rings
    /// (at most `CALL_RING_SECS`); stopping is simply not repeating it.
    Ring,
    /// A message in a text channel I do not have open. One soft, low bell note: the quietest of
    /// the chat sounds, since a busy channel plays it all day.
    ChannelMessage,
    /// Somebody mentioned me (v0.8). Three rising bright notes, higher than a direct message.
    Mention,
}

impl AlertKind {
    pub const ALL: [AlertKind; 15] = [
        AlertKind::Join,
        AlertKind::Leave,
        AlertKind::Online,
        AlertKind::Offline,
        AlertKind::Mute,
        AlertKind::Unmute,
        AlertKind::Deafen,
        AlertKind::Undeafen,
        AlertKind::StreamStart,
        AlertKind::StreamStop,
        AlertKind::Viewer,
        AlertKind::DirectMessage,
        AlertKind::Ring,
        AlertKind::ChannelMessage,
        AlertKind::Mention,
    ];

    fn index(self) -> usize {
        Self::ALL.iter().position(|k| *k == self).expect("listed in ALL")
    }

    /// Short stable name for logs.
    pub fn name(self) -> &'static str {
        match self {
            AlertKind::Join => "join",
            AlertKind::Leave => "leave",
            AlertKind::Online => "online",
            AlertKind::Offline => "offline",
            AlertKind::Mute => "mute",
            AlertKind::Unmute => "unmute",
            AlertKind::Deafen => "deafen",
            AlertKind::Undeafen => "undeafen",
            AlertKind::StreamStart => "stream_start",
            AlertKind::StreamStop => "stream_stop",
            AlertKind::Viewer => "viewer",
            AlertKind::DirectMessage => "direct_message",
            AlertKind::Ring => "ring",
            AlertKind::ChannelMessage => "channel_message",
            AlertKind::Mention => "mention",
        }
    }

    /// The precomputed 48 kHz interleaved stereo buffer (built once per palette, shared) in the
    /// palette chosen with [`set_palette`]. Every playback path plays this.
    pub fn samples(self) -> Arc<[i16]> {
        self.samples_in(palette())
    }

    /// The precomputed 48 kHz interleaved stereo buffer of this alert in `palette` (each palette
    /// is built once, on first use, and shared).
    pub fn samples_in(self, palette: Palette) -> Arc<[i16]> {
        static BANKS: [OnceLock<Vec<Arc<[i16]>>>; Palette::ALL.len()] = [const { OnceLock::new() }; Palette::ALL.len()];
        BANKS[palette.index()].get_or_init(|| AlertKind::ALL.iter().map(|k| Arc::from(k.render(palette))).collect())[self.index()].clone()
    }

    /// Synthesizes this alert in `palette` (uncached; see [`AlertKind::samples_in`]).
    pub fn render(self, palette: Palette) -> Vec<i16> {
        match palette {
            Palette::Atual => synth(&self.spec()),
            _ => synth_patch(&self.patch(palette), self.level_offset_db()),
        }
    }

    /// Loudness of this alert relative to the palette's reference level (the voice-channel
    /// alerts), in dB. Shared by every new palette so they keep the same hierarchy.
    fn level_offset_db(self) -> f32 {
        match self {
            AlertKind::Mention => 1.5,
            AlertKind::Join
            | AlertKind::Leave
            | AlertKind::StreamStart
            | AlertKind::StreamStop
            | AlertKind::Viewer
            | AlertKind::DirectMessage
            | AlertKind::Ring => 0.0,
            AlertKind::Mute | AlertKind::Unmute | AlertKind::Deafen | AlertKind::Undeafen => -1.0,
            AlertKind::ChannelMessage => -7.0,
            AlertKind::Online | AlertKind::Offline => -10.0,
        }
    }

    /// The notes of this alert in one of the new palettes (not [`Palette::Atual`]).
    fn patch(self, palette: Palette) -> Patch {
        // Pitches (Hz), 3rd..5th octave.
        const A3: f32 = 220.0;
        const B3: f32 = 246.94;
        const C4: f32 = 261.63;
        const D4: f32 = 293.66;
        const E4: f32 = 329.63;
        const F4: f32 = 349.23;
        const FS4: f32 = 369.99;
        const G4: f32 = 392.0;
        const A4: f32 = 440.0;
        const B4: f32 = 493.88;
        const C5: f32 = 523.25;
        const D5: f32 = 587.33;
        const E5: f32 = 659.25;
        const F5: f32 = 698.46;
        const G5: f32 = 783.99;
        let n = |freq: f32, start_ms: f32, dur_ms: f32| Tone { freq, start_ms, dur_ms };
        use AlertKind as K;
        match palette {
            Palette::Atual => unreachable!("the current sounds use AlertKind::spec"),
            // Marimba in G-major pentatonic: a falling or rising fifth for presence, a three-note
            // run for streams and a mention, single soft "tok"s for the quiet ones.
            Palette::Madeira => Patch::new(
                &MADEIRA,
                match self {
                    K::Join => vec![n(G4, 0.0, 220.0), n(D5, 85.0, 300.0)],
                    K::Leave => vec![n(D5, 0.0, 220.0), n(G4, 85.0, 300.0)],
                    K::Online => vec![n(E5, 0.0, 200.0)],
                    K::Offline => vec![n(B4, 0.0, 200.0)],
                    K::Mute => vec![n(A4, 0.0, 150.0), n(D4, 70.0, 230.0)],
                    K::Unmute => vec![n(D4, 0.0, 150.0), n(A4, 70.0, 230.0)],
                    K::Deafen => vec![n(B4, 0.0, 140.0), n(E4, 70.0, 140.0), n(B3, 140.0, 250.0)],
                    K::Undeafen => vec![n(B3, 0.0, 140.0), n(E4, 70.0, 140.0), n(B4, 140.0, 250.0)],
                    K::StreamStart => vec![n(G4, 0.0, 180.0), n(B4, 80.0, 180.0), n(D5, 160.0, 280.0)],
                    K::StreamStop => vec![n(D5, 0.0, 180.0), n(B4, 80.0, 180.0), n(G4, 160.0, 280.0)],
                    K::Viewer => vec![n(E5, 0.0, 150.0), n(E5, 130.0, 230.0)],
                    K::DirectMessage => vec![n(B4, 0.0, 200.0), n(E5, 85.0, 300.0)],
                    K::Ring => vec![n(G4, 0.0, 110.0), n(D5, 75.0, 110.0), n(G4, 150.0, 110.0), n(D5, 225.0, 210.0)],
                    K::ChannelMessage => vec![n(D5, 0.0, 260.0)],
                    K::Mention => vec![n(B4, 0.0, 140.0), n(D5, 65.0, 140.0), n(G5, 130.0, 300.0)],
                },
            ),
            // Drops in F-major pentatonic: quicker, closer notes than the wood, every one scooping
            // up into pitch.
            Palette::Gota => Patch::new(
                &GOTA,
                match self {
                    K::Join => vec![n(F4, 0.0, 170.0), n(C5, 90.0, 240.0)],
                    K::Leave => vec![n(C5, 0.0, 170.0), n(F4, 90.0, 240.0)],
                    K::Online => vec![n(D5, 0.0, 170.0)],
                    K::Offline => vec![n(A4, 0.0, 170.0)],
                    K::Mute => vec![n(C5, 0.0, 130.0), n(F4, 75.0, 200.0)],
                    K::Unmute => vec![n(F4, 0.0, 130.0), n(C5, 75.0, 200.0)],
                    K::Deafen => vec![n(D5, 0.0, 120.0), n(A4, 75.0, 120.0), n(D4, 150.0, 220.0)],
                    K::Undeafen => vec![n(D4, 0.0, 120.0), n(A4, 75.0, 120.0), n(D5, 150.0, 220.0)],
                    K::StreamStart => vec![n(F4, 0.0, 150.0), n(A4, 80.0, 150.0), n(C5, 160.0, 240.0)],
                    K::StreamStop => vec![n(C5, 0.0, 150.0), n(A4, 80.0, 150.0), n(F4, 160.0, 240.0)],
                    K::Viewer => vec![n(A4, 0.0, 140.0), n(A4, 140.0, 200.0)],
                    K::DirectMessage => vec![n(G4, 0.0, 160.0), n(D5, 85.0, 240.0)],
                    K::Ring => vec![n(A4, 0.0, 100.0), n(D5, 70.0, 100.0), n(A4, 140.0, 100.0), n(D5, 210.0, 200.0)],
                    K::ChannelMessage => vec![n(C5, 0.0, 200.0)],
                    K::Mention => vec![n(A4, 0.0, 120.0), n(D5, 60.0, 120.0), n(F5, 120.0, 260.0)],
                },
            ),
            // The current melodies an octave down, slower and longer so the bell can ring.
            Palette::SinoSuave => Patch::new(
                &SINO_SUAVE,
                match self {
                    K::Join => vec![n(E4, 0.0, 220.0), n(B4, 100.0, 320.0)],
                    K::Leave => vec![n(A4, 0.0, 200.0), n(D4, 100.0, 320.0)],
                    K::Online => vec![n(E5, 0.0, 250.0)],
                    K::Offline => vec![n(A4, 0.0, 250.0)],
                    K::Mute => vec![n(FS4, 0.0, 150.0), n(B3, 80.0, 230.0)],
                    K::Unmute => vec![n(B3, 0.0, 150.0), n(FS4, 80.0, 230.0)],
                    K::Deafen => vec![n(A4, 0.0, 130.0), n(E4, 85.0, 130.0), n(A3, 170.0, 260.0)],
                    K::Undeafen => vec![n(A3, 0.0, 130.0), n(E4, 85.0, 130.0), n(A4, 170.0, 260.0)],
                    K::StreamStart => vec![n(C4, 0.0, 170.0), n(E4, 85.0, 170.0), n(G4, 170.0, 270.0)],
                    K::StreamStop => vec![n(G4, 0.0, 170.0), n(E4, 85.0, 170.0), n(C4, 170.0, 270.0)],
                    K::Viewer => vec![n(G4, 0.0, 150.0), n(G4, 140.0, 250.0)],
                    K::DirectMessage => vec![n(B4, 0.0, 170.0), n(E5, 95.0, 300.0)],
                    K::Ring => vec![n(E4, 0.0, 110.0), n(B4, 80.0, 110.0), n(E4, 160.0, 110.0), n(B4, 240.0, 200.0)],
                    K::ChannelMessage => vec![n(G4, 0.0, 300.0)],
                    K::Mention => vec![n(B4, 0.0, 130.0), n(E5, 70.0, 130.0), n(G5, 140.0, 300.0)],
                },
            ),
        }
    }

    fn spec(self) -> Spec {
        // Pitches: E5, A5, B5, E6 and friends from one pentatonic-ish family, so the chimes sit
        // together pleasantly.
        const E5: f32 = 659.25;
        const B5: f32 = 987.77;
        const A5: f32 = 880.0;
        const D5: f32 = 587.33;
        const C5: f32 = 523.25;
        const G5: f32 = 783.99;
        const E6: f32 = 1318.5;
        const FS5: f32 = 739.99;
        const B4: f32 = 493.88;
        const A4: f32 = 440.0;
        const G6: f32 = 1568.0;
        let n = |freq: f32, start_ms: f32, dur_ms: f32| Note { freq, start_ms, dur_ms };
        match self {
            AlertKind::Join => Spec::bell(0.38, vec![n(E5, 0.0, 130.0), n(B5, 80.0, 150.0)]),
            AlertKind::Leave => Spec::bell(0.34, vec![n(A5, 0.0, 120.0), n(D5, 80.0, 150.0)]),
            AlertKind::Online => Spec::bell(0.14, vec![n(E6, 0.0, 100.0)]),
            AlertKind::Offline => Spec::bell(0.12, vec![n(A5, 0.0, 100.0)]),
            AlertKind::Mute => Spec::blip(0.32, vec![n(FS5, 0.0, 70.0), n(B4, 55.0, 85.0)]),
            AlertKind::Unmute => Spec::blip(0.32, vec![n(B4, 0.0, 70.0), n(FS5, 55.0, 85.0)]),
            AlertKind::Deafen => Spec::blip(0.32, vec![n(A5, 0.0, 70.0), n(E5, 65.0, 70.0), n(A4, 130.0, 100.0)]),
            AlertKind::Undeafen => Spec::blip(0.32, vec![n(A4, 0.0, 70.0), n(E5, 65.0, 70.0), n(A5, 130.0, 100.0)]),
            AlertKind::StreamStart => Spec::bell(0.34, vec![n(C5, 0.0, 100.0), n(E5, 60.0, 100.0), n(G5, 120.0, 120.0)]),
            AlertKind::StreamStop => Spec::bell(0.30, vec![n(G5, 0.0, 100.0), n(E5, 60.0, 100.0), n(C5, 120.0, 120.0)]),
            AlertKind::Viewer => Spec::bell(0.30, vec![n(G5, 0.0, 80.0), n(G5, 105.0, 105.0)]),
            AlertKind::DirectMessage => Spec::bell(0.32, vec![n(B5, 0.0, 90.0), n(E6, 70.0, 140.0)]),
            AlertKind::Ring => Spec::blip(0.30, vec![n(E5, 0.0, 60.0), n(B5, 60.0, 60.0), n(E5, 120.0, 60.0), n(B5, 180.0, 60.0)]),
            AlertKind::ChannelMessage => Spec::bell(0.16, vec![n(G5, 0.0, 110.0)]),
            AlertKind::Mention => Spec::bell(0.34, vec![n(B5, 0.0, 80.0), n(E6, 55.0, 80.0), n(G6, 110.0, 130.0)]),
        }
    }
}

struct Note {
    freq: f32,
    start_ms: f32,
    dur_ms: f32,
}

#[derive(Clone, Copy, PartialEq)]
enum Timbre {
    /// Sine plus a little second partial: a soft bell that rings out.
    Bell,
    /// Triangle-ish (odd partials): a rounder, shorter "pop".
    Blip,
}

struct Spec {
    timbre: Timbre,
    /// Peak of the whole sound as a fraction of full scale.
    peak: f32,
    notes: Vec<Note>,
}

impl Spec {
    fn bell(peak: f32, notes: Vec<Note>) -> Self {
        Self { timbre: Timbre::Bell, peak, notes }
    }
    fn blip(peak: f32, notes: Vec<Note>) -> Self {
        Self { timbre: Timbre::Blip, peak, notes }
    }
}

const SR: f32 = MIX_RATE as f32;
/// Note attack (raised cosine) and release (raised cosine to zero).
const ATTACK_MS: f32 = 5.0;
const RELEASE_MS: f32 = 12.0;
/// Edge fades over the whole mixed sound (belt and braces on top of the per-note ones).
const EDGE_IN_MS: f32 = 2.0;
const EDGE_OUT_MS: f32 = 8.0;

fn ms_to_samples(ms: f32) -> usize {
    (ms * SR / 1000.0).round() as usize
}

fn waveform(timbre: Timbre, phase: f32) -> f32 {
    match timbre {
        Timbre::Bell => phase.sin() + 0.20 * (2.0 * phase).sin() + 0.05 * (3.0 * phase).sin(),
        Timbre::Blip => phase.sin() - (3.0 * phase).sin() / 9.0 + (5.0 * phase).sin() / 25.0,
    }
}

fn synth(spec: &Spec) -> Vec<i16> {
    let total = spec.notes.iter().map(|n| ms_to_samples(n.start_ms + n.dur_ms)).max().unwrap_or(0);
    let mut mono = vec![0f32; total];
    // How far a note decays by its end: bells ring out (-36 dB), blips are punchier (-14 dB).
    let decay_db = if spec.timbre == Timbre::Bell { 36.0f32 } else { 14.0 };
    for note in &spec.notes {
        let start = ms_to_samples(note.start_ms);
        let len = ms_to_samples(note.dur_ms);
        let (att, rel) = (ms_to_samples(ATTACK_MS).min(len / 2), ms_to_samples(RELEASE_MS).min(len / 2));
        let k = decay_db / 20.0 * std::f32::consts::LN_10 / len as f32; // exp(-k*len) = -decay_db
        for i in 0..len {
            let mut env = (-k * i as f32).exp();
            if i < att {
                env *= (std::f32::consts::FRAC_PI_2 * i as f32 / att as f32).sin().powi(2);
            }
            if i >= len - rel {
                env *= (std::f32::consts::FRAC_PI_2 * (len - 1 - i) as f32 / rel as f32).sin().powi(2);
            }
            let phase = TAU * note.freq * i as f32 / SR;
            mono[start + i] += env * waveform(spec.timbre, phase);
        }
    }
    // Edge fades over the whole sound; the very first and last sample are exactly 0.
    let (fin, fout) = (ms_to_samples(EDGE_IN_MS), ms_to_samples(EDGE_OUT_MS));
    for (i, v) in mono.iter_mut().enumerate().take(fin) {
        *v *= (std::f32::consts::FRAC_PI_2 * i as f32 / fin as f32).sin().powi(2);
    }
    for i in 0..fout.min(total) {
        mono[total - 1 - i] *= (std::f32::consts::FRAC_PI_2 * i as f32 / fout as f32).sin().powi(2);
    }
    let max = mono.iter().fold(0f32, |m, s| m.max(s.abs())).max(1e-6);
    let scale = spec.peak * i16::MAX as f32 / max;
    let mut out = Vec::with_capacity(total * MIX_CHANNELS);
    for s in mono {
        let v = (s * scale).round() as i16;
        out.extend(std::iter::repeat_n(v, MIX_CHANNELS));
    }
    out
}

// ---- New palettes (v1.1, card S6) ----------------------------------------------------------------

/// One partial of a [`Voice`]: frequency ratio to the note, level, and how many times faster than
/// the fundamental it dies away (below 1: it lingers).
#[derive(Clone, Copy)]
struct Partial {
    ratio: f32,
    amp: f32,
    decay: f32,
}

const fn p(ratio: f32, amp: f32, decay: f32) -> Partial {
    Partial { ratio, amp, decay }
}

/// The instrument of a palette.
struct Voice {
    partials: &'static [Partial],
    /// Raised-cosine attack and release of every note.
    attack_ms: f32,
    release_ms: f32,
    /// How far the fundamental has decayed (exponentially) by the end of a note.
    decay_db: f32,
    /// Each note starts `glide` (a fraction) below its pitch and settles up into it with this
    /// time constant: the "bloop" of a drop. 0 = no glide.
    glide: f32,
    glide_ms: f32,
    /// Two cascaded one-pole low-passes (12 dB/octave) over the whole sound.
    lowpass_hz: f32,
}

/// Marimba: tuned bar partials 1 : 4 : ~10, the upper two dying fast (the mallet "tok").
const MADEIRA: Voice = Voice {
    partials: &[p(1.0, 1.0, 1.0), p(4.0, 0.30, 4.0), p(9.8, 0.06, 9.0)],
    attack_ms: 3.0,
    release_ms: 20.0,
    decay_db: 30.0,
    glide: 0.0,
    glide_ms: 0.0,
    lowpass_hz: 4500.0,
};

/// Bubbly pop: almost a sine, a little 2nd and 3rd harmonic for body, scooping up into pitch.
const GOTA: Voice = Voice {
    partials: &[p(1.0, 1.0, 1.0), p(2.0, 0.25, 2.0), p(3.0, 0.06, 3.0)],
    attack_ms: 4.0,
    release_ms: 18.0,
    decay_db: 32.0,
    glide: 0.22,
    glide_ms: 14.0,
    lowpass_hz: 3200.0,
};

/// Soft low bell: a subtle sub-octave hum, octave and twelfth, a faint shimmer partial.
const SINO_SUAVE: Voice = Voice {
    partials: &[p(0.5, 0.20, 1.0), p(1.0, 1.0, 1.0), p(2.0, 0.30, 1.5), p(3.0, 0.08, 2.5), p(4.16, 0.03, 4.0)],
    attack_ms: 7.0,
    release_ms: 25.0,
    decay_db: 30.0,
    glide: 0.0,
    glide_ms: 0.0,
    lowpass_hz: 3500.0,
};

/// Reference level of the new palettes' voice-channel alerts, in dB of [`perceived_level_db`]
/// (the current join measures about -17.8 on it: this is ~4 dB more present). The loudest that
/// every voice-channel alert of every palette reaches under [`PATCH_PEAK`] is about -13.5.
const PATCH_LEVEL_DB: f32 = -14.0;
/// Peak ceiling of the new palettes (-3.7 dBFS): loudness comes from body and RMS, not peaks.
const PATCH_PEAK: f32 = 0.65;

struct Tone {
    freq: f32,
    start_ms: f32,
    dur_ms: f32,
}

struct Patch {
    voice: &'static Voice,
    notes: Vec<Tone>,
}

impl Patch {
    fn new(voice: &'static Voice, notes: Vec<Tone>) -> Self {
        Self { voice, notes }
    }
}

fn synth_patch(patch: &Patch, level_offset_db: f32) -> Vec<i16> {
    let v = patch.voice;
    let total = patch.notes.iter().map(|n| ms_to_samples(n.start_ms + n.dur_ms)).max().unwrap_or(0);
    let mut mono = vec![0f32; total];
    for note in &patch.notes {
        let start = ms_to_samples(note.start_ms);
        let len = ms_to_samples(note.dur_ms);
        let (att, rel) = (ms_to_samples(v.attack_ms).min(len / 2), ms_to_samples(v.release_ms).min(len / 2));
        let k = v.decay_db / 20.0 * LN_10 / len as f32;
        let glide_k = if v.glide_ms > 0.0 { 1000.0 / (v.glide_ms * SR) } else { 0.0 };
        let mut phase = 0f64;
        for i in 0..len {
            let mut env = 1.0f32;
            if i < att {
                env *= (FRAC_PI_2 * i as f32 / att as f32).sin().powi(2);
            }
            if i >= len - rel {
                env *= (FRAC_PI_2 * (len - 1 - i) as f32 / rel as f32).sin().powi(2);
            }
            let s: f32 = v.partials.iter().map(|pt| pt.amp * (-k * pt.decay * i as f32).exp() * (pt.ratio as f64 * phase).sin() as f32).sum();
            mono[start + i] += env * s;
            let freq = note.freq * (1.0 - v.glide * (-glide_k * i as f32).exp());
            phase += (TAU * freq / SR) as f64;
        }
    }
    // Gentle 12 dB/octave low-pass: rounds off the top without dulling the "tok".
    let a = 1.0 - (-TAU * v.lowpass_hz / SR).exp();
    for _ in 0..2 {
        let mut y = 0f32;
        for s in mono.iter_mut() {
            y += a * (*s - y);
            *s = y;
        }
    }
    let (fin, fout) = (ms_to_samples(EDGE_IN_MS), ms_to_samples(EDGE_OUT_MS));
    for (i, s) in mono.iter_mut().enumerate().take(fin) {
        *s *= (FRAC_PI_2 * i as f32 / fin as f32).sin().powi(2);
    }
    for i in 0..fout.min(total) {
        mono[total - 1 - i] *= (FRAC_PI_2 * i as f32 / fout as f32).sin().powi(2);
    }
    // Loudness-match on the perceived level, then never above the peak ceiling.
    let max = mono.iter().fold(0f32, |m, s| m.max(s.abs())).max(1e-6);
    let gain = 10f32.powf((PATCH_LEVEL_DB + level_offset_db - level_db(&mono)) / 20.0).min(PATCH_PEAK / max);
    let mut out = Vec::with_capacity(total * MIX_CHANNELS);
    for s in mono {
        let v = (s * gain * i16::MAX as f32).round() as i16;
        out.extend(std::iter::repeat_n(v, MIX_CHANNELS));
    }
    out
}

/// Short-term perceived level of an interleaved stereo [`MIX_RATE`] buffer (first channel), in
/// dB (a full-scale 1 kHz sine reads about -3): the loudest 100 ms window of the B-weighted
/// signal. A cheap stand-in for "how loud does it sound": B-weighting follows the ear at about
/// 70 phon (a notification at a normal volume), which hears 200..300 Hz some 2..3 dB softer than
/// 1 kHz, so deeper sounds need a little more body to be as present.
pub fn perceived_level_db(stereo: &[i16]) -> f32 {
    let mono: Vec<f32> = stereo.iter().step_by(MIX_CHANNELS).map(|s| *s as f32 / i16::MAX as f32).collect();
    level_db(&mono)
}

/// [`perceived_level_db`] of a mono buffer in full-scale units.
fn level_db(mono: &[f32]) -> f32 {
    // B-weighting (IEC 61672 Annex) as one-pole sections: zeros at DC and poles at 20.6 Hz x2 and
    // 158.5 Hz (high-passes), poles at 12194 Hz x2 (low-passes); +0.17 dB so 1 kHz passes at 0.
    let dt = 1.0 / SR;
    let mut x: Vec<f32> = mono.to_vec();
    for fc in [20.6f32, 20.6, 158.5] {
        let rc = 1.0 / (TAU * fc);
        let a = rc / (rc + dt);
        let (mut px, mut py) = (0f32, 0f32);
        for s in x.iter_mut() {
            let y = a * (py + *s - px);
            px = *s;
            py = y;
            *s = y;
        }
    }
    for fc in [12194.0f32, 12194.0] {
        let rc = 1.0 / (TAU * fc);
        let a = dt / (rc + dt);
        let mut y = 0f32;
        for s in x.iter_mut() {
            y += a * (*s - y);
            *s = y;
        }
    }
    let window = ms_to_samples(100.0);
    let hop = ms_to_samples(5.0);
    let mut prefix = Vec::with_capacity(x.len() + 1);
    prefix.push(0f64);
    for s in &x {
        prefix.push(prefix.last().unwrap() + (*s as f64) * (*s as f64));
    }
    let mut best = 0f64;
    let mut at = 0;
    loop {
        let end = (at + window).min(x.len());
        best = best.max(prefix[end] - prefix[at]);
        if end == x.len() {
            break;
        }
        at += hop;
    }
    let rms = (best / window as f64).sqrt().max(1e-9) as f32;
    20.0 * rms.log10() + 0.17
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEW: [Palette; 3] = [Palette::Madeira, Palette::Gota, Palette::SinoSuave];

    fn frames(p: Palette, k: AlertKind) -> Vec<i16> {
        k.samples_in(p).as_chunks::<MIX_CHANNELS>().0.iter().map(|f| f[0]).collect()
    }

    fn peak(p: Palette, k: AlertKind) -> i16 {
        frames(p, k).iter().map(|s| s.saturating_abs()).max().unwrap()
    }

    fn level(p: Palette, k: AlertKind) -> f32 {
        perceived_level_db(&k.samples_in(p))
    }

    fn ms(p: Palette, k: AlertKind) -> f32 {
        frames(p, k).len() as f32 * 1000.0 / SR
    }

    /// Pitch in `from_ms..to_ms` by normalised autocorrelation: the shortest period whose
    /// correlation is within 15 % of the best one (so an overtone or a sub-octave hum does not
    /// fool it the way zero crossings would).
    fn pitch(p: Palette, k: AlertKind, from_ms: f32, to_ms: f32) -> f32 {
        let f: Vec<f32> = frames(p, k).iter().map(|s| *s as f32).collect();
        let (a, b) = (ms_to_samples(from_ms), ms_to_samples(to_ms).min(f.len()));
        let (min_lag, max_lag) = ((SR / 1600.0) as usize, (SR / 90.0) as usize);
        let n = b - a;
        let r: Vec<f32> = (0..=max_lag)
            .map(|lag| {
                if lag < min_lag || lag >= n {
                    return f32::MIN;
                }
                let (x, y) = (&f[a..b - lag], &f[a + lag..b]);
                let xy: f32 = x.iter().zip(y).map(|(x, y)| x * y).sum();
                let xx: f32 = x.iter().map(|x| x * x).sum();
                let yy: f32 = y.iter().map(|y| y * y).sum();
                xy / (xx * yy).sqrt().max(1e-9)
            })
            .collect();
        let best = r.iter().copied().fold(f32::MIN, f32::max);
        let lag = (min_lag + 1..max_lag)
            .find(|&l| r[l] >= 0.85 * best && r[l] >= r[l - 1] && r[l] >= r[l + 1])
            .expect("a period");
        SR / lag as f32
    }

    #[test]
    fn the_chosen_palette_plays_from_the_next_sound_and_each_keeps_its_cache() {
        let _guard = palette_test_guard();
        assert_eq!(Palette::default(), Palette::SinoSuave);
        assert_eq!(palette(), Palette::SinoSuave, "nothing chosen yet: Sino suave");
        for k in AlertKind::ALL {
            assert!(Arc::ptr_eq(&k.samples(), &k.samples_in(Palette::SinoSuave)), "{k:?}");
        }
        let before = AlertKind::Join.samples();
        for p in Palette::ALL {
            set_palette(p);
            assert_eq!(palette(), p);
            // The very next call plays the new palette, from its own cached bank (no synthesis
            // per play: the same buffer every time).
            let a = AlertKind::Join.samples();
            assert!(Arc::ptr_eq(&a, &AlertKind::Join.samples_in(p)), "{p:?}");
            assert!(Arc::ptr_eq(&a, &AlertKind::Join.samples()), "{p:?} cached");
            assert_eq!(Arc::ptr_eq(&a, &before), p == Palette::SinoSuave, "{p:?}");
        }
        // Switching back finds the first bank again, untouched.
        set_palette(Palette::SinoSuave);
        assert!(Arc::ptr_eq(&AlertKind::Join.samples(), &before));
    }

    #[test]
    fn palettes_have_stable_keys_and_all_is_in_declaration_order() {
        for (i, p) in Palette::ALL.into_iter().enumerate() {
            assert_eq!(p as usize, i, "{p:?}: CURRENT stores discriminants");
            assert_eq!(Palette::from_key(p.key()), Some(p));
        }
        let keys: Vec<_> = Palette::ALL.iter().map(|p| p.key()).collect();
        assert_eq!(keys, ["atual", "madeira", "gota", "sino_suave"]);
        assert_eq!(Palette::from_key("Sino suave"), None);
        assert_eq!(Palette::from_key(""), None);
    }

    #[test]
    fn lengths_are_short_and_stereo() {
        for p in Palette::ALL {
            let bounds = if p == Palette::Atual { 80.0..=250.0 } else { 150.0..=450.0 };
            for k in AlertKind::ALL {
                let s = k.samples_in(p);
                assert_eq!(s.len() % MIX_CHANNELS, 0, "{p:?} {k:?}");
                let ms = ms(p, k);
                assert!(bounds.contains(&ms), "{p:?} {k:?} lasts {ms} ms");
                // Mono content duplicated into both channels.
                assert!(s.as_chunks::<2>().0.iter().all(|f| f[0] == f[1]), "{p:?} {k:?} L != R");
            }
        }
    }

    #[test]
    fn peaks_are_audible_but_never_near_clipping() {
        for p in Palette::ALL {
            // The current chimes stay under -6 dBFS; the new ones under -3 dBFS (in practice
            // under PATCH_PEAK).
            let ceiling = if p == Palette::Atual { 0.5 } else { 0.708 };
            for k in AlertKind::ALL {
                let pk = peak(p, k);
                assert!(pk > 3_000, "{p:?} {k:?} peak {pk} too quiet to hear");
                assert!((pk as f32) < i16::MAX as f32 * ceiling, "{p:?} {k:?} peak {pk} too loud");
            }
        }
        // Even every alert stacked on top of each other stays inside i16 when summed in i32 and
        // the mixer saturates; a realistic pair of the current chimes never exceeds full scale.
        let p = Palette::Atual;
        assert!((peak(p, AlertKind::Join) as i32 + peak(p, AlertKind::Unmute) as i32) < i16::MAX as i32);
    }

    #[test]
    fn online_and_offline_are_quieter_than_the_voice_alerts() {
        for p in Palette::ALL {
            for quiet in [AlertKind::Online, AlertKind::Offline] {
                for loud in [AlertKind::Join, AlertKind::Leave, AlertKind::Mute, AlertKind::Unmute, AlertKind::Deafen, AlertKind::Undeafen] {
                    assert!(peak(p, quiet) * 2 < peak(p, loud), "{p:?}: {quiet:?} vs {loud:?}");
                    assert!(level(p, quiet) + 6.0 < level(p, loud), "{p:?}: {quiet:?} vs {loud:?}");
                }
            }
        }
    }

    #[test]
    fn chat_sounds_are_ordered_soft_to_bright() {
        use AlertKind::{ChannelMessage, DirectMessage, Mention};
        for p in Palette::ALL {
            // A channel message is the softest, a direct message louder, a mention the brightest.
            let (c, d, m) = (peak(p, ChannelMessage), peak(p, DirectMessage), peak(p, Mention));
            assert!(c * 3 / 2 < d && c < m, "{p:?}: channel {c}, dm {d}, mention {m}");
            let (lc, ld, lm) = (level(p, ChannelMessage), level(p, DirectMessage), level(p, Mention));
            assert!(lc + 3.0 < ld && ld <= lm + 0.5, "{p:?}: levels channel {lc}, dm {ld}, mention {lm}");
            // The last note of a mention is higher than the last note of a DM.
            let top = |k: AlertKind| pitch(p, k, ms(p, k) - 110.0, ms(p, k) - 40.0);
            assert!(top(Mention) > top(DirectMessage) * 1.05, "{p:?}: the mention is brighter than a DM");
            assert!(frames(p, Mention).len() > frames(p, ChannelMessage).len(), "{p:?}");
        }
    }

    #[test]
    fn edges_fade_from_and_to_silence_without_clicks() {
        let half_ms = ms_to_samples(0.5);
        for p in Palette::ALL {
            for k in AlertKind::ALL {
                let f = frames(p, k);
                let pk = peak(p, k) as f32;
                assert_eq!(f[0], 0, "{p:?} {k:?} starts at zero");
                assert_eq!(*f.last().unwrap(), 0, "{p:?} {k:?} ends at zero");
                // The first and last half millisecond are still nearly silent.
                assert!(f[..half_ms].iter().all(|s| (s.abs() as f32) < pk * 0.05), "{p:?} {k:?} fade-in too abrupt");
                assert!(f[f.len() - half_ms..].iter().all(|s| (s.abs() as f32) < pk * 0.05), "{p:?} {k:?} fade-out too abrupt");
                // No jump between neighbouring samples anywhere (a click would show as one).
                let max_step = f.windows(2).map(|w| (w[1] as i32 - w[0] as i32).abs()).max().unwrap() as f32;
                assert!(max_step < pk * 0.3, "{p:?} {k:?} has a {max_step} step at peak {pk}");
            }
        }
    }

    #[test]
    fn rising_and_falling_shapes_are_recognisable() {
        for p in Palette::ALL {
            let ratio = |k| pitch(p, k, 5.0, 35.0) / pitch(p, k, ms(p, k) - 45.0, ms(p, k) - 15.0);
            for k in [AlertKind::Join, AlertKind::Unmute, AlertKind::Undeafen, AlertKind::StreamStart, AlertKind::Mention] {
                assert!(ratio(k) < 0.8, "{p:?} {k:?} should rise (start/end pitch = {})", ratio(k));
            }
            for k in [AlertKind::Leave, AlertKind::Mute, AlertKind::Deafen, AlertKind::StreamStop] {
                assert!(ratio(k) > 1.25, "{p:?} {k:?} should fall (start/end pitch = {})", ratio(k));
            }
        }
    }

    #[test]
    fn new_palettes_sit_lower_and_are_more_present_than_the_current_one() {
        for p in NEW {
            for k in AlertKind::ALL {
                // Every first note is in the 3rd..5th octave (the current ones are 440..1570 Hz).
                let f0 = pitch(p, k, 5.0, 35.0);
                assert!((200.0..=820.0).contains(&f0), "{p:?} {k:?} starts at {f0} Hz");
                // Louder to the ear than the current chime.
                assert!(level(p, k) > level(Palette::Atual, k) + 2.0, "{p:?} {k:?}: {} vs {} dB", level(p, k), level(Palette::Atual, k));
            }
        }
    }

    #[test]
    fn new_palettes_are_loudness_matched() {
        // The same alert in two new palettes sounds equally loud (within 1.5 dB), and every
        // alert lands on its target unless the peak ceiling held it back.
        for k in AlertKind::ALL {
            let levels: Vec<f32> = NEW.iter().map(|p| level(*p, k)).collect();
            let (lo, hi) = levels.iter().fold((f32::MAX, f32::MIN), |(lo, hi), l| (lo.min(*l), hi.max(*l)));
            assert!(hi - lo < 1.5, "{k:?} levels {levels:?}");
            for p in NEW {
                let target = PATCH_LEVEL_DB + k.level_offset_db();
                let capped = peak(p, k) as f32 >= i16::MAX as f32 * PATCH_PEAK * 0.99;
                assert!((level(p, k) - target).abs() < 0.3 || (capped && level(p, k) < target), "{p:?} {k:?}: {} vs target {target}", level(p, k));
            }
        }
    }

    #[test]
    fn every_alert_sounds_different() {
        for p in Palette::ALL {
            for (i, a) in AlertKind::ALL.iter().enumerate() {
                for b in &AlertKind::ALL[i + 1..] {
                    assert_ne!(a.samples_in(p), b.samples_in(p), "{p:?}: {a:?} and {b:?} are identical");
                }
            }
            // Mute/unmute and deafen/undeafen differ in note count so they are never confused.
            assert!(frames(p, AlertKind::Deafen).len() > frames(p, AlertKind::Mute).len(), "{p:?}");
        }
    }

    #[test]
    fn buffers_are_computed_once_and_shared() {
        for p in Palette::ALL {
            let a = AlertKind::Join.samples_in(p);
            let b = AlertKind::Join.samples_in(p);
            assert!(Arc::ptr_eq(&a, &b));
            // One bank per palette: never another palette's buffer.
            for q in Palette::ALL.into_iter().filter(|q| *q != p) {
                assert!(!Arc::ptr_eq(&a, &AlertKind::Join.samples_in(q)), "{p:?} vs {q:?}");
            }
        }
    }

    #[test]
    fn names_are_unique() {
        let mut names: Vec<_> = AlertKind::ALL.iter().map(|k| k.name()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), AlertKind::ALL.len());
        let mut labels: Vec<_> = Palette::ALL.iter().map(|p| p.label()).collect();
        labels.sort();
        labels.dedup();
        assert_eq!(labels.len(), Palette::ALL.len());
    }
}
