//! Host mode end to end (v0.8, "Criar / abrir servidor"): the app's in-process server, created in a
//! temp root, run by `HostRunner` on its own thread with the network part off (no router, no
//! STUN: the real router is never touched by tests) and ports from block 7 on (never the live
//! server's 7890 / 7880-7882). The host claims it from 127.0.0.1, makes an invite, a friend joins
//! with only the code, the host stops and reopens it (same ports, the friend's device still works),
//! and a busy port moves the server to the next free block, remembered for next time.

use std::net::{Ipv4Addr, TcpListener};
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde_json::json;
use vox_proto::*;
use voxhub_server::host::{self, ports::PortBlock, HostRunner, HostStatus, NetMode, Phase, RunOptions};

fn opts() -> RunOptions {
    RunOptions { net: NetMode::Off, mapper: None, bind_ip: Ipv4Addr::LOCALHOST, first_block: 7, livekit_bin: None, lan_override: Some((Ipv4Addr::LOCALHOST, 8)) }
}

async fn running(r: &HostRunner) -> HostStatus {
    let t = Instant::now();
    loop {
        let s = r.status();
        match &s.phase {
            Phase::Running => return s,
            Phase::Failed(e) => panic!("host failed: {e}"),
            _ => {}
        }
        assert!(t.elapsed() < Duration::from_secs(20), "not running: {s:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn login(http: &reqwest::Client, base: &str, req: LoginRequest) -> reqwest::Response {
    let req = LoginRequest { protocol_version: PROTOCOL_VERSION, ..req };
    http.post(format!("{base}/api/login")).json(&req).send().await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_invite_join_stop_and_reopen() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let root = tempfile::tempdir().unwrap();
    let h = host::create(root.path(), "Servidor da Tati").unwrap();
    assert_eq!(host::list(root.path()), vec![h.clone()]);
    let http = reqwest::Client::new();

    // A port of block 7 is busy (another program): the server takes block 8.
    let blocker = TcpListener::bind((Ipv4Addr::LOCALHOST, PortBlock::nth(7).signal)).unwrap();
    let mut runner = HostRunner::start(&h, opts(), Arc::new(|| {}));
    let st = running(&runner).await;
    assert_eq!(st.ports, Some(PortBlock::nth(8)));
    let base = format!("http://{}", st.hub_addr.unwrap());

    // The host's app: logs in through 127.0.0.1 with the server password and becomes the owner.
    let l: LoginResponse = login(&http, &base, LoginRequest { username: "Tati".into(), server_password: h.password.clone(), ..Default::default() }).await.json().await.unwrap();
    assert_eq!(l.user.role, Role::Owner);
    assert_eq!(l.server_name, "Servidor da Tati");
    let r = http.post(format!("{base}/api/account/password")).bearer_auth(&l.token).json(&json!({"new": "tati-senha-1"})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let owner: LoginResponse = login(&http, &base, LoginRequest { username: "Tati".into(), server_password: h.password.clone(), password: Some("tati-senha-1".into()), ..Default::default() })
        .await
        .json()
        .await
        .unwrap();

    // An invite, turned into the code the app shows (addresses from the host status).
    let created: InviteCreated = http.post(format!("{base}/api/invites")).bearer_auth(&owner.token).json(&json!({"max_uses": 5})).send().await.unwrap().json().await.unwrap();
    let code = InviteCode { token: invite_token_from_hex(&created.token).unwrap(), hub_port: st.hub_addr.unwrap().port(), addrs: st.invite_addrs(), tls_pin: st.tls_pin }.encode();

    // The friend: only the code.
    let decoded = InviteCode::decode(&code).unwrap();
    let urls = decoded.candidate_urls(&[(Ipv4Addr::LOCALHOST.into(), 8)]);
    assert_eq!(urls, [base.as_str()]);
    let joao: LoginResponse = login(&http, &urls[0], LoginRequest { username: "Joao".into(), invite: Some(decoded.token_hex()), remember_device: true, ..Default::default() })
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(joao.user.role, Role::Member, "a loopback login does not make a second owner");
    let device = joao.device_token.expect("device token");

    // Stop ("Parar servidor"): nothing listens; reopen ("Abrir servidor"): the block it used is free
    // again but block 8 was saved, so it stays there; Joao's computer still gets in.
    runner.stop_and_wait(Duration::from_secs(10));
    assert_eq!(runner.status().phase, Phase::Stopped);
    assert!(std::net::TcpStream::connect(st.hub_addr.unwrap()).is_err());
    drop(blocker);
    assert_eq!(host::load(&h.dir).unwrap().ports, PortBlock::nth(8));
    let runner = HostRunner::start(&h, opts(), Arc::new(|| {}));
    let st2 = running(&runner).await;
    assert_eq!(st2.ports, Some(PortBlock::nth(8)));
    let r = login(&http, &base, LoginRequest { username: "Joao".into(), device_token: Some(device), ..Default::default() }).await;
    assert_eq!(r.status(), StatusCode::OK);
    // The invite and its use survived.
    let list: Vec<Invite> = http.get(format!("{base}/api/invites")).bearer_auth(&owner.token).send().await.unwrap().json().await.unwrap();
    assert_eq!(list[0].uses, 1);
    drop(runner);
}
