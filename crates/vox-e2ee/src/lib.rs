//! concord's end-to-end encryption, client side (v1.1, phase E0 of docs/PLANO-v1.1-E2EE.md).
//!
//! - [`Identity`]: one X25519 static key per (hub, user, device), with a random 16-byte device id.
//!   The hub only keeps the public half in its directory (`/api/e2ee/devices`); the secret never
//!   leaves the device (the app keeps it through DPAPI / the Keychain) and is wiped on drop.
//! - [`wrap`] / [`unwrap`]: the envelope that carries a small secret (a message key, a media key)
//!   from one device to another:
//!
//!   ```text
//!   shared = HKDF-SHA256(salt = "concord-wrap-v1",
//!                        ikm  = DH(eph, R) || DH(S_sender, R),
//!                        info = sender_dev || recipient_dev || eph_pub || context)
//!   wrapped = ChaCha20-Poly1305(key = shared, nonce = 0, aad = empty, plaintext)
//!   ```
//!
//!   `eph` is a fresh X25519 key drawn by every `wrap` call, so each AEAD key is used exactly once
//!   and the all-zero nonce is safe. `DH(S_sender, R)` authenticates the sender's device (only the
//!   holder of a directory key can produce a wrap that opens against it); `context` binds the
//!   wrap to what it is for (a DM, a room and key index...), so a wrap cannot be replayed into
//!   another place. No forward secrecy against theft of the recipient's static key (accepted in
//!   the plan).
//!
//! Phase E2 (DMs) builds on that envelope:
//! - [`envelope`]: one random message key wrapped for many devices under one ephemeral key (the
//!   DM header).
//! - [`dm`]: the `e2e1` body of a DM line (header + ChaCha20-Poly1305 ciphertext), its JSON
//!   plaintext, and the per-image keys of sealed attachments.
//! - [`safety`]: the 30-digit safety code two people compare.
//!
//! Phase E1 (calls): [`media`] carries each member's media key (LiveKit's frame encryption) to
//! every other member's device in the room, wrapped with the same envelope.
//!
//! Nothing here logs, and `Debug` never prints a secret.

use std::fmt;

use base64::Engine;
use ring::{aead, hkdf};
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::{Zeroize, Zeroizing};

pub use zeroize;

pub mod dm;
pub mod envelope;
pub mod media;
pub mod safety;

/// Bytes of a device id.
pub const DEVICE_ID_LEN: usize = 16;
/// Bytes of an X25519 key (public or secret).
pub const KEY_LEN: usize = 32;
/// Bytes the AEAD adds to a wrapped secret.
pub const TAG_LEN: usize = 16;
/// The HKDF salt of the envelope (the format's version).
pub const WRAP_SALT: &[u8] = b"concord-wrap-v1";

/// A device id (random, 16 bytes; hex on the wire).
pub type DeviceId = [u8; DEVICE_ID_LEN];

/// What can go wrong. Deliberately coarse: an `unwrap` that fails says nothing about why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The operating system gave no randomness.
    Random,
    /// A peer's public key is a low-order point (the Diffie-Hellman result carries no secret).
    WeakKey,
    /// The wrap did not open: wrong key, wrong device, wrong context, or it was altered.
    Open,
    /// Malformed input (lengths, encodings).
    Format,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::Random => "no randomness from the operating system",
            Error::WeakKey => "a public key without a shared secret (low-order point)",
            Error::Open => "the wrapped key could not be opened",
            Error::Format => "malformed key material",
        })
    }
}

impl std::error::Error for Error {}

pub(crate) fn random<const N: usize>() -> Result<[u8; N], Error> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|_| Error::Random)?;
    Ok(b)
}

/// A fresh random device id.
pub fn new_device_id() -> Result<DeviceId, Error> {
    random()
}

/// Lowercase hex (device ids on the wire).
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}

/// A device id from its wire form (exactly 32 lowercase hex digits).
pub fn device_id_from_hex(s: &str) -> Option<DeviceId> {
    if s.len() != DEVICE_ID_LEN * 2 || !s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) {
        return None;
    }
    let mut out = [0u8; DEVICE_ID_LEN];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// A public key as the directory carries it: base64url without padding (43 characters).
pub fn public_key_to_b64(key: &[u8; KEY_LEN]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key)
}

/// A directory public key back to bytes (exactly 32 bytes, canonical base64url, no padding).
pub fn public_key_from_b64(s: &str) -> Option<[u8; KEY_LEN]> {
    let v = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s.as_bytes()).ok()?;
    v.try_into().ok()
}

/// This device's identity on one hub, for one user: its id and its X25519 static secret. The
/// secret is wiped when the identity is dropped.
pub struct Identity {
    pub device_id: DeviceId,
    secret: StaticSecret,
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity").field("device_id", &hex(&self.device_id)).field("secret", &"<hidden>").finish()
    }
}

impl Identity {
    /// A new identity: a random device id and a random secret.
    pub fn generate() -> Result<Identity, Error> {
        let mut bytes = Zeroizing::new(random::<KEY_LEN>()?);
        let device_id = new_device_id()?;
        Ok(Identity { device_id, secret: StaticSecret::from(std::mem::take(&mut *bytes)) })
    }

    /// An identity kept from before (the app's e2ee.json). The caller's copy of `secret` is wiped.
    pub fn from_parts(device_id: DeviceId, secret: &mut [u8; KEY_LEN]) -> Identity {
        let id = Identity { device_id, secret: StaticSecret::from(*secret) };
        secret.zeroize();
        id
    }

    /// The same key under a new random device id (a reinstall reused the id: the hub already has
    /// another key under it).
    pub fn renew_device_id(&mut self) -> Result<(), Error> {
        self.device_id = new_device_id()?;
        Ok(())
    }

    /// The public half, for the directory.
    pub fn public_key(&self) -> [u8; KEY_LEN] {
        PublicKey::from(&self.secret).to_bytes()
    }

    /// The secret, for the app to store (protected). Wiped when the returned value is dropped.
    pub fn secret_bytes(&self) -> Zeroizing<[u8; KEY_LEN]> {
        Zeroizing::new(self.secret.to_bytes())
    }

    /// The device id in its wire form.
    pub fn device_id_hex(&self) -> String {
        hex(&self.device_id)
    }
}

/// A wrapped secret: the ephemeral public key of the wrap and the AEAD output (plaintext + tag).
#[derive(Clone, PartialEq, Eq)]
pub struct Wrapped {
    pub eph_pub: [u8; KEY_LEN],
    pub ciphertext: Vec<u8>,
}

impl fmt::Debug for Wrapped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Lengths only: wrapped blobs are never logged.
        f.debug_struct("Wrapped").field("ciphertext_len", &self.ciphertext.len()).finish_non_exhaustive()
    }
}

impl Wrapped {
    /// `eph_pub || ciphertext`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(KEY_LEN + self.ciphertext.len());
        v.extend_from_slice(&self.eph_pub);
        v.extend_from_slice(&self.ciphertext);
        v
    }

    /// Back from [`Wrapped::to_bytes`] (at least the key and a tag).
    pub fn from_bytes(b: &[u8]) -> Result<Wrapped, Error> {
        if b.len() < KEY_LEN + TAG_LEN {
            return Err(Error::Format);
        }
        let (eph, ct) = b.split_at(KEY_LEN);
        Ok(Wrapped { eph_pub: eph.try_into().map_err(|_| Error::Format)?, ciphertext: ct.to_vec() })
    }
}

/// X25519 with the low-order check: a result without a secret in it is refused.
pub(crate) fn dh(secret: &StaticSecret, public: &[u8; KEY_LEN]) -> Result<Zeroizing<[u8; KEY_LEN]>, Error> {
    let shared = secret.diffie_hellman(&PublicKey::from(*public));
    if !shared.was_contributory() {
        return Err(Error::WeakKey);
    }
    Ok(Zeroizing::new(shared.to_bytes()))
}

/// HKDF's output length for a 32-byte key.
struct Len32;

impl hkdf::KeyType for Len32 {
    fn len(&self) -> usize {
        KEY_LEN
    }
}

/// The envelope's key: HKDF-SHA256 over both Diffie-Hellman results, bound to both devices, the
/// ephemeral key and the context.
fn derive_key(dh_eph: &[u8; KEY_LEN], dh_static: &[u8; KEY_LEN], sender_dev: &DeviceId, recipient_dev: &DeviceId, eph_pub: &[u8; KEY_LEN], context: &[u8]) -> Result<Zeroizing<[u8; KEY_LEN]>, Error> {
    let mut ikm = Zeroizing::new([0u8; 2 * KEY_LEN]);
    ikm[..KEY_LEN].copy_from_slice(dh_eph);
    ikm[KEY_LEN..].copy_from_slice(dh_static);
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, WRAP_SALT).extract(&ikm[..]);
    let info: [&[u8]; 4] = [sender_dev, recipient_dev, eph_pub, context];
    let okm = prk.expand(&info, Len32).map_err(|_| Error::Format)?;
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    okm.fill(&mut key[..]).map_err(|_| Error::Format)?;
    Ok(key)
}

pub(crate) fn aead_key(key: &[u8; KEY_LEN]) -> Result<aead::LessSafeKey, Error> {
    Ok(aead::LessSafeKey::new(aead::UnboundKey::new(&aead::CHACHA20_POLY1305, key).map_err(|_| Error::Format)?))
}

/// The all-zero nonce: every envelope key is used for exactly one seal.
pub(crate) fn nonce0() -> aead::Nonce {
    aead::Nonce::assume_unique_for_key([0u8; aead::NONCE_LEN])
}

/// Wrap `plaintext` from `sender` for the device `recipient_dev` whose directory key is
/// `recipient_pub`, bound to `context`. Draws a fresh ephemeral key every call.
pub fn wrap(sender: &Identity, recipient_dev: &DeviceId, recipient_pub: &[u8; KEY_LEN], context: &[u8], plaintext: &[u8]) -> Result<Wrapped, Error> {
    let mut eph = Zeroizing::new(random::<KEY_LEN>()?);
    let eph = StaticSecret::from(std::mem::take(&mut *eph));
    wrap_with_ephemeral(sender, eph, recipient_dev, recipient_pub, context, plaintext)
}

/// [`wrap`] with a given ephemeral secret (consumed and wiped). Only [`wrap`] and the known-answer
/// tests call it: a reused ephemeral key would reuse an AEAD key.
fn wrap_with_ephemeral(sender: &Identity, eph: StaticSecret, recipient_dev: &DeviceId, recipient_pub: &[u8; KEY_LEN], context: &[u8], plaintext: &[u8]) -> Result<Wrapped, Error> {
    let w = wrap_shared(sender, &eph, recipient_dev, recipient_pub, context, plaintext);
    drop(eph);
    w
}

/// [`wrap`] under an ephemeral key shared by several recipients of one message (E2's header,
/// [`envelope`]). Each recipient's AEAD key still differs: the HKDF input has `DH(eph, R)` and the
/// recipient's device id, and the caller never wraps twice for the same device.
pub(crate) fn wrap_shared(sender: &Identity, eph: &StaticSecret, recipient_dev: &DeviceId, recipient_pub: &[u8; KEY_LEN], context: &[u8], plaintext: &[u8]) -> Result<Wrapped, Error> {
    let eph_pub = PublicKey::from(eph).to_bytes();
    let dh_eph = dh(eph, recipient_pub)?;
    let dh_static = dh(&sender.secret, recipient_pub)?;
    let key = derive_key(&dh_eph, &dh_static, &sender.device_id, recipient_dev, &eph_pub, context)?;
    // Room for the tag up front: appending it must not move the buffer and leave a copy of the
    // plaintext (a message key) behind in freed memory.
    let mut buf = Vec::with_capacity(plaintext.len() + TAG_LEN);
    buf.extend_from_slice(plaintext);
    aead_key(&key)?.seal_in_place_append_tag(nonce0(), aead::Aad::empty(), &mut buf).map_err(|_| Error::Format)?;
    Ok(Wrapped { eph_pub, ciphertext: buf })
}

/// A fresh ephemeral X25519 secret (wiped on drop) and its public half.
pub(crate) fn ephemeral() -> Result<(StaticSecret, [u8; KEY_LEN]), Error> {
    let mut bytes = Zeroizing::new(random::<KEY_LEN>()?);
    let eph = StaticSecret::from(std::mem::take(&mut *bytes));
    let public = PublicKey::from(&eph).to_bytes();
    Ok((eph, public))
}

/// Open a wrap made by the device `sender_dev` (directory key `sender_pub`) for `recipient`, with
/// the same `context`. Any mismatch or alteration is [`Error::Open`] (or [`Error::WeakKey`] for a
/// low-order ephemeral or sender key). The plaintext is wiped when dropped.
pub fn unwrap(recipient: &Identity, sender_dev: &DeviceId, sender_pub: &[u8; KEY_LEN], context: &[u8], wrapped: &Wrapped) -> Result<Zeroizing<Vec<u8>>, Error> {
    if wrapped.ciphertext.len() < TAG_LEN {
        return Err(Error::Format);
    }
    let dh_eph = dh(&recipient.secret, &wrapped.eph_pub)?;
    let dh_static = dh(&recipient.secret, sender_pub)?;
    let key = derive_key(&dh_eph, &dh_static, sender_dev, &recipient.device_id, &wrapped.eph_pub, context)?;
    let mut buf = Zeroizing::new(wrapped.ciphertext.clone());
    let n = aead_key(&key)?.open_in_place(nonce0(), aead::Aad::empty(), &mut buf[..]).map_err(|_| Error::Open)?.len();
    buf.truncate(n);
    Ok(buf)
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_dm;
