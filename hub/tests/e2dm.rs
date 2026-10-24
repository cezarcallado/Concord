//! E2 (v1.1): end-to-end encrypted DMs as the hub sees them. A real hub on an ephemeral port. The
//! hub never reads a sealed line: it stores the body as is (`format = 'e2e1'`), hands it back
//! with the placeholder an older app shows, never adds the `[imagem]` marker or a reply excerpt,
//! keeps the size cap, and keeps every privacy rule (a third member, an Admin and the owner get
//! `404`). Sealed images are stored undecoded as `application/octet-stream`, go only on sealed
//! DM lines (`ERR_SEALED_ONLY_DM` on a channel or a readable line) and count against the quotas.

use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use vox_proto::*;
use voxhub_server::{start, Options, Server};

const PASSWORD: &str = "pw-e2dm";

struct Env {
    _server: Server,
    base: String,
    http: reqwest::Client,
    dir: tempfile::TempDir,
}

#[derive(Clone)]
struct Who {
    token: String,
    user: User,
}

async fn setup(extra: &str) -> Env {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "server_name = \"e2dm\"\nserver_password = \"{PASSWORD}\"\nowner = \"Dono\"\n{extra}\n\
             [livekit]\napi_key = \"APIe2dm0000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123456789abcdef\"\nsignal_port = 1\n"
        ),
    )
    .unwrap();
    let server = start(Options { data_dir: dir.path().to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: Some("127.0.0.1:0".into()) })
        .await
        .unwrap();
    let base = format!("127.0.0.1:{}", server.addr.port());
    Env { _server: server, base, http: reqwest::Client::new(), dir }
}

impl Env {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.base)
    }

    async fn who(&self, name: &str) -> Who {
        let req = LoginRequest { username: name.into(), server_password: PASSWORD.into(), protocol_version: PROTOCOL_VERSION, ..Default::default() };
        let r = self.http.post(self.url("/api/login")).json(&req).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "login {name}");
        let l: LoginResponse = r.json().await.unwrap();
        Who { token: l.token, user: l.user }
    }

    async fn post(&self, who: &Who, path: &str, body: Value) -> reqwest::Response {
        self.http.post(self.url(path)).bearer_auth(&who.token).json(&body).send().await.unwrap()
    }

    async fn get(&self, who: &Who, path: &str) -> reqwest::Response {
        self.http.get(self.url(path)).bearer_auth(&who.token).send().await.unwrap()
    }

    async fn open(&self, who: &Who, other: &Who) -> DmConversation {
        let r = self.post(who, &format!("/api/dms/{}", other.user.id), json!({})).await;
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    }

    async fn upload(&self, who: &Who, main: &[u8], thumb: &[u8], sealed: bool) -> reqwest::Response {
        let mut body = main.to_vec();
        body.extend(thumb);
        let q = if sealed { format!("?sealed=1&thumb_len={}", thumb.len()) } else { format!("?thumb_len={}", thumb.len()) };
        self.http.post(self.url(&format!("/api/attachments{q}"))).bearer_auth(&who.token).body(body).send().await.unwrap()
    }

    async fn history(&self, who: &Who, dm: DmId) -> Vec<DmMessage> {
        let r = self.get(who, &format!("/api/dms/{dm}/messages")).await;
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    }

    async fn ws(&self, who: &Who) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws?token={}", self.base, who.token)).await.unwrap();
        ws
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.dir.path().join("voxhub.db")).unwrap()
    }
}

async fn error_of(r: reqwest::Response) -> (StatusCode, String) {
    let s = r.status();
    (s, r.json::<ApiError>().await.map(|e| e.error).unwrap_or_default())
}

/// Random-looking bytes (what a sealed body or file is to the hub).
fn noise(n: usize, seed: u32) -> Vec<u8> {
    let mut s = seed | 1;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s >> 24) as u8
        })
        .collect()
}

fn b64(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x * 7) as u8, (y * 5) as u8, 90]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

async fn next_dm_line(ws: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>) -> DmMessage {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(5) {
        let Ok(Some(Ok(WsMsg::Text(txt)))) = tokio::time::timeout(Duration::from_secs(5), ws.next()).await else { continue };
        if let Ok(ServerEvent::DmMessageCreated { message }) = serde_json::from_str::<ServerEvent>(&txt) {
            return message;
        }
    }
    panic!("no DmMessageCreated");
}

#[tokio::test]
async fn a_sealed_line_is_stored_as_is_and_old_apps_get_the_placeholder() {
    let env = setup("").await;
    let (ana, bia) = (env.who("Ana").await, env.who("Bia").await);
    let dm = env.open(&ana, &bia).await;
    let mut ws_bia = env.ws(&bia).await;
    let body = b64(&noise(900, 1));

    let r = env.post(&ana, &format!("/api/dms/{}/messages", dm.id), json!({ "content": "", "sealed": body })).await;
    assert_eq!(r.status(), StatusCode::OK);
    let sent: DmMessage = r.json().await.unwrap();
    assert_eq!(sent.content, DM_SEALED_PLACEHOLDER);
    assert_eq!(sent.sealed.as_deref(), Some(body.as_str()));
    assert!(sent.attachments.is_empty() && sent.sealed_attachments.is_empty());
    assert_eq!(next_dm_line(&mut ws_bia).await, sent, "the event is the same line");

    // History and the list's newest line: the same.
    let lines = env.history(&bia, dm.id).await;
    assert_eq!(lines, vec![sent.clone()]);
    let list: Vec<DmConversation> = env.get(&bia, "/api/dms").await.json().await.unwrap();
    assert_eq!(list[0].last_message.as_ref(), Some(&sent));
    assert_eq!(list[0].unread, 1, "unread counts work on sealed lines");

    // What an old app reads: the placeholder, never the body as text.
    let raw: Value = env.get(&bia, &format!("/api/dms/{}/messages", dm.id)).await.json().await.unwrap();
    assert_eq!(raw[0]["content"], "Mensagem criptografada. Atualize o concord para ler.");

    // In the database: the body as sent, format e2e1.
    let (format, stored): (String, String) = env.db().query_row("SELECT format, body FROM dm_messages WHERE id = ?", [sent.id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!((format.as_str(), stored.as_str()), ("e2e1", body.as_str()));

    // A reply to it: no excerpt from the hub, marked sealed; a sealed reply to a plain line keeps it.
    let r = env.post(&bia, &format!("/api/dms/{}/messages", dm.id), json!({ "sealed": b64(&noise(200, 2)), "reply_to": sent.id })).await;
    assert_eq!(r.status(), StatusCode::OK);
    let reply: DmMessage = r.json().await.unwrap();
    let q = reply.reply.clone().unwrap();
    assert_eq!((q.id, q.excerpt.as_str(), q.sealed, q.deleted), (sent.id, "", true, false));
    let lines = env.history(&ana, dm.id).await;
    assert_eq!(lines[0].reply.as_ref().map(|r| (r.sealed, r.excerpt.clone())), Some((true, String::new())));
    let plain = env.post(&ana, &format!("/api/dms/{}/messages", dm.id), json!({ "content": "oi em claro" })).await.json::<DmMessage>().await.unwrap();
    assert!(plain.sealed.is_none() && plain.content == "oi em claro");
    let r: DmMessage = env.post(&bia, &format!("/api/dms/{}/messages", dm.id), json!({ "sealed": b64(&noise(64, 3)), "reply_to": plain.id })).await.json().await.unwrap();
    assert_eq!(r.reply.map(|q| (q.sealed, q.excerpt)), Some((false, "oi em claro".to_string())));

    // Reactions on a sealed line stay readable.
    let r = env.post(&bia, &format!("/api/dms/{}/messages/{}/reactions", dm.id, sent.id), json!({"emoji": "👍", "add": true})).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(env.history(&ana, dm.id).await.iter().find(|m| m.id == sent.id).unwrap().reactions[0].emoji, "👍");
}

#[tokio::test]
async fn the_sealed_size_cap_and_shape() {
    let env = setup("").await;
    let (ana, bia) = (env.who("Ana").await, env.who("Bia").await);
    let dm = env.open(&ana, &bia).await;
    let path = format!("/api/dms/{}/messages", dm.id);
    let exact = "A".repeat(DM_SEALED_MAX_BYTES);
    assert_eq!(env.post(&ana, &path, json!({ "sealed": exact })).await.status(), StatusCode::OK, "exactly the cap");
    for bad in [json!({ "sealed": "A".repeat(DM_SEALED_MAX_BYTES + 1) }), json!({ "sealed": "" }), json!({ "sealed": "não é base64" }), json!({ "sealed": "AAAA==" }), json!({ "content": "texto às claras", "sealed": "AAAA" })] {
        assert_eq!(env.post(&ana, &path, bad.clone()).await.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
    assert_eq!(env.history(&bia, dm.id).await.len(), 1);
}

#[tokio::test]
async fn nobody_else_reaches_a_sealed_line_or_its_images() {
    let env = setup("").await;
    let owner = env.who("Dono").await;
    assert_eq!(env.post(&owner, "/api/account/password", json!({"new": "dono-senha"})).await.status(), StatusCode::OK);
    let adm = env.who("Adm").await;
    assert_eq!(env.post(&adm, "/api/account/password", json!({"new": "adm-senha"})).await.status(), StatusCode::OK);
    assert_eq!(env.post(&owner, &format!("/api/admin/users/{}/role", adm.user.id), json!({"role": "admin"})).await.status(), StatusCode::OK);
    let (ana, bia, caio) = (env.who("Ana").await, env.who("Bia").await, env.who("Caio").await);
    let dm = env.open(&ana, &bia).await;
    let r = env.upload(&ana, &noise(5000, 7), &noise(700, 8), true).await;
    assert_eq!(r.status(), StatusCode::OK);
    let att: Attachment = r.json().await.unwrap();
    let r = env.post(&ana, &format!("/api/dms/{}/messages", dm.id), json!({ "sealed": b64(&noise(300, 9)), "attachments": [att.id] })).await;
    assert_eq!(r.status(), StatusCode::OK);
    let line: DmMessage = r.json().await.unwrap();
    for who in [&caio, &adm, &owner] {
        assert_eq!(env.get(who, &format!("/api/dms/{}/messages", dm.id)).await.status(), StatusCode::NOT_FOUND, "{}", who.user.username);
        assert_eq!(env.get(who, &format!("/api/attachments/{}", att.id)).await.status(), StatusCode::NOT_FOUND, "{}", who.user.username);
        assert_eq!(env.get(who, &format!("/api/attachments/{}/thumb", att.id)).await.status(), StatusCode::NOT_FOUND, "{}", who.user.username);
        let r = env.post(who, &format!("/api/dms/{}/messages/{}/delete", dm.id, line.id), json!({})).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }
    assert_eq!(env.get(&bia, &format!("/api/attachments/{}", att.id)).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn sealed_images_are_stored_undecoded_and_only_go_on_sealed_lines() {
    let env = setup("attachment_daily_mb = 1").await;
    let (ana, bia) = (env.who("Ana").await, env.who("Bia").await);
    let dm = env.open(&ana, &bia).await;
    let limits: AttachmentLimits = env.get(&ana, "/api/attachments/limits").await.json().await.unwrap();
    assert!(limits.sealed, "the hub says it takes sealed uploads");
    let before = limits.daily_bytes_left;

    let (main, thumb) = (noise(40_000, 11), noise(3_000, 12));
    let r = env.upload(&ana, &main, &thumb, true).await;
    assert_eq!(r.status(), StatusCode::OK);
    let att: Attachment = r.json().await.unwrap();
    assert_eq!((att.mime.as_str(), att.width, att.height, att.thumb_w, att.thumb_h, att.sealed, att.size), (ATTACHMENT_SEALED_MIME, 0, 0, 0, 0, true, main.len() as u64));
    // Without sealed=1 the same bytes are refused (not an image).
    assert_eq!(env.upload(&ana, &main, &[], false).await.status(), StatusCode::BAD_REQUEST);
    // Quotas apply: the daily allowance went down by both parts.
    let limits: AttachmentLimits = env.get(&ana, "/api/attachments/limits").await.json().await.unwrap();
    assert_eq!(before - limits.daily_bytes_left, (main.len() + thumb.len()) as u64);
    let r = env.upload(&ana, &noise(1024 * 1024, 13), &[], true).await;
    assert_eq!(error_of(r).await, (StatusCode::TOO_MANY_REQUESTS, ERR_ATTACHMENT_DAILY.to_string()), "the daily allowance holds for sealed files");
    // Too short to be ciphertext, a thumbnail too long.
    assert_eq!(env.upload(&ana, &noise(10, 1), &[], true).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(env.upload(&ana, &noise(100, 1), &noise(ATTACHMENT_MAX_THUMB_BYTES + 17, 2), true).await.status(), StatusCode::BAD_REQUEST);

    // Not on a channel, not on a readable DM line.
    let chs: Vec<Channel> = env.get(&ana, "/api/channels").await.json().await.unwrap();
    let geral = chs.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    let r = env.post(&ana, &format!("/api/channels/{geral}/messages"), json!({ "content": "", "attachments": [att.id] })).await;
    assert_eq!(error_of(r).await, (StatusCode::BAD_REQUEST, ERR_SEALED_ONLY_DM.to_string()));
    let r = env.post(&ana, &format!("/api/dms/{}/messages", dm.id), json!({ "content": "", "attachments": [att.id] })).await;
    assert_eq!(error_of(r).await, (StatusCode::BAD_REQUEST, ERR_SEALED_ONLY_DM.to_string()));
    // A readable image does not go on a sealed line either.
    let plain_img: Attachment = env.upload(&ana, &png(32, 24), &[], false).await.json().await.unwrap();
    let r = env.post(&ana, &format!("/api/dms/{}/messages", dm.id), json!({ "sealed": "AAAA", "attachments": [plain_img.id] })).await;
    assert_eq!(error_of(r).await, (StatusCode::BAD_REQUEST, ERR_SEALED_ONLY_DM.to_string()));

    // On a sealed line: in `sealed_attachments`, no marker, files served exactly as uploaded.
    let r = env.post(&ana, &format!("/api/dms/{}/messages", dm.id), json!({ "sealed": "AAAA", "attachments": [att.id] })).await;
    assert_eq!(r.status(), StatusCode::OK);
    let line: DmMessage = r.json().await.unwrap();
    assert!(line.attachments.is_empty());
    assert_eq!(line.sealed_attachments.iter().map(|a| a.id).collect::<Vec<_>>(), vec![att.id]);
    assert_eq!(line.content, DM_SEALED_PLACEHOLDER, "no [imagem] marker");
    let hist = env.history(&bia, dm.id).await;
    assert_eq!((hist[0].attachments.len(), hist[0].sealed_attachments.len()), (0, 1));
    let got = env.get(&bia, &format!("/api/attachments/{}", att.id)).await;
    assert_eq!(got.headers()["content-type"], "application/octet-stream");
    assert_eq!(got.bytes().await.unwrap().to_vec(), main);
    assert_eq!(env.get(&bia, &format!("/api/attachments/{}/thumb", att.id)).await.bytes().await.unwrap().to_vec(), thumb);
    let stored: i64 = env.db().query_row("SELECT sealed FROM attachments WHERE id = ?", [att.id], |r| r.get(0)).unwrap();
    assert_eq!(stored, 1);
    // Deleting the line deletes its sealed files too.
    assert_eq!(env.post(&ana, &format!("/api/dms/{}/messages/{}/delete", dm.id, line.id), json!({})).await.status(), StatusCode::OK);
    assert_eq!(env.get(&bia, &format!("/api/attachments/{}", att.id)).await.status(), StatusCode::NOT_FOUND);
}
