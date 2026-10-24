//! Alert sounds while *not* in a voice channel (no playback stream exists then).
//!
//! The selected output device is opened lazily on the first alert and closed again a few
//! seconds after the last one ends, so an idle app holds no audio device and no thread. While in
//! voice the alerts go to the session's own mixer instead (`VoiceSession::play_alert`).

use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tracing::{debug, info, warn};

use super::alerts::AlertKind;
use super::device::{AudioIo, PlaybackConfig};
use super::mixer::Mixer;

/// The stream stays open this long after the last alert finished (back-to-back alerts reuse it).
pub const IDLE_CLOSE: Duration = Duration::from_secs(4);
/// Never keep the stream for an alert that does not finish (a stalled device) longer than this.
const STALL_LIMIT: Duration = Duration::from_secs(10);
/// After a failed open, alerts are dropped for this long instead of retrying on every event.
const RETRY_AFTER: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(250);

#[derive(Default)]
struct State {
    /// A worker thread owns an open (or opening) output stream.
    running: bool,
    retry_at: Option<Instant>,
}

struct Shared {
    mixer: Arc<Mixer>,
    state: Mutex<State>,
}

/// Plays alerts on the output device on demand. Cheap to clone.
#[derive(Clone)]
pub struct UiPlayer {
    shared: Arc<Shared>,
}

impl Default for UiPlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl UiPlayer {
    pub fn new() -> Self {
        Self { shared: Arc::new(Shared { mixer: Arc::new(Mixer::new()), state: Mutex::new(State::default()) }) }
    }

    /// Queue `kind` at `gain` on `device` (`None` = system default; a substring of the device
    /// name otherwise), opening the stream if it is not open. Never blocks on the device.
    /// Returns false when the alert was dropped (device recently failed, or too many playing).
    pub fn play(&self, kind: AlertKind, gain: f32, device: Option<&str>) -> bool {
        self.play_pcm(kind.samples(), gain, device)
    }

    /// Like [`UiPlayer::play`] for any clip: `samples` are interleaved stereo, 48 kHz (the trim
    /// editor's "Ouvir trecho" plays the clip it would upload).
    pub fn play_pcm(&self, samples: Arc<[i16]>, gain: f32, device: Option<&str>) -> bool {
        let mut st = self.shared.state.lock();
        if st.retry_at.is_some_and(|t| Instant::now() < t) {
            debug!("ui sound dropped: the output device failed recently");
            return false;
        }
        // Queue under the state lock so the worker's "idle, close now" check cannot miss it.
        if !self.shared.mixer.play_ui(samples, gain) {
            return false;
        }
        if !st.running {
            st.running = true;
            let shared = self.shared.clone();
            let device = device.map(str::to_string);
            let spawned = std::thread::Builder::new().name("vox-ui-sound".into()).spawn(move || worker(shared, device));
            if let Err(e) = spawned {
                warn!(error = %e, "cannot start the alert sound thread");
                st.running = false;
                self.shared.mixer.clear_ui();
                return false;
            }
        }
        true
    }

    /// Whether an output stream is currently open (or opening) for alerts.
    pub fn is_open(&self) -> bool {
        self.shared.state.lock().running
    }
}

fn worker(shared: Arc<Shared>, device: Option<String>) {
    let started = Instant::now();
    let io = AudioIo::start(
        None,
        Some(PlaybackConfig {
            device,
            mixer: shared.mixer.clone(),
            deafened: Arc::new(AtomicBool::new(false)),
            volume: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            apm: None,
        }),
    );
    if !io.playback_ok {
        warn!(error = io.playback_error.as_deref().unwrap_or("unknown"), "alert sounds: cannot open the output device");
        let mut st = shared.state.lock();
        st.running = false;
        st.retry_at = Some(Instant::now() + RETRY_AFTER);
        shared.mixer.clear_ui();
        return;
    }
    debug!(ms = started.elapsed().as_millis() as u64, "alert sounds: output opened");
    let mut idle_since: Option<Instant> = None;
    let mut busy_since = Instant::now();
    loop {
        std::thread::sleep(POLL);
        let active = shared.mixer.ui_active() > 0;
        let now = Instant::now();
        if active {
            idle_since = None;
            if now.duration_since(busy_since) > STALL_LIMIT {
                warn!("alert sounds: the output device is not consuming audio; closing it");
                shared.mixer.clear_ui();
                shared.state.lock().retry_at = Some(now + RETRY_AFTER);
            }
            continue;
        }
        busy_since = now;
        let since = *idle_since.get_or_insert(now);
        if now.duration_since(since) >= IDLE_CLOSE {
            let mut st = shared.state.lock();
            if shared.mixer.ui_active() == 0 {
                st.running = false;
                break;
            }
            // An alert arrived at the last moment: keep going.
            idle_since = None;
        }
    }
    drop(io);
    info!("alert sounds: output closed (idle)");
}

#[cfg(test)]
mod tests {
    use super::super::alerts::{palette_test_guard, set_palette, Palette};
    use super::*;

    /// A player whose stream counts as open, so `play` only queues into its mixer: no worker,
    /// no device, nothing reaches the speakers.
    fn silent_player() -> UiPlayer {
        let p = UiPlayer::new();
        p.shared.state.lock().running = true;
        p
    }

    /// What the mixer plays for the queued alerts, start to end.
    fn drain(p: &UiPlayer) -> Vec<i16> {
        let mut all = Vec::new();
        let mut block = [0i16; 960];
        loop {
            block.fill(0);
            if !p.shared.mixer.mix_ui(&mut block) {
                return all;
            }
            all.extend_from_slice(&block);
        }
    }

    #[test]
    fn the_mixer_receives_the_chosen_palette_from_the_next_alert() {
        let _guard = palette_test_guard();
        let p = silent_player();
        for palette in [Palette::SinoSuave, Palette::Atual, Palette::Gota, Palette::Madeira, Palette::SinoSuave] {
            set_palette(palette);
            assert!(p.play(AlertKind::Mention, 1.0, None));
            let want = AlertKind::Mention.samples_in(palette);
            let got = drain(&p);
            assert_eq!(&got[..want.len()], &want[..], "{palette:?}");
            assert!(got[want.len()..].iter().all(|s| *s == 0), "{palette:?}: one alert, then silence");
        }
        assert_eq!(p.shared.mixer.ui_active(), 0);
    }
}
