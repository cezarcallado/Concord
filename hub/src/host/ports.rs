//! The four ports of a hosted server, and the free block chosen when the usual ones are taken
//! (for example by the classic `concord-server.exe` running on this same PC).

use std::net::{Ipv4Addr, UdpSocket};

/// One server's ports: the hub (TCP), LiveKit's signal (TCP), and LiveKit's media over TCP and UDP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortBlock {
    pub hub: u16,
    pub signal: u16,
    pub rtc_tcp: u16,
    pub rtc_udp: u16,
}

/// Blocks tried, 100 ports apart: 7890/7880-7882 (the classic server's), 7990/7980-7982, ...
pub const BLOCKS: u16 = 10;
const STEP: u16 = 100;

impl PortBlock {
    pub const DEFAULT: PortBlock = PortBlock::nth(0);

    /// Block `k`: the default one shifted by `k * 100`.
    pub const fn nth(k: u16) -> PortBlock {
        let d = k * STEP;
        PortBlock { hub: 7890 + d, signal: 7880 + d, rtc_tcp: 7881 + d, rtc_udp: 7882 + d }
    }

    /// Which block this is, if it is one of [`BLOCKS`].
    pub fn index(&self) -> Option<u16> {
        (0..BLOCKS).find(|k| PortBlock::nth(*k) == *self)
    }

    /// The TCP ports (hub, signal, media).
    pub fn tcp(&self) -> [u16; 3] {
        [self.hub, self.signal, self.rtc_tcp]
    }

    /// `7990, 7980-7982` (for people).
    pub fn describe(&self) -> String {
        format!("{} (TCP), {}-{} (TCP) e {} (UDP)", self.hub, self.signal, self.rtc_tcp, self.rtc_udp)
    }
}

/// The block to use: `saved` (the one this server used last time) when it is free, else the first
/// free block from block `first` on. `None`: nothing free at all. A saved block below `first` is
/// not even looked at (tests start at block 1 so that the live server's ports are never probed).
pub fn choose(saved: Option<PortBlock>, first: u16, is_free: impl Fn(&PortBlock) -> bool) -> Option<PortBlock> {
    let saved = saved.filter(|b| b.index().is_none_or(|k| k >= first));
    if let Some(b) = saved.filter(|b| is_free(b)) {
        return Some(b);
    }
    (first..BLOCKS).map(PortBlock::nth).find(|b| Some(*b) != saved && is_free(b))
}

/// Can every port of `block` be bound on `ip` right now? (Bound and released at once: TCP is
/// never listened on, so nothing can connect.) TCP is also tried on every interface and on
/// loopback: Windows lets `127.0.0.1:7890` be bound while another program listens on
/// `0.0.0.0:7890`, and the two would then split this PC's connections between them.
pub fn block_is_free(block: &PortBlock, ip: Ipv4Addr) -> bool {
    let tcp_ok = |p: u16| [Ipv4Addr::UNSPECIFIED, Ipv4Addr::LOCALHOST, ip].iter().all(|a| tcp_bindable(*a, p));
    block.tcp().iter().all(|p| tcp_ok(*p)) && UdpSocket::bind((ip, block.rtc_udp)).is_ok()
}

/// A TCP socket can be bound on `ip:port` (bound only, never listening: a probe on every
/// interface must not make the Windows Firewall ask about this program; the port conflict is
/// decided at bind time anyway). Same address-reuse rule as `std::net::TcpListener::bind`.
fn tcp_bindable(ip: Ipv4Addr, port: u16) -> bool {
    use socket2::{Domain, Protocol, Socket, Type};
    let Ok(s) = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)) else { return false };
    #[cfg(not(windows))]
    if s.set_reuse_address(true).is_err() {
        return false;
    }
    s.bind(&std::net::SocketAddr::from((ip, port)).into()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn blocks() {
        assert_eq!(PortBlock::DEFAULT, PortBlock { hub: 7890, signal: 7880, rtc_tcp: 7881, rtc_udp: 7882 });
        assert_eq!(PortBlock::nth(1), PortBlock { hub: 7990, signal: 7980, rtc_tcp: 7981, rtc_udp: 7982 });
        assert_eq!(PortBlock::nth(1).index(), Some(1));
        assert_eq!(PortBlock { hub: 1, signal: 2, rtc_tcp: 3, rtc_udp: 4 }.index(), None);
        assert_eq!(PortBlock::nth(1).describe(), "7990 (TCP), 7980-7981 (TCP) e 7982 (UDP)");
    }

    #[test]
    fn the_next_free_block_is_chosen() {
        let busy = |taken: &'static [u16]| move |b: &PortBlock| !taken.iter().any(|p| b.tcp().contains(p) || b.rtc_udp == *p);
        // Nothing busy: the default block.
        assert_eq!(choose(None, 0, busy(&[])), Some(PortBlock::DEFAULT));
        // The official server runs here: its hub port (or any of its ports) is taken.
        assert_eq!(choose(None, 0, busy(&[7890])), Some(PortBlock::nth(1)));
        assert_eq!(choose(None, 0, busy(&[7882])), Some(PortBlock::nth(1)));
        assert_eq!(choose(None, 0, busy(&[7882, 7981])), Some(PortBlock::nth(2)));
        // The block used last time wins while it is free, even if it is not the first.
        assert_eq!(choose(Some(PortBlock::nth(3)), 0, busy(&[])), Some(PortBlock::nth(3)));
        // ... and is replaced when it is not.
        assert_eq!(choose(Some(PortBlock::nth(3)), 0, busy(&[8190])), Some(PortBlock::DEFAULT));
        // A saved block that is not one of ours (edited by hand) is kept while free.
        let odd = PortBlock { hub: 9000, signal: 9001, rtc_tcp: 9002, rtc_udp: 9003 };
        assert_eq!(choose(Some(odd), 0, busy(&[])), Some(odd));
        // Tests start from block 1, never touching the live server's ports, not even a saved
        // block 0 is probed.
        assert_eq!(choose(None, 1, busy(&[])), Some(PortBlock::nth(1)));
        let probed = std::cell::RefCell::new(Vec::new());
        let chosen = choose(Some(PortBlock::DEFAULT), 1, |b| {
            probed.borrow_mut().push(b.hub);
            true
        });
        assert_eq!(chosen, Some(PortBlock::nth(1)));
        assert_eq!(probed.into_inner(), [7990]);
        // Everything busy.
        assert_eq!(choose(None, 0, |_| false), None);
    }

    #[test]
    fn a_bound_port_is_busy() {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        let b = PortBlock { hub: p, signal: p.wrapping_add(1).max(1024), rtc_tcp: p.wrapping_add(2).max(1024), rtc_udp: p.wrapping_add(3).max(1024) };
        assert!(!block_is_free(&b, Ipv4Addr::LOCALHOST));
        drop(l);
        // Taken on every interface by another program: busy for a loopback bind too. (Bound, not
        // listening: a test never listens on every interface, so no firewall prompt.)
        let any = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
        any.bind(&std::net::SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)).into()).unwrap();
        let p = any.local_addr().unwrap().as_socket().unwrap().port();
        let b = PortBlock { hub: p, ..b };
        assert!(!block_is_free(&b, Ipv4Addr::LOCALHOST));
    }
}
