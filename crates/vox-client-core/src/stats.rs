//! Periodic resource and stream statistics (one info line per active stream + process line).
//!
//! Hot paths only bump atomics in [`StreamCounters`]; everything else (SDK `get_stats`, rate
//! computation, logging) happens here every `vox_log::STATS_INTERVAL` or on demand.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use livekit::prelude::Track;
use livekit::webrtc::stats::RtcStats;
use tracing::info;

/// Lock-free counters bumped from media threads.
#[derive(Default, Debug)]
pub struct StreamCounters {
    pub frames: AtomicU64,
    pub width: AtomicU32,
    pub height: AtomicU32,
    pub errors: AtomicU64,
    /// Sum of squared samples and sample count (audio level -> RMS between reports).
    pub sum_sq: AtomicU64,
    pub samples: AtomicU64,
    /// Stereo streams only ([`StreamCounters::audio_stereo`]): sum of squared `L - R` and the
    /// number of stereo frames behind it. Zero side energy over a stretch with sound means the
    /// audio is effectively mono.
    pub side_sq: AtomicU64,
    pub pairs: AtomicU64,
    /// Audio (v1.1 E1, measurements): when the last chunk with sound in it (above
    /// [`LOUD_MEAN_SQ`]) came, in ms since the process started, and the longest stretch without
    /// such a chunk since [`StreamCounters::take_max_quiet_ms`] (a tone that stops being heard,
    /// e.g. while keys rotate, shows here; silence frames keep the chunk count going).
    pub loud_at_ms: AtomicU64,
    pub max_quiet_ms: AtomicU64,
}

/// A chunk's mean square above this has sound in it (about -40 dBFS).
pub const LOUD_MEAN_SQ: u64 = 107_000;

fn process_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 + 1
}

impl StreamCounters {
    /// The longest stretch without sound since the last call (then starts again).
    pub fn take_max_quiet_ms(&self) -> u64 {
        self.max_quiet_ms.swap(0, Ordering::Relaxed)
    }

    pub fn frame(&self, width: u32, height: u32) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.width.store(width, Ordering::Relaxed);
        self.height.store(height, Ordering::Relaxed);
    }

    /// Count one audio chunk and accumulate its energy (a few hundred multiply-adds per 10 ms).
    pub fn audio(&self, samples: &[i16]) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        let sq: u64 = samples.iter().map(|&s| (s as i64 * s as i64) as u64).sum();
        self.sum_sq.fetch_add(sq, Ordering::Relaxed);
        self.samples.fetch_add(samples.len() as u64, Ordering::Relaxed);
        if !samples.is_empty() && sq / samples.len() as u64 > LOUD_MEAN_SQ {
            let now = process_ms();
            let before = self.loud_at_ms.swap(now, Ordering::Relaxed);
            if before != 0 {
                self.max_quiet_ms.fetch_max(now.saturating_sub(before), Ordering::Relaxed);
            }
        }
    }

    /// [`StreamCounters::audio`] for interleaved stereo, plus the energy of the difference
    /// between the channels (how much stereo there really is).
    pub fn audio_stereo(&self, samples: &[i16]) {
        self.audio(samples);
        let side: u64 = samples.as_chunks::<2>().0.iter().map(|p| (p[0] as i64 - p[1] as i64).pow(2) as u64).sum();
        self.side_sq.fetch_add(side, Ordering::Relaxed);
        self.pairs.fetch_add((samples.len() / 2) as u64, Ordering::Relaxed);
    }
}

/// Feedback from WebRTC to a pre-encoded video source (keyframe requests, congestion-control
/// bitrate), bumped from the media thread and reported every stats interval.
#[derive(Debug)]
pub struct EncodedFeedback {
    pub keyframe_requests: AtomicU64,
    /// Requests the source could not honour (file source).
    pub keyframe_requests_ignored: AtomicU64,
    pub keyframes_sent: AtomicU64,
    /// Bitrate changes forwarded to the encoder.
    pub rate_changes: AtomicU64,
    /// Last target forwarded to the encoder (bps); 0 = none yet.
    pub target_bps: AtomicU64,
    /// Lowest / highest WebRTC target since the last report (reset by the reporter).
    pub min_target_bps: AtomicU64,
    pub max_target_bps: AtomicU64,
    /// Access units WebRTC actually encoded (forwarded by the pass-through to the packetizer),
    /// seen through the sender's publish-timing events.
    pub webrtc_encoded: AtomicU64,
    /// Access units handed to `capture_encoded_frame` that WebRTC dropped before the
    /// pass-through (frame dropper, encoder queue, paused encoder): each one breaks the H.264
    /// reference chain for every viewer until the next IDR.
    pub webrtc_dropped: AtomicU64,
    /// Runs of consecutive drops (one recovery each).
    pub drop_events: AtomicU64,
    /// IDRs requested from the encoder because of a drop.
    pub recovery_idrs: AtomicU64,
    /// Delta frames sent with a broken reference chain (after a drop, before the next keyframe):
    /// what viewers see as smeared blocks. 0 = no corruption from WebRTC drops.
    pub broken_frames: AtomicU64,
    /// Delta frames held back after a drop until the next IDR (v0.7, see `vox_capture::IdrGate`):
    /// they would have been broken frames.
    pub withheld_frames: AtomicU64,
    /// Drop guard: current / lowest (since the last report) multiplier on the encoder target, %.
    pub guard_pct: AtomicU64,
    pub guard_min_pct: AtomicU64,
}

impl Default for EncodedFeedback {
    fn default() -> Self {
        Self {
            keyframe_requests: AtomicU64::new(0),
            keyframe_requests_ignored: AtomicU64::new(0),
            keyframes_sent: AtomicU64::new(0),
            rate_changes: AtomicU64::new(0),
            target_bps: AtomicU64::new(0),
            min_target_bps: AtomicU64::new(u64::MAX),
            max_target_bps: AtomicU64::new(0),
            webrtc_encoded: AtomicU64::new(0),
            webrtc_dropped: AtomicU64::new(0),
            drop_events: AtomicU64::new(0),
            recovery_idrs: AtomicU64::new(0),
            broken_frames: AtomicU64::new(0),
            withheld_frames: AtomicU64::new(0),
            guard_pct: AtomicU64::new(100),
            guard_min_pct: AtomicU64::new(100),
        }
    }
}

impl EncodedFeedback {
    /// Record a WebRTC rate-control target (before clamping/forwarding).
    pub fn observe_target(&self, bps: u64) {
        self.min_target_bps.fetch_min(bps, Ordering::Relaxed);
        self.max_target_bps.fetch_max(bps, Ordering::Relaxed);
    }
}

/// Extra reporting for a share published through the pre-encoded path.
#[derive(Clone)]
pub struct EncoderReport {
    /// "nvenc", "mf" or "h264file".
    pub backend: String,
    pub feedback: Arc<EncodedFeedback>,
    /// Hardware encoder statistics (absent for the file source).
    pub hw: Option<vox_capture::EncodedStats>,
}

/// RMS level in dBFS from accumulated energy; `None` when no samples (or digital silence).
pub fn rms_dbfs(sum_sq: u64, samples: u64) -> Option<f64> {
    if samples == 0 || sum_sq == 0 {
        return None;
    }
    let rms = (sum_sq as f64 / samples as f64).sqrt() / 32768.0;
    Some(20.0 * rms.log10())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Send,
    Recv,
}

/// A stream registered for reporting.
#[derive(Clone)]
pub struct StreamEntry {
    pub label: String,
    pub dir: Direction,
    pub is_video: bool,
    pub track: Option<Track>,
    pub counters: Arc<StreamCounters>,
    /// Set for pre-encoded video publishing.
    pub encoder: Option<EncoderReport>,
}

/// One rendered report line (also logged).
#[derive(Debug, Clone, Default)]
pub struct StreamReport {
    pub label: String,
    pub dir: &'static str,
    /// Frames per second counted by us (video frames or 10 ms audio chunks).
    pub fps: f64,
    pub width: u32,
    pub height: u32,
    /// Bitrate from the SDK's RTP stats (payload + headers excluded), when available.
    pub kbps: Option<f64>,
    /// SDK-reported frames per second (encoder output / decoder output).
    pub rtp_fps: Option<f64>,
    pub codec_impl: String,
    pub quality_limitation: String,
    /// Receive side, since the previous report: frames decoded / dropped by the decoder
    /// pipeline (WebRTC `framesDropped`), freezes, packets lost, keyframe requests sent.
    pub decoded: Option<u64>,
    pub decode_dropped: Option<u64>,
    pub freezes: Option<u64>,
    pub packets_lost: Option<i64>,
    pub plis: Option<u64>,
    /// Receive side, since the previous report: complete frames assembled by the jitter buffer,
    /// keyframes decoded and NACKs sent.
    pub received: Option<u64>,
    pub key_decoded: Option<u64>,
    pub nacks: Option<u64>,
    /// Send side (video), since the previous report: frames that entered WebRTC's video stream
    /// encoder (`media-source.frames`), frames it encoded (for the pre-encoded path: forwarded by
    /// the pass-through), frames sent as RTP, keyframes, and PLIs received. `src - encoded` are
    /// frames WebRTC dropped before the encoder (frame dropper, encoder queue, paused encoder).
    pub tx: Option<TxDelta>,
    /// Audio RMS level since the previous report (dBFS); `None` = silence / no data.
    pub rms_dbfs: Option<f64>,
    /// Stereo audio: RMS of the side signal `(L - R) / 2` since the previous report (dBFS);
    /// `None` = no difference between the channels (mono) or no data.
    pub side_dbfs: Option<f64>,
    /// Audio: negotiated codec as the SDK reports it ("audio/opus 48000 Hz ch=2 ...stereo=1").
    pub codec: String,
}

/// Per-interval send counters of a video track (see [`StreamReport::tx`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TxDelta {
    pub src: u64,
    pub encoded: u64,
    pub sent: u64,
    pub keyframes: u64,
    pub huge: u64,
    pub plis: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessSample {
    /// CPU usage over the sample window, as % of the whole machine (Task Manager style).
    pub cpu_pct: f64,
    /// CPU usage as % of one core.
    pub cpu_pct_core: f64,
    pub working_set_bytes: u64,
    pub private_bytes: u64,
    pub handles: u32,
}

/// Process CPU/memory sampler (GetProcessTimes / GetProcessMemoryInfo on Windows).
pub struct ProcessMeter {
    last_wall: Instant,
    last_cpu_100ns: u64,
    ncpu: f64,
}

impl Default for ProcessMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessMeter {
    pub fn new() -> Self {
        Self {
            last_wall: Instant::now(),
            last_cpu_100ns: process_cpu_100ns(),
            ncpu: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64,
        }
    }

    /// CPU% since the previous call (or since construction) plus current memory.
    pub fn sample(&mut self) -> ProcessSample {
        let now = Instant::now();
        let cpu = process_cpu_100ns();
        let wall = now.duration_since(self.last_wall).as_secs_f64().max(1e-6);
        let used = cpu.saturating_sub(self.last_cpu_100ns) as f64 / 1e7;
        self.last_wall = now;
        self.last_cpu_100ns = cpu;
        let core_pct = used / wall * 100.0;
        let (ws, private) = process_memory();
        ProcessSample {
            cpu_pct: core_pct / self.ncpu,
            cpu_pct_core: core_pct,
            working_set_bytes: ws,
            private_bytes: private,
            handles: process_handles(),
        }
    }
}

#[cfg(windows)]
pub(crate) fn process_cpu_100ns() -> u64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut c, mut e, mut k, mut u) = (z, z, z, z);
    // SAFETY: pseudo-handle of our own process, valid out-pointers.
    let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
    if ok == 0 {
        return 0;
    }
    let ft = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    ft(k) + ft(u)
}

#[cfg(windows)]
pub(crate) fn process_memory() -> (u64, u64) {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: zeroed POD struct, size passed matches, own-process pseudo-handle.
    unsafe {
        let mut pmc: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        if GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc as *mut _ as *mut PROCESS_MEMORY_COUNTERS, pmc.cb) == 0 {
            return (0, 0);
        }
        (pmc.WorkingSetSize as u64, pmc.PrivateUsage as u64)
    }
}

#[cfg(windows)]
fn process_handles() -> u32 {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
    let mut n = 0u32;
    // SAFETY: own-process pseudo-handle, valid out pointer.
    unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut n) };
    n
}

#[cfg(not(windows))]
fn process_handles() -> u32 {
    0
}

#[cfg(not(windows))]
pub(crate) fn process_cpu_100ns() -> u64 {
    0
}

#[cfg(not(windows))]
pub(crate) fn process_memory() -> (u64, u64) {
    (0, 0)
}

/// Return free heap memory to the OS (`HeapCompact` on every process heap) and, optionally,
/// trim the working set (`SetProcessWorkingSetSize(-1, -1)`; pages fault back in on use).
/// Experimental knob for memory measurements; see the `trim` REPL command.
pub fn trim_memory(trim_working_set: bool) -> usize {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::System::Memory::{GetProcessHeaps, HeapCompact};
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
        let mut heaps: Vec<HANDLE> = vec![std::ptr::null_mut(); 64];
        // SAFETY: buffer of `heaps.len()` handles; the call returns how many exist.
        let n = unsafe { GetProcessHeaps(heaps.len() as u32, heaps.as_mut_ptr()) } as usize;
        for h in heaps.iter().take(n.min(heaps.len())) {
            // SAFETY: heap handles just returned by GetProcessHeaps.
            unsafe { HeapCompact(*h, 0) };
        }
        if trim_working_set {
            // SAFETY: own-process pseudo-handle; (-1, -1) means "trim as much as possible".
            unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX) };
        }
        n
    }
    #[cfg(not(windows))]
    {
        let _ = trim_working_set;
        0
    }
}

#[derive(Default)]
struct Prev {
    at: Option<Instant>,
    frames: u64,
    bytes: u64,
    sum_sq: u64,
    samples: u64,
    side_sq: u64,
    pairs: u64,
    kf_req: u64,
    kf_ignored: u64,
    kf_sent: u64,
    rate_changes: u64,
    webrtc_encoded: u64,
    webrtc_dropped: u64,
    drop_events: u64,
    recovery_idrs: u64,
    broken_frames: u64,
    withheld_frames: u64,
    /// Cumulative receive counters from the previous report (decoded, dropped, freezes, lost, pli).
    rx: Option<RxTotals>,
    /// Cumulative send counters from the previous report.
    tx: Option<TxDelta>,
}

#[derive(Default, Clone, Copy)]
struct RxTotals {
    decoded: u64,
    dropped: u64,
    freezes: u64,
    lost: i64,
    plis: u64,
    received: u64,
    key_decoded: u64,
    nacks: u64,
}

/// Turns counters + SDK stats into rates between successive reports.
#[derive(Default)]
pub struct Reporter {
    meter: ProcessMeter,
    prev: HashMap<String, Prev>,
}

impl Reporter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build (and log) one report for the given streams plus the process line.
    pub async fn report(&mut self, streams: &[StreamEntry]) -> (Vec<StreamReport>, ProcessSample) {
        let now = Instant::now();
        let mut out = Vec::with_capacity(streams.len());
        for s in streams {
            let mut r = StreamReport {
                label: s.label.clone(),
                dir: if s.dir == Direction::Send { "send" } else { "recv" },
                width: s.counters.width.load(Ordering::Relaxed),
                height: s.counters.height.load(Ordering::Relaxed),
                ..Default::default()
            };
            let frames = s.counters.frames.load(Ordering::Relaxed);
            let sum_sq = s.counters.sum_sq.load(Ordering::Relaxed);
            let samples = s.counters.samples.load(Ordering::Relaxed);
            let side_sq = s.counters.side_sq.load(Ordering::Relaxed);
            let pairs = s.counters.pairs.load(Ordering::Relaxed);
            let mut bytes = None;
            let mut rx = None;
            let mut tx = None;
            if let Some(track) = &s.track {
                if let Ok(stats) = track.get_stats().await {
                    extract_rtp(&stats, s.dir, &mut r, &mut bytes);
                    rx = rx_totals(&stats);
                    if s.is_video && s.dir == Direction::Send {
                        tx = tx_totals(&stats);
                    }
                }
            }
            let prev = self.prev.entry(s.label.clone()).or_default();
            if let (Some(now_rx), Some(p)) = (rx, prev.rx) {
                r.decoded = Some(now_rx.decoded.saturating_sub(p.decoded));
                r.decode_dropped = Some(now_rx.dropped.saturating_sub(p.dropped));
                r.freezes = Some(now_rx.freezes.saturating_sub(p.freezes));
                r.packets_lost = Some(now_rx.lost - p.lost);
                r.plis = Some(now_rx.plis.saturating_sub(p.plis));
                r.received = Some(now_rx.received.saturating_sub(p.received));
                r.key_decoded = Some(now_rx.key_decoded.saturating_sub(p.key_decoded));
                r.nacks = Some(now_rx.nacks.saturating_sub(p.nacks));
            }
            if rx.is_some() {
                prev.rx = rx;
            }
            if let Some(now_tx) = tx {
                let p = prev.tx.unwrap_or_default();
                r.tx = Some(TxDelta {
                    src: now_tx.src.saturating_sub(p.src),
                    encoded: now_tx.encoded.saturating_sub(p.encoded),
                    sent: now_tx.sent.saturating_sub(p.sent),
                    keyframes: now_tx.keyframes.saturating_sub(p.keyframes),
                    huge: now_tx.huge.saturating_sub(p.huge),
                    plis: now_tx.plis.saturating_sub(p.plis),
                });
                prev.tx = Some(now_tx);
            }
            if let Some(at) = prev.at {
                let dt = now.duration_since(at).as_secs_f64().max(1e-3);
                r.fps = frames.saturating_sub(prev.frames) as f64 / dt;
                if let Some(b) = bytes {
                    r.kbps = Some(b.saturating_sub(prev.bytes) as f64 * 8.0 / dt / 1000.0);
                }
            }
            r.rms_dbfs = rms_dbfs(sum_sq.saturating_sub(prev.sum_sq), samples.saturating_sub(prev.samples));
            // side = (L - R) / 2, so its energy is a quarter of the summed (L - R)^2
            r.side_dbfs = rms_dbfs(side_sq.saturating_sub(prev.side_sq) / 4, pairs.saturating_sub(prev.pairs));
            prev.sum_sq = sum_sq;
            prev.samples = samples;
            prev.side_sq = side_sq;
            prev.pairs = pairs;
            prev.at = Some(now);
            prev.frames = frames;
            prev.bytes = bytes.unwrap_or(0);
            log_stream(&r, s.is_video);
            if let Some(e) = &s.encoder {
                log_encoder(e, prev);
            }
            out.push(r);
        }
        // Forget streams that went away.
        self.prev.retain(|k, _| streams.iter().any(|s| &s.label == k));

        let p = self.meter.sample();
        info!(
            cpu_pct = format!("{:.2}", p.cpu_pct),
            cpu_pct_of_core = format!("{:.1}", p.cpu_pct_core),
            working_set_mb = format!("{:.1}", p.working_set_bytes as f64 / 1048576.0),
            private_mb = format!("{:.1}", p.private_bytes as f64 / 1048576.0),
            handles = p.handles,
            streams = streams.len(),
            "process stats"
        );
        (out, p)
    }
}

fn extract_rtp(stats: &[RtcStats], dir: Direction, r: &mut StreamReport, bytes: &mut Option<u64>) {
    for st in stats {
        match (dir, st) {
            (Direction::Send, RtcStats::OutboundRtp(o)) => {
                *bytes = Some(bytes.unwrap_or(0) + o.sent.bytes_sent);
                if o.outbound.frames_per_second > 0.0 {
                    r.rtp_fps = Some(o.outbound.frames_per_second);
                }
                if o.outbound.frame_width > 0 {
                    r.width = o.outbound.frame_width;
                    r.height = o.outbound.frame_height;
                }
                if !o.outbound.encoder_implementation.is_empty() {
                    r.codec_impl = o.outbound.encoder_implementation.clone();
                }
                r.quality_limitation = format!("{:?}", o.outbound.quality_limitation_reason).to_lowercase();
            }
            (Direction::Recv, RtcStats::InboundRtp(i)) => {
                *bytes = Some(bytes.unwrap_or(0) + i.inbound.bytes_received);
                if i.inbound.frames_per_second > 0.0 {
                    r.rtp_fps = Some(i.inbound.frames_per_second);
                }
                if i.inbound.frame_width > 0 {
                    r.width = i.inbound.frame_width;
                    r.height = i.inbound.frame_height;
                }
                if !i.inbound.decoder_implementation.is_empty() {
                    r.codec_impl = i.inbound.decoder_implementation.clone();
                }
            }
            (_, RtcStats::Codec(c)) if c.codec.mime_type.starts_with("audio/") => {
                r.codec = format!("{} {} Hz ch={} {}", c.codec.mime_type, c.codec.clock_rate, c.codec.channels, c.codec.sdp_fmtp_line);
            }
            _ => {}
        }
    }
}

/// Cumulative video receive counters of a track's inbound RTP stats (`None` without any).
fn rx_totals(stats: &[RtcStats]) -> Option<RxTotals> {
    let mut t: Option<RxTotals> = None;
    for st in stats {
        if let RtcStats::InboundRtp(i) = st {
            if i.stream.kind != "video" {
                continue;
            }
            let a = t.get_or_insert_with(RxTotals::default);
            a.decoded += i.inbound.frames_decoded as u64;
            a.dropped += i.inbound.frames_dropped as u64;
            a.freezes += i.inbound.freeze_count as u64;
            a.lost += i.received.packets_lost;
            a.plis += i.inbound.pli_count as u64;
            a.received += i.inbound.frames_received;
            a.key_decoded += i.inbound.key_frames_decoded as u64;
            a.nacks += i.inbound.nack_count as u64;
        }
    }
    t
}

/// Cumulative video send counters of a local track's stats (`None` without outbound RTP stats).
fn tx_totals(stats: &[RtcStats]) -> Option<TxDelta> {
    let mut t: Option<TxDelta> = None;
    let mut src = 0u64;
    for st in stats {
        match st {
            RtcStats::OutboundRtp(o) if o.stream.kind == "video" => {
                let a = t.get_or_insert_with(TxDelta::default);
                a.encoded += o.outbound.frames_encoded as u64;
                a.sent += o.outbound.frames_sent as u64;
                a.keyframes += o.outbound.key_frames_encoded as u64;
                a.huge += o.outbound.huge_frames_sent as u64;
                a.plis += o.outbound.pli_count as u64;
            }
            RtcStats::MediaSource(m) if m.source.kind == "video" => src += m.video.frames as u64,
            _ => {}
        }
    }
    if let Some(a) = t.as_mut() {
        a.src = src;
    }
    t
}

/// Cumulative send-side health of a local video track, for the encoder safety net
/// (`golive::encoders::StormDetector`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SendHealth {
    /// Keyframe requests received (PLI + FIR).
    pub plis: u64,
    pub packets_sent: u64,
    /// Packets the receiver (the SFU) reports lost, from its RTCP receiver reports
    /// (remote-inbound-rtp), and its last `fractionLost` (0..1).
    pub packets_lost: i64,
    pub fraction_lost: f64,
}

pub fn send_health(stats: &[RtcStats]) -> SendHealth {
    let mut h = SendHealth::default();
    for st in stats {
        match st {
            RtcStats::OutboundRtp(o) if o.stream.kind == "video" => {
                h.plis += o.outbound.pli_count as u64 + o.outbound.fir_count as u64;
                h.packets_sent += o.sent.packets_sent;
            }
            RtcStats::RemoteInboundRtp(r) if r.stream.kind == "video" => {
                h.packets_lost += r.received.packets_lost;
                h.fraction_lost = h.fraction_lost.max(r.remote_inbound.fraction_lost);
            }
            _ => {}
        }
    }
    h
}

/// Cumulative receive-side health of a remote video track, for the viewer's decode-health
/// reports to the publisher (`golive::encoders::RxHealthReport`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RecvHealth {
    pub decoded: u64,
    /// Frames the decoder pipeline dropped (WebRTC `framesDropped`).
    pub dropped: u64,
    /// Our own downlink (SFU -> us).
    pub packets_lost: i64,
    pub packets_received: u64,
}

/// `None` without inbound video RTP stats (not subscribed yet, paused).
pub fn recv_health(stats: &[RtcStats]) -> Option<RecvHealth> {
    let mut h: Option<RecvHealth> = None;
    for st in stats {
        if let RtcStats::InboundRtp(i) = st {
            if i.stream.kind != "video" {
                continue;
            }
            let a = h.get_or_insert_with(RecvHealth::default);
            a.decoded += i.inbound.frames_decoded as u64;
            a.dropped += i.inbound.frames_dropped as u64;
            a.packets_lost += i.received.packets_lost;
            a.packets_received += i.received.packets_received;
        }
    }
    h
}

/// Aggregated pre-encoded path line: keyframe requests and rate control since the last report.
fn log_encoder(e: &EncoderReport, prev: &mut Prev) {
    let f = &e.feedback;
    let kf_req = f.keyframe_requests.load(Ordering::Relaxed);
    let kf_ignored = f.keyframe_requests_ignored.load(Ordering::Relaxed);
    let kf_sent = f.keyframes_sent.load(Ordering::Relaxed);
    let rate_changes = f.rate_changes.load(Ordering::Relaxed);
    let wenc = f.webrtc_encoded.load(Ordering::Relaxed);
    let wdrop = f.webrtc_dropped.load(Ordering::Relaxed);
    let drop_events = f.drop_events.load(Ordering::Relaxed);
    let recovery_idrs = f.recovery_idrs.load(Ordering::Relaxed);
    let broken = f.broken_frames.load(Ordering::Relaxed);
    let withheld = f.withheld_frames.load(Ordering::Relaxed);
    let guard_min = f.guard_min_pct.swap(f.guard_pct.load(Ordering::Relaxed), Ordering::Relaxed);
    let min = f.min_target_bps.swap(u64::MAX, Ordering::Relaxed);
    let max = f.max_target_bps.swap(0, Ordering::Relaxed);
    let kbps = |b: u64| if b == 0 || b == u64::MAX { "-".to_string() } else { format!("{}", b / 1000) };
    let hw = e
        .hw
        .map(|h| {
            format!(
                "encoded={} dropped={} keyframes={} coalesced_kf_requests={} avg_encode_us={} bitrate_kbps={} target_kbps={} encoder_kbps={} fps_cap={}",
                h.frames_encoded,
                h.frames_dropped,
                h.keyframes,
                h.keyframe_requests_coalesced,
                h.avg_encode_us,
                h.bitrate_bps / 1000,
                h.target_bitrate_bps / 1000,
                h.encoder_bitrate_bps / 1000,
                h.fps_cap
            )
        })
        .unwrap_or_else(|| "-".into());
    info!(
        backend = %e.backend,
        keyframe_requests = kf_req.saturating_sub(prev.kf_req),
        keyframe_requests_ignored = kf_ignored.saturating_sub(prev.kf_ignored),
        keyframes_sent = kf_sent.saturating_sub(prev.kf_sent),
        rate_changes = rate_changes.saturating_sub(prev.rate_changes),
        webrtc_encoded = wenc.saturating_sub(prev.webrtc_encoded),
        webrtc_dropped = wdrop.saturating_sub(prev.webrtc_dropped),
        drop_events = drop_events.saturating_sub(prev.drop_events),
        recovery_idrs = recovery_idrs.saturating_sub(prev.recovery_idrs),
        broken_frames = broken.saturating_sub(prev.broken_frames),
        withheld_frames = withheld.saturating_sub(prev.withheld_frames),
        guard_min_pct = guard_min,
        target_kbps = %kbps(f.target_bps.load(Ordering::Relaxed)),
        webrtc_min_kbps = %kbps(min),
        webrtc_max_kbps = %kbps(max),
        hw = %hw,
        "encoder feedback"
    );
    prev.kf_req = kf_req;
    prev.kf_ignored = kf_ignored;
    prev.kf_sent = kf_sent;
    prev.rate_changes = rate_changes;
    prev.webrtc_encoded = wenc;
    prev.webrtc_dropped = wdrop;
    prev.drop_events = drop_events;
    prev.recovery_idrs = recovery_idrs;
    prev.broken_frames = broken;
    prev.withheld_frames = withheld;
}

fn log_stream(r: &StreamReport, is_video: bool) {
    let kbps = r.kbps.map(|k| format!("{k:.0}")).unwrap_or_else(|| "-".into());
    let rtp_fps = r.rtp_fps.map(|f| format!("{f:.1}")).unwrap_or_else(|| "-".into());
    if is_video {
        let opt = |v: Option<u64>| v.map(|x| x.to_string()).unwrap_or_else(|| "-".into());
        info!(
            stream = %r.label,
            dir = r.dir,
            fps = format!("{:.1}", r.fps),
            rtp_fps = %rtp_fps,
            res = format!("{}x{}", r.width, r.height),
            kbps = %kbps,
            codec_impl = %r.codec_impl,
            limit = %r.quality_limitation,
            decoded = %opt(r.decoded),
            decode_dropped = %opt(r.decode_dropped),
            freezes = %opt(r.freezes),
            packets_lost = %r.packets_lost.map(|x| x.to_string()).unwrap_or_else(|| "-".into()),
            plis = %opt(r.plis),
            received = %opt(r.received),
            key_decoded = %opt(r.key_decoded),
            nacks = %opt(r.nacks),
            tx = %r.tx.map(|t| format!("src={} encoded={} sent={} keyframes={} huge={} plis_in={}", t.src, t.encoded, t.sent, t.keyframes, t.huge, t.plis)).unwrap_or_else(|| "-".into()),
            "stream stats"
        );
    } else {
        let rms = r.rms_dbfs.map(|d| format!("{d:.1}")).unwrap_or_else(|| "silent".into());
        let side = r.side_dbfs.map(|d| format!("{d:.1}")).unwrap_or_else(|| "-".into());
        info!(
            stream = %r.label,
            dir = r.dir,
            chunks_per_s = format!("{:.1}", r.fps),
            kbps = %kbps,
            rms_dbfs = %rms,
            side_dbfs = %side,
            codec = %r.codec,
            "stream stats"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_levels() {
        let c = StreamCounters::default();
        c.audio(&[16384, -16384, 16384, -16384]); // half scale square wave = -6 dBFS
        let db = rms_dbfs(c.sum_sq.load(Ordering::Relaxed), c.samples.load(Ordering::Relaxed)).unwrap();
        assert!((db + 6.02).abs() < 0.05, "{db}");
        assert_eq!(rms_dbfs(0, 960), None);
        assert_eq!(rms_dbfs(10, 0), None);
    }

    #[test]
    fn stereo_side_energy_tells_mono_from_stereo() {
        let mono = StreamCounters::default();
        mono.audio_stereo(&[1000, 1000, -2000, -2000]);
        assert_eq!(mono.side_sq.load(Ordering::Relaxed), 0);
        assert_eq!(mono.pairs.load(Ordering::Relaxed), 2);
        let st = StreamCounters::default();
        st.audio_stereo(&[16384, -16384, -16384, 16384]); // L = -R at half scale
        // side = (L - R) / 2 = +-16384, so it is as loud as each channel: -6 dBFS
        let db = rms_dbfs(st.side_sq.load(Ordering::Relaxed) / 4, st.pairs.load(Ordering::Relaxed)).unwrap();
        assert!((db + 6.02).abs() < 0.05, "{db}");
    }

    #[test]
    fn process_meter_reports_memory_and_sane_cpu() {
        let mut m = ProcessMeter::new();
        // Burn a little CPU so the delta is non-zero.
        let mut x = 0u64;
        let t = Instant::now();
        while t.elapsed().as_millis() < 50 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        std::hint::black_box(x);
        let s = m.sample();
        assert!(s.cpu_pct >= 0.0 && s.cpu_pct_core <= 100.0 * 1.5 * m.ncpu);
        if cfg!(windows) {
            assert!(s.working_set_bytes > 0 && s.private_bytes > 0);
            assert!(s.cpu_pct_core > 0.0);
        }
    }
}
