//! v1.1 (T0): TLS between the apps and the hub, on the hub's own port.
//!
//! - **Certificate.** Self-signed, made on the first start: ECDSA P-256, valid for 100 years
//!   (rcgen, ring). Kept in `<data>/tls/server.key` (PKCS#8 DER) and `server.crt` (DER), not
//!   DPAPI-protected, because hosts move the data folder between PCs. The apps do not check names
//!   or dates: they pin the SHA-256 of the whole certificate ([`HubTls::pin`]), which an invite
//!   carries and the app learns on first use otherwise. Its first 10 bytes, in Crockford base32,
//!   are the "Código do servidor" people can compare ([`vox_proto::tls_server_code`]).
//! - **Same port.** `serve.rs` peeks the first byte of each connection: `0x16` (a TLS handshake
//!   record) is TLS, anything else is the plain HTTP of before, so an app older than 1.1 keeps
//!   working ([`TlsMode::Auto`]). With [`TlsMode::Required`] the plain one gets a `426` instead,
//!   except from this PC itself (loopback: the host's own app, "quem hospeda vira dono").
//! - **LiveKit.** Over TLS the voice server's signalling comes through this port too: a TLS
//!   connection whose first request is for `/rtc` or `/rtc/...` ([`is_rtc_path`]) is spliced to
//!   `127.0.0.1:<signal_port>` (`serve.rs`), so `livekit_url` is `wss://<host>:<hub port>`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::task::{Context as TaskCx, Poll};

use anyhow::{Context, Result};
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_rustls::rustls;
use tokio_rustls::TlsAcceptor;

/// `config.toml` `tls = "auto" | "required" | "off"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    /// TLS for the apps that speak it (1.1 and newer), plain HTTP for older ones (the default).
    #[default]
    Auto,
    /// TLS only: a plain request from another computer gets `426` with
    /// [`vox_proto::ERR_TLS_REQUIRED`]. This PC's own (loopback) connections may still be plain.
    Required,
    /// No TLS at all (the hub of before 1.1; no certificate is made).
    Off,
}

impl TlsMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TlsMode::Auto => "auto",
            TlsMode::Required => "required",
            TlsMode::Off => "off",
        }
    }
}

/// The hub's certificate, ready to serve.
#[derive(Clone)]
pub struct HubTls {
    pub acceptor: TlsAcceptor,
    /// SHA-256 of the certificate (DER): what the apps pin.
    pub pin: [u8; 32],
}

impl std::fmt::Debug for HubTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubTls").field("pin", &vox_proto::tls_pin_hex(&self.pin)).finish()
    }
}

/// SHA-256 of a certificate's DER bytes.
pub fn pin_of(cert_der: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(cert_der));
    out
}

/// The folder of the certificate.
pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("tls")
}

const KEY_FILE: &str = "server.key";
const CERT_FILE: &str = "server.crt";

/// Load `<data>/tls/server.{key,crt}`, making them on the first start. A damaged pair is kept
/// aside (`*.bad`) and replaced: the apps that pinned the old one then say "A identidade do
/// servidor mudou", which is the truth.
pub fn load_or_create(data_dir: &Path) -> Result<HubTls> {
    let dir = dir(data_dir);
    let (key_path, cert_path) = (dir.join(KEY_FILE), dir.join(CERT_FILE));
    // Audit (T0): only a pair that was READ and is unusable (or half missing) is put aside. A file
    // that cannot be read right now (permissions, an antivirus or backup holding it) stops the
    // start instead: a silent new certificate would make every app say "A identidade do servidor
    // mudou" and teach people to click "Confiar no novo código".
    let read = |p: &Path| match std::fs::read(p) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context(format!("reading {} (not replaced: fix its permissions, or move the tls folder away to make a new certificate)", p.display()))),
    };
    let (key, cert) = (read(&key_path)?, read(&cert_path)?);
    if key.is_some() || cert.is_some() {
        let loaded = match (key, cert) {
            (Some(key), Some(cert)) => from_der(key, cert),
            (None, _) => Err(anyhow::anyhow!("{} is missing", key_path.display())),
            (_, None) => Err(anyhow::anyhow!("{} is missing", cert_path.display())),
        };
        match loaded {
            Ok(t) => return Ok(t),
            Err(e) => {
                tracing::error!("TLS certificate in {} is damaged ({e:#}): making a new one", dir.display());
                for p in [&key_path, &cert_path] {
                    if p.exists() {
                        let _ = std::fs::rename(p, p.with_extension(format!("{}.bad", p.extension().and_then(|x| x.to_str()).unwrap_or(""))));
                    }
                }
            }
        }
    }
    create(&dir)?;
    load(&key_path, &cert_path)
}

fn load(key_path: &Path, cert_path: &Path) -> Result<HubTls> {
    let key = std::fs::read(key_path).with_context(|| format!("reading {}", key_path.display()))?;
    let cert = std::fs::read(cert_path).with_context(|| format!("reading {}", cert_path.display()))?;
    from_der(key, cert)
}

fn from_der(key: Vec<u8>, cert: Vec<u8>) -> Result<HubTls> {
    let pin = pin_of(&cert);
    let cert = rustls::pki_types::CertificateDer::from(cert);
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(key));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS versions")?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .context("the TLS key does not match the certificate")?;
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(HubTls { acceptor: TlsAcceptor::from(Arc::new(cfg)), pin })
}

fn create(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).context("making the TLS key")?;
    let mut params = rcgen::CertificateParams::new(vec!["concord".to_string()]).context("certificate parameters")?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.distinguished_name.push(rcgen::DnType::CommonName, "concord");
    params.not_before = rcgen::date_time_ymd(2025, 1, 1);
    params.not_after = rcgen::date_time_ymd(2125, 1, 1);
    let cert = params.self_signed(&key).context("making the TLS certificate")?;
    // The key first: a certificate without its key is useless, a key without its certificate is
    // made again.
    write_private(&dir.join(KEY_FILE), &key.serialize_der())?;
    write_atomic(&dir.join(CERT_FILE), cert.der())?;
    tracing::info!(dir = %dir.display(), code = %vox_proto::tls_server_code(&pin_of(cert.der())), "TLS certificate created");
    Ok(())
}

/// Write through a temporary file and a rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = tmp_of(path);
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

/// The private key: only its owner may read it (Unix `0600`; on Windows the data folder's own
/// permissions apply, as for the database and `config.toml`, which hold the LiveKit secret).
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = tmp_of(path);
    // Audit (T0): a leftover temporary file would keep its own (wider) permissions: start afresh,
    // so the Unix mode below applies.
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(bytes).with_context(|| format!("writing {}", tmp.display()))?;
    f.sync_all().ok();
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

/// `server.key.tmp` / `server.crt.tmp` (audit T0: both used to be `server.tmp`).
fn tmp_of(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

// ---------------------------------------------------------------- the live policy

/// Whether plain HTTP from other computers is still served (`auto`) or refused (`required`),
/// changeable while the hub runs (the app's "Aceitar apps antigos sem conexão protegida").
#[derive(Clone, Debug)]
pub struct Policy(Arc<AtomicU8>, Arc<std::sync::atomic::AtomicBool>);

impl Policy {
    pub fn new(mode: TlsMode) -> Policy {
        let p = Policy(Arc::new(AtomicU8::new(0)), Arc::new(std::sync::atomic::AtomicBool::new(true)));
        p.set(mode);
        p
    }

    /// Tests only (everything there is loopback): treat this PC's own connections like any other.
    #[doc(hidden)]
    pub fn spare_loopback(&self, on: bool) {
        self.1.store(on, Ordering::Relaxed);
    }

    pub fn mode(&self) -> TlsMode {
        match self.0.load(Ordering::Relaxed) {
            1 => TlsMode::Required,
            2 => TlsMode::Off,
            _ => TlsMode::Auto,
        }
    }

    /// `Off` cannot be switched on or off while running (there is no certificate loaded).
    pub fn set(&self, mode: TlsMode) {
        self.0.store(
            match mode {
                TlsMode::Auto => 0,
                TlsMode::Required => 1,
                TlsMode::Off => 2,
            },
            Ordering::Relaxed,
        );
    }

    /// Plain HTTP from `peer` is refused right now.
    pub fn refuses_plain(&self, peer: std::net::IpAddr) -> bool {
        self.mode() == TlsMode::Required && !(self.1.load(Ordering::Relaxed) && peer.to_canonical().is_loopback())
    }
}

// ---------------------------------------------------------------- per request

/// How a request reached the hub (a request extension set by `serve.rs`; absent = plain).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transport {
    Tls,
    #[default]
    Plain,
}

impl<S: Send + Sync> FromRequestParts<S> for Transport {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(parts.extensions.get::<Transport>().copied().unwrap_or_default())
    }
}

/// A request for LiveKit's signalling (`/rtc`, `/rtc/v1`, `/rtc/validate?...`): exactly `/rtc` or
/// anything under `/rtc/`. Never `/rtcx`, `/twirp/...` (LiveKit's server API stays on loopback)
/// or `/api/rtc`.
pub fn is_rtc_path(target: &str) -> bool {
    let path = target.split(['?', '#']).next().unwrap_or("");
    // Audit (T0): and nothing a server could resolve to another route (`/rtc/../twirp`,
    // `/rtc/%2e%2e/twirp`, `/rtc//x`, backslashes). LiveKit's own paths are plain words.
    let plain = !path.contains(['%', '\\']) && !path.contains("//") && !path.split('/').any(|seg| seg == ".." || seg == ".");
    plain && (path == "/rtc" || path.starts_with("/rtc/"))
}

/// The request target of an HTTP/1 request line in `buf`: `Some(Some(path))` once the line is
/// complete, `Some(None)` when it is not a request line at all (hyper answers it), `None` while
/// more bytes are needed.
pub fn request_target(buf: &[u8]) -> Option<Option<String>> {
    let line_done = buf.windows(2).any(|w| w == b"\r\n");
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(buf) {
        Ok(_) => match req.path {
            Some(p) => Some(Some(p.to_string())),
            None if line_done => Some(None),
            None => None,
        },
        // Too many headers for our small array: the request line itself parsed.
        Err(httparse::Error::TooManyHeaders) => Some(req.path.map(str::to_string)),
        Err(_) => Some(None),
    }
}

/// A stream that first gives back bytes already read from it.
pub struct Prefixed<S> {
    prefix: Vec<u8>,
    at: usize,
    inner: S,
}

impl<S> Prefixed<S> {
    pub fn new(prefix: Vec<u8>, inner: S) -> Self {
        Prefixed { prefix, at: 0, inner }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Prefixed<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut TaskCx<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        if self.at < self.prefix.len() {
            let n = (self.prefix.len() - self.at).min(buf.remaining());
            let at = self.at;
            buf.put_slice(&self.prefix[at..at + n]);
            self.at += n;
            if self.at == self.prefix.len() {
                self.prefix = Vec::new();
                self.at = 0;
            }
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Prefixed<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut TaskCx<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskCx<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskCx<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
    fn poll_write_vectored(mut self: Pin<&mut Self>, cx: &mut TaskCx<'_>, bufs: &[std::io::IoSlice<'_>]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rtc_matcher() {
        for yes in ["/rtc", "/rtc/v1", "/rtc/validate?access_token=x&auto_subscribe=1", "/rtc?access_token=x", "/rtc/"] {
            assert!(is_rtc_path(yes), "{yes}");
        }
        for no in [
            "/rtcx",
            "/rtc-x",
            "/twirp/livekit.RoomService/ListRooms",
            "/api/rtc",
            "/",
            "",
            "rtc",
            "/RTC",
            "/ws?token=x",
            "/api/health",
            // Audit (T0): paths a server could resolve elsewhere, and absolute-form targets.
            "/rtc/../twirp/livekit.RoomService/ListRooms",
            "/rtc/./x",
            "/rtc/..",
            "/rtc/%2e%2e/twirp/livekit.RoomService/ListRooms",
            "/rtc%2F..%2Ftwirp",
            "/rtc//twirp",
            "/rtc/..\\twirp",
            "http://127.0.0.1:7880/rtc",
            "http://x/twirp/livekit.RoomService/ListRooms",
        ] {
            assert!(!is_rtc_path(no), "{no}");
        }
    }

    #[test]
    fn request_lines() {
        assert_eq!(request_target(b"GET /rtc/v1?x=1 HTTP/1.1\r\nHost: a\r\n\r\n"), Some(Some("/rtc/v1?x=1".into())));
        // The line done, headers still coming.
        assert_eq!(request_target(b"GET /rtc HTTP/1.1\r\nHost: a"), Some(Some("/rtc".into())));
        assert_eq!(request_target(b"GET /rt"), None);
        assert_eq!(request_target(b""), None);
        assert_eq!(request_target(b"\x00\x01garbage\r\n"), Some(None));
        assert_eq!(request_target(b"GET\r\n"), Some(None));
        let many: String = (0..100).map(|i| format!("X-{i}: y\r\n")).collect();
        assert_eq!(request_target(format!("GET /api/health HTTP/1.1\r\n{many}\r\n").as_bytes()), Some(Some("/api/health".into())));
    }

    #[test]
    fn certificates_are_made_once_and_kept() {
        let dir = tempdir();
        let a = load_or_create(&dir).unwrap();
        let b = load_or_create(&dir).unwrap();
        assert_eq!(a.pin, b.pin, "the same certificate on the next start");
        let der = std::fs::read(super::dir(&dir).join(CERT_FILE)).unwrap();
        assert_eq!(pin_of(&der), a.pin);
        // Another data folder, another certificate.
        let other = tempdir();
        assert_ne!(load_or_create(&other).unwrap().pin, a.pin);
        // A damaged certificate is put aside and replaced.
        std::fs::write(super::dir(&dir).join(CERT_FILE), b"not a certificate").unwrap();
        let c = load_or_create(&dir).unwrap();
        assert_ne!(c.pin, a.pin);
        assert!(super::dir(&dir).join("server.crt.bad").exists());
        // Audit (T0): the private key never goes through the shared temporary name, and a key
        // that cannot be READ (here: a folder in its place) stops the start: nothing is put aside
        // or replaced, the identity does not change behind anybody's back.
        assert!(!super::dir(&dir).join("server.tmp").exists() && !super::dir(&dir).join("server.key.tmp").exists());
        let unreadable = tempdir();
        let kept = load_or_create(&unreadable).unwrap();
        let tls = super::dir(&unreadable);
        std::fs::rename(tls.join(KEY_FILE), tls.join("key-elsewhere")).unwrap();
        std::fs::create_dir(tls.join(KEY_FILE)).unwrap();
        let err = load_or_create(&unreadable).expect_err("an unreadable key must stop the start");
        assert!(format!("{err:#}").contains("not replaced"), "{err:#}");
        assert!(!tls.join("server.crt.bad").exists() && tls.join(CERT_FILE).exists(), "nothing put aside");
        std::fs::remove_dir(tls.join(KEY_FILE)).unwrap();
        std::fs::rename(tls.join("key-elsewhere"), tls.join(KEY_FILE)).unwrap();
        assert_eq!(load_or_create(&unreadable).unwrap().pin, kept.pin, "the same identity once readable again");
        // Half a pair (the certificate lost): made again, the key put aside.
        std::fs::remove_file(tls.join(CERT_FILE)).unwrap();
        assert_ne!(load_or_create(&unreadable).unwrap().pin, kept.pin);
        assert!(tls.join("server.key.bad").exists());
        let _ = std::fs::remove_dir_all(&unreadable);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn the_policy_spares_loopback() {
        let p = Policy::new(TlsMode::Auto);
        assert!(!p.refuses_plain("203.0.113.9".parse().unwrap()));
        p.set(TlsMode::Required);
        assert!(p.refuses_plain("203.0.113.9".parse().unwrap()));
        assert!(p.refuses_plain("2001:db8::1".parse().unwrap()));
        assert!(!p.refuses_plain("127.0.0.1".parse().unwrap()));
        assert!(!p.refuses_plain("::1".parse().unwrap()));
        assert!(!p.refuses_plain("::ffff:127.0.0.1".parse().unwrap()));
        assert_eq!(toml::from_str::<std::collections::HashMap<String, TlsMode>>("tls = \"required\"").unwrap()["tls"], TlsMode::Required);
    }

    fn tempdir() -> PathBuf {
        use std::sync::atomic::AtomicU32;
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!("concord-tls-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&d);
        d
    }
}
