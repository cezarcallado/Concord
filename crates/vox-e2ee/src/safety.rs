//! The safety code two people compare in person or on a call ("Verificar código de segurança").
//!
//! `SHA-256("concord-safety-v1" || for each person, by user id: user_id (8 bytes, big-endian) ||
//! number of keys (4 bytes, big-endian) || their device keys, sorted)`, shown as 30 digits in 6
//! groups of 5 (each group: 5 bytes of the hash as a big-endian number, modulo 100000). Both
//! people get the same code whatever the order, and any added, removed or changed device key
//! changes it.

use ring::digest;

use crate::KEY_LEN;

/// The hash's domain prefix.
pub const SAFETY_PREFIX: &[u8] = b"concord-safety-v1";

/// The code of these people and their device keys (`"12345 67890 ..."`).
pub fn safety_code(people: &[(i64, Vec<[u8; KEY_LEN]>)]) -> String {
    let mut people: Vec<(i64, Vec<[u8; KEY_LEN]>)> = people.to_vec();
    people.sort_by_key(|(id, _)| *id);
    people.dedup_by_key(|(id, _)| *id);
    let mut ctx = digest::Context::new(&digest::SHA256);
    ctx.update(SAFETY_PREFIX);
    for (id, keys) in &mut people {
        keys.sort();
        keys.dedup();
        ctx.update(&id.to_be_bytes());
        ctx.update(&(keys.len() as u32).to_be_bytes());
        for k in keys.iter() {
            ctx.update(k);
        }
    }
    let hash = ctx.finish();
    let h = hash.as_ref();
    (0..6)
        .map(|g| {
            let n = h[g * 5..g * 5 + 5].iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
            format!("{:05}", n % 100_000)
        })
        .collect::<Vec<_>>()
        .join(" ")
}
