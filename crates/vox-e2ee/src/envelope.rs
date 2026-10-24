//! One random key wrapped for many devices under ONE ephemeral key (E2: the header of a DM line).
//!
//! Each entry is the [`crate::wrap`] envelope with the ephemeral public key factored out:
//! `HKDF-SHA256(salt = "concord-wrap-v1", ikm = DH(eph, R) || DH(S_sender, R),
//! info = sender_dev || recipient_dev || eph_pub || context)`, then ChaCha20-Poly1305 with nonce 0.
//! Every recipient gets its own AEAD key (another `R`, another device id in `info`), and a device
//! is never wrapped for twice in one header, so each AEAD key is still used exactly once.

use x25519_dalek::StaticSecret;
use zeroize::Zeroizing;

use crate::{DeviceId, Error, Identity, Wrapped, DEVICE_ID_LEN, KEY_LEN, TAG_LEN};

/// Bytes of one wrapped 32-byte key (the key and the AEAD tag).
pub const WRAPPED_KEY_LEN: usize = KEY_LEN + TAG_LEN;

/// One device to wrap for: its id and its directory key.
#[derive(Clone, PartialEq, Eq)]
pub struct Recipient {
    pub device_id: DeviceId,
    pub public_key: [u8; KEY_LEN],
}

impl std::fmt::Debug for Recipient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recipient").field("device_id", &crate::hex(&self.device_id)).finish_non_exhaustive()
    }
}

/// The wrapped key for every device: the shared ephemeral public key, then one entry per device.
#[derive(Clone, PartialEq, Eq)]
pub struct MultiWrap {
    pub eph_pub: [u8; KEY_LEN],
    pub entries: Vec<(DeviceId, [u8; WRAPPED_KEY_LEN])>,
}

impl std::fmt::Debug for MultiWrap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultiWrap").field("entries", &self.entries.len()).finish_non_exhaustive()
    }
}

/// Wrap `key` from `sender` for every device of `recipients` (a device listed twice gets one
/// entry). The ephemeral secret is drawn here and wiped before returning.
pub fn wrap_for_all(sender: &Identity, recipients: &[Recipient], context: &[u8], key: &[u8; KEY_LEN]) -> Result<MultiWrap, Error> {
    let (eph, _): (StaticSecret, _) = crate::ephemeral()?;
    wrap_for_all_with(sender, eph, recipients, context, key)
}

/// [`wrap_for_all`] under a given ephemeral secret (consumed and wiped). Only [`wrap_for_all`] and
/// the known-answer tests call it: a reused ephemeral key would reuse the AEAD keys.
pub(crate) fn wrap_for_all_with(sender: &Identity, eph: StaticSecret, recipients: &[Recipient], context: &[u8], key: &[u8; KEY_LEN]) -> Result<MultiWrap, Error> {
    let eph_pub = x25519_dalek::PublicKey::from(&eph).to_bytes();
    let mut entries: Vec<(DeviceId, [u8; WRAPPED_KEY_LEN])> = Vec::with_capacity(recipients.len());
    for r in recipients {
        if entries.iter().any(|(d, _)| *d == r.device_id) {
            continue;
        }
        let w = crate::wrap_shared(sender, &eph, &r.device_id, &r.public_key, context, key)?;
        let ct: [u8; WRAPPED_KEY_LEN] = w.ciphertext.as_slice().try_into().map_err(|_| Error::Format)?;
        entries.push((r.device_id, ct));
    }
    drop(eph);
    Ok(MultiWrap { eph_pub, entries })
}

/// Open the entry of `recipient` (its own device) made by `sender_dev` (directory key
/// `sender_pub`) with the same `context`. Any mismatch is [`Error::Open`] (or [`Error::WeakKey`]).
pub fn unwrap_one(recipient: &Identity, sender_dev: &DeviceId, sender_pub: &[u8; KEY_LEN], eph_pub: &[u8; KEY_LEN], wrapped: &[u8; WRAPPED_KEY_LEN], context: &[u8]) -> Result<Zeroizing<[u8; KEY_LEN]>, Error> {
    let w = Wrapped { eph_pub: *eph_pub, ciphertext: wrapped.to_vec() };
    let plain = crate::unwrap(recipient, sender_dev, sender_pub, context, &w)?;
    let mut out = Zeroizing::new([0u8; KEY_LEN]);
    if plain.len() != KEY_LEN {
        return Err(Error::Open);
    }
    out.copy_from_slice(&plain);
    Ok(out)
}

/// Bytes of one header entry: the device id and its wrapped key.
pub const ENTRY_LEN: usize = DEVICE_ID_LEN + WRAPPED_KEY_LEN;
