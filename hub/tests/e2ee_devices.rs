//! End-to-end encryption, E0 (v1.1): the directory of device keys on a real hub (ephemeral port).
//! Register, list and delete; the five-device cap; a registered (id, key) pair never changes
//! (`409`); nobody removes somebody else's device; validation; the rate limit; the
//! `E2eeDevicesChanged` broadcast and `Ready.e2ee`; and an app that predates all of it.

use std::time::{Duration, Instant};

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message as WsMsg;
use vox_proto::*;
use voxhub_server::{start, Options, Server};

const PASSWORD: &str = "hunter2";

struct Env {
    _server: Server,
    base: String,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}

struct Who {
    token: String,
    user: User,
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn setup() -> Env {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "server_name = \"testsrv\"\nserver_password = \"{PASSWORD}\"\n\
             [livekit]\napi_key = \"APItest0000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123456789abcdef\"\nsignal_port = 1\n"
        ),
    )
    .unwrap();
    let server = start(Options { data_dir: dir.path().to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: Some("127.0.0.1:0".into()) }).await.unwrap();
    let base = format!("127.0.0.1:{}", server.addr.port());
    Env { _server: server, base, http: reqwest::Client::new(), _dir: dir }
}

fn dev(n: u8) -> String {
    format!("{n:02x}").repeat(16)
}

fn key(n: u8) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([n; 32])
}

impl Env {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.base)
    }

    async fn login(&self, name: &str) -> Who {
        let req = LoginRequest { username: name.into(), server_password: PASSWORD.into(), protocol_version: PROTOCOL_VERSION, ..Default::default() };
        let r = self.http.post(self.url("/api/login")).json(&req).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "login {name}");
        let l: LoginResponse = r.json().await.unwrap();
        Who { token: l.token, user: l.user }
    }

    async fn put(&self, who: &Who, device: &str, body: serde_json::Value) -> (StatusCode, String) {
        let r = self.http.put(self.url(&format!("/api/e2ee/devices/{device}"))).bearer_auth(&who.token).json(&body).send().await.unwrap();
        (r.status(), r.text().await.unwrap())
    }

    async fn register(&self, who: &Who, n: u8, label: &str) -> (StatusCode, String) {
        self.put(who, &dev(n), json!({ "public_key": key(n), "label": label })).await
    }

    async fn delete(&self, who: &Who, device: &str) -> StatusCode {
        self.http.delete(self.url(&format!("/api/e2ee/devices/{device}"))).bearer_auth(&who.token).send().await.unwrap().status()
    }

    async fn list(&self, who: &Who) -> Vec<E2eeDevice> {
        let r = self.http.get(self.url("/api/e2ee/devices")).bearer_auth(&who.token).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    }

    /// The WebSocket and its `Ready`.
    async fn ws(&self, who: &Who) -> (Socket, String) {
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws?token={}", self.base, who.token)).await.unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let msg = tokio::time::timeout(until.saturating_duration_since(Instant::now()), ws.next()).await.expect("no Ready").unwrap().unwrap();
            if let WsMsg::Text(t) = msg {
                if t.contains(r#""type":"ready""#) {
                    return (ws, t.to_string());
                }
            }
        }
    }
}

/// Every text frame for `ms` milliseconds.
async fn frames(ws: &mut Socket, ms: u64) -> Vec<String> {
    let mut out = Vec::new();
    let until = Instant::now() + Duration::from_millis(ms);
    while let Ok(Some(Ok(m))) = tokio::time::timeout(until.saturating_duration_since(Instant::now()), ws.next()).await {
        if let WsMsg::Text(t) = m {
            out.push(t.to_string());
        }
    }
    out
}

fn device_events(frames: &[String]) -> Vec<(UserId, Vec<E2eeDevice>)> {
    frames
        .iter()
        .filter_map(|f| match serde_json::from_str::<ServerEvent>(f) {
            Ok(ServerEvent::E2eeDevicesChanged { user_id, devices }) => Some((user_id, devices)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn register_list_delete_and_the_broadcast() {
    let env = setup().await;
    let (ana, bia) = (env.login("ana").await, env.login("bia").await);
    let (mut bia_ws, ready) = env.ws(&bia).await;
    assert!(matches!(serde_json::from_str::<ServerEvent>(&ready).unwrap(), ServerEvent::Ready { e2ee: true, .. }), "{ready}");
    assert!(env.list(&ana).await.is_empty());

    // Ana registers her PC: everybody hears it.
    let (st, body) = env.register(&ana, 1, "  PC\u{7} da sala ").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let d: E2eeDevice = serde_json::from_str(&body).unwrap();
    assert_eq!((d.user_id, d.device_id.as_str(), d.public_key.as_str(), d.label.as_str()), (ana.user.id, dev(1).as_str(), key(1).as_str(), "PC da sala"));
    assert!(d.created_at_ms > 0);
    let ev = device_events(&frames(&mut bia_ws, 600).await);
    assert_eq!(ev, vec![(ana.user.id, vec![d.clone()])]);

    // The same device again (a new login): the same answer, nothing to tell anybody.
    let (st, body) = env.register(&ana, 1, "PC da sala").await;
    assert_eq!((st, serde_json::from_str::<E2eeDevice>(&body).unwrap()), (StatusCode::OK, d.clone()));
    assert!(device_events(&frames(&mut bia_ws, 400).await).is_empty(), "a confirmation is not news");
    // Renamed: news.
    let (st, _) = env.register(&ana, 1, "Notebook").await;
    assert_eq!(st, StatusCode::OK);
    let ev = device_events(&frames(&mut bia_ws, 600).await);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].1[0].label, "Notebook");

    // A second device; Bia's own; the list has everybody's.
    assert_eq!(env.register(&ana, 2, "Mac").await.0, StatusCode::OK);
    assert_eq!(env.register(&bia, 9, "PC da Bia").await.0, StatusCode::OK);
    let all = env.list(&bia).await;
    let mine: Vec<(UserId, String)> = all.iter().map(|d| (d.user_id, d.device_id.clone())).collect();
    assert_eq!(mine, vec![(ana.user.id, dev(1)), (ana.user.id, dev(2)), (bia.user.id, dev(9))]);
    let ev = device_events(&frames(&mut bia_ws, 600).await);
    assert_eq!(ev.iter().map(|(u, d)| (*u, d.len())).collect::<Vec<_>>(), vec![(ana.user.id, 2), (bia.user.id, 1)], "each change carries that member's whole list");

    // Bia cannot remove Ana's device (as if it did not exist), nor a device she does not have.
    assert_eq!(env.delete(&bia, &dev(1)).await, StatusCode::NOT_FOUND);
    assert_eq!(env.delete(&bia, &dev(3)).await, StatusCode::NOT_FOUND);
    assert_eq!(env.list(&bia).await.len(), 3);
    assert!(device_events(&frames(&mut bia_ws, 300).await).is_empty());
    // Ana removes her Mac: everybody hears the new list.
    assert_eq!(env.delete(&ana, &dev(2)).await, StatusCode::OK);
    let ev = device_events(&frames(&mut bia_ws, 600).await);
    assert_eq!(ev, vec![(ana.user.id, vec![E2eeDevice { label: "Notebook".into(), ..d.clone() }])]);
    assert_eq!(env.delete(&ana, &dev(2)).await, StatusCode::NOT_FOUND, "already gone");
    assert_eq!(env.list(&ana).await.len(), 2);

    // Without a session: nothing.
    let r = env.http.get(env.url("/api/e2ee/devices")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = env.http.put(env.url(&format!("/api/e2ee/devices/{}", dev(5)))).json(&json!({ "public_key": key(5), "label": "x" })).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn five_devices_and_keys_that_never_change() {
    let env = setup().await;
    let (ana, bia) = (env.login("ana").await, env.login("bia").await);
    for n in 1..=5 {
        assert_eq!(env.register(&ana, n, &format!("PC {n}")).await.0, StatusCode::OK);
    }
    let (st, body) = env.register(&ana, 6, "PC 6").await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(serde_json::from_str::<ApiError>(&body).unwrap().error, ERR_E2EE_TOO_MANY_DEVICES);
    // The ones she has still confirm.
    assert_eq!(env.register(&ana, 5, "PC 5").await.0, StatusCode::OK);
    // Another key under a registered id: refused, the old key stays.
    let (st, body) = env.put(&ana, &dev(1), json!({ "public_key": key(77), "label": "PC 1" })).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(serde_json::from_str::<ApiError>(&body).unwrap().error, ERR_E2EE_KEY_CHANGED);
    assert_eq!(env.list(&ana).await.iter().find(|d| d.device_id == dev(1)).unwrap().public_key, key(1));
    // Removing one makes room.
    assert_eq!(env.delete(&ana, &dev(3)).await, StatusCode::OK);
    assert_eq!(env.register(&ana, 6, "PC 6").await.0, StatusCode::OK);
    // Bia's cap is her own; the same id under her name is her own device.
    assert_eq!(env.put(&bia, &dev(1), json!({ "public_key": key(88), "label": "PC" })).await.0, StatusCode::OK);
    let all = env.list(&bia).await;
    assert_eq!(all.iter().filter(|d| d.user_id == ana.user.id).count(), 5);
    assert_eq!(all.iter().find(|d| d.user_id == bia.user.id).unwrap().public_key, key(88));
}

#[tokio::test]
async fn validation() {
    let env = setup().await;
    let ana = env.login("ana").await;
    let bad_ids = [dev(0xab).to_uppercase(), dev(1)[..31].to_string(), format!("{}0", dev(1)), "zz".repeat(16), "not-an-id".into()];
    for id in &bad_ids {
        let (st, body) = env.put(&ana, id, json!({ "public_key": key(1), "label": "PC" })).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "id {id}: {body}");
    }
    let std_alphabet = base64::engine::general_purpose::STANDARD_NO_PAD.encode([0xfb; 32]);
    let bad_keys = [
        format!("{}=", key(1)),
        key(1)[..42].to_string(),
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([1; 31]),
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([1; 33]),
        std_alphabet,
        key(0),
        String::new(),
        "!".repeat(43),
    ];
    for k in &bad_keys {
        let (st, body) = env.put(&ana, &dev(1), json!({ "public_key": k, "label": "PC" })).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "key {k:?}: {body}");
    }
    for label in ["", "   ", "\u{1}\u{2}", &"x".repeat(41)] {
        let (st, body) = env.put(&ana, &dev(1), json!({ "public_key": key(1), "label": label })).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "label {label:?}: {body}");
    }
    // Missing fields, not JSON.
    assert_eq!(env.put(&ana, &dev(1), json!({ "public_key": key(1) })).await.0, StatusCode::BAD_REQUEST);
    let r = env.http.put(env.url(&format!("/api/e2ee/devices/{}", dev(1)))).bearer_auth(&ana.token).body("oi").header("content-type", "application/json").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    // A label of exactly 40 characters (after trimming) is fine; nothing above was stored.
    assert!(env.list(&ana).await.is_empty());
    let forty = format!("  {}  ", "é".repeat(40));
    let (st, body) = env.put(&ana, &dev(1), json!({ "public_key": key(1), "label": forty })).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(env.list(&ana).await[0].label, "é".repeat(40));
    // (Every attempt above counted towards Ana's rate limit: another member checks the removal.)
    let bia = env.login("bia").await;
    assert_eq!(env.delete(&bia, "NOT-AN-ID").await, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn changes_are_rate_limited() {
    let env = setup().await;
    let (ana, bia) = (env.login("ana").await, env.login("bia").await);
    assert_eq!(env.register(&ana, 1, "PC").await.0, StatusCode::OK);
    for _ in 1..voxhub_server::e2ee::CHANGES_PER_WINDOW {
        assert_eq!(env.register(&ana, 1, "PC").await.0, StatusCode::OK);
    }
    let (st, body) = env.register(&ana, 1, "PC").await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(env.delete(&ana, &dev(1)).await, StatusCode::TOO_MANY_REQUESTS, "removals share the limit");
    // Per member: Bia is not slowed down; reading is never limited.
    assert_eq!(env.register(&bia, 2, "PC").await.0, StatusCode::OK);
    assert_eq!(env.list(&ana).await.len(), 2);
}

/// An app that predates E0 (v1.0.2's wire shapes): its `Ready` parses, the new event is one frame
/// it cannot parse and skips (as its event loop does), and everything after goes on as before.
#[tokio::test]
async fn an_older_app_is_unaffected() {
    #[derive(serde::Deserialize, Debug)]
    #[serde(tag = "type", rename_all = "snake_case")]
    #[allow(dead_code)]
    enum OldEvent {
        Ready { user: User, channels: Vec<Channel>, online: Vec<User>, voice: Vec<VoiceMember>, server_id: Option<String> },
        MessageCreated { message: Message },
        UserOnline { user: User },
        UserOffline { user_id: UserId },
        Pong { nonce: u64 },
    }
    let env = setup().await;
    let (old, ana) = (env.login("velho").await, env.login("ana").await);
    let (mut ws, ready) = env.ws(&old).await;
    assert!(matches!(serde_json::from_str::<OldEvent>(&ready), Ok(OldEvent::Ready { .. })), "{ready}");
    let _ = frames(&mut ws, 300).await;

    assert_eq!(env.register(&ana, 1, "PC").await.0, StatusCode::OK);
    let channels: Vec<Channel> = env.http.get(env.url("/api/channels")).bearer_auth(&old.token).send().await.unwrap().json().await.unwrap();
    let text = channels.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    ws.send(WsMsg::Text(serde_json::to_string(&ClientEvent::SendMessage { channel_id: text, content: "ainda funciona".into(), reply_to: None, reply_mention: false, mentions: vec![] }).unwrap().into())).await.unwrap();
    ws.send(WsMsg::Text(serde_json::to_string(&ClientEvent::Ping { nonce: 42 }).unwrap().into())).await.unwrap();
    let got = frames(&mut ws, 800).await;
    let (mut skipped, mut message, mut pong) = (0, false, false);
    for f in &got {
        match serde_json::from_str::<OldEvent>(f) {
            Ok(OldEvent::MessageCreated { message: m }) => message |= m.content == "ainda funciona",
            Ok(OldEvent::Pong { nonce: 42 }) => pong = true,
            Ok(_) => {}
            Err(_) => {
                assert!(f.contains(r#""type":"e2ee_devices_changed""#), "only the new event is unknown to it: {f}");
                skipped += 1;
            }
        }
    }
    assert_eq!(skipped, 1, "{got:?}");
    assert!(message && pong, "{got:?}");
}
