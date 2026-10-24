//! vox-cli: interactive test client for vox-client-core.
//!
//! Hub mode:    vox-cli --hub http://127.0.0.1:7890 --user alice [--password X]
//! Direct mode: vox-cli --direct ws://127.0.0.1:7980 --api-key devkey --api-secret secret --room test --user alice
//!
//! Type `help` at the prompt for commands.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::{ArgGroup, Parser};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use vox_client_core::golive::{capture_call, pattern::parse_geometry, EncoderChoice, H264FileSource, SwitchRequest};
use vox_client_core::hub::{HubClient, HubEvent, HubSender};
use vox_client_core::stats::Reporter;
use vox_client_core::token::mint_token;
use vox_client_core::{ShareOptions, ShareSource, VoiceConfig, VoiceEvent, VoiceSession};
use vox_proto::{ChannelId, ChannelKind, ClientEvent, ServerEvent};

#[derive(Parser, Debug)]
#[command(name = "vox-cli", about = "concord test client", group(ArgGroup::new("mode").required(true).args(["hub", "direct"])))]
struct Args {
    /// Hub base URL, e.g. http://127.0.0.1:7890
    #[arg(long)]
    hub: Option<String>,
    /// LiveKit URL for direct mode (no hub), e.g. ws://127.0.0.1:7980
    #[arg(long)]
    direct: Option<String>,
    /// Username (hub mode) / LiveKit identity (direct mode)
    #[arg(long)]
    user: String,
    /// Shared server password (hub mode)
    #[arg(long, default_value = "")]
    password: String,
    /// v1.1: the hub's TLS certificate pin (64 hex digits, as in an invite or the hub's
    /// `tls/server.crt` SHA-256): talk to the hub over pinned TLS only (hub mode).
    #[arg(long)]
    tls_pin: Option<String>,
    #[arg(long, default_value = "devkey")]
    api_key: String,
    #[arg(long, default_value = "secret")]
    api_secret: String,
    /// Room to join in direct mode
    #[arg(long, default_value = "test")]
    room: String,
    /// Do not open/publish the microphone (headless testing)
    #[arg(long)]
    no_mic: bool,
    /// Do not open the speaker output
    #[arg(long)]
    no_playback: bool,
    /// Microphone name substring (default device if omitted)
    #[arg(long)]
    input_device: Option<String>,
    /// Speaker name substring (default device if omitted)
    #[arg(long)]
    output_device: Option<String>,
    /// Publish a sine tone (Hz) as the microphone instead of a real device (headless tests)
    #[arg(long)]
    mic_tone: Option<f32>,
    /// Start with the microphone muted
    #[arg(long)]
    muted: bool,
    /// Output volume in percent (0-200)
    #[arg(long, default_value_t = 100)]
    volume: u32,
    /// Stats interval in seconds
    #[arg(long, default_value_t = 30)]
    stats_secs: u64,
    /// Log directory (default: %LOCALAPPDATA%\concord\logs\client)
    #[arg(long)]
    log_dir: Option<PathBuf>,
}

const HELP: &str = "\
commands:
  join [voice-channel-id]     join a voice channel (direct mode: optional room name)
  leave                       leave voice
  m                           toggle mute
  d                           toggle deafen
  vol <0-200>                 output volume in percent
  uvol <identity> <0-200>     local volume of one person's mic (only you hear it)
  umute <identity> on|off     mute one person's mic for you only
  svol <identity> <0-200>     volume of that person's screenshare audio
  smute <identity> on|off     mute that person's screenshare audio
  muteall on|off              silence every other participant locally (your mic untouched)
  probe [ms]                  --no-playback only: pull the mix like the speaker would, print RMS per 100 ms
  windows                     list capturable windows (and monitors)
  share audio-app <pid> | audio-system | audio-tone [--high]      (audio-only Go Live, so audio)
  share window <hwnd> [WxH@fps] [--no-audio] [--audio-pid <pid>] [--encoder auto|nvenc|mf|software]
  share monitor <n> [WxH@fps] [--no-audio] [--audio-pid <pid>] [--encoder auto|nvenc|mf|software]
  share test [WxH@fps] [--no-audio]  moving test pattern + sine tone (software encoder)
  share h264file <path> [--fps N] [--loop]  replay an Annex-B .h264 file (pre-encoded path, no audio)
      audio default: window -> that app's process tree; monitor -> all system audio except concord
      encoder default: auto = hardware (pre-encoded) if available, else software (OpenH264)
      extra share options: --kbps <max video kbps> (default by size/fps), --smooth (keep fps under
      congestion instead of dropping it first)
  switch window <hwnd> | monitor <n> [--audio|--no-audio] [--audio-pid <pid>]
                              change what the running share captures, without stopping it (audio: keeps
                              its current on/off unless told otherwise)
  vprobe [ms]                 watching: sample the watched video every 20 ms, print resolution changes
                              and the longest gap between frames
  bwcap <kbps>|off            test hook: cap the share's encoder target as if the network allowed only that
  overshoot <percent>|off     test hook: the encoder produces that % of its target (WebRTC drop guard test)
  unshare                     stop Go Live
  cam on [device] [360|540|720|1080] [--encoder auto|nvenc|mf|software]
                              turn the camera on (device: id, name or vox:synthetic-camera; default:
                              the first camera, or the synthetic one with VOX_FAKE_CAMERA=1)
  cam off                     turn the camera off (device released)
  cams on|off                 camera tiles visible (subscribe/enable remote cameras) or hidden
  cameras                     list cameras
  watch <identity|name>       watch someone's Go Live (several at once, up to 4)
  unwatch [identity]          stop watching one stream (all without an identity)
  say <channel-id> <text>     send a chat message (hub mode)
  channels                    list hub channels
  stats                       print stats now
  trim [ws]                   compact heaps (and trim working set with 'ws'); for measurements
  q                           quit";

struct App {
    args: Args,
    hub: Option<Arc<HubClient>>,
    hub_tx: Option<HubSender>,
    voice: Option<VoiceSession>,
    new_voice_rx: Option<mpsc::UnboundedReceiver<VoiceEvent>>,
    voice_channel: Option<ChannelId>,
    streaming: bool,
    share_audio_only: bool,
    camera: bool,
    reporter: Reporter,
    channels: Vec<vox_proto::Channel>,
}

async fn recv_opt<T>(rx: &mut Option<mpsc::Receiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn recv_opt_unbounded<T>(rx: &mut Option<mpsc::UnboundedReceiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let _log = vox_log::init("client", args.log_dir.as_deref(), true);
    if let Err(e) = run(args).await {
        error!(error = %format!("{e:#}"), "vox-cli failed");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<()> {
    let stats_every = Duration::from_secs(args.stats_secs.max(1));
    let mut app = App {
        args,
        hub: None,
        hub_tx: None,
        voice: None,
        new_voice_rx: None,
        voice_channel: None,
        streaming: false,
        share_audio_only: false,
        camera: false,
        reporter: Reporter::new(),
        channels: Vec::new(),
    };

    let mut hub_rx: Option<mpsc::Receiver<HubEvent>> = None;
    if let Some(url) = app.args.hub.clone() {
        let hub = match app.args.tls_pin.as_deref() {
            Some(hex) => {
                let pin = vox_proto::tls_pin_from_hex(hex).ok_or_else(|| anyhow!("--tls-pin must be 64 hex digits"))?;
                println!("pinned TLS to the hub (server code {})", vox_proto::tls_server_code(&pin));
                Arc::new(HubClient::new_pinned(&url, pin)?)
            }
            None => Arc::new(HubClient::new(&url)?),
        };
        match hub.health().await {
            Ok(h) => info!(version = %h.version, protocol = h.protocol_version, livekit_ok = h.livekit_ok, "hub health"),
            Err(e) => warn!(error = %format!("{e:#}"), "hub health check failed"),
        }
        let login = hub.login(&app.args.user, &app.args.password).await?;
        println!("logged in as {} (id {}) on '{}'", login.user.username, login.user.id, login.server_name);
        match hub.channels().await {
            Ok(ch) => {
                print_channels(&ch);
                app.channels = ch;
            }
            Err(e) => warn!(error = %format!("{e:#}"), "list channels failed"),
        }
        let (tx, rx) = hub.connect_events()?;
        app.hub_tx = Some(tx);
        hub_rx = Some(rx);
        app.hub = Some(hub);
    } else {
        // Direct mode: join the room right away.
        let room = app.args.room.clone();
        if let Err(e) = app.join_direct(&room).await {
            error!(error = %format!("{e:#}"), "join failed");
        }
    }
    println!("type 'help' for commands");

    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if line_tx.send(l).is_err() {
                break;
            }
        }
    });

    let mut voice_rx: Option<mpsc::UnboundedReceiver<VoiceEvent>> = app.new_voice_rx.take();
    let mut stats_tick = tokio::time::interval(stats_every);
    stats_tick.tick().await;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    loop {
        tokio::select! {
            line = line_rx.recv() => {
                let Some(line) = line else { info!("stdin closed"); break };
                match app.command(line.trim()).await {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(e) => println!("error: {e:#}"),
                }
                if let Some(rx) = app.new_voice_rx.take() {
                    voice_rx = Some(rx);
                }
                if app.voice.is_none() {
                    voice_rx = None;
                }
            }
            ev = recv_opt(&mut hub_rx) => match ev {
                Some(ev) => app.on_hub(ev),
                None => { hub_rx = None; }
            },
            ev = recv_opt_unbounded(&mut voice_rx) => match ev {
                Some(ev) => app.on_voice(ev),
                None => { voice_rx = None; }
            },
            _ = stats_tick.tick() => app.report_stats().await,
            _ = &mut ctrl_c => { info!("ctrl-c"); break; }
        }
    }

    app.report_stats().await;
    app.leave().await;
    info!("bye");
    Ok(())
}

fn print_channels(ch: &[vox_proto::Channel]) {
    for c in ch {
        let kind = match c.kind {
            ChannelKind::Text => "text ",
            ChannelKind::Voice => "voice",
        };
        println!("  [{kind}] {:>3}  {}", c.id, c.name);
    }
}

fn parse_hwnd(s: &str) -> Result<isize> {
    let v = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        isize::from_str_radix(hex, 16)?
    } else {
        s.parse::<isize>()?
    };
    Ok(v)
}

impl App {
    fn voice_config(&self) -> VoiceConfig {
        VoiceConfig {
            mic: !self.args.no_mic || self.args.mic_tone.is_some(),
            mic_test_tone: self.args.mic_tone,
            fake_mics: None,
            playback: !self.args.no_playback,
            input_device: self.args.input_device.clone(),
            output_device: self.args.output_device.clone(),
            start_muted: self.args.muted,
            output_volume: Some(self.args.volume.min(200) as f32 / 100.0),
            user_audio: Default::default(),
            others_muted: false,
            gate: None,
            noise: None,
            tls_pin: self.hub.as_ref().and_then(|h| h.tls_pin()),
            // (The CLI has no device identity: it joins plain rooms only, like an app before 1.1.)
            e2ee: None,
        }
    }

    async fn join_direct(&mut self, room: &str) -> Result<()> {
        let url = self.args.direct.clone().ok_or_else(|| anyhow!("not in direct mode"))?;
        let token = mint_token(&self.args.api_key, &self.args.api_secret, room, &self.args.user, Some(&self.args.user), Duration::from_secs(6 * 3600))?;
        self.leave().await;
        let (session, rx) = VoiceSession::connect(&url, &token, self.voice_config()).await?;
        println!("joined room '{}' as '{}'", session.room_name(), session.identity());
        self.voice = Some(session);
        self.new_voice_rx = Some(rx);
        Ok(())
    }

    async fn join_hub(&mut self, channel: ChannelId) -> Result<()> {
        let hub = self.hub.clone().ok_or_else(|| anyhow!("not in hub mode"))?;
        let resp = hub.voice_join(channel).await?;
        self.leave().await;
        let (session, rx) = VoiceSession::connect(&resp.livekit_url, &resp.livekit_token, self.voice_config()).await?;
        println!("joined voice channel {channel} (room '{}') as '{}'", resp.room, session.identity());
        self.voice = Some(session);
        self.new_voice_rx = Some(rx);
        self.voice_channel = Some(channel);
        self.push_voice_state();
        Ok(())
    }

    async fn leave(&mut self) {
        if let Some(v) = self.voice.take() {
            v.leave().await;
            println!("left voice");
        }
        self.streaming = false;
        self.camera = false;
        if self.voice_channel.take().is_some() {
            if let Some(tx) = &self.hub_tx {
                let _ = tx.send(ClientEvent::VoiceState { channel_id: None, muted: false, deafened: false, streaming: false, camera: false, audio_only: false });
            }
        }
    }

    fn push_voice_state(&self) {
        let (Some(tx), Some(ch), Some(v)) = (&self.hub_tx, self.voice_channel, &self.voice) else { return };
        let _ = tx.send(ClientEvent::VoiceState {
            channel_id: Some(ch),
            muted: v.is_muted(),
            deafened: v.is_deafened(),
            streaming: self.streaming,
            camera: self.camera,
            audio_only: self.streaming && self.share_audio_only,
        });
    }

    fn voice(&self) -> Result<&VoiceSession> {
        self.voice.as_ref().ok_or_else(|| anyhow!("not in a voice channel (use 'join')"))
    }

    async fn report_stats(&mut self) {
        let streams = match &self.voice {
            Some(v) => v.streams().await,
            None => Vec::new(),
        };
        self.reporter.report(&streams).await;
    }

    /// Returns Ok(false) to quit.
    async fn command(&mut self, line: &str) -> Result<bool> {
        let mut parts = line.split_whitespace();
        let Some(cmd) = parts.next() else { return Ok(true) };
        let rest: Vec<&str> = parts.collect();
        match cmd {
            "help" | "?" => println!("{HELP}"),
            "q" | "quit" | "exit" => return Ok(false),
            "join" => {
                if self.hub.is_some() {
                    let id: ChannelId = rest.first().ok_or_else(|| anyhow!("usage: join <voice-channel-id>"))?.parse().context("channel id")?;
                    self.join_hub(id).await?;
                } else {
                    let room = rest.first().map(|s| s.to_string()).unwrap_or_else(|| self.args.room.clone());
                    self.join_direct(&room).await?;
                }
            }
            "leave" => self.leave().await,
            "m" => {
                let v = self.voice()?;
                if v.is_deafened() {
                    bail!("deafened; undeafen first ('d')");
                }
                if !v.has_mic() {
                    println!("(no microphone published)");
                }
                v.set_muted(!v.is_muted());
                println!("muted: {}", v.is_muted());
                self.push_voice_state();
            }
            "d" => {
                let v = self.voice()?;
                v.set_deafened(!v.is_deafened());
                println!("deafened: {} (muted: {})", v.is_deafened(), v.is_muted());
                self.push_voice_state();
            }
            "vol" => {
                let v = self.voice()?;
                let pct: u32 = rest.first().ok_or_else(|| anyhow!("usage: vol <0-200>"))?.parse().context("volume")?;
                v.set_output_volume(pct.min(200) as f32 / 100.0);
                println!("volume: {}%", (v.output_volume() * 100.0).round());
            }
            "uvol" | "svol" => {
                let v = self.voice()?;
                let who = rest.first().ok_or_else(|| anyhow!("usage: {cmd} <identity> <0-200>"))?;
                let pct: u32 = rest.get(1).ok_or_else(|| anyhow!("usage: {cmd} <identity> <0-200>"))?.parse().context("volume")?;
                let gain = pct.min(200) as f32 / 100.0;
                if cmd == "uvol" {
                    v.set_user_volume(who, gain);
                } else {
                    v.set_stream_volume(who, gain);
                }
                println!("{who}: {:?}", v.user_audio(who));
            }
            "umute" | "smute" => {
                let v = self.voice()?;
                let who = rest.first().ok_or_else(|| anyhow!("usage: {cmd} <identity> on|off"))?;
                let on = matches!(rest.get(1).copied(), Some("on" | "1" | "true"));
                if cmd == "umute" {
                    v.set_user_muted(who, on);
                } else {
                    v.set_stream_muted(who, on);
                }
                println!("{who}: {:?}", v.user_audio(who));
            }
            "muteall" => {
                let v = self.voice()?;
                v.set_others_muted(matches!(rest.first().copied(), Some("on" | "1" | "true")));
                println!("others muted: {}", v.others_muted());
            }
            "probe" => {
                if !self.args.no_playback {
                    bail!("probe needs --no-playback (the speaker callback would compete for the mix)");
                }
                let v = self.voice()?;
                let ms: u64 = rest.first().map(|s| s.parse()).transpose().context("ms")?.unwrap_or(1000);
                let mut block = vec![0i16; vox_client_core::audio::MIX_10MS];
                let mut tick = tokio::time::interval(Duration::from_millis(10));
                let (mut sum, mut cnt) = (0f64, 0u64);
                let mut windows = Vec::new();
                for i in 1..=(ms / 10) {
                    tick.tick().await;
                    v.pull_playback_block(&mut block);
                    sum += block.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>();
                    cnt += block.len() as u64;
                    if i % 10 == 0 {
                        windows.push((sum / cnt.max(1) as f64).sqrt());
                        sum = 0.0;
                        cnt = 0;
                    }
                }
                let avg = windows.iter().sum::<f64>() / windows.len().max(1) as f64;
                println!("probe: rms per 100 ms = {:?} avg {:.0}", windows.iter().map(|w| w.round() as i64).collect::<Vec<_>>(), avg);
            }
            "windows" => {
                match capture_call("list_windows", vox_capture::list_windows) {
                    Ok(ws) => {
                        for w in ws {
                            println!("  hwnd {:>10} (0x{:x})  pid {:>6}  {:<20} {}", w.hwnd, w.hwnd, w.process_id, w.exe_name, w.title);
                        }
                    }
                    Err(e) => println!("windows: {e:#}"),
                }
                match capture_call("list_monitors", vox_capture::list_monitors) {
                    Ok(ms) => {
                        for m in ms {
                            println!("  monitor {}  {}x{}{}  {}", m.index, m.width, m.height, if m.primary { " (primary)" } else { "" }, m.name);
                        }
                    }
                    Err(e) => println!("monitors: {e:#}"),
                }
            }
            "share" => {
                let kind = rest.first().copied().ok_or_else(|| anyhow!("usage: share window <hwnd> | monitor <n> | test [WxH@fps] | h264file <path>"))?;
                let (source, extra) = match kind {
                    "window" => (ShareSource::Window(parse_hwnd(rest.get(1).ok_or_else(|| anyhow!("missing hwnd"))?)?), &rest[2..]),
                    "monitor" => (ShareSource::Monitor(rest.get(1).ok_or_else(|| anyhow!("missing monitor index"))?.parse()?), &rest[2..]),
                    "test" => (ShareSource::TestPattern, &rest[1..]),
                    // "Só áudio": nothing but sound (no video track, capture or encoder).
                    "audio-app" => (ShareSource::AudioApp { pid: rest.get(1).ok_or_else(|| anyhow!("missing pid"))?.parse().context("pid")? }, &rest[2..]),
                    "audio-system" => (ShareSource::AudioSystem, &rest[1..]),
                    "audio-tone" => (ShareSource::AudioTone, &rest[1..]),
                    "h264file" => {
                        let path = rest.get(1).ok_or_else(|| anyhow!("missing .h264 path"))?;
                        (ShareSource::H264File(H264FileSource { path: PathBuf::from(path), fps: 30, looped: false }), &rest[2..])
                    }
                    other => bail!("unknown share target {other:?}"),
                };
                let mut source = source;
                let default = if source == ShareSource::TestPattern { (1280, 720, 30) } else { (1920, 1080, 30) };
                let mut opts = ShareOptions { width: default.0, height: default.1, fps: default.2, ..Default::default() };
                if matches!(source, ShareSource::H264File(_)) {
                    opts.audio = false;
                }
                let mut toks = extra.iter();
                while let Some(tok) = toks.next() {
                    if *tok == "noaudio" || *tok == "--no-audio" {
                        opts.audio = false;
                    } else if *tok == "--high" {
                        // audio-only: "Alta" (256 kbps) instead of "Normal" (128 kbps)
                        opts.audio_bitrate = vox_client_core::golive::AUDIO_ONLY_HIGH_BPS;
                    } else if *tok == "--audio-pid" {
                        let pid: u32 = toks.next().ok_or_else(|| anyhow!("--audio-pid needs a pid"))?.parse().context("pid")?;
                        opts.audio_pid = Some(pid);
                    } else if *tok == "--kbps" {
                        let k: u64 = toks.next().ok_or_else(|| anyhow!("--kbps needs a number"))?.parse().context("kbps")?;
                        opts.max_bitrate = Some(k * 1000);
                    } else if *tok == "--smooth" {
                        opts.degradation = vox_client_core::golive::DegradationPreference::Smoothness;
                    } else if *tok == "--encoder" {
                        opts.encoder = toks.next().ok_or_else(|| anyhow!("--encoder needs auto|nvenc|mf|software"))?.parse()?;
                    } else if *tok == "--fps" {
                        let fps: u32 = toks.next().ok_or_else(|| anyhow!("--fps needs a number"))?.parse().context("fps")?;
                        match &mut source {
                            ShareSource::H264File(f) => f.fps = fps.clamp(1, 240),
                            _ => opts.fps = fps.clamp(1, 240),
                        }
                    } else if *tok == "--loop" {
                        match &mut source {
                            ShareSource::H264File(f) => f.looped = true,
                            _ => bail!("--loop only applies to h264file"),
                        }
                    } else {
                        let (w, h, fps) = parse_geometry(tok, default).ok_or_else(|| anyhow!("bad geometry {tok:?}, expected WxH@fps"))?;
                        (opts.width, opts.height, opts.fps) = (w, h, fps);
                    }
                }
                if let ShareSource::H264File(f) = &source {
                    opts.fps = f.fps;
                }
                self.voice()?.start_share(source.clone(), opts).await?;
                self.streaming = true;
                self.share_audio_only = source.is_audio_only();
                let audio = match (opts.audio, opts.audio_pid) {
                    (false, _) => "off".to_string(),
                    (true, Some(pid)) => format!("pid {pid} tree"),
                    (true, None) => "default".to_string(),
                };
                let path = self.voice()?.share_path().await.map(|p| format!("{p:?}")).unwrap_or_default();
                let encoder = if opts.encoder == EncoderChoice::Auto { String::new() } else { format!(", encoder: {:?}", opts.encoder) };
                println!(
                    "sharing {source:?} at up to {}x{}@{} {} kbps (audio: {audio}, path: {path}{encoder}, degradation: {:?})",
                    opts.width,
                    opts.height,
                    opts.fps,
                    opts.bitrate() / 1000,
                    opts.degradation
                );
                self.push_voice_state();
            }
            "switch" => {
                let kind = rest.first().copied().ok_or_else(|| anyhow!("usage: switch window <hwnd> | monitor <n> [--audio|--no-audio] [--audio-pid <pid>]"))?;
                let source = match kind {
                    "window" => ShareSource::Window(parse_hwnd(rest.get(1).ok_or_else(|| anyhow!("missing hwnd"))?)?),
                    "monitor" => ShareSource::Monitor(rest.get(1).ok_or_else(|| anyhow!("missing monitor index"))?.parse()?),
                    other => bail!("unknown switch target {other:?}"),
                };
                let v = self.voice()?;
                let mut audio = v.share_has_audio().await;
                let mut audio_pid = None;
                let mut toks = rest[2..].iter();
                while let Some(tok) = toks.next() {
                    match *tok {
                        "--audio" => audio = true,
                        "--no-audio" | "noaudio" => audio = false,
                        "--audio-pid" => {
                            audio_pid = Some(toks.next().ok_or_else(|| anyhow!("--audio-pid needs a pid"))?.parse().context("pid")?);
                            audio = true;
                        }
                        other => bail!("unknown option {other:?}"),
                    }
                }
                let r = v.switch_share(SwitchRequest { source, audio, audio_pid }).await?;
                println!(
                    "switched {:?} -> {:?} (path {:?}, gap {} ms, total {} ms, audio {} -> {}{})",
                    r.from,
                    r.to,
                    r.path,
                    r.gap.as_millis(),
                    r.total.as_millis(),
                    if r.audio_before { "on" } else { "off" },
                    if r.audio_after { "on" } else { "off" },
                    r.audio_error.map(|e| format!(", {e}")).unwrap_or_default()
                );
            }
            "vprobe" => {
                let ms: u64 = rest.first().map(|s| s.parse()).transpose().context("ms")?.unwrap_or(5000);
                let v = self.voice()?;
                let find = |streams: Vec<vox_client_core::stats::StreamEntry>| streams.into_iter().find(|s| s.label.starts_with("watch-video"));
                let Some(entry) = find(v.streams().await) else { bail!("not watching any video") };
                let c = entry.counters;
                let (mut last_frames, mut last_res) = (c.frames.load(Ordering::Relaxed), (c.width.load(Ordering::Relaxed), c.height.load(Ordering::Relaxed)));
                let t0 = Instant::now();
                let mut last_change = t0;
                let (mut max_gap, mut max_gap_at) = (Duration::ZERO, Duration::ZERO);
                let mut tick = tokio::time::interval(Duration::from_millis(20));
                println!("vprobe: start {}x{}", last_res.0, last_res.1);
                while t0.elapsed() < Duration::from_millis(ms) {
                    tick.tick().await;
                    let now = Instant::now();
                    let f = c.frames.load(Ordering::Relaxed);
                    if f != last_frames {
                        let gap = now - last_change;
                        if gap > max_gap {
                            (max_gap, max_gap_at) = (gap, now - t0);
                        }
                        last_change = now;
                        last_frames = f;
                        let res = (c.width.load(Ordering::Relaxed), c.height.load(Ordering::Relaxed));
                        if res != last_res {
                            println!("vprobe: t={} ms resolution {}x{} -> {}x{} (gap before first new frame {} ms)", t0.elapsed().as_millis(), last_res.0, last_res.1, res.0, res.1, gap.as_millis());
                            last_res = res;
                        }
                    }
                }
                println!("vprobe: {} frames in {} ms, longest gap between frames {} ms at t={} ms", last_frames, ms, max_gap.as_millis(), max_gap_at.as_millis());
            }
            "bwcap" => {
                let arg = rest.first().ok_or_else(|| anyhow!("usage: bwcap <kbps>|off"))?;
                let cap = if *arg == "off" { None } else { Some(arg.parse::<u64>().context("kbps")? * 1000) };
                if self.voice()?.set_share_bitrate_cap(cap).await {
                    println!("bitrate cap: {}", cap.map(|c| format!("{} kbps", c / 1000)).unwrap_or_else(|| "off".into()));
                } else {
                    println!("not sharing");
                }
            }
            "overshoot" => {
                let arg = rest.first().ok_or_else(|| anyhow!("usage: overshoot <percent>|off"))?;
                let pct = if *arg == "off" { None } else { Some(arg.parse::<u64>().context("percent")?) };
                if self.voice()?.set_share_encoder_overshoot(pct).await {
                    println!("encoder overshoot: {}", pct.map(|p| format!("{p}%")).unwrap_or_else(|| "off".into()));
                } else {
                    println!("not sharing");
                }
            }
            "unshare" => {
                let stopped = self.voice()?.stop_share().await;
                self.streaming = false;
                println!("{}", if stopped { "share stopped" } else { "not sharing" });
                self.push_voice_state();
            }
            "cam" => match rest.first().copied() {
                Some("on") => {
                    let mut q = vox_client_core::golive::camera::CameraQuality::P720;
                    let mut device = String::new();
                    let mut encoder = EncoderChoice::Auto;
                    let mut i = 1;
                    while i < rest.len() {
                        match rest[i] {
                            "360" => q = vox_client_core::golive::camera::CameraQuality::P360,
                            "540" => q = vox_client_core::golive::camera::CameraQuality::P540,
                            "720" => q = vox_client_core::golive::camera::CameraQuality::P720,
                            "1080" => q = vox_client_core::golive::camera::CameraQuality::P1080,
                            "--encoder" => {
                                i += 1;
                                encoder = rest.get(i).ok_or_else(|| anyhow!("--encoder needs a value"))?.parse()?;
                            }
                            d => device = d.to_string(),
                        }
                        i += 1;
                    }
                    let mut opts = vox_client_core::golive::camera::CameraOptions::with_quality(device, q);
                    opts.encoder = encoder;
                    let t0 = Instant::now();
                    match self.voice()?.start_camera(opts).await {
                        Ok(st) => println!("camera on in {} ms: {} ({:?})", t0.elapsed().as_millis(), st.backend, st.format),
                        Err(e) => println!("camera failed: {e:#}"),
                    }
                    self.camera = self.voice()?.camera_on().await;
                    self.push_voice_state();
                }
                Some("off") => {
                    let t0 = Instant::now();
                    let was = self.voice()?.stop_camera().await;
                    println!("{} ({} ms)", if was { "camera off" } else { "camera was not on" }, t0.elapsed().as_millis());
                    self.camera = false;
                    self.push_voice_state();
                }
                _ => println!("usage: cam on [device] [360|540|720|1080] | cam off"),
            },
            "cams" => {
                let on = rest.first() != Some(&"off");
                self.voice()?.set_cameras_visible(on).await;
                println!("camera tiles {}; cameras on: {:?}", if on { "visible" } else { "hidden" }, self.voice()?.cameras());
            }
            "cameras" => match vox_capture::list_cameras() {
                Ok(list) => {
                    for c in list {
                        println!("  {:<32} {}", c.name, c.id);
                    }
                }
                Err(e) => println!("cannot list cameras: {e}"),
            },
            "watch" => {
                let who = rest.first().ok_or_else(|| anyhow!("usage: watch <identity>"))?;
                let id = self.voice()?.watch(who)?;
                println!("watching {id}");
            }
            "unwatch" => {
                let v = self.voice()?;
                let stopped = match rest.first() {
                    Some(who) => if v.unwatch(who) { vec![who.to_string()] } else { Vec::new() },
                    None => v.unwatch_all(),
                };
                if stopped.is_empty() {
                    println!("not watching that / anyone");
                } else {
                    println!("stopped watching {stopped:?}; still watching {:?}", v.watching());
                }
            }
            "say" => {
                let tx = self.hub_tx.as_ref().ok_or_else(|| anyhow!("'say' needs hub mode"))?;
                let id: ChannelId = rest.first().ok_or_else(|| anyhow!("usage: say <channel-id> <text>"))?.parse().context("channel id")?;
                let text = rest[1..].join(" ");
                if text.is_empty() {
                    bail!("empty message");
                }
                tx.send(ClientEvent::SendMessage { channel_id: id, content: text, mentions: vec![], reply_to: None, reply_mention: false })?;
            }
            "channels" => {
                let hub = self.hub.clone().ok_or_else(|| anyhow!("needs hub mode"))?;
                self.channels = hub.channels().await?;
                print_channels(&self.channels);
            }
            "stats" => self.report_stats().await,
            "trim" => {
                let ws = rest.first() == Some(&"ws");
                let heaps = vox_client_core::stats::trim_memory(ws);
                println!("compacted {heaps} heaps{}", if ws { ", trimmed working set" } else { "" });
            }
            other => println!("unknown command {other:?}; type 'help'"),
        }
        Ok(true)
    }

    fn channel_name(&self, id: ChannelId) -> String {
        self.channels.iter().find(|c| c.id == id).map(|c| c.name.clone()).unwrap_or_else(|| id.to_string())
    }

    fn on_hub(&mut self, ev: HubEvent) {
        match ev {
            HubEvent::Connected => println!("[hub] connected"),
            HubEvent::Disconnected { reason } => println!("[hub] disconnected: {reason} (reconnecting)"),
            HubEvent::AuthFailed => println!("[hub] session rejected; restart to log in again"),
            HubEvent::Server(ev) => match ev {
                ServerEvent::Ready { user, channels, online, voice, .. } => {
                    println!(
                        "[hub] ready as {} - {} channels, online: [{}], in voice: {}",
                        user.username,
                        channels.len(),
                        online.iter().map(|u| u.username.as_str()).collect::<Vec<_>>().join(", "),
                        voice.len()
                    );
                    self.channels = channels;
                    // Re-announce our voice state after a reconnect.
                    self.push_voice_state();
                }
                ServerEvent::MessageCreated { message } => {
                    println!("[#{}] {}: {}", self.channel_name(message.channel_id), message.author.username, message.content);
                }
                ServerEvent::UserOnline { user } => println!("[hub] {} is online", user.username),
                ServerEvent::UserOffline { user_id } => println!("[hub] user {user_id} went offline"),
                ServerEvent::VoiceStateChanged { member } => println!(
                    "[voice] {} -> {} muted={} deafened={} streaming={}",
                    member.user.username,
                    member.channel_id.map(|c| self.channel_name(c)).unwrap_or_else(|| "(left)".into()),
                    member.muted,
                    member.deafened,
                    member.streaming
                ),
                ServerEvent::Pong { .. } => {}
                ServerEvent::Error { error } => println!("[hub] error: {error}"),
                ServerEvent::ChannelsChanged { channels } => {
                    println!("[hub] channels changed ({} channels)", channels.len());
                    self.channels = channels;
                }
                // Role / moderation events (item 12) are not interactive in the CLI.
                other => println!("[hub] {other:?}"),
            },
        }
    }

    fn on_voice(&mut self, ev: VoiceEvent) {
        match ev {
            VoiceEvent::Connected { room, identity, participants } => {
                println!("[voice] in room '{room}' as '{identity}', others: [{}]", participants.join(", "))
            }
            VoiceEvent::ParticipantJoined { identity, name } => println!("[voice] {identity} ({name}) joined"),
            VoiceEvent::ParticipantLeft { identity } => println!("[voice] {identity} left"),
            VoiceEvent::ActiveSpeakers { identities } => {
                if identities.is_empty() {
                    println!("[voice] nobody speaking");
                } else {
                    println!("[voice] speaking: {}", identities.join(", "));
                }
            }
            VoiceEvent::ShareStarted { identity } => println!("[golive] {identity} started sharing (use 'watch {identity}')"),
            VoiceEvent::ShareStopped { identity } => println!("[golive] {identity} stopped sharing"),
            VoiceEvent::ShareKind { identity, audio_only } => println!("[golive] {identity}'s live is {}", if audio_only { "audio-only" } else { "a screen share" }),
            VoiceEvent::LocalShareEnded { reason } => {
                println!("[golive] your share ended: {reason}");
                self.streaming = false;
                self.push_voice_state();
            }
            VoiceEvent::ServerMuted { muted } => {
                println!("[voice] {}", if muted { "server-muted by a moderator" } else { "server mute lifted" })
            }
            VoiceEvent::Mic(status) => println!("[voice] microphone: {status:?}"),
            VoiceEvent::MicAvailable => println!("[voice] a microphone showed up: published"),
            VoiceEvent::WatchStarted { identity } => println!("[golive] watching {identity}"),
            VoiceEvent::CameraStarted { identity } => println!("[camera] {identity} turned the camera on"),
            VoiceEvent::CameraStopped { identity } => println!("[camera] {identity} turned the camera off"),
            VoiceEvent::EncoderSwitched { camera, label, backend } => {
                println!("[{}] unstable video: switched encoder to {label} ({backend})", if camera { "camera" } else { "golive" })
            }
            VoiceEvent::LocalCameraEnded { reason } => {
                println!("[camera] your camera stopped: {reason}");
                self.camera = false;
                self.push_voice_state();
            }
            VoiceEvent::WatchEnded { identity, reason } => println!("[golive] stopped watching {identity}: {reason}"),
            VoiceEvent::E2eeState { identity, state } => println!("[e2ee] {identity}: {state:?}"),
            VoiceEvent::E2eePeerDevice { identity, device_id } => println!("[e2ee] {identity} is here with device {}", device_id.get(..8).unwrap_or("")),
            VoiceEvent::Reconnecting => println!("[voice] reconnecting..."),
            VoiceEvent::Reconnected => println!("[voice] reconnected"),
            VoiceEvent::Disconnected { reason } => {
                println!("[voice] disconnected: {reason}");
                self.voice = None;
                self.streaming = false;
                self.camera = false;
            }
        }
    }
}
