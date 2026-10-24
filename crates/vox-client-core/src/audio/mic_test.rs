//! "Testar microfone": open the selected mic (in or out of a voice channel), run it through the
//! same chain as the real thing (echo cancellation, AGC, the selected noise suppression, then the
//! gate) and
//! play the result back on the selected output, so you hear yourself as the others would.
//!
//! It is entirely local: the processed audio goes into a private mixer, never to a room. The
//! devices are open only while a [`MicTest`] exists; dropping it closes them and the threads end.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use super::device::{new_voice_apm, AudioIo, MicConfig, MicSink, PlaybackConfig};
use super::denoise::NoiseControl;
use super::gate::VoiceGate;
use super::mixer::Mixer;
use crate::stats::StreamCounters;

pub struct MicTestConfig {
    /// Substring of the input device name; `None` = system default.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    /// Play the mic back. False = meter only (no output device is opened).
    pub playback: bool,
    /// Output volume, 1.0 = unity.
    pub volume: f32,
    /// Gate settings in, meter out. Use a gate of its own, not the voice session's.
    pub gate: Arc<VoiceGate>,
    /// Noise suppression level (shared with the voice session: changes apply live).
    pub noise: Arc<NoiseControl>,
}

pub struct MicTest {
    io: Option<AudioIo>,
    gate: Arc<VoiceGate>,
    volume: Arc<AtomicU32>,
    /// Why the speakers could not be opened (the meter still works).
    pub playback_error: Option<String>,
    /// The mic is being played back.
    pub playing: bool,
}

impl MicTest {
    /// Open the devices. Blocks until they are open or failed (at most `DEVICE_OPEN_TIMEOUT`):
    /// call it from a blocking context. An unusable microphone is an error.
    pub fn start(cfg: MicTestConfig) -> Result<MicTest, String> {
        let apm = Some(new_voice_apm(&cfg.noise));
        let mixer = Arc::new(Mixer::new());
        let volume = Arc::new(AtomicU32::new(cfg.volume.clamp(0.0, 2.0).to_bits()));
        let mic = MicConfig {
            device: cfg.input_device,
            sink: if cfg.playback { MicSink::Loopback(mixer.clone()) } else { MicSink::Discard },
            muted: Arc::new(AtomicBool::new(false)),
            apm: apm.clone(),
            counters: Arc::new(StreamCounters::default()),
            gate: cfg.gate.clone(),
            noise: cfg.noise.clone(),
            // A test of the chosen device: another one in its place would prove nothing.
            fall_back: false,
            on_status: None,
            fake: None,
        };
        let playback = cfg.playback.then(|| PlaybackConfig {
            device: cfg.output_device,
            mixer,
            deafened: Arc::new(AtomicBool::new(false)),
            volume: volume.clone(),
            apm,
        });
        let io = AudioIo::start(Some(mic), playback);
        if !io.mic_ok {
            let why = io.mic_error.clone().unwrap_or_else(|| "unknown error".into());
            drop(io);
            cfg.gate.clear_outputs();
            return Err(why);
        }
        let playing = cfg.playback && io.playback_ok;
        let playback_error = if cfg.playback { io.playback_error.clone() } else { None };
        Ok(MicTest { io: Some(io), gate: cfg.gate, volume, playback_error, playing })
    }

    /// Output volume while running (1.0 = unity, up to 2.0).
    pub fn set_volume(&self, volume: f32) {
        self.volume.store(volume.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }
}

impl Drop for MicTest {
    fn drop(&mut self) {
        drop(self.io.take());
        self.gate.clear_outputs();
    }
}
