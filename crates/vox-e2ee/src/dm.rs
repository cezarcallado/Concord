//! E2: end-to-end encrypted DM lines (`dm_messages.format = 'e2e1'`) and their images.
//!
//! **Body** (base64url without padding, at most `DM_SEALED_MAX_BYTES` characters on the wire):
//!
//! ```text
//! header     = [1] [sender_dev 16] [eph_pub 32] [n] n x ([device_id 16] [wrapped K 48])
//! ciphertext = ChaCha20-Poly1305(K, nonce 0,
//!                                aad = "concord-dm-v1" || dm_id || sender_user_id || header,
//!                                plaintext JSON)
//! body       = b64url(header || ciphertext)
//! ```
//!
//! - `K` is 32 random bytes, drawn for this line only (so nonce 0 is safe), wrapped for every
//!   device of the peer and every device of the sender ([`crate::envelope`]) with the context
//!   `"concord-dm-v1" || dm_id || sender_user_id` (ids as 8-byte big-endian integers).
//! - The AAD binds the line to its conversation and its author (the hub cannot move it to another
//!   DM or pass it off as somebody else's) and to the whole header (the recipient list cannot be
//!   edited).
//! - The plaintext ([`DmPlain`]) is `{"t", "id", "ts", "att"}`: the text, a random uuid (the app
//!   drops a second line with an `id` it already showed: replays), the sender's clock and the
//!   images' keys ([`AttKey`]).
//!
//! **Images** ([`seal_blob`] / [`open_blob`]): each picture has its own random key; the file is
//! sealed with nonce 0 and its thumbnail with nonce 1 (as 96-bit big-endian counters), no AAD. The
//! hub stores the ciphertext as `application/octet-stream`; the receiver decrypts and only then
//! runs its usual image decoder (with its usual limits).

use std::fmt;

use base64::Engine;
use ring::aead;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::envelope::{self, Recipient, ENTRY_LEN, WRAPPED_KEY_LEN};
use crate::{DeviceId, Error, Identity, DEVICE_ID_LEN, KEY_LEN, TAG_LEN};

/// `dm_messages.format` of a sealed line.
pub const DM_FORMAT: &str = "e2e1";
/// The first byte of the header.
pub const DM_VERSION: u8 = 1;
/// The prefix of the wrap context and of the AAD.
pub const DM_AAD_PREFIX: &[u8] = b"concord-dm-v1";
/// Most devices one line can be wrapped for (`n` is one byte).
pub const MAX_RECIPIENTS: usize = 255;
/// Bytes of the header before the entries.
const FIXED: usize = 1 + DEVICE_ID_LEN + KEY_LEN + 1;

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

/// Which line: the conversation and the author (both in the wrap context and the AAD).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmContext {
    pub dm_id: i64,
    pub sender_user: i64,
}

impl DmContext {
    fn wrap_context(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(DM_AAD_PREFIX.len() + 16);
        v.extend_from_slice(DM_AAD_PREFIX);
        v.extend_from_slice(&self.dm_id.to_be_bytes());
        v.extend_from_slice(&self.sender_user.to_be_bytes());
        v
    }

    fn aad(&self, header: &[u8]) -> Vec<u8> {
        let mut v = self.wrap_context();
        v.extend_from_slice(header);
        v
    }
}

/// Seal `plaintext` (the JSON of a [`DmPlain`]) from `sender` for `recipients` (the peer's devices
/// and the sender's own; at least one, at most [`MAX_RECIPIENTS`]). Returns the wire body.
pub fn seal(sender: &Identity, ctx: DmContext, recipients: &[Recipient], plaintext: &[u8]) -> Result<String, Error> {
    let key = Zeroizing::new(crate::random::<KEY_LEN>()?);
    let (eph, _) = crate::ephemeral()?;
    seal_with(sender, ctx, recipients, plaintext, &key, eph)
}

/// [`seal`] with a given line key and ephemeral secret. Only [`seal`] and the known-answer test
/// call it: each line must have a fresh key.
pub(crate) fn seal_with(sender: &Identity, ctx: DmContext, recipients: &[Recipient], plaintext: &[u8], key: &[u8; KEY_LEN], eph: x25519_dalek::StaticSecret) -> Result<String, Error> {
    let mw = envelope::wrap_for_all_with(sender, eph, recipients, &ctx.wrap_context(), key)?;
    if mw.entries.is_empty() || mw.entries.len() > MAX_RECIPIENTS {
        return Err(Error::Format);
    }
    let mut out = Vec::with_capacity(FIXED + mw.entries.len() * ENTRY_LEN + plaintext.len() + TAG_LEN);
    out.push(DM_VERSION);
    out.extend_from_slice(&sender.device_id);
    out.extend_from_slice(&mw.eph_pub);
    out.push(mw.entries.len() as u8);
    for (dev, w) in &mw.entries {
        out.extend_from_slice(dev);
        out.extend_from_slice(w);
    }
    let header_len = out.len();
    // (Room for the tag up front: the buffer never moves and leaves the plaintext behind.)
    let mut ct = Vec::with_capacity(plaintext.len() + TAG_LEN);
    ct.extend_from_slice(plaintext);
    crate::aead_key(key)?.seal_in_place_append_tag(crate::nonce0(), aead::Aad::from(ctx.aad(&out[..header_len])), &mut ct).map_err(|_| Error::Format)?;
    out.extend_from_slice(&ct);
    Ok(b64().encode(&out))
}

/// A parsed body: who sealed it, for which devices, and the bytes to open.
#[derive(Clone, PartialEq, Eq)]
pub struct Sealed {
    pub sender_dev: DeviceId,
    pub eph_pub: [u8; KEY_LEN],
    pub entries: Vec<(DeviceId, [u8; WRAPPED_KEY_LEN])>,
    bytes: Vec<u8>,
    header_len: usize,
}

impl fmt::Debug for Sealed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealed").field("sender_dev", &crate::hex(&self.sender_dev)).field("entries", &self.entries.len()).field("len", &self.bytes.len()).finish_non_exhaustive()
    }
}

impl Sealed {
    /// Was this line wrapped for `device`?
    pub fn is_for(&self, device: &DeviceId) -> bool {
        self.entries.iter().any(|(d, _)| d == device)
    }
}

/// Parse a wire body. Anything malformed (bad base64, another version, truncated, no ciphertext
/// tag) is [`Error::Format`].
pub fn parse(body: &str) -> Result<Sealed, Error> {
    let bytes = b64().decode(body.as_bytes()).map_err(|_| Error::Format)?;
    if bytes.len() < FIXED || bytes[0] != DM_VERSION {
        return Err(Error::Format);
    }
    let sender_dev: DeviceId = bytes[1..1 + DEVICE_ID_LEN].try_into().map_err(|_| Error::Format)?;
    let eph_pub: [u8; KEY_LEN] = bytes[1 + DEVICE_ID_LEN..1 + DEVICE_ID_LEN + KEY_LEN].try_into().map_err(|_| Error::Format)?;
    let n = bytes[FIXED - 1] as usize;
    let header_len = FIXED + n * ENTRY_LEN;
    if n == 0 || bytes.len() < header_len + TAG_LEN {
        return Err(Error::Format);
    }
    let mut entries = Vec::with_capacity(n);
    for i in 0..n {
        let at = FIXED + i * ENTRY_LEN;
        let dev: DeviceId = bytes[at..at + DEVICE_ID_LEN].try_into().map_err(|_| Error::Format)?;
        let w: [u8; WRAPPED_KEY_LEN] = bytes[at + DEVICE_ID_LEN..at + ENTRY_LEN].try_into().map_err(|_| Error::Format)?;
        entries.push((dev, w));
    }
    Ok(Sealed { sender_dev, eph_pub, entries, bytes, header_len })
}

/// Why a line did not open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenError {
    /// Not wrapped for this device: it did not exist (or was not known) when the line was sent.
    NotForThisDevice,
    /// Wrong key, wrong conversation or author, altered, or malformed.
    Failed,
}

/// Open a parsed line on `recipient` (this device), knowing the sender device's directory key.
/// The plaintext is wiped when dropped.
pub fn open(recipient: &Identity, ctx: DmContext, sender_pub: &[u8; KEY_LEN], sealed: &Sealed) -> Result<Zeroizing<Vec<u8>>, OpenError> {
    let Some((_, wrapped)) = sealed.entries.iter().find(|(d, _)| *d == recipient.device_id) else {
        return Err(OpenError::NotForThisDevice);
    };
    let key = envelope::unwrap_one(recipient, &sealed.sender_dev, sender_pub, &sealed.eph_pub, wrapped, &ctx.wrap_context()).map_err(|_| OpenError::Failed)?;
    let (header, ct) = sealed.bytes.split_at(sealed.header_len);
    let mut buf = Zeroizing::new(ct.to_vec());
    let n = crate::aead_key(&key)
        .map_err(|_| OpenError::Failed)?
        .open_in_place(crate::nonce0(), aead::Aad::from(ctx.aad(header)), &mut buf[..])
        .map_err(|_| OpenError::Failed)?
        .len();
    buf.truncate(n);
    Ok(buf)
}

/// The plaintext of a line.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmPlain {
    /// The text (may be empty when there are images).
    pub t: String,
    /// A random uuid (replays are dropped by it).
    pub id: String,
    /// The sender's clock (Unix ms).
    pub ts: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub att: Vec<AttKey>,
}

impl fmt::Debug for DmPlain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the text: lengths only.
        f.debug_struct("DmPlain").field("t_len", &self.t.len()).field("att", &self.att.len()).finish_non_exhaustive()
    }
}

impl Drop for DmPlain {
    fn drop(&mut self) {
        self.t.zeroize();
    }
}

impl DmPlain {
    pub fn to_json(&self) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(serde_json::to_vec(self).unwrap_or_default())
    }

    pub fn from_json(b: &[u8]) -> Result<DmPlain, Error> {
        serde_json::from_slice(b).map_err(|_| Error::Format)
    }
}

/// One image of a line: the attachment id on the hub, its key (base64url) and what the hub does
/// not know any more (type, size, thumbnail size, animated).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttKey {
    pub id: i64,
    pub k: String,
    pub mime: String,
    pub w: u32,
    pub h: u32,
    pub tw: u32,
    pub th: u32,
    /// An animation (animated WebP or GIF): the viewer plays it. Absent when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub anim: bool,
}

impl fmt::Debug for AttKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AttKey").field("id", &self.id).field("mime", &self.mime).field("w", &self.w).field("h", &self.h).finish_non_exhaustive()
    }
}

impl Drop for AttKey {
    fn drop(&mut self) {
        self.k.zeroize();
    }
}

impl AttKey {
    /// The key, back from base64url (exactly 32 bytes).
    pub fn key(&self) -> Option<Zeroizing<[u8; KEY_LEN]>> {
        let v = Zeroizing::new(b64().decode(self.k.as_bytes()).ok()?);
        let mut out = Zeroizing::new([0u8; KEY_LEN]);
        if v.len() != KEY_LEN {
            return None;
        }
        out.copy_from_slice(&v);
        Some(out)
    }
}

/// A key as [`AttKey::k`] carries it.
pub fn key_to_b64(key: &[u8; KEY_LEN]) -> String {
    b64().encode(key)
}

/// A random uuid (version 4, lowercase), for [`DmPlain::id`].
pub fn new_message_id() -> Result<String, Error> {
    let mut b = crate::random::<16>()?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = crate::hex(&b);
    Ok(format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]))
}

/// A fresh random key for one image.
pub fn new_blob_key() -> Result<Zeroizing<[u8; KEY_LEN]>, Error> {
    Ok(Zeroizing::new(crate::random::<KEY_LEN>()?))
}

/// Which part of an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blob {
    /// The file (nonce 0).
    Main,
    /// Its thumbnail (nonce 1).
    Thumb,
}

fn blob_nonce(which: Blob) -> aead::Nonce {
    let mut n = [0u8; aead::NONCE_LEN];
    if which == Blob::Thumb {
        n[aead::NONCE_LEN - 1] = 1;
    }
    aead::Nonce::assume_unique_for_key(n)
}

/// Seal one part of an image with its key.
pub fn seal_blob(key: &[u8; KEY_LEN], which: Blob, data: &[u8]) -> Result<Vec<u8>, Error> {
    let mut buf = Vec::with_capacity(data.len() + TAG_LEN);
    buf.extend_from_slice(data);
    crate::aead_key(key)?.seal_in_place_append_tag(blob_nonce(which), aead::Aad::empty(), &mut buf).map_err(|_| Error::Format)?;
    Ok(buf)
}

/// Open one part of an image ([`Error::Open`]: wrong key, wrong part, altered or truncated).
pub fn open_blob(key: &[u8; KEY_LEN], which: Blob, data: &[u8]) -> Result<Vec<u8>, Error> {
    if data.len() < TAG_LEN {
        return Err(Error::Open);
    }
    let mut buf = data.to_vec();
    let n = crate::aead_key(key)?.open_in_place(blob_nonce(which), aead::Aad::empty(), &mut buf).map_err(|_| Error::Open)?.len();
    buf.truncate(n);
    Ok(buf)
}
