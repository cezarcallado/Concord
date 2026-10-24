//! E1: the key exchange of end-to-end encrypted calls (voice, cameras, Go Live).
//!
//! Media is encrypted by LiveKit's frame cryptor (AES-GCM, per-sender keys, a ring of
//! [`KEY_RING`] indexes). Every member of a room draws its own random 32-byte media key and hands
//! it to each other member's device over LiveKit's reliable data channel (topic [`TOPIC`],
//! addressed to that person only), wrapped with the E0 envelope ([`crate::wrap`]) so only that
//! device can open it, and only as coming from the sender's directory device:
//!
//! ```text
//! Hello     = [1][1][device_id 16][cur_idx 1]                                    (19 bytes)
//! SenderKey = [1][2][from_dev 16][to_dev 16][idx 1][eph_pub 32][wrapped 48]      (115 bytes)
//! context   = "concord-media-key-v1" || len(room) || room || len(from) || from
//!             || len(to) || to || idx                    (lengths: one byte each)
//! ```
//!
//! `from` / `to` are the LiveKit identities (user ids). The context binds a wrap to its room, its
//! sender, its recipient and its key index: a key cannot be replayed into another room, from
//! another person, to another person, or under another index.
//!
//! Nothing here logs, and `Debug` never prints a key.

use zeroize::Zeroizing;

use crate::{DeviceId, Error, Identity, Wrapped, DEVICE_ID_LEN, KEY_LEN, TAG_LEN};

/// The data channel topic of the key exchange.
pub const TOPIC: &str = "concord-e2ee";
/// The format's version (first byte of every message).
pub const VERSION: u8 = 1;
/// Indexes of the key ring (LiveKit's `key_ring_size`): key indexes are `0..KEY_RING`.
pub const KEY_RING: u8 = 16;
/// LiveKit's key derivation salt for concord's media keys (`ratchet_salt`).
pub const RATCHET_SALT: &[u8] = b"concord-media-v1";
/// The prefix of a wrap's context.
pub const CONTEXT_LABEL: &[u8] = b"concord-media-key-v1";

const KIND_HELLO: u8 = 1;
const KIND_SENDER_KEY: u8 = 2;
/// Bytes of a [`Message::Hello`].
pub const HELLO_LEN: usize = 2 + DEVICE_ID_LEN + 1;
/// Bytes of a wrapped media key (the key and the AEAD tag).
pub const WRAPPED_LEN: usize = KEY_LEN + TAG_LEN;
/// Bytes of a [`Message::SenderKey`].
pub const SENDER_KEY_LEN: usize = 2 + 2 * DEVICE_ID_LEN + 1 + KEY_LEN + WRAPPED_LEN;

/// A media key (wiped on drop).
pub type MediaKey = Zeroizing<[u8; KEY_LEN]>;

/// A fresh random media key.
pub fn new_media_key() -> Result<MediaKey, Error> {
    Ok(Zeroizing::new(crate::random::<KEY_LEN>()?))
}

/// One message of the key exchange.
#[derive(Clone, PartialEq, Eq)]
pub enum Message {
    /// "I am here with this device, using this key index": sent to everybody on joining, and to
    /// somebody whose media stays undecryptable ("send me your key").
    Hello { device_id: DeviceId, cur_idx: u8 },
    /// My media key at `idx`, wrapped for one device of the recipient.
    SenderKey(SenderKey),
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Message::Hello { device_id, cur_idx } => f.debug_struct("Hello").field("device_id", &crate::hex(device_id)).field("cur_idx", cur_idx).finish(),
            Message::SenderKey(k) => k.fmt(f),
        }
    }
}

/// A wrapped media key ([`Message::SenderKey`]).
#[derive(Clone, PartialEq, Eq)]
pub struct SenderKey {
    pub from_dev: DeviceId,
    pub to_dev: DeviceId,
    pub idx: u8,
    pub eph_pub: [u8; KEY_LEN],
    pub wrapped: [u8; WRAPPED_LEN],
}

impl std::fmt::Debug for SenderKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SenderKey").field("from_dev", &crate::hex(&self.from_dev)).field("to_dev", &crate::hex(&self.to_dev)).field("idx", &self.idx).finish_non_exhaustive()
    }
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Message::Hello { device_id, cur_idx } => {
                let mut v = Vec::with_capacity(HELLO_LEN);
                v.extend_from_slice(&[VERSION, KIND_HELLO]);
                v.extend_from_slice(device_id);
                v.push(*cur_idx);
                v
            }
            Message::SenderKey(k) => {
                let mut v = Vec::with_capacity(SENDER_KEY_LEN);
                v.extend_from_slice(&[VERSION, KIND_SENDER_KEY]);
                v.extend_from_slice(&k.from_dev);
                v.extend_from_slice(&k.to_dev);
                v.push(k.idx);
                v.extend_from_slice(&k.eph_pub);
                v.extend_from_slice(&k.wrapped);
                v
            }
        }
    }

    /// A message from the wire: exact lengths, a known version and kind, a key index inside the
    /// ring. Anything else is [`Error::Format`].
    pub fn decode(b: &[u8]) -> Result<Message, Error> {
        let Some((head, rest)) = b.split_first_chunk::<2>() else { return Err(Error::Format) };
        let [version, kind] = *head;
        if version != VERSION {
            return Err(Error::Format);
        }
        let take = |r: &[u8], at: usize, n: usize| r.get(at..at + n).ok_or(Error::Format).map(|s| s.to_vec());
        match kind {
            KIND_HELLO if b.len() == HELLO_LEN => {
                let device_id: DeviceId = take(rest, 0, DEVICE_ID_LEN)?.try_into().map_err(|_| Error::Format)?;
                let cur_idx = rest[DEVICE_ID_LEN];
                if cur_idx >= KEY_RING {
                    return Err(Error::Format);
                }
                Ok(Message::Hello { device_id, cur_idx })
            }
            KIND_SENDER_KEY if b.len() == SENDER_KEY_LEN => {
                let from_dev: DeviceId = take(rest, 0, DEVICE_ID_LEN)?.try_into().map_err(|_| Error::Format)?;
                let to_dev: DeviceId = take(rest, DEVICE_ID_LEN, DEVICE_ID_LEN)?.try_into().map_err(|_| Error::Format)?;
                let idx = rest[2 * DEVICE_ID_LEN];
                if idx >= KEY_RING {
                    return Err(Error::Format);
                }
                let eph_pub: [u8; KEY_LEN] = take(rest, 2 * DEVICE_ID_LEN + 1, KEY_LEN)?.try_into().map_err(|_| Error::Format)?;
                let wrapped: [u8; WRAPPED_LEN] = take(rest, 2 * DEVICE_ID_LEN + 1 + KEY_LEN, WRAPPED_LEN)?.try_into().map_err(|_| Error::Format)?;
                Ok(Message::SenderKey(SenderKey { from_dev, to_dev, idx, eph_pub, wrapped }))
            }
            _ => Err(Error::Format),
        }
    }
}

/// The wrap's context: what the key is for (see the module docs). Identities longer than 255
/// bytes are not ours (they are user ids) and are refused.
pub fn context(room: &str, from: &str, to: &str, idx: u8) -> Result<Vec<u8>, Error> {
    let mut v = Vec::with_capacity(CONTEXT_LABEL.len() + room.len() + from.len() + to.len() + 4);
    v.extend_from_slice(CONTEXT_LABEL);
    for part in [room, from, to] {
        let n = u8::try_from(part.len()).map_err(|_| Error::Format)?;
        v.push(n);
        v.extend_from_slice(part.as_bytes());
    }
    v.push(idx);
    Ok(v)
}

/// Wrap my media key `key` at `idx` for the device `to_dev` (directory key `to_pub`) of the person
/// `to`, in `room`, from me (`from`, my identity in the room).
pub fn seal_key(me: &Identity, room: &str, from: &str, to: &str, to_dev: &DeviceId, to_pub: &[u8; KEY_LEN], idx: u8, key: &[u8; KEY_LEN]) -> Result<SenderKey, Error> {
    if idx >= KEY_RING {
        return Err(Error::Format);
    }
    let w = crate::wrap(me, to_dev, to_pub, &context(room, from, to, idx)?, key)?;
    let wrapped: [u8; WRAPPED_LEN] = w.ciphertext.as_slice().try_into().map_err(|_| Error::Format)?;
    Ok(SenderKey { from_dev: me.device_id, to_dev: *to_dev, idx, eph_pub: w.eph_pub, wrapped })
}

/// Open a [`SenderKey`] that the person `from` sent me (`to`) in `room`. `from_pub` is the
/// directory key of `k.from_dev` UNDER `from` (the caller looks it up; a device that is not in
/// the directory under that person gives no key). Any mismatch (another device, room, person,
/// index, an altered byte) is [`Error::Open`].
pub fn open_key(me: &Identity, room: &str, from: &str, to: &str, from_pub: &[u8; KEY_LEN], k: &SenderKey) -> Result<MediaKey, Error> {
    if k.to_dev != me.device_id || k.idx >= KEY_RING {
        return Err(Error::Open);
    }
    let w = Wrapped { eph_pub: k.eph_pub, ciphertext: k.wrapped.to_vec() };
    let plain = crate::unwrap(me, &k.from_dev, from_pub, &context(room, from, to, k.idx)?, &w)?;
    if plain.len() != KEY_LEN {
        return Err(Error::Open);
    }
    let mut out = Zeroizing::new([0u8; KEY_LEN]);
    out.copy_from_slice(&plain);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Identity, Identity) {
        (Identity::generate().unwrap(), Identity::generate().unwrap())
    }

    #[test]
    fn messages_round_trip_with_exact_lengths() {
        let hello = Message::Hello { device_id: [7; 16], cur_idx: 15 };
        let b = hello.encode();
        assert_eq!(b.len(), HELLO_LEN);
        assert_eq!(&b[..2], &[1, 1]);
        assert_eq!(Message::decode(&b).unwrap(), hello);

        let (a, bob) = pair();
        let key = new_media_key().unwrap();
        let k = seal_key(&a, "voice-3", "1", "2", &bob.device_id, &bob.public_key(), 4, &key).unwrap();
        let msg = Message::SenderKey(k.clone());
        let b = msg.encode();
        assert_eq!(b.len(), SENDER_KEY_LEN);
        assert_eq!(SENDER_KEY_LEN, 115);
        assert_eq!(&b[..2], &[1, 2]);
        assert_eq!(Message::decode(&b).unwrap(), msg);

        // Wrong version, kind, length, an index outside the ring: refused.
        let mut bad = b.clone();
        bad[0] = 2;
        assert_eq!(Message::decode(&bad), Err(Error::Format));
        let mut bad = b.clone();
        bad[1] = 9;
        assert_eq!(Message::decode(&bad), Err(Error::Format));
        assert_eq!(Message::decode(&b[..b.len() - 1]), Err(Error::Format));
        let mut long = b.clone();
        long.push(0);
        assert_eq!(Message::decode(&long), Err(Error::Format));
        let mut bad = b.clone();
        bad[2 + 32] = KEY_RING;
        assert_eq!(Message::decode(&bad), Err(Error::Format));
        let mut bad = hello.encode();
        bad[18] = 16;
        assert_eq!(Message::decode(&bad), Err(Error::Format));
        assert_eq!(Message::decode(&[]), Err(Error::Format));
        assert_eq!(Message::decode(&[1]), Err(Error::Format));
        // Debug never shows the wrapped key or the ephemeral key.
        let dbg = format!("{msg:?}");
        assert!(!dbg.contains("wrapped") && !dbg.contains("eph_pub"), "{dbg}");
    }

    #[test]
    fn a_key_opens_only_for_its_room_people_and_index() {
        let (ana, bia) = pair();
        let caio = Identity::generate().unwrap();
        let key = new_media_key().unwrap();
        let k = seal_key(&ana, "voice-3", "1", "2", &bia.device_id, &bia.public_key(), 5, &key).unwrap();
        let got = open_key(&bia, "voice-3", "1", "2", &ana.public_key(), &k).unwrap();
        assert_eq!(*got, *key);

        // Another room, sender, recipient name, or index: nothing.
        assert_eq!(open_key(&bia, "voice-4", "1", "2", &ana.public_key(), &k).err(), Some(Error::Open));
        assert_eq!(open_key(&bia, "dm-1-2", "1", "2", &ana.public_key(), &k).err(), Some(Error::Open));
        assert_eq!(open_key(&bia, "voice-3", "3", "2", &ana.public_key(), &k).err(), Some(Error::Open));
        assert_eq!(open_key(&bia, "voice-3", "1", "4", &ana.public_key(), &k).err(), Some(Error::Open));
        let mut other_idx = k.clone();
        other_idx.idx = 6;
        assert_eq!(open_key(&bia, "voice-3", "1", "2", &ana.public_key(), &other_idx).err(), Some(Error::Open));
        // A device that is not the sender's directory key (the hub or a member slipping a key in).
        assert!(open_key(&bia, "voice-3", "1", "2", &caio.public_key(), &k).is_err());
        // Another sender device id on the same key.
        let mut other_dev = k.clone();
        other_dev.from_dev = caio.device_id;
        assert_eq!(open_key(&bia, "voice-3", "1", "2", &ana.public_key(), &other_dev).err(), Some(Error::Open));
        // Not for this device.
        assert_eq!(open_key(&caio, "voice-3", "1", "2", &ana.public_key(), &k).err(), Some(Error::Open));
        let mut readdressed = k.clone();
        readdressed.to_dev = caio.device_id;
        assert_eq!(open_key(&caio, "voice-3", "1", "2", &ana.public_key(), &readdressed).err(), Some(Error::Open));
        // Any altered byte.
        for i in 0..WRAPPED_LEN {
            let mut t = k.clone();
            t.wrapped[i] ^= 1;
            assert_eq!(open_key(&bia, "voice-3", "1", "2", &ana.public_key(), &t).err(), Some(Error::Open), "byte {i}");
        }
        let mut t = k.clone();
        t.eph_pub[3] ^= 1;
        assert!(open_key(&bia, "voice-3", "1", "2", &ana.public_key(), &t).is_err());
    }

    #[test]
    fn the_context_is_unambiguous() {
        // "voice-1" + "23" must not equal "voice-12" + "3".
        assert_ne!(context("voice-1", "23", "4", 0).unwrap(), context("voice-12", "3", "4", 0).unwrap());
        assert_ne!(context("r", "1", "2", 0).unwrap(), context("r", "1", "2", 1).unwrap());
        assert!(context(&"x".repeat(256), "1", "2", 0).is_err());
        assert!(context("r", "1", "2", 3).unwrap().starts_with(CONTEXT_LABEL));
        // Indexes outside the ring are never sealed.
        let (a, b) = pair();
        assert!(seal_key(&a, "r", "1", "2", &b.device_id, &b.public_key(), KEY_RING, &[1; 32]).is_err());
    }

    #[test]
    fn every_wrap_is_fresh() {
        let (a, b) = pair();
        let key = [9u8; 32];
        let k1 = seal_key(&a, "r", "1", "2", &b.device_id, &b.public_key(), 0, &key).unwrap();
        let k2 = seal_key(&a, "r", "1", "2", &b.device_id, &b.public_key(), 0, &key).unwrap();
        assert_ne!(k1.eph_pub, k2.eph_pub);
        assert_ne!(k1.wrapped, k2.wrapped);
        assert!(!k1.wrapped.windows(8).any(|w| key.windows(8).any(|k| k == w)), "the key is not in the clear");
    }
}
