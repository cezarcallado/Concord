//! Playback mixer: every remote audio track (voice + watched screenshare audio) pushes 48 kHz
//! stereo PCM into its own small jitter FIFO; the output device callback pulls and sums them.
//!
//! Lock hold times are tiny (a memcpy of 10 ms per push / per pull), so a plain mutex is fine
//! even on the realtime audio thread at this scale (~15 sources max).
//!
//! Soundboard clips (see [`Mixer::play_sound`]) are mixed inside [`Mixer::mix`], with the voices,
//! so deafening silences them with everybody else, and they reach the echo canceller's reference
//! with the rest of the playback. Alerts ([`Mixer::play_ui`]) are the opposite: they survive
//! deafen.
//!
//! Per-source volume / mute ("what *I* hear") lives in [`Gain`] cells made of atomics. The UI
//! flips them from any thread without touching the mixer lock; the audio callback reads them
//! with relaxed loads and ramps from the previous gain to the new one across the block, so a
//! change never clicks. Gains above 1.0 can exceed `i16`: sums are done in `i32` and saturated
//! once at the end, so they clip, never wrap.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

pub const MIX_RATE: u32 = 48_000;
pub const MIX_CHANNELS: usize = 2;
/// Samples (interleaved) in 10 ms of mixer audio.
pub const MIX_10MS: usize = (MIX_RATE as usize / 100) * MIX_CHANNELS;

/// A source starts (and restarts after an underrun) only once this much audio is queued, so a
/// late packet produces one short gap instead of constant crackle.
const PRIME_SAMPLES: usize = 3 * MIX_10MS; // 30 ms
/// Hard cap per source; older audio is dropped beyond it so latency stays bounded.
const MAX_SAMPLES: usize = 20 * MIX_10MS; // 200 ms

pub type SourceId = u64;

/// Highest per-source gain (200 %).
pub const MAX_GAIN: f32 = 2.0;

/// Volume and mute of one audio path. All access is a relaxed atomic load/store, so it can be
/// shared between the UI and the realtime mixer without a lock.
#[derive(Debug)]
pub struct Gain {
    level: AtomicU32,
    muted: AtomicBool,
}

impl Default for Gain {
    fn default() -> Self {
        Self::new(1.0, false)
    }
}

fn clamp_gain(g: f32) -> f32 {
    if g.is_nan() {
        1.0
    } else {
        g.clamp(0.0, MAX_GAIN)
    }
}

impl Gain {
    pub fn new(level: f32, muted: bool) -> Self {
        Self { level: AtomicU32::new(clamp_gain(level).to_bits()), muted: AtomicBool::new(muted) }
    }

    /// Set the volume (clamped to `0.0..=MAX_GAIN`; NaN is ignored).
    pub fn set_level(&self, level: f32) {
        if !level.is_nan() {
            self.level.store(clamp_gain(level).to_bits(), Ordering::Relaxed);
        }
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    /// Gain actually applied: 0 when muted, else the volume.
    pub fn effective(&self) -> f32 {
        if self.muted() {
            0.0
        } else {
            self.level()
        }
    }
}

/// What controls one mixer source: its own [`Gain`] plus an optional shared "silence" switch
/// (the "mute everyone" toggle) that forces it to 0 without touching its own settings.
#[derive(Debug, Clone, Default)]
pub struct SourceCtl {
    pub gain: Arc<Gain>,
    pub silence: Option<Arc<AtomicBool>>,
}

impl SourceCtl {
    pub fn new(gain: Arc<Gain>, silence: Option<Arc<AtomicBool>>) -> Self {
        Self { gain, silence }
    }

    /// Gain to reach by the end of the next block.
    pub fn target(&self) -> f32 {
        if self.silence.as_ref().is_some_and(|s| s.load(Ordering::Relaxed)) {
            0.0
        } else {
            self.gain.effective()
        }
    }
}

struct Source {
    buf: VecDeque<i16>,
    primed: bool,
    ctl: SourceCtl,
    /// Gain at the end of the previous block (start of the next ramp).
    applied: f32,
    underruns: u64,
    overflow_drops: u64,
}

/// One alert sound being played out of a precomputed buffer (see [`Mixer::play_ui`]).
struct UiVoice {
    samples: Arc<[i16]>,
    pos: usize,
    gain: f32,
    /// A soundboard preview ("Ouvir só aqui"), which "Parar" stops; alerts are never stopped.
    preview: bool,
    /// Frames left of a stop's fade-out.
    fade: Option<usize>,
}

/// Who started a soundboard clip: the player's user id (the hub's `SoundPlayed::by`), so a stop
/// can name whose sounds end.
pub type SoundTag = i64;
/// The tag of a clip nobody is known to have started.
pub const NO_TAG: SoundTag = i64::MIN;
/// A stopped clip fades out over this many frames (5 ms at 48 kHz) instead of being cut: no click.
pub const STOP_FADE_FRAMES: usize = 240;

/// Gain of frame `i` of a block for a fade-out with `left` frames to go (1.0 down to 1/FADE).
fn fade_gain(left: usize, i: usize) -> f32 {
    left.saturating_sub(i) as f32 / STOP_FADE_FRAMES as f32
}

/// At most this many alert sounds overlap; further ones are dropped (a flood cannot pile up).
pub const MAX_UI_VOICES: usize = 6;

/// One soundboard clip being played: native 48 kHz samples, mono or stereo (interleaved).
struct SoundVoice {
    samples: Arc<[i16]>,
    channels: usize,
    /// Next frame to play.
    pos: usize,
    tag: SoundTag,
    /// Frames left of a stop's fade-out ([`Mixer::stop_sounds`]).
    fade: Option<usize>,
}

impl SoundVoice {
    fn frames(&self) -> usize {
        self.samples.len() / self.channels
    }
}

/// At most this many soundboard clips overlap; further ones are dropped.
pub const MAX_SOUND_VOICES: usize = 8;

#[derive(Default)]
struct Inner {
    sources: HashMap<SourceId, Source>,
    next_id: SourceId,
    acc: Vec<i32>,
    ui: Vec<UiVoice>,
    sounds: Vec<SoundVoice>,
    /// Soundboard gain at the end of the previous block (start of the next ramp).
    sound_applied: f32,
}

#[derive(Default)]
pub struct Mixer {
    inner: Mutex<Inner>,
    /// Volume and mute of the soundboard, as the user set them ("what *I* hear").
    sound_gain: Gain,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceStats {
    pub queued_samples: usize,
    pub underruns: u64,
    pub overflow_drops: u64,
}

impl Mixer {
    pub fn new() -> Self {
        Self::default()
    }

    /// A source at unity gain with nothing controlling it.
    pub fn add_source(&self) -> SourceId {
        self.add_source_with(SourceCtl::default())
    }

    /// A source whose volume / mute follow `ctl` (starting at its current value, no ramp).
    pub fn add_source_with(&self, ctl: SourceCtl) -> SourceId {
        let mut g = self.inner.lock();
        let id = g.next_id;
        g.next_id += 1;
        let applied = ctl.target();
        g.sources.insert(
            id,
            Source { buf: VecDeque::with_capacity(MAX_SAMPLES), primed: false, ctl, applied, underruns: 0, overflow_drops: 0 },
        );
        id
    }

    pub fn remove_source(&self, id: SourceId) {
        self.inner.lock().sources.remove(&id);
    }

    pub fn source_count(&self) -> usize {
        self.inner.lock().sources.len()
    }

    pub fn source_stats(&self, id: SourceId) -> Option<SourceStats> {
        self.inner.lock().sources.get(&id).map(|s| SourceStats {
            queued_samples: s.buf.len(),
            underruns: s.underruns,
            overflow_drops: s.overflow_drops,
        })
    }

    /// Queue interleaved stereo samples for a source. Unknown ids are ignored.
    pub fn push(&self, id: SourceId, samples: &[i16]) {
        let mut g = self.inner.lock();
        let Some(s) = g.sources.get_mut(&id) else { return };
        s.buf.extend(samples.iter().copied());
        if s.buf.len() > MAX_SAMPLES {
            let excess = s.buf.len() - MAX_SAMPLES;
            // Keep frame alignment (stereo pairs).
            let excess = excess + (excess % MIX_CHANNELS);
            s.buf.drain(..excess.min(s.buf.len()));
            s.overflow_drops += 1;
        }
    }

    /// Start an alert sound ("ui sound") at `gain`. It plays from the start of `samples`
    /// (interleaved stereo, 48 kHz) the next time [`Mixer::mix_ui`] runs and is dropped when it
    /// ends. Returns false (and plays nothing) when [`MAX_UI_VOICES`] are already playing.
    pub fn play_ui(&self, samples: Arc<[i16]>, gain: f32) -> bool {
        let mut g = self.inner.lock();
        if g.ui.len() >= MAX_UI_VOICES || samples.is_empty() {
            return false;
        }
        g.ui.push(UiVoice { samples, pos: 0, gain: clamp_gain(gain), preview: false, fade: None });
        true
    }

    /// A soundboard preview: an alert-like sound (it survives deafen) that
    /// [`Mixer::stop_previews`] can stop.
    pub fn play_preview(&self, samples: Arc<[i16]>, gain: f32) -> bool {
        let mut g = self.inner.lock();
        if g.ui.len() >= MAX_UI_VOICES || samples.is_empty() {
            return false;
        }
        g.ui.push(UiVoice { samples, pos: 0, gain: clamp_gain(gain), preview: true, fade: None });
        true
    }

    /// Fade out every playing preview (5 ms). Returns how many were playing.
    pub fn stop_previews(&self) -> usize {
        let mut g = self.inner.lock();
        let mut n = 0;
        for v in g.ui.iter_mut().filter(|v| v.preview && v.fade.is_none()) {
            v.fade = Some(STOP_FADE_FRAMES);
            n += 1;
        }
        n
    }

    /// The soundboard's volume and mute (flipped from any thread; the mix ramps to the change).
    pub fn sound_gain(&self) -> &Gain {
        &self.sound_gain
    }

    /// Start a soundboard clip: `samples` are interleaved 48 kHz frames of `channels` (1 or 2).
    /// It plays from its start inside the next [`Mixer::mix`] calls, at the soundboard gain, and
    /// is dropped when it ends. Returns false (and plays nothing) for an empty clip, another
    /// channel count, or when [`MAX_SOUND_VOICES`] are already playing.
    pub fn play_sound(&self, samples: Arc<[i16]>, channels: usize) -> bool {
        self.play_sound_by(samples, channels, NO_TAG)
    }

    /// [`Mixer::play_sound`] for a clip started by `tag` (see [`Mixer::stop_sounds`]).
    pub fn play_sound_by(&self, samples: Arc<[i16]>, channels: usize, tag: SoundTag) -> bool {
        let mut g = self.inner.lock();
        if !(1..=2).contains(&channels) || samples.len() < channels || !samples.len().is_multiple_of(channels) || g.sounds.len() >= MAX_SOUND_VOICES {
            return false;
        }
        if g.sounds.is_empty() {
            // The first clip starts at the current gain (no fade-in from silence).
            g.sound_applied = self.sound_gain.effective();
        }
        g.sounds.push(SoundVoice { samples, channels, pos: 0, tag, fade: None });
        true
    }

    /// Stop the soundboard clips of `tag` (`None`: all of them): each fades out over
    /// [`STOP_FADE_FRAMES`] in the next mix and is gone after it. Returns how many were playing.
    pub fn stop_sounds(&self, tag: Option<SoundTag>) -> usize {
        let mut g = self.inner.lock();
        let mut n = 0;
        for v in g.sounds.iter_mut().filter(|v| v.fade.is_none() && tag.is_none_or(|t| v.tag == t)) {
            v.fade = Some(STOP_FADE_FRAMES);
            n += 1;
        }
        n
    }

    /// Stop (5 ms fade) the newest playing copy of `samples` that `tag` started, and only it (a
    /// play the hub refused, while an older sound of the same player goes on). Returns whether
    /// one was playing.
    pub fn stop_clip(&self, samples: &Arc<[i16]>, tag: SoundTag) -> bool {
        let mut g = self.inner.lock();
        match g.sounds.iter_mut().filter(|v| v.tag == tag && v.fade.is_none() && Arc::ptr_eq(&v.samples, samples)).min_by_key(|v| v.pos) {
            Some(v) => {
                v.fade = Some(STOP_FADE_FRAMES);
                true
            }
            None => false,
        }
    }

    /// How much longer the clips of `tag` play (the longest; `None`: none is playing or they
    /// are all being stopped).
    pub fn sound_left(&self, tag: SoundTag) -> Option<std::time::Duration> {
        let g = self.inner.lock();
        let frames = g.sounds.iter().filter(|v| v.tag == tag && v.fade.is_none()).map(|v| v.frames() - v.pos).max()?;
        Some(std::time::Duration::from_millis((frames as u64 * 1000).div_ceil(MIX_RATE as u64)))
    }

    /// Soundboard clips still playing.
    pub fn sounds_active(&self) -> usize {
        self.inner.lock().sounds.len()
    }

    /// Alert sounds still playing.
    pub fn ui_active(&self) -> usize {
        self.inner.lock().ui.len()
    }

    /// Forget every alert sound (they never started, e.g. because the device failed to open).
    pub fn clear_ui(&self) {
        self.inner.lock().ui.clear();
    }

    /// Add the playing alert sounds to `out` (the voice mix, interleaved stereo), saturating at
    /// i16, and advance them. Kept apart from [`Mixer::mix`] so the caller can silence voices
    /// (deafen) and still let the user hear their own alerts. Returns true when anything was
    /// added. With nothing playing this is a lock and an empty check.
    pub fn mix_ui(&self, out: &mut [i16]) -> bool {
        let mut g = self.inner.lock();
        if g.ui.is_empty() {
            return false;
        }
        for v in g.ui.iter_mut() {
            let mut n = out.len().min(v.samples.len() - v.pos);
            if let Some(left) = v.fade {
                n = n.min(left * MIX_CHANNELS);
            }
            for (k, (o, s)) in out[..n].iter_mut().zip(&v.samples[v.pos..v.pos + n]).enumerate() {
                let gain = match v.fade {
                    Some(left) => v.gain * fade_gain(left, k / MIX_CHANNELS),
                    None => v.gain,
                };
                let mixed = *o as i32 + (*s as f32 * gain).round() as i32;
                *o = mixed.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            }
            v.pos += n;
            if let Some(left) = v.fade.as_mut() {
                *left = left.saturating_sub(n / MIX_CHANNELS);
                if *left == 0 {
                    v.pos = v.samples.len();
                }
            }
        }
        g.ui.retain(|v| v.pos < v.samples.len());
        true
    }

    /// Fill `out` (interleaved stereo) with the sum of all primed sources (each scaled by its
    /// gain), saturating at i16. Sources are always drained, so muting playback (deafen) or a
    /// single source never makes queues grow. Alert sounds are not included: see
    /// [`Mixer::mix_ui`].
    pub fn mix(&self, out: &mut [i16]) {
        let mut g = self.inner.lock();
        let Inner { sources, acc, sounds, sound_applied, .. } = &mut *g;
        acc.clear();
        acc.resize(out.len(), 0);
        for s in sources.values_mut() {
            let target = s.ctl.target();
            if !s.primed {
                if s.buf.len() < PRIME_SAMPLES {
                    s.applied = target;
                    continue;
                }
                s.primed = true;
            }
            let n = out.len().min(s.buf.len());
            let (from, to) = (s.applied, target);
            s.applied = to;
            if from == to {
                if to == 1.0 {
                    for (a, v) in acc.iter_mut().zip(s.buf.drain(..n)) {
                        *a += v as i32;
                    }
                } else if to == 0.0 {
                    s.buf.drain(..n);
                } else {
                    for (a, v) in acc.iter_mut().zip(s.buf.drain(..n)) {
                        *a += (v as f32 * to).round() as i32;
                    }
                }
            } else {
                // Linear ramp over this block (per frame, so both channels share a gain).
                let frames = (n / MIX_CHANNELS).max(1) as f32;
                let step = (to - from) / frames;
                for (i, (a, v)) in acc.iter_mut().zip(s.buf.drain(..n)).enumerate() {
                    let gain = from + step * (i / MIX_CHANNELS) as f32;
                    *a += (v as f32 * gain).round() as i32;
                }
            }
            if n < out.len() {
                s.primed = false;
                s.underruns += 1;
            }
        }
        if !sounds.is_empty() {
            let (from, to) = (*sound_applied, self.sound_gain.effective());
            *sound_applied = to;
            let frames = out.len() / MIX_CHANNELS;
            let step = (to - from) / frames.max(1) as f32;
            for v in sounds.iter_mut() {
                let mut n = frames.min(v.frames() - v.pos);
                if let Some(left) = v.fade {
                    n = n.min(left);
                }
                for i in 0..n {
                    let mut gain = if from == to { to } else { from + step * i as f32 };
                    if let Some(left) = v.fade {
                        gain *= fade_gain(left, i);
                    }
                    let at = (v.pos + i) * v.channels;
                    let (l, r) = (v.samples[at] as f32, v.samples[at + v.channels - 1] as f32);
                    acc[i * MIX_CHANNELS] += (l * gain).round() as i32;
                    acc[i * MIX_CHANNELS + 1] += (r * gain).round() as i32;
                }
                v.pos += n;
                if let Some(left) = v.fade.as_mut() {
                    *left -= n;
                    if *left == 0 {
                        v.pos = v.frames();
                    }
                }
            }
            sounds.retain(|v| v.pos < v.frames());
        } else {
            *sound_applied = self.sound_gain.effective();
        }
        for (o, a) in out.iter_mut().zip(acc.iter()) {
            *o = (*a).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled(v: i16, n: usize) -> Vec<i16> {
        vec![v; n]
    }

    #[test]
    fn silent_until_primed() {
        let m = Mixer::new();
        let id = m.add_source();
        m.push(id, &filled(1000, MIX_10MS)); // 10 ms < 30 ms prime
        let mut out = vec![7i16; MIX_10MS];
        m.mix(&mut out);
        assert!(out.iter().all(|&s| s == 0));
        m.push(id, &filled(1000, 2 * MIX_10MS));
        m.mix(&mut out);
        assert!(out.iter().all(|&s| s == 1000));
    }

    #[test]
    fn sums_sources_and_saturates() {
        let m = Mixer::new();
        let a = m.add_source();
        let b = m.add_source();
        m.push(a, &filled(20_000, PRIME_SAMPLES));
        m.push(b, &filled(20_000, PRIME_SAMPLES));
        let mut out = vec![0i16; MIX_10MS];
        m.mix(&mut out);
        assert!(out.iter().all(|&s| s == i16::MAX));

        let c = m.add_source();
        let d = m.add_source();
        m.push(c, &filled(100, PRIME_SAMPLES));
        m.push(d, &filled(-30, PRIME_SAMPLES));
        m.remove_source(a);
        m.remove_source(b);
        m.mix(&mut out);
        assert!(out.iter().all(|&s| s == 70));
    }

    #[test]
    fn underrun_pads_with_silence_and_reprimes() {
        let m = Mixer::new();
        let id = m.add_source();
        m.push(id, &filled(5, PRIME_SAMPLES));
        let mut out = vec![0i16; 2 * PRIME_SAMPLES];
        m.mix(&mut out);
        assert!(out[..PRIME_SAMPLES].iter().all(|&s| s == 5));
        assert!(out[PRIME_SAMPLES..].iter().all(|&s| s == 0));
        assert_eq!(m.source_stats(id).unwrap().underruns, 1);
        // Not primed again until 30 ms is queued.
        m.push(id, &filled(5, MIX_10MS));
        let mut out = vec![1i16; MIX_10MS];
        m.mix(&mut out);
        assert!(out.iter().all(|&s| s == 0));
    }

    #[test]
    fn overflow_keeps_newest_audio_bounded() {
        let m = Mixer::new();
        let id = m.add_source();
        m.push(id, &filled(1, MAX_SAMPLES));
        m.push(id, &filled(2, MIX_10MS));
        let st = m.source_stats(id).unwrap();
        assert_eq!(st.queued_samples, MAX_SAMPLES);
        assert_eq!(st.overflow_drops, 1);
        let mut out = vec![0i16; MAX_SAMPLES];
        m.mix(&mut out);
        assert_eq!(out[MAX_SAMPLES - 1], 2);
        assert_eq!(out[0], 1);
    }

    fn ctl(level: f32, muted: bool) -> (SourceCtl, Arc<Gain>) {
        let g = Arc::new(Gain::new(level, muted));
        (SourceCtl::new(g.clone(), None), g)
    }

    /// Mix one primed block of constant `v` from a source with the given controls.
    fn one_block(ctl: SourceCtl, v: i16) -> Vec<i16> {
        let m = Mixer::new();
        let id = m.add_source_with(ctl);
        m.push(id, &filled(v, PRIME_SAMPLES));
        let mut out = vec![0i16; MIX_10MS];
        m.mix(&mut out);
        out
    }

    #[test]
    fn gain_is_applied() {
        assert!(one_block(ctl(0.5, false).0, 1000).iter().all(|&s| s == 500));
    }

    #[test]
    fn gain_0_100_200() {
        assert!(one_block(ctl(0.0, false).0, 1000).iter().all(|&s| s == 0));
        assert!(one_block(ctl(1.0, false).0, 1000).iter().all(|&s| s == 1000));
        assert!(one_block(ctl(2.0, false).0, 1000).iter().all(|&s| s == 2000));
        // Above 200 % is clamped to 200 %.
        assert!(one_block(ctl(9.0, false).0, 1000).iter().all(|&s| s == 2000));
    }

    #[test]
    fn two_hundred_percent_clips_instead_of_wrapping() {
        let loud = one_block(ctl(2.0, false).0, 20_000);
        assert!(loud.iter().all(|&s| s == i16::MAX), "{:?}", &loud[..4]);
        let loud_neg = one_block(ctl(2.0, false).0, -20_000);
        assert!(loud_neg.iter().all(|&s| s == i16::MIN));
        // The sum of two boosted sources saturates too.
        let m = Mixer::new();
        let a = m.add_source_with(ctl(2.0, false).0);
        let b = m.add_source_with(ctl(2.0, false).0);
        m.push(a, &filled(30_000, PRIME_SAMPLES));
        m.push(b, &filled(30_000, PRIME_SAMPLES));
        let mut out = vec![0i16; MIX_10MS];
        m.mix(&mut out);
        assert!(out.iter().all(|&s| s == i16::MAX));
    }

    #[test]
    fn mute_silences_but_keeps_draining() {
        let m = Mixer::new();
        let (c, g) = ctl(0.5, true);
        let id = m.add_source_with(c);
        let other = m.add_source();
        m.push(id, &filled(1000, 3 * MIX_10MS));
        m.push(other, &filled(7, 3 * MIX_10MS));
        let mut out = vec![0i16; MIX_10MS];
        m.mix(&mut out);
        assert!(out.iter().all(|&s| s == 7), "muted source must not be heard");
        assert_eq!(m.source_stats(id).unwrap().queued_samples, 2 * MIX_10MS, "muted source is still drained");
        // Unmuting restores the volume that was set before (50 %), after a short ramp.
        g.set_muted(false);
        m.mix(&mut out); // ramp block
        assert!(out[0] < out[MIX_10MS - 1]);
        m.mix(&mut out);
        assert!(out.iter().all(|&s| s == 507), "{:?}", &out[..2]);
    }

    #[test]
    fn silence_switch_overrides_and_restores() {
        let silence = Arc::new(AtomicBool::new(false));
        let g = Arc::new(Gain::new(1.5, false));
        let m = Mixer::new();
        let id = m.add_source_with(SourceCtl::new(g.clone(), Some(silence.clone())));
        let mut out = vec![0i16; MIX_10MS];
        // Feed 4 blocks, mix 3 (ramp + 2 settled), then drain the rest.
        let mut settled = |expect: i16| {
            m.push(id, &filled(1000, 4 * MIX_10MS));
            m.mix(&mut out);
            m.mix(&mut out);
            m.mix(&mut out);
            assert!(out.iter().all(|&s| s == expect), "expected {expect}, got {:?}", &out[..2]);
            m.mix(&mut out);
        };
        settled(1500);
        silence.store(true, Ordering::Relaxed);
        settled(0);
        // The per-source settings were untouched by the switch: back to 150 %.
        silence.store(false, Ordering::Relaxed);
        assert_eq!(g.level(), 1.5);
        assert!(!g.muted());
        settled(1500);
    }

    #[test]
    fn gain_changes_ramp_without_a_jump() {
        let m = Mixer::new();
        let (c, g) = ctl(1.0, false);
        let id = m.add_source_with(c);
        m.push(id, &filled(10_000, 4 * MIX_10MS));
        let mut out = vec![0i16; MIX_10MS];
        m.mix(&mut out);
        assert!(out.iter().all(|&s| s == 10_000));
        g.set_muted(true);
        m.mix(&mut out);
        // Monotonic fade from ~full to ~0 inside the block, no click at the edges.
        assert!(out[0] >= 9_900, "{}", out[0]);
        assert!(out[MIX_10MS - 1] <= 50, "{}", out[MIX_10MS - 1]);
        assert!(out.windows(2).all(|w| w[1] <= w[0]));
        m.mix(&mut out);
        assert!(out.iter().all(|&s| s == 0));
    }

    fn ui_buf(v: i16, frames: usize) -> Arc<[i16]> {
        Arc::from(vec![v; frames * MIX_CHANNELS])
    }

    #[test]
    fn ui_sounds_play_through_to_the_end_and_are_removed() {
        let m = Mixer::new();
        let mut out = vec![0i16; MIX_10MS];
        assert!(!m.mix_ui(&mut out), "nothing playing: nothing added");
        assert!(m.play_ui(ui_buf(1000, 600), 1.0)); // 600 frames = 1.25 blocks of 480
        assert_eq!(m.ui_active(), 1);
        assert!(m.mix_ui(&mut out));
        assert!(out.iter().all(|&s| s == 1000));
        let mut out = vec![0i16; MIX_10MS];
        assert!(m.mix_ui(&mut out));
        assert!(out[..240].iter().all(|&s| s == 1000), "the remaining 120 frames");
        assert!(out[240..].iter().all(|&s| s == 0), "then silence");
        assert_eq!(m.ui_active(), 0);
        assert!(!m.mix_ui(&mut out));
    }

    #[test]
    fn ui_sounds_add_to_the_voice_mix_gain_and_saturate() {
        let m = Mixer::new();
        let v = m.add_source();
        m.push(v, &filled(100, 4 * MIX_10MS));
        m.play_ui(ui_buf(50, 4800), 0.5);
        let mut out = vec![0i16; MIX_10MS];
        m.mix(&mut out);
        assert!(out.iter().all(|&s| s == 100), "voices alone");
        assert!(m.mix_ui(&mut out));
        assert!(out.iter().all(|&s| s == 125), "voice + 50 * 0.5");
        // Saturation, not wrap-around.
        m.play_ui(ui_buf(i16::MAX, 480), 1.0);
        let mut out = vec![i16::MAX - 10; MIX_10MS];
        m.mix_ui(&mut out);
        assert!(out.iter().all(|&s| s == i16::MAX));
    }

    #[test]
    fn ui_sounds_survive_silenced_voices_and_overlap_is_capped() {
        let m = Mixer::new();
        for _ in 0..MAX_UI_VOICES {
            assert!(m.play_ui(ui_buf(1, 480), 1.0));
        }
        assert!(!m.play_ui(ui_buf(1, 480), 1.0), "over the cap");
        assert!(!m.play_ui(Arc::from(Vec::<i16>::new()), 1.0), "empty buffers are ignored");
        // The caller zeroes the voice mix (deafen) and still adds the alerts afterwards.
        let mut out = vec![0i16; MIX_10MS];
        m.mix_ui(&mut out);
        assert!(out.iter().all(|&s| s == MAX_UI_VOICES as i16));
        assert_eq!(m.ui_active(), 0);
        m.play_ui(ui_buf(1, 480), 1.0);
        m.clear_ui();
        assert_eq!(m.ui_active(), 0);
    }

    fn clip(v: i16, frames: usize, channels: usize) -> Arc<[i16]> {
        Arc::from(vec![v; frames * channels])
    }

    #[test]
    fn soundboard_clips_are_mixed_with_the_voices_at_the_soundboard_gain() {
        let m = Mixer::new();
        m.sound_gain().set_level(0.5);
        let v = m.add_source();
        m.push(v, &filled(100, 4 * MIX_10MS));
        // A mono clip of 600 frames (1.25 blocks of 480) plays on both channels.
        assert!(m.play_sound(clip(1000, 600, 1), 1));
        assert_eq!(m.sounds_active(), 1);
        let mut out = vec![0i16; MIX_10MS];
        m.mix(&mut out);
        assert!(out.iter().all(|&s| s == 100 + 500), "voice + clip * 50 %: {:?}", &out[..2]);
        m.mix(&mut out);
        assert!(out[..240].iter().all(|&s| s == 600) && out[240..].iter().all(|&s| s == 100), "the clip ends mid-block");
        assert_eq!(m.sounds_active(), 0);
        // Stereo clips keep their channels.
        let stereo: Arc<[i16]> = Arc::from((0..480).flat_map(|_| [200i16, -400]).collect::<Vec<_>>());
        assert!(m.play_sound(stereo, 2));
        let mut out = vec![0i16; MIX_10MS];
        m.mix(&mut out);
        // The voice (100 on both channels) plus the clip at 50 %: (100, -200).
        assert_eq!(&out[..4], &[200, -100, 200, -100]);
    }

    #[test]
    fn soundboard_mute_and_volume_ramp_without_a_jump_and_clips_keep_draining() {
        let m = Mixer::new();
        m.play_sound(clip(10_000, 4 * 480, 1), 1);
        let mut out = vec![0i16; MIX_10MS];
        m.mix(&mut out);
        assert!(out.iter().all(|&s| s == 10_000));
        m.sound_gain().set_muted(true);
        m.mix(&mut out);
        assert!(out[0] >= 9_900 && out[MIX_10MS - 1] <= 50, "fade, not a cut: {} -> {}", out[0], out[MIX_10MS - 1]);
        m.mix(&mut out);
        assert!(out.iter().all(|&s| s == 0));
        assert_eq!(m.sounds_active(), 1, "still draining while muted");
        m.mix(&mut out);
        assert_eq!(m.sounds_active(), 0);
    }

    #[test]
    fn soundboard_overlap_is_capped_and_bad_clips_are_ignored() {
        let m = Mixer::new();
        for _ in 0..MAX_SOUND_VOICES {
            assert!(m.play_sound(clip(1, 480, 2), 2));
        }
        assert!(!m.play_sound(clip(1, 480, 2), 2), "over the cap");
        let m = Mixer::new();
        assert!(!m.play_sound(Arc::from(Vec::<i16>::new()), 1));
        assert!(!m.play_sound(clip(1, 10, 2), 3), "only mono or stereo");
        assert!(!m.play_sound(Arc::from(vec![1i16; 5]), 2), "a half frame");
        // Clips add up and saturate instead of wrapping.
        for _ in 0..3 {
            m.play_sound(clip(20_000, 480, 1), 1);
        }
        let mut out = vec![0i16; MIX_10MS];
        m.mix(&mut out);
        assert!(out.iter().all(|&s| s == i16::MAX));
    }

    #[test]
    fn a_stopped_clip_fades_out_in_5_ms_and_only_the_named_players_clips_stop() {
        let m = Mixer::new();
        assert!(m.play_sound_by(clip(10_000, 4800, 1), 1, 7));
        assert!(m.play_sound_by(clip(100, 4800, 1), 1, 8));
        let mut out = vec![0i16; MIX_10MS];
        m.mix(&mut out);
        assert!(out.iter().all(|&s| s == 10_100));
        assert_eq!(m.sound_left(7), Some(std::time::Duration::from_millis(90)));
        assert_eq!(m.stop_sounds(Some(7)), 1);
        assert!(m.sound_left(7).is_none() && m.sound_left(8).is_some());
        assert_eq!(m.stop_sounds(Some(7)), 0, "already stopping");
        m.mix(&mut out);
        // Frame by frame: from the clip's level down to about nothing over 240 frames, never up.
        let left: Vec<i16> = out.chunks(2).map(|f| f[0]).collect();
        assert!(left[0] >= 10_000, "starts at the clip's level: {}", left[0]);
        assert!(left[..STOP_FADE_FRAMES].windows(2).all(|w| w[1] <= w[0]), "monotonic fade");
        assert!(left[STOP_FADE_FRAMES - 1] <= 100 + 10_000 / STOP_FADE_FRAMES as i16 + 1, "{}", left[STOP_FADE_FRAMES - 1]);
        assert!(left[STOP_FADE_FRAMES..].iter().all(|&s| s == 100), "then only the other player's clip");
        assert_eq!(m.sounds_active(), 1, "the stopped clip is gone");
        // Stop everything.
        assert_eq!(m.stop_sounds(None), 1);
        m.mix(&mut out);
        m.mix(&mut out);
        assert!(out.iter().all(|&s| s == 0));
        assert_eq!(m.sounds_active(), 0);
    }

    #[test]
    fn a_refused_play_stops_only_its_own_clip() {
        // 7 played a long clip; later the same clip again and another one, which the hub refused.
        let m = Mixer::new();
        let (long, short) = (clip(1000, 48_000, 1), clip(10, 9600, 1));
        assert!(m.play_sound_by(long.clone(), 1, 7));
        let mut out = vec![0i16; MIX_10MS];
        m.mix(&mut out);
        assert!(m.play_sound_by(long.clone(), 1, 7));
        assert!(m.play_sound_by(short.clone(), 1, 7));
        assert!(m.stop_clip(&long, 7), "the newest copy");
        assert!(m.stop_clip(&short, 7));
        assert!(!m.stop_clip(&short, 7) && !m.stop_clip(&short, 8));
        for _ in 0..2 {
            m.mix(&mut out);
        }
        // Only the first play goes on, untouched.
        assert!(out.iter().all(|&s| s == 1000), "{:?}", &out[..4]);
        assert_eq!(m.sounds_active(), 1);
        assert_eq!(m.sound_left(7), Some(std::time::Duration::from_millis(970)));
    }

    #[test]
    fn previews_stop_with_a_fade_and_alerts_do_not() {
        let m = Mixer::new();
        assert!(m.play_preview(ui_buf(8000, 4800), 1.0));
        assert!(m.play_ui(ui_buf(10, 4800), 1.0));
        assert_eq!(m.stop_previews(), 1);
        let mut out = vec![0i16; MIX_10MS];
        m.mix_ui(&mut out);
        let last = out[2 * (STOP_FADE_FRAMES - 1)];
        assert!(out[0] >= 8000 && last <= 50, "{} -> {last}", out[0]);
        assert!(out[2 * STOP_FADE_FRAMES..].iter().all(|&s| s == 10), "the alert keeps playing");
        assert_eq!(m.ui_active(), 1);
    }

    #[test]
    fn a_clip_is_heard_in_the_very_next_block() {
        // No priming, no queueing delay: the first frame of the next mix is the clip's first.
        let m = Mixer::new();
        let v = m.add_source();
        m.push(v, &filled(0, 4 * MIX_10MS));
        let mut out = vec![0i16; MIX_10MS];
        m.mix(&mut out);
        let samples: Arc<[i16]> = Arc::from((1..=480i16).collect::<Vec<_>>());
        m.play_sound(samples, 1);
        m.mix(&mut out);
        assert_eq!(&out[..4], &[1, 1, 2, 2]);
    }

    #[test]
    fn gain_setters_clamp_and_ignore_nan() {
        let g = Gain::default();
        g.set_level(5.0);
        assert_eq!(g.level(), MAX_GAIN);
        g.set_level(-1.0);
        assert_eq!(g.level(), 0.0);
        g.set_level(0.8);
        g.set_level(f32::NAN);
        assert_eq!(g.level(), 0.8);
        g.set_muted(true);
        assert_eq!(g.effective(), 0.0);
        assert_eq!(g.level(), 0.8);
    }
}
