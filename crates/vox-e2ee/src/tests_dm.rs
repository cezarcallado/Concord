//! E2's tests: sealed DM lines for 1 to 10 devices, every way a line must fail to open (each part
//! of the AAD, the header, truncation, tampering), the "before this device" path, images, and the
//! safety code.

use crate::dm::{self, Blob, DmContext, DmPlain, OpenError};
use crate::envelope::Recipient;
use crate::safety::safety_code;
use crate::{Identity, KEY_LEN};

fn ident() -> Identity {
    Identity::generate().unwrap()
}

fn recipient(i: &Identity) -> Recipient {
    Recipient { device_id: i.device_id, public_key: i.public_key() }
}

const CTX: DmContext = DmContext { dm_id: 7, sender_user: 3 };

fn plain(text: &str) -> Vec<u8> {
    let p = DmPlain { t: text.into(), id: dm::new_message_id().unwrap(), ts: 1_700_000_000_000, att: vec![] };
    p.to_json().to_vec()
}

#[test]
fn seal_and_open_for_one_to_ten_devices() {
    for n in 1..=10usize {
        let sender = ident();
        let devices: Vec<Identity> = (0..n).map(|_| ident()).collect();
        let mut list: Vec<Recipient> = devices.iter().map(recipient).collect();
        // A device listed twice is wrapped once.
        list.push(recipient(&devices[0]));
        let text = format!("oi para {n} aparelhos — çã 🎉");
        let body = dm::seal(&sender, CTX, &list, &plain(&text)).unwrap();
        assert!(!body.contains(&text) && !body.contains("para"), "no plaintext in the body");
        let sealed = dm::parse(&body).unwrap();
        assert_eq!(sealed.entries.len(), n);
        assert_eq!(sealed.sender_dev, sender.device_id);
        for d in &devices {
            assert!(sealed.is_for(&d.device_id));
            let out = dm::open(d, CTX, &sender.public_key(), &sealed).unwrap();
            let p = DmPlain::from_json(&out).unwrap();
            assert_eq!(p.t, text);
            assert_eq!(p.id.len(), 36);
        }
    }
}

#[test]
fn the_sender_can_read_its_own_line_when_it_is_a_recipient() {
    let me = ident();
    let my_other = ident();
    let peer = ident();
    let body = dm::seal(&me, CTX, &[recipient(&peer), recipient(&my_other), recipient(&me)], &plain("meu")).unwrap();
    let s = dm::parse(&body).unwrap();
    for d in [&me, &my_other, &peer] {
        assert_eq!(DmPlain::from_json(&dm::open(d, CTX, &me.public_key(), &s).unwrap()).unwrap().t, "meu");
    }
}

#[test]
fn a_device_missing_from_the_list_takes_the_before_this_device_path() {
    let sender = ident();
    let peer = ident();
    let newcomer = ident();
    let s = dm::parse(&dm::seal(&sender, CTX, &[recipient(&peer)], &plain("antes")).unwrap()).unwrap();
    assert!(!s.is_for(&newcomer.device_id));
    assert_eq!(dm::open(&newcomer, CTX, &sender.public_key(), &s), Err(OpenError::NotForThisDevice));
    // Its own key under a listed device id does not help either.
    let mut forged = s.clone();
    forged.entries[0].0 = newcomer.device_id;
    assert_eq!(dm::open(&newcomer, CTX, &sender.public_key(), &forged), Err(OpenError::Failed));
}

#[test]
fn every_part_of_the_aad_and_the_keys_matter() {
    let sender = ident();
    let peer = ident();
    let other = ident();
    let body = dm::seal(&sender, CTX, &[recipient(&peer), recipient(&other)], &plain("segredo")).unwrap();
    let s = dm::parse(&body).unwrap();
    assert!(dm::open(&peer, CTX, &sender.public_key(), &s).is_ok());
    // Another conversation, another author, another sender key.
    for ctx in [DmContext { dm_id: 8, ..CTX }, DmContext { sender_user: 4, ..CTX }, DmContext { dm_id: -7, sender_user: 3 }] {
        assert_eq!(dm::open(&peer, ctx, &sender.public_key(), &s), Err(OpenError::Failed), "{ctx:?}");
    }
    assert_eq!(dm::open(&peer, CTX, &other.public_key(), &s), Err(OpenError::Failed));
    // Any byte of the header changed (version, sender device, ephemeral key, count, any entry,
    // including the OTHER recipient's) makes it fail for this recipient, or makes it unreadable.
    let raw = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &body).unwrap();
    let header_len = 1 + 16 + 32 + 1 + 2 * 64;
    for i in 0..header_len {
        let mut t = raw.clone();
        t[i] ^= 0x01;
        let tb = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &t);
        let ok = dm::parse(&tb).ok().and_then(|ts| dm::open(&peer, CTX, &sender.public_key(), &ts).ok());
        assert!(ok.is_none(), "header byte {i} changed and the line still opened");
    }
}

#[test]
fn truncated_or_tampered_bodies_fail() {
    let sender = ident();
    let peer = ident();
    let body = dm::seal(&sender, CTX, &[recipient(&peer)], &plain("inteiro")).unwrap();
    let raw = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &body).unwrap();
    for cut in 0..raw.len() {
        let tb = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &raw[..cut]);
        let opened = dm::parse(&tb).ok().and_then(|s| dm::open(&peer, CTX, &sender.public_key(), &s).ok());
        assert!(opened.is_none(), "cut at {cut} still opened");
    }
    for i in (1 + 16 + 32 + 1 + 64)..raw.len() {
        let mut t = raw.clone();
        t[i] ^= 0x80;
        let tb = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &t);
        assert_eq!(dm::open(&peer, CTX, &sender.public_key(), &dm::parse(&tb).unwrap()), Err(OpenError::Failed), "ciphertext byte {i}");
    }
    // Extra bytes at the end, another version, garbage.
    let mut longer = raw.clone();
    longer.push(0);
    let tb = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &longer);
    assert_eq!(dm::open(&peer, CTX, &sender.public_key(), &dm::parse(&tb).unwrap()), Err(OpenError::Failed));
    let mut v2 = raw.clone();
    v2[0] = 2;
    assert!(dm::parse(&base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &v2)).is_err());
    assert!(dm::parse("não é base64!").is_err());
    assert!(dm::parse("").is_err());
    // No recipients at all cannot be sealed.
    assert!(dm::seal(&sender, CTX, &[], b"x").is_err());
}

#[test]
fn every_line_has_its_own_key_and_ephemeral() {
    let sender = ident();
    let peer = ident();
    let a = dm::parse(&dm::seal(&sender, CTX, &[recipient(&peer)], &plain("igual")).unwrap()).unwrap();
    let b = dm::parse(&dm::seal(&sender, CTX, &[recipient(&peer)], &plain("igual")).unwrap()).unwrap();
    assert_ne!(a.eph_pub, b.eph_pub);
    assert_ne!(a.entries[0].1, b.entries[0].1);
    assert_ne!(dm::new_message_id().unwrap(), dm::new_message_id().unwrap());
    let id = dm::new_message_id().unwrap();
    assert_eq!((id.len(), &id[14..15], id.matches('-').count()), (36, "4", 4));
}

#[test]
fn the_plaintext_json_shape() {
    let k = dm::new_blob_key().unwrap();
    let p = DmPlain {
        t: "olha".into(),
        id: "u".into(),
        ts: 5,
        att: vec![dm::AttKey { id: 9, k: dm::key_to_b64(&k), mime: "image/webp".into(), w: 640, h: 480, tw: 400, th: 300, anim: false }],
    };
    let json = String::from_utf8(p.to_json().to_vec()).unwrap();
    assert!(json.starts_with(r#"{"t":"olha","id":"u","ts":5,"att":[{"id":9,"k":""#), "{json}");
    let back = DmPlain::from_json(json.as_bytes()).unwrap();
    assert_eq!(*back.att[0].key().unwrap(), *k);
    assert!(!format!("{back:?}").contains("olha") && !format!("{:?}", back.att[0]).contains(&back.att[0].k));
    // No images: no "att" at all.
    let bare = DmPlain { t: "x".into(), id: "u".into(), ts: 1, att: vec![] };
    assert_eq!(String::from_utf8(bare.to_json().to_vec()).unwrap(), r#"{"t":"x","id":"u","ts":1}"#);
    assert!(DmPlain::from_json(br#"{"t":"x"}"#).is_err());
}

#[test]
fn image_parts_seal_with_their_own_nonce() {
    let k = dm::new_blob_key().unwrap();
    let img: Vec<u8> = (0..300u32).map(|i| (i * 7 % 251) as u8).collect();
    let main = dm::seal_blob(&k, Blob::Main, &img).unwrap();
    let thumb = dm::seal_blob(&k, Blob::Thumb, &img).unwrap();
    assert_ne!(main, thumb, "the two parts never share a nonce");
    assert!(!main.windows(16).any(|w| w == &img[..16]));
    assert_eq!(dm::open_blob(&k, Blob::Main, &main).unwrap(), img);
    assert_eq!(dm::open_blob(&k, Blob::Thumb, &thumb).unwrap(), img);
    assert!(dm::open_blob(&k, Blob::Thumb, &main).is_err());
    assert!(dm::open_blob(&[9u8; KEY_LEN], Blob::Main, &main).is_err());
    assert!(dm::open_blob(&k, Blob::Main, &main[..main.len() - 1]).is_err());
    assert!(dm::open_blob(&k, Blob::Main, &[]).is_err());
}

#[test]
fn the_safety_code_ignores_order_and_follows_the_keys() {
    let (a1, a2, b1) = ([1u8; 32], [2u8; 32], [3u8; 32]);
    let code = safety_code(&[(3, vec![a1, a2]), (9, vec![b1])]);
    assert_eq!(code.len(), 35);
    let groups: Vec<&str> = code.split(' ').collect();
    assert_eq!(groups.len(), 6);
    assert!(groups.iter().all(|g| g.len() == 5 && g.chars().all(|c| c.is_ascii_digit())));
    // Whoever computes it, in whatever order.
    assert_eq!(safety_code(&[(9, vec![b1]), (3, vec![a2, a1])]), code);
    // A device added, removed, or another key: another code.
    assert_ne!(safety_code(&[(3, vec![a1, a2]), (9, vec![b1, [4u8; 32]])]), code);
    assert_ne!(safety_code(&[(3, vec![a1]), (9, vec![b1])]), code);
    assert_ne!(safety_code(&[(3, vec![a1, a2]), (9, vec![[5u8; 32]])]), code);
    // The keys belong to their person: moving one across is another code.
    assert_ne!(safety_code(&[(3, vec![a1]), (9, vec![a2, b1])]), code);
    assert_ne!(safety_code(&[(4, vec![a1, a2]), (9, vec![b1])]), code);
}

fn seq<const N: usize>(start: u8) -> [u8; N] {
    std::array::from_fn(|i| start.wrapping_add(i as u8))
}

/// The whole `e2e1` body against a vector computed outside this code (Python's `cryptography`:
/// X25519, HKDF-SHA256, ChaCha20-Poly1305), with the line key and the ephemeral key fixed: the
/// header layout, the wrap context, the AAD and the image nonces cannot change unnoticed.
#[test]
fn dm_known_answer() {
    let mut s = seq::<32>(0x00);
    let sender = Identity::from_parts(seq(0x01), &mut s);
    let mut r = seq::<32>(0x20);
    let peer = Identity::from_parts(seq(0x11), &mut r);
    let pt = br#"{"t":"oi","id":"x","ts":1}"#;
    let body = dm::seal_with(&sender, CTX, &[recipient(&peer), recipient(&sender)], pt, &seq(0x60), x25519_dalek::StaticSecret::from(seq::<32>(0x40))).unwrap();
    assert_eq!(
        body,
        "AQECAwQFBgcICQoLDA0ODxB5pjHu3hv5yY8SAyzerdDnoHk5j8eGuIzIRuyJr4WlGgIREhMUFRYXGBkaGxwdHh8gQnNRj-5AZFbQv7bC_mOao34JA6IG59Gcb2TLNldxRe8ezNmCgdtRtv5ckXa7aJ-2AQIDBAUGBwgJCgsMDQ4PENrxa4MSyQm94YR057XYzRgkzkEHn4CVaRVikOkro7bTZjWU27_FRm6AJfgmpYf7mo21tGEpwIsq46fVQ9Z1HmmoatFks0r2Kz9Gn4SbtVDnD_Of_LMpTiQTow"
    );
    for who in [&peer, &sender] {
        assert_eq!(&dm::open(who, CTX, &sender.public_key(), &dm::parse(&body).unwrap()).unwrap()[..], &pt[..]);
    }
    let k2 = seq::<32>(0x90);
    let hex = |v: Vec<u8>| crate::hex(&v);
    assert_eq!(hex(dm::seal_blob(&k2, Blob::Main, b"img").unwrap()), "f2db946be4c39f32494396ebc501ca6d30e8ac");
    assert_eq!(hex(dm::seal_blob(&k2, Blob::Thumb, b"img").unwrap()), "fa5db914a1d343fb0a359740d7b3b3ae39749a");
}

/// What a malicious hub or peer can put in a body: every count of entries, duplicated device ids,
/// the all-zero / low-order ephemeral key, huge or empty inputs. Never a panic, always an error.
#[test]
fn hostile_headers_fail_cleanly() {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let sender = ident();
    let peer = ident();
    let body = dm::seal(&sender, CTX, &[recipient(&peer)], &plain("x")).unwrap();
    let raw = b64.decode(&body).unwrap();
    // n = 0 .. 255 over the same bytes (too short for most, wrong AAD for the rest).
    for n in 0..=255u8 {
        let mut t = raw.clone();
        t[1 + 16 + 32] = n;
        let opened = dm::parse(&b64.encode(&t)).ok().and_then(|s| dm::open(&peer, CTX, &sender.public_key(), &s).ok());
        assert!(n == 1 || opened.is_none(), "n = {n}");
    }
    // n = 255 with room for every entry: parses, does not open.
    let mut big = raw[..1 + 16 + 32].to_vec();
    big.push(255);
    big.extend(vec![0u8; 255 * 64 + 16]);
    let s = dm::parse(&b64.encode(&big)).unwrap();
    assert_eq!(s.entries.len(), 255);
    assert!(dm::open(&peer, CTX, &sender.public_key(), &s).is_err());
    // My device id twice: the first entry is used, the edit breaks the AAD anyway.
    let mut dup = raw[..1 + 16 + 32].to_vec();
    dup.push(2);
    dup.extend_from_slice(&raw[1 + 16 + 32 + 1..1 + 16 + 32 + 1 + 64]);
    dup.extend_from_slice(&raw[1 + 16 + 32 + 1..]);
    assert_eq!(dm::open(&peer, CTX, &sender.public_key(), &dm::parse(&b64.encode(&dup)).unwrap()), Err(OpenError::Failed));
    // A zero (low-order) ephemeral key.
    let mut zero = raw.clone();
    zero[1 + 16..1 + 16 + 32].fill(0);
    assert_eq!(dm::open(&peer, CTX, &sender.public_key(), &dm::parse(&b64.encode(&zero)).unwrap()), Err(OpenError::Failed));
    // A sender key of zero.
    assert_eq!(dm::open(&peer, CTX, &[0; KEY_LEN], &dm::parse(&body).unwrap()), Err(OpenError::Failed));
    // Garbage of every small length, and a large one.
    for len in 0..200 {
        let _ = dm::parse(&b64.encode(vec![1u8; len]));
    }
    assert!(dm::parse(&"A".repeat(1 << 20)).is_err(), "version 0");
    let mut huge = vec![0u8; 1 << 20];
    huge[0] = 1;
    huge[1 + 16 + 32] = 1;
    assert!(dm::parse(&b64.encode(&huge)).is_ok_and(|s| dm::open(&peer, CTX, &sender.public_key(), &s).is_err()));
    // Hostile plaintexts: not JSON, wrong types, a key of the wrong size.
    for bad in [&b"not json"[..], br#"{"t":1,"id":"x","ts":0}"#, br#"{"t":"x"}"#, br#"[1,2]"#] {
        assert!(DmPlain::from_json(bad).is_err());
    }
    let p = DmPlain::from_json(br#"{"t":"","id":"x","ts":0,"att":[{"id":1,"k":"AAAA","mime":"text/html","w":4294967295,"h":0,"tw":0,"th":0}]}"#).unwrap();
    assert!(p.att[0].key().is_none(), "a 3-byte key");
    assert!(dm::open_blob(&[7; KEY_LEN], Blob::Main, &[]).is_err() && dm::open_blob(&[7; KEY_LEN], Blob::Main, &[0; 15]).is_err());
}
