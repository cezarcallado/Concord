//! Microphones that do not exist, for tests: no device is opened, a real capture chain
//! ([`MicState`]: resampling, gate, denoiser, the room or a mixer) is fed a synthetic tone in the
//! format the made-up device claims to have. They can be unplugged, refuse to open, go silent
//! without a word, or stop being the system default.
//!
//! The unit tests feed them by hand; an autotest [`install`]s a bank and it runs in real time.
//! Nothing here touches the real microphone, and nothing is ever played.

use std::any::Any;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::Mutex;

use super::device::{new_mic_state, MicConfig, MicState};
use super::mic_route::{MicBackend, MicOpen};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeFormat {
    F32,
    I16,
}

/// A device's name and what it delivers.
#[derive(Debug, Clone)]
pub struct FakeMic {
    pub name: String,
    pub rate: u32,
    pub channels: u16,
    pub format: FakeFormat,
    pub tone_hz: f32,
    /// Speech-like instead of a steady tone: syllables and a moving pitch, which the noise
    /// suppressors let through.
    pub speech: bool,
}

impl FakeMic {
    pub fn new(name: &str, rate: u32, channels: u16, format: FakeFormat) -> Self {
        Self { name: name.into(), rate, channels, format, tone_hz: 440.0, speech: false }
    }

    pub fn speaking(mut self) -> Self {
        self.speech = true;
        self
    }
}

struct Slot {
    mic: FakeMic,
    listed: bool,
    broken: bool,
    silent: bool,
}

/// One open stream.
struct Live {
    id: u64,
    mic: FakeMic,
    state: MicState,
    /// Samples produced so far (per channel), the rest of a block that did not make a sample and
    /// the tone's phase.
    produced: u64,
    owed: f64,
    phase: f64,
    on_lost: Box<dyn Fn() + Send>,
}

#[derive(Default)]
struct Bank {
    slots: Vec<Slot>,
    default: Option<String>,
    live: Vec<Live>,
    next_id: u64,
    opens: u32,
}

/// A set of made-up devices (a cheap handle).
#[derive(Clone, Default)]
pub struct FakeMics(Arc<Mutex<Bank>>);

impl std::fmt::Debug for FakeMics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("FakeMics").field(&self.open_names()).finish()
    }
}

impl FakeMics {
    /// These devices, all listed; the first one is the system default.
    pub fn new(mics: Vec<FakeMic>) -> Self {
        let default = mics.first().map(|m| m.name.clone());
        let slots = mics.into_iter().map(|mic| Slot { mic, listed: true, broken: false, silent: false }).collect();
        Self(Arc::new(Mutex::new(Bank { slots, default, ..Default::default() })))
    }

    fn with_slot(&self, name: &str, f: impl FnOnce(&mut Slot)) {
        let mut b = self.0.lock();
        if let Some(s) = b.slots.iter_mut().find(|s| s.mic.name == name) {
            f(s);
        }
    }

    /// Plugged in or not. Unplugging says nothing to an open stream: see [`Self::report_lost`] and
    /// [`Self::set_silent`] for the two ways a real one finds out.
    pub fn set_listed(&self, name: &str, listed: bool) {
        self.with_slot(name, |s| s.listed = listed);
    }

    /// Listed, but every open of it fails.
    pub fn set_broken(&self, name: &str, broken: bool) {
        self.with_slot(name, |s| s.broken = broken);
    }

    /// Its streams stop calling back, with no error: a driver that says nothing.
    pub fn set_silent(&self, name: &str, silent: bool) {
        self.with_slot(name, |s| s.silent = silent);
    }

    /// The system default (`None`: there is none).
    pub fn set_default(&self, name: Option<&str>) {
        self.0.lock().default = name.map(str::to_string);
    }

    /// The streams of this device report their death, like cpal's error callback.
    pub fn report_lost(&self, name: &str) {
        for l in self.0.lock().live.iter().filter(|l| l.mic.name == name) {
            (l.on_lost)();
        }
    }

    /// Names of the devices being captured now.
    pub fn open_names(&self) -> Vec<String> {
        self.0.lock().live.iter().map(|l| l.mic.name.clone()).collect()
    }

    /// Streams opened so far.
    pub fn opens(&self) -> u32 {
        self.0.lock().opens
    }

    /// Deliver `ms` of audio from every open stream, in its own format.
    pub fn feed(&self, ms: u32) {
        let mut b = self.0.lock();
        let Bank { slots, live, .. } = &mut *b;
        for l in live.iter_mut() {
            if slots.iter().any(|s| s.mic.name == l.mic.name && s.silent) {
                continue;
            }
            l.owed += l.mic.rate as f64 * ms as f64 / 1000.0;
            let frames = l.owed as usize;
            l.owed -= frames as f64;
            l.deliver(frames);
        }
    }

    /// Feed in real time, forever (the autotests: until the process ends).
    pub fn run_in_real_time(&self) {
        let bank = self.clone();
        let spawned = std::thread::Builder::new().name("vox-fake-mic".into()).spawn(move || loop {
            std::thread::sleep(Duration::from_millis(10));
            bank.feed(10);
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not start the fake microphones");
        }
    }

    /// The device side of a capture with this configuration.
    pub(super) fn backend(self, cfg: MicConfig) -> FakeBackend {
        FakeBackend { bank: self, cfg }
    }
}

impl Live {
    fn deliver(&mut self, frames: usize) {
        use std::f64::consts::TAU;
        let ch = self.mic.channels as usize;
        let rate = self.mic.rate as f64;
        let mono: Vec<f32> = (0..frames)
            .map(|_| {
                let t = self.produced as f64 / rate;
                self.produced += 1;
                let (bend, level) = if self.mic.speech { (1.0 + 0.3 * (TAU * 2.7 * t).sin(), if (t * 7.0) as u64 % 3 == 2 { 0.01 } else { 1.0 }) } else { (1.0, 1.0) };
                self.phase = (self.phase + TAU * self.mic.tone_hz as f64 * bend / rate) % TAU;
                (self.phase.sin() * 0.25 * level) as f32
            })
            .collect();
        match self.mic.format {
            FakeFormat::F32 => {
                let buf: Vec<f32> = mono.iter().flat_map(|&s| std::iter::repeat_n(s, ch)).collect();
                self.state.on_data(&buf);
            }
            FakeFormat::I16 => {
                let buf: Vec<i16> = mono.iter().flat_map(|&s| std::iter::repeat_n((s * i16::MAX as f32) as i16, ch)).collect();
                self.state.on_data(&buf);
            }
        }
    }
}

/// Closes its stream when dropped.
struct Handle(FakeMics, u64);

impl Drop for Handle {
    fn drop(&mut self) {
        let closed = {
            let mut b = self.0 .0.lock();
            b.live.iter().position(|l| l.id == self.1).map(|i| b.live.remove(i))
        };
        drop(closed); // outside the lock
    }
}

pub(super) struct FakeBackend {
    bank: FakeMics,
    cfg: MicConfig,
}

impl MicBackend for FakeBackend {
    fn open(&mut self, device: Option<&str>, on_lost: Box<dyn Fn() + Send>) -> Result<MicOpen, String> {
        let mut b = self.bank.0.lock();
        let want = match device {
            Some(n) => n.to_string(),
            None => b.default.clone().ok_or("no default input device")?,
        };
        let slot = b.slots.iter().find(|s| s.listed && s.mic.name == want).ok_or_else(|| format!("no input device matching {want:?}"))?;
        if slot.broken {
            return Err("build mic stream: the device refused the format".to_string());
        }
        let mic = slot.mic.clone();
        let pulse = Arc::new(AtomicU64::new(0));
        let state = new_mic_state(&self.cfg, mic.channels as usize, mic.rate, pulse.clone());
        let id = b.next_id;
        b.next_id += 1;
        b.opens += 1;
        b.live.push(Live { id, mic: mic.clone(), state, produced: 0, owed: 0.0, phase: 0.0, on_lost });
        drop(b);
        let handle: Box<dyn Any> = Box::new(Handle(self.bank.clone(), id));
        Ok(MicOpen { name: mic.name.clone(), id: Some(mic.name), pulse, _stream: handle })
    }

    fn present(&mut self, name: &str) -> bool {
        self.bank.0.lock().slots.iter().any(|s| s.listed && s.mic.name == name)
    }

    fn default_id(&mut self) -> Option<String> {
        self.bank.0.lock().default.clone()
    }
}

static INSTALLED: OnceLock<FakeMics> = OnceLock::new();

/// An autotest takes the place of the real microphones for this process (the first bank stays).
pub fn install(bank: FakeMics) -> FakeMics {
    let bank = INSTALLED.get_or_init(|| bank).clone();
    bank.run_in_real_time();
    bank
}

/// The bank an autotest installed.
pub fn installed() -> Option<FakeMics> {
    INSTALLED.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::device::{AudioIo, MicSink};
    use crate::audio::gate::{GateConfig, GateMode, VoiceGate};
    use crate::audio::mic_route::{MicRoute, MicStatus, HEALTH_TICK, STALL_AFTER};
    use crate::audio::mixer::{Mixer, MIX_10MS, MIX_RATE};
    use crate::audio::{NoiseControl, NoiseLevel};
    use crate::stats::StreamCounters;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    const M48: &str = "48 kHz mono";
    const S44: &str = "44,1 kHz estéreo";
    const H16: &str = "Headset 16 kHz";
    const Q96: &str = "96 kHz, quatro canais";

    fn bank() -> FakeMics {
        FakeMics::new(vec![
            FakeMic::new(M48, 48_000, 1, FakeFormat::F32),
            FakeMic::new(S44, 44_100, 2, FakeFormat::F32),
            FakeMic::new(H16, 16_000, 1, FakeFormat::I16),
            FakeMic::new(Q96, 96_000, 4, FakeFormat::F32),
        ])
    }

    struct Rig {
        bank: FakeMics,
        route: MicRoute,
        counters: Arc<StreamCounters>,
        mixer: Arc<Mixer>,
        muted: Arc<AtomicBool>,
        gate: Arc<VoiceGate>,
        said: Arc<Mutex<Vec<MicStatus>>>,
    }

    fn cfg(bank: &FakeMics, mixer: &Arc<Mixer>, gate: &Arc<VoiceGate>, muted: &Arc<AtomicBool>, counters: &Arc<StreamCounters>) -> MicConfig {
        MicConfig {
            device: None,
            sink: MicSink::Loopback(mixer.clone()),
            muted: muted.clone(),
            apm: None,
            counters: counters.clone(),
            gate: gate.clone(),
            noise: Arc::new(NoiseControl::new(NoiseLevel::Standard)),
            fall_back: true,
            on_status: None,
            fake: Some(bank.clone()),
        }
    }

    fn rig(wanted: Option<&str>) -> Rig {
        let bank = bank();
        let mixer = Arc::new(Mixer::new());
        let gate = Arc::new(VoiceGate::new(GateConfig { mode: GateMode::Manual, threshold_db: -60.0, hold_ms: 100 }));
        let counters = Arc::new(StreamCounters::default());
        let muted = Arc::new(AtomicBool::new(false));
        let said = Arc::new(Mutex::new(Vec::new()));
        let s2 = said.clone();
        let c = cfg(&bank, &mixer, &gate, &muted, &counters);
        let mut route = MicRoute::new(Box::new(bank.clone().backend(c)), Arc::new(|_| {}), Some(Arc::new(move |m| s2.lock().push(m))), true, wanted.map(str::to_string));
        route.start(Instant::now()).unwrap();
        Rig { bank, route, counters, mixer, muted, gate, said }
    }

    impl Rig {
        fn frames(&self) -> u64 {
            self.counters.frames.load(Ordering::Relaxed)
        }

        /// Run `ms` of capture in 10 ms steps and take what the chain sent: mono, 48 kHz.
        fn capture(&self, ms: u32) -> Vec<i16> {
            let mut out = Vec::new();
            let mut block = vec![0i16; MIX_10MS];
            for _ in 0..ms / 10 {
                self.bank.feed(10);
                self.mixer.mix(&mut block);
                out.extend(block.iter().step_by(2).copied());
            }
            out
        }

        fn switch(&mut self, to: Option<&str>) {
            self.route.set_device(to.map(str::to_string), Instant::now());
        }
    }

    /// Frequency from the rising zero crossings of the tail of `samples` at 48 kHz.
    fn tone_hz(samples: &[i16]) -> f32 {
        let tail = &samples[samples.len() / 2..];
        let ups: Vec<usize> = tail.windows(2).enumerate().filter(|(_, w)| w[0] < 0 && w[1] >= 0).map(|(i, _)| i).collect();
        assert!(ups.len() > 10, "no tone in the output ({} crossings)", ups.len());
        let cycles = (ups.len() - 1) as f32;
        cycles * MIX_RATE as f32 / (ups[ups.len() - 1] - ups[0]) as f32
    }

    #[test]
    fn output_stays_48k_mono_with_the_right_pitch_across_any_device_format() {
        let mut r = rig(None);
        assert_eq!(r.route.current(), Some(M48));
        let mut before = r.frames();
        for dev in [Some(S44), Some(H16), Some(Q96), None, Some(S44), Some(M48)] {
            r.switch(dev);
            assert_eq!(r.bank.open_names().len(), 1, "one mic at a time");
            let got = r.capture(1000);
            let chunks = r.frames() - before;
            before = r.frames();
            // 1 s in = 100 chunks of 48 kHz mono out: the rate of *this* device is the one used.
            assert!((99..=101).contains(&chunks), "{dev:?}: {chunks} chunks in 1 s");
            assert_eq!(got.len(), 48_000);
            let hz = tone_hz(&got);
            assert!((hz - 440.0).abs() < 4.0, "{dev:?}: heard {hz} Hz instead of 440");
            assert_eq!(r.counters.errors.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn the_old_device_gets_no_more_audio_through_after_the_switch() {
        let mut r = rig(Some(S44));
        r.capture(100);
        r.switch(Some(H16));
        assert_eq!(r.bank.open_names(), [H16], "the previous mic is closed");
        assert_eq!(r.said.lock().as_slice(), [MicStatus::Switched { device: H16.into() }]);
    }

    #[test]
    fn mute_and_the_gate_survive_a_switch_with_nothing_stuck() {
        let mut r = rig(None);
        r.capture(200);
        assert!(r.gate.is_open());
        // Switching while muted: still muted on the new device, and it comes back with the unmute.
        r.muted.store(true, Ordering::Relaxed);
        r.capture(50);
        let sent = r.frames();
        r.switch(Some(S44));
        r.capture(300);
        assert_eq!(r.frames(), sent, "muted: nothing from the new device either");
        assert!(!r.gate.is_open());
        r.muted.store(false, Ordering::Relaxed);
        r.capture(300);
        assert!(r.frames() > sent + 25, "unmuted, the new device is heard");
        assert!(r.gate.is_open(), "the fresh gate opened on speech");
        assert!(r.gate.level_db() > -30.0);
    }

    #[test]
    fn unplugged_with_an_error_or_in_silence_the_call_goes_on_from_the_default() {
        let mut r = rig(Some(S44));
        r.capture(100);
        let t0 = Instant::now();
        // The driver reports it: the audio thread gets the message.
        r.bank.set_listed(S44, false);
        r.route.on_lost(1, t0);
        assert!(r.bank.open_names().is_empty());
        r.route.on_timer(t0 + Duration::from_millis(350));
        assert_eq!(r.bank.open_names(), [M48]);
        assert_eq!(r.said.lock().pop(), Some(MicStatus::FellBack { wanted: S44.into(), device: M48.into() }));
        let got = r.capture(600);
        assert!((tone_hz(&got) - 440.0).abs() < 4.0);
        // Back: it takes over.
        r.bank.set_listed(S44, true);
        let t1 = t0 + Duration::from_secs(5);
        r.route.on_timer(t1);
        r.route.on_timer(t1 + HEALTH_TICK);
        assert_eq!(r.bank.open_names(), [S44]);
        assert_eq!(r.said.lock().pop(), Some(MicStatus::Restored { device: S44.into() }));
        // This time nothing says it: the callback just stops (the time flows below by hand).
        r.bank.set_silent(S44, true);
        r.bank.set_listed(S44, false);
        r.capture(100);
        let mut t = t1 + HEALTH_TICK;
        for _ in 0..5 {
            t += HEALTH_TICK;
            r.route.on_timer(t);
        }
        assert!(t > t1 + STALL_AFTER);
        assert_eq!(r.bank.open_names(), [M48]);
        assert!(r.capture(600).iter().any(|&s| s != 0));
    }

    /// The whole thing on the real audio thread: [`AudioIo`]'s messages reach the route.
    #[test]
    fn audio_io_switches_on_its_own_thread_and_stops_capturing_when_dropped() {
        let bank = bank();
        let mixer = Arc::new(Mixer::new());
        let gate = Arc::new(VoiceGate::default());
        let counters = Arc::new(StreamCounters::default());
        let muted = Arc::new(AtomicBool::new(false));
        let said = Arc::new(Mutex::new(Vec::new()));
        let s2 = said.clone();
        let mut c = cfg(&bank, &mixer, &gate, &muted, &counters);
        c.device = Some("não existe".into());
        c.on_status = Some(Arc::new(move |m| s2.lock().push(m)));
        let io = AudioIo::start(Some(c), None);
        assert!(io.mic_ok, "the default stands in for a device that is gone: {:?}", io.mic_error);
        assert_eq!(bank.open_names(), [M48]);
        assert_eq!(said.lock().as_slice(), [MicStatus::FellBack { wanted: "não existe".into(), device: M48.into() }]);
        io.set_mic_device(Some(H16.into()));
        let deadline = Instant::now() + Duration::from_secs(5);
        while bank.open_names() != [H16] && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(bank.open_names(), [H16]);
        // A stale report does nothing; a real one reopens the device on that thread.
        bank.report_lost(H16);
        let deadline = Instant::now() + Duration::from_secs(5);
        while bank.opens() < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(bank.opens(), 3);
        assert_eq!(bank.open_names(), [H16]);
        drop(io);
        assert!(bank.open_names().is_empty(), "leaving the room closes the microphone");
    }
}
