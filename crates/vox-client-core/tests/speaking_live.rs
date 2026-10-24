//! v1.1 speaking ring against a real LiveKit: a remote voice lights its person from the decoded
//! level, goes off soon after it stops, and still lights while the listener is deafened (their
//! audio is paused at the SFU, so LiveKit's active speakers decide then).
//!
//! Needs the dev LiveKit binary at `target-v02/devbin/livekit-server.exe` (or `VOX_LIVEKIT_BIN`)
//! and the dev ports (hub 7997, LiveKit 7970-7972). No audio devices: the speaker publishes a
//! sine tone as its microphone, the listener plays nothing.
//!
//! `cargo test -p vox-client-core --test speaking_live -- --ignored --nocapture`

use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use vox_client_core::hub::HubClient;
use vox_client_core::{VoiceConfig, VoiceSession};
use vox_proto::*;
use voxhub_server::{start, Options};

const HUB_PORT: u16 = 7997;
const LK_SIGNAL: u16 = 7970;
const API_KEY: &str = "APIlivetest12";
const API_SECRET: &str = "livetestlivetestlivetestlivetest12";

fn livekit_bin() -> PathBuf {
    std::env::var_os("VOX_LIVEKIT_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target-v02/devbin/livekit-server.exe"))
}

async fn wait_port(port: u16) {
    let t = Instant::now();
    while tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err() {
        assert!(t.elapsed() < Duration::from_secs(20), "LiveKit did not open port {port}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn join(hub: &HubClient, channel: ChannelId, cfg: VoiceConfig) -> (VoiceSession, tokio::sync::mpsc::UnboundedReceiver<vox_client_core::VoiceEvent>) {
    let j = hub.voice_join(channel).await.unwrap();
    VoiceSession::connect(&j.livekit_url, &j.livekit_token, cfg).await.unwrap()
}

/// How long until the listener's speaking set has `identity` (`lit`) or not; `None`: not within
/// `within`.
async fn until(listener: &VoiceSession, identity: &str, lit: bool, within: Duration) -> Option<u128> {
    let t = Instant::now();
    while t.elapsed() < within {
        if listener.speaking().iter().any(|i| i == identity) == lit {
            return Some(t.elapsed().as_millis());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

/// Audio chunks the listener received from `identity` during `secs`.
async fn flow(listener: &VoiceSession, identity: &str, secs: u64) -> u64 {
    let label = format!("voice:{identity}");
    let count = || async { listener.streams().await.iter().filter(|s| s.label == label).map(|s| s.counters.frames.load(Relaxed)).sum::<u64>() };
    let a = count().await;
    tokio::time::sleep(Duration::from_secs(secs)).await;
    count().await.saturating_sub(a)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the dev LiveKit binary and the dev ports"]
async fn a_remote_voice_lights_its_ring_also_while_deafened() {
    let _ = vox_log::init("speaking-live-test", Some(std::env::temp_dir().join("concord-speaking-live").as_path()), true);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "server_name = \"speak\"\nserver_password = \"\"\nbind = \"127.0.0.1:{HUB_PORT}\"\npublic_host = \"127.0.0.1\"\nowner = \"dono\"\n\
             [livekit]\napi_key = \"{API_KEY}\"\napi_secret = \"{API_SECRET}\"\nsignal_port = {LK_SIGNAL}\nrtc_tcp_port = 7971\nrtc_udp_port = 7972\n"
        ),
    )
    .unwrap();
    let bin = livekit_bin();
    assert!(bin.exists(), "no LiveKit binary at {}", bin.display());
    let server = start(Options { data_dir: dir.path().to_path_buf(), no_livekit: false, livekit_bin: bin, bind_override: None }).await.unwrap();
    wait_port(LK_SIGNAL).await;
    let base = format!("http://127.0.0.1:{HUB_PORT}");
    let owner = HubClient::new(&base).unwrap();
    owner.login("dono", "").await.unwrap();
    let member = HubClient::new(&base).unwrap();
    let bia = member.login("bia", "").await.unwrap().user;
    let room_ch = owner.channels().await.unwrap().into_iter().find(|c| c.kind == ChannelKind::Voice).unwrap().id;
    let ident = bia.id.to_string();

    let (listener, _lrx) = join(&owner, room_ch, VoiceConfig { mic: false, playback: false, ..Default::default() }).await;
    let (speaker, _srx) = join(&member, room_ch, VoiceConfig { mic: true, playback: false, mic_test_tone: Some(440.0), ..Default::default() }).await;

    // The tone lights her ring; muting turns it off within about the release time.
    let on = until(&listener, &ident, true, Duration::from_secs(10)).await;
    println!("lit after {on:?} ms");
    assert!(on.is_some(), "her voice never lit the ring");
    speaker.set_muted(true);
    let off = until(&listener, &ident, false, Duration::from_secs(5)).await;
    println!("muted: off after {off:?} ms");
    assert!(off.is_some_and(|ms| ms < 1500), "the ring stayed on after she muted: {off:?}");
    speaker.set_muted(false);
    let on = until(&listener, &ident, true, Duration::from_secs(3)).await;
    println!("unmuted: lit after {on:?} ms");
    assert!(on.is_some_and(|ms| ms < 1000), "{on:?}");

    // Deafened: her audio is paused at the SFU. Whatever still reaches the listener, her ring
    // keeps following her voice (from LiveKit's speakers when the decoded audio says nothing).
    listener.set_deafened(true);
    let chunks = flow(&listener, &ident, 3).await;
    println!("deafened: {chunks} chunks from her in 3 s; speaking now {:?}", listener.speaking());
    let lit = until(&listener, &ident, true, Duration::from_secs(5)).await;
    println!("deafened, she speaks: lit after {lit:?} ms");
    assert!(lit.is_some(), "deafened: her ring went dark while she speaks ({chunks} chunks in 3 s)");
    speaker.set_muted(true);
    let off = until(&listener, &ident, false, Duration::from_secs(5)).await;
    println!("deafened, she mutes: off after {off:?} ms");
    assert!(off.is_some(), "deafened: her ring stayed on after she muted");
    speaker.set_muted(false);
    let on = until(&listener, &ident, true, Duration::from_secs(5)).await;
    println!("deafened, she unmutes: lit after {on:?} ms");
    assert!(on.is_some(), "deafened: her ring did not come back");

    // Undeafened again: the decoded level decides once more.
    listener.set_deafened(false);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(listener.speaking().contains(&ident));
    speaker.set_muted(true);
    let off = until(&listener, &ident, false, Duration::from_secs(5)).await;
    println!("undeafened, she mutes: off after {off:?} ms");
    assert!(off.is_some_and(|ms| ms < 1500), "{off:?}");

    speaker.leave().await;
    listener.leave().await;
    server.shutdown().await;
}
