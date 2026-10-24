//! cpal microphone capture and speaker playback.
//!
//! Both streams live on one dedicated thread (cpal streams are not `Send` on every platform);
//! dropping [`AudioIo`] stops and joins it.
//!
//! A stream the system invalidates while in use (macOS: the device's sample rate changed, e.g.
//! another app or the Bluetooth headset switched modes; any OS: the device went away) is reopened
//! on that thread by itself, with the same settings, retrying with a growing delay while the
//! device is missing: nobody has to leave and rejoin the room for the microphone to come back.
//! The microphone goes further ([`super::mic_route`]): it changes device mid-call, falls back to
//! the system default when the chosen one disappears, and notices a stream that died in silence.
//!
//! Mic: device -> mono f32 -> (resample to 48 kHz) -> 10 ms i16 chunks -> WebRTC APM
//! (AEC3 + AGC + HPF, plus its noise suppressor at the "Padrão" level) -> AI denoiser (RNNoise at
//! "IA leve", see `audio::denoise`) -> voice changer ("Modificador de voz", `audio::voicefx`; a
//! bit-exact bypass when off) -> level meter + voice-activity gate -> `NativeAudioSource` (or, for
//! the mic test, a local loopback mixer). When muted the callback returns at once: the device
//! stays open (instant unmute) but nothing is processed or sent.
//!
//! Playback: mixer (48 kHz stereo) -> 10 ms blocks -> APM reverse stream (far-end reference
//! for the echo canceller) -> (resample) -> device channel layout.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig, I24};
use futures_util::FutureExt;
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::native::apm::AudioProcessingModule;
use parking_lot::Mutex;
use tracing::{info, warn};

use super::denoise::{Denoiser, NoiseControl};
use super::fake_mic::FakeMics;
use super::gate::{GateProcessor, VoiceGate};
use super::mic_route::{reopen_delay, MicBackend, MicOpen, MicRoute, MicStatus};
use super::mixer::{Mixer, SourceId, MIX_10MS, MIX_CHANNELS, MIX_RATE};
use super::resample::LinearResampler;
use super::voicefx::VoiceChanger;
use crate::stats::StreamCounters;

pub type SharedApm = Arc<VoiceApm>;

/// Mono 10 ms at 48 kHz.
const MIC_CHUNK: usize = 480;
/// Fixed render->capture delay hint for AEC3 (it also estimates the delay itself).
const APM_STREAM_DELAY_MS: i32 = 40;

/// The voice WebRTC APM (echo cancellation, gain control, high-pass, optional noise
/// suppression), shared by the mic callback (capture side) and the playback callback (the far-end
/// reference). The LiveKit wrapper fixes its configuration at creation, so turning the noise
/// suppressor on or off builds a new one (on the caller's thread) and swaps it in; the echo
/// canceller re-converges within about a second.
pub struct VoiceApm {
    apm: Mutex<AudioProcessingModule>,
    /// Whether the current APM runs its noise suppressor (the lock also serialises rebuilds).
    ns: Mutex<bool>,
}

impl std::fmt::Debug for VoiceApm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceApm").field("ns", &*self.ns.lock()).finish()
    }
}

fn build_apm(ns: bool) -> AudioProcessingModule {
    AudioProcessingModule::new(true, true, true, ns)
}

impl VoiceApm {
    pub fn new(noise_suppression: bool) -> Self {
        Self { apm: Mutex::new(build_apm(noise_suppression)), ns: Mutex::new(noise_suppression) }
    }

    pub fn noise_suppression(&self) -> bool {
        *self.ns.lock()
    }

    /// Turn the APM's own noise suppressor on or off (no-op when it already is).
    pub fn set_noise_suppression(&self, on: bool) {
        let mut ns = self.ns.lock();
        if *ns == on {
            return;
        }
        let fresh = build_apm(on);
        let old = std::mem::replace(&mut *self.apm.lock(), fresh);
        *ns = on;
        drop(ns);
        drop(old); // outside the audio lock
        info!(apm_noise_suppression = on, "voice APM rebuilt");
    }

    /// Process one or more 10 ms capture chunks in place.
    pub(crate) fn process_capture(&self, chunk: &mut [i16]) -> bool {
        let mut apm = self.apm.lock();
        let _ = apm.set_stream_delay_ms(APM_STREAM_DELAY_MS);
        apm.process_stream(chunk, MIX_RATE as i32, 1).is_ok()
    }

    /// Feed one 10 ms playback block (the far-end reference).
    fn process_render(&self, block: &mut [i16]) {
        let _ = self.apm.lock().process_reverse_stream(block, MIX_RATE as i32, MIX_CHANNELS as i32);
    }
}

/// Create the voice APM with its noise suppressor following `noise`'s level.
pub fn new_voice_apm(noise: &NoiseControl) -> SharedApm {
    let apm = Arc::new(VoiceApm::new(noise.level().apm_ns()));
    noise.attach(&apm);
    apm
}

/// Where the processed mic chunks go.
#[derive(Clone)]
pub enum MicSink {
    /// Published to the room.
    Source(NativeAudioSource),
    /// Played back locally through this mixer (the mic test). Nothing leaves the machine.
    Loopback(Arc<Mixer>),
    /// Metered only.
    Discard,
}

#[derive(Clone)]
pub struct MicConfig {
    pub device: Option<String>,
    pub sink: MicSink,
    pub muted: Arc<AtomicBool>,
    pub apm: Option<SharedApm>,
    pub counters: Arc<StreamCounters>,
    /// Voice-activity gate settings in, level meter out.
    pub gate: Arc<VoiceGate>,
    /// Noise suppression level (the AI denoiser reads it on every chunk) and the voice changer
    /// setting it carries.
    pub noise: Arc<NoiseControl>,
    /// The chosen device is gone or will not open: capture the system default instead of nothing.
    pub fall_back: bool,
    /// Told when the microphone changes by itself or as asked (see [`MicStatus`]), on the audio
    /// thread.
    pub on_status: Option<Arc<dyn Fn(MicStatus) + Send + Sync>>,
    /// Autotests: these made-up devices instead of the real ones.
    pub fake: Option<FakeMics>,
}

#[derive(Clone)]
pub struct PlaybackConfig {
    pub device: Option<String>,
    pub mixer: Arc<Mixer>,
    pub deafened: Arc<AtomicBool>,
    /// Master output volume, `f32` bits (1.0 = unity).
    pub volume: Arc<AtomicU32>,
    pub apm: Option<SharedApm>,
}

/// How long the audio devices get to open before the join gives up on them. Opening a device
/// normally takes tens of milliseconds; a hang here means a broken driver or audio service.
pub const DEVICE_OPEN_TIMEOUT: Duration = Duration::from_secs(8);
/// How long dropping [`AudioIo`] waits for its thread before leaving it behind.
const DEVICE_CLOSE_TIMEOUT: Duration = Duration::from_secs(3);

pub struct AudioIo {
    stop: Option<std::sync::mpsc::Sender<Ctl>>,
    thread: Option<JoinHandle<()>>,
    abandoned: Arc<AtomicBool>,
    pub mic_ok: bool,
    pub playback_ok: bool,
    /// Why the microphone could not be opened (`None` when it opened or was not requested).
    pub mic_error: Option<String>,
    /// Why the speakers could not be opened (`None` when they opened or were not requested).
    pub playback_error: Option<String>,
}

enum Opened {
    Mic(Result<(), String>),
    Playback(Result<(), String>),
}

/// What the audio thread is told.
enum Ctl {
    Stop,
    /// The system invalidated the microphone stream of this generation: reopen it.
    MicLost { gen: u64 },
    /// The system invalidated the speakers' stream: reopen it.
    OutputLost,
    /// The person picked another microphone (`None`: the system default).
    SetMic(Option<String>),
}

/// Errors after which a cpal stream is dead and must be built again.
fn stream_lost(kind: cpal::ErrorKind) -> bool {
    use cpal::ErrorKind as E;
    match kind {
        E::StreamInvalidated | E::DeviceNotAvailable => true,
        E::Xrun | E::DeviceChanged | E::RealtimeDenied => false,
        // WASAPI's capture and render threads end after reporting anything else (the audio service
        // restarted, another app took the device, a driver error): the stream will never call
        // again. CoreAudio reports a failed render this way and carries on.
        _ => cfg!(windows),
    }
}

impl AudioIo {
    /// Open the requested streams. Device failures are logged and reported via `mic_ok` /
    /// `mic_error` (and the playback twins); they never fail the voice session (a user without
    /// a mic can still listen). A device that does not answer within [`DEVICE_OPEN_TIMEOUT`] is
    /// reported as failed and closed if it ever finishes opening.
    pub fn start(mic: Option<MicConfig>, playback: Option<PlaybackConfig>) -> AudioIo {
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<Ctl>();
        let reopen_tx = stop_tx.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Opened>();
        let want_mic = mic.is_some();
        let want_out = playback.is_some();
        let abandoned = Arc::new(AtomicBool::new(false));
        let gave_up = abandoned.clone();
        let thread = std::thread::Builder::new()
            .name("vox-audio-io".into())
            .spawn(move || {
                let host = cpal::default_host();
                let out_lost = || {
                    let tx = reopen_tx.clone();
                    move || {
                        let _ = tx.send(Ctl::OutputLost);
                    }
                };
                let mut route = mic.map(|cfg| {
                    let tx = reopen_tx.clone();
                    let lost: Arc<dyn Fn(u64) + Send + Sync> = Arc::new(move |gen| {
                        let _ = tx.send(Ctl::MicLost { gen });
                    });
                    let (status, fall_back, wanted) = (cfg.on_status.clone(), cfg.fall_back, cfg.device.clone());
                    let backend: Box<dyn MicBackend> = match cfg.fake.clone() {
                        Some(fake) => Box::new(fake.backend(cfg)),
                        None => Box::new(CpalMics { host: cpal::default_host(), cfg }),
                    };
                    let mut route = MicRoute::new(backend, lost, status, fall_back, wanted);
                    let started = Instant::now();
                    let res = route.start(started);
                    match &res {
                        Ok(()) => info!(ms = started.elapsed().as_millis() as u64, "microphone opened"),
                        Err(e) => warn!(error = %e, "microphone unavailable; continuing without mic"),
                    }
                    let _ = ready_tx.send(Opened::Mic(res));
                    route
                });
                if gave_up.load(Ordering::Relaxed) {
                    return; // the caller already moved on; do not keep the mic open
                }
                let mut out_stream = playback.as_ref().and_then(|cfg| {
                    let started = std::time::Instant::now();
                    let res = open_playback(&host, cfg.clone(), out_lost());
                    let _ = ready_tx.send(Opened::Playback(match &res {
                        Ok(_) => Ok(()),
                        Err(e) => Err(format!("{e:#}")),
                    }));
                    match res {
                        Ok(s) => {
                            info!(ms = started.elapsed().as_millis() as u64, "speaker output opened");
                            Some(s)
                        }
                        Err(e) => {
                            warn!(error = %format!("{e:#}"), "speaker output unavailable; continuing without playback");
                            None
                        }
                    }
                });
                if gave_up.load(Ordering::Relaxed) {
                    return;
                }
                // Until told to stop: reopen what the system invalidates, follow the microphone.
                // `out_retry`: the speakers' attempt number and when it is due.
                let mut out_retry: Option<(u32, Instant)> = None;
                loop {
                    let now = Instant::now();
                    let out_wait = out_retry.map(|(_, at)| at.saturating_duration_since(now));
                    let wait = [route.as_ref().and_then(|r| r.next_wait(now)), out_wait].into_iter().flatten().min();
                    let msg = match wait {
                        Some(d) => match stop_rx.recv_timeout(d) {
                            Ok(m) => Some(m),
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Some(Ctl::Stop),
                        },
                        None => Some(stop_rx.recv().unwrap_or(Ctl::Stop)),
                    };
                    let now = Instant::now();
                    match (msg, route.as_mut()) {
                        (Some(Ctl::Stop), _) => break,
                        (Some(Ctl::MicLost { gen }), Some(r)) => r.on_lost(gen, now),
                        (Some(Ctl::SetMic(device)), Some(r)) => r.set_device(device, now),
                        (Some(Ctl::OutputLost), _) if out_retry.is_none() => {
                            warn!("speaker stream lost; reopening it");
                            out_retry = Some((0, now + reopen_delay(0)));
                            // Drop the dead stream now (it may still hold the device).
                            out_stream = None;
                        }
                        _ => {}
                    }
                    if let Some(r) = route.as_mut() {
                        r.on_timer(now);
                    }
                    if let (Some((n, at)), Some(cfg)) = (out_retry, playback.as_ref()) {
                        if now >= at {
                            match open_playback(&host, cfg.clone(), out_lost()) {
                                Ok(s) => {
                                    info!(attempt = n + 1, "speaker output reopened");
                                    out_stream = Some(s);
                                    out_retry = None;
                                }
                                Err(e) => {
                                    if n < 3 || n % 12 == 0 {
                                        warn!(attempt = n + 1, error = %format!("{e:#}"), "speaker output not back yet");
                                    }
                                    out_retry = Some((n + 1, now + reopen_delay(n + 1)));
                                }
                            }
                        }
                    }
                }
                drop(route);
                drop(out_stream);
            })
            .expect("spawn audio io thread");

        let deadline = std::time::Instant::now() + DEVICE_OPEN_TIMEOUT;
        let mut mic_res = (!want_mic).then_some(Ok(()));
        let mut out_res = (!want_out).then_some(Ok(()));
        while mic_res.is_none() || out_res.is_none() {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match ready_rx.recv_timeout(left) {
                Ok(Opened::Mic(r)) => mic_res = Some(r),
                Ok(Opened::Playback(r)) => out_res = Some(r),
                Err(_) => break,
            }
        }
        let hung = format!("the audio device did not answer within {} s", DEVICE_OPEN_TIMEOUT.as_secs());
        if mic_res.is_none() || out_res.is_none() {
            abandoned.store(true, Ordering::Relaxed);
            warn!(mic_pending = mic_res.is_none(), playback_pending = out_res.is_none(), "audio device open timed out");
        }
        let mic_res = mic_res.unwrap_or_else(|| Err(hung.clone()));
        let out_res = out_res.unwrap_or_else(|| Err(hung.clone()));
        if want_mic || want_out {
            info!(mic_ok = want_mic && mic_res.is_ok(), playback_ok = want_out && out_res.is_ok(), "audio devices opened");
        }
        AudioIo {
            stop: Some(stop_tx),
            thread: Some(thread),
            abandoned,
            mic_ok: want_mic && mic_res.is_ok(),
            playback_ok: want_out && out_res.is_ok(),
            mic_error: if want_mic { mic_res.err() } else { None },
            playback_error: if want_out { out_res.err() } else { None },
        }
    }

    /// Capture from another microphone (`None`: the system default), now. Does not wait: the
    /// outcome comes back through [`MicConfig::on_status`]. Nothing happens without a microphone.
    pub fn set_mic_device(&self, device: Option<String>) {
        if let Some(tx) = &self.stop {
            let _ = tx.send(Ctl::SetMic(device));
        }
    }
}

impl Drop for AudioIo {
    fn drop(&mut self) {
        // (The streams' error callbacks hold senders too: say it, do not just hang up.)
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(Ctl::Stop);
        }
        let Some(t) = self.thread.take() else { return };
        // A wedged audio driver must not wedge leaving the room: wait a little, then let go.
        let deadline = std::time::Instant::now() + if self.abandoned.load(Ordering::Relaxed) { Duration::ZERO } else { DEVICE_CLOSE_TIMEOUT };
        while !t.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if t.is_finished() {
            let _ = t.join();
        } else {
            warn!("audio thread did not stop in time; leaving it behind");
        }
    }
}

/// Names of the available capture (`input = true`) or playback devices on the default host,
/// for pickers. Pass a name (or a unique part of it) as `VoiceConfig::input_device` /
/// `output_device`.
pub fn list_devices(input: bool) -> Vec<String> {
    let host = cpal::default_host();
    let devices = if input { host.input_devices() } else { host.output_devices() };
    let mut names: Vec<String> = devices.map(|it| it.filter_map(|d| d.description().ok().map(|d| d.name().to_string())).collect()).unwrap_or_default();
    names.dedup();
    names
}

/// Name of the system default capture / playback device.
pub fn default_device_name(input: bool) -> Option<String> {
    let host = cpal::default_host();
    let d = if input { host.default_input_device() } else { host.default_output_device() };
    d.map(|d| device_name(&d))
}

fn find_device(host: &cpal::Host, name: Option<&str>, input: bool) -> Result<cpal::Device> {
    if let Some(want) = name {
        let want = want.to_lowercase();
        let devices: Vec<cpal::Device> = if input {
            host.input_devices().context("enumerate input devices")?.collect()
        } else {
            host.output_devices().context("enumerate output devices")?.collect()
        };
        let named: Vec<(String, cpal::Device)> = devices.into_iter().filter_map(|d| Some((d.description().ok()?.name().to_lowercase(), d))).collect();
        return match best_match(named.iter().map(|(n, _)| n.as_str()), &want) {
            Some(i) => Ok(named.into_iter().nth(i).expect("an index into the list").1),
            None => Err(anyhow!("no {} device matching {want:?}", if input { "input" } else { "output" })),
        };
    }
    let d = if input { host.default_input_device() } else { host.default_output_device() };
    d.ok_or_else(|| anyhow!("no default {} device", if input { "input" } else { "output" }))
}

/// Which of the (lowercased) device names `want` means: the same name if there is one (the picker
/// saves full names, and "microphone (headset)" must not open "headset microphone (headset)"), else
/// the first one containing it (`--input-device usb`).
fn best_match<'a>(names: impl Iterator<Item = &'a str> + Clone, want: &str) -> Option<usize> {
    names.clone().position(|n| n == want).or_else(|| names.clone().position(|n| n.contains(want)))
}

fn device_name(d: &cpal::Device) -> String {
    d.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "?".into())
}

// ---------------------------------------------------------------- mic

enum SinkState {
    Source(NativeAudioSource),
    Loopback { mixer: Arc<Mixer>, id: SourceId, stereo: Vec<i16> },
    Discard,
}

impl SinkState {
    fn new(sink: MicSink) -> Self {
        match sink {
            MicSink::Source(s) => SinkState::Source(s),
            MicSink::Loopback(mixer) => {
                let id = mixer.add_source();
                SinkState::Loopback { mixer, id, stereo: Vec::with_capacity(MIX_10MS) }
            }
            MicSink::Discard => SinkState::Discard,
        }
    }

    /// Hand one 10 ms mono chunk on; false when it could not be delivered.
    fn send(&mut self, chunk: &[i16]) -> bool {
        match self {
            SinkState::Source(source) => {
                let frame = AudioFrame {
                    data: Cow::Borrowed(chunk),
                    sample_rate: MIX_RATE,
                    num_channels: 1,
                    samples_per_channel: chunk.len() as u32,
                };
                // queue_size_ms = 0 => synchronous fast path, the future is ready on first poll.
                matches!(source.capture_frame(&frame).now_or_never(), Some(Ok(())))
            }
            SinkState::Loopback { mixer, id, stereo } => {
                stereo.clear();
                for &s in chunk {
                    stereo.push(s);
                    stereo.push(s);
                }
                mixer.push(*id, stereo);
                true
            }
            SinkState::Discard => true,
        }
    }
}

impl Drop for SinkState {
    fn drop(&mut self) {
        if let SinkState::Loopback { mixer, id, .. } = self {
            mixer.remove_source(*id);
        }
    }
}

pub(super) struct MicState {
    channels: usize,
    sink: SinkState,
    muted: Arc<AtomicBool>,
    apm: Option<SharedApm>,
    counters: Arc<StreamCounters>,
    gate: GateProcessor,
    denoiser: Denoiser,
    fx: VoiceChanger,
    resampler: Option<LinearResampler>,
    mono: Vec<f32>,
    resampled: Vec<f32>,
    chunk: Vec<i16>,
    was_muted: bool,
    /// Bumped by every callback, muted or not: the capture is alive ([`super::mic_route`]).
    pulse: Arc<AtomicU64>,
}

impl MicState {
    pub(super) fn on_data<T: SizedSample>(&mut self, data: &[T])
    where
        f32: FromSample<T>,
    {
        self.pulse.fetch_add(1, Ordering::Relaxed);
        // Mute wins over everything (gate, push-to-talk): no processing, nothing sent, meter blank.
        if self.muted.load(Ordering::Relaxed) {
            if !self.was_muted {
                self.was_muted = true;
                self.chunk.clear();
                self.gate.reset();
                self.denoiser.reset();
                self.fx.reset();
            }
            return;
        }
        self.was_muted = false;
        let ch = self.channels;
        let mut mono = std::mem::take(&mut self.mono);
        mono.clear();
        let inv = 1.0 / ch as f32;
        for frame in data.chunks_exact(ch) {
            let sum: f32 = frame.iter().map(|&s| s.to_sample::<f32>()).sum();
            mono.push(sum * inv);
        }
        let mut resampled = std::mem::take(&mut self.resampled);
        let samples: &[f32] = match self.resampler.as_mut() {
            Some(r) => {
                resampled.clear();
                r.process(&mono, &mut resampled);
                &resampled
            }
            None => &mono,
        };
        for &s in samples {
            self.chunk.push((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
            if self.chunk.len() == MIC_CHUNK {
                self.flush_chunk();
            }
        }
        self.mono = mono;
        self.resampled = resampled;
    }

    fn flush_chunk(&mut self) {
        if let Some(apm) = &self.apm {
            if !apm.process_capture(&mut self.chunk) {
                self.counters.errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        // AI denoiser (a bit-exact bypass at the "Padrão" level), before the meter and the gate
        // so both see what is actually sent.
        self.denoiser.process(&mut self.chunk);
        // Voice changer on the denoised voice (a bit-exact bypass when off), before the meter and
        // the gate so they see the voice that is sent.
        self.fx.process(&mut self.chunk);
        // Level meter + voice-activity gate on what would be sent; a closed gate turns the chunk
        // into digital silence (which the Opus DTX makes nearly free).
        self.gate.process(&mut self.chunk);
        if self.sink.send(&self.chunk) {
            self.counters.audio(&self.chunk);
        } else {
            self.counters.errors.fetch_add(1, Ordering::Relaxed);
        }
        self.chunk.clear();
    }
}

impl Drop for MicState {
    /// The stream is gone: the meter and the "speaking" ring must not stay on its last value.
    fn drop(&mut self) {
        self.gate.reset();
    }
}

/// The capture chain for one stream of a device with this format: everything that depends on the
/// format is here, so a new stream never inherits the previous device's.
pub(super) fn new_mic_state(cfg: &MicConfig, channels: usize, rate: u32, pulse: Arc<AtomicU64>) -> MicState {
    MicState {
        channels,
        sink: SinkState::new(cfg.sink.clone()),
        muted: cfg.muted.clone(),
        apm: cfg.apm.clone(),
        counters: cfg.counters.clone(),
        gate: GateProcessor::new(cfg.gate.clone()),
        fx: VoiceChanger::new(cfg.noise.clone()),
        denoiser: Denoiser::new(cfg.noise.clone()),
        resampler: (rate != MIX_RATE).then(|| LinearResampler::new(rate, MIX_RATE, 1)),
        mono: Vec::with_capacity(4096),
        resampled: Vec::with_capacity(4096),
        chunk: Vec::with_capacity(MIC_CHUNK),
        was_muted: false,
        pulse,
    }
}

/// The real devices, for [`MicRoute`].
struct CpalMics {
    host: cpal::Host,
    cfg: MicConfig,
}

impl MicBackend for CpalMics {
    fn open(&mut self, device: Option<&str>, on_lost: Box<dyn Fn() + Send>) -> Result<MicOpen, String> {
        open_mic(&self.host, &self.cfg, device, on_lost).map_err(|e| format!("{e:#}"))
    }

    fn present(&mut self, name: &str) -> bool {
        find_device(&self.host, Some(name), true).is_ok()
    }

    fn default_id(&mut self) -> Option<String> {
        self.host.default_input_device().and_then(|d| d.id().ok()).map(|id| id.to_string())
    }
}

fn open_mic(host: &cpal::Host, cfg: &MicConfig, name: Option<&str>, on_lost: impl Fn() + Send + 'static) -> Result<MicOpen> {
    let device = find_device(host, name, true)?;
    let supported = device.default_input_config().context("default input config")?;
    let config: StreamConfig = supported.config();
    let channels = config.channels as usize;
    let rate = config.sample_rate;
    info!(device = %device_name(&device), rate, channels, format = ?supported.sample_format(), "opening microphone");
    let pulse = Arc::new(AtomicU64::new(0));
    let state = new_mic_state(cfg, channels, rate, pulse.clone());
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build_mic::<f32>(&device, &config, state, on_lost)?,
        SampleFormat::F64 => build_mic::<f64>(&device, &config, state, on_lost)?,
        SampleFormat::I16 => build_mic::<i16>(&device, &config, state, on_lost)?,
        SampleFormat::I24 => build_mic::<I24>(&device, &config, state, on_lost)?,
        SampleFormat::I32 => build_mic::<i32>(&device, &config, state, on_lost)?,
        SampleFormat::I64 => build_mic::<i64>(&device, &config, state, on_lost)?,
        SampleFormat::U8 => build_mic::<u8>(&device, &config, state, on_lost)?,
        SampleFormat::U16 => build_mic::<u16>(&device, &config, state, on_lost)?,
        other => return Err(anyhow!("unsupported mic sample format {other:?}")),
    };
    stream.play().context("start mic stream")?;
    Ok(MicOpen { name: device_name(&device), id: device.id().ok().map(|id| id.to_string()), pulse, _stream: Box::new(stream) })
}

fn build_mic<T: SizedSample>(device: &cpal::Device, config: &StreamConfig, mut state: MicState, on_lost: impl Fn() + Send + 'static) -> Result<cpal::Stream>
where
    f32: FromSample<T>,
{
    static ERR_LOG: vox_log::RateLimiter = vox_log::RateLimiter::new();
    device
        .build_input_stream::<T, _, _>(
            *config,
            move |data: &[T], _| state.on_data(data),
            move |e| {
                let lost = stream_lost(e.kind());
                if lost || ERR_LOG.ready(vox_log::STATS_INTERVAL) {
                    warn!(error = %e, lost, "microphone stream error");
                }
                if lost {
                    on_lost();
                }
            },
            None,
        )
        .context("build mic stream")
}

// ---------------------------------------------------------------- playback

struct PlaybackState {
    channels: usize,
    mixer: Arc<Mixer>,
    deafened: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    apm: Option<SharedApm>,
    resampler: Option<LinearResampler>,
    block: Vec<i16>,
    rev: Vec<i16>,
    block_f32: Vec<f32>,
    resampled: Vec<f32>,
    fifo: VecDeque<f32>,
}

impl PlaybackState {
    fn pull_block(&mut self) {
        self.block.resize(MIX_10MS, 0);
        self.mixer.mix(&mut self.block);
        let deafened = self.deafened.load(Ordering::Relaxed);
        if deafened {
            self.block.iter_mut().for_each(|s| *s = 0);
        }
        // Alert sounds go in after the deafen silencing: deafen mutes people, not the user's own
        // feedback sounds (so the undeafen chime is heard).
        let alert = self.mixer.mix_ui(&mut self.block);
        if let (Some(apm), true) = (&self.apm, !deafened || alert) {
            // Far-end reference for AEC3 (only what is actually played, alerts included).
            self.rev.clear();
            self.rev.extend_from_slice(&self.block);
            apm.process_render(&mut self.rev);
        }
        let gain = f32::from_bits(self.volume.load(Ordering::Relaxed)) / 32768.0;
        self.block_f32.clear();
        self.block_f32.extend(self.block.iter().map(|&s| s as f32 * gain));
        match self.resampler.as_mut() {
            Some(r) => {
                self.resampled.clear();
                r.process(&self.block_f32, &mut self.resampled);
                self.fifo.extend(self.resampled.iter().copied());
            }
            None => self.fifo.extend(self.block_f32.iter().copied()),
        }
    }

    fn on_data<T: SizedSample + FromSample<f32>>(&mut self, out: &mut [T]) {
        let ch = self.channels;
        let frames = out.len() / ch;
        while self.fifo.len() < frames * MIX_CHANNELS {
            self.pull_block();
        }
        for frame in out.chunks_exact_mut(ch) {
            let l = self.fifo.pop_front().unwrap_or(0.0);
            let r = self.fifo.pop_front().unwrap_or(0.0);
            match ch {
                1 => frame[0] = T::from_sample((l + r) * 0.5),
                _ => {
                    frame[0] = T::from_sample(l);
                    frame[1] = T::from_sample(r);
                    for s in frame.iter_mut().skip(2) {
                        *s = T::from_sample(0.0f32);
                    }
                }
            }
        }
    }
}

fn open_playback(host: &cpal::Host, cfg: PlaybackConfig, on_lost: impl Fn() + Send + 'static) -> Result<cpal::Stream> {
    let device = find_device(host, cfg.device.as_deref(), false)?;
    let supported = device.default_output_config().context("default output config")?;
    let config: StreamConfig = supported.config();
    let channels = config.channels as usize;
    let rate = config.sample_rate;
    info!(device = %device_name(&device), rate, channels, format = ?supported.sample_format(), "opening speaker output");
    let state = PlaybackState {
        channels,
        mixer: cfg.mixer,
        deafened: cfg.deafened,
        volume: cfg.volume,
        apm: cfg.apm,
        resampler: (rate != MIX_RATE).then(|| LinearResampler::new(MIX_RATE, rate, MIX_CHANNELS)),
        block: Vec::with_capacity(MIX_10MS),
        rev: Vec::with_capacity(MIX_10MS),
        block_f32: Vec::with_capacity(MIX_10MS),
        resampled: Vec::with_capacity(MIX_10MS * 2),
        fifo: VecDeque::with_capacity(MIX_10MS * 8),
    };
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build_out::<f32>(&device, &config, state, on_lost)?,
        SampleFormat::I16 => build_out::<i16>(&device, &config, state, on_lost)?,
        SampleFormat::I32 => build_out::<i32>(&device, &config, state, on_lost)?,
        SampleFormat::U16 => build_out::<u16>(&device, &config, state, on_lost)?,
        other => return Err(anyhow!("unsupported output sample format {other:?}")),
    };
    stream.play().context("start output stream")?;
    Ok(stream)
}

fn build_out<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: &StreamConfig,
    mut state: PlaybackState,
    on_lost: impl Fn() + Send + 'static,
) -> Result<cpal::Stream> {
    static ERR_LOG: vox_log::RateLimiter = vox_log::RateLimiter::new();
    device
        .build_output_stream::<T, _, _>(
            *config,
            move |out: &mut [T], _| state.on_data(out),
            move |e| {
                let lost = stream_lost(e.kind());
                if lost || ERR_LOG.ready(vox_log::STATS_INTERVAL) {
                    warn!(error = %e, lost, "speaker stream error");
                }
                if lost {
                    on_lost();
                }
            },
            None,
        )
        .context("build output stream")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A playback callback state with no device behind it (the test pulls blocks by hand).
    fn state(deafened: bool) -> (PlaybackState, Arc<Mixer>) {
        let mixer = Arc::new(Mixer::new());
        let st = PlaybackState {
            channels: 2,
            mixer: mixer.clone(),
            deafened: Arc::new(AtomicBool::new(deafened)),
            volume: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            apm: None,
            resampler: None,
            block: Vec::new(),
            rev: Vec::new(),
            block_f32: Vec::new(),
            resampled: Vec::new(),
            fifo: VecDeque::new(),
        };
        (st, mixer)
    }

    fn pulled(st: &mut PlaybackState) -> Vec<f32> {
        st.fifo.clear();
        st.pull_block();
        st.fifo.iter().copied().collect()
    }

    #[test]
    fn deafen_silences_voices_but_not_alert_sounds() {
        let (mut st, mixer) = state(true);
        let voice = mixer.add_source();
        mixer.push(voice, &vec![8000i16; 6 * MIX_10MS]);
        mixer.play_ui(Arc::from(vec![1000i16; 4 * MIX_10MS]), 1.0);
        let block = pulled(&mut st);
        assert_eq!(block.len(), MIX_10MS);
        let want = 1000.0 / 32768.0;
        assert!(block.iter().all(|&s| (s - want).abs() < 1e-6), "only the alert is heard while deafened");
        // Same without deafen: voice and alert add up.
        let (mut st, mixer) = state(false);
        let voice = mixer.add_source();
        mixer.push(voice, &vec![8000i16; 6 * MIX_10MS]);
        mixer.play_ui(Arc::from(vec![1000i16; 4 * MIX_10MS]), 1.0);
        let block = pulled(&mut st);
        let want = 9000.0 / 32768.0;
        assert!(block.iter().all(|&s| (s - want).abs() < 1e-6));
    }

    #[test]
    fn deafen_silences_soundboard_clips_unlike_alerts_and_undeafen_does_not_replay_them() {
        let (mut st, mixer) = state(true);
        mixer.play_sound(Arc::from(vec![4000i16; 6 * 480]), 1);
        mixer.play_ui(Arc::from(vec![1000i16; 4 * MIX_10MS]), 1.0);
        let block = pulled(&mut st);
        let want = 1000.0 / 32768.0;
        assert!(block.iter().all(|&s| (s - want).abs() < 1e-6), "deafened: the alert only, not the clip");
        assert_eq!(mixer.sounds_active(), 1, "the silenced clip is still consumed, not queued up");
        // Without deafen the clip is heard on top of the alert (its default gain is 100 %).
        let (mut st, mixer) = state(false);
        mixer.play_sound(Arc::from(vec![4000i16; 6 * 480]), 1);
        mixer.play_ui(Arc::from(vec![1000i16; 4 * MIX_10MS]), 1.0);
        let block = pulled(&mut st);
        let want = 5000.0 / 32768.0;
        assert!(block.iter().all(|&s| (s - want).abs() < 1e-6), "{:?}", &block[..2]);
        // The block that is played is the one that feeds the echo canceller's reference, and the
        // clip is part of it (it is mixed in `Mixer::mix`, before the deafen silencing).
    }

    #[test]
    fn alert_sounds_follow_the_output_volume_and_end() {
        let (mut st, mixer) = state(false);
        st.volume.store(0.5f32.to_bits(), Ordering::Relaxed);
        mixer.play_ui(Arc::from(vec![2000i16; MIX_10MS]), 1.0);
        let first = pulled(&mut st);
        assert!(first.iter().all(|&s| (s - 1000.0 / 32768.0).abs() < 1e-6));
        let second = pulled(&mut st);
        assert!(second.iter().all(|&s| s == 0.0), "nothing left after the alert ended");
        assert_eq!(mixer.ui_active(), 0);
    }

    /// A mic callback state with no device behind it; chunks are fed by hand.
    fn mic_state(gate: &Arc<VoiceGate>, muted: bool) -> (MicState, Arc<AtomicBool>, Arc<Mixer>) {
        let muted = Arc::new(AtomicBool::new(muted));
        let mixer = Arc::new(Mixer::new());
        let st = MicState {
            channels: 1,
            sink: SinkState::new(MicSink::Loopback(mixer.clone())),
            muted: muted.clone(),
            apm: None,
            counters: Arc::new(StreamCounters::default()),
            gate: GateProcessor::new(gate.clone()),
            denoiser: Denoiser::new(Arc::new(NoiseControl::new(crate::audio::NoiseLevel::Standard))),
            fx: VoiceChanger::new(Arc::new(NoiseControl::new(crate::audio::NoiseLevel::Standard))),
            resampler: None,
            mono: Vec::new(),
            resampled: Vec::new(),
            chunk: Vec::new(),
            was_muted: false,
            pulse: Arc::new(AtomicU64::new(0)),
        };
        (st, muted, mixer)
    }

    fn loud_chunk() -> Vec<f32> {
        (0..MIC_CHUNK).map(|i| (i as f32 * 0.13).sin() * 0.3).collect()
    }

    #[test]
    fn mute_wins_over_push_to_talk_and_the_gate() {
        use crate::audio::gate::{GateConfig, GateMode};
        // A gate that would never open, with push-to-talk on (which bypasses it).
        let gate = Arc::new(VoiceGate::new(GateConfig { mode: GateMode::Manual, threshold_db: -10.0, hold_ms: 350 }));
        gate.set_ptt(true);
        let (mut st, muted, _mixer) = mic_state(&gate, true);
        let frames = |st: &MicState| st.counters.frames.load(Ordering::Relaxed);
        for _ in 0..5 {
            st.on_data(&loud_chunk());
        }
        assert_eq!(frames(&st), 0, "muted: nothing is sent");
        assert!(!gate.is_open());
        assert_eq!(gate.level_db(), crate::audio::gate::MIN_DB, "muted: the meter is blank");
        // Unmuted with push-to-talk: sent, open, not faded.
        muted.store(false, Ordering::Relaxed);
        st.on_data(&loud_chunk());
        assert_eq!(frames(&st), 1);
        assert!(gate.is_open());
        assert!(gate.level_db() > -20.0);
        // Mute again: closes at once.
        muted.store(true, Ordering::Relaxed);
        st.on_data(&loud_chunk());
        assert_eq!(frames(&st), 1);
        assert!(!gate.is_open());
    }

    #[test]
    fn noise_level_drives_the_apm_suppressor_and_the_denoiser_live() {
        use crate::audio::gate::{GateConfig, GateMode};
        use crate::audio::NoiseLevel;
        let noise = Arc::new(NoiseControl::new(NoiseLevel::AiLight));
        let apm = new_voice_apm(&noise);
        assert!(!apm.noise_suppression(), "IA leve: the APM's own suppressor is off");
        let gate = Arc::new(VoiceGate::new(GateConfig { mode: GateMode::Manual, threshold_db: -70.0, hold_ms: 100 }));
        let (mut st, _muted, _mixer) = mic_state(&gate, false);
        st.apm = Some(apm.clone());
        st.denoiser = Denoiser::new(noise.clone());
        for k in 0..60 {
            match k {
                20 => noise.set_level(NoiseLevel::Standard),
                40 => noise.set_level(NoiseLevel::AiLight),
                _ => {}
            }
            st.on_data(&loud_chunk());
            assert_eq!(apm.noise_suppression(), noise.level().apm_ns());
            assert_eq!(st.denoiser.active(), noise.level().is_ai());
        }
        assert_eq!(st.counters.frames.load(Ordering::Relaxed), 60, "every chunk was sent");
        assert_eq!(st.counters.errors.load(Ordering::Relaxed), 0);
        // RNNoise ran for chunks 0-20 (fade-out included) and 40-59.
        assert_eq!(st.denoiser.frames, 41);
        // A second chain (the mic test) follows the same control; dropped APMs are forgotten.
        let other = new_voice_apm(&noise);
        noise.set_level(NoiseLevel::Standard);
        assert!(apm.noise_suppression() && other.noise_suppression());
        drop(other);
        noise.set_level(NoiseLevel::AiLight);
        assert!(!apm.noise_suppression());
        // "IA máxima" keeps the APM's suppressor off too (no rebuild from IA leve).
        noise.set_level(NoiseLevel::AiMax);
        assert!(!apm.noise_suppression());
        noise.set_level(NoiseLevel::Standard);
        assert!(apm.noise_suppression());
    }

    #[test]
    fn voice_changer_runs_after_the_denoiser_and_before_the_gate() {
        use crate::audio::gate::{GateConfig, GateMode};
        use crate::audio::{NoiseLevel, VoiceEffect, VoiceFxConfig};
        // Half a 10 ms chunk of voice, then silence; what is sent in the chunks where the echo
        // comes back (280 ms later) tells whether the gate saw the processed voice.
        let run = |effect: VoiceEffect, mute_between: bool| {
            let gate = Arc::new(VoiceGate::new(GateConfig { mode: GateMode::Manual, threshold_db: -60.0, hold_ms: 100 }));
            let (mut st, muted, _mixer) = mic_state(&gate, false);
            let noise = Arc::new(NoiseControl::new(NoiseLevel::Standard));
            noise.voice_fx().set(VoiceFxConfig { effect, intensity_pct: 100 });
            st.denoiser = Denoiser::new(noise.clone());
            st.fx = VoiceChanger::new(noise);
            let mut echo_energy = 0;
            for k in 0..40 {
                if mute_between {
                    muted.store((10..12).contains(&k), Ordering::Relaxed);
                }
                let before = st.counters.sum_sq.load(Ordering::Relaxed);
                st.on_data(&if k < 3 { loud_chunk() } else { vec![0.0; MIC_CHUNK] });
                if (27..34).contains(&k) {
                    echo_energy += st.counters.sum_sq.load(Ordering::Relaxed) - before;
                }
            }
            echo_energy
        };
        assert_eq!(run(VoiceEffect::Off, false), 0, "no effect: silence after the voice");
        assert!(run(VoiceEffect::Echo, false) > 0, "the echo opened the gate and was sent");
        assert_eq!(run(VoiceEffect::Echo, true), 0, "muting dropped the echo tail");
    }

    /// CPU of the whole mic chain per level (APM + denoiser + gate, no device). Run by hand:
    /// `cargo test -p vox-client-core --release --lib mic_chain_cpu -- --ignored --nocapture`.
    #[test]
    #[ignore = "benchmark"]
    fn mic_chain_cpu() {
        use crate::audio::NoiseLevel;
        const CHUNKS: usize = 6_000; // 60 s of audio
        let mut x = 1u32;
        let input: Vec<f32> = (0..CHUNKS * MIC_CHUNK)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (i as f32 * 0.021).sin() * 0.2 * (1.0 + (i as f32 * 0.00002).sin()) + (x as f32 / u32::MAX as f32 - 0.5) * 0.05
            })
            .collect();
        for level in NoiseLevel::ALL {
            let noise = Arc::new(NoiseControl::new(level));
            let gate = Arc::new(VoiceGate::default());
            let (mut st, _muted, _mixer) = mic_state(&gate, false);
            st.sink = SinkState::Discard;
            st.apm = Some(new_voice_apm(&noise));
            st.denoiser = Denoiser::new(noise);
            // IA máxima: wait for each frame of its worker (the time below then includes the
            // worker's inference, as wall time), instead of replacing frames that are "late".
            st.denoiser.set_offline(true);
            let t = std::time::Instant::now();
            for c in input.as_chunks::<MIC_CHUNK>().0 {
                st.on_data(c);
            }
            let per = t.elapsed() / CHUNKS as u32;
            println!("{:9}: {per:?} per 10 ms chunk = {:.2}% of one core", level.label(), per.as_secs_f64() / 0.010 * 100.0);
            if level == NoiseLevel::AiMax {
                println!("           (DeepFilterNet worker alone: {:?} per frame)", st.denoiser.dfn().frame_time());
            }
        }
    }

    #[test]
    fn lost_streams_are_reopened_with_a_growing_delay() {
        assert!(stream_lost(cpal::ErrorKind::StreamInvalidated), "macOS: the device's sample rate changed");
        assert!(stream_lost(cpal::ErrorKind::DeviceNotAvailable), "unplugged");
        for k in [cpal::ErrorKind::Xrun, cpal::ErrorKind::DeviceChanged, cpal::ErrorKind::RealtimeDenied] {
            assert!(!stream_lost(k), "{k:?} keeps the stream");
        }
        // WASAPI's stream thread ends after any of these: nobody would ever reopen it.
        for k in [cpal::ErrorKind::BackendError, cpal::ErrorKind::DeviceBusy, cpal::ErrorKind::HostUnavailable] {
            assert_eq!(stream_lost(k), cfg!(windows), "{k:?}");
        }
        let d: Vec<Duration> = (0..8).map(reopen_delay).collect();
        assert_eq!(d[0], Duration::from_millis(300));
        assert!(d.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(d[7], Duration::from_secs(5), "then every 5 s");
    }

    #[test]
    fn a_device_is_found_by_its_exact_name_before_a_part_of_one() {
        let names = ["headset microphone (headset)", "microphone (headset)", "microphone (usb)"];
        let find = |want| best_match(names.iter().copied(), want);
        assert_eq!(find("microphone (headset)"), Some(1), "not the first one that contains it");
        assert_eq!(find("usb"), Some(2), "a part of a name still works (--input-device)");
        assert_eq!(find("webcam"), None);
    }

    #[test]
    fn a_stream_going_away_blanks_the_meter_and_the_speaking_state() {
        use crate::audio::gate::{GateConfig, GateMode};
        let gate = Arc::new(VoiceGate::new(GateConfig { mode: GateMode::Manual, threshold_db: -60.0, hold_ms: 100 }));
        let (mut st, _muted, _mixer) = mic_state(&gate, false);
        st.on_data(&loud_chunk());
        assert!(gate.is_open() && gate.level_db() > -30.0);
        drop(st);
        assert!(!gate.is_open(), "unplugged mid-word: nobody stays \"speaking\"");
        assert_eq!(gate.level_db(), crate::audio::gate::MIN_DB);
    }

    #[test]
    fn a_loopback_sink_leaves_the_mixer_when_dropped() {
        let mixer = Arc::new(Mixer::new());
        let before = mixer.source_count();
        let sink = SinkState::new(MicSink::Loopback(mixer.clone()));
        assert_eq!(mixer.source_count(), before + 1);
        drop(sink);
        assert_eq!(mixer.source_count(), before);
    }

    #[test]
    fn closed_gate_sends_silence_without_stopping_the_stream() {
        use crate::audio::gate::{GateConfig, GateMode};
        let gate = Arc::new(VoiceGate::new(GateConfig { mode: GateMode::Manual, threshold_db: -30.0, hold_ms: 100 }));
        let (mut st, _muted, _mixer) = mic_state(&gate, false);
        // Quiet room (-60 dBFS): frames keep flowing, as zeros.
        let quiet: Vec<f32> = (0..MIC_CHUNK).map(|i| (i as f32 * 0.13).sin() * 0.0014).collect();
        for _ in 0..10 {
            st.on_data(&quiet);
        }
        assert_eq!(st.counters.frames.load(Ordering::Relaxed), 10);
        assert_eq!(st.counters.sum_sq.load(Ordering::Relaxed), 0, "everything sent was digital silence");
        assert!(!gate.is_open());
        assert!(gate.level_db() < -55.0 && gate.level_db() > -65.0, "{}", gate.level_db());
        // Speech opens it and real signal is sent.
        for _ in 0..3 {
            st.on_data(&loud_chunk());
        }
        assert!(gate.is_open());
        assert!(st.counters.sum_sq.load(Ordering::Relaxed) > 0);
    }
}
