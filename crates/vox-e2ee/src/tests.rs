//! The envelope's tests: round trips, every way a wrap must fail, the freshness of the ephemeral
//! key, and known-answer vectors made independently (Python's `cryptography`: X25519, HKDF-SHA256,
//! ChaCha20-Poly1305), so a refactor cannot change the format without a test noticing.

use super::*;

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn arr<const N: usize>(s: &str) -> [u8; N] {
    unhex(s).try_into().unwrap()
}

fn seq<const N: usize>(start: u8) -> [u8; N] {
    std::array::from_fn(|i| start.wrapping_add(i as u8))
}

fn ident(dev: DeviceId, secret: [u8; KEY_LEN]) -> Identity {
    let mut s = secret;
    Identity::from_parts(dev, &mut s)
}

struct Pair {
    alice: Identity,
    bob: Identity,
}

fn pair() -> Pair {
    Pair { alice: Identity::generate().unwrap(), bob: Identity::generate().unwrap() }
}

const CTX: &[u8] = b"concord-test-v1|dm 7|user 3 -> user 9";
const SECRET: &[u8] = b"a 32-byte message key, for real.";

#[test]
fn roundtrip() {
    let p = pair();
    let w = wrap(&p.alice, &p.bob.device_id, &p.bob.public_key(), CTX, SECRET).unwrap();
    assert_eq!(w.ciphertext.len(), SECRET.len() + TAG_LEN);
    assert_ne!(&w.ciphertext[..SECRET.len()], SECRET, "the secret is not in the clear");
    let out = unwrap(&p.bob, &p.alice.device_id, &p.alice.public_key(), CTX, &w).unwrap();
    assert_eq!(&out[..], SECRET);
    // Through the byte form too, and with an empty plaintext and context.
    let back = Wrapped::from_bytes(&w.to_bytes()).unwrap();
    assert_eq!(back, w);
    let empty = wrap(&p.alice, &p.bob.device_id, &p.bob.public_key(), b"", b"").unwrap();
    assert_eq!(empty.ciphertext.len(), TAG_LEN);
    assert!(unwrap(&p.bob, &p.alice.device_id, &p.alice.public_key(), b"", &empty).unwrap().is_empty());
    // A device can wrap for itself (another device of the same person does exactly this).
    let me = wrap(&p.alice, &p.alice.device_id, &p.alice.public_key(), CTX, SECRET).unwrap();
    assert_eq!(&unwrap(&p.alice, &p.alice.device_id, &p.alice.public_key(), CTX, &me).unwrap()[..], SECRET);
}

#[test]
fn a_kept_identity_is_the_same_identity() {
    let a = Identity::generate().unwrap();
    let mut secret = *a.secret_bytes();
    let again = Identity::from_parts(a.device_id, &mut secret);
    assert_eq!(secret, [0u8; KEY_LEN], "the caller's copy is wiped");
    assert_eq!(again.public_key(), a.public_key());
    let b = Identity::generate().unwrap();
    let w = wrap(&b, &a.device_id, &a.public_key(), CTX, SECRET).unwrap();
    assert_eq!(&unwrap(&again, &b.device_id, &b.public_key(), CTX, &w).unwrap()[..], SECRET);
}

#[test]
fn the_wrong_recipient_cannot_open() {
    let p = pair();
    let eve = Identity::generate().unwrap();
    let w = wrap(&p.alice, &p.bob.device_id, &p.bob.public_key(), CTX, SECRET).unwrap();
    assert_eq!(unwrap(&eve, &p.alice.device_id, &p.alice.public_key(), CTX, &w), Err(Error::Open));
    // Even with Bob's device id (only the key matters for opening, both are bound).
    let eve_as_bob = ident(p.bob.device_id, *Identity::generate().unwrap().secret_bytes());
    assert_eq!(unwrap(&eve_as_bob, &p.alice.device_id, &p.alice.public_key(), CTX, &w), Err(Error::Open));
}

#[test]
fn the_wrong_sender_key_fails() {
    let p = pair();
    let mallory = Identity::generate().unwrap();
    let w = wrap(&p.alice, &p.bob.device_id, &p.bob.public_key(), CTX, SECRET).unwrap();
    // Bob thinks it came from Mallory's key: no.
    assert_eq!(unwrap(&p.bob, &p.alice.device_id, &mallory.public_key(), CTX, &w), Err(Error::Open));
    // Mallory wraps and claims Alice's device: Bob checks against Alice's key and refuses.
    let forged = wrap(&mallory, &p.bob.device_id, &p.bob.public_key(), CTX, SECRET).unwrap();
    assert_eq!(unwrap(&p.bob, &p.alice.device_id, &p.alice.public_key(), CTX, &forged), Err(Error::Open));
}

#[test]
fn the_wrong_context_fails() {
    let p = pair();
    let w = wrap(&p.alice, &p.bob.device_id, &p.bob.public_key(), CTX, SECRET).unwrap();
    for other in [&b""[..], b"concord-test-v1|dm 8|user 3 -> user 9", &CTX[..CTX.len() - 1], b"concord-test-v1|dm 7|user 3 -> user 9 "] {
        assert_eq!(unwrap(&p.bob, &p.alice.device_id, &p.alice.public_key(), other, &w), Err(Error::Open));
    }
}

#[test]
fn the_wrong_device_ids_fail() {
    let p = pair();
    let w = wrap(&p.alice, &p.bob.device_id, &p.bob.public_key(), CTX, SECRET).unwrap();
    // Another sender device id.
    let mut sdev = p.alice.device_id;
    sdev[0] ^= 1;
    assert_eq!(unwrap(&p.bob, &sdev, &p.alice.public_key(), CTX, &w), Err(Error::Open));
    // Bob's key under another device id (a wrap for Bob's other device is not for this one).
    let mut bob2 = ident(p.bob.device_id, *p.bob.secret_bytes());
    bob2.renew_device_id().unwrap();
    assert_ne!(bob2.device_id, p.bob.device_id);
    assert_eq!(bob2.public_key(), p.bob.public_key());
    assert_eq!(unwrap(&bob2, &p.alice.device_id, &p.alice.public_key(), CTX, &w), Err(Error::Open));
    // Swapping the two ids is not the same either.
    let w2 = wrap(&p.alice, &p.alice.device_id, &p.bob.public_key(), CTX, SECRET).unwrap();
    let bob_as_alice_dev = ident(p.alice.device_id, *p.bob.secret_bytes());
    assert!(unwrap(&bob_as_alice_dev, &p.alice.device_id, &p.alice.public_key(), CTX, &w2).is_ok());
    let bob_other = ident(p.bob.device_id, *p.bob.secret_bytes());
    assert_eq!(unwrap(&bob_other, &p.bob.device_id, &p.alice.public_key(), CTX, &w2), Err(Error::Open));
}

#[test]
fn tampering_anywhere_fails() {
    let p = pair();
    let w = wrap(&p.alice, &p.bob.device_id, &p.bob.public_key(), CTX, SECRET).unwrap();
    let bytes = w.to_bytes();
    for i in 0..bytes.len() {
        for bit in [0x01u8, 0x80] {
            let mut b = bytes.clone();
            b[i] ^= bit;
            let t = Wrapped::from_bytes(&b).unwrap();
            let r = unwrap(&p.bob, &p.alice.device_id, &p.alice.public_key(), CTX, &t);
            assert!(r.is_err(), "flipping bit {bit:#x} of byte {i} went unnoticed");
        }
    }
    // Truncated or extended.
    let mut longer = w.clone();
    longer.ciphertext.push(0);
    assert_eq!(unwrap(&p.bob, &p.alice.device_id, &p.alice.public_key(), CTX, &longer), Err(Error::Open));
    let mut shorter = w.clone();
    shorter.ciphertext.pop();
    assert_eq!(unwrap(&p.bob, &p.alice.device_id, &p.alice.public_key(), CTX, &shorter), Err(Error::Open));
    shorter.ciphertext.truncate(TAG_LEN - 1);
    assert_eq!(unwrap(&p.bob, &p.alice.device_id, &p.alice.public_key(), CTX, &shorter), Err(Error::Format));
    assert_eq!(Wrapped::from_bytes(&bytes[..KEY_LEN + TAG_LEN - 1]), Err(Error::Format));
}

#[test]
fn the_ephemeral_key_is_fresh_every_time() {
    let p = pair();
    let mut seen = std::collections::HashSet::new();
    let mut cts = std::collections::HashSet::new();
    for _ in 0..64 {
        let w = wrap(&p.alice, &p.bob.device_id, &p.bob.public_key(), CTX, SECRET).unwrap();
        assert!(seen.insert(w.eph_pub), "an ephemeral key came back");
        assert!(cts.insert(w.ciphertext.clone()), "the same plaintext gave the same ciphertext");
        assert_ne!(w.eph_pub, p.alice.public_key());
    }
}

#[test]
fn low_order_keys_are_refused() {
    let p = pair();
    // The all-zero point and the order-8 point of RFC 7748 §6.1's list.
    let zero = [0u8; KEY_LEN];
    let order8: [u8; KEY_LEN] = arr("e0eb7a7c3b41b8ae1656e3faf19fc46ada098deb9c32b1fd866205165f49b800");
    for bad in [zero, order8] {
        assert_eq!(wrap(&p.alice, &p.bob.device_id, &bad, CTX, SECRET), Err(Error::WeakKey));
        let w = wrap(&p.alice, &p.bob.device_id, &p.bob.public_key(), CTX, SECRET).unwrap();
        assert_eq!(unwrap(&p.bob, &p.alice.device_id, &bad, CTX, &w), Err(Error::WeakKey));
        let w = Wrapped { eph_pub: bad, ..w };
        assert_eq!(unwrap(&p.bob, &p.alice.device_id, &p.alice.public_key(), CTX, &w), Err(Error::WeakKey));
    }
}

/// RFC 7748 §6.1: the X25519 underneath is the standard one.
#[test]
fn x25519_rfc7748_vector() {
    let a = ident([0; DEVICE_ID_LEN], arr("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a"));
    let b = ident([0; DEVICE_ID_LEN], arr("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb"));
    assert_eq!(a.public_key(), arr::<32>("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a"));
    assert_eq!(b.public_key(), arr::<32>("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f"));
    assert_eq!(*dh(&a.secret, &b.public_key()).unwrap(), arr::<32>("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742"));
}

struct Kat {
    s_sec: [u8; 32],
    r_sec: [u8; 32],
    e_sec: [u8; 32],
    sdev: DeviceId,
    rdev: DeviceId,
    ctx: &'static [u8],
    pt: Vec<u8>,
    s_pub: &'static str,
    r_pub: &'static str,
    e_pub: &'static str,
    key: &'static str,
    ct: &'static str,
}

fn kats() -> Vec<Kat> {
    vec![
        Kat {
            s_sec: seq(0x00),
            r_sec: seq(0x20),
            e_sec: seq(0x40),
            sdev: seq(0x01),
            rdev: seq(0x11),
            ctx: b"concord-test-context",
            pt: seq::<32>(0x80).to_vec(),
            s_pub: "8f40c5adb68f25624ae5b214ea767a6ec94d829d3d7b5e1ad1ba6f3e2138285f",
            r_pub: "358072d6365880d1aeea329adf9121383851ed21a28e3b75e965d0d2cd166254",
            e_pub: "79a631eede1bf9c98f12032cdeadd0e7a079398fc786b88cc846ec89af85a51a",
            key: "05b238daa5606c3b0b652f5449235d70368150efd711be6a1be4c0406b13dcda",
            ct: "603227d3df36fa8c42cdc1efdc9dbe9c56a55b4157a08264c0afc8f402fcb9271641cdeefda045411c7b50cb785647c6",
        },
        Kat {
            s_sec: [0x5a; 32],
            r_sec: [0xa5; 32],
            e_sec: [0x3c; 32],
            sdev: [0xee; 16],
            rdev: [0x77; 16],
            ctx: b"",
            pt: Vec::new(),
            s_pub: "b0d08f35b4683381489afb32825e59152d47d19bc9e050d6d5a954984c9d1e2c",
            r_pub: "5fef13fc76023a9ee6ded987b6aa93958cdc2097ef9fc845d5319c9ca100d35e",
            e_pub: "513f6639b3e84e8db530e84bba689373da3b6f3ecf45df32414738d58d792d66",
            key: "874f962b0c182b9c4a85c135e9c0b41c98dffc0a71e264a6f729b9c813b9f3b9",
            ct: "a9b3d800a9966bce2616c35e4c20d5aa",
        },
    ]
}

/// The whole composition against vectors computed outside this code: the HKDF key, then the
/// sealed bytes, then the opening.
#[test]
fn known_answers() {
    for (i, k) in kats().into_iter().enumerate() {
        let sender = ident(k.sdev, k.s_sec);
        let recipient = ident(k.rdev, k.r_sec);
        assert_eq!(hex(&sender.public_key()), k.s_pub, "vector {i}: sender public key");
        assert_eq!(hex(&recipient.public_key()), k.r_pub, "vector {i}: recipient public key");
        let eph = StaticSecret::from(k.e_sec);
        assert_eq!(hex(PublicKey::from(&eph).as_bytes()), k.e_pub, "vector {i}: ephemeral public key");
        let dh1 = dh(&eph, &recipient.public_key()).unwrap();
        let dh2 = dh(&sender.secret, &recipient.public_key()).unwrap();
        let key = derive_key(&dh1, &dh2, &k.sdev, &k.rdev, &arr(k.e_pub), k.ctx).unwrap();
        assert_eq!(hex(&key[..]), k.key, "vector {i}: HKDF output");
        let w = wrap_with_ephemeral(&sender, eph, &k.rdev, &recipient.public_key(), k.ctx, &k.pt).unwrap();
        assert_eq!(hex(&w.eph_pub), k.e_pub, "vector {i}: wrap's ephemeral key");
        assert_eq!(hex(&w.ciphertext), k.ct, "vector {i}: sealed bytes");
        let opened = unwrap(&recipient, &k.sdev, &sender.public_key(), k.ctx, &Wrapped { eph_pub: arr(k.e_pub), ciphertext: unhex(k.ct) }).unwrap();
        assert_eq!(&opened[..], &k.pt[..], "vector {i}: opened");
    }
}

#[test]
fn wire_forms() {
    let id = Identity::generate().unwrap();
    let h = id.device_id_hex();
    assert_eq!(h.len(), 32);
    assert_eq!(device_id_from_hex(&h), Some(id.device_id));
    assert_eq!(device_id_from_hex(&h.to_uppercase()), None, "lowercase only");
    assert_eq!(device_id_from_hex(&h[1..]), None);
    assert_eq!(device_id_from_hex("zz112233445566778899aabbccddeeff"), None);
    let k = id.public_key();
    let b = public_key_to_b64(&k);
    assert_eq!(b.len(), 43);
    assert!(!b.contains('=') && !b.contains('+') && !b.contains('/'));
    assert_eq!(public_key_from_b64(&b), Some(k));
    assert_eq!(public_key_from_b64(&format!("{b}=")), None, "no padding");
    assert_eq!(public_key_from_b64(&b[..42]), None);
    assert_eq!(public_key_from_b64(&public_key_to_b64(&[1; 32])[..]).map(|k| k[0]), Some(1));
    assert_eq!(public_key_from_b64("AAAA"), None, "3 bytes are not a key");
    // Debug never prints the secret.
    let dbg = format!("{id:?}");
    assert!(dbg.contains("<hidden>") && !dbg.contains(&hex(&id.secret_bytes()[..])));
    let w = wrap(&id, &id.device_id, &id.public_key(), b"", b"x").unwrap();
    assert!(!format!("{w:?}").contains(&hex(&w.ciphertext)));
}

#[test]
fn identities_are_random() {
    let a = Identity::generate().unwrap();
    let b = Identity::generate().unwrap();
    assert_ne!(a.device_id, b.device_id);
    assert_ne!(a.public_key(), b.public_key());
}
