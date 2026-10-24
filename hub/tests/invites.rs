//! Invites end to end (v0.8): who may create / list / revoke them, a newcomer joining with only
//! the token, the remembered device that keeps them in afterwards, uses, expiry, revocation, the
//! login brake, and that nothing changes for everybody else (the server password still rules
//! people who did not come through an invite). Real server on an ephemeral port, no LiveKit.

use std::time::Duration;

use reqwest::StatusCode;
use serde_json::{json, Value};
use vox_proto::*;
use voxhub_server::{start, Options, Server};

const PASSWORD: &str = "senha-do-servidor";
const OWNER_PW: &str = "dono-senha-1";

struct Env {
    server: Server,
    base: String,
    http: reqwest::Client,
    dir: tempfile::TempDir,
}

async fn setup() -> Env {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "server_name = \"Casa\"\nserver_password = \"{PASSWORD}\"\nowner = \"dono\"\npublic_host = \"203.0.113.50\"\n\
             [livekit]\napi_key = \"APItest0000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123456789abcdef\"\nsignal_port = 1\n"
        ),
    )
    .unwrap();
    let server = start(Options { data_dir: dir.path().to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: Some("127.0.0.1:0".into()) })
        .await
        .unwrap();
    let base = format!("http://127.0.0.1:{}", server.addr.port());
    Env { server, base, http: reqwest::Client::new(), dir }
}

impl Env {
    async fn login(&self, req: &LoginRequest) -> reqwest::Response {
        let mut req = req.clone();
        req.protocol_version = PROTOCOL_VERSION;
        self.http.post(format!("{}/api/login", self.base)).json(&req).send().await.unwrap()
    }

    async fn login_ok(&self, req: &LoginRequest) -> LoginResponse {
        let r = self.login(req).await;
        let (status, text) = (r.status(), r.text().await.unwrap());
        assert_eq!(status, StatusCode::OK, "{}: {text}", req.username);
        serde_json::from_str(&text).unwrap()
    }

    /// The error text of a refused login.
    async fn login_err(&self, req: &LoginRequest) -> (StatusCode, String) {
        let r = self.login(req).await;
        let status = r.status();
        let e: ApiError = r.json().await.unwrap();
        (status, e.error)
    }

    async fn post(&self, token: &str, path: &str, body: Value) -> reqwest::Response {
        self.http.post(format!("{}{path}", self.base)).bearer_auth(token).json(&body).send().await.unwrap()
    }

    async fn get(&self, token: &str, path: &str) -> reqwest::Response {
        self.http.get(format!("{}{path}", self.base)).bearer_auth(token).send().await.unwrap()
    }

    /// The owner, logged in with powers (first personal password set from this PC).
    async fn owner(&self) -> String {
        let l = self.login_ok(&LoginRequest { username: "dono".into(), server_password: PASSWORD.into(), ..Default::default() }).await;
        let r = self.post(&l.token, "/api/account/password", json!({"new": OWNER_PW})).await;
        assert_eq!(r.status(), StatusCode::OK);
        self.login_ok(&LoginRequest { username: "dono".into(), server_password: PASSWORD.into(), password: Some(OWNER_PW.into()), ..Default::default() }).await.token
    }

    async fn create(&self, token: &str, body: Value) -> InviteCreated {
        let r = self.post(token, "/api/invites", body).await;
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    }

    async fn list(&self, token: &str) -> Vec<Invite> {
        self.get(token, "/api/invites").await.json().await.unwrap()
    }
}

fn with_invite(name: &str, token: &str) -> LoginRequest {
    LoginRequest { username: name.into(), invite: Some(token.into()), remember_device: true, ..Default::default() }
}

#[tokio::test]
async fn a_newcomer_joins_with_only_the_invite_and_stays_in() {
    let env = setup().await;
    let dono = env.owner().await;
    let port = env.server.addr.port();

    // Only Admin / Dono manage invites.
    let tati = env.login_ok(&LoginRequest { username: "Tati".into(), server_password: PASSWORD.into(), ..Default::default() }).await;
    assert_eq!(env.post(&tati.token, "/api/invites", json!({})).await.status(), StatusCode::FORBIDDEN);
    assert_eq!(env.get(&tati.token, "/api/invites").await.status(), StatusCode::FORBIDDEN);

    let created = env.create(&dono, json!({"expires_secs": 3600, "max_uses": 2})).await;
    assert_eq!(created.token.len(), 32);
    assert_eq!((created.invite.uses, created.invite.max_uses, created.invite.created_by_name.as_str()), (0, Some(2), "dono"));
    assert!(created.invite.expires_at_ms.is_some());
    // What the app builds the code from: the port it used and the configured public address.
    assert_eq!(created.hints.hub_port, port);
    assert_eq!(created.hints.public_host, "203.0.113.50");

    // "redeem" only checks: the server's name, and no use is taken.
    let r = env.http.post(format!("{}/api/invites/redeem", env.base)).json(&InviteRedeemRequest { token: created.token.clone() }).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let redeem: InviteRedeemResponse = r.json().await.unwrap();
    assert_eq!(redeem.server_name, "Casa");
    assert_eq!(env.list(&dono).await[0].uses, 0);

    // The code the app hands out round-trips to the same token.
    let code = InviteCode { token: invite_token_from_hex(&created.token).unwrap(), hub_port: port, addrs: vec![InviteAddr::Lan("127.0.0.1".parse().unwrap(), 8)], tls_pin: None };
    let decoded = InviteCode::decode(&code.encode()).unwrap();
    assert_eq!(decoded.candidate_urls(&[("127.0.0.1".parse().unwrap(), 8)]), [format!("http://127.0.0.1:{port}")]);

    // Joao joins with only the invite (no server password): he gets a device token.
    let joao = env.login_ok(&with_invite("Joao", &decoded.token_hex())).await;
    let device = joao.device_token.clone().expect("a device token for the newcomer");
    assert_eq!(joao.server_name, "Casa");
    assert_eq!(env.list(&dono).await[0].uses, 1);
    // He chats like anybody.
    let chans: Vec<Channel> = env.get(&joao.token, "/api/channels").await.json().await.unwrap();
    assert!(!chans.is_empty());

    // Next time: this computer's device token, no password, no invite. No use taken.
    let again = env.login_ok(&LoginRequest { username: "Joao".into(), device_token: Some(device.clone()), ..Default::default() }).await;
    assert_eq!(again.user.id, joao.user.id);
    assert_eq!(again.device_token, None);
    assert_eq!(env.list(&dono).await[0].uses, 1);
    // The token is Joao's only.
    let (s, e) = env.login_err(&LoginRequest { username: "Tati".into(), device_token: Some(device.clone()), ..Default::default() }).await;
    assert_eq!((s, e.as_str()), (StatusCode::UNAUTHORIZED, "wrong server password"));

    // The second use, then the invite is used up.
    env.login_ok(&with_invite("Ana", &created.token)).await;
    let (s, e) = env.login_err(&with_invite("Carla", &created.token)).await;
    assert_eq!((s, e.as_str()), (StatusCode::UNAUTHORIZED, ERR_INVITE_USED_UP));
    // Joao is still in.
    env.login_ok(&LoginRequest { username: "Joao".into(), device_token: Some(device), ..Default::default() }).await;

    // The moderation log tells who came in through which invite.
    let audit: Vec<AuditEntry> = env.get(&dono, "/api/admin/audit").await.json().await.unwrap();
    let joins: Vec<&str> = audit.iter().filter(|a| a.action == AUDIT_INVITE_JOIN).filter_map(|a| a.actor.as_ref().map(|u| u.username.as_str())).collect();
    assert_eq!(joins, ["Ana", "Joao"]);
    assert!(audit.iter().any(|a| a.action == AUDIT_INVITE_CREATE));
    env.server.shutdown().await;
}

#[tokio::test]
async fn revoked_expired_and_unknown_invites_are_refused() {
    let env = setup().await;
    let dono = env.owner().await;

    let unlimited = env.create(&dono, json!({})).await;
    assert_eq!((unlimited.invite.expires_at_ms, unlimited.invite.max_uses), (None, None));
    env.login_ok(&with_invite("Bia", &unlimited.token)).await;

    // Revoke: the next attempt is refused, the person who already joined stays.
    assert_eq!(env.post(&dono, &format!("/api/invites/{}/revoke", unlimited.invite.id), json!({})).await.status(), StatusCode::OK);
    assert_eq!(env.post(&dono, "/api/invites/999/revoke", json!({})).await.status(), StatusCode::NOT_FOUND);
    let (s, e) = env.login_err(&with_invite("Caio", &unlimited.token)).await;
    assert_eq!((s, e.as_str()), (StatusCode::UNAUTHORIZED, ERR_INVITE_REVOKED));
    let list = env.list(&dono).await;
    assert!(list[0].revoked && list[0].state(i64::MAX / 2) == InviteState::Revoked);
    let r = env.http.post(format!("{}/api/invites/redeem", env.base)).json(&InviteRedeemRequest { token: unlimited.token.clone() }).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(r.json::<ApiError>().await.unwrap().error, ERR_INVITE_REVOKED);

    // A good login in between (the brake counts failures in a row per address).
    env.login_ok(&LoginRequest { username: "Bia".into(), server_password: PASSWORD.into(), ..Default::default() }).await;

    // Expired: the hub compares with the clock (moved back here through the database).
    let soon = env.create(&dono, json!({"expires_secs": 60})).await;
    let db = voxhub_server::db::Db::open(&env.dir.path().join("voxhub.db")).unwrap();
    let id = soon.invite.id;
    db.call_now(move |c| c.execute("UPDATE invites SET expires_at_ms = 1 WHERE id = ?", [id])).unwrap();
    let (s, e) = env.login_err(&with_invite("Caio", &soon.token)).await;
    assert_eq!((s, e.as_str()), (StatusCode::UNAUTHORIZED, ERR_INVITE_EXPIRED));

    env.login_ok(&LoginRequest { username: "Bia".into(), server_password: PASSWORD.into(), ..Default::default() }).await;
    // Nonsense and unknown tokens.
    let (_, e) = env.login_err(&with_invite("Caio", "nao-e-um-convite")).await;
    assert_eq!(e, ERR_INVITE_INVALID);
    env.login_ok(&LoginRequest { username: "Bia".into(), server_password: PASSWORD.into(), ..Default::default() }).await;
    let (_, e) = env.login_err(&with_invite("Caio", &"0".repeat(32))).await;
    assert_eq!(e, ERR_INVITE_INVALID);

    // Bad requests.
    assert_eq!(env.post(&dono, "/api/invites", json!({"expires_secs": 5})).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(env.post(&dono, "/api/invites", json!({"max_uses": 0})).await.status(), StatusCode::BAD_REQUEST);
    env.server.shutdown().await;
}

#[tokio::test]
async fn wrong_invites_are_slowed_down_like_wrong_passwords() {
    let env = setup().await;
    let mut statuses = Vec::new();
    for i in 0..5 {
        let r = env.login(&with_invite("Caio", &format!("{i:032x}"))).await;
        statuses.push(r.status());
    }
    assert_eq!(&statuses[..3], [StatusCode::UNAUTHORIZED; 3], "three free failures");
    assert_eq!(statuses[3], StatusCode::TOO_MANY_REQUESTS, "{statuses:?}");
    // The redeem check shares the brake.
    let r = env.http.post(format!("{}/api/invites/redeem", env.base)).json(&InviteRedeemRequest { token: "0".repeat(32) }).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    env.server.shutdown().await;
}

#[tokio::test]
async fn the_server_password_still_rules_everybody_else() {
    let env = setup().await;
    // Bia logs in with the password and protects her name: her device token skips the personal
    // password, but never the server password (she did not come through an invite).
    let bia = env.login_ok(&LoginRequest { username: "Bia".into(), server_password: PASSWORD.into(), ..Default::default() }).await;
    let r = env.post(&bia.token, "/api/account/password", json!({"new": "bia-senha-1", "remember_device": true})).await;
    let set: SetPasswordResponse = r.json().await.unwrap();
    let device = set.device_token.expect("device token");
    let ok = env.login_ok(&LoginRequest { username: "Bia".into(), server_password: PASSWORD.into(), device_token: Some(device.clone()), ..Default::default() }).await;
    assert_eq!(ok.user.username, "Bia");
    let (s, e) = env.login_err(&LoginRequest { username: "Bia".into(), device_token: Some(device), ..Default::default() }).await;
    assert_eq!((s, e.as_str()), (StatusCode::UNAUTHORIZED, "wrong server password"));
    // An invite does not skip a name's personal password either.
    let dono = env.owner().await;
    let inv = env.create(&dono, json!({})).await;
    let (s, e) = env.login_err(&with_invite("Bia", &inv.token)).await;
    assert_eq!((s, e.as_str()), (StatusCode::UNAUTHORIZED, ERR_PERSONAL_PASSWORD_REQUIRED));
    assert_eq!(env.list(&dono).await[0].uses, 0, "a refused login takes no use");
    // An old app's login (exactly the v0.7.0 JSON) works as before.
    let r = env
        .http
        .post(format!("{}/api/login", env.base))
        .json(&json!({"username": "Velho", "server_password": PASSWORD, "protocol_version": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    env.server.shutdown().await;
}

/// v1.0.1 (audit `voxhub:hub:invite-login-marks-existing-account-server-password-bypass`): an
/// invite only creates new accounts. An invite login with the name of an existing account that
/// has no personal password is refused, takes no use and leaves the account as it was, so no
/// device token comes out of it that would outlive the invite or a new server password. A name
/// with its own proof (personal password, remembered device) gets in, but does not become an
/// invite member either.
#[tokio::test]
async fn an_invite_does_not_take_over_an_existing_name() {
    let env = setup().await;
    let dono = env.owner().await;
    let rafa = env.login_ok(&LoginRequest { username: "Rafa".into(), server_password: PASSWORD.into(), ..Default::default() }).await;
    let inv = env.create(&dono, json!({})).await;

    // The unprotected name, any case: refused, no use taken, no device token.
    for name in ["Rafa", "rafa"] {
        let (s, e) = env.login_err(&with_invite(name, &inv.token)).await;
        assert_eq!((s, e.as_str()), (StatusCode::UNAUTHORIZED, ERR_INVITE_NAME_TAKEN), "{name}");
    }
    assert_eq!(env.list(&dono).await[0].uses, 0, "a refused login takes no use");
    // Rafa is still exactly a server-password member.
    let again = env.login_ok(&LoginRequest { username: "Rafa".into(), server_password: PASSWORD.into(), ..Default::default() }).await;
    assert_eq!(again.user.id, rafa.user.id);

    // A protected name with its own personal password gets in through the invite (it proved who
    // it is), but takes no use and is not made an invite member: its remembered devices still
    // need the server password.
    let bia = env.login_ok(&LoginRequest { username: "Bia".into(), server_password: PASSWORD.into(), ..Default::default() }).await;
    let r = env.post(&bia.token, "/api/account/password", json!({"new": "bia-senha-1", "remember_device": true})).await;
    let old_device = r.json::<SetPasswordResponse>().await.unwrap().device_token.expect("device token");
    let mut req = with_invite("Bia", &inv.token);
    req.password = Some("bia-senha-1".into());
    let via_invite = env.login_ok(&req).await;
    assert_eq!(via_invite.user.id, bia.user.id);
    assert_eq!(env.list(&dono).await[0].uses, 0);
    for device in [Some(old_device), via_invite.device_token] {
        let (s, e) = env.login_err(&LoginRequest { username: "Bia".into(), device_token: device, ..Default::default() }).await;
        assert_eq!((s, e.as_str()), (StatusCode::UNAUTHORIZED, "wrong server password"));
    }
    // A new name still joins with only the invite.
    let novo = env.login_ok(&with_invite("Novo", &inv.token)).await;
    assert!(novo.device_token.is_some());
    assert_eq!(env.list(&dono).await[0].uses, 1);

    // A wrong personal password with the invite is still just a wrong personal password.
    req.password = Some("errada".into());
    let (_, e) = env.login_err(&req).await;
    assert_eq!(e, ERR_WRONG_PERSONAL_PASSWORD);
    assert_eq!(env.list(&dono).await[0].uses, 1);
    env.server.shutdown().await;
}
