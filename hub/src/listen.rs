//! Where the hub listens (v1.0.2): IPv4 and IPv6.
//!
//! Before v1.0.2 the hub only listened on IPv4 (`0.0.0.0:7890`), although invites could carry an
//! IPv6 address: friends with IPv6 (most home connections in Brazil, which get past the
//! provider's CGNAT that way) timed out. Now an every-interface `bind` (`[::]` or `0.0.0.0`, see
//! [`plan`]) listens on one dual-stack socket (`[::]` with `IPV6_V6ONLY` off), or, where the
//! system cannot do that, on two sockets (`0.0.0.0` and `[::]`). A specific address is used
//! exactly as written, and `ipv6 = false` in `config.toml` keeps the old IPv4-only behaviour.
//!
//! On a dual-stack socket IPv4 peers show up as IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`):
//! [`canonical_peer`] turns them back into plain IPv4 before anything sees them (`serve.rs`), so
//! the loopback rule ("quem hospeda vira dono"), the logs and the per-address limits stay as they
//! were. [`peer_group`] is what the per-address limits count: an IPv6 peer by its /64 network
//! (one home connection gets a whole /64, so counting single addresses would let one person
//! appear as millions).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::{Context, Result};
use tokio::net::TcpListener;

/// How to listen for a `bind` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Every interface, IPv4 and IPv6, on this port (`0.0.0.0:p` or `[::]:p` with `ipv6 = true`).
    DualStack(u16),
    /// Exactly this address: a specific IP, or an every-interface IPv4 bind with `ipv6 = false`.
    Exact(SocketAddr),
    /// A host name (`localhost:7890`): resolved by the system, as before v1.0.2.
    Named(String),
}

impl Plan {
    /// Does this plan take IPv6 connections (from other machines)?
    pub fn has_ipv6(&self) -> bool {
        match self {
            Plan::DualStack(_) => true,
            Plan::Exact(a) => a.is_ipv6() && !a.ip().is_loopback(),
            Plan::Named(_) => false,
        }
    }
}

/// The plan for `bind` (from `config.toml`, or a test's override) and the `ipv6` switch.
pub fn plan(bind: &str, ipv6: bool) -> Plan {
    let bind = bind.trim();
    match bind.parse::<SocketAddr>() {
        Ok(a) if a.ip().is_unspecified() && ipv6 => Plan::DualStack(a.port()),
        Ok(a) if a.ip().is_unspecified() => Plan::Exact(SocketAddr::from((Ipv4Addr::UNSPECIFIED, a.port()))),
        Ok(a) => Plan::Exact(a),
        Err(_) => Plan::Named(bind.to_string()),
    }
}

/// The sockets a plan got.
pub struct Bound {
    pub listeners: Vec<TcpListener>,
    /// Where each listener listens (same order).
    pub addrs: Vec<SocketAddr>,
    /// One socket takes both IPv4 and IPv6 (`[::]` with `IPV6_V6ONLY` off).
    pub dual_stack: bool,
}

impl Bound {
    /// The addresses, for people: `[::]:7890 (IPv4 e IPv6)`, `0.0.0.0:7890 (IPv4)`...
    pub fn describe(&self) -> String {
        describe(&self.addrs, self.dual_stack)
    }
}

/// See [`Bound::describe`].
pub fn describe(addrs: &[SocketAddr], dual_stack: bool) -> String {
    let what = |a: &SocketAddr| match a {
        SocketAddr::V6(v6) if dual_stack && v6.ip().is_unspecified() => "IPv4 e IPv6",
        SocketAddr::V6(_) => "IPv6",
        SocketAddr::V4(_) => "IPv4",
    };
    addrs.iter().map(|a| format!("{a} ({})", what(a))).collect::<Vec<_>>().join(" e ")
}

/// Listen as `plan` says. Only the IPv4 side of a [`Plan::DualStack`] is required: without IPv6
/// on this PC (or with `[::]:port` taken by another program) the hub still starts, on IPv4, and
/// says why IPv6 is missing.
pub async fn bind(plan: &Plan) -> Result<Bound> {
    let bound = match plan {
        Plan::Exact(a) => one(TcpListener::bind(a).await?, false)?,
        Plan::Named(name) => one(TcpListener::bind(name.as_str()).await?, false)?,
        Plan::DualStack(port) => match v6_socket(Ipv6Addr::UNSPECIFIED, *port, false) {
            Ok(l) => one(TcpListener::from_std(l)?, true)?,
            Err(dual_err) => {
                let v4 = TcpListener::bind((Ipv4Addr::UNSPECIFIED, *port)).await?;
                let port = v4.local_addr()?.port();
                let mut b = one(v4, false)?;
                match v6_socket(Ipv6Addr::UNSPECIFIED, port, true).and_then(TcpListener::from_std) {
                    Ok(v6) => {
                        tracing::info!("one socket for IPv4 and IPv6 not possible ({dual_err}): two sockets");
                        b.addrs.push(v6.local_addr()?);
                        b.listeners.push(v6);
                    }
                    Err(e) => tracing::warn!("IPv6 not available ({dual_err}; {e}): listening on IPv4 only"),
                }
                b
            }
        },
    };
    Ok(bound)
}

fn one(l: TcpListener, dual_stack: bool) -> Result<Bound> {
    let addr = l.local_addr().context("listener address")?;
    Ok(Bound { listeners: vec![l], addrs: vec![addr], dual_stack })
}

/// A listening IPv6 socket (`[::]:port` for real), nonblocking. `only_v6 = false`: dual-stack
/// (IPv4 peers too, as IPv4-mapped addresses).
/// Like tokio's own `TcpListener::bind`: `SO_REUSEADDR` only outside Windows (on Windows it would
/// let another program take the port over), backlog 1024, not inherited by child processes
/// (LiveKit).
fn v6_socket(ip: Ipv6Addr, port: u16, only_v6: bool) -> std::io::Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    s.set_only_v6(only_v6)?;
    #[cfg(not(windows))]
    s.set_reuse_address(true)?;
    s.bind(&SocketAddr::from((ip, port)).into())?;
    s.listen(1024)?;
    s.set_nonblocking(true)?;
    Ok(s.into())
}

/// An IPv4 peer on a dual-stack socket (`[::ffff:a.b.c.d]:p`) as the plain IPv4 it is.
pub fn canonical_peer(peer: SocketAddr) -> SocketAddr {
    match peer {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::from((v4, v6.port())),
            None => peer,
        },
        v4 => v4,
    }
}

/// What per-address limits count: an IPv4 address as itself, an IPv6 one by its /64 network
/// (loopback as itself).
pub fn peer_group(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) if !v6.is_loopback() => {
            let s = v6.segments();
            IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapped_peers_become_ipv4_and_ipv6_counts_by_network() {
        let mapped: SocketAddr = "[::ffff:192.0.2.7]:5000".parse().unwrap();
        assert_eq!(canonical_peer(mapped), "192.0.2.7:5000".parse::<SocketAddr>().unwrap());
        let lo: SocketAddr = "[::ffff:127.0.0.1]:5000".parse().unwrap();
        assert!(canonical_peer(lo).ip().is_loopback(), "the host's own login stays a loopback one");
        let v6: SocketAddr = "[2001:db8:1:2:3:4:5:6]:9".parse().unwrap();
        assert_eq!(canonical_peer(v6), v6);
        assert_eq!(canonical_peer("[::1]:9".parse().unwrap()), "[::1]:9".parse::<SocketAddr>().unwrap());

        assert_eq!(peer_group("2001:db8:1:2:3:4:5:6".parse().unwrap()), "2001:db8:1:2::".parse::<IpAddr>().unwrap());
        assert_eq!(peer_group("2001:db8:1:2:ffff::1".parse().unwrap()), peer_group("2001:db8:1:2::9".parse().unwrap()));
        assert_ne!(peer_group("2001:db8:1:2::1".parse().unwrap()), peer_group("2001:db8:1:3::1".parse().unwrap()));
        assert_eq!(peer_group("192.0.2.7".parse().unwrap()), "192.0.2.7".parse::<IpAddr>().unwrap());
        assert_eq!(peer_group("::ffff:192.0.2.7".parse().unwrap()), "192.0.2.7".parse::<IpAddr>().unwrap());
        assert_eq!(peer_group("::1".parse().unwrap()), "::1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn plans_say_whether_they_take_ipv6() {
        assert!(plan("[::]:7890", true).has_ipv6());
        assert!(plan("0.0.0.0:7890", true).has_ipv6());
        assert!(!plan("0.0.0.0:7890", false).has_ipv6());
        assert!(!plan("[::]:7890", false).has_ipv6());
        assert!(plan("[2001:db8::1]:7890", true).has_ipv6());
        assert!(!plan("[::1]:7890", true).has_ipv6(), "loopback: nobody else reaches it");
        assert!(!plan("192.0.2.1:7890", true).has_ipv6());
        assert!(!plan("localhost:7890", true).has_ipv6());
    }

    #[test]
    fn descriptions_name_the_families() {
        let dual = ["[::]:7890".parse().unwrap()];
        assert_eq!(describe(&dual, true), "[::]:7890 (IPv4 e IPv6)");
        let two = ["0.0.0.0:7890".parse().unwrap(), "[::]:7890".parse().unwrap()];
        assert_eq!(describe(&two, false), "0.0.0.0:7890 (IPv4) e [::]:7890 (IPv6)");
    }

    /// Loopback only (a test must not open a port to the network): an explicit `[::1]` and
    /// `127.0.0.1` plan each take their own family.
    #[tokio::test]
    async fn exact_plans_bind_what_they_say() {
        let b = bind(&plan("[::1]:0", true)).await.unwrap();
        assert_eq!(b.addrs.len(), 1);
        assert!(b.addrs[0].is_ipv6() && b.addrs[0].port() != 0 && !b.dual_stack);
        let port = b.addrs[0].port();
        assert!(tokio::net::TcpStream::connect(("::1", port)).await.is_ok());
        assert!(tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err(), "an IPv6 address takes no IPv4");
        let b4 = bind(&plan("127.0.0.1:0", true)).await.unwrap();
        assert!(b4.addrs[0].is_ipv4() && !b4.dual_stack);
    }

    /// A dual-stack socket hands IPv4 peers over as `::ffff:a.b.c.d`; the hub's handlers see the
    /// plain IPv4 (so the host's own `127.0.0.1` login is still a loopback one). Bound to the
    /// IPv4-mapped loopback address: the same dual-stack socket as `[::]`, without opening a port
    /// to the network.
    #[tokio::test]
    async fn ipv4_peers_on_a_dual_stack_socket_look_like_ipv4() {
        use axum::extract::ConnectInfo;
        // reqwest has no provider of its own here (ring only); another test may not have set one yet.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let std_l = v6_socket(Ipv4Addr::LOCALHOST.to_ipv6_mapped(), 0, false).unwrap();
        let l = TcpListener::from_std(std_l).unwrap();
        let port = l.local_addr().unwrap().port();
        let app = axum::Router::new().route("/", axum::routing::get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move { peer.ip().to_string() }));
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let slots = crate::serve::Slots::new(crate::serve::Limits::default());
        let task = tokio::spawn(crate::serve::serve(l, app, slots, stop_rx));
        let body = reqwest::get(format!("http://127.0.0.1:{port}/")).await.unwrap().text().await.unwrap();
        assert_eq!(body, "127.0.0.1");
        let _ = stop_tx.send(true);
        let _ = task.await;
    }
}
