//! Invites ("Convidar amigos", v0.8.0, no protocol bump): the wire types of the invite endpoints
//! and the `vox1-...` invite code the apps exchange.
//!
//! **What protects a server.** An invite is a random 128-bit token the hub stores (as a hash)
//! with an expiry, a use limit and a "revoked" mark. Logging in with a valid token
//! ([`crate::LoginRequest::invite`]) replaces the server password, once per use; after that the
//! person keeps getting in with the device token the hub hands out on that login (a remembered
//! device skips the server password, see `LoginRequest`). Only Admin and Dono create, list and
//! revoke invites ([`Role::can_admin`](crate::Role::can_admin)). Wrong tokens are slowed down like
//! wrong server passwords.
//!
//! **What the code hides, honestly.** The code carries the token plus where to find the server
//! (public IPv4 / IPv6 or a host name, the LAN IPv4 with its prefix, the hub port). Its bytes are
//! scrambled with a ChaCha20 keystream from a key built into the app ([`InviteCode::encode`]), so
//! the host's IP is not readable by someone glancing at a chat. That is *obfuscation, not
//! security*: the app is open source, so anyone can decode a code with the same key, and every
//! member learns the host's IP anyway the moment they connect (that is how a connection works).
//! The real protection is the token: it expires, runs out and can be revoked.
//!
//! **Typos.** The code ends with a CRC-32 of its bytes: a mistyped or truncated code is reported
//! as such ([`InviteCodeError::Typo`]) instead of sending a wrong token or address anywhere.
//! Decoding is forgiving about what people do to a code: case, spaces, line breaks, dashes and
//! Crockford's look-alikes (O is 0, I and L are 1).
//!
//! HTTP:
//! - `POST /api/invites` [`InviteCreateRequest`] -> [`InviteCreated`] (Admin+). The token is only
//!   ever shown here; the hub keeps its hash.
//! - `GET  /api/invites` -> `Vec<Invite>`, newest first (Admin+).
//! - `POST /api/invites/{id}/revoke` -> [`crate::OkResponse`] (Admin+).
//! - `POST /api/invites/redeem` [`InviteRedeemRequest`] -> [`InviteRedeemResponse`]: no login
//!   needed; checks a token without using it up (the app shows the server's name, or why the
//!   invite does not work, before asking for a name). The use is counted by the login.
//!
//! Errors (`ApiError::error`): [`ERR_INVITE_INVALID`] (401: no such token), [`ERR_INVITE_EXPIRED`],
//! [`ERR_INVITE_USED_UP`], [`ERR_INVITE_REVOKED`] (all 401), and `429` "too many attempts ..."
//! while an address is being slowed down. A hub older than invites answers the login with "wrong
//! server password" (it ignores the field) and `404` on the endpoints.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use serde::{Deserialize, Serialize};

use crate::UserId;

pub type InviteId = i64;

/// Random bytes in an invite token.
pub const INVITE_TOKEN_LEN: usize = 16;
/// Longest validity a new invite may ask for (a year); `None` means "never expires".
pub const INVITE_MAX_EXPIRY_SECS: u64 = 365 * 24 * 3600;
/// Highest use limit a new invite may ask for; `None` means "unlimited".
pub const INVITE_MAX_USES: u32 = 1000;
/// Invites that still work, per server (old ones can be revoked to make room).
pub const INVITE_MAX_ACTIVE: usize = 50;

/// Login / redeem answer: there is no invite with this token.
pub const ERR_INVITE_INVALID: &str = "invite not valid";
/// Login / redeem answer: the invite's time is over.
pub const ERR_INVITE_EXPIRED: &str = "invite expired";
/// Login / redeem answer: the invite was used as many times as it allows.
pub const ERR_INVITE_USED_UP: &str = "invite used up";
/// Login / redeem answer: an Admin or the Dono cancelled the invite.
pub const ERR_INVITE_REVOKED: &str = "invite revoked";
/// Login answer (v1.0.1): an invite only makes new accounts. This name already has one, so the
/// login needs the server password, or the account's own personal password / remembered device.
pub const ERR_INVITE_NAME_TAKEN: &str = "name already exists on this server";

/// Moderation-log actions.
pub const AUDIT_INVITE_CREATE: &str = "invite_create";
pub const AUDIT_INVITE_REVOKE: &str = "invite_revoke";
/// Somebody joined with an invite (the actor is the person who joined; detail: the invite id).
pub const AUDIT_INVITE_JOIN: &str = "invite_join";

/// `POST /api/invites`
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteCreateRequest {
    /// Valid for this many seconds (at most [`INVITE_MAX_EXPIRY_SECS`]); `None` = never expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_secs: Option<u64>,
    /// Works this many times (1..=[`INVITE_MAX_USES`]); `None` = unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_uses: Option<u32>,
}

/// One invite, as Admins see it in the list (never with its token).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    pub id: InviteId,
    pub created_by: UserId,
    /// The creator's username.
    pub created_by_name: String,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_uses: Option<u32>,
    pub uses: u32,
    #[serde(default)]
    pub revoked: bool,
}

/// What an invite is good for right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteState {
    Active,
    Expired,
    UsedUp,
    Revoked,
}

impl Invite {
    pub fn state(&self, now_ms: i64) -> InviteState {
        if self.revoked {
            InviteState::Revoked
        } else if self.expires_at_ms.is_some_and(|e| e <= now_ms) {
            InviteState::Expired
        } else if self.max_uses.is_some_and(|m| self.uses >= m) {
            InviteState::UsedUp
        } else {
            InviteState::Active
        }
    }
}

/// Where the hub thinks it can be reached, for building the code (the app adds what it knows:
/// the address it used itself, and in host mode the router's / STUN's public IP).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteHints {
    /// The port the request came in on (the hub's own port unless a router remaps it).
    pub hub_port: u16,
    /// `public_host` from the hub's `config.toml` (an IP or a DDNS name); empty or loopback when
    /// the host never set it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub public_host: String,
    /// The server PC's LAN IPv4 and its prefix length, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lan_ipv4: Option<Ipv4Addr>,
    #[serde(default)]
    pub lan_prefix: u8,
    /// v1.1 (T0): the SHA-256 of the hub's TLS certificate, 64 hex digits ([`tls_pin_hex`]), when
    /// the hub speaks TLS. The code then carries it ([`InviteCode::tls_pin`]) and the app joining
    /// with it talks to that certificate only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_pin: Option<String>,
}

/// `POST /api/invites` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteCreated {
    pub invite: Invite,
    /// The token, hex (only here: the hub keeps a hash).
    pub token: String,
    pub hints: InviteHints,
}

/// `POST /api/invites/redeem`
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteRedeemRequest {
    /// The token, hex.
    pub token: String,
}

/// `POST /api/invites/redeem` answer: the invite works (it is not used up by this call).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteRedeemResponse {
    pub server_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
}

/// Token bytes as the hex the API carries.
pub fn invite_token_hex(token: &[u8; INVITE_TOKEN_LEN]) -> String {
    token.iter().map(|b| format!("{b:02x}")).collect()
}

/// The API's hex back into token bytes (`None`: not 32 hex digits).
pub fn invite_token_from_hex(hex: &str) -> Option<[u8; INVITE_TOKEN_LEN]> {
    let hex = hex.trim();
    if hex.len() != INVITE_TOKEN_LEN * 2 || !hex.is_ascii() {
        return None;
    }
    let mut out = [0u8; INVITE_TOKEN_LEN];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// v1.1 (T0): a TLS pin (SHA-256 of the hub's certificate) as the 64 lowercase hex digits the API
/// and the saved settings carry.
pub fn tls_pin_hex(pin: &[u8; 32]) -> String {
    pin.iter().map(|b| format!("{b:02x}")).collect()
}

/// [`tls_pin_hex`] back into bytes (`None`: not 64 hex digits).
pub fn tls_pin_from_hex(hex: &str) -> Option<[u8; 32]> {
    let hex = hex.trim();
    // Audit (T0): hex digits only (`from_str_radix` alone also takes a sign: "+f").
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// v1.1 (T0): the "Código do servidor" people compare by eye (the app's "Segurança da conexão",
/// the server console): Crockford base32 of the pin's first 10 bytes, in groups of four
/// (`K7QM-2D9X-4HVA-PB1C`). 80 bits, for a person comparing two certificates; the app itself
/// always compares the whole pin.
pub fn tls_server_code(pin: &[u8; 32]) -> String {
    let text = base32_encode(&pin[..10]);
    let groups: Vec<&str> = (0..text.len()).step_by(4).map(|i| &text[i..(i + 4).min(text.len())]).collect();
    groups.join("-")
}

// ---------------------------------------------------------------- the code

/// What every code starts with ("vox", format 1).
pub const INVITE_PREFIX: &str = "vox1-";
/// The layout version inside the scrambled bytes: 1, or (v1.1) 2 when the code carries the hub's
/// TLS pin. A v1.0 app reads a layout-2 code as [`InviteCodeError::NewerVersion`] ("atualize").
const LAYOUT_VERSION: u8 = 1;
const LAYOUT_TLS: u8 = 2;
/// Most addresses in one code.
const MAX_ADDRS: usize = 6;

const TAG_PUBLIC_V4: u8 = 0x04;
const TAG_PUBLIC_V6: u8 = 0x06;
const TAG_LAN_V4: u8 = 0x14;
const TAG_HOST: u8 = 0x48;
/// v1.1 (layout 2): the SHA-256 of the hub's TLS certificate, 32 bytes, exactly once.
const TAG_TLS_PIN: u8 = 0x50;

/// One way to reach the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InviteAddr {
    /// A public address (the router's WAN IP, a public IPv6 of the server PC...).
    Public(IpAddr),
    /// The server PC's LAN address and prefix length: tried first by someone on that network.
    Lan(Ipv4Addr, u8),
    /// A DNS name (a classic server's DDNS `public_host`).
    Host(String),
}

/// A decoded invite code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteCode {
    pub token: [u8; INVITE_TOKEN_LEN],
    pub hub_port: u16,
    pub addrs: Vec<InviteAddr>,
    /// v1.1: the SHA-256 of the hub's TLS certificate. With it the app talks to the hub over TLS
    /// with exactly that certificate, never in the clear (layout 2); without it the code is the
    /// layout 1 every app reads.
    pub tls_pin: Option<[u8; 32]>,
}

/// Why a text is not a usable invite code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteCodeError {
    /// It does not start with `vox1-`.
    NotAnInvite,
    /// The checksum does not match: mistyped, cut short or with a character that does not belong.
    Typo,
    /// Made by a newer app (a layout this one does not know).
    NewerVersion,
    /// Right checksum but nonsense inside (made by something else).
    Damaged,
}

impl std::fmt::Display for InviteCodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            InviteCodeError::NotAnInvite => "not an invite code",
            InviteCodeError::Typo => "the invite code has a typo or is incomplete",
            InviteCodeError::NewerVersion => "the invite code is from a newer version",
            InviteCodeError::Damaged => "the invite code is damaged",
        })
    }
}

impl std::error::Error for InviteCodeError {}

/// Does this text look like an invite code (so the app should not treat it as an address)?
pub fn looks_like_invite(text: &str) -> bool {
    let t = text.trim();
    t.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("vox1"))
}

impl InviteCode {
    /// `vox1-` + Crockford base32 of the scrambled payload and its CRC-32.
    pub fn encode(&self) -> String {
        let mut plain = Vec::with_capacity(100);
        plain.push(if self.tls_pin.is_some() { LAYOUT_TLS } else { LAYOUT_VERSION });
        plain.extend_from_slice(&self.token);
        plain.extend_from_slice(&self.hub_port.to_be_bytes());
        for a in self.addrs.iter().take(MAX_ADDRS) {
            match a {
                InviteAddr::Public(IpAddr::V4(ip)) => {
                    plain.push(TAG_PUBLIC_V4);
                    plain.extend_from_slice(&ip.octets());
                }
                InviteAddr::Public(IpAddr::V6(ip)) => {
                    plain.push(TAG_PUBLIC_V6);
                    plain.extend_from_slice(&ip.octets());
                }
                InviteAddr::Lan(ip, prefix) => {
                    plain.push(TAG_LAN_V4);
                    plain.extend_from_slice(&ip.octets());
                    plain.push((*prefix).min(32));
                }
                InviteAddr::Host(name) => {
                    let name = name.as_bytes();
                    if name.is_empty() || name.len() > 253 {
                        continue;
                    }
                    plain.push(TAG_HOST);
                    plain.push(name.len() as u8);
                    plain.extend_from_slice(name);
                }
            }
        }
        if let Some(pin) = &self.tls_pin {
            plain.push(TAG_TLS_PIN);
            plain.extend_from_slice(pin);
        }
        scramble(&mut plain);
        let crc = crc32(&plain);
        plain.extend_from_slice(&crc.to_be_bytes());
        format!("{INVITE_PREFIX}{}", base32_encode(&plain))
    }

    pub fn decode(text: &str) -> Result<InviteCode, InviteCodeError> {
        let t = text.trim();
        if !looks_like_invite(t) {
            return Err(InviteCodeError::NotAnInvite);
        }
        let mut bytes = base32_decode(&t[4..]).ok_or(InviteCodeError::Typo)?;
        // version + token + port + crc
        if bytes.len() < 1 + INVITE_TOKEN_LEN + 2 + 4 {
            return Err(InviteCodeError::Typo);
        }
        let split = bytes.len() - 4;
        let crc = u32::from_be_bytes([bytes[split], bytes[split + 1], bytes[split + 2], bytes[split + 3]]);
        bytes.truncate(split);
        if crc32(&bytes) != crc {
            return Err(InviteCodeError::Typo);
        }
        unscramble(&mut bytes);
        let layout = bytes[0];
        if layout != LAYOUT_VERSION && layout != LAYOUT_TLS {
            return Err(InviteCodeError::NewerVersion);
        }
        let mut token = [0u8; INVITE_TOKEN_LEN];
        token.copy_from_slice(&bytes[1..1 + INVITE_TOKEN_LEN]);
        let mut at = 1 + INVITE_TOKEN_LEN;
        let hub_port = u16::from_be_bytes([bytes[at], bytes[at + 1]]);
        at += 2;
        if hub_port == 0 {
            return Err(InviteCodeError::Damaged);
        }
        let take = |at: &mut usize, n: usize| -> Result<&[u8], InviteCodeError> {
            let s = bytes.get(*at..*at + n).ok_or(InviteCodeError::Damaged)?;
            *at += n;
            Ok(s)
        };
        let mut addrs = Vec::new();
        let mut tls_pin = None;
        while at < bytes.len() {
            let tag = take(&mut at, 1)?[0];
            if tag == TAG_TLS_PIN {
                if layout != LAYOUT_TLS || tls_pin.is_some() {
                    return Err(InviteCodeError::Damaged);
                }
                let mut pin = [0u8; 32];
                pin.copy_from_slice(take(&mut at, 32)?);
                tls_pin = Some(pin);
                continue;
            }
            if addrs.len() == MAX_ADDRS {
                return Err(InviteCodeError::Damaged);
            }
            let addr = match tag {
                TAG_PUBLIC_V4 => {
                    let o = take(&mut at, 4)?;
                    InviteAddr::Public(IpAddr::V4(Ipv4Addr::new(o[0], o[1], o[2], o[3])))
                }
                TAG_PUBLIC_V6 => {
                    let mut o = [0u8; 16];
                    o.copy_from_slice(take(&mut at, 16)?);
                    InviteAddr::Public(IpAddr::V6(Ipv6Addr::from(o)))
                }
                TAG_LAN_V4 => {
                    let o = take(&mut at, 5)?;
                    if o[4] > 32 {
                        return Err(InviteCodeError::Damaged);
                    }
                    InviteAddr::Lan(Ipv4Addr::new(o[0], o[1], o[2], o[3]), o[4])
                }
                TAG_HOST => {
                    let n = take(&mut at, 1)?[0] as usize;
                    let name = take(&mut at, n)?;
                    let ok = n > 0 && name.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'));
                    if !ok {
                        return Err(InviteCodeError::Damaged);
                    }
                    InviteAddr::Host(String::from_utf8_lossy(name).into_owned())
                }
                _ => return Err(InviteCodeError::Damaged),
            };
            addrs.push(addr);
        }
        // Layout 2 exists for the pin: one without it is not from a concord.
        if addrs.is_empty() || (layout == LAYOUT_TLS) != tls_pin.is_some() {
            return Err(InviteCodeError::Damaged);
        }
        Ok(InviteCode { token, hub_port, addrs, tls_pin })
    }

    pub fn token_hex(&self) -> String {
        invite_token_hex(&self.token)
    }

    /// The hub URLs to try, in order. `local` is this computer's interface addresses with their
    /// prefix lengths: a LAN address comes first only when one of them is on the same network
    /// (a friend at the host's house); otherwise the public ones, and the LAN address only as a
    /// last resort when the code has nothing else.
    pub fn candidate_urls(&self, local: &[(IpAddr, u8)]) -> Vec<String> {
        let url = |ip: IpAddr| match ip {
            IpAddr::V4(v4) => format!("http://{v4}:{}", self.hub_port),
            IpAddr::V6(v6) => format!("http://[{v6}]:{}", self.hub_port),
        };
        let mut near = Vec::new();
        let mut far_lan = Vec::new();
        let mut v4 = Vec::new();
        let mut names = Vec::new();
        let mut v6 = Vec::new();
        for a in &self.addrs {
            match a {
                InviteAddr::Lan(ip, prefix) => {
                    let here = local.iter().any(|(mine, _)| matches!(mine, IpAddr::V4(m) if same_subnet(*m, *ip, *prefix)));
                    if here { near.push(url(IpAddr::V4(*ip))) } else { far_lan.push(url(IpAddr::V4(*ip))) }
                }
                InviteAddr::Public(ip @ IpAddr::V4(_)) => v4.push(url(*ip)),
                InviteAddr::Public(ip) => v6.push(url(*ip)),
                InviteAddr::Host(name) => names.push(format!("http://{name}:{}", self.hub_port)),
            }
        }
        let mut out: Vec<String> = near.into_iter().chain(v4).chain(names).chain(v6).collect();
        if out.is_empty() {
            out = far_lan;
        }
        out.dedup();
        out
    }
}

/// Are `a` and `b` on the same IPv4 network of `prefix` bits?
pub fn same_subnet(a: Ipv4Addr, b: Ipv4Addr, prefix: u8) -> bool {
    let prefix = prefix.min(32) as u32;
    if prefix == 0 {
        return true;
    }
    let mask = u32::MAX << (32 - prefix);
    (u32::from(a) & mask) == (u32::from(b) & mask)
}

// ---------------------------------------------------------------- scrambling (obfuscation only)

/// The app's built-in key. It is public (this file is open source): it keeps an IP from being
/// readable at a glance in a chat, nothing more. See the module docs.
const OBFUSCATION_KEY: &[u8; 32] = b"voxhub-invite-obfuscation-key-v1";
/// Version and token: scrambled with a fixed nonce. The rest: with the token as nonce, so two
/// codes of the same server share no visible pieces.
const HEAD_LEN: usize = 1 + INVITE_TOKEN_LEN;

fn scramble(buf: &mut [u8]) {
    let (head, tail) = buf.split_at_mut(HEAD_LEN);
    let nonce = tail_nonce(&head[1..]);
    chacha20_xor(OBFUSCATION_KEY, &[0u8; 12], 0, head);
    chacha20_xor(OBFUSCATION_KEY, &nonce, 1, tail);
}

fn unscramble(buf: &mut [u8]) {
    let (head, tail) = buf.split_at_mut(HEAD_LEN);
    chacha20_xor(OBFUSCATION_KEY, &[0u8; 12], 0, head);
    let nonce = tail_nonce(&head[1..]);
    chacha20_xor(OBFUSCATION_KEY, &nonce, 1, tail);
}

fn tail_nonce(token: &[u8]) -> [u8; 12] {
    let mut n = [0u8; 12];
    n.copy_from_slice(&token[..12]);
    n
}

/// ChaCha20 (RFC 8439) keystream XORed into `data`, starting at block `counter`.
fn chacha20_xor(key: &[u8; 32], nonce: &[u8; 12], counter: u32, data: &mut [u8]) {
    let word = |b: &[u8], i: usize| u32::from_le_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
    let mut state = [0u32; 16];
    state[..4].copy_from_slice(&[0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574]);
    for i in 0..8 {
        state[4 + i] = word(key, i);
    }
    for i in 0..3 {
        state[13 + i] = word(nonce, i);
    }
    for (n, chunk) in data.chunks_mut(64).enumerate() {
        state[12] = counter.wrapping_add(n as u32);
        let mut x = state;
        for _ in 0..10 {
            for (a, b, c, d) in [(0, 4, 8, 12), (1, 5, 9, 13), (2, 6, 10, 14), (3, 7, 11, 15), (0, 5, 10, 15), (1, 6, 11, 12), (2, 7, 8, 13), (3, 4, 9, 14)] {
                x[a] = x[a].wrapping_add(x[b]);
                x[d] = (x[d] ^ x[a]).rotate_left(16);
                x[c] = x[c].wrapping_add(x[d]);
                x[b] = (x[b] ^ x[c]).rotate_left(12);
                x[a] = x[a].wrapping_add(x[b]);
                x[d] = (x[d] ^ x[a]).rotate_left(8);
                x[c] = x[c].wrapping_add(x[d]);
                x[b] = (x[b] ^ x[c]).rotate_left(7);
            }
        }
        for (i, byte) in chunk.iter_mut().enumerate() {
            let w = x[i / 4].wrapping_add(state[i / 4]);
            *byte ^= w.to_le_bytes()[i % 4];
        }
    }
}

// ---------------------------------------------------------------- checksum and text

/// CRC-32 (IEEE 802.3): catches every single wrong character, swapped neighbours and any burst
/// up to 32 bits.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// Crockford base32: digits and letters without I, L, O, U.
const BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 8 / 5 + 1);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in bytes {
        acc = (acc << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32[((acc >> bits) & 31) as usize] as char);
        }
        acc &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(BASE32[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// The inverse, ignoring case, spaces, line breaks and dashes; O reads as 0, I and L as 1.
/// `None` for anything else, or for leftover bits that are not zero.
fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text.chars() {
        if c == '-' || c.is_whitespace() {
            continue;
        }
        let c = match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        };
        let v = BASE32.iter().position(|&a| a as char == c)? as u32;
        acc = (acc << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
        acc &= (1 << bits) - 1;
    }
    (acc == 0).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> InviteCode {
        InviteCode {
            token: [0x5a; INVITE_TOKEN_LEN],
            hub_port: 7990,
            addrs: vec![InviteAddr::Public("203.0.113.7".parse().unwrap()), InviteAddr::Lan(Ipv4Addr::new(192, 168, 0, 20), 24)],
            tls_pin: None,
        }
    }

    #[test]
    fn chacha20_matches_rfc_8439() {
        // RFC 8439 section 2.4.2.
        let key: [u8; 32] = std::array::from_fn(|i| i as u8);
        let nonce = [0, 0, 0, 0, 0, 0, 0, 0x4a, 0, 0, 0, 0];
        let mut text = *b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
        chacha20_xor(&key, &nonce, 1, &mut text);
        assert_eq!(&text[..16], &[0x6e, 0x2e, 0x35, 0x9a, 0x25, 0x68, 0xf9, 0x80, 0x41, 0xba, 0x07, 0x28, 0xdd, 0x0d, 0x69, 0x81]);
        assert_eq!(&text[text.len() - 2..], &[0x87, 0x4d]);
    }

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn codes_round_trip() {
        let c = sample();
        let text = c.encode();
        assert!(text.starts_with("vox1-"), "{text}");
        assert!(text.len() < 64, "short enough to paste: {} chars", text.len());
        assert_eq!(InviteCode::decode(&text), Ok(c.clone()));
        // Every kind of address, at once.
        let all = InviteCode {
            token: std::array::from_fn(|i| i as u8 * 7),
            hub_port: 7890,
            addrs: vec![
                InviteAddr::Lan(Ipv4Addr::new(10, 0, 0, 5), 8),
                InviteAddr::Public("198.51.100.20".parse().unwrap()),
                InviteAddr::Public("2001:db8::7".parse().unwrap()),
                InviteAddr::Host("voxhub.example.org".into()),
            ],
            tls_pin: None,
        };
        assert_eq!(InviteCode::decode(&all.encode()), Ok(all));
    }

    /// Encode raw (already laid out) plain bytes the way `encode` does.
    fn raw_code(mut plain: Vec<u8>) -> String {
        scramble(&mut plain);
        let crc = crc32(&plain);
        plain.extend_from_slice(&crc.to_be_bytes());
        format!("vox1-{}", base32_encode(&plain))
    }

    /// v1.1 (T0): a pin makes a layout-2 code; without one the code is byte for byte layout 1.
    #[test]
    fn layouts_1_and_2() {
        let plain = sample();
        let v1 = plain.encode();
        let pin: [u8; 32] = std::array::from_fn(|i| 0xa0 ^ i as u8);
        let pinned = InviteCode { tls_pin: Some(pin), ..sample() };
        let v2 = pinned.encode();
        assert_ne!(v1, v2);
        assert!(v2.len() < 120, "still pasteable: {} chars", v2.len());
        assert_eq!(InviteCode::decode(&v1), Ok(plain.clone()));
        assert_eq!(InviteCode::decode(&v2), Ok(pinned.clone()));
        let unpack = |code: &str| {
            let mut raw = base32_decode(&code[5..]).unwrap();
            raw.truncate(raw.len() - 4);
            unscramble(&mut raw);
            raw
        };
        // Layout 1 is what every app reads; layout 2 ends with the pin.
        assert_eq!(unpack(&v1)[0], 1);
        let raw = unpack(&v2);
        assert_eq!(raw[0], 2);
        assert_eq!(raw[raw.len() - 33], TAG_TLS_PIN);
        assert_eq!(&raw[raw.len() - 32..], &pin);
        // The pin is under the CRC: a wrong character anywhere is caught.
        let chars: Vec<char> = v2.chars().collect();
        for i in 5..chars.len() {
            let mut t = chars.clone();
            t[i] = if t[i] == 'Z' { 'Y' } else { 'Z' };
            assert!(InviteCode::decode(&t.into_iter().collect::<String>()).is_err(), "typo at {i} not caught");
        }
        // A pin in layout 1, two pins, a cut pin, or layout 2 without one: not from a concord.
        let base = |layout: u8| {
            let mut p = vec![layout];
            p.extend_from_slice(&plain.token);
            p.extend_from_slice(&plain.hub_port.to_be_bytes());
            p.push(TAG_PUBLIC_V4);
            p.extend_from_slice(&[203, 0, 113, 7]);
            p
        };
        let with_pin = |mut p: Vec<u8>| {
            p.push(TAG_TLS_PIN);
            p.extend_from_slice(&pin);
            p
        };
        assert_eq!(InviteCode::decode(&raw_code(with_pin(base(1)))), Err(InviteCodeError::Damaged));
        assert_eq!(InviteCode::decode(&raw_code(with_pin(with_pin(base(2))))), Err(InviteCodeError::Damaged));
        assert_eq!(InviteCode::decode(&raw_code(base(2))), Err(InviteCodeError::Damaged));
        let mut cut = with_pin(base(2));
        cut.pop();
        assert_eq!(InviteCode::decode(&raw_code(cut)), Err(InviteCodeError::Damaged));
        assert!(InviteCode::decode(&raw_code(with_pin(base(2)))).is_ok());
        // Layout 3 is still "newer".
        assert_eq!(InviteCode::decode(&raw_code(base(3))), Err(InviteCodeError::NewerVersion));
    }

    #[test]
    fn pins_as_text_and_the_server_code() {
        let pin: [u8; 32] = std::array::from_fn(|i| (i as u8).wrapping_mul(9));
        let hex = tls_pin_hex(&pin);
        assert_eq!(hex.len(), 64);
        assert_eq!(tls_pin_from_hex(&hex), Some(pin));
        assert_eq!(tls_pin_from_hex(&hex.to_uppercase()), Some(pin));
        assert_eq!(tls_pin_from_hex(&hex[..62]), None);
        assert_eq!(tls_pin_from_hex(&"zz".repeat(32)), None);
        assert_eq!(tls_pin_from_hex(&format!("+f{}", &hex[2..])), None, "a sign is not a hex digit");
        let code = tls_server_code(&pin);
        assert_eq!(code.len(), 19, "{code}");
        assert_eq!(code.matches('-').count(), 3, "{code}");
        assert!(code.chars().all(|c| c == '-' || BASE32.contains(&(c as u8))), "{code}");
        assert_eq!(tls_server_code(&[0u8; 32]), "0000-0000-0000-0000");
        assert_eq!(tls_server_code(&[0xffu8; 32]), "ZZZZ-ZZZZ-ZZZZ-ZZZZ");
        let mut other = pin;
        other[9] ^= 1;
        assert_ne!(tls_server_code(&other), code, "all of the first 10 bytes count");
    }

    #[test]
    fn people_can_mangle_the_text() {
        let text = sample().encode();
        let body = &text[5..];
        let messy = format!("  VOX1-{}\n", body.to_lowercase().chars().enumerate().map(|(i, c)| if i % 9 == 8 { format!("{c} ") } else { c.to_string() }).collect::<String>());
        assert_eq!(InviteCode::decode(&messy), Ok(sample()));
        // Crockford look-alikes typed by hand.
        let looks = format!("vox1-{}", body.replace('0', "O").replace('1', "l"));
        assert_eq!(InviteCode::decode(&looks), Ok(sample()));
        // Without the dash after the prefix.
        assert_eq!(InviteCode::decode(&format!("vox1{body}")), Ok(sample()));
    }

    #[test]
    fn typos_are_caught() {
        let text = sample().encode();
        let chars: Vec<char> = text.chars().collect();
        // Every single wrong character.
        for i in 5..chars.len() {
            for &c in BASE32 {
                let c = c as char;
                if c == chars[i] {
                    continue;
                }
                let mut t = chars.clone();
                t[i] = c;
                let t: String = t.into_iter().collect();
                assert!(InviteCode::decode(&t).is_err(), "typo at {i} ({c}) not caught");
            }
        }
        // Swapped neighbours, a missing character, a cut-off code, an extra one.
        for i in 5..chars.len() - 1 {
            if chars[i] == chars[i + 1] {
                continue;
            }
            let mut t = chars.clone();
            t.swap(i, i + 1);
            assert_eq!(InviteCode::decode(&t.into_iter().collect::<String>()), Err(InviteCodeError::Typo), "swap at {i}");
        }
        let mut short = text.clone();
        short.remove(20);
        assert_eq!(InviteCode::decode(&short), Err(InviteCodeError::Typo));
        assert_eq!(InviteCode::decode(&text[..text.len() - 6]), Err(InviteCodeError::Typo));
        assert_eq!(InviteCode::decode(&format!("{text}0")), Err(InviteCodeError::Typo));
        let mut with_u = chars.clone();
        with_u[10] = 'U';
        assert_eq!(InviteCode::decode(&with_u.into_iter().collect::<String>()), Err(InviteCodeError::Typo), "U is not in the alphabet");
        assert_eq!(InviteCode::decode("http://203.0.113.7:7890"), Err(InviteCodeError::NotAnInvite));
        assert_eq!(InviteCode::decode("vox1-"), Err(InviteCodeError::Typo));
    }

    #[test]
    fn the_ip_is_not_readable_in_the_code() {
        let c = sample();
        let text = c.encode();
        let raw = base32_decode(&text[5..]).unwrap();
        // Neither the IP bytes nor the port appear as such in the bytes of the code.
        for needle in [&[203u8, 0, 113, 7][..], &[192, 168, 0, 20], &7990u16.to_be_bytes()] {
            assert!(!raw.windows(needle.len()).any(|w| w == needle), "{needle:?} visible");
        }
        // Obfuscation round trip on its own.
        let mut buf: Vec<u8> = (0u8..60).collect();
        scramble(&mut buf);
        assert_ne!(buf, (0u8..60).collect::<Vec<_>>());
        unscramble(&mut buf);
        assert_eq!(buf, (0u8..60).collect::<Vec<_>>());
        // Two invites of the same server share no visible piece after the head.
        let mut other = c.clone();
        other.token[0] ^= 1;
        let (a, b) = (c.encode(), other.encode());
        assert_ne!(a[33..], b[33..]);
    }

    #[test]
    fn another_layout_version_is_reported() {
        let c = sample();
        let mut plain = vec![3u8];
        plain.extend_from_slice(&c.token);
        plain.extend_from_slice(&c.hub_port.to_be_bytes());
        scramble(&mut plain);
        let crc = crc32(&plain);
        plain.extend_from_slice(&crc.to_be_bytes());
        assert_eq!(InviteCode::decode(&format!("vox1-{}", base32_encode(&plain))), Err(InviteCodeError::NewerVersion));
    }

    #[test]
    fn lan_first_only_on_the_same_network() {
        let c = sample();
        let at_home = [("192.168.0.33".parse().unwrap(), 24u8)];
        assert_eq!(c.candidate_urls(&at_home), ["http://192.168.0.20:7990", "http://203.0.113.7:7990"]);
        let elsewhere = [("192.168.1.33".parse().unwrap(), 24u8), ("2001:db8::9".parse().unwrap(), 64)];
        assert_eq!(c.candidate_urls(&elsewhere), ["http://203.0.113.7:7990"]);
        let lan_only = InviteCode { addrs: vec![InviteAddr::Lan(Ipv4Addr::new(192, 168, 0, 20), 24)], ..c.clone() };
        assert_eq!(lan_only.candidate_urls(&elsewhere), ["http://192.168.0.20:7990"]);
        let v6 = InviteCode { addrs: vec![InviteAddr::Public("2001:db8::7".parse().unwrap()), InviteAddr::Host("a.example.org".into())], ..c };
        assert_eq!(v6.candidate_urls(&[]), ["http://a.example.org:7990", "http://[2001:db8::7]:7990"]);
        assert!(same_subnet(Ipv4Addr::new(10, 1, 2, 3), Ipv4Addr::new(10, 9, 9, 9), 8));
        assert!(!same_subnet(Ipv4Addr::new(10, 1, 2, 3), Ipv4Addr::new(11, 1, 2, 3), 8));
    }

    /// The login request exactly as the v0.7.0 app sends it.
    #[derive(Debug, Serialize, Deserialize)]
    struct LoginV070 {
        username: String,
        server_password: String,
        protocol_version: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_token: Option<String>,
        #[serde(default)]
        remember_device: bool,
    }

    #[test]
    fn the_invite_field_is_compatible_both_ways() {
        // An old app's login parses on a new hub, without an invite.
        let old = LoginV070 { username: "Tati".into(), server_password: "x".into(), protocol_version: 1, password: None, device_token: Some("d".into()), remember_device: true };
        let new: crate::LoginRequest = serde_json::from_str(&serde_json::to_string(&old).unwrap()).unwrap();
        assert_eq!(new.invite, None);
        assert_eq!(new.device_token.as_deref(), Some("d"));
        // Without an invite a new app sends exactly what the old one did.
        let plain = crate::LoginRequest { username: "Tati".into(), server_password: "x".into(), protocol_version: 1, ..Default::default() };
        assert!(!serde_json::to_string(&plain).unwrap().contains("invite"));
        // With one, an old hub still reads the request (and ignores the invite).
        let with = crate::LoginRequest { invite: Some("ab".repeat(16)), ..plain };
        let json = serde_json::to_string(&with).unwrap();
        assert!(json.contains("\"invite\":\""), "{json}");
        let seen: LoginV070 = serde_json::from_str(&json).unwrap();
        assert_eq!(seen.username, "Tati");
    }

    #[test]
    fn invite_states() {
        let mut i = Invite { id: 1, created_by: 1, created_by_name: "dono".into(), created_at_ms: 0, expires_at_ms: Some(100), max_uses: Some(2), uses: 0, revoked: false };
        assert_eq!(i.state(50), InviteState::Active);
        assert_eq!(i.state(100), InviteState::Expired);
        i.uses = 2;
        assert_eq!(i.state(50), InviteState::UsedUp);
        i.revoked = true;
        assert_eq!(i.state(50), InviteState::Revoked);
        let t = [0xab; INVITE_TOKEN_LEN];
        assert_eq!(invite_token_from_hex(&invite_token_hex(&t)), Some(t));
        assert_eq!(invite_token_from_hex("abc"), None);
        assert_eq!(invite_token_from_hex(&"zz".repeat(16)), None);
    }
}
