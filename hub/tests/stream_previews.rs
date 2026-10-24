//! Go Live hover previews (v1.1): a real hub on an ephemeral port. The capability in `Ready`, an
//! upload while live (picture, app, icon), a viewer outside the voice room getting it, the upload
//! rate limit, the icon left out when the viewer has it, a private room (somebody without access
//! gets `404` as for no live at all) and the preview dropped when the live moves or ends.

use std::time::{Duration, Instant};

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{json, Value};
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

#[derive(Clone)]
struct Who {
    token: String,
    user: User,
}

async fn setup() -> Env {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "server_name = \"testsrv\"\nserver_password = \"{PASSWORD}\"\nowner = \"Dono\"\n\
             [livekit]\napi_key = \"APItest0000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123456789abcdef\"\nsignal_port = 1\n"
        ),
    )
    .unwrap();
    let server = start(Options { data_dir: dir.path().to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: Some("127.0.0.1:0".into()) }).await.unwrap();
    let base = format!("127.0.0.1:{}", server.addr.port());
    Env { _server: server, base, http: reqwest::Client::new(), _dir: dir }
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

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

    async fn post(&self, who: &Who, path: &str, body: Value) -> reqwest::Response {
        self.http.post(self.url(path)).bearer_auth(&who.token).json(&body).send().await.unwrap()
    }

    async fn put_preview(&self, who: &Who, up: &StreamPreviewUpload) -> StatusCode {
        self.http.put(self.url("/api/me/stream-preview")).bearer_auth(&who.token).json(up).send().await.unwrap().status()
    }

    async fn preview(&self, who: &Who, of: UserId, icon: Option<&str>) -> Result<StreamPreview, StatusCode> {
        let mut path = format!("/api/users/{of}/stream-preview");
        if let Some(h) = icon {
            path.push_str(&format!("?icon={h}"));
        }
        let r = self.http.get(self.url(&path)).bearer_auth(&who.token).send().await.unwrap();
        match r.status() {
            StatusCode::OK => Ok(r.json().await.unwrap()),
            s => Err(s),
        }
    }

    /// Opens the WebSocket; returns it and whether `Ready` announced the previews.
    async fn ws(&self, who: &Who) -> (Socket, bool) {
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws?token={}", self.base, who.token)).await.unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let msg = tokio::time::timeout(until.saturating_duration_since(Instant::now()), ws.next()).await.expect("no Ready").unwrap().unwrap();
            if let WsMsg::Text(t) = msg {
                if let Ok(ServerEvent::Ready { stream_previews, .. }) = serde_json::from_str::<ServerEvent>(&t) {
                    return (ws, stream_previews);
                }
            }
        }
    }

    async fn channels(&self, who: &Who) -> Vec<Channel> {
        self.http.get(self.url("/api/channels")).bearer_auth(&who.token).send().await.unwrap().json().await.unwrap()
    }
}

async fn voice(ws: &mut Socket, ch: Option<ChannelId>, streaming: bool) {
    let ev = ClientEvent::VoiceState { channel_id: ch, muted: false, deafened: false, streaming, camera: false, audio_only: false };
    ws.send(WsMsg::Text(serde_json::to_string(&ev).unwrap().into())).await.unwrap();
    // (The hub applies it before it answers anything sent after it on the same socket.)
    let ping = ClientEvent::Ping { nonce: 7 };
    ws.send(WsMsg::Text(serde_json::to_string(&ping).unwrap().into())).await.unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let msg = tokio::time::timeout(until.saturating_duration_since(Instant::now()), ws.next()).await.expect("no pong").unwrap().unwrap();
        if let WsMsg::Text(t) = msg {
            if matches!(serde_json::from_str::<ServerEvent>(&t), Ok(ServerEvent::Pong { nonce: 7 })) {
                return;
            }
        }
    }
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

fn jpeg() -> Vec<u8> {
    let img = image::RgbImage::from_fn(320, 180, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 40]));
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 65).encode_image(&img).unwrap();
    out
}

fn png() -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(32, 32, image::Rgba([240, 180, 76, 255]));
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
    out
}

#[tokio::test]
async fn previews_follow_the_live_and_the_channel_access() {
    let env = setup().await;
    let dono = env.login("Dono").await;
    assert_eq!(env.post(&dono, "/api/account/password", json!({ "new": "dono-senha" })).await.status(), StatusCode::OK);
    let ana = env.login("ana").await;
    let bia = env.login("bia").await;
    let (mut ws, announced) = env.ws(&dono).await;
    assert!(announced, "Ready says the hub keeps hover previews");
    let public_voice = env.channels(&dono).await.iter().find(|c| c.kind == ChannelKind::Voice).unwrap().id;
    let private: Channel = env
        .post(&dono, "/api/admin/channels", json!(ChannelCreateRequest { name: "cofre".into(), kind: ChannelKind::Voice, private: Some(PrivateAccess { roles: vec![], users: vec![ana.user.id] }) }))
        .await
        .json()
        .await
        .unwrap();

    let full = StreamPreviewUpload {
        jpeg: Some(b64(&jpeg())),
        app: Some(StreamApp { name: "Counter-Strike 2".into(), is_game: true, screen: false }),
        icon_png: Some(b64(&png())),
    };
    // Without a login: nothing either way.
    assert_eq!(env.http.put(env.url("/api/me/stream-preview")).json(&full).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(env.http.get(env.url(&format!("/api/users/{}/stream-preview", dono.user.id))).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(env.http.delete(env.url("/api/me/stream-preview")).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    // A body past the cap is cut off before it is read whole (`413`), live or not.
    let huge = format!(r#"{{"jpeg":"{}"}}"#, "A".repeat(STREAM_PREVIEW_MAX_BODY + 1));
    let r = env.http.put(env.url("/api/me/stream-preview")).bearer_auth(&dono.token).header("content-type", "application/json").body(huge).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
    // Not live: refused.
    assert_eq!(env.put_preview(&dono, &full).await, StatusCode::FORBIDDEN);
    voice(&mut ws, Some(public_voice), false).await;
    assert_eq!(env.put_preview(&dono, &full).await, StatusCode::FORBIDDEN, "in voice but not live");
    voice(&mut ws, Some(public_voice), true).await;
    assert_eq!(env.preview(&bia, dono.user.id, None).await.unwrap_err(), StatusCode::NOT_FOUND, "live, nothing sent yet");
    assert_eq!(env.put_preview(&dono, &full).await, StatusCode::OK);

    // bia is not in the voice room and sees the preview of a public room's live.
    let p = env.preview(&bia, dono.user.id, None).await.unwrap();
    assert_eq!((p.user_id, p.channel_id), (dono.user.id, public_voice));
    assert_eq!(p.jpeg.as_deref(), full.jpeg.as_deref());
    assert_eq!(p.app.as_ref().map(|a| (a.name.as_str(), a.is_game)), Some(("Counter-Strike 2", true)));
    let hash = p.icon_hash.clone().expect("an icon hash");
    assert_eq!(p.icon_png.as_deref(), full.icon_png.as_deref());
    // With the icon already in hand, its bytes are left out.
    let p = env.preview(&bia, dono.user.id, Some(&hash)).await.unwrap();
    assert!(p.icon_png.is_none() && p.icon_hash.as_deref() == Some(hash.as_str()) && p.jpeg.is_some());
    // Uploads too close together are refused.
    let frame = StreamPreviewUpload { jpeg: Some(b64(&jpeg())), ..Default::default() };
    assert_eq!(env.put_preview(&dono, &frame).await, StatusCode::TOO_MANY_REQUESTS);
    // A broken picture is refused.
    tokio::time::sleep(Duration::from_millis(3100)).await;
    assert_eq!(env.put_preview(&dono, &StreamPreviewUpload { jpeg: Some(b64(&png())), ..Default::default() }).await, StatusCode::BAD_REQUEST);

    // The live moves to the private room: the public room's picture is gone at once.
    voice(&mut ws, Some(private.id), true).await;
    assert_eq!(env.preview(&ana, dono.user.id, None).await.unwrap_err(), StatusCode::NOT_FOUND, "dropped with the move");
    assert_eq!(env.put_preview(&dono, &full).await, StatusCode::OK);
    let p = env.preview(&ana, dono.user.id, None).await.unwrap();
    assert_eq!(p.channel_id, private.id, "ana has access");
    assert_eq!(env.preview(&bia, dono.user.id, None).await.unwrap_err(), StatusCode::NOT_FOUND, "bia may not see the private room");

    // ana loses access mid-live: the very next lookup is a 404 (checked on every request).
    let revoke = ChannelAccessRequest { private: Some(PrivateAccess { roles: vec![], users: vec![] }) };
    assert_eq!(env.post(&dono, &format!("/api/admin/channels/{}/access", private.id), json!(revoke)).await.status(), StatusCode::OK);
    assert_eq!(env.preview(&ana, dono.user.id, None).await.unwrap_err(), StatusCode::NOT_FOUND, "access removed");
    assert!(env.preview(&dono, dono.user.id, None).await.is_ok(), "the streamer still sees their own");
    let give_back = ChannelAccessRequest { private: Some(PrivateAccess { roles: vec![], users: vec![ana.user.id] }) };
    assert_eq!(env.post(&dono, &format!("/api/admin/channels/{}/access", private.id), json!(give_back)).await.status(), StatusCode::OK);
    assert!(env.preview(&ana, dono.user.id, None).await.is_ok());

    // The streamer turns hover previews off: dropped at once.
    let del = env.http.delete(env.url("/api/me/stream-preview")).bearer_auth(&dono.token).send().await.unwrap();
    assert_eq!(del.status(), StatusCode::OK);
    assert_eq!(env.preview(&ana, dono.user.id, None).await.unwrap_err(), StatusCode::NOT_FOUND, "deleted");
    tokio::time::sleep(Duration::from_millis(3100)).await;
    assert_eq!(env.put_preview(&dono, &full).await, StatusCode::OK);

    // The live becomes sound only: no picture, and none comes back with the screen.
    let audio_only = ClientEvent::VoiceState { channel_id: Some(private.id), muted: false, deafened: false, streaming: true, camera: false, audio_only: true };
    ws.send(WsMsg::Text(serde_json::to_string(&audio_only).unwrap().into())).await.unwrap();
    voice(&mut ws, Some(private.id), true).await;
    assert_eq!(env.preview(&ana, dono.user.id, None).await.unwrap_err(), StatusCode::NOT_FOUND, "dropped with the switch to sound only");
    tokio::time::sleep(Duration::from_millis(3100)).await;
    assert_eq!(env.put_preview(&dono, &full).await, StatusCode::OK);

    // The live ends: nothing for anybody.
    voice(&mut ws, Some(private.id), false).await;
    assert_eq!(env.preview(&ana, dono.user.id, None).await.unwrap_err(), StatusCode::NOT_FOUND);
    assert_eq!(env.preview(&dono, dono.user.id, None).await.unwrap_err(), StatusCode::NOT_FOUND);
    // Live again without a new upload: still nothing (the old picture was dropped).
    voice(&mut ws, Some(private.id), true).await;
    assert_eq!(env.preview(&ana, dono.user.id, None).await.unwrap_err(), StatusCode::NOT_FOUND);
    // Disconnecting drops it too.
    assert_eq!(env.put_preview(&dono, &full).await, StatusCode::OK);
    assert!(env.preview(&ana, dono.user.id, None).await.is_ok());
    drop(ws);
    let until = Instant::now() + Duration::from_secs(5);
    while env.preview(&ana, dono.user.id, None).await.is_ok() {
        assert!(Instant::now() < until, "the preview outlived the connection");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// v1.1 (E1): a live in an end-to-end encrypted room never leaves a picture with the hub: an
/// upload with one is refused whole (`403`), the name and icon alone are kept and shown.
#[tokio::test]
async fn an_encrypted_rooms_live_has_no_picture() {
    let env = setup().await;
    let dono = env.login("Dono").await;
    assert_eq!(env.post(&dono, "/api/account/password", json!({ "new": "dono-senha" })).await.status(), StatusCode::OK);
    let bia = env.login("bia").await;
    let (mut ws, _) = env.ws(&dono).await;
    let sala: Channel = env.post(&dono, "/api/admin/channels", json!(ChannelCreateRequest { name: "sigilo".into(), kind: ChannelKind::Voice, private: None })).await.json().await.unwrap();
    let r = env.post(&dono, &format!("/api/channels/{}/e2ee", sala.id), json!(ChannelE2eeRequest { on: true })).await;
    assert_eq!(r.status(), StatusCode::OK);
    voice(&mut ws, Some(sala.id), true).await;

    let app = StreamApp { name: "Counter-Strike 2".into(), is_game: true, screen: false };
    let full = StreamPreviewUpload { jpeg: Some(b64(&jpeg())), app: Some(app.clone()), icon_png: Some(b64(&png())) };
    let r = env.http.put(env.url("/api/me/stream-preview")).bearer_auth(&dono.token).json(&full).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert_eq!(r.json::<ApiError>().await.unwrap().error, ERR_STREAM_PREVIEW_E2EE);
    assert_eq!(env.preview(&bia, dono.user.id, None).await.unwrap_err(), StatusCode::NOT_FOUND, "nothing was kept");
    let named = StreamPreviewUpload { jpeg: None, app: Some(app), icon_png: Some(b64(&png())) };
    assert_eq!(env.put_preview(&dono, &named).await, StatusCode::OK);
    let p = env.preview(&bia, dono.user.id, None).await.unwrap();
    assert!(p.jpeg.is_none(), "never a picture");
    assert_eq!(p.app.map(|a| a.name), Some("Counter-Strike 2".to_string()));
    assert!(p.icon_png.is_some());
}
