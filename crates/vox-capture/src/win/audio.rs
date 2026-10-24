//! WASAPI process loopback: capture only one process tree's audio (or everything except it).
//!
//! The stream is opened on the `VAD\Process_Loopback` virtual device with an explicit
//! 48 kHz / stereo / f32 format (AUTOCONVERTPCM does the mix-format conversion and SRC). We take float
//! and convert to i16 ourselves: the engine's own s16 path adds +-1 LSB dither, which would keep a
//! "silent" stream from ever being digital zero (bad for Opus DTX). Output is re-chunked into exact 10 ms blocks. When the target
//! is silent, process loopback may deliver nothing at all; a high-resolution periodic timer then
//! keeps emitting zero chunks on a steady cadence so downstream encoder timing stays smooth.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::{implement, Interface, HRESULT, PCWSTR};
use windows::Win32::Foundation::{E_NOINTERFACE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    ActivateAudioInterfaceAsync, PROCESS_LOOPBACK_MODE, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::BLOB;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateWaitableTimerExW,
    OpenProcess, SetWaitableTimer, WaitForMultipleObjects, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
    INFINITE, PROCESS_QUERY_LIMITED_INFORMATION, TIMER_ALL_ACCESS,
};
use windows::Win32::System::Variant::VT_BLOB;

use super::enumerate::exe_name_of;
use super::{monotonic_now_us, os, ComInit, Event, OwnedHandle};
use crate::{
    AudioCallback, AudioChunk, AudioTarget, CaptureError, Result, AUDIO_CHANNELS,
    AUDIO_CHUNK_FRAMES, AUDIO_SAMPLE_RATE,
};

const CHUNK_SAMPLES: usize = AUDIO_CHUNK_FRAMES * AUDIO_CHANNELS as usize;
const CHUNK_US: i64 = 10_000;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
/// WASAPI buffer. Capture buffers don't add latency (we drain on every event), so be generous.
const BUFFER_HNS: i64 = 500_000; // 50 ms
/// Synthesize silence only once no packet has arrived for this long (stream starved)...
const STARVED_US: i64 = 25_000;
/// ...and keep the synthetic timeline this far behind "now", so late packets still fit.
const SILENCE_LAG_US: i64 = 20_000;
/// If we fall this far behind (thread stalled), jump instead of flooding zeros.
const MAX_CATCHUP_US: i64 = 500_000;
const ACTIVATE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct AudioInner {
    stop: Arc<Event>,
    thread: Option<JoinHandle<()>>,
    running: Arc<AtomicBool>,
}

impl AudioInner {
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

impl Drop for AudioInner {
    fn drop(&mut self) {
        self.stop.set();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The process loopback mode and target pid for `target`. A whole-screen share (and "Todo o som
/// do PC") asks for everything *except* our own process tree: every sound concord makes (the
/// call's voices, watched streams, the soundboard, alerts) is played by cpal inside this process,
/// so viewers never hear the call echoed back.
pub(crate) fn loopback_mode(target: AudioTarget) -> (u32, PROCESS_LOOPBACK_MODE) {
    match target {
        AudioTarget::ProcessTree { pid } => (pid, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE),
        AudioTarget::SystemExcludingProcessTree { pid } => (pid, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE),
    }
}

/// Said once per run: exclude-mode process loopback is missing on this Windows.
static EXCLUDE_UNAVAILABLE_LOGGED: AtomicBool = AtomicBool::new(false);

pub fn start_audio(target: AudioTarget, on_audio: AudioCallback) -> Result<AudioInner> {
    let res = start_audio_inner(target, on_audio);
    if let (Err(CaptureError::Unsupported(why)), AudioTarget::SystemExcludingProcessTree { .. }) = (&res, target) {
        // The behaviour of before stays: the share goes on without the PC's sound. Plain endpoint
        // loopback is never used instead, since it would send the call's own voices back.
        if !EXCLUDE_UNAVAILABLE_LOGGED.swap(true, Ordering::Relaxed) {
            tracing::warn!(reason = %why, "system sound without our own: exclude-mode process loopback is unavailable on this Windows (needs Windows 10 2004+); sharing without the PC's sound");
        }
    }
    res
}

fn start_audio_inner(target: AudioTarget, on_audio: AudioCallback) -> Result<AudioInner> {
    let (pid, _) = loopback_mode(target);
    // Process loopback happily "captures" a pid that doesn't exist (silence forever); fail early.
    match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(h) => drop(OwnedHandle(h)),
        Err(e) => {
            return Err(CaptureError::NotFound(format!(
                "process {pid}: {}",
                e.message()
            )))
        }
    }

    let stop = Arc::new(Event::new(true)?);
    let running = Arc::new(AtomicBool::new(true));
    let (tx, rx) = mpsc::sync_channel::<Result<()>>(1);
    let thread = {
        let stop = stop.clone();
        let running = running.clone();
        std::thread::Builder::new()
            .name("vox-audio".into())
            .spawn(move || {
                let _com = ComInit::mta();
                let _mmcss = Mmcss::pro_audio();
                match Stream::open(target) {
                    Ok(mut s) => {
                        let _ = tx.send(Ok(()));
                        s.run(&stop, on_audio);
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
                running.store(false, Ordering::Relaxed);
            })
            .map_err(|e| CaptureError::Os(format!("spawn audio thread: {e}")))?
    };
    match rx.recv() {
        Ok(Ok(())) => Ok(AudioInner {
            stop,
            thread: Some(thread),
            running,
        }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(CaptureError::Stopped(
                "audio thread exited during setup".into(),
            ))
        }
    }
}

/// MMCSS "Pro Audio" registration for the capture thread, reverted on drop.
struct Mmcss(Option<windows::Win32::Foundation::HANDLE>);

impl Mmcss {
    fn pro_audio() -> Self {
        let mut idx = 0u32;
        match unsafe { AvSetMmThreadCharacteristicsW(windows::core::w!("Pro Audio"), &mut idx) } {
            Ok(h) => Self(Some(h)),
            Err(e) => {
                tracing::warn!(
                    hr = format!("0x{:08X}", e.code().0 as u32),
                    "MMCSS registration failed"
                );
                Self(None)
            }
        }
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        if let Some(h) = self.0 {
            unsafe {
                let _ = AvRevertMmThreadCharacteristics(h);
            }
        }
    }
}

type ActivateResult = windows_core::Result<IAudioClient>;

/// COM completion callback for `ActivateAudioInterfaceAsync` (agile by default via #[implement]).
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivateHandler {
    tx: Mutex<Option<mpsc::Sender<ActivateResult>>>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivateHandler_Impl {
    fn ActivateCompleted(
        &self,
        op: windows_core::Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows_core::Result<()> {
        let result = (|| -> ActivateResult {
            let op = op.ok()?;
            let mut hr = HRESULT(0);
            let mut unk = None;
            unsafe { op.GetActivateResult(&mut hr, &mut unk)? };
            hr.ok()?;
            unk.ok_or_else(|| windows_core::Error::from(E_NOINTERFACE))?
                .cast::<IAudioClient>()
        })();
        if let Some(tx) = self.tx.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = tx.send(result);
        }
        Ok(())
    }
}

struct Stream {
    target: AudioTarget,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: Event,
    timer: OwnedHandle,
    fifo: Vec<i16>,
    /// Timestamp of the next chunk to emit (continuous, advances exactly 10 ms per chunk).
    timeline_us: i64,
    last_packet_us: i64,
    // stats
    chunks: u64,
    silent_chunks: u64,
    packets: u64,
    discontinuities: u64,
    stats_gate: vox_log::RateLimiter,
}

impl Stream {
    fn open(target: AudioTarget) -> Result<Self> {
        let (pid, mode) = loopback_mode(target);
        let params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: mode,
                },
            },
        };
        // PROPVARIANT(VT_BLOB) pointing at `params` on our stack. The `windows` crate gives
        // PROPVARIANT a Drop that calls PropVariantClear, which would CoTaskMemFree our stack
        // pointer (heap-corruption fail-fast 0xC0000374), so it must never be dropped.
        let pv = std::mem::ManuallyDrop::new(PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_BLOB,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: PROPVARIANT_0_0_0 {
                        blob: BLOB {
                            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                            pBlobData: &params as *const _ as *mut u8,
                        },
                    },
                }),
            },
        });

        let (tx, rx) = mpsc::channel::<ActivateResult>();
        let handler: IActivateAudioInterfaceCompletionHandler = ActivateHandler {
            tx: Mutex::new(Some(tx)),
        }
        .into();
        let _op = unsafe {
            ActivateAudioInterfaceAsync(
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                &IAudioClient::IID,
                Some(&*pv as *const PROPVARIANT),
                &handler,
            )
        }
        .map_err(|e| unsupported_or_os("ActivateAudioInterfaceAsync(process loopback)", e))?;
        let client = match rx.recv_timeout(ACTIVATE_TIMEOUT) {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => return Err(unsupported_or_os("process loopback activation", e)),
            Err(_) => {
                return Err(CaptureError::Os(
                    "process loopback activation timed out".into(),
                ))
            }
        };

        let block_align = AUDIO_CHANNELS * 4; // f32
        let wfx = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: AUDIO_CHANNELS,
            nSamplesPerSec: AUDIO_SAMPLE_RATE,
            nAvgBytesPerSec: AUDIO_SAMPLE_RATE * block_align as u32,
            nBlockAlign: block_align,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        let flags = AUDCLNT_STREAMFLAGS_LOOPBACK
            | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER_HNS, 0, &wfx, None) }
            .map_err(|e| os("IAudioClient::Initialize(48k/2ch/f32)", e))?;
        let event = Event::new(false)?;
        unsafe { client.SetEventHandle(event.raw()) }
            .map_err(|e| os("IAudioClient::SetEventHandle", e))?;
        let capture: IAudioCaptureClient =
            unsafe { client.GetService() }.map_err(|e| os("GetService(IAudioCaptureClient)", e))?;

        let timer = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
            .or_else(|_| CreateWaitableTimerExW(None, PCWSTR::null(), 0, TIMER_ALL_ACCESS.0))
            .map_err(|e| os("CreateWaitableTimerExW", e))?
        };
        let timer = OwnedHandle(timer);
        let due: i64 = -(CHUNK_US * 10);
        unsafe { SetWaitableTimer(timer.0, &due, (CHUNK_US / 1000) as i32, None, None, false) }
            .map_err(|e| os("SetWaitableTimer", e))?;

        unsafe { client.Start() }.map_err(|e| os("IAudioClient::Start", e))?;
        let excluding = mode == PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE;
        tracing::info!(
            target = ?target,
            mode = if excluding { "exclude" } else { "include" },
            own_process = pid == std::process::id(),
            exe = %exe_name_of(pid),
            "audio capture started (process loopback, 48kHz f32->s16 stereo)"
        );
        let now = monotonic_now_us();
        Ok(Self {
            target,
            client,
            capture,
            event,
            timer,
            fifo: Vec::with_capacity(CHUNK_SAMPLES * 8),
            timeline_us: now,
            last_packet_us: now,
            chunks: 0,
            silent_chunks: 0,
            packets: 0,
            discontinuities: 0,
            stats_gate: vox_log::RateLimiter::new(),
        })
    }

    fn run(&mut self, stop: &Event, mut cb: AudioCallback) {
        let handles = [stop.raw(), self.event.raw(), self.timer.0];
        let _ = self.stats_gate.ready(vox_log::STATS_INTERVAL);
        let mut chunk = [0i16; CHUNK_SAMPLES];
        let reason: String = loop {
            let r = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
            match r.0.wrapping_sub(WAIT_OBJECT_0.0) {
                0 => break "stopped by owner".into(),
                1 | 2 => {}
                _ => {
                    break format!(
                        "WaitForMultipleObjects failed: {:?}",
                        windows_core::Error::from_thread()
                    )
                }
            }
            if let Err(e) = self.drain_packets() {
                break format!("{e}");
            }
            self.emit(&mut chunk, &mut cb);
            if self.stats_gate.ready(vox_log::STATS_INTERVAL) {
                tracing::info!(
                    chunks = self.chunks,
                    synthesized_silence = self.silent_chunks,
                    packets = self.packets,
                    discontinuities = self.discontinuities,
                    "audio capture stats"
                );
            }
        };
        unsafe {
            let _ = self.client.Stop();
        }
        tracing::info!(reason = %reason, target = ?self.target, chunks = self.chunks, synthesized_silence = self.silent_chunks, discontinuities = self.discontinuities, "audio capture stopped");
    }

    fn drain_packets(&mut self) -> Result<()> {
        loop {
            let n = unsafe { self.capture.GetNextPacketSize() }
                .map_err(|e| os("GetNextPacketSize", e))?;
            if n == 0 {
                return Ok(());
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            unsafe {
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
            }
            .map_err(|e| os("IAudioCaptureClient::GetBuffer", e))?;
            let samples = frames as usize * AUDIO_CHANNELS as usize;
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                self.fifo.resize(self.fifo.len() + samples, 0);
            } else {
                // SAFETY: WASAPI hands us `frames` frames of the format we initialized (f32 stereo).
                let s = unsafe { std::slice::from_raw_parts(data as *const f32, samples) };
                self.fifo.extend(
                    s.iter()
                        .map(|&x| (x * 32767.0).round().clamp(-32768.0, 32767.0) as i16),
                );
            }
            if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                self.discontinuities += 1;
            }
            unsafe { self.capture.ReleaseBuffer(frames) }
                .map_err(|e| os("IAudioCaptureClient::ReleaseBuffer", e))?;
            self.packets += 1;
            self.last_packet_us = monotonic_now_us();
        }
    }

    fn emit(&mut self, chunk: &mut [i16; CHUNK_SAMPLES], cb: &mut AudioCallback) {
        // 1) Real data, in exact 10 ms chunks.
        let mut off = 0;
        while self.fifo.len() - off >= CHUNK_SAMPLES {
            chunk.copy_from_slice(&self.fifo[off..off + CHUNK_SAMPLES]);
            off += CHUNK_SAMPLES;
            self.send(chunk, cb);
        }
        if off > 0 {
            self.fifo.drain(..off);
        }

        // 2) Starved (target silent / not playing): keep the 10 ms cadence with zeros.
        let now = monotonic_now_us();
        if now - self.last_packet_us < STARVED_US {
            return;
        }
        let due = now - SILENCE_LAG_US;
        if due - self.timeline_us > MAX_CATCHUP_US {
            tracing::debug!(behind_us = due - self.timeline_us, "audio timeline jump");
            self.timeline_us = due - CHUNK_US;
        }
        if self.timeline_us + CHUNK_US > due {
            return;
        }
        if !self.fifo.is_empty() {
            // Tail of a sound: pad the partial chunk with zeros.
            let n = self.fifo.len();
            chunk[..n].copy_from_slice(&self.fifo);
            chunk[n..].fill(0);
            self.fifo.clear();
            self.send(chunk, cb);
        }
        chunk.fill(0);
        while self.timeline_us + CHUNK_US <= due {
            self.silent_chunks += 1;
            self.send(chunk, cb);
        }
    }

    fn send(&mut self, chunk: &[i16; CHUNK_SAMPLES], cb: &mut AudioCallback) {
        cb(&AudioChunk {
            samples: chunk,
            sample_rate: AUDIO_SAMPLE_RATE,
            channels: AUDIO_CHANNELS,
            timestamp_us: self.timeline_us,
        });
        self.timeline_us += CHUNK_US;
        self.chunks += 1;
    }
}

fn unsupported_or_os(ctx: &str, e: windows_core::Error) -> CaptureError {
    use windows::Win32::Foundation::{E_INVALIDARG, E_NOTIMPL};
    if e.code() == E_NOTIMPL || e.code() == E_INVALIDARG {
        CaptureError::Unsupported(format!(
            "{ctx}: hr=0x{:08X} (process loopback needs Windows 10 build 20348+ / Windows 11)",
            e.code().0 as u32
        ))
    } else {
        os(ctx, e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_screen_share_excludes_our_own_process_tree() {
        let me = std::process::id();
        assert_eq!(loopback_mode(AudioTarget::SystemExcludingProcessTree { pid: me }), (me, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE));
        assert_eq!(loopback_mode(AudioTarget::ProcessTree { pid: 42 }), (42, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE));
    }

    /// Opens the real exclude-mode stream for this test process for a moment. Nothing is kept:
    /// only the number of 10 ms chunks is counted (no samples are read or stored). It taps the
    /// machine's sound, so it is ignored by default and, even with `--ignored`, only runs with
    /// `VOX_TEST_ALLOW_REAL_CAPTURE=1` (a person at the PC asked for it).
    #[test]
    #[ignore]
    fn exclude_mode_opens_for_our_own_tree() {
        use std::sync::atomic::AtomicU64;
        if std::env::var("VOX_TEST_ALLOW_REAL_CAPTURE").as_deref() != Ok("1") {
            eprintln!("skipped: taps the PC's real sound; set VOX_TEST_ALLOW_REAL_CAPTURE=1 to run it");
            return;
        }
        let n = Arc::new(AtomicU64::new(0));
        let c = n.clone();
        let cap = start_audio(AudioTarget::SystemExcludingProcessTree { pid: std::process::id() }, Box::new(move |_| {
            c.fetch_add(1, Ordering::Relaxed);
        }))
        .expect("exclude-mode process loopback");
        std::thread::sleep(Duration::from_millis(400));
        assert!(cap.is_running());
        drop(cap);
        let chunks = n.load(Ordering::Relaxed);
        assert!(chunks >= 20, "only {chunks} chunks in 400 ms");
    }
}
