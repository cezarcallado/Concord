//! "Quem hospeda vira dono": the first login from the server PC (loopback) becomes the owner, the
//! one-time claim code, `config.toml`'s `owner` as the manual override, handing the server over,
//! and `--reset-owner`. Real server on an ephemeral port, no LiveKit.
//!
//! The tests are all on 127.0.0.1, so a login that must NOT count as coming from the server PC is
//! made to look like it went through a proxy (`X-Forwarded-For`, or a `Host` that is not a
//! loopback name): those headers veto the automatic claim. A login from a non-loopback address of
//! this machine (the LAN address) is tested for real when the machine has one.

use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMsg;
use vox_proto::*;
use voxhub_server::{start, Options, Server};

fn write_config(dir: &std::path::Path, owner: Option<&str>) {
    let owner_line = owner.map(|o| format!("owner = \"{o}\"\n")).unwrap_or_default();
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "server_name = \"testsrv\"\nserver_password = \"\"\n{owner_line}\
             [livekit]\napi_key = \"APItest0000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123456789abcdef\"\nsignal_port = 1\n"
        ),
    )
    .unwrap();
}

async fn start_in(dir: &std::path::Path, bind: &str) -> Server {
    start(Options { data_dir: dir.to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: Some(bind.into()) }).await.unwrap()
}

struct Env {
    server: Option<Server>,
    http: reqwest::Client,
    /// The host the requests go to.
    host: String,
    dir: tempfile::TempDir,
}

/// No owner configured: "quem hospeda vira dono".
async fn setup() -> Env {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path(), None);
    let server = start_in(dir.path(), "127.0.0.1:0").await;
    let host = format!("127.0.0.1:{}", server.addr.port());
    Env { server: Some(server), http: reqwest::Client::new(), host, dir }
}

#[derive(Clone)]
struct Who {
    token: String,
    user: User,
}

/// What a login through a proxy on the server PC looks like: loopback peer, but not the host's own app.
const PROXIED: (&str, &str) = ("x-forwarded-for", "203.0.113.9");

impl Env {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.host)
    }

    fn server(&self) -> &Server {
        self.server.as_ref().expect("the server is running")
    }

    fn claim_code(&self) -> Option<String> {
        self.server().claim_code()
    }

    /// Stop the server (its files stay), e.g. to edit them or run a command-line tool.
    async fn stop(&mut self) {
        if let Some(s) = self.server.take() {
            s.shutdown().await;
        }
    }

    async fn start(&mut self) {
        let dir = self.dir.path().to_path_buf();
        let server = start_in(&dir, "127.0.0.1:0").await;
        self.host = format!("127.0.0.1:{}", server.addr.port());
        self.server = Some(server);
    }

    async fn restart(&mut self) {
        self.stop().await;
        self.start().await;
    }

    async fn login_with(&self, name: &str, password: Option<&str>, header: Option<(&str, &str)>) -> reqwest::Response {
        let req = LoginRequest {
            username: name.into(),
            server_password: String::new(),
            protocol_version: PROTOCOL_VERSION,
            password: password.map(Into::into),
            ..Default::default()
        };
        let mut r = self.http.post(self.url("/api/login")).json(&req);
        if let Some((k, v)) = header {
            r = r.header(k, v);
        }
        r.send().await.unwrap()
    }

    async fn login_ok(&self, name: &str, password: Option<&str>, header: Option<(&str, &str)>) -> LoginResponse {
        let r = self.login_with(name, password, header).await;
        let (status, text) = (r.status(), r.text().await.unwrap());
        assert_eq!(status, StatusCode::OK, "login {name}: {text}");
        serde_json::from_str(&text).unwrap()
    }

    /// A login from the server PC itself.
    async fn host_login(&self, name: &str) -> Who {
        let l = self.login_ok(name, None, None).await;
        Who { token: l.token, user: l.user }
    }

    /// A login that does not count as the server PC.
    async fn remote_login(&self, name: &str) -> Who {
        let l = self.login_ok(name, None, Some(PROXIED)).await;
        Who { token: l.token, user: l.user }
    }

    async fn post(&self, who: &Who, path: &str, body: Value) -> reqwest::Response {
        self.http.post(self.url(path)).bearer_auth(&who.token).json(&body).send().await.unwrap()
    }

    async fn get(&self, who: &Who, path: &str) -> reqwest::Response {
        self.http.get(self.url(path)).bearer_auth(&who.token).send().await.unwrap()
    }

    async fn set_password(&self, who: &Who, pw: &str) {
        let r = self.post(who, "/api/account/password", json!({"new": pw})).await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    async fn claim(&self, who: &Who, code: &str) -> reqwest::Response {
        self.post(who, "/api/owner/claim", json!({"code": code})).await
    }

    /// A connection that does not count as the server PC (through a proxy, like `remote_login`).
    async fn ws(&self, who: &Who) -> Ws {
        self.connect(who, true).await
    }

    /// The host's own app connecting.
    async fn ws_host(&self, who: &Who) -> Ws {
        self.connect(who, false).await
    }

    async fn connect(&self, who: &Who, proxied: bool) -> Ws {
        let mut req = format!("ws://{}/ws?token={}", self.host, who.token).into_client_request().unwrap();
        if proxied {
            req.headers_mut().insert(PROXIED.0, PROXIED.1.parse().unwrap());
        }
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        Ws(ws)
    }

    async fn ready(&self, who: &Who) -> (User, Account) {
        self.ws(who).await.ready().await
    }

    async fn ready_host(&self, who: &Who) -> (User, Account) {
        self.ws_host(who).await.ready().await
    }

    async fn audit(&self, who: &Who) -> Vec<AuditEntry> {
        let r = self.get(who, "/api/admin/audit").await;
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    }
}

async fn error_of(r: reqwest::Response) -> (StatusCode, String) {
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

    async fn ready(&mut self) -> (User, Account) {
        match self.next_within(Duration::from_secs(5)).await {
            Some(ServerEvent::Ready { user, account, .. }) => (user, account.expect("a hub that knows roles sends the account")),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    /// Skip events until one matches (`secs` at most).
    async fn wait_for(&mut self, what: &str, secs: u64, mut pred: impl FnMut(&ServerEvent) -> bool) -> ServerEvent {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(secs) {
            match self.next_within(Duration::from_secs(secs)).await {
                Some(ev) if pred(&ev) => return ev,
                Some(_) => {}
                None => break,
            }
        }
        panic!("no {what} event");
    }
}

// ---------------------------------------------------------------- the server PC claims it

#[tokio::test]
async fn the_first_login_from_the_server_pc_becomes_the_owner() {
    let mut env = setup().await;
    let code = env.claim_code().expect("a code is printed while there is no owner");

    // A login that is not the server PC (here: through a proxy) does not claim, and the hub says so.
    let visitor = env.remote_login("Visita").await;
    assert_eq!(visitor.user.role, Role::Member);
    let (_, account) = env.ready(&visitor).await;
    assert!(account.no_owner && !account.owner_pinned, "{account:?}");
    // A loopback peer that says it is a remote address is no host either.
    let named = env.login_ok("Nomeada", None, Some(("host", "meu-servidor.example.com"))).await;
    assert_eq!(named.user.role, Role::Member);
    assert_eq!(env.claim_code().as_deref(), Some(code.as_str()), "nothing claimed yet");

    // The app on the server PC: first loopback login.
    let mut visitor_ws = env.ws(&visitor).await;
    visitor_ws.ready().await;
    let anfitriao = env.host_login("anfitriao").await;
    assert_eq!(anfitriao.user.role, Role::Owner, "the host becomes the owner on the spot");
    let (me, account) = env.ready_host(&anfitriao).await;
    assert_eq!(me.role, Role::Owner);
    assert_eq!((account.no_owner, account.must_set_password, account.has_password), (false, true, false), "a personal password is asked for right away");
    assert_eq!(env.claim_code(), None, "the code dies with the claim");
    // Everybody who is online learns who the owner is.
    visitor_ws.wait_for("UserUpdated for the new owner", 5, |e| matches!(e, ServerEvent::UserUpdated { user } if user.username == "anfitriao" && user.role == Role::Owner)).await;

    // Until the password is set, the owner's powers are off; afterwards they work.
    assert_eq!(env.get(&anfitriao, "/api/admin/users").await.status(), StatusCode::FORBIDDEN);
    env.set_password(&anfitriao, "dono-senha").await;
    assert_eq!(env.get(&anfitriao, "/api/admin/users").await.status(), StatusCode::OK);

    // Only one owner: the next loopback login is a member, and so is a returning visitor.
    let bia = env.host_login("Bia").await;
    assert_eq!(bia.user.role, Role::Member);
    let (_, account) = env.ready_host(&bia).await;
    assert!(!account.no_owner);

    // The claim is audited, and it survives a restart (the same person is the owner).
    let log = env.audit(&anfitriao).await;
    let entry = log.iter().find(|e| e.action == AUDIT_CLAIM_OWNER).expect("claim audited");
    assert_eq!((entry.actor.as_ref().map(|a| a.id), entry.detail.as_str()), (Some(anfitriao.user.id), "pc-do-servidor"));
    env.restart().await;
    assert_eq!(env.claim_code(), None, "an owner exists: no code");
    let l = env.login_ok("ANFITRIAO", Some("dono-senha"), None).await;
    assert_eq!(l.user.role, Role::Owner);
    assert_eq!(env.login_ok("Bia", None, None).await.user.role, Role::Member);
    env.stop().await;
}

#[tokio::test]
async fn a_session_reconnecting_from_the_server_pc_claims_it_too() {
    // The update restarts the hub under the host's open app: its session is still valid, and it
    // reconnects without logging in again.
    let mut env = setup().await;
    let host_session = env.remote_login("anfitriao").await; // logged in before the hub knew about owners
    assert_eq!(host_session.user.role, Role::Member);
    env.restart().await;
    let (me, account) = env.ready_host(&host_session).await;
    assert_eq!(me.role, Role::Owner, "the connection comes from 127.0.0.1");
    assert!(account.must_set_password && !account.no_owner);
    // A proxied connection does not.
    let mut env2 = setup().await;
    let remote = env2.remote_login("Visita").await;
    let (_, account) = env2.ready(&remote).await;
    assert!(account.no_owner);
    env2.restart().await;
    env2.stop().await;
    env.stop().await;
}

#[tokio::test]
async fn a_login_from_a_non_loopback_address_does_not_claim() {
    // The real thing: connect to this machine's LAN address, so the TCP peer is not loopback.
    // It must listen on every interface, so the firewall asks about every new test build: only
    // with VOX_TEST_LAN=1 (the decision itself is unit-tested in hub/src/owner.rs).
    if std::env::var_os("VOX_TEST_LAN").is_none() {
        eprintln!("skipped: listens on the LAN (set VOX_TEST_LAN=1 to run it)");
        return;
    }
    let Some(lan) = lan_address() else {
        eprintln!("skipped: no non-loopback address on this machine (the decision is unit-tested in hub/src/owner.rs)");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path(), None);
    let server = start_in(dir.path(), "0.0.0.0:0").await;
    let host = format!("{lan}:{}", server.addr.port());
    let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(3)).build().unwrap();
    let req = LoginRequest { username: "anfitriao".into(), server_password: String::new(), protocol_version: PROTOCOL_VERSION, ..Default::default() };
    let r = match http.post(format!("http://{host}/api/login")).json(&req).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipped: could not reach {host} ({e})");
            server.shutdown().await;
            return;
        }
    };
    let l: LoginResponse = r.json().await.unwrap();
    assert_eq!(l.user.role, Role::Member, "logging in with the LAN address of the server PC does not claim");
    assert!(server.claim_code().is_some());
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{host}/ws?token={}", l.token)).await.unwrap();
    let mut ws = Ws(ws);
    let (me, account) = ws.ready().await;
    assert_eq!(me.role, Role::Member);
    assert!(account.no_owner, "a connection over the LAN does not claim either");
    // The same machine through 127.0.0.1 does.
    let req2 = LoginRequest { username: "anfitriao".into(), ..req };
    let l2: LoginResponse = http.post(format!("http://127.0.0.1:{}/api/login", server.addr.port())).json(&req2).send().await.unwrap().json().await.unwrap();
    assert_eq!(l2.user.role, Role::Owner);
    server.shutdown().await;
}

/// This machine's address on its default route (a UDP "connect" sends nothing).
fn lan_address() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

// ---------------------------------------------------------------- the claim code

#[tokio::test]
async fn the_claim_code_works_once_and_wrong_codes_are_slowed_down() {
    let mut env = setup().await;
    let code = env.claim_code().unwrap();
    assert_eq!(code.len(), 9);
    let ana = env.remote_login("Ana").await;
    let bia = env.remote_login("Bia").await;
    let mut bia_ws = env.ws(&bia).await;
    bia_ws.ready().await;

    // Wrong codes: refused; after three in a row the whole server must wait (also for the right code).
    for _ in 0..3 {
        let (status, msg) = error_of(env.claim(&ana, "AAAA-AAAA").await).await;
        assert_eq!((status, msg.as_str()), (StatusCode::FORBIDDEN, "wrong claim code"));
    }
    let (status, msg) = error_of(env.claim(&ana, &code).await).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{msg}");
    assert!(msg.contains(ERR_TOO_MANY_ATTEMPTS));
    assert_eq!(env.get(&ana, "/api/channels").await.status(), StatusCode::OK, "the brake is only on claiming");
    // Another person's attempts count against the same brake (it is per server, not per address).
    assert_eq!(env.claim(&bia, &code).await.status(), StatusCode::TOO_MANY_REQUESTS);
    tokio::time::sleep(Duration::from_millis(1200)).await;

    // The right code, typed the way people type it.
    let typed = code.to_lowercase().replace('-', " ");
    let r = env.claim(&ana, &typed).await;
    assert_eq!(r.status(), StatusCode::OK);
    let resp: ClaimOwnerResponse = r.json().await.unwrap();
    assert_eq!((resp.user.role, resp.user.id), (Role::Owner, ana.user.id));
    assert!(resp.account.must_set_password && !resp.account.no_owner, "the owner must now set a personal password: {:?}", resp.account);
    assert_eq!(env.claim_code(), None);
    bia_ws.wait_for("UserUpdated for the new owner", 5, |e| matches!(e, ServerEvent::UserUpdated { user } if user.id == ana.user.id && user.role == Role::Owner)).await;

    // Once: the same code again, from anyone, is a conflict (including the owner), not a second owner.
    assert_eq!(env.claim(&bia, &code).await.status(), StatusCode::CONFLICT);
    assert_eq!(env.claim(&ana, &code).await.status(), StatusCode::CONFLICT);
    assert_eq!(env.host_login("Bia").await.user.role, Role::Member, "even the server PC cannot take over a claimed server");

    // The owner sets the password, and the claim is in the log, with its actor and how.
    env.set_password(&ana, "ana-senha-1").await;
    let log = env.audit(&ana).await;
    let e = log.iter().find(|e| e.action == AUDIT_CLAIM_OWNER).unwrap();
    assert_eq!((e.actor.as_ref().map(|a| a.id), e.detail.as_str()), (Some(ana.user.id), "codigo"));

    // Not logged in: no claiming (the code is useless without a session).
    let r = env.http.post(env.url("/api/owner/claim")).json(&json!({"code": code})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    env.restart().await;
    assert_eq!(env.claim_code(), None);
    env.stop().await;
}

#[tokio::test]
async fn the_claim_code_is_new_on_every_start_while_there_is_no_owner() {
    let mut env = setup().await;
    let first = env.claim_code().unwrap();
    let ana = env.remote_login("Ana").await;
    env.restart().await;
    let second = env.claim_code().unwrap();
    assert_ne!(first, second);
    let ana = Who { token: env.remote_login("Ana").await.token, ..ana };
    let (status, _) = error_of(env.claim(&ana, &first).await).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "the old code is dead");
    assert_eq!(env.claim(&ana, &second).await.status(), StatusCode::OK);
    env.stop().await;
}

// ---------------------------------------------------------------- the manual override

#[tokio::test]
async fn an_owner_in_the_config_overrides_the_claim() {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path(), Some("Dono"));
    let server = start_in(dir.path(), "127.0.0.1:0").await;
    let mut env = Env { host: format!("127.0.0.1:{}", server.addr.port()), server: Some(server), http: reqwest::Client::new(), dir };
    assert_eq!(env.claim_code(), None, "no code when the config names the owner");

    // The server PC's first login is just a member: the configured name is the owner.
    let host_pc = env.host_login("anfitriao").await;
    assert_eq!(host_pc.user.role, Role::Member);
    let (_, account) = env.ready_host(&host_pc).await;
    assert!(!account.no_owner && account.owner_pinned);
    let dono = env.host_login("dono").await;
    assert_eq!(dono.user.role, Role::Owner);
    assert_eq!(env.claim(&host_pc, "AAAA-AAAA").await.status(), StatusCode::CONFLICT);
    let (_, msg) = error_of(env.claim(&host_pc, "AAAA-AAAA").await).await;
    assert!(msg.contains("config.toml"), "{msg}");

    // Handing the server over is off too, and says why.
    env.set_password(&dono, "dono-senha").await;
    env.set_password(&host_pc, "anfitriao-senha").await;
    let r = env.post(&dono, "/api/admin/transfer-owner", json!({"user_id": host_pc.user.id})).await;
    let (status, msg) = error_of(r).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(msg.contains("config.toml"), "{msg}");
    assert_eq!(env.ready_host(&dono).await.0.role, Role::Owner);

    // It stays pinned after a restart; and a config that names an owner wins over a stored one.
    env.restart().await;
    assert_eq!(env.login_ok("Dono", Some("dono-senha"), None).await.user.role, Role::Owner);
    assert_eq!(env.login_ok("anfitriao", Some("anfitriao-senha"), None).await.user.role, Role::Member);
    env.stop().await;
}

#[tokio::test]
async fn a_config_owner_beats_a_stored_one() {
    let mut env = setup().await;
    let anfitriao = env.host_login("anfitriao").await;
    assert_eq!(anfitriao.user.role, Role::Owner);
    env.stop().await;
    // The host now writes `owner = "Ana"` in config.toml and restarts.
    write_config(env.dir.path(), Some("Ana"));
    env.start().await;
    assert_eq!(env.login_ok("anfitriao", None, None).await.user.role, Role::Member);
    assert_eq!(env.login_ok("ana", None, None).await.user.role, Role::Owner);
    // And taking the line out again brings the stored owner back.
    env.stop().await;
    write_config(env.dir.path(), None);
    env.start().await;
    assert_eq!(env.login_ok("anfitriao", None, None).await.user.role, Role::Owner);
    env.stop().await;
}

// ---------------------------------------------------------------- handing it over

#[tokio::test]
async fn the_owner_can_hand_the_server_over() {
    let mut env = setup().await;
    let ana = env.host_login("Ana").await;
    assert_eq!(ana.user.role, Role::Owner);
    env.set_password(&ana, "ana-senha-1").await;
    let bia = env.remote_login("Bia").await;
    let caio = env.remote_login("Caio").await;
    let transfer = |from: &Who, to: &Who| {
        let (from, id) = (from.clone(), to.user.id);
        let env = &env;
        async move { env.post(&from, "/api/admin/transfer-owner", json!({"user_id": id})).await }
    };
    let mut ana_ws = env.ws(&ana).await;
    ana_ws.ready().await;
    let mut bia_ws = env.ws(&bia).await;
    bia_ws.ready().await;

    // The target needs a personal password; the others cannot hand anything over; not to yourself.
    let (status, msg) = error_of(transfer(&ana, &bia).await).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(msg.contains("personal password first"), "{msg}");
    assert_eq!(transfer(&bia, &caio).await.status(), StatusCode::FORBIDDEN, "members cannot");
    assert_eq!(transfer(&ana, &ana).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(env.post(&ana, "/api/admin/transfer-owner", json!({"user_id": 9999})).await.status(), StatusCode::NOT_FOUND);
    env.set_password(&bia, "bia-senha-1").await;
    // An Admin is not the owner either.
    env.set_password(&caio, "caio-senha-1").await;
    assert_eq!(env.post(&ana, &format!("/api/admin/users/{}/role", caio.user.id), json!({"role": "admin"})).await.status(), StatusCode::OK);
    assert_eq!(transfer(&caio, &bia).await.status(), StatusCode::FORBIDDEN, "admins cannot");

    // Ana hands it over to Bia.
    assert_eq!(transfer(&ana, &bia).await.status(), StatusCode::OK);
    let ev = bia_ws.wait_for("Bia becomes owner", 5, |e| matches!(e, ServerEvent::UserUpdated { user } if user.id == bia.user.id)).await;
    assert!(matches!(ev, ServerEvent::UserUpdated { user } if user.role == Role::Owner));
    ana_ws.wait_for("Ana becomes admin", 5, |e| matches!(e, ServerEvent::UserUpdated { user } if user.id == ana.user.id && user.role == Role::Admin)).await;
    assert_eq!(env.get(&bia, "/api/admin/users").await.status(), StatusCode::OK, "Bia already has a password: her powers work at once");
    assert_eq!(env.post(&bia, &format!("/api/admin/users/{}/reset-password", caio.user.id), json!({})).await.status(), StatusCode::OK, "owner-only power");
    // Ana is an Admin now: she cannot do owner things, and cannot take it back.
    assert_eq!(transfer(&ana, &ana).await.status(), StatusCode::FORBIDDEN);
    assert_eq!(env.post(&ana, &format!("/api/admin/users/{}/reset-password", bia.user.id), json!({})).await.status(), StatusCode::FORBIDDEN);
    let (status, _) = error_of(env.claim(&ana, "AAAA-AAAA").await).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Audited: who handed it to whom.
    let log = env.audit(&bia).await;
    let e = log.iter().find(|e| e.action == AUDIT_TRANSFER_OWNER).unwrap();
    assert_eq!((e.actor.as_ref().map(|a| a.id), e.target.as_ref().map(|t| t.id)), (Some(ana.user.id), Some(bia.user.id)));

    // It lasts: after a restart Bia is the owner and Ana is an Admin.
    env.restart().await;
    assert_eq!(env.login_ok("Bia", Some("bia-senha-1"), None).await.user.role, Role::Owner);
    assert_eq!(env.login_ok("Ana", Some("ana-senha-1"), None).await.user.role, Role::Admin);
    env.stop().await;
}

// ---------------------------------------------------------------- --reset-owner

#[tokio::test]
async fn reset_owner_clears_it_and_the_host_claims_again() {
    let mut env = setup().await;
    let ana = env.host_login("Ana").await;
    env.set_password(&ana, "ana-senha-1").await;
    let bia = env.remote_login("Bia").await;
    assert_eq!(env.claim_code(), None);

    // While the server runs: the host runs `voxhub-server --reset-owner`.
    let mut bia_ws = env.ws(&bia).await;
    bia_ws.ready().await;
    let msg = voxhub_server::owner::reset_owner_cli(env.dir.path()).unwrap();
    assert!(msg.contains("Ana"), "{msg}");
    bia_ws.wait_for("Ana is no longer the owner", 8, |e| matches!(e, ServerEvent::UserUpdated { user } if user.id == ana.user.id && user.role != Role::Owner)).await;
    assert!(env.claim_code().is_some(), "a new code is printed once the running hub notices");
    assert_eq!(env.get(&ana, "/api/admin/users").await.status(), StatusCode::FORBIDDEN, "Ana lost the powers");
    let (_, account) = env.ready(&bia).await;
    assert!(account.no_owner);

    // The host claims it again from the server PC: the first loopback login wins.
    let host = env.host_login("Anfitriao").await;
    assert_eq!(host.user.role, Role::Owner);
    env.set_password(&host, "anfitriao-senha-1").await;
    let log = env.audit(&host).await;
    let reset = log.iter().find(|e| e.action == AUDIT_RESET_OWNER).expect("the reset is in the log");
    assert!(reset.actor.is_none(), "the host from the command line");
    assert_eq!(reset.detail, "Ana");
    assert_eq!(log.iter().filter(|e| e.action == AUDIT_CLAIM_OWNER).count(), 2);

    // Stopped server: the same tool works on the files, and again on a server with no owner.
    env.stop().await;
    let msg = voxhub_server::owner::reset_owner_cli(env.dir.path()).unwrap();
    assert!(msg.contains("Anfitriao"), "{msg}");
    let msg = voxhub_server::owner::reset_owner_cli(env.dir.path()).unwrap();
    assert!(msg.contains("não tinha dono"), "{msg}");
    env.start().await;
    assert!(env.claim_code().is_some());
    assert_eq!(env.login_ok("Ana", Some("ana-senha-1"), None).await.user.role, Role::Owner, "the server PC logs in as Ana this time");
    assert!(voxhub_server::owner::reset_owner_cli(&env.dir.path().join("nada")).is_err(), "wrong --data-dir");
    env.stop().await;
}

// ---------------------------------------------------------------- the owner's first password

impl Env {
    /// Set a password through a proxy (not the server PC), with the setup code if given.
    async fn set_password_remote(&self, who: &Who, pw: &str, code: Option<&str>) -> reqwest::Response {
        self.http
            .post(self.url("/api/account/password"))
            .bearer_auth(&who.token)
            .header(PROXIED.0, PROXIED.1)
            .json(&json!({ "new": pw, "owner_code": code }))
            .send()
            .await
            .unwrap()
    }
}

/// The owner's powers switch on with the first personal password of the owner's name, and that
/// name is open to anybody with the server password until then: the first password needs proof
/// of being the host (the server PC, the session that claimed, or the console's setup code).
#[tokio::test]
async fn the_owners_first_password_needs_the_host() {
    // Pinned in config.toml: a squatter on the name, from another PC, cannot take it.
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path(), Some("Dono"));
    let server = start_in(dir.path(), "127.0.0.1:0").await;
    let mut env = Env { host: format!("127.0.0.1:{}", server.addr.port()), server: Some(server), http: reqwest::Client::new(), dir };
    let code = env.server().owner_setup_code().expect("the console shows a setup code while the owner has no password");
    let squatter = env.remote_login("dono").await;
    let (status, msg) = error_of(env.set_password_remote(&squatter, "squat-pw", None).await).await;
    assert_eq!((status, msg.as_str()), (StatusCode::FORBIDDEN, ERR_OWNER_PROOF_REQUIRED));
    let (status, msg) = error_of(env.set_password_remote(&squatter, "squat-pw", Some("AAAA-AAAA")).await).await;
    assert_eq!((status, msg.as_str()), (StatusCode::FORBIDDEN, "wrong claim code"));
    assert_eq!(env.get(&squatter, "/api/admin/users").await.status(), StatusCode::FORBIDDEN, "no owner powers");
    // A member's own first password needs nothing special.
    let ana = env.remote_login("Ana").await;
    assert_eq!(env.set_password_remote(&ana, "ana-senha", None).await.status(), StatusCode::OK);
    // The host, from another PC, with the code the console shows.
    tokio::time::sleep(Duration::from_millis(1100)).await; // the wrong code's brake
    let host = env.remote_login("Dono").await;
    assert_eq!(env.set_password_remote(&host, "dono-senha", Some(&code.to_lowercase())).await.status(), StatusCode::OK);
    assert_eq!(env.get(&host, "/api/admin/users").await.status(), StatusCode::OK);
    assert_eq!(env.server().owner_setup_code(), None, "done: no code any more");
    env.stop().await;

    // Claimed: the claiming session may set it from anywhere; another session of the same name
    // may not (without the server PC or the code).
    let mut env = setup().await;
    let claimer = env.host_login("anfitriao").await;
    assert_eq!(claimer.user.role, Role::Owner);
    let other = env.remote_login("anfitriao").await;
    assert!(env.server().owner_setup_code().is_some());
    let (status, msg) = error_of(env.set_password_remote(&other, "outra-senha", None).await).await;
    assert_eq!((status, msg.as_str()), (StatusCode::FORBIDDEN, ERR_OWNER_PROOF_REQUIRED));
    assert_eq!(env.set_password_remote(&claimer, "dono-senha", None).await.status(), StatusCode::OK);
    assert_eq!(env.get(&claimer, "/api/admin/users").await.status(), StatusCode::OK);
    assert_eq!(env.get(&other, "/api/channels").await.status(), StatusCode::UNAUTHORIZED, "the other session is gone");
    env.stop().await;
}
