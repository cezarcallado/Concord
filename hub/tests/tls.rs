//! v1.1 (T0): TLS on the hub's own port. A real hub on an ephemeral port (no LiveKit; a fake one
//! answers on the signal port), clients pinned to its certificate.
//!
//! - login, the event socket and a chat image over pinned TLS, and `wss://<hub port>` for voice;
//! - another certificate is refused before any HTTP;
//! - plain HTTP still works in `auto`, gets `426` in `required` (except from this PC, unless the
//!   test says otherwise), and `off` has no TLS at all;
//! - `/rtc...` is spliced to the signal port (and only that);
//! - the connection caps hold during handshakes, and a stalled handshake is dropped.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use vox_proto::*;
use voxhub_server::serve::Limits;
use voxhub_server::tls::TlsMode;
use voxhub_server::{start_tuned, Options, Server, Tuning};

const PASSWORD: &str = "hunter2";

/// Accepts exactly one certificate (by SHA-256), checking handshake signatures with ring.
#[derive(Debug)]
struct Pinned([u8; 32], Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(&self, cert: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        if Sha256::digest(cert.as_ref()).as_slice() == self.0 {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure))
        }
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.1.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.1.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.1.signature_verification_algorithms.supported_schemes()
    }
}

fn client_config(pin: [u8; 32]) -> rustls::ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned(pin, provider)))
        .with_no_client_auth()
}

fn pinned_http(pin: [u8; 32]) -> reqwest::Client {
    reqwest::Client::builder().use_preconfigured_tls(client_config(pin)).https_only(true).build().unwrap()
}

async fn tls_stream(port: u16, pin: [u8; 32]) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(("127.0.0.1", port)).await?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config(pin)));
    connector.connect(ServerName::IpAddress(std::net::IpAddr::from([127, 0, 0, 1]).into()), tcp).await
}

/// A fake LiveKit on loopback: answers every request with `livekit <request line>` in the body,
/// like Go's server keeps the connection for the next request; a WebSocket upgrade gets `101` (the
/// line in a header) and then an echo, as a WebSocket's frames would flow both ways. Every request
/// line it gets is kept (audit T0: what reached "LiveKit").
async fn fake_livekit_seen() -> (u16, Arc<std::sync::Mutex<Vec<String>>>) {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { return };
            let log = log.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    let end = loop {
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                        match s.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    };
                    let text = String::from_utf8_lossy(&buf[..end]).to_string();
                    buf.drain(..end);
                    let line = text.lines().next().unwrap_or_default().to_string();
                    log.lock().unwrap().push(line.clone());
                    if text.to_ascii_lowercase().contains("upgrade: websocket") {
                        let _ = s.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\nx-livekit: livekit {line}\r\n\r\n").as_bytes()).await;
                        if !buf.is_empty() && s.write_all(&buf).await.is_err() {
                            return;
                        }
                        loop {
                            match s.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => {
                                    if s.write_all(&chunk[..n]).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                    }
                    let body = format!("livekit {line}");
                    if s.write_all(format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (port, seen)
}

async fn fake_livekit() -> u16 {
    fake_livekit_seen().await.0
}

/// A WebSocket-style upgrade request head for `path`.
fn upgrade(path: &str, port: u16) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n")
}

/// Read until the peer closes (or 5 s): everything it sent.
async fn read_to_close<S: tokio::io::AsyncRead + Unpin>(s: &mut S) -> (String, bool) {
    let mut out = Vec::new();
    let r = tokio::time::timeout(Duration::from_secs(5), async {
        let mut chunk = [0u8; 4096];
        loop {
            match s.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => out.extend_from_slice(&chunk[..n]),
            }
        }
    })
    .await;
    (String::from_utf8_lossy(&out).into_owned(), r.is_ok())
}

async fn hub(tls: TlsMode, signal_port: u16, limits: Limits) -> (Server, u16, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "server_name = \"tls\"\nserver_password = \"{PASSWORD}\"\ntls = \"{}\"\n\
             [livekit]\napi_key = \"APItest0000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123456789abcdef\"\nsignal_port = {signal_port}\n",
            tls.as_str()
        ),
    )
    .unwrap();
    let opts = Options { data_dir: dir.path().to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: Some("127.0.0.1:0".into()) };
    let server = start_tuned(opts, Default::default(), Tuning { limits, ..Tuning::default() }).await.unwrap();
    let port = server.addr.port();
    (server, port, dir)
}

fn login_req(name: &str) -> LoginRequest {
    LoginRequest { username: name.into(), server_password: PASSWORD.into(), protocol_version: PROTOCOL_VERSION, ..Default::default() }
}

async fn send_raw<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(s: &mut S, req: &str) -> String {
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let mut chunk = [0u8; 4096];
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match s.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    out.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&out);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end].lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
                        if out.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
            }
        }
    })
    .await;
    String::from_utf8_lossy(&out).into_owned()
}

/// Login, the event socket, a chat image and the voice URL, all over pinned TLS.
#[tokio::test]
async fn a_pinned_client_does_everything_over_tls() {
    let lk = fake_livekit().await;
    let (server, port, dir) = hub(TlsMode::Auto, lk, Limits::default()).await;
    let pin = server.tls_pin().expect("auto makes a certificate");
    // The pin is the SHA-256 of the certificate file, kept in <data>/tls.
    let der = std::fs::read(dir.path().join("tls").join("server.crt")).unwrap();
    assert_eq!(Sha256::digest(&der).as_slice(), pin);
    assert!(dir.path().join("tls").join("server.key").exists());

    let base = format!("https://127.0.0.1:{port}");
    let http = pinned_http(pin);
    let health: HealthResponse = http.get(format!("{base}/api/health")).send().await.unwrap().json().await.unwrap();
    assert!(health.ok);
    let login: LoginResponse = http.post(format!("{base}/api/login")).json(&login_req("ana")).send().await.unwrap().json().await.unwrap();

    // Voice over TLS: LiveKit through the hub's own port.
    let chs: Vec<Channel> = http.get(format!("{base}/api/channels")).bearer_auth(&login.token).send().await.unwrap().json().await.unwrap();
    let voice = chs.iter().find(|c| c.kind == ChannelKind::Voice).unwrap().id;
    let text = chs.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    let join: VoiceJoinResponse = http.post(format!("{base}/api/voice/{voice}/join")).bearer_auth(&login.token).send().await.unwrap().json().await.unwrap();
    assert_eq!(join.livekit_url, format!("wss://127.0.0.1:{port}"));
    // ...and the direct ws:// of before in the clear.
    let plain = reqwest::Client::new();
    let join: VoiceJoinResponse = plain.post(format!("http://127.0.0.1:{port}/api/voice/{voice}/join")).bearer_auth(&login.token).send().await.unwrap().json().await.unwrap();
    assert_eq!(join.livekit_url, format!("ws://127.0.0.1:{lk}"));

    // The event socket over the same TLS.
    let tls = tls_stream(port, pin).await.unwrap();
    let (mut ws, _) = tokio_tungstenite::client_async(format!("wss://127.0.0.1:{port}/ws?token={}", login.token), tls).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.unwrap().unwrap().unwrap();
    assert!(first.to_text().unwrap().contains("\"ready\""), "{first:?}");

    // A chat image up and down.
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80).encode_image(&image::RgbImage::from_pixel(64, 48, image::Rgb([200, 30, 30]))).unwrap();
    let r = http.post(format!("{base}/api/attachments?thumb_len=0")).bearer_auth(&login.token).body(jpeg).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let att: Attachment = r.json().await.unwrap();
    let r = http.post(format!("{base}/api/channels/{text}/messages")).bearer_auth(&login.token).json(&serde_json::json!({ "content": "foto", "attachments": [att.id] })).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let got = http.get(format!("{base}/api/attachments/{}", att.id)).bearer_auth(&login.token).send().await.unwrap();
    assert_eq!(got.status(), StatusCode::OK);
    assert!(!got.bytes().await.unwrap().is_empty());
    let created = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(m)) = ws.next().await {
            if m.to_text().is_ok_and(|t| t.contains("message_created") && t.contains("foto")) {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(created, Ok(true), "the event came over the TLS socket");
    let _ = ws.send(tokio_tungstenite::tungstenite::Message::Close(None)).await;

    // Another certificate: refused in the handshake, nothing sent.
    assert!(tls_stream(port, [0u8; 32]).await.is_err());
    assert!(pinned_http([0u8; 32]).get(format!("{base}/api/health")).send().await.is_err());

    // The invite carries the pin.
    let r = http.post(format!("{base}/api/account/password")).bearer_auth(&login.token).json(&serde_json::json!({ "new": "senha-ana" })).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    server.shutdown().await;
}

#[tokio::test]
async fn invites_carry_the_pin() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!("server_name = \"t\"\nserver_password = \"{PASSWORD}\"\nowner = \"dono\"\n[livekit]\napi_key = \"APItest0000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123456789abcdef\"\nsignal_port = 1\n"),
    )
    .unwrap();
    let opts = Options { data_dir: dir.path().to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: Some("127.0.0.1:0".into()) };
    let server = voxhub_server::start(opts).await.unwrap();
    let pin = server.tls_pin().unwrap();
    let base = format!("https://127.0.0.1:{}", server.addr.port());
    let http = pinned_http(pin);
    let login: LoginResponse = http.post(format!("{base}/api/login")).json(&login_req("dono")).send().await.unwrap().json().await.unwrap();
    let r = http.post(format!("{base}/api/account/password")).bearer_auth(&login.token).json(&serde_json::json!({ "new": "senha-do-dono" })).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let created: InviteCreated = http.post(format!("{base}/api/invites")).bearer_auth(&login.token).json(&InviteCreateRequest::default()).send().await.unwrap().json().await.unwrap();
    assert_eq!(created.hints.tls_pin.as_deref(), Some(tls_pin_hex(&pin).as_str()));
    server.shutdown().await;
}

/// `auto` keeps plain HTTP; `required` answers it with 426 (this PC's own connections excepted);
/// switching is live; `off` has no TLS at all.
#[tokio::test]
async fn plain_in_auto_426_in_required_nothing_in_off() {
    let (server, port, _dir) = hub(TlsMode::Auto, 1, Limits::default()).await;
    // A new connection per request: the policy applies to connections as they arrive.
    let plain = reqwest::Client::builder().pool_max_idle_per_host(0).build().unwrap();
    let url = format!("http://127.0.0.1:{port}/api/health");
    assert_eq!(plain.get(&url).send().await.unwrap().status(), StatusCode::OK);
    let policy = server.tls_policy();
    policy.set(TlsMode::Required);
    assert_eq!(plain.get(&url).send().await.unwrap().status(), StatusCode::OK, "this PC itself may still be plain");
    policy.spare_loopback(false);
    let r = plain.get(&url).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UPGRADE_REQUIRED);
    assert_eq!(r.json::<ApiError>().await.unwrap().error, ERR_TLS_REQUIRED);
    let r = plain.post(format!("http://127.0.0.1:{port}/api/login")).json(&login_req("bia")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UPGRADE_REQUIRED, "a login in the clear never reaches the hub");
    // TLS still works.
    let tls = pinned_http(server.tls_pin().unwrap());
    assert_eq!(tls.get(format!("https://127.0.0.1:{port}/api/health")).send().await.unwrap().status(), StatusCode::OK);
    policy.set(TlsMode::Auto);
    assert_eq!(plain.get(&url).send().await.unwrap().status(), StatusCode::OK);
    server.shutdown().await;

    let (server, port, dir) = hub(TlsMode::Off, 1, Limits::default()).await;
    assert_eq!(server.tls_pin(), None);
    assert!(!dir.path().join("tls").exists(), "no certificate is made with tls = off");
    assert_eq!(plain.get(format!("http://127.0.0.1:{port}/api/health")).send().await.unwrap().status(), StatusCode::OK);
    assert!(tls_stream(port, [0u8; 32]).await.is_err());
    server.shutdown().await;
}

/// `/rtc`, `/rtc/v1`, `/rtc/validate?...` over TLS go to LiveKit's signal port; nothing else does.
#[tokio::test]
async fn rtc_is_spliced_to_the_signal_port() {
    let lk = fake_livekit().await;
    let (server, port, _dir) = hub(TlsMode::Auto, lk, Limits::default()).await;
    let pin = server.tls_pin().unwrap();
    for path in ["/rtc", "/rtc/v1?access_token=abc", "/rtc/validate?access_token=abc&auto_subscribe=1"] {
        // A WebSocket (101): the splice keeps going both ways.
        let mut s = tls_stream(port, pin).await.unwrap();
        let answer = send_raw(&mut s, &upgrade(path, port)).await;
        assert!(answer.starts_with("HTTP/1.1 101") && answer.contains(&format!("livekit GET {path} HTTP/1.1")), "{path}: {answer}");
        s.write_all(b"ping-frame").await.unwrap();
        let mut back = [0u8; 10];
        tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut back)).await.unwrap().unwrap();
        assert_eq!(&back, b"ping-frame");
        // A plain request (`/rtc/validate`): its answer, then the connection ends.
        let mut s = tls_stream(port, pin).await.unwrap();
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes()).await.unwrap();
        let (answer, closed) = read_to_close(&mut s).await;
        assert!(answer.starts_with("HTTP/1.1 200") && answer.ends_with(&format!("livekit GET {path} HTTP/1.1")), "{path}: {answer}");
        assert!(closed, "{path}: the connection ends after a non-WebSocket answer");
    }
    // The first line arriving in pieces is still read whole.
    let mut s = tls_stream(port, pin).await.unwrap();
    s.write_all(b"GET /rt").await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let answer = send_raw(&mut s, &format!("c/v1 HTTP/1.1\r\nHost: x\r\n\r\n")).await;
    assert!(answer.contains("livekit GET /rtc/v1 HTTP/1.1"), "{answer}");
    for path in ["/rtcx", "/twirp/livekit.RoomService/ListRooms", "/api/rtc", "/api/health"] {
        let mut s = tls_stream(port, pin).await.unwrap();
        let answer = send_raw(&mut s, &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")).await;
        assert!(!answer.contains("livekit GET"), "{path} went to LiveKit: {answer}");
        assert!(answer.starts_with("HTTP/1.1 "), "{path}: {answer}");
    }
    // In the clear, /rtc is the hub's own (unknown) route: LiveKit stays on its own port.
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let answer = send_raw(&mut s, "GET /rtc HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await;
    assert!(answer.starts_with("HTTP/1.1 404"), "{answer}");
    // A first line longer than 8 KiB is not spliced (hyper refuses it).
    let mut s = tls_stream(port, pin).await.unwrap();
    let long = format!("GET /rtc/{} HTTP/1.1\r\nHost: x\r\n\r\n", "a".repeat(9000));
    let answer = send_raw(&mut s, &long).await;
    assert!(!answer.contains("livekit GET"), "{}", &answer[..answer.len().min(200)]);
    // Shutdown closes a live splice.
    let mut s = tls_stream(port, pin).await.unwrap();
    let answer = send_raw(&mut s, &upgrade("/rtc", port)).await;
    assert!(answer.starts_with("HTTP/1.1 101"), "{answer}");
    server.shutdown().await;
    let mut buf = [0u8; 16];
    let closed = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf)).await;
    assert!(matches!(closed, Ok(Ok(0)) | Ok(Err(_))), "{closed:?}");
}

/// Audit (T0): nothing but one body-less `GET /rtc...` per connection reaches LiveKit: not a second
/// request after it (pipelined or keep-alive: LiveKit's `/twirp` API is on the same port), not a
/// path that resolves elsewhere, not an absolute-form target or a `Host` naming LiveKit, not a
/// request with a body.
#[tokio::test]
async fn the_splice_reaches_nothing_but_rtc() {
    let (lk, seen) = fake_livekit_seen().await;
    let (server, port, _dir) = hub(TlsMode::Auto, lk, Limits::default()).await;
    let pin = server.tls_pin().unwrap();
    let twirp = "POST /twirp/livekit.RoomService/ListRooms HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";

    // Pipelined behind a validate: the validate is answered, the second request never goes out.
    let mut s = tls_stream(port, pin).await.unwrap();
    s.write_all(format!("GET /rtc/validate?access_token=a HTTP/1.1\r\nHost: x\r\n\r\n{twirp}").as_bytes()).await.unwrap();
    let (answer, closed) = read_to_close(&mut s).await;
    assert!(answer.ends_with("livekit GET /rtc/validate?access_token=a HTTP/1.1") && closed, "{answer}");
    // Keep-alive: a second request after the first answer finds the connection closed.
    let mut s = tls_stream(port, pin).await.unwrap();
    let answer = send_raw(&mut s, "GET /rtc/validate HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(answer.contains("livekit GET /rtc/validate"), "{answer}");
    let _ = s.write_all(twirp.as_bytes()).await;
    let (more, closed) = read_to_close(&mut s).await;
    assert!(more.is_empty() && closed, "{more}");
    // HTTP/1.0 works the same.
    let mut s = tls_stream(port, pin).await.unwrap();
    s.write_all(b"GET /rtc/validate HTTP/1.0\r\n\r\n").await.unwrap();
    let (answer, closed) = read_to_close(&mut s).await;
    assert!(answer.contains("livekit GET /rtc/validate HTTP/1.0") && closed, "{answer}");

    // Paths that resolve elsewhere, absolute-form targets, a Host naming LiveKit: the hub's own
    // router answers them (404 / 400), LiveKit never sees them.
    for head in [
        "GET /rtc/../twirp/livekit.RoomService/ListRooms HTTP/1.1\r\nHost: x".to_string(),
        "GET /rtc/%2e%2e/twirp/livekit.RoomService/ListRooms HTTP/1.1\r\nHost: x".to_string(),
        "GET /rtc%2F..%2Ftwirp%2Flivekit.RoomService%2FListRooms HTTP/1.1\r\nHost: x".to_string(),
        "GET /rtc//twirp HTTP/1.1\r\nHost: x".to_string(),
        format!("GET http://127.0.0.1:{lk}/rtc HTTP/1.1\r\nHost: 127.0.0.1:{lk}"),
        format!("POST http://127.0.0.1:{lk}/twirp/livekit.RoomService/ListRooms HTTP/1.1\r\nHost: 127.0.0.1:{lk}\r\nContent-Length: 0"),
        format!("POST /twirp/livekit.RoomService/ListRooms HTTP/1.1\r\nHost: 127.0.0.1:{lk}\r\nContent-Length: 0"),
    ] {
        let mut s = tls_stream(port, pin).await.unwrap();
        let answer = send_raw(&mut s, &format!("{head}\r\nConnection: close\r\n\r\n")).await;
        assert!(answer.starts_with("HTTP/1.1 4"), "{head}: {answer}");
        assert!(!answer.contains("livekit"), "{head} reached LiveKit: {answer}");
    }
    // A body, or another method: refused before LiveKit.
    for head in ["POST /rtc HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{}", "GET /rtc HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n", "GET /rtc HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello"] {
        let mut s = tls_stream(port, pin).await.unwrap();
        s.write_all(head.as_bytes()).await.unwrap();
        let (answer, closed) = read_to_close(&mut s).await;
        assert!(answer.starts_with("HTTP/1.1 400") && closed, "{head}: {answer}");
    }

    let seen = seen.lock().unwrap().clone();
    assert!(seen.iter().all(|l| l.starts_with("GET /rtc/validate")), "LiveKit got more than /rtc: {seen:?}");
    assert_eq!(seen.len(), 3, "{seen:?}");
    server.shutdown().await;
}

/// A TLS hello holds its slot like any connection (the caps count it); a handshake that stalls is
/// dropped within the header timeout and gives its slot back.
#[tokio::test]
async fn the_caps_hold_during_handshakes() {
    let limits = Limits { max_conns_per_ip: 3, header_read_timeout: Duration::from_secs(2), ..Limits::default() };
    let (server, port, _dir) = hub(TlsMode::Auto, 1, limits).await;
    let pin = server.tls_pin().unwrap();
    // Three stalled handshakes: a TLS record header and nothing more.
    let mut stalled = Vec::new();
    for _ in 0..3 {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(&[0x16, 0x03, 0x01]).await.unwrap();
        stalled.push(s);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(server.open_connections(), 3);
    // The fourth is refused at once.
    assert!(tls_stream(port, pin).await.is_err(), "over the cap");
    // The stalled ones are dropped by the handshake timeout, and their slots come back.
    let t = Instant::now();
    for s in &mut stalled {
        let mut buf = [0u8; 64];
        let r = tokio::time::timeout(Duration::from_secs(6), s.read(&mut buf)).await;
        assert!(matches!(r, Ok(Ok(0)) | Ok(Err(_))), "{r:?}");
    }
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(server.open_connections(), 0);
    let http = pinned_http(pin);
    assert_eq!(http.get(format!("https://127.0.0.1:{port}/api/health")).send().await.unwrap().status(), StatusCode::OK);
    // A TLS connection that never sends its request line is dropped too.
    let mut s = tls_stream(port, pin).await.unwrap();
    let mut buf = [0u8; 16];
    let r = tokio::time::timeout(Duration::from_secs(6), s.read(&mut buf)).await;
    assert!(matches!(r, Ok(Ok(0)) | Ok(Err(_))), "{r:?}");
    server.shutdown().await;
}
