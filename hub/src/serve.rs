//! The HTTP listener: hyper's HTTP/1 server with the limits `axum::serve` does not set.
//!
//! `axum::serve` gives hyper no timer, so hyper's header-read timeout silently never fires, and
//! it takes every connection it is offered. On a home PC open to the internet that lets anybody,
//! without logging in, hold any number of sockets (each with up to ~400 KiB of half-sent request
//! head) forever. Here:
//!
//! - **Header-read / idle timeout** ([`Limits::header_read_timeout`]): a request head must arrive
//!   in full within this time, and an idle keep-alive connection is closed after it (hyper starts
//!   the same timer while it waits for the next request).
//! - **Smaller read buffer** ([`Limits::max_buf`]) for request heads.
//! - **Connection caps**, global and per address ([`Limits::max_conns`],
//!   [`Limits::max_conns_per_ip`]): one more is closed right after `accept`. A WebSocket keeps its
//!   connection's slot for as long as it lives (the [`ConnSlot`] travels with the upgrade, see
//!   `ws.rs`), so long-lived sockets count too.
//! - **Request timeout** ([`Limits::request_timeout`], [`Limits::login_timeout`]): the time to
//!   answer one request (body upload included), applied by [`timeout_layer`].
//!
//! v1.0.2: the hub may listen on several sockets (IPv4 and IPv6, `listen.rs`): each runs its own
//! [`serve`] with the same [`Slots`], so the caps count all of them together. An IPv4 peer on a
//! dual-stack socket is turned back into plain IPv4 here ([`crate::listen::canonical_peer`]), and
//! an IPv6 peer counts against the per-address cap by its /64 network
//! ([`crate::listen::peer_group`]).
//!
//! v1.1 (T0, `tls.rs`): after taking its slot, a connection's first byte is peeked (within the
//! header-read timeout): `0x16` starts a TLS handshake (bounded by [`TLS_HANDSHAKE_TIMEOUT`], the
//! slot held all along), anything else is plain HTTP as before. On TLS, the first request line is
//! read (at most [`FIRST_LINE_MAX`] bytes, same timeout): LiveKit's `/rtc...` is spliced to the
//! signal port on loopback for as long as it lives (slot held, closed on shutdown); everything
//! else goes to the router with [`crate::tls::Transport::Tls`]. A plain connection from another
//! computer under `tls = "required"` gets a `426` and is closed.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{ConnectInfo, Request};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tower::ServiceExt;

use crate::tls::{HubTls, Policy, Prefixed, Transport};

/// A TLS handshake must be done within this (and within the header-read timeout).
pub const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// The first request line of a TLS connection must fit in this many bytes.
pub const FIRST_LINE_MAX: usize = 8 * 1024;

/// TLS on the listener (`tls.rs`): the certificate (none with `tls = "off"`), whether plain HTTP
/// from other computers is still served, and where LiveKit's signalling is.
#[derive(Clone)]
pub struct TlsSetup {
    pub tls: Option<HubTls>,
    pub policy: Policy,
    pub signal_port: u16,
}

impl TlsSetup {
    /// No TLS at all (the hub of before 1.1; tests of the plain listener).
    pub fn off() -> TlsSetup {
        TlsSetup { tls: None, policy: Policy::new(crate::tls::TlsMode::Off), signal_port: 0 }
    }
}

/// The listener's limits.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Connections open at once, all addresses together.
    pub max_conns: usize,
    /// Connections open at once from one IP address.
    pub max_conns_per_ip: usize,
    /// A request head must arrive within this; also how long an idle keep-alive connection stays.
    pub header_read_timeout: Duration,
    /// Time to answer one request (reading its body included).
    pub request_timeout: Duration,
    /// Same, for the unauthenticated `POST /api/login` and `GET /api/health`.
    pub login_timeout: Duration,
    /// hyper's read buffer (request heads must fit in it).
    pub max_buf: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_conns: 1024,
            max_conns_per_ip: 128,
            header_read_timeout: Duration::from_secs(20),
            request_timeout: Duration::from_secs(120),
            login_timeout: Duration::from_secs(20),
            max_buf: 64 * 1024,
        }
    }
}

#[derive(Default)]
struct Counts {
    total: usize,
    per_ip: HashMap<IpAddr, usize>,
}

/// Who holds which connection slots.
#[derive(Clone)]
pub struct Slots {
    limits: Arc<Limits>,
    counts: Arc<Mutex<Counts>>,
}

/// One open connection's place in the caps; freed when the last clone is dropped (the
/// connection closed, or the WebSocket it became ended).
#[derive(Clone)]
pub struct ConnSlot(#[allow(dead_code)] Arc<SlotInner>);

struct SlotInner {
    ip: IpAddr,
    counts: Arc<Mutex<Counts>>,
}

impl Drop for SlotInner {
    fn drop(&mut self) {
        let mut c = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        c.total = c.total.saturating_sub(1);
        if let Some(n) = c.per_ip.get_mut(&self.ip) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                c.per_ip.remove(&self.ip);
            }
        }
    }
}

impl Slots {
    pub fn new(limits: Limits) -> Self {
        Slots { limits: Arc::new(limits), counts: Arc::default() }
    }

    /// A slot for a new connection from `ip`, or `None` when a cap is reached.
    pub fn take(&self, ip: IpAddr) -> Option<ConnSlot> {
        let ip = crate::listen::peer_group(ip);
        let mut c = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        let mine = c.per_ip.get(&ip).copied().unwrap_or(0);
        if c.total >= self.limits.max_conns || mine >= self.limits.max_conns_per_ip {
            return None;
        }
        c.total += 1;
        *c.per_ip.entry(ip).or_default() += 1;
        Some(ConnSlot(Arc::new(SlotInner { ip, counts: self.counts.clone() })))
    }

    /// Connections open right now (tests).
    pub fn open(&self) -> usize {
        self.counts.lock().unwrap_or_else(|e| e.into_inner()).total
    }
}

/// Request timeout middleware (see [`Limits`]).
pub async fn timeout_layer(limits: Arc<Limits>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let limit = if path == "/api/login" || path == "/api/health" { limits.login_timeout } else { limits.request_timeout };
    match tokio::time::timeout(limit, next.run(req)).await {
        Ok(r) => r,
        Err(_) => (StatusCode::REQUEST_TIMEOUT, "request timed out").into_response(),
    }
}

/// Accept and serve connections until `shutdown`; then let the open ones finish (bounded). Plain
/// HTTP only (see [`serve_with`]).
pub async fn serve(listener: TcpListener, app: Router, slots: Slots, shutdown: watch::Receiver<bool>) {
    serve_with(listener, app, slots, shutdown, Arc::new(TlsSetup::off())).await
}

/// [`serve`] with TLS on the same port (v1.1).
pub async fn serve_with(listener: TcpListener, app: Router, slots: Slots, mut shutdown: watch::Receiver<bool>, tls: Arc<TlsSetup>) {
    let limits = slots.limits.clone();
    // Every connection task holds a sender; all of them gone = all connections closed.
    let (alive_tx, mut alive_rx) = mpsc::channel::<Infallible>(1);
    let mut refused: u64 = 0;
    loop {
        let (stream, peer) = tokio::select! {
            r = listener.accept() => match r {
                Ok(x) => x,
                Err(e) => {
                    // Out of sockets / handles: back off a little instead of spinning.
                    tracing::debug!("accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
            _ = crate::state::wait_shutdown(&mut shutdown) => break,
        };
        let peer = crate::listen::canonical_peer(peer);
        let Some(slot) = slots.take(peer.ip()) else {
            refused += 1;
            if refused.is_power_of_two() {
                tracing::warn!(%peer, refused, "connection refused: too many open connections");
            }
            drop(stream);
            continue;
        };
        let _ = stream.set_nodelay(true);
        let app = app.clone();
        let (limits, alive, stop, tls) = (limits.clone(), alive_tx.clone(), shutdown.clone(), tls.clone());
        tokio::spawn(async move {
            let _alive = alive;
            connection(stream, peer, slot.clone(), app, limits, stop, tls).await;
            drop(slot);
        });
    }
    drop(alive_tx);
    let _ = tokio::time::timeout(Duration::from_secs(5), alive_rx.recv()).await;
}

/// What a connection starts with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sniffed {
    /// A TLS record (`0x16`, handshake).
    Tls,
    Plain,
    /// Closed, or nothing within the time.
    Nothing,
}

/// Peek the first byte (nothing is consumed).
pub async fn sniff(stream: &TcpStream, limit: Duration) -> Sniffed {
    let mut b = [0u8; 1];
    match tokio::time::timeout(limit, stream.peek(&mut b)).await {
        Ok(Ok(1)) if b[0] == 0x16 => Sniffed::Tls,
        Ok(Ok(1)) => Sniffed::Plain,
        _ => Sniffed::Nothing,
    }
}

/// One accepted connection, its slot held by the caller until this returns.
async fn connection(stream: TcpStream, peer: std::net::SocketAddr, slot: ConnSlot, app: Router, limits: Arc<Limits>, mut stop: watch::Receiver<bool>, tls: Arc<TlsSetup>) {
    let Some(cert) = tls.tls.clone() else {
        // `tls = "off"`: exactly the listener of before.
        return http(stream, peer, slot, app, &limits, stop, Transport::Plain).await;
    };
    match sniff(&stream, limits.header_read_timeout).await {
        Sniffed::Nothing => {}
        Sniffed::Plain if tls.policy.refuses_plain(peer.ip()) => {
            tracing::debug!(%peer, "plain connection refused: this server requires TLS");
            tls_required(stream).await
        }
        Sniffed::Plain => http(stream, peer, slot, app, &limits, stop, Transport::Plain).await,
        Sniffed::Tls => {
            // Audit (T0): the handshake and the first request line together within one
            // header-read timeout (what a plain request head gets), the handshake itself within
            // TLS_HANDSHAKE_TIMEOUT; a hub stopping drops them at once.
            let deadline = tokio::time::Instant::now() + limits.header_read_timeout;
            let opened = async {
                let handshake = TLS_HANDSHAKE_TIMEOUT.min(limits.header_read_timeout);
                let mut s = match tokio::time::timeout(handshake, cert.acceptor.accept(stream)).await {
                    Ok(Ok(s)) => s,
                    Ok(Err(e)) => {
                        tracing::debug!(%peer, "TLS handshake failed: {e}");
                        return None;
                    }
                    Err(_) => {
                        tracing::debug!(%peer, "TLS handshake timed out");
                        return None;
                    }
                };
                match tokio::time::timeout_at(deadline, first_line(&mut s)).await {
                    Ok((head, target)) => Some((s, head, target)),
                    Err(_) => {
                        tracing::trace!(%peer, "no request line in time");
                        None
                    }
                }
            };
            let opened = tokio::select! {
                o = opened => o,
                _ = crate::state::wait_shutdown(&mut stop) => None,
            };
            let Some((s, head, target)) = opened else { return };
            if target.as_deref().is_some_and(crate::tls::is_rtc_path) {
                splice(s, head, peer, tls.signal_port, limits.header_read_timeout, &mut stop).await
            } else if !head.is_empty() {
                http(Prefixed::new(head, s), peer, slot, app, &limits, stop, Transport::Tls).await
            }
        }
    }
}

/// Read until the first request line is complete (at most [`FIRST_LINE_MAX`] bytes): the bytes
/// read, and the request target when the line parsed.
async fn first_line<S: AsyncRead + Unpin>(s: &mut S) -> (Vec<u8>, Option<String>) {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let room = FIRST_LINE_MAX - buf.len();
        let n = match s.read(&mut chunk[..room.min(1024)]).await {
            Ok(0) | Err(_) => return (buf, None),
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        if let Some(target) = crate::tls::request_target(&buf) {
            return (buf, target);
        }
        if buf.len() >= FIRST_LINE_MAX {
            // hyper refuses it with its own answer.
            return (buf, None);
        }
    }
}

/// The whole head of the request spliced to LiveKit (request line with its access token, headers)
/// and the head of LiveKit's answer must each fit in this.
pub const RTC_HEAD_MAX: usize = 16 * 1024;

/// LiveKit's signalling, over the hub's TLS: ONE request goes to the signal port on this PC (with
/// the bytes already read), and LiveKit's answer comes back. Only when that answer is `101` (the
/// signalling WebSocket) are the two connections joined for good, until either side closes or
/// the hub stops; any other answer ends the connection after its body.
///
/// Audit (T0): a plain byte splice from the first request on let a second, pipelined or
/// keep-alive request on the same connection (`GET /rtc ...` then `POST /twirp/...`) reach
/// LiveKit's other routes, against "never `/twirp`". So the first request must be a whole,
/// body-less `GET` of an `/rtc` path, nothing after it is forwarded before the `101`, and nothing
/// at all after another answer.
async fn splice<S: AsyncRead + AsyncWrite + Unpin>(mut tls: S, mut head: Vec<u8>, peer: std::net::SocketAddr, signal_port: u16, limit: Duration, stop: &mut watch::Receiver<bool>) {
    let end = match tokio::time::timeout(limit, read_head(&mut tls, &mut head)).await {
        Ok(Some(end)) => end,
        _ => return refuse(&mut tls, "400 Bad Request").await,
    };
    if !is_rtc_get(&head[..end]) {
        tracing::debug!(%peer, "voice signalling: not a body-less GET of /rtc, refused");
        return refuse(&mut tls, "400 Bad Request").await;
    }
    // Bytes the client sent after this request's head: forwarded only once it is a WebSocket.
    let early = head.split_off(end);
    let up = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, signal_port))).await;
    let mut up = match up {
        Ok(Ok(u)) => u,
        _ => {
            tracing::debug!(%peer, signal_port, "voice server not reachable on loopback");
            return refuse(&mut tls, "503 Service Unavailable").await;
        }
    };
    let _ = up.set_nodelay(true);
    if up.write_all(&head).await.is_err() {
        return;
    }
    let mut answer = Vec::new();
    let answer_end = match tokio::time::timeout(limit, read_head(&mut up, &mut answer)).await {
        Ok(Some(end)) => end,
        _ => return refuse(&mut tls, "502 Bad Gateway").await,
    };
    let (status, body_len) = answer_info(&answer[..answer_end]);
    if tls.write_all(&answer).await.is_err() {
        return;
    }
    if status != Some(101) {
        // One answer (e.g. `/rtc/validate`, a refused token), its body, and the connection ends.
        let have = (answer.len() - answer_end) as u64;
        let body = async {
            match body_len {
                Some(n) => {
                    let _ = tokio::io::copy(&mut (&mut up).take(n.saturating_sub(have)), &mut tls).await;
                }
                None => loop {
                    let mut chunk = [0u8; 4096];
                    match tokio::time::timeout(Duration::from_secs(2), up.read(&mut chunk)).await {
                        Ok(Ok(n)) if n > 0 => {
                            if tls.write_all(&chunk[..n]).await.is_err() {
                                break;
                            }
                        }
                        _ => break,
                    }
                },
            }
        };
        let _ = tokio::time::timeout(limit, body).await;
        let _ = tls.shutdown().await;
        return;
    }
    if !early.is_empty() && up.write_all(&early).await.is_err() {
        return;
    }
    tracing::debug!(%peer, "voice signalling spliced to the voice server");
    tokio::select! {
        r = tokio::io::copy_bidirectional(&mut tls, &mut up) => {
            if let Err(e) = r {
                tracing::trace!(%peer, "voice signalling splice ended: {e}");
            }
        }
        _ = crate::state::wait_shutdown(stop) => {}
    }
}

/// Read into `buf` (which may already hold some) until it holds a whole HTTP head (`\r\n\r\n`):
/// its length, or `None` (closed, or more than [`RTC_HEAD_MAX`]).
async fn read_head<R: AsyncRead + Unpin>(r: &mut R, buf: &mut Vec<u8>) -> Option<usize> {
    let mut chunk = [0u8; 2048];
    loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            return Some(i + 4);
        }
        if buf.len() >= RTC_HEAD_MAX {
            return None;
        }
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// A whole request head that is a `GET` of an `/rtc` path with no body.
pub fn is_rtc_get(head: &[u8]) -> bool {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    if !matches!(req.parse(head), Ok(httparse::Status::Complete(n)) if n == head.len()) {
        return false;
    }
    req.method == Some("GET")
        && req.path.is_some_and(crate::tls::is_rtc_path)
        && !req.headers.iter().any(|h| h.name.eq_ignore_ascii_case("transfer-encoding") || (h.name.eq_ignore_ascii_case("content-length") && h.value.trim_ascii() != b"0"))
}

/// The status of an answer head and its `content-length`, when it has one.
fn answer_info(head: &[u8]) -> (Option<u16>, Option<u64>) {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut resp = httparse::Response::new(&mut headers);
    if resp.parse(head).is_err() {
        return (None, None);
    }
    let len = resp.headers.iter().find(|h| h.name.eq_ignore_ascii_case("content-length")).and_then(|h| std::str::from_utf8(h.value).ok()?.trim().parse().ok());
    (resp.code, len)
}

/// A short refusal, then the connection closes.
async fn refuse<S: AsyncWrite + Unpin>(s: &mut S, status: &str) {
    let _ = s.write_all(format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").as_bytes()).await;
    let _ = s.shutdown().await;
}

/// `tls = "required"`: a plain request from another computer gets the `426` an older app shows.
async fn tls_required(stream: TcpStream) {
    let svc = hyper::service::service_fn(|_req: hyper::Request<hyper::body::Incoming>| async {
        let body = Json(vox_proto::ApiError { error: vox_proto::ERR_TLS_REQUIRED.to_string() });
        Ok::<_, Infallible>((StatusCode::UPGRADE_REQUIRED, [(header::CONNECTION, "close")], body).into_response())
    });
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder.timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(10)).max_buf_size(16 * 1024).keep_alive(false);
    let _ = tokio::time::timeout(Duration::from_secs(15), builder.serve_connection(TokioIo::new(stream), svc)).await;
}

/// Serve HTTP/1 (and its WebSocket upgrades) on one connection.
async fn http<S>(io: S, peer: std::net::SocketAddr, slot: ConnSlot, app: Router, limits: &Limits, mut stop: watch::Receiver<bool>, transport: Transport)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let svc = hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
        req.extensions_mut().insert(ConnectInfo(peer));
        req.extensions_mut().insert(slot.clone());
        req.extensions_mut().insert(transport);
        app.clone().oneshot(req.map(axum::body::Body::new))
    });
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_read_timeout)
        .max_buf_size(limits.max_buf.max(8192))
        .keep_alive(true);
    let conn = builder.serve_connection(TokioIo::new(io), svc).with_upgrades();
    tokio::pin!(conn);
    tokio::select! {
        r = conn.as_mut() => {
            if let Err(e) = r {
                tracing::trace!(%peer, "connection ended: {e}");
            }
        }
        _ = crate::state::wait_shutdown(&mut stop) => {
            conn.as_mut().graceful_shutdown();
            let _ = tokio::time::timeout(Duration::from_secs(5), conn.as_mut()).await;
        }
    }
}
