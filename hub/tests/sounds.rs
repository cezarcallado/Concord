//! Server soundboard end to end: upload validation, limits, the permission matrix (and the audit
//! log), the audio download, `PlaySound` (who is in the room, rate limits, refusals) and the
//! `SoundsChanged` broadcasts. Real server on an ephemeral port, no LiveKit.

use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use vox_proto::*;
use voxhub_server::{start, Options, Server};

const PASSWORD: &str = "hunter2";

struct Env {
    server: Server,
    base: String,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
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
    let server = start(Options { data_dir: dir.path().to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: Some("127.0.0.1:0".into()) })
        .await
        .unwrap();
    let base = format!("127.0.0.1:{}", server.addr.port());
    Env { server, base, http: reqwest::Client::new(), _dir: dir }
}

#[derive(Clone)]
struct Who {
    token: String,
    user: User,
}

impl Env {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.base)
    }

    async fn login(&self, name: &str, personal: Option<&str>) -> Who {
        let req = LoginRequest {
            username: name.into(),
            server_password: PASSWORD.into(),
            protocol_version: PROTOCOL_VERSION,
            password: personal.map(Into::into),
            ..Default::default()
        };
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

    async fn upload_raw(&self, who: &Who, name: &str, emoji: &str, body: Vec<u8>) -> reqwest::Response {
        let mut url = url::Url::parse(&self.url("/api/sounds")).unwrap();
        url.query_pairs_mut().append_pair("name", name).append_pair("emoji", emoji);
        self.http
            .post(url)
            .header("content-type", "audio/wav")
            .bearer_auth(&who.token)
            .body(body)
            .send()
            .await
            .unwrap()
    }

    /// An upload by somebody new each time (a refused upload still uses a turn of the rate limit).
    async fn upload_fresh(&self, name: &str, emoji: &str, body: Vec<u8>) -> reqwest::Response {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let who = self.login(&format!("anon{}", N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)), None).await;
        self.upload_raw(&who, name, emoji, body).await
    }

    async fn upload(&self, who: &Who, name: &str) -> Sound {
        let r = self.upload_raw(who, name, "🎺", tone(300)).await;
        let status = r.status();
        let text = r.text().await.unwrap();
        assert_eq!(status, StatusCode::OK, "upload {name}: {text}");
        serde_json::from_str(&text).unwrap()
    }

    async fn sounds(&self, who: &Who) -> Vec<Sound> {
        let r = self.get(who, "/api/sounds").await;
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    }

    async fn ws(&self, who: &Who) -> Ws {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws?token={}", self.base, who.token)).await.unwrap();
        let mut ws = Ws(ws);
        ws.wait_for("ready", |e| matches!(e, ServerEvent::Ready { .. })).await;
        ws
    }

    async fn channels(&self, who: &Who) -> Vec<Channel> {
        self.get(who, "/api/channels").await.json().await.unwrap()
    }

    async fn audit(&self, who: &Who) -> Vec<AuditEntry> {
        self.get(who, "/api/admin/audit").await.json().await.unwrap()
    }

    /// The owner, an admin and a moderator (promoted; their roles need personal passwords), and
    /// plain members for the rest of a test.
    async fn staff(&self) -> (Who, Who, Who) {
        let set_pw = |who: Who, pw: &'static str| async move {
            let r = self.post(&who, "/api/account/password", json!({ "new": pw })).await;
            assert_eq!(r.status(), StatusCode::OK);
            who
        };
        let owner = self.login("Dono", None).await;
        let owner = set_pw(owner, "dono-senha").await;
        let admin = self.login("adm", None).await;
        let admin = set_pw(admin, "adm-senha").await;
        let moder = self.login("modi", None).await;
        let moder = set_pw(moder, "mod-senha").await;
        for (who, role) in [(&admin, "admin"), (&moder, "moderator")] {
            let r = self.post(&owner, &format!("/api/admin/users/{}/role", who.user.id), json!({ "role": role })).await;
            assert_eq!(r.status(), StatusCode::OK);
        }
        (owner, admin, moder)
    }
}

/// A WAV in the stored format: a 440 Hz tone of `ms` milliseconds.
fn tone_with(channels: u16, ms: u32) -> Vec<u8> {
    let frames = (SOUND_RATE as u64 * ms as u64 / 1000) as usize;
    let mut samples = Vec::with_capacity(frames * channels as usize);
    for i in 0..frames {
        let v = ((i as f32 * 440.0 * std::f32::consts::TAU / SOUND_RATE as f32).sin() * 8000.0) as i16;
        samples.extend(std::iter::repeat_n(v, channels as usize));
    }
    canonical_wav(channels, &samples)
}

fn tone(ms: u32) -> Vec<u8> {
    tone_with(1, ms)
}

async fn error_text(r: reqwest::Response) -> (StatusCode, String) {
    let s = r.status();
    (s, r.json::<ApiError>().await.map(|e| e.error).unwrap_or_default())
}

struct Ws(tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>);

impl Ws {
    async fn next_within(&mut self, wait: Duration) -> Option<ServerEvent> {
        loop {
            match tokio::time::timeout(wait, self.0.next()).await.ok()?? {
                Ok(WsMsg::Text(t)) => return Some(serde_json::from_str(&t).unwrap()),
                Ok(WsMsg::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    async fn wait_for(&mut self, what: &str, mut pred: impl FnMut(&ServerEvent) -> bool) -> ServerEvent {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(5) {
            match self.next_within(Duration::from_secs(5)).await {
                Some(ev) if pred(&ev) => return ev,
                Some(_) => {}
                None => break,
            }
        }
        panic!("no {what} event");
    }

    /// Everything that arrives in the next `wait`.
    async fn drain(&mut self, wait: Duration) -> Vec<ServerEvent> {
        let mut out = Vec::new();
        let end = Instant::now() + wait;
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            match self.next_within(left).await {
                Some(ev) => out.push(ev),
                None => break,
            }
        }
        out
    }

    /// Wait until the hub has announced every `(user, channel)` pair (in any order).
    async fn wait_placed(&mut self, mut want: Vec<(UserId, i64)>) {
        let t = Instant::now();
        while !want.is_empty() && t.elapsed() < Duration::from_secs(5) {
            if let Some(ServerEvent::VoiceStateChanged { member }) = self.next_within(Duration::from_secs(5)).await {
                want.retain(|(u, c)| !(member.user.id == *u && member.channel_id == Some(*c)));
            }
        }
        assert!(want.is_empty(), "never placed: {want:?}");
    }

    async fn send(&mut self, ev: &ClientEvent) {
        self.0.send(WsMsg::Text(serde_json::to_string(ev).unwrap().into())).await.unwrap();
    }

    async fn voice(&mut self, channel: Option<i64>, deafened: bool) {
        self.send(&ClientEvent::VoiceState { channel_id: channel, muted: false, deafened, streaming: false, camera: false, audio_only: false }).await;
    }

    async fn play(&mut self, sound_id: SoundId) {
        self.send(&ClientEvent::PlaySound { sound_id }).await;
    }
}

fn voice_ids(chs: &[Channel]) -> (i64, i64) {
    let v: Vec<i64> = chs.iter().filter(|c| c.kind == ChannelKind::Voice).map(|c| c.id).collect();
    (v[0], v[1])
}

fn is_played(id: SoundId) -> impl Fn(&ServerEvent) -> bool {
    move |e| matches!(e, ServerEvent::SoundPlayed { sound_id, .. } if *sound_id == id)
}

// ---------------------------------------------------------------- upload

#[tokio::test]
async fn an_empty_server_has_no_sounds_and_anyone_uploads() {
    let env = setup().await;
    let ana = env.login("ana", None).await;
    assert!(env.sounds(&ana).await.is_empty(), "nothing is seeded");
    // Not logged in.
    assert_eq!(env.http.get(env.url("/api/sounds")).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);

    let mut watcher = env.ws(&env.login("bia", None).await).await;
    let s = env.upload(&ana, "  trombone  ").await;
    assert_eq!((s.name.as_str(), s.emoji.as_str(), s.uploader.id, s.duration_ms), ("trombone", "🎺", ana.user.id, 300));
    assert_eq!(s.size as usize, tone(300).len());
    assert_eq!(s.hash.len(), 16);
    assert_eq!(env.sounds(&ana).await, vec![s.clone()]);
    // Everybody is told, with the whole list.
    let ev = watcher.wait_for("sounds changed", |e| matches!(e, ServerEvent::SoundsChanged { .. })).await;
    assert!(matches!(ev, ServerEvent::SoundsChanged { sounds } if sounds == vec![s.clone()]));

    // No emoji is fine; stereo too.
    let r = env.upload_raw(&ana, "stereo", "", tone_with(2, 1000)).await;
    assert_eq!(r.status(), StatusCode::OK);
    let s2: Sound = r.json().await.unwrap();
    assert_eq!((s2.emoji.as_str(), s2.duration_ms), ("", 1000));
    assert_ne!(s2.hash, s.hash);
    env.server.shutdown().await;
}

#[tokio::test]
async fn uploads_are_validated_again_on_the_hub() {
    let env = setup().await;
    let ana = env.login("ana", None).await;
    let ok = |r: &reqwest::Response| r.status() == StatusCode::OK;

    // The edges of the duration: 5 s fits, 5.01 s does not; 0.1 s fits, a click does not.
    assert!(ok(&env.upload_fresh("limite", "", tone(5000)).await));
    let r = env.upload_fresh("longo", "", tone(5010)).await;
    let (status, msg) = error_text(r).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(msg.contains("too long"), "{msg}");
    let (status, msg) = error_text(env.upload_fresh("longo-mono", "", tone_with(1, 6000)).await).await;
    assert_eq!((status, msg.contains("too long")), (StatusCode::BAD_REQUEST, true));
    // Stereo: 5.01 s is a bit over the duration limit but still under the 1 MiB size limit.
    let (status, msg) = error_text(env.upload_fresh("longo-estereo", "", tone_with(2, 5010)).await).await;
    assert_eq!((status, msg.contains("too long")), (StatusCode::BAD_REQUEST, true));
    // 6 s of stereo is over 1 MiB: refused for its size before anything is parsed.
    assert_eq!(env.upload_fresh("gigante", "", tone_with(2, 6000)).await.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(ok(&env.upload_fresh("curto", "", tone(100)).await));
    assert_eq!(env.upload_fresh("clique", "", tone(50)).await.status(), StatusCode::BAD_REQUEST);

    // Other formats are refused, whatever the Content-Type says.
    let mp3 = [&[0xff, 0xfb, 0x90, 0x00][..], &[0u8; 4000][..]].concat();
    let ogg = [&b"OggS\0\x02"[..], &[0u8; 4000][..]].concat();
    let flac = [&b"fLaC"[..], &[0u8; 4000][..]].concat();
    for (what, body) in [("mp3", mp3), ("ogg", ogg), ("flac", flac), ("empty", vec![]), ("text", b"hello".to_vec())] {
        let (status, _) = error_text(env.upload_fresh("formato", "", body).await).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{what}");
    }
    // A WAV in another sample rate / sample type.
    let mut w = tone(500);
    w[24..28].copy_from_slice(&44_100u32.to_le_bytes());
    let (status, msg) = error_text(env.upload_fresh("taxa", "", w).await).await;
    assert_eq!((status, msg.contains("48 kHz")), (StatusCode::BAD_REQUEST, true));
    let mut w = tone(500);
    w[34] = 8;
    assert_eq!(env.upload_fresh("bits", "", w).await.status(), StatusCode::BAD_REQUEST);
    // Truncated: the header promises more audio than there is.
    let w = tone(500);
    assert_eq!(env.upload_fresh("cortado", "", w[..w.len() - 100].to_vec()).await.status(), StatusCode::BAD_REQUEST);

    // Over 1 MB is refused before anything else (and does not eat the whole body into memory).
    let (status, _) = error_text(env.upload_fresh("enorme", "", vec![0u8; SOUND_MAX_UPLOAD_BYTES + 100_000]).await).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let (status, _) = error_text(env.upload_fresh("enorme", "", vec![0u8; SOUND_MAX_UPLOAD_BYTES + 1]).await).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

    // Names and emoji.
    for name in ["", "   ", &"x".repeat(33), "a\u{7}b"] {
        assert_eq!(env.upload_fresh(name, "", tone(300)).await.status(), StatusCode::BAD_REQUEST, "name {name:?}");
    }
    for emoji in ["two words", &"x".repeat(13), "a\nb"] {
        assert_eq!(env.upload_fresh("emoji", emoji, tone(300)).await.status(), StatusCode::BAD_REQUEST, "emoji {emoji:?}");
    }
    // Only the valid ones were stored.
    let names: Vec<String> = env.sounds(&ana).await.into_iter().map(|s| s.name).collect();
    assert_eq!(names, ["limite", "curto"]);
    env.server.shutdown().await;
}

#[tokio::test]
async fn the_stored_clip_is_a_canonical_re_encoding() {
    let env = setup().await;
    let ana = env.login("ana", None).await;
    // The upload carries a junk chunk after the format: it must not be stored.
    let base = tone(400);
    let mut padded = base[..36].to_vec();
    padded.extend_from_slice(b"JUNK\x08\0\0\0DEADBEEF");
    padded.extend_from_slice(&base[36..]);
    let s: Sound = env.upload_raw(&ana, "junk", "", padded.clone()).await.json().await.unwrap();
    assert_eq!(s.size as usize, base.len());

    let r = env.get(&ana, &format!("/api/sounds/{}/audio?v={}", s.id, s.hash)).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["content-type"], "audio/wav");
    assert_eq!(r.headers()["etag"].to_str().unwrap(), format!("\"{}\"", s.hash));
    assert!(r.headers()["cache-control"].to_str().unwrap().contains("immutable"));
    assert_eq!(r.bytes().await.unwrap().as_ref(), base.as_slice(), "the same samples, in a plain WAV");

    // Cacheable: a repeat with the ETag is 304 and has no body.
    let r = env.http.get(env.url(&format!("/api/sounds/{}/audio", s.id))).bearer_auth(&ana.token).header("if-none-match", format!("\"{}\"", s.hash)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_MODIFIED);
    assert!(r.bytes().await.unwrap().is_empty());
    // Needs a login; unknown ids are 404.
    assert_eq!(env.http.get(env.url(&format!("/api/sounds/{}/audio", s.id))).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(env.get(&ana, "/api/sounds/999/audio").await.status(), StatusCode::NOT_FOUND);
    // Two uploads of the same samples share a hash; different samples do not.
    let again: Sound = env.upload_raw(&ana, "igual", "", base.clone()).await.json().await.unwrap();
    assert_eq!(again.hash, s.hash);
    let other: Sound = env.upload_raw(&ana, "outro", "", tone(500)).await.json().await.unwrap();
    assert_ne!(other.hash, s.hash);
    env.server.shutdown().await;
}

// ---------------------------------------------------------------- limits

#[tokio::test]
async fn the_server_holds_at_most_fifty_sounds() {
    let env = setup().await;
    // Five uploads each (the per-person burst) from ten people.
    for p in 0..10 {
        let who = env.login(&format!("pessoa{p}"), None).await;
        for i in 0..5 {
            env.upload(&who, &format!("som {p}-{i}")).await;
        }
    }
    let late = env.login("atrasado", None).await;
    assert_eq!(env.sounds(&late).await.len(), SOUND_MAX_COUNT);
    let (status, msg) = error_text(env.upload_raw(&late, "um a mais", "", tone(300)).await).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(msg.contains("full"), "{msg}");
    // Deleting one makes room.
    let first = env.sounds(&late).await[0].clone();
    let owner = env.login(&first.uploader.username, None).await;
    assert_eq!(env.post(&owner, &format!("/api/sounds/{}/delete", first.id), json!({})).await.status(), StatusCode::OK);
    assert_eq!(env.upload_raw(&late, "agora cabe", "", tone(300)).await.status(), StatusCode::OK);
    assert_eq!(env.sounds(&late).await.len(), SOUND_MAX_COUNT);
    env.server.shutdown().await;
}

#[tokio::test]
async fn uploading_is_rate_limited_per_person() {
    let env = setup().await;
    let ana = env.login("ana", None).await;
    let bia = env.login("bia", None).await;
    for i in 0..5 {
        env.upload(&ana, &format!("som {i}")).await;
    }
    let (status, msg) = error_text(env.upload_raw(&ana, "sexto", "", tone(300)).await).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(msg.contains("too many"), "{msg}");
    // Somebody else is not affected, and a refused upload stored nothing.
    env.upload(&bia, "da bia").await;
    assert_eq!(env.sounds(&ana).await.len(), 6);
    // A request that fails validation does not use up a turn.
    let cara = env.login("cara", None).await;
    for _ in 0..8 {
        assert_eq!(env.upload_raw(&cara, "", "", tone(300)).await.status(), StatusCode::BAD_REQUEST);
    }
    env.upload(&cara, "ainda posso").await;
    env.server.shutdown().await;
}

// ---------------------------------------------------------------- permissions

#[tokio::test]
async fn rename_and_delete_follow_the_permission_matrix() {
    let env = setup().await;
    let (owner, admin, moder) = env.staff().await;
    let ana = env.login("ana", None).await;
    let bia = env.login("bia", None).await;
    let mut watcher = env.ws(&bia).await;

    let s = env.upload(&ana, "original").await;
    watcher.wait_for("added", |e| matches!(e, ServerEvent::SoundsChanged { .. })).await;
    let rename = |id: SoundId| format!("/api/sounds/{id}/rename");
    let delete = |id: SoundId| format!("/api/sounds/{id}/delete");

    // Another member cannot, anywhere.
    assert_eq!(env.post(&bia, &rename(s.id), json!({"name": "invadido"})).await.status(), StatusCode::FORBIDDEN);
    assert_eq!(env.post(&bia, &delete(s.id), json!({})).await.status(), StatusCode::FORBIDDEN);
    assert_eq!(env.sounds(&ana).await[0].name, "original");
    // Unknown sound.
    assert_eq!(env.post(&ana, &rename(999), json!({"name": "x"})).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(env.post(&ana, &delete(999), json!({})).await.status(), StatusCode::NOT_FOUND);

    // The uploader renames (name, and optionally the emoji) and it is announced.
    let r = env.post(&ana, &rename(s.id), json!({"name": "  novo nome ", "emoji": "🥁"})).await;
    assert_eq!(r.status(), StatusCode::OK);
    let renamed: Sound = r.json().await.unwrap();
    assert_eq!((renamed.name.as_str(), renamed.emoji.as_str(), renamed.hash.as_str()), ("novo nome", "🥁", s.hash.as_str()));
    let ev = watcher.wait_for("renamed", |e| matches!(e, ServerEvent::SoundsChanged { .. })).await;
    assert!(matches!(ev, ServerEvent::SoundsChanged { sounds } if sounds[0].name == "novo nome"));
    // Without an emoji field the emoji stays; an empty one clears it; bad names are 400.
    assert_eq!(env.post(&ana, &rename(s.id), json!({"name": "so o nome"})).await.json::<Sound>().await.unwrap().emoji, "🥁");
    assert_eq!(env.post(&ana, &rename(s.id), json!({"name": "so o nome", "emoji": ""})).await.json::<Sound>().await.unwrap().emoji, "");
    assert_eq!(env.post(&ana, &rename(s.id), json!({"name": ""})).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(env.post(&ana, &rename(s.id), json!({"name": "x", "emoji": "a b"})).await.status(), StatusCode::BAD_REQUEST);
    // Her own changes are not moderation: nothing in the log.
    assert!(env.audit(&admin).await.iter().all(|e| e.action != AUDIT_SOUND_RENAME));

    // Moderador, Admin and Dono change anybody's.
    for (who, label) in [(&moder, "mod"), (&admin, "adm"), (&owner, "dono")] {
        let r = env.post(who, &rename(s.id), json!({"name": label})).await;
        assert_eq!(r.status(), StatusCode::OK, "{label} renames");
    }
    let log = env.audit(&admin).await;
    let renames: Vec<_> = log.iter().filter(|e| e.action == AUDIT_SOUND_RENAME).collect();
    assert_eq!(renames.len(), 3);
    assert!(renames.iter().all(|e| e.target.as_ref().map(|t| t.id) == Some(ana.user.id)));
    assert_eq!(renames[2].detail, "so o nome -> mod");
    assert_eq!(renames[2].actor.as_ref().map(|a| a.id), Some(moder.user.id));

    // Delete: the moderator's delete is audited, with the sound's name.
    assert_eq!(env.post(&moder, &delete(s.id), json!({})).await.status(), StatusCode::OK);
    assert!(env.sounds(&ana).await.is_empty());
    let ev = watcher.wait_for("deleted", |e| matches!(e, ServerEvent::SoundsChanged { sounds } if sounds.is_empty())).await;
    assert!(matches!(ev, ServerEvent::SoundsChanged { .. }));
    assert_eq!(env.get(&ana, &format!("/api/sounds/{}/audio", s.id)).await.status(), StatusCode::NOT_FOUND, "the clip is gone too");
    let log = env.audit(&admin).await;
    let del = log.iter().find(|e| e.action == AUDIT_SOUND_DELETE).expect("delete audited");
    assert_eq!((del.detail.as_str(), del.actor.as_ref().map(|a| a.id), del.target.as_ref().map(|a| a.id)), ("dono", Some(moder.user.id), Some(ana.user.id)));

    // The uploader deletes her own (not audited); a moderator's own sound is hers to delete too.
    let mine = env.upload(&ana, "minha").await;
    assert_eq!(env.post(&ana, &delete(mine.id), json!({})).await.status(), StatusCode::OK);
    let modsound = env.upload(&moder, "do moderador").await;
    assert_eq!(env.post(&bia, &delete(modsound.id), json!({})).await.status(), StatusCode::FORBIDDEN, "a member cannot touch a moderator's sound");
    assert_eq!(env.post(&admin, &delete(modsound.id), json!({})).await.status(), StatusCode::OK);
    assert_eq!(env.audit(&admin).await.iter().filter(|e| e.action == AUDIT_SOUND_DELETE).count(), 2);
    env.server.shutdown().await;
}

#[tokio::test]
async fn a_role_without_its_personal_password_has_no_powers_here_either() {
    let env = setup().await;
    let (owner, _admin, _moder) = env.staff().await;
    let ana = env.login("ana", None).await;
    let s = env.upload(&ana, "da ana").await;
    // Promote bia to moderator, then clear her password through a reset: until she sets a new one
    // her role counts as a plain member.
    let bia = env.login("bia", None).await;
    assert_eq!(env.post(&bia, "/api/account/password", json!({"new": "bia-senha"})).await.status(), StatusCode::OK);
    assert_eq!(env.post(&owner, &format!("/api/admin/users/{}/role", bia.user.id), json!({"role": "moderator"})).await.status(), StatusCode::OK);
    assert_eq!(env.post(&bia, &format!("/api/sounds/{}/rename", s.id), json!({"name": "ok"})).await.status(), StatusCode::OK);
    let r = env.post(&owner, &format!("/api/admin/users/{}/reset-password", bia.user.id), json!({})).await;
    let code = r.json::<ResetPasswordResponse>().await.unwrap().code;
    let bia = env.login("bia", Some(&code)).await;
    assert_eq!(env.post(&bia, &format!("/api/sounds/{}/delete", s.id), json!({})).await.status(), StatusCode::FORBIDDEN);
    env.server.shutdown().await;
}

// ---------------------------------------------------------------- playing

#[tokio::test]
async fn a_sound_reaches_only_the_players_voice_channel() {
    let env = setup().await;
    let ana = env.login("ana", None).await;
    let bia = env.login("bia", None).await;
    let caio = env.login("caio", None).await;
    let dani = env.login("dani", None).await;
    let (room1, room2) = voice_ids(&env.channels(&ana).await);
    let s = env.upload(&ana, "buzina").await;

    let (mut wa, mut wb, mut wc, mut wd) = (env.ws(&ana).await, env.ws(&bia).await, env.ws(&caio).await, env.ws(&dani).await);
    wa.voice(Some(room1), false).await;
    wb.voice(Some(room1), true).await; // deafened: the hub still sends, the client decides
    wc.voice(Some(room2), false).await;
    // Dani is online but not in voice. Wait until the hub has everybody placed.
    wd.wait_placed(vec![(ana.user.id, room1), (bia.user.id, room1), (caio.user.id, room2)]).await;

    wa.play(s.id).await;
    let ev = wb.wait_for("played", is_played(s.id)).await;
    let ServerEvent::SoundPlayed { by, channel_id, .. } = ev else { unreachable!() };
    assert_eq!((by.id, channel_id), (ana.user.id, room1));
    wa.wait_for("played for the player too", is_played(s.id)).await;
    for (name, ws) in [("caio (other room)", &mut wc), ("dani (not in voice)", &mut wd)] {
        let got = ws.drain(Duration::from_millis(500)).await;
        assert!(!got.iter().any(|e| matches!(e, ServerEvent::SoundPlayed { .. })), "{name} must not get it: {got:?}");
    }
    // The other room has its own sounds.
    wc.play(s.id).await;
    wc.wait_for("c hears own", is_played(s.id)).await;
    assert!(!wb.drain(Duration::from_millis(400)).await.iter().any(is_played(s.id)), "room 1 does not hear room 2");
    env.server.shutdown().await;
}

#[tokio::test]
async fn a_player_stops_their_sound_for_the_room() {
    let env = setup().await;
    let ana = env.login("ana", None).await;
    let bia = env.login("bia", None).await;
    let caio = env.login("caio", None).await;
    let (room1, room2) = voice_ids(&env.channels(&ana).await);
    let s = env.upload(&ana, "buzina").await;
    let (mut wa, mut wb, mut wc) = (env.ws(&ana).await, env.ws(&bia).await, env.ws(&caio).await);
    wa.voice(Some(room1), false).await;
    wb.voice(Some(room1), false).await;
    wc.voice(Some(room2), false).await;
    wc.wait_placed(vec![(ana.user.id, room1), (bia.user.id, room1), (caio.user.id, room2)]).await;
    let stopped = |e: &ServerEvent| matches!(e, ServerEvent::SoundStopped { .. });

    // Nothing played yet: a stop goes nowhere (and is not an error).
    wa.send(&ClientEvent::StopSound).await;
    assert!(!wb.drain(Duration::from_millis(300)).await.iter().any(stopped));
    assert!(!wa.drain(Duration::from_millis(100)).await.iter().any(|e| matches!(e, ServerEvent::Error { .. })));

    // This hub relays stops, and says so on every play.
    wa.play(s.id).await;
    let ev = wb.wait_for("played", is_played(s.id)).await;
    assert!(matches!(ev, ServerEvent::SoundPlayed { stoppable: true, .. }), "{ev:?}");
    wa.send(&ClientEvent::StopSound).await;
    let ev = wb.wait_for("stopped", stopped).await;
    let ServerEvent::SoundStopped { by, channel_id } = ev else { unreachable!() };
    assert_eq!((by, channel_id), (ana.user.id, room1));
    wa.wait_for("the player gets it too", stopped).await;
    assert!(!wc.drain(Duration::from_millis(300)).await.iter().any(stopped), "another room is not told");
    // One stop per play: more stops are not relayed.
    wa.send(&ClientEvent::StopSound).await;
    assert!(!wb.drain(Duration::from_millis(300)).await.iter().any(stopped));
    // Somebody else cannot stop ana's sound: a stop only ever names its sender.
    tokio::time::sleep(Duration::from_millis(2100)).await;
    wa.play(s.id).await;
    wb.wait_for("played again", is_played(s.id)).await;
    wb.send(&ClientEvent::StopSound).await;
    assert!(!wa.drain(Duration::from_millis(300)).await.iter().any(stopped), "bia played nothing: her stop goes nowhere");
    env.server.shutdown().await;
}

#[tokio::test]
async fn playing_is_refused_outside_voice_for_unknown_sounds_and_when_server_muted() {
    let env = setup().await;
    let (owner, _admin, moder) = env.staff().await;
    let ana = env.login("ana", None).await;
    let (room1, _) = voice_ids(&env.channels(&ana).await);
    let s = env.upload(&ana, "som").await;
    let mut wa = env.ws(&ana).await;

    let is_err = |text: &'static str| move |e: &ServerEvent| matches!(e, ServerEvent::Error { error } if error == text);
    wa.play(s.id).await;
    wa.wait_for("not in voice", is_err(ERR_SOUND_NOT_IN_VOICE)).await;
    wa.voice(Some(room1), false).await;
    wa.play(404).await;
    wa.wait_for("unknown", is_err(ERR_SOUND_UNKNOWN)).await;

    // A server-muted member cannot talk through the soundboard.
    assert_eq!(env.post(&moder, &format!("/api/mod/users/{}/mute", ana.user.id), json!({"muted": true})).await.status(), StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(2100)).await; // past the per-person gap of the refused-unknown play
    wa.play(s.id).await;
    wa.wait_for("muted", is_err(ERR_SOUND_SERVER_MUTED)).await;
    assert_eq!(env.post(&moder, &format!("/api/mod/users/{}/mute", ana.user.id), json!({"muted": false})).await.status(), StatusCode::OK);
    wa.play(s.id).await;
    wa.wait_for("played after the unmute", is_played(s.id)).await;
    let _ = owner;
    env.server.shutdown().await;
}

#[tokio::test]
async fn play_rate_limits_per_person_and_per_room() {
    let env = setup().await;
    let (room1, room2) = voice_ids(&env.channels(&env.login("xx", None).await).await);
    let s = env.upload(&env.login("ana", None).await, "som").await;
    let people: Vec<Who> = {
        let mut v = Vec::new();
        for n in 0..7 {
            v.push(env.login(&format!("p{n}"), None).await);
        }
        v
    };
    let mut socks = Vec::new();
    for p in &people {
        socks.push(env.ws(p).await);
    }
    for ws in socks.iter_mut().take(6) {
        ws.voice(Some(room1), false).await;
    }
    socks[6].voice(Some(room2), false).await;
    let mut placed: Vec<(UserId, i64)> = people.iter().take(6).map(|p| (p.user.id, room1)).collect();
    placed.push((people[6].user.id, room2));
    socks[0].wait_placed(placed).await;

    let limited = |e: &ServerEvent| matches!(e, ServerEvent::Error { error } if error == ERR_SOUND_RATE_LIMITED);
    // One person: the second play within 2 s is refused, and nobody hears it.
    socks[0].play(s.id).await;
    socks[0].wait_for("first", is_played(s.id)).await;
    socks[0].play(s.id).await;
    socks[0].wait_for("too fast", limited).await;
    let heard = socks[1].drain(Duration::from_millis(300)).await;
    assert_eq!(heard.iter().filter(|e| is_played(s.id)(e)).count(), 1, "only the first reached the room");

    // The room: five plays in 10 s in all; the sixth person is refused even though it is their first.
    for ws in socks.iter_mut().take(5).skip(1) {
        ws.play(s.id).await;
    }
    for ws in socks.iter_mut().take(5).skip(1) {
        ws.wait_for("allowed", is_played(s.id)).await;
    }
    socks[5].play(s.id).await;
    socks[5].wait_for("room is full", limited).await;
    // Another room is counted on its own.
    socks[6].play(s.id).await;
    socks[6].wait_for("room 2 plays", is_played(s.id)).await;

    // After the gap the first person plays again (the room window has not slid yet: still full).
    tokio::time::sleep(Duration::from_millis(2100)).await;
    socks[0].play(s.id).await;
    socks[0].wait_for("still limited by the room", limited).await;
    env.server.shutdown().await;
}

// ---------------------------------------------------------------- compatibility

#[tokio::test]
async fn old_clients_are_not_bothered_by_the_new_events() {
    // An older client's ServerEvent enum fails to parse the soundboard events and skips them.
    #[derive(serde::Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    #[allow(dead_code)]
    enum V041Event {
        Pong { nonce: u64 },
        Error { error: String },
    }
    let played = serde_json::to_string(&ServerEvent::SoundPlayed { sound_id: 3, by: User::new(1, "ana"), channel_id: 2, stoppable: false }).unwrap();
    let changed = serde_json::to_string(&ServerEvent::SoundsChanged { sounds: vec![] }).unwrap();
    let stopped = serde_json::to_string(&ServerEvent::SoundStopped { by: 1, channel_id: 2 }).unwrap();
    assert!(serde_json::from_str::<V041Event>(&played).is_err() && serde_json::from_str::<V041Event>(&changed).is_err());
    assert!(serde_json::from_str::<V041Event>(&stopped).is_err());
    assert_eq!(played, r#"{"type":"sound_played","sound_id":3,"by":{"id":1,"username":"ana"},"channel_id":2}"#);
    assert_eq!(serde_json::to_string(&ClientEvent::PlaySound { sound_id: 3 }).unwrap(), r#"{"type":"play_sound","sound_id":3}"#);
    // v1.1 (no protocol bump): a hub that relays stops says so on each play; a v1.0 client
    // ignores the field, and a v1.1 client reading a v1.0 hub's play sees `false` (it then stops
    // the sound only for itself and never sends `StopSound`, which that hub would not know).
    let new_play = serde_json::to_string(&ServerEvent::SoundPlayed { sound_id: 3, by: User::new(1, "ana"), channel_id: 2, stoppable: true }).unwrap();
    assert_eq!(new_play, r#"{"type":"sound_played","sound_id":3,"by":{"id":1,"username":"ana"},"channel_id":2,"stoppable":true}"#);
    let old_play: ServerEvent = serde_json::from_str(r#"{"type":"sound_played","sound_id":3,"by":{"id":1,"username":"ana"},"channel_id":2}"#).unwrap();
    assert!(matches!(old_play, ServerEvent::SoundPlayed { stoppable: false, .. }));
    assert_eq!(stopped, r#"{"type":"sound_stopped","by":1,"channel_id":2}"#);
    assert_eq!(serde_json::to_string(&ClientEvent::StopSound).unwrap(), r#"{"type":"stop_sound"}"#);
    // A hub too old for it answers a PlaySound with the generic parse error, and keeps the socket.
}
