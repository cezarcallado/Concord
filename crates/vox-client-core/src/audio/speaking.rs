//! Who is speaking right now: the ring around an avatar (v1.1).
//!
//! LiveKit's active-speaker events alone were not enough: the SFU decides from the audio-level
//! header with its own threshold and smoothing, so a quiet voice that our gate does send, or a
//! short word, often never lit the ring, and it came on and went off late. Every remote
//! microphone is decoded here anyway (it goes to the playback mixer), so each one also goes
//! through a [`LevelDetector`]: a 10 ms RMS against a low threshold, on after 20 ms above it, off
//! after [`RELEASE_MS`] below. The sender's gate sends digital silence when it is shut, so a low
//! threshold only reacts to what the person really sends.
//!
//! [`Speakers`] merges three inputs into the one set the app shows
//! ([`VoiceEvent::ActiveSpeakers`](crate::voice::VoiceEvent::ActiveSpeakers)):
//!
//! - the detector of each subscribed microphone, while its audio is arriving (a frame in the last
//!   [`FRESH_MS`]); it alone decides for that person then, so the ring follows the voice closely;
//! - LiveKit's active speakers, for everyone while we are deafened (their tracks are paused at the
//!   SFU) and for people whose audio is not arriving or who are not subscribed yet;
//! - the soundboard: whoever played a sound counts as speaking until it ends or is stopped (the
//!   clips are played by every app locally, they never travel on a track).
//!
//! Our own identity is never taken from LiveKit (the app lights our ring from our own gate);
//! it is in the set only while one of our soundboard sounds plays. The set is sent only when it
//! changes, so a quiet room sends nothing and costs no repaint.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::gate::rms_dbfs;

/// A 10 ms chunk at least this loud (RMS, dBFS) counts as voice.
pub const OPEN_DB: f32 = -50.0;
/// While speaking, chunks down to this level still count (hysteresis).
pub const KEEP_DB: f32 = -56.0;
/// Voice for this long turns the ring on (two chunks: one click does not).
pub const ATTACK_MS: u32 = 20;
/// Quiet for this long turns it off (word gaps do not flicker it).
pub const RELEASE_MS: u32 = 300;
/// Audio that stops arriving while someone speaks (track paused, network hiccup) turns them off
/// after this long.
pub const STALL: Duration = Duration::from_millis(RELEASE_MS as u64);
/// A track whose last frame is older than this no longer decides for its person.
pub const FRESH_MS: u64 = 1500;

/// Voice activity of one decoded audio track, with hysteresis. Allocation-free.
#[derive(Debug, Default, Clone)]
pub struct LevelDetector {
    speaking: bool,
    loud_ms: u32,
    quiet_ms: u32,
}

impl LevelDetector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn speaking(&self) -> bool {
        self.speaking
    }

    /// Feed one chunk of interleaved samples lasting `chunk_ms`. Returns the new state when it
    /// changed.
    pub fn feed(&mut self, samples: &[i16], chunk_ms: u32) -> Option<bool> {
        self.feed_db(rms_dbfs(samples), chunk_ms)
    }

    /// [`LevelDetector::feed`] with the chunk's level already measured.
    pub fn feed_db(&mut self, db: f32, chunk_ms: u32) -> Option<bool> {
        if self.speaking {
            if db >= KEEP_DB {
                self.quiet_ms = 0;
                return None;
            }
            self.quiet_ms += chunk_ms;
            if self.quiet_ms >= RELEASE_MS {
                self.speaking = false;
                self.loud_ms = 0;
                return Some(false);
            }
            None
        } else if db >= OPEN_DB {
            self.loud_ms += chunk_ms;
            if self.loud_ms >= ATTACK_MS {
                self.speaking = true;
                self.quiet_ms = 0;
                return Some(true);
            }
            None
        } else {
            self.loud_ms = 0;
            None
        }
    }

    /// The audio stopped arriving: whatever was going on is over.
    pub fn stalled(&mut self) -> Option<bool> {
        self.loud_ms = 0;
        self.quiet_ms = 0;
        std::mem::take(&mut self.speaking).then_some(false)
    }
}

/// When a track last delivered a frame (ms since [`Speakers`] started). Touched once per frame
/// with a relaxed store: no lock on the audio path.
#[derive(Debug, Default)]
struct TrackCell {
    last_frame_ms: AtomicU64,
}

struct Track {
    identity: String,
    speaking: bool,
    cell: Arc<TrackCell>,
}

#[derive(Default)]
struct State {
    me: String,
    /// LiveKit's active speakers (without us).
    lk: HashSet<String>,
    /// Subscribed microphones, by a key of their own (a resubscribed track with the same sid
    /// gets a new key, so the old one going away cannot remove it).
    tracks: HashMap<u64, Track>,
    next_key: u64,
    /// Soundboard: identity -> (generation, playing until ms).
    sounds: HashMap<String, (u64, u64)>,
    gen: u64,
    /// The set last sent (sorted).
    shown: Vec<String>,
    /// We are deafened: the remote microphones are paused at the SFU, so what is decoded (nothing,
    /// or the receiver's own silence) says nothing about them; LiveKit decides for everyone.
    deafened: bool,
}

impl State {
    /// Who is speaking at `now_ms`, sorted.
    fn compute(&self, now_ms: u64) -> Vec<String> {
        let mut out: HashSet<&str> = HashSet::new();
        // People whose audio arrives: their detector decides.
        let mut decided: HashSet<&str> = HashSet::new();
        for t in self.tracks.values().filter(|_| !self.deafened) {
            let last = t.cell.last_frame_ms.load(Ordering::Relaxed);
            let fresh = last > 0 && now_ms.saturating_sub(last) <= FRESH_MS;
            if fresh {
                decided.insert(&t.identity);
                if t.speaking {
                    out.insert(&t.identity);
                }
            }
        }
        for id in &self.lk {
            if *id != self.me && !decided.contains(id.as_str()) {
                out.insert(id);
            }
        }
        for (id, (_, until)) in &self.sounds {
            if *until > now_ms {
                out.insert(id);
            }
        }
        let mut v: Vec<String> = out.into_iter().map(str::to_string).collect();
        v.sort();
        v
    }
}

type Emit = Box<dyn Fn(Vec<String>) + Send + Sync>;

/// The speaking set of one voice session (see the module docs).
pub struct Speakers {
    epoch: Instant,
    st: Mutex<State>,
    emit: Emit,
}

impl Speakers {
    /// `me`: our own identity. `emit` gets the whole new set (sorted) each time it changes.
    pub fn new(me: &str, emit: impl Fn(Vec<String>) + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self { epoch: Instant::now(), st: Mutex::new(State { me: me.to_string(), ..State::default() }), emit: Box::new(emit) })
    }

    /// Milliseconds since this session's set started (never 0, so 0 can mean "never").
    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64 + 1
    }

    fn publish(&self, st: &mut State) {
        let now = st.compute(self.now_ms());
        if now != st.shown {
            st.shown = now.clone();
            (self.emit)(now);
        }
    }

    /// The set as last sent.
    pub fn current(&self) -> Vec<String> {
        self.st.lock().shown.clone()
    }

    /// LiveKit's active speakers changed.
    pub fn set_active(&self, identities: impl IntoIterator<Item = String>) {
        let mut st = self.st.lock();
        let me = st.me.clone();
        st.lk = identities.into_iter().filter(|i| *i != me).collect();
        self.publish(&mut st);
    }

    /// We were deafened or undeafened (see [`State::deafened`]).
    pub fn set_deafened(&self, deafened: bool) {
        let mut st = self.st.lock();
        st.deafened = deafened;
        self.publish(&mut st);
    }

    /// A remote microphone of `identity` was subscribed: feed its frames to the returned handle;
    /// dropping it removes the track.
    pub fn add_track(self: &Arc<Self>, identity: &str) -> TrackLevel {
        let cell = Arc::new(TrackCell::default());
        let mut st = self.st.lock();
        st.next_key += 1;
        let key = st.next_key;
        st.tracks.insert(key, Track { identity: identity.to_string(), speaking: false, cell: cell.clone() });
        self.publish(&mut st);
        TrackLevel { speakers: self.clone(), key, cell, detector: LevelDetector::new() }
    }

    fn remove_track(&self, key: u64) {
        let mut st = self.st.lock();
        if st.tracks.remove(&key).is_some() {
            self.publish(&mut st);
        }
    }

    /// Someone left the room: nothing of theirs is lit any more.
    pub fn participant_left(&self, identity: &str) {
        let mut st = self.st.lock();
        st.tracks.retain(|_, t| t.identity != identity);
        st.lk.remove(identity);
        st.sounds.remove(identity);
        self.publish(&mut st);
    }

    fn set_track(&self, key: u64, speaking: bool) {
        let mut st = self.st.lock();
        if let Some(t) = st.tracks.get_mut(&key) {
            t.speaking = speaking;
            self.publish(&mut st);
        }
    }

    /// A soundboard sound of `identity` started (lit for `length`) or ended (`None`).
    pub fn sound(self: &Arc<Self>, identity: &str, length: Option<Duration>) {
        let mut st = self.st.lock();
        st.gen += 1;
        let gen = st.gen;
        match length {
            Some(d) => {
                let until = self.now_ms() + d.as_millis() as u64;
                st.sounds.insert(identity.to_string(), (gen, until));
                // Off again when it ends, unless a newer sound of theirs took over.
                if let Ok(rt) = tokio::runtime::Handle::try_current() {
                    let (me, who) = (Arc::downgrade(self), identity.to_string());
                    rt.spawn(async move {
                        tokio::time::sleep(d).await;
                        if let Some(sp) = me.upgrade() {
                            sp.sound_ended(&who, gen);
                        }
                    });
                }
            }
            None => {
                st.sounds.remove(identity);
            }
        }
        self.publish(&mut st);
    }

    fn sound_ended(&self, identity: &str, gen: u64) {
        let mut st = self.st.lock();
        if st.sounds.get(identity).is_some_and(|(g, _)| *g == gen) {
            st.sounds.remove(identity);
            self.publish(&mut st);
        }
    }
}

/// The detector of one subscribed microphone, owned by the task that receives its frames.
pub struct TrackLevel {
    speakers: Arc<Speakers>,
    key: u64,
    cell: Arc<TrackCell>,
    detector: LevelDetector,
}

impl TrackLevel {
    /// One decoded frame (interleaved, `chunk_ms` long). Cheap: an RMS and an atomic store; the
    /// shared set is only touched when this person starts or stops speaking.
    pub fn frame(&mut self, samples: &[i16], chunk_ms: u32) {
        self.cell.last_frame_ms.store(self.speakers.now_ms(), Ordering::Relaxed);
        if let Some(s) = self.detector.feed(samples, chunk_ms) {
            self.speakers.set_track(self.key, s);
        }
    }

    /// Speaking right now (the receiving task waits with a timeout only then).
    pub fn speaking(&self) -> bool {
        self.detector.speaking()
    }

    /// No frame came for [`STALL`]: off.
    pub fn stalled(&mut self) {
        if let Some(s) = self.detector.stalled() {
            self.speakers.set_track(self.key, s);
        }
    }
}

impl Drop for TrackLevel {
    fn drop(&mut self) {
        self.speakers.remove_track(self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 10 ms of a constant-amplitude stereo chunk at about `db` dBFS RMS.
    fn chunk(db: f32) -> Vec<i16> {
        let amp = (32768.0 * 10f32.powf(db / 20.0)).round() as i16;
        (0..960).map(|i| if i % 4 < 2 { amp } else { -amp }).collect()
    }

    #[test]
    fn a_short_word_lights_it_and_silence_turns_it_off_after_the_release() {
        let mut d = LevelDetector::new();
        // Silence and a single loud chunk (a click): nothing.
        assert_eq!(d.feed(&[0; 960], 10), None);
        assert_eq!(d.feed(&chunk(-30.0), 10), None);
        assert_eq!(d.feed(&[0; 960], 10), None);
        assert!(!d.speaking());
        // A 60 ms word ("é"): on at its second chunk.
        assert_eq!(d.feed(&chunk(-35.0), 10), None);
        assert_eq!(d.feed(&chunk(-35.0), 10), Some(true));
        for _ in 0..4 {
            assert_eq!(d.feed(&chunk(-35.0), 10), None);
        }
        // Then silence: still lit for the release time, off exactly at it.
        let mut off_at = None;
        for i in 1..=40 {
            if d.feed(&[0; 960], 10) == Some(false) {
                off_at = Some(i * 10);
                break;
            }
        }
        assert_eq!(off_at, Some(RELEASE_MS));
        assert!(!d.speaking());
    }

    #[test]
    fn quiet_voice_counts_and_word_gaps_do_not_flicker() {
        let mut d = LevelDetector::new();
        // A quiet talker (-48 dBFS), above what the sender's gate lets through, but well below
        // what LiveKit's speaker detection reacts to.
        d.feed(&chunk(-48.0), 10);
        assert_eq!(d.feed(&chunk(-48.0), 10), Some(true));
        // A 200 ms gap between words and a dip to -54 dBFS (hysteresis): no change.
        for _ in 0..20 {
            assert_eq!(d.feed(&[0; 960], 10), None);
        }
        assert_eq!(d.feed(&chunk(-54.0), 10), None);
        for _ in 0..25 {
            assert_eq!(d.feed(&[0; 960], 10), None);
        }
        assert!(d.speaking());
        // Comfort noise / room tone far below the threshold never lights it.
        let mut q = LevelDetector::new();
        for _ in 0..100 {
            assert_eq!(q.feed(&chunk(-70.0), 10), None);
        }
        // A stall ends it at once.
        assert_eq!(d.stalled(), Some(false));
        assert_eq!(d.stalled(), None);
    }

    fn collector() -> (Arc<Speakers>, Arc<Mutex<Vec<Vec<String>>>>) {
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        (Speakers::new("me", move |v| g.lock().push(v)), got)
    }

    #[test]
    fn the_detector_decides_while_audio_arrives_and_livekit_fills_in_otherwise() {
        let (sp, got) = collector();
        // LiveKit says ana and we are speaking: we are never taken from LiveKit.
        sp.set_active(["ana".to_string(), "me".to_string()]);
        assert_eq!(sp.current(), ["ana"]);
        // ana's microphone arrives and is quiet: the detector decides, she is off at once.
        let mut ana = sp.add_track("ana");
        ana.frame(&[0; 960], 10);
        sp.set_active(["ana".to_string()]);
        assert!(sp.current().is_empty(), "{:?}", sp.current());
        // She speaks: on with the second chunk, without waiting for LiveKit.
        ana.frame(&chunk(-40.0), 10);
        ana.frame(&chunk(-40.0), 10);
        assert_eq!(sp.current(), ["ana"]);
        // bob has no track (not subscribed yet): LiveKit decides for him.
        sp.set_active(["bob".to_string()]);
        assert_eq!(sp.current(), ["ana", "bob"]);
        // ana's track goes away: back to LiveKit (which no longer lists her).
        drop(ana);
        assert_eq!(sp.current(), ["bob"]);
        sp.participant_left("bob");
        assert!(sp.current().is_empty());
        // Only changes were sent.
        let sent = got.lock().clone();
        assert!(sent.windows(2).all(|w| w[0] != w[1]), "{sent:?}");
    }

    #[test]
    fn a_stale_track_hands_over_to_livekit() {
        let (sp, _) = collector();
        let t = sp.add_track("carla");
        // The track never delivered a frame (paused at the SFU while we are deafened).
        sp.set_active(["carla".to_string()]);
        assert_eq!(sp.current(), ["carla"]);
        // A frame that is too old does not decide either.
        t.cell.last_frame_ms.store(1, Ordering::Relaxed);
        let st = sp.st.lock();
        assert_eq!(st.compute(FRESH_MS + 10), ["carla"]);
        assert!(st.compute(FRESH_MS).is_empty(), "fresh and quiet: the detector says no");
    }

    #[test]
    fn deafened_livekit_decides_even_while_the_receiver_decodes_silence() {
        let (sp, _) = collector();
        let mut davi = sp.add_track("davi");
        davi.frame(&chunk(-30.0), 10);
        davi.frame(&chunk(-30.0), 10);
        assert_eq!(sp.current(), ["davi"]);
        // Deafened while he speaks: his track is paused at the SFU, at most silence is decoded
        // now, and LiveKit's list does not change while he goes on.
        sp.set_active(["davi".to_string()]);
        sp.set_deafened(true);
        for _ in 0..40 {
            davi.frame(&[0; 960], 10);
        }
        davi.stalled();
        assert_eq!(sp.current(), ["davi"], "LiveKit still lists him");
        sp.set_active([]);
        assert!(sp.current().is_empty());
        // Undeafened: his decoded voice decides again.
        sp.set_active(["davi".to_string()]);
        sp.set_deafened(false);
        davi.frame(&[0; 960], 10);
        assert!(sp.current().is_empty(), "fresh and quiet: {:?}", sp.current());
    }

    #[tokio::test]
    async fn a_soundboard_sound_keeps_its_player_lit_until_it_ends_or_is_stopped() {
        let (sp, _) = collector();
        // Our own sound lights us (we are never taken from LiveKit, but the soundboard counts).
        sp.sound("me", Some(Duration::from_millis(150)));
        sp.sound("ana", Some(Duration::from_millis(5000)));
        assert_eq!(sp.current(), ["ana", "me"]);
        // Lit for the whole sound, even through ana's own silence on her microphone.
        let mut ana = sp.add_track("ana");
        for _ in 0..50 {
            ana.frame(&[0; 960], 10);
        }
        assert_eq!(sp.current(), ["ana", "me"]);
        // Ours ends by itself.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(sp.current(), ["ana"]);
        // ana's is stopped.
        sp.sound("ana", None);
        assert!(sp.current().is_empty());
        // A newer sound is not ended by the timer of an older one.
        sp.sound("me", Some(Duration::from_millis(100)));
        sp.sound("me", Some(Duration::from_millis(5000)));
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(sp.current(), ["me"]);
    }
}
