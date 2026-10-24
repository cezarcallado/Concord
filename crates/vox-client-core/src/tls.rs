//! v1.1 (T0): TLS to the hub with a pinned certificate.
//!
//! The hub's certificate is self-signed (`hub/src/tls.rs`). The app does not trust it through any
//! certificate authority: it compares the SHA-256 of the certificate the hub shows with the pin
//! it got from an invite code or learned on the first login ([`PinVerifier`]). Names and dates are
//! not checked (the hub is reached by IP as often as by name, and the certificate is made once
//! for 100 years); the handshake signatures are, by rustls' own code with ring's algorithms
//! ([`rustls::crypto::verify_tls12_signature`] / [`rustls::crypto::verify_tls13_signature`]), so
//! only the holder of the pinned certificate's key can complete a handshake.
//!
//! - [`probe`]: what speaks at an address (TLS and its certificate's pin, no TLS at all: a hub
//!   older than 1.1, or nothing), without sending a byte of HTTP. The app decides the transport
//!   from it (`backend.rs`, the trust rules).
//! - [`client_config`]: the rustls client configuration for reqwest
//!   (`ClientBuilder::use_preconfigured_tls`) and tungstenite (`Connector::Rustls`), always on the
//!   ring provider and always pinned.
//! - [`Tunnel`]: the LiveKit SDK cannot be given a verifier, so over TLS it talks to a local
//!   `ws://127.0.0.1:<port>` that wraps every connection in pinned TLS to the hub.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use url::Url;

/// SHA-256 of a hub's certificate (DER).
pub type Pin = [u8; 32];

/// TCP connect limit of a probe or a tunnelled connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A TLS handshake must be done within this: a hub older than 1.1 answers a TLS hello with a plain
/// HTTP error at once, so this only bounds a silent peer.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);

/// SHA-256 of a certificate's DER bytes.
pub fn pin_of(cert_der: &[u8]) -> Pin {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(cert_der));
    out
}

/// The ring provider (never aws-lc-rs: the hub and the app use the same small one).
pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// What a [`PinVerifier`] accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// Exactly this certificate.
    Pinned(Pin),
    /// Any certificate, recording its pin (a probe: nothing is sent over such a connection).
    Tofu,
}

/// Checks the hub's certificate against a pin (ignoring names and dates) and delegates the
/// handshake signatures to rustls with ring's algorithms.
#[derive(Debug)]
pub struct PinVerifier {
    trust: Trust,
    provider: Arc<CryptoProvider>,
    /// The pin of the last certificate shown (also when it was refused).
    seen: Mutex<Option<Pin>>,
}

impl PinVerifier {
    pub fn new(trust: Trust) -> Arc<PinVerifier> {
        Arc::new(PinVerifier { trust, provider: provider(), seen: Mutex::new(None) })
    }

    /// The pin of the certificate the peer showed, if it showed one.
    pub fn seen(&self) -> Option<Pin> {
        *self.seen.lock()
    }
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(&self, end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>], _name: &ServerName<'_>, _ocsp: &[u8], _now: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        let got = pin_of(end_entity.as_ref());
        *self.seen.lock() = Some(got);
        match self.trust {
            Trust::Tofu => Ok(ServerCertVerified::assertion()),
            Trust::Pinned(want) if want == got => Ok(ServerCertVerified::assertion()),
            Trust::Pinned(_) => Err(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure)),
        }
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// A client configuration that talks only to the certificate `trust` names (or, for a probe, to
/// any, recording it). HTTP/1.1 only, like the hub.
pub fn client_config_with(verifier: Arc<PinVerifier>) -> rustls::ClientConfig {
    let mut cfg = rustls::ClientConfig::builder_with_provider(verifier.provider.clone())
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    cfg
}

/// [`client_config_with`] for exactly the certificate `pin`.
pub fn client_config(pin: Pin) -> rustls::ClientConfig {
    client_config_with(PinVerifier::new(Trust::Pinned(pin)))
}

/// The name rustls is given for `host` (an IP address, else a DNS name; neither is checked).
fn server_name(host: &str) -> ServerName<'static> {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return ServerName::IpAddress(ip.into());
    }
    ServerName::try_from(bare.to_string()).unwrap_or_else(|_| ServerName::try_from("concord").expect("a valid name"))
}

/// What answers at an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// A TLS server with this certificate (the handshake completed, its signature checked).
    Tls(Pin),
    /// Something that does not speak TLS there (a hub older than 1.1, or `tls = "off"`).
    NoTls,
    /// Nothing answers (TCP), or the TLS handshake broke in a way that says nothing about what is
    /// there. The reason is for the log.
    Unreachable(String),
}

/// Connect to `host:port` and try a TLS handshake that accepts any certificate, then close: no
/// HTTP is sent. Bounded by [`CONNECT_TIMEOUT`] + [`HANDSHAKE_TIMEOUT`].
pub async fn probe(host: &str, port: u16) -> Probe {
    let tcp = match tcp_connect(host, port).await {
        Ok(t) => t,
        Err(e) => return Probe::Unreachable(format!("{e:#}")),
    };
    let verifier = PinVerifier::new(Trust::Tofu);
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config_with(verifier.clone())));
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, connector.connect(server_name(host), tcp)).await {
        Ok(Ok(mut s)) => {
            use tokio::io::AsyncWriteExt;
            let _ = s.shutdown().await;
            match verifier.seen() {
                Some(pin) => Probe::Tls(pin),
                None => Probe::Unreachable("TLS without a certificate".into()),
            }
        }
        // It showed a certificate but the handshake still failed (a bad signature...).
        Ok(Err(e)) if verifier.seen().is_some() => Probe::Unreachable(format!("TLS handshake failed: {e}")),
        // Plain HTTP (an older hub answers a TLS hello with "400 Bad Request"), or a reset.
        Ok(Err(e)) => {
            debug!(host, port, error = %e, "no TLS there");
            Probe::NoTls
        }
        Err(_) => Probe::NoTls,
    }
}

/// [`probe`] for the host and port of a hub URL (`http://host:port`).
pub async fn probe_url(url: &str) -> Probe {
    match host_port(url) {
        Ok((host, port)) => probe(&host, port).await,
        Err(e) => Probe::Unreachable(format!("{e:#}")),
    }
}

/// The host (IPv6 without brackets) and port of a URL.
pub fn host_port(url: &str) -> Result<(String, u16)> {
    let u = Url::parse(url).with_context(|| format!("invalid url {url:?}"))?;
    let host = match u.host() {
        Some(url::Host::Ipv6(ip)) => ip.to_string(),
        Some(h) => h.to_string(),
        None => return Err(anyhow!("url {url:?} has no host")),
    };
    let port = u.port_or_known_default().ok_or_else(|| anyhow!("url {url:?} has no port"))?;
    Ok((host, port))
}

async fn tcp_connect(host: &str, port: u16) -> Result<TcpStream> {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let s = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((bare, port))).await {
        Ok(r) => r.with_context(|| format!("tcp connect to {host}:{port}"))?,
        Err(_) => return Err(anyhow!("tcp connect to {host}:{port} timed out")),
    };
    let _ = s.set_nodelay(true);
    Ok(s)
}

/// Open a pinned TLS connection to `host:port`.
pub async fn connect_pinned(host: &str, port: u16, pin: Pin) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let tcp = tcp_connect(host, port).await?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config(pin)));
    match tokio::time::timeout(HANDSHAKE_TIMEOUT * 3, connector.connect(server_name(host), tcp)).await {
        Ok(r) => r.with_context(|| format!("TLS to {host}:{port}")),
        Err(_) => Err(anyhow!("TLS to {host}:{port} timed out")),
    }
}

// ---------------------------------------------------------------- the LiveKit tunnel

/// `ws://127.0.0.1:<port>` for the LiveKit SDK, each connection carried in pinned TLS to the hub
/// (whose port splices `/rtc` to LiveKit). Lives as long as the voice session (the SDK reconnects
/// and asks `/rtc/validate` through it too); dropping it closes it and every connection in it.
pub struct Tunnel {
    local: SocketAddr,
    target: String,
    task: JoinHandle<()>,
}

impl Tunnel {
    /// Listen on `127.0.0.1:0` for `wss_url` (`wss://host:port`, the hub's).
    pub async fn open(wss_url: &str, pin: Pin) -> Result<Tunnel> {
        let (host, port) = host_port(wss_url)?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.context("local voice tunnel")?;
        let local = listener.local_addr()?;
        let target = format!("{host}:{port}");
        info!(%local, %target, "voice tunnel open (pinned TLS to the hub)");
        let task = tokio::spawn(async move {
            let mut conns = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((mut inner, peer)) = accepted else {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            continue;
                        };
                        // Only this PC's own SDK uses it (it listens on loopback only).
                        let _ = inner.set_nodelay(true);
                        let host = host.clone();
                        conns.spawn(async move {
                            match connect_pinned(&host, port, pin).await {
                                Ok(mut outer) => {
                                    if let Err(e) = tokio::io::copy_bidirectional(&mut inner, &mut outer).await {
                                        debug!(%peer, error = %e, "voice tunnel connection ended");
                                    }
                                }
                                Err(e) => warn!(error = %format!("{e:#}"), "voice tunnel: cannot reach the hub"),
                            }
                        });
                    }
                    Some(_) = conns.join_next(), if !conns.is_empty() => {}
                }
            }
        });
        Ok(Tunnel { local, target, task })
    }

    /// The URL to give the LiveKit SDK.
    pub fn url(&self) -> String {
        format!("ws://{}", self.local)
    }

    /// `host:port` of the hub it leads to.
    pub fn target(&self) -> &str {
        &self.target
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.task.abort();
        debug!(target = %self.target, "voice tunnel closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::internal::msgs::codec::{Codec, Reader};

    /// A self-signed P-256 certificate like the hub's, with its key.
    fn cert() -> (CertificateDer<'static>, rcgen::KeyPair) {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let cert = rcgen::CertificateParams::new(vec!["concord".to_string()]).unwrap().self_signed(&key).unwrap();
        (cert.der().clone(), key)
    }

    fn dss(scheme: SignatureScheme, sig: &[u8]) -> DigitallySignedStruct {
        let mut bytes = Vec::new();
        scheme.encode(&mut bytes);
        bytes.extend_from_slice(&(sig.len() as u16).to_be_bytes());
        bytes.extend_from_slice(sig);
        DigitallySignedStruct::read(&mut Reader::init(&bytes)).unwrap()
    }

    fn sign(key: &rcgen::KeyPair, msg: &[u8]) -> Vec<u8> {
        use rcgen::SigningKey;
        key.sign(msg).unwrap()
    }

    #[test]
    fn only_the_pinned_certificate_passes() {
        let (a, _) = cert();
        let (b, _) = cert();
        let name = ServerName::try_from("whatever.example").unwrap();
        let v = PinVerifier::new(Trust::Pinned(pin_of(&a)));
        assert!(v.verify_server_cert(&a, &[], &name, &[], UnixTime::now()).is_ok());
        assert_eq!(v.seen(), Some(pin_of(&a)));
        assert!(v.verify_server_cert(&b, &[], &name, &[], UnixTime::now()).is_err(), "another certificate");
        assert_eq!(v.seen(), Some(pin_of(&b)), "the refused one is recorded");
        // Any name and any time: the pin is the identity.
        let ip = ServerName::IpAddress(IpAddr::from([203, 0, 113, 9]).into());
        assert!(v.verify_server_cert(&a, &[], &ip, &[], UnixTime::since_unix_epoch(Duration::from_secs(5_000_000_000))).is_ok());
        let tofu = PinVerifier::new(Trust::Tofu);
        assert!(tofu.verify_server_cert(&b, &[], &name, &[], UnixTime::now()).is_ok());
        assert_eq!(tofu.seen(), Some(pin_of(&b)));
    }

    /// The verifier really checks the handshake signature (it delegates to rustls + ring).
    #[test]
    fn signatures_are_really_checked() {
        let (a, key) = cert();
        let (b, _) = cert();
        let v = PinVerifier::new(Trust::Pinned(pin_of(&a)));
        let msg = b"the handshake transcript";
        let good = sign(&key, msg);
        let scheme = SignatureScheme::ECDSA_NISTP256_SHA256;
        assert!(v.verify_tls13_signature(msg, &a, &dss(scheme, &good)).is_ok());
        assert!(v.verify_tls12_signature(msg, &a, &dss(scheme, &good)).is_ok());
        // A flipped bit, another message, another key's certificate, a scheme that does not fit,
        // garbage: all refused.
        let mut bad = good.clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 0x01;
        for (m, c, d, what) in [
            (&msg[..], &a, dss(scheme, &bad), "flipped bit"),
            (&b"another transcript"[..], &a, dss(scheme, &good), "another message"),
            (&msg[..], &b, dss(scheme, &good), "another certificate's key"),
            (&msg[..], &a, dss(SignatureScheme::ED25519, &good), "wrong scheme"),
            (&msg[..], &a, dss(SignatureScheme::ECDSA_NISTP384_SHA384, &good), "wrong curve"),
            (&msg[..], &a, dss(scheme, &[0u8; 8]), "garbage"),
            (&msg[..], &a, dss(scheme, &[]), "empty"),
        ] {
            assert!(v.verify_tls13_signature(m, c, &d).is_err(), "tls13 accepted: {what}");
            assert!(v.verify_tls12_signature(m, c, &d).is_err(), "tls12 accepted: {what}");
        }
        assert!(v.supported_verify_schemes().contains(&scheme));
    }

    /// A TLS server with `cert`/`key` on loopback answering one plain-text line per connection.
    async fn tls_server(cert: CertificateDer<'static>, key: &rcgen::KeyPair) -> u16 {
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into());
        let cfg = rustls::ServerConfig::builder_with_provider(provider()).with_safe_default_protocol_versions().unwrap().with_no_client_auth().with_single_cert(vec![cert], key).unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(cfg));
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((s, _)) = l.accept().await else { return };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    if let Ok(mut s) = acceptor.accept(s).await {
                        let mut b = [0u8; 64];
                        if let Ok(n) = s.read(&mut b).await {
                            let _ = s.write_all(&b[..n]).await;
                        }
                    }
                });
            }
        });
        port
    }

    /// A plain server that answers anything with an HTTP 400, like hyper does to a TLS hello.
    async fn plain_server() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut b = [0u8; 512];
                    let _ = s.read(&mut b).await;
                    let _ = s.write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
                });
            }
        });
        port
    }

    #[test]
    fn probes_and_pinned_connections() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (a, key) = cert();
            let (b, _) = cert();
            let port = tls_server(a.clone(), &key).await;
            assert_eq!(probe("127.0.0.1", port).await, Probe::Tls(pin_of(&a)));
            assert_eq!(probe_url(&format!("http://127.0.0.1:{port}")).await, Probe::Tls(pin_of(&a)));
            let old = plain_server().await;
            let t = std::time::Instant::now();
            assert_eq!(probe("127.0.0.1", old).await, Probe::NoTls);
            assert!(t.elapsed() < Duration::from_secs(2), "an older hub is told apart at once");
            // A closed port: nothing there.
            let closed = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
            assert!(matches!(probe("127.0.0.1", closed).await, Probe::Unreachable(_)));
            // Pinned: the right certificate talks, another one never gets a byte.
            let mut s = connect_pinned("127.0.0.1", port, pin_of(&a)).await.unwrap();
            s.write_all(b"hello").await.unwrap();
            let mut got = [0u8; 5];
            s.read_exact(&mut got).await.unwrap();
            assert_eq!(&got, b"hello");
            assert!(connect_pinned("127.0.0.1", port, pin_of(&b)).await.is_err());
            assert!(connect_pinned("127.0.0.1", old, pin_of(&a)).await.is_err());

            // The tunnel: plain on loopback, pinned TLS to the "hub".
            let tunnel = Tunnel::open(&format!("wss://127.0.0.1:{port}"), pin_of(&a)).await.unwrap();
            assert!(tunnel.url().starts_with("ws://127.0.0.1:"));
            let mut c = TcpStream::connect(tunnel.local).await.unwrap();
            c.write_all(b"via tunnel").await.unwrap();
            let mut got = [0u8; 10];
            c.read_exact(&mut got).await.unwrap();
            assert_eq!(&got, b"via tunnel");
            // Pinned to another certificate: the tunnel closes the connection without a reply.
            let wrong = Tunnel::open(&format!("wss://127.0.0.1:{port}"), pin_of(&b)).await.unwrap();
            let mut c = TcpStream::connect(wrong.local).await.unwrap();
            let _ = c.write_all(b"secret").await;
            let mut buf = [0u8; 8];
            let n = tokio::time::timeout(Duration::from_secs(10), c.read(&mut buf)).await.unwrap().unwrap_or(0);
            assert_eq!(n, 0);
            // Dropped: no longer listening.
            let at = tunnel.local;
            drop(tunnel);
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(TcpStream::connect(at).await.is_err());
        });
    }

    /// Shows `cert` but signs the handshake with `key` (another one): someone who copied the
    /// hub's public certificate without its private key.
    #[derive(Debug)]
    struct CertWithoutItsKey(Arc<rustls::sign::CertifiedKey>);

    impl rustls::server::ResolvesServerCert for CertWithoutItsKey {
        fn resolve(&self, _: rustls::server::ClientHello<'_>) -> Option<Arc<rustls::sign::CertifiedKey>> {
            Some(self.0.clone())
        }
    }

    /// Audit (T0), end to end: a server that presents the PINNED certificate but does not hold its
    /// key cannot finish a handshake with a pinned client, in TLS 1.3 nor 1.2 (this fails if
    /// `verify_tls1x_signature` stops delegating to rustls), and a probe does not take it for the
    /// hub either. Extra (intermediate) certificates after another end-entity do not help.
    #[test]
    fn the_pinned_certificate_without_its_key_is_refused() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (a, _key_a) = cert();
            let (b, key_b) = cert();
            let signer = provider().key_provider.load_private_key(rustls::pki_types::PrivateKeyDer::Pkcs8(key_b.serialize_der().into())).unwrap();
            let serve = |chain: Vec<CertificateDer<'static>>, versions: &'static [&'static rustls::SupportedProtocolVersion]| {
                let signer = signer.clone();
                async move {
                    let resolver = Arc::new(CertWithoutItsKey(Arc::new(rustls::sign::CertifiedKey::new(chain, signer))));
                    let cfg = rustls::ServerConfig::builder_with_provider(provider()).with_protocol_versions(versions).unwrap().with_no_client_auth().with_cert_resolver(resolver);
                    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(cfg));
                    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let port = l.local_addr().unwrap().port();
                    tokio::spawn(async move {
                        while let Ok((s, _)) = l.accept().await {
                            let acceptor = acceptor.clone();
                            tokio::spawn(async move {
                                if let Ok(mut s) = acceptor.accept(s).await {
                                    let mut buf = [0u8; 64];
                                    let n = s.read(&mut buf).await.unwrap_or(0);
                                    let _ = s.write_all(&buf[..n]).await;
                                }
                            });
                        }
                    });
                    port
                }
            };
            static V13: &[&rustls::SupportedProtocolVersion] = &[&rustls::version::TLS13];
            static V12: &[&rustls::SupportedProtocolVersion] = &[&rustls::version::TLS12];
            for versions in [V13, V12] {
                let port = serve(vec![a.clone()], versions).await;
                assert!(connect_pinned("127.0.0.1", port, pin_of(&a)).await.is_err(), "{versions:?}: a handshake signed by another key");
                assert!(matches!(probe("127.0.0.1", port).await, Probe::Unreachable(_)), "a probe must not report the pin of a server that cannot sign for it");
                // Control: the same server with the matching certificate (b, signed by b) works.
                let ok = serve(vec![b.clone()], versions).await;
                let mut s = connect_pinned("127.0.0.1", ok, pin_of(&b)).await.unwrap();
                s.write_all(b"hi").await.unwrap();
                let mut got = [0u8; 2];
                s.read_exact(&mut got).await.unwrap();
            }
            // The pinned certificate sent as an "intermediate" behind another end-entity: refused.
            let port = serve(vec![b.clone(), a.clone()], V13).await;
            assert!(connect_pinned("127.0.0.1", port, pin_of(&a)).await.is_err());
        });
    }

    #[test]
    fn urls_and_names() {
        assert_eq!(host_port("http://[2001:db8::1]:7890").unwrap(), ("2001:db8::1".to_string(), 7890));
        assert_eq!(host_port("wss://example.org:7997").unwrap(), ("example.org".to_string(), 7997));
        assert_eq!(host_port("https://example.org").unwrap().1, 443);
        assert!(matches!(server_name("[::1]"), ServerName::IpAddress(_)));
        assert!(matches!(server_name("192.0.2.1"), ServerName::IpAddress(_)));
        assert!(matches!(server_name("meu_servidor.local"), ServerName::DnsName(_)));
    }
}
