//! End-to-end encrypted calls, E1 (v1.1): a real hub on an ephemeral port, LiveKit's server API
//! faked (who is in which room). The channel flag (Admin and up, only while the room is empty,
//! the "outdated" list), join tokens only for apps that encrypt (`?e2ee=1`, else `403` with the
//! text an older app shows), DM calls encrypted when both ends can (an older app answering gets
//! `409`), and the channel list an older app reads.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode as AxStatus;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine;
use reqwest::StatusCode;
use serde_json::{json, Value};
use vox_proto::*;
use voxhub_server::{start_tuned, Options, Server, Tuning};

const PASSWORD: &str = "hunter2";

/// room -> identities in it.
type FakeRef = Arc<Mutex<BTreeMap<String, Vec<String>>>>;

async fn twirp(State(f): State<FakeRef>, Path(method): Path<String>, body: String) -> Response {
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let mut g = f.lock().unwrap();
    let room = v["room"].as_str().unwrap_or_default();
    match method.as_str() {
        "RemoveParticipant" => {
            let who = v["identity"].as_str().unwrap_or_default().to_string();
            if let Some(list) = g.get_mut(room) {
                list.retain(|p| *p != who);
            }
            Json(json!({})).into_response()
        }
        "ListRooms" => Json(json!({ "rooms": g.keys().map(|n| json!({ "name": n })).collect::<Vec<_>>() })).into_response(),
        "ListParticipants" => match g.get(room) {
            Some(list) => Json(json!({ "participants": list.iter().map(|id| json!({ "identity": id, "joinedAt": "1", "permission": { "canPublish": true }, "tracks": [] })).collect::<Vec<_>>() })).into_response(),
            None => (AxStatus::NOT_FOUND, Json(json!({ "code": "not_found", "msg": "not found" }))).into_response(),
        },
        _ => Json(json!({})).into_response(),
    }
}

struct Env {
    _server: Server,
    base: String,
    http: reqwest::Client,
    fake: FakeRef,
    _dir: tempfile::TempDir,
}

#[derive(Clone)]
struct Who {
    token: String,
    user: User,
}

async fn setup() -> Env {
    let fake: FakeRef = Arc::default();
    let app = Router::new().route("/twirp/livekit.RoomService/{method}", post(twirp)).with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "server_name = \"testsrv\"\nserver_password = \"{PASSWORD}\"\nowner = \"Dono\"\n\
             [livekit]\napi_key = \"APItest0000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123456789abcdef\"\nsignal_port = {port}\n"
        ),
    )
    .unwrap();
    let opts = Options { data_dir: dir.path().to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: Some("127.0.0.1:0".into()) };
    // The periodic room check stays out of the way (nobody here declares a voice state).
    let tuning = Tuning { livekit_api: true, voice_check_every: std::time::Duration::from_secs(600), voice_grace: std::time::Duration::from_secs(600), ..Tuning::default() };
    let server = start_tuned(opts, Default::default(), tuning).await.unwrap();
    let base = format!("127.0.0.1:{}", server.addr.port());
    Env { _server: server, base, http: reqwest::Client::new(), fake, _dir: dir }
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

    async fn post(&self, who: &Who, path: &str, body: Value) -> (StatusCode, String) {
        let r = self.http.post(self.url(path)).bearer_auth(&who.token).json(&body).send().await.unwrap();
        (r.status(), r.text().await.unwrap())
    }

    async fn get(&self, who: &Who, path: &str) -> (StatusCode, String) {
        let r = self.http.get(self.url(path)).bearer_auth(&who.token).send().await.unwrap();
        (r.status(), r.text().await.unwrap())
    }

    /// A device with encryption keys for `who` (E0's directory).
    async fn register_device(&self, who: &Who, n: u8) {
        let key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([n; 32]);
        let r = self.http.put(self.url(&format!("/api/e2ee/devices/{}", format!("{n:02x}").repeat(16)))).bearer_auth(&who.token).json(&json!({ "public_key": key, "label": "PC" })).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    async fn staff(&self) -> (Who, Who) {
        let owner = self.login("Dono").await;
        assert_eq!(self.post(&owner, "/api/account/password", json!({ "new": "dono-senha" })).await.0, StatusCode::OK);
        let moder = self.login("modi").await;
        assert_eq!(self.post(&moder, "/api/account/password", json!({ "new": "mod-senha" })).await.0, StatusCode::OK);
        let (st, body) = self.post(&owner, &format!("/api/admin/users/{}/role", moder.user.id), json!({ "role": Role::Moderator })).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        (owner, moder)
    }

    async fn channels(&self, who: &Who) -> Vec<Channel> {
        let (st, body) = self.get(who, "/api/channels").await;
        assert_eq!(st, StatusCode::OK);
        serde_json::from_str(&body).unwrap()
    }

    async fn voice_channel(&self, who: &Who) -> ChannelId {
        self.channels(who).await.into_iter().find(|c| c.kind == ChannelKind::Voice).unwrap().id
    }

    fn occupy(&self, ch: ChannelId, user: UserId) {
        self.fake.lock().unwrap().entry(format!("voice-{ch}")).or_default().push(user.to_string());
    }

    fn empty(&self, ch: ChannelId) {
        self.fake.lock().unwrap().remove(&format!("voice-{ch}"));
    }
}

fn error_of(body: &str) -> String {
    serde_json::from_str::<ApiError>(body).map(|e| e.error).unwrap_or_default()
}

#[tokio::test]
async fn the_channel_flag_is_for_admins_and_changes_only_in_an_empty_room() {
    let env = setup().await;
    let (owner, moder) = env.staff().await;
    let bia = env.login("bia").await;
    env.register_device(&owner, 1).await;
    env.register_device(&moder, 2).await;
    let ch = env.voice_channel(&owner).await;
    let text = env.channels(&owner).await.into_iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    let path = format!("/api/channels/{ch}/e2ee");

    // Moderador and Membro may not; a text channel has no calls; an unknown one does not exist.
    assert_eq!(env.post(&moder, &path, json!({ "on": true })).await.0, StatusCode::FORBIDDEN);
    assert_eq!(env.post(&bia, &path, json!({ "on": true })).await.0, StatusCode::FORBIDDEN);
    assert_eq!(env.post(&owner, &format!("/api/channels/{text}/e2ee"), json!({ "on": true })).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(env.post(&owner, "/api/channels/987654/e2ee", json!({ "on": true })).await.0, StatusCode::NOT_FOUND);

    // Somebody is in the room (LiveKit says so, even unlisted by the hub): not now.
    env.occupy(ch, bia.user.id);
    let (st, body) = env.post(&owner, &path, json!({ "on": true })).await;
    assert_eq!((st, error_of(&body).as_str()), (StatusCode::CONFLICT, ERR_ROOM_OCCUPIED));
    assert!(!env.channels(&owner).await.iter().find(|c| c.id == ch).unwrap().e2ee);

    // Empty: on. Bia has no device with keys (an older app): she is on the "outdated" list.
    env.empty(ch);
    let (st, body) = env.post(&owner, &path, json!({ "on": true })).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let r: ChannelE2eeResponse = serde_json::from_str(&body).unwrap();
    assert!(r.on);
    assert_eq!(r.outdated.iter().map(|u| u.username.as_str()).collect::<Vec<_>>(), vec!["bia"]);
    assert!(env.channels(&bia).await.iter().find(|c| c.id == ch).unwrap().e2ee, "everybody's list says it");
    let (st, body) = env.get(&owner, &path).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(serde_json::from_str::<ChannelE2eeResponse>(&body).unwrap(), r);

    // Bia updates (registers a device): nobody is left out.
    env.register_device(&bia, 3).await;
    let r: ChannelE2eeResponse = serde_json::from_str(&env.get(&owner, &path).await.1).unwrap();
    assert!(r.outdated.is_empty());

    // Setting it to what it is changes nothing, even with somebody in the room; off needs it empty.
    env.occupy(ch, bia.user.id);
    assert_eq!(env.post(&owner, &path, json!({ "on": true })).await.0, StatusCode::OK);
    let (st, body) = env.post(&owner, &path, json!({ "on": false })).await;
    assert_eq!((st, error_of(&body).as_str()), (StatusCode::CONFLICT, ERR_ROOM_OCCUPIED));
    env.empty(ch);
    assert_eq!(env.post(&owner, &path, json!({ "on": false })).await.0, StatusCode::OK);
    assert!(!env.channels(&owner).await.iter().find(|c| c.id == ch).unwrap().e2ee);

    // The moderation log has both changes.
    let (_, log) = env.get(&owner, "/api/admin/audit").await;
    let log: Vec<AuditEntry> = serde_json::from_str(&log).unwrap();
    assert_eq!(log.iter().filter(|e| e.action == AUDIT_CHANNEL_E2EE).count(), 2);
}

#[tokio::test]
async fn join_tokens_of_an_encrypted_room_only_for_apps_that_encrypt() {
    let env = setup().await;
    let (owner, _) = env.staff().await;
    let bia = env.login("bia").await;
    let ch = env.voice_channel(&owner).await;
    let join = |q: &'static str| format!("/api/voice/{ch}/join{q}");

    // Not encrypted: every app gets a plain token (`e2ee` absent, as before).
    let (st, body) = env.post(&bia, &join(""), json!({})).await;
    assert_eq!(st, StatusCode::OK);
    assert!(!body.contains("e2ee"), "{body}");
    let r: VoiceJoinResponse = serde_json::from_str(&env.post(&bia, &join("?e2ee=1"), json!({})).await.1).unwrap();
    assert!(!r.e2ee);

    assert_eq!(env.post(&owner, &format!("/api/channels/{ch}/e2ee"), json!({ "on": true })).await.0, StatusCode::OK);
    // An older app (no `?e2ee=1`): refused with the Portuguese text it shows as is.
    let (st, body) = env.post(&bia, &join(""), json!({})).await;
    assert_eq!((st, error_of(&body).as_str()), (StatusCode::FORBIDDEN, ERR_E2EE_REQUIRED));
    assert_eq!(ERR_E2EE_REQUIRED, "Esta sala usa criptografia de ponta a ponta. Atualize o concord para entrar.");
    assert_eq!(env.post(&bia, &join("?e2ee=0"), json!({})).await.0, StatusCode::FORBIDDEN);
    // 1.1: a token, and the answer says to encrypt.
    let (st, body) = env.post(&bia, &join("?e2ee=1"), json!({})).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let r: VoiceJoinResponse = serde_json::from_str(&body).unwrap();
    assert!(r.e2ee);
    assert_eq!(r.room, format!("voice-{ch}"));
}

/// Audit (E1): the flag changes only in an empty room, but a join token handed out a moment before
/// the change can still be used to enter after it (the hub listed nobody yet): that participant
/// (plain in a now encrypted room) is removed by the room check; one with a fresh token stays.
#[tokio::test]
async fn a_join_that_raced_the_encryption_toggle_is_removed() {
    let env = setup().await;
    let (owner, _) = env.staff().await;
    let bia = env.login("bia").await;
    let ch = env.voice_channel(&owner).await;
    // Bia's app asks for a token while the room is still plain...
    let r: VoiceJoinResponse = serde_json::from_str(&env.post(&bia, &format!("/api/voice/{ch}/join?e2ee=1"), json!({})).await.1).unwrap();
    assert!(!r.e2ee);
    // ...the room is empty, so the Admin turns the encryption on...
    assert_eq!(env.post(&owner, &format!("/api/channels/{ch}/e2ee"), json!({ "on": true })).await.0, StatusCode::OK);
    // ...and Bia enters with her plain token; the owner enters with an encrypted one.
    let r: VoiceJoinResponse = serde_json::from_str(&env.post(&owner, &format!("/api/voice/{ch}/join?e2ee=1"), json!({})).await.1).unwrap();
    assert!(r.e2ee);
    env.occupy(ch, bia.user.id);
    env.occupy(ch, owner.user.id);
    let room = format!("voice-{ch}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    loop {
        let list = env.fake.lock().unwrap().get(&room).cloned().unwrap_or_default();
        if !list.contains(&bia.user.id.to_string()) {
            assert!(list.contains(&owner.user.id.to_string()), "the owner's token is of the room's mode: {list:?}");
            break;
        }
        assert!(std::time::Instant::now() < deadline, "Bia was not removed: {list:?}");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn an_older_app_reads_the_channel_list_unchanged() {
    // What v1.0.2 parses: no `e2ee` field (serde ignores the unknown one when it is there).
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct OldChannel {
        id: ChannelId,
        name: String,
        kind: ChannelKind,
        position: i32,
    }
    let env = setup().await;
    let (owner, _) = env.staff().await;
    let ch = env.voice_channel(&owner).await;
    let (_, before) = env.get(&owner, "/api/channels").await;
    assert!(!before.contains("e2ee"), "byte-identical while off: {before}");
    assert_eq!(env.post(&owner, &format!("/api/channels/{ch}/e2ee"), json!({ "on": true })).await.0, StatusCode::OK);
    let (_, after) = env.get(&owner, "/api/channels").await;
    let old: Vec<OldChannel> = serde_json::from_str(&after).unwrap();
    assert!(old.iter().any(|c| c.id == ch));
}

#[tokio::test]
async fn dm_calls_are_encrypted_when_both_ends_can() {
    let env = setup().await;
    let (ana, bia, caio) = (env.login("ana").await, env.login("bia").await, env.login("caio").await);
    env.register_device(&ana, 1).await;
    env.register_device(&bia, 2).await;
    let open = |a: &Who, b: &Who| {
        let (a, b) = (a.clone(), b.clone());
        let env = &env;
        async move {
            let (st, body) = env.post(&a, &format!("/api/dms/{}", b.user.id), json!({})).await;
            assert_eq!(st, StatusCode::OK);
            serde_json::from_str::<DmConversation>(&body).unwrap().id
        }
    };

    // Ana (1.1) calls Bia (who has a device): encrypted.
    let dm = open(&ana, &bia).await;
    let (st, body) = env.post(&ana, &format!("/api/dms/{dm}/call?e2ee=1"), json!({})).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let r: DmCallResponse = serde_json::from_str(&body).unwrap();
    assert!(r.call.e2ee && r.join.e2ee);
    // Bia answers from an older app: refused with the text it shows; the call keeps ringing.
    let (st, body) = env.post(&bia, &format!("/api/dms/{dm}/call/accept"), json!({})).await;
    assert_eq!((st, error_of(&body).as_str()), (StatusCode::CONFLICT, ERR_E2EE_CALL_REQUIRED));
    // ...and through "Ligar" (the rejoin path) the same.
    assert_eq!(env.post(&bia, &format!("/api/dms/{dm}/call"), json!({})).await.0, StatusCode::CONFLICT);
    // From 1.1: answered, encrypted.
    let (st, body) = env.post(&bia, &format!("/api/dms/{dm}/call/accept?e2ee=1"), json!({})).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let r: DmCallResponse = serde_json::from_str(&body).unwrap();
    assert!(r.call.e2ee && r.join.e2ee && r.call.state == CallState::Active);
    // Ana rejoining without saying she can encrypt (another of her computers, older): refused.
    assert_eq!(env.post(&ana, &format!("/api/dms/{dm}/call"), json!({})).await.0, StatusCode::CONFLICT);
    assert_eq!(env.post(&ana, &format!("/api/dms/{dm}/call/end"), json!({})).await.0, StatusCode::OK);

    // Caio has no device (an older app): Ana's call to him is not encrypted, and he can answer.
    let dm2 = open(&ana, &caio).await;
    let r: DmCallResponse = serde_json::from_str(&env.post(&ana, &format!("/api/dms/{dm2}/call?e2ee=1"), json!({})).await.1).unwrap();
    assert!(!r.call.e2ee && !r.join.e2ee);
    let (st, body) = env.post(&caio, &format!("/api/dms/{dm2}/call/accept"), json!({})).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(!body.contains("e2ee"), "an older app's answer is unchanged: {body}");
    assert_eq!(env.post(&ana, &format!("/api/dms/{dm2}/call/end"), json!({})).await.0, StatusCode::OK);

    // An older caller (no `?e2ee=1`): not encrypted even though Bia could.
    let r: DmCallResponse = serde_json::from_str(&env.post(&ana, &format!("/api/dms/{dm}/call"), json!({})).await.1).unwrap();
    assert!(!r.call.e2ee);
}
