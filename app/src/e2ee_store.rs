//! `%APPDATA%\concord\e2ee.json` (v1.1, phase E0 of docs/PLANO-v1.1-E2EE.md): this computer's
//! identity keys and the device keys it has seen.
//!
//! ```json
//! { "identities": [{"server_id", "user_id", "device_id", "secret", "created_ms"}],
//!   "pins": {"<server_id>": {"<user_id>": [{"device_id", "key", "first_seen_ms", "verified"}]}} }
//! ```
//!
//! - One identity per (hub `server_id`, user): a random device id and an X25519 secret. The secret
//!   is only ever stored through `settings::protect_key` (DPAPI on Windows; on macOS a `kck:`
//!   reference into the user-only secrets.json, whose entries `settings::secrets_gc` keeps
//!   because it scans this file too).
//!   If `protect` fails, nothing is stored and this computer has no end-to-end encryption
//!   ([`UNAVAILABLE_TEXT`]).
//! - The key is kept whether or not "Lembrar este computador" is ticked: it is not a login secret,
//!   and losing it makes this computer look like a new device to everybody.
//! - `pins`: every device key seen in the hub's directory, per member, trust on first use. E0
//!   records them (and tells about a new device of my own account); E2 compares them ("o código de
//!   segurança mudou", "Fulano adicionou um aparelho"), keeps "Marcar como verificado" in their
//!   `verified` flags, and opens a line from a device that left the directory with its pinned key.
//! - `sealed_dms` (E2): the conversations that were ever end-to-end encrypted on this computer.
//!   Sending in one of them without encryption is never silent: the app asks first.
//! - Autotests without their own settings folder keep all of it in memory (nothing is written, the
//!   real secrets are never touched).
//!
//! Nothing here logs a key or a secret.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use tracing::warn;
use vox_e2ee::zeroize::Zeroizing;
use vox_e2ee::Identity;
use vox_proto::{E2eeDevice, UserId};

/// What the Privacy page says when this computer cannot keep a key.
pub const UNAVAILABLE_TEXT: &str = "Não foi possível guardar a chave de criptografia neste computador";

/// Whether e2ee.json is the real file (the app) or memory only (autotests without
/// `--settings-dir`). Unset: memory only.
static PERSISTENT: OnceLock<bool> = OnceLock::new();

/// At startup (`main.rs`): the identities live in e2ee.json next to settings.json.
pub fn set_persistent(on: bool) {
    let _ = PERSISTENT.set(on);
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct E2eeFile {
    #[serde(default)]
    pub identities: Vec<StoredIdentity>,
    /// server_id -> user id -> the devices seen.
    #[serde(default)]
    pub pins: BTreeMap<String, BTreeMap<UserId, Vec<DevicePin>>>,
    /// E2: server_id -> the DMs that were ever encrypted here.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sealed_dms: BTreeMap<String, BTreeSet<vox_proto::DmId>>,
}

/// What a look at the directory changed (E2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observed {
    /// Devices of my own account this computer never saw (E0's alert).
    pub mine_new: Vec<E2eeDevice>,
    /// Other people whose keys changed since this computer first knew them: the new devices (new
    /// ids; a known id with another key is a reinstall: in `changed` only), and whether their code
    /// was verified before.
    pub others: Vec<PeerChange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerChange {
    pub user_id: UserId,
    pub added: Vec<E2eeDevice>,
    pub was_verified: bool,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredIdentity {
    pub server_id: String,
    pub user_id: UserId,
    pub device_id: String,
    /// The secret's hex through `settings::protect_key`: never the key itself.
    pub secret: String,
    pub created_ms: i64,
}

impl fmt::Debug for StoredIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredIdentity").field("server_id", &self.server_id).field("user_id", &self.user_id).field("device_id", &self.device_id).finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevicePin {
    pub device_id: String,
    /// The public key (base64url), as the directory had it.
    pub key: String,
    pub first_seen_ms: i64,
    /// Compared in person (E2's safety code); always false in E0.
    #[serde(default)]
    pub verified: bool,
}

/// Why there is no identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    /// `protect` failed, or e2ee.json cannot be written: this computer has no E2EE.
    NoKeyStorage,
    /// No randomness from the operating system.
    Random,
}

type Protect = fn(&str) -> Option<String>;

/// Memory-only "protection" (autotests without a settings folder): the value stays in this process.
fn mem_protect(s: &str) -> Option<String> {
    Some(format!("mem:{s}"))
}

fn mem_unprotect(s: &str) -> Option<String> {
    s.strip_prefix("mem:").map(str::to_string)
}

pub struct Store {
    /// `None`: memory only.
    path: Option<PathBuf>,
    pub file: E2eeFile,
    protect: Protect,
    unprotect: Protect,
    /// e2ee.json was there but could not be read (a copy was kept as e2ee.json.bad).
    damaged: bool,
    /// What the person must be told (a new identity replaced one this computer had).
    warning: Option<String>,
}

/// Told when e2ee.json was damaged and this computer got a new key.
pub const DAMAGED_TEXT: &str = "O arquivo das chaves de criptografia deste computador (e2ee.json) estava danificado e foi guardado como e2ee.json.bad. Este computador ganhou uma chave nova: quem conversa com você vai ver que o código de segurança mudou, e mensagens antigas podem não abrir aqui.";
/// Told when the kept key no longer opens and this computer got a new one.
pub const UNREADABLE_KEY_TEXT: &str = "A chave de criptografia guardada neste computador não pôde ser lida (outra conta do Windows ou, no Mac, a chave antiga do Keychain recusada na atualização). Este computador ganhou uma chave nova: quem conversa com você vai ver que o código de segurança mudou, e mensagens antigas podem não abrir aqui.";

/// Where a damaged e2ee.json is kept (`settings::secrets_gc` reads it too: its references
/// stay).
pub fn bad_path(path: &std::path::Path) -> PathBuf {
    path.with_extension("json.bad")
}

fn unhex32(s: &str) -> Option<Zeroizing<[u8; 32]>> {
    if s.len() != 64 {
        return None;
    }
    let mut out = Zeroizing::new([0u8; 32]);
    for i in 0..32 {
        out[i] = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

impl Store {
    /// The app's store: e2ee.json next to settings.json, or memory only (see [`set_persistent`]).
    pub fn open() -> Store {
        let path = PERSISTENT.get().copied().unwrap_or(false).then(crate::settings::e2ee_path).flatten();
        match path {
            Some(p) => Store::at(p, crate::settings::protect_key, crate::settings::unprotect_key),
            None => Store::in_memory(),
        }
    }

    pub fn in_memory() -> Store {
        Store { path: None, file: E2eeFile::default(), protect: mem_protect, unprotect: mem_unprotect, damaged: false, warning: None }
    }

    /// The file at `path` (read now). A damaged file is copied to e2ee.json.bad (never silently
    /// overwritten: the person is told when a new identity replaces the old one, see
    /// [`Store::take_warning`]) and the store starts empty.
    pub fn at(path: PathBuf, protect: Protect, unprotect: Protect) -> Store {
        let mut damaged = false;
        let file = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                warn!(error = %e, "e2ee.json is invalid; kept as e2ee.json.bad, starting a new one");
                if let Err(e) = std::fs::copy(&path, bad_path(&path)) {
                    warn!(error = %e, "e2ee: cannot keep a copy of the damaged e2ee.json");
                }
                damaged = true;
                E2eeFile::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => E2eeFile::default(),
            Err(e) => {
                warn!(error = %e, "e2ee.json cannot be read; starting a new one");
                damaged = true;
                E2eeFile::default()
            }
        };
        Store { path: Some(path), file, protect, unprotect, damaged, warning: None }
    }

    /// What the person must be told once (a new identity replaced one this computer had).
    pub fn take_warning(&mut self) -> Option<String> {
        self.warning.take()
    }

    fn save(&self) -> Result<(), String> {
        let Some(p) = &self.path else { return Ok(()) };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = serde_json::to_string_pretty(&self.file).map_err(|e| e.to_string())?;
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, p).map_err(|e| e.to_string())
    }

    /// My identity on this hub for this user: the kept one, or a new one (stored before it is
    /// returned). A kept one that no longer opens (e2ee.json copied to another Windows account, a
    /// Keychain item gone) is replaced.
    pub fn identity(&mut self, server_id: &str, user_id: UserId, now_ms: i64) -> Result<Identity, StoreError> {
        if let Some(i) = self.file.identities.iter().position(|e| e.server_id == server_id && e.user_id == user_id) {
            let e = &self.file.identities[i];
            let opened = (self.unprotect)(&e.secret).map(Zeroizing::new);
            match (opened.as_deref().and_then(|h| unhex32(h)), vox_e2ee::device_id_from_hex(&e.device_id)) {
                (Some(mut secret), Some(dev)) => return Ok(Identity::from_parts(dev, &mut secret)),
                _ => {
                    warn!(user_id, "e2ee: the kept key of this computer cannot be read; making a new one");
                    self.file.identities.remove(i);
                    self.warning = Some(UNREADABLE_KEY_TEXT.into());
                }
            }
        } else if std::mem::take(&mut self.damaged) {
            self.warning = Some(DAMAGED_TEXT.into());
        }
        let id = Identity::generate().map_err(|_| StoreError::Random)?;
        let hex = Zeroizing::new(vox_e2ee::hex(&id.secret_bytes()[..]));
        let Some(secret) = (self.protect)(&hex) else {
            warn!("e2ee: the key cannot be protected on this computer (DPAPI / secrets.json): no end-to-end encryption here");
            return Err(StoreError::NoKeyStorage);
        };
        self.file.identities.push(StoredIdentity { server_id: server_id.into(), user_id, device_id: id.device_id_hex(), secret, created_ms: now_ms });
        if let Err(e) = self.save() {
            warn!(error = %e, "e2ee: cannot write e2ee.json: no end-to-end encryption here");
            self.file.identities.pop();
            return Err(StoreError::NoKeyStorage);
        }
        Ok(id)
    }

    /// The hub has another key under this device id (a reinstall reused it): the same key under a
    /// new id, stored.
    pub fn renew_device_id(&mut self, server_id: &str, user_id: UserId, id: &mut Identity) -> Result<(), StoreError> {
        id.renew_device_id().map_err(|_| StoreError::Random)?;
        if let Some(e) = self.file.identities.iter_mut().find(|e| e.server_id == server_id && e.user_id == user_id) {
            e.device_id = id.device_id_hex();
        }
        self.save().map_err(|_| StoreError::NoKeyStorage)
    }

    /// This computer was removed from the account: forget its identity (the next login makes a new
    /// one, which everybody sees as a new device).
    pub fn forget(&mut self, server_id: &str, user_id: UserId) {
        self.file.identities.retain(|e| !(e.server_id == server_id && e.user_id == user_id));
        if let Err(e) = self.save() {
            warn!(error = %e, "e2ee: cannot write e2ee.json");
        }
    }

    /// Record the directory's devices of `users` (trust on first use). Returns the devices of my
    /// own account (`me`, other than `this_device`) that this computer never saw before, once it
    /// already knew my account (the very first look records silently). (E0's view; the app uses
    /// [`Store::observe_all`].)
    #[cfg(test)]
    pub fn observe(&mut self, server_id: &str, me: UserId, this_device: Option<&str>, devices: &[E2eeDevice], now_ms: i64) -> Vec<E2eeDevice> {
        self.observe_all(server_id, me, this_device, devices, now_ms).mine_new
    }

    /// [`Store::observe`], plus the key changes of everybody else this computer already knew (E2:
    /// "o código de segurança de Fulano mudou").
    pub fn observe_all(&mut self, server_id: &str, me: UserId, this_device: Option<&str>, devices: &[E2eeDevice], now_ms: i64) -> Observed {
        let server = self.file.pins.entry(server_id.to_string()).or_default();
        let knew: BTreeMap<UserId, (bool, bool)> = devices
            .iter()
            .map(|d| d.user_id)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|u| {
                let pins = server.get(&u);
                (u, (pins.is_some_and(|v| !v.is_empty()), pins.is_some_and(|v| v.iter().any(|p| p.verified))))
            })
            .collect();
        let mut out = Observed::default();
        let mut changed = false;
        for d in devices {
            let pins = server.entry(d.user_id).or_default();
            if pins.iter().any(|p| p.device_id == d.device_id && p.key == d.public_key) {
                continue;
            }
            let known_id = pins.iter().any(|p| p.device_id == d.device_id);
            pins.push(DevicePin { device_id: d.device_id.clone(), key: d.public_key.clone(), first_seen_ms: now_ms, verified: false });
            changed = true;
            let (knew_user, was_verified) = knew.get(&d.user_id).copied().unwrap_or_default();
            if d.user_id == me {
                if Some(d.device_id.as_str()) != this_device && knew_user {
                    out.mine_new.push(d.clone());
                }
            } else if knew_user {
                let i = match out.others.iter().position(|c| c.user_id == d.user_id) {
                    Some(i) => i,
                    None => {
                        out.others.push(PeerChange { user_id: d.user_id, added: Vec::new(), was_verified });
                        out.others.len() - 1
                    }
                };
                if !known_id {
                    out.others[i].added.push(d.clone());
                }
            }
        }
        if changed {
            if let Err(e) = self.save() {
                warn!(error = %e, "e2ee: cannot write e2ee.json");
            }
        }
        out
    }

    /// The keys this computer pinned for a device (E2: a line from a device that has since left
    /// the directory still opens with the key it had).
    pub fn pinned_keys(&self, server_id: &str, user_id: UserId, device_id: &str) -> Vec<String> {
        self.file.pins.get(server_id).and_then(|s| s.get(&user_id)).map(|v| v.iter().filter(|p| p.device_id == device_id).map(|p| p.key.clone()).collect()).unwrap_or_default()
    }

    /// Is this exact (device, key) pinned as verified?
    pub fn is_verified(&self, server_id: &str, user_id: UserId, device_id: &str, key: &str) -> bool {
        self.file.pins.get(server_id).and_then(|s| s.get(&user_id)).is_some_and(|v| v.iter().any(|p| p.device_id == device_id && p.key == key && p.verified))
    }

    /// Was any key of `user_id` ever marked verified here?
    pub fn ever_verified(&self, server_id: &str, user_id: UserId) -> bool {
        self.file.pins.get(server_id).and_then(|s| s.get(&user_id)).is_some_and(|v| v.iter().any(|p| p.verified))
    }

    /// "Marcar como verificado": these exact devices (both people's, as the safety code covered
    /// them) are verified. "Desmarcar" (`on` false): every key of `peer` is not.
    pub fn set_verified(&mut self, server_id: &str, peer: UserId, devices: &[E2eeDevice], on: bool) {
        let server = self.file.pins.entry(server_id.to_string()).or_default();
        if on {
            for d in devices {
                if let Some(p) = server.entry(d.user_id).or_default().iter_mut().find(|p| p.device_id == d.device_id && p.key == d.public_key) {
                    p.verified = true;
                }
            }
        } else if let Some(v) = server.get_mut(&peer) {
            for p in v.iter_mut() {
                p.verified = false;
            }
        }
        if let Err(e) = self.save() {
            warn!(error = %e, "e2ee: cannot write e2ee.json");
        }
    }

    /// The DMs that were ever encrypted on this computer (on this hub).
    pub fn sealed_dms(&self, server_id: &str) -> BTreeSet<vox_proto::DmId> {
        self.file.sealed_dms.get(server_id).cloned().unwrap_or_default()
    }

    /// DM `dm` was encrypted: from now on, never without asking. Returns whether it is new.
    pub fn mark_sealed_dm(&mut self, server_id: &str, dm: vox_proto::DmId) -> bool {
        let added = self.file.sealed_dms.entry(server_id.to_string()).or_default().insert(dm);
        if added {
            if let Err(e) = self.save() {
                warn!(error = %e, "e2ee: cannot write e2ee.json");
            }
        }
        added
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(user: UserId, n: u8, key: u8) -> E2eeDevice {
        E2eeDevice { user_id: user, device_id: format!("{n:02x}").repeat(16), public_key: vox_e2ee::public_key_to_b64(&[key; 32]), label: format!("PC {n}"), created_at_ms: 1 }
    }

    fn fake_protect(s: &str) -> Option<String> {
        Some(format!("p:{}", s.chars().rev().collect::<String>()))
    }

    fn fake_unprotect(s: &str) -> Option<String> {
        s.strip_prefix("p:").map(|r| r.chars().rev().collect())
    }

    fn no_protect(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn one_identity_per_server_and_user_kept_across_runs() {
        let dir = tempfile_dir("keep");
        let path = dir.join("e2ee.json");
        let mut s = Store::at(path.clone(), fake_protect, fake_unprotect);
        let a = s.identity("srv1", 3, 100).unwrap();
        let a_again = s.identity("srv1", 3, 200).unwrap();
        assert_eq!((a_again.device_id, a_again.public_key()), (a.device_id, a.public_key()));
        let other_user = s.identity("srv1", 4, 100).unwrap();
        let other_server = s.identity("srv2", 3, 100).unwrap();
        assert_ne!(other_user.public_key(), a.public_key());
        assert_ne!(other_server.public_key(), a.public_key());
        assert_eq!(s.file.identities.len(), 3);
        // The file has the protected form only.
        let text = std::fs::read_to_string(&path).unwrap();
        let secret_hex = vox_e2ee::hex(&a.secret_bytes()[..]);
        assert!(!text.contains(&secret_hex), "no plain secret in e2ee.json");
        assert!(text.contains("\"identities\"") && text.contains("\"pins\"") && text.contains(&a.device_id_hex()));
        // A new run reads it back.
        let mut s2 = Store::at(path.clone(), fake_protect, fake_unprotect);
        let back = s2.identity("srv1", 3, 300).unwrap();
        assert_eq!((back.device_id, back.public_key()), (a.device_id, a.public_key()));
        assert_eq!(s2.file.identities.iter().find(|e| e.user_id == 3 && e.server_id == "srv1").unwrap().created_ms, 100);
        // A new device id keeps the key; forgetting drops only that identity.
        let mut renewed = back;
        s2.renew_device_id("srv1", 3, &mut renewed).unwrap();
        assert_ne!(renewed.device_id, a.device_id);
        let s3_id = Store::at(path.clone(), fake_protect, fake_unprotect).identity("srv1", 3, 400).unwrap();
        assert_eq!((s3_id.device_id, s3_id.public_key()), (renewed.device_id, a.public_key()));
        s2.forget("srv1", 3);
        let mut s4 = Store::at(path.clone(), fake_protect, fake_unprotect);
        assert_eq!(s4.file.identities.len(), 2);
        assert_ne!(s4.identity("srv1", 3, 500).unwrap().public_key(), a.public_key(), "a new identity after being removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_key_storage_there_is_no_identity_and_nothing_is_written() {
        let dir = tempfile_dir("noprotect");
        let path = dir.join("e2ee.json");
        let mut s = Store::at(path.clone(), no_protect, fake_unprotect);
        assert_eq!(s.identity("srv1", 3, 100).err(), Some(StoreError::NoKeyStorage));
        assert!(s.file.identities.is_empty());
        assert!(!path.exists());
        assert!(UNAVAILABLE_TEXT.starts_with("Não foi possível guardar a chave"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_key_that_no_longer_opens_is_replaced() {
        let dir = tempfile_dir("reopen");
        let path = dir.join("e2ee.json");
        let first = Store::at(path.clone(), fake_protect, fake_unprotect).identity("srv1", 3, 100).unwrap();
        // Another Windows account / a lost Keychain item: unprotect fails.
        let mut s = Store::at(path.clone(), fake_protect, |_| None);
        let second = s.identity("srv1", 3, 200).unwrap();
        assert_ne!(second.public_key(), first.public_key());
        assert_eq!(s.file.identities.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pins_are_recorded_and_only_new_devices_of_mine_are_news() {
        let mut s = Store::in_memory();
        let me = 3;
        let mine = dev(me, 1, 1);
        // First look: everything recorded silently (my other PC registered before this one).
        assert!(s.observe("srv", me, Some(&mine.device_id), &[mine.clone(), dev(me, 2, 2), dev(9, 5, 5)], 10).is_empty());
        assert_eq!(s.file.pins["srv"][&me].len(), 2);
        assert_eq!(s.file.pins["srv"][&9][0], DevicePin { device_id: dev(9, 5, 5).device_id, key: dev(9, 5, 5).public_key, first_seen_ms: 10, verified: false });
        // Same list again: nothing.
        assert!(s.observe("srv", me, Some(&mine.device_id), &[mine.clone(), dev(me, 2, 2)], 20).is_empty());
        // A new device of mine: news; a new one of somebody else: recorded, not news.
        let fresh = s.observe("srv", me, Some(&mine.device_id), &[mine.clone(), dev(me, 2, 2), dev(me, 3, 3), dev(9, 6, 6)], 30);
        assert_eq!(fresh, vec![dev(me, 3, 3)]);
        assert_eq!(s.file.pins["srv"][&9].len(), 2);
        // A known id with another key: news too (and both keys stay recorded for E2).
        let swapped = dev(me, 2, 77);
        assert_eq!(s.observe("srv", me, Some(&mine.device_id), &[swapped.clone()], 40), vec![swapped]);
        assert_eq!(s.file.pins["srv"][&me].len(), 4);
        // My own new identity on this computer is never news; another server is its own world.
        let renewed = dev(me, 8, 1);
        assert!(s.observe("srv", me, Some(&renewed.device_id), &[renewed.clone()], 50).is_empty());
        assert!(s.observe("srv-b", me, Some("x"), &[dev(me, 2, 2)], 60).is_empty());
        // The pins survive the file format (user ids as JSON object keys).
        let json = serde_json::to_string(&s.file).unwrap();
        let back: E2eeFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pins, s.file.pins);
        assert!(serde_json::from_str::<E2eeFile>("{}").unwrap().identities.is_empty());
    }

    #[test]
    fn key_changes_of_other_people_and_the_verified_flags() {
        let mut s = Store::in_memory();
        let (me, bia) = (3, 9);
        let mine = dev(me, 1, 1);
        // First look: silent for everybody.
        let o = s.observe_all("srv", me, Some(&mine.device_id), &[mine.clone(), dev(bia, 5, 5)], 10);
        assert_eq!(o, Observed::default());
        // Bia adds a computer: news, with its label; not verified before.
        let o = s.observe_all("srv", me, Some(&mine.device_id), &[mine.clone(), dev(bia, 5, 5), dev(bia, 6, 6)], 20);
        assert_eq!(o.others, vec![PeerChange { user_id: bia, added: vec![dev(bia, 6, 6)], was_verified: false }]);
        // Verified: both people's current devices.
        s.set_verified("srv", bia, &[mine.clone(), dev(bia, 5, 5), dev(bia, 6, 6)], true);
        assert!(s.is_verified("srv", bia, &dev(bia, 6, 6).device_id, &dev(bia, 6, 6).public_key));
        assert!(s.is_verified("srv", me, &mine.device_id, &mine.public_key));
        // A reinstall (same id, another key): a change without a new device, and it WAS verified.
        let o = s.observe_all("srv", me, Some(&mine.device_id), &[dev(bia, 5, 77)], 30);
        assert_eq!(o.others, vec![PeerChange { user_id: bia, added: vec![], was_verified: true }]);
        assert!(!s.is_verified("srv", bia, &dev(bia, 5, 77).device_id, &dev(bia, 5, 77).public_key), "the new key is not verified");
        assert_eq!(s.pinned_keys("srv", bia, &dev(bia, 5, 5).device_id).len(), 2, "both keys of the device stay pinned");
        // "Desmarcar": nothing of Bia's is verified any more (mine stays).
        s.set_verified("srv", bia, &[], false);
        assert!(!s.ever_verified("srv", bia) && s.ever_verified("srv", me));
        // Encrypted conversations are remembered.
        assert!(s.mark_sealed_dm("srv", 4) && !s.mark_sealed_dm("srv", 4));
        assert_eq!(s.sealed_dms("srv").into_iter().collect::<Vec<_>>(), vec![4]);
        assert!(s.sealed_dms("other").is_empty());
        let back: E2eeFile = serde_json::from_str(&serde_json::to_string(&s.file).unwrap()).unwrap();
        assert_eq!(back.sealed_dms, s.file.sealed_dms);
    }

    #[test]
    fn debug_never_shows_the_protected_secret() {
        let e = StoredIdentity { server_id: "s".into(), user_id: 1, device_id: "d".into(), secret: "01000000d08c9ddf-protected".into(), created_ms: 0 };
        assert!(!format!("{e:?}").contains("protected"));
    }

    /// Audit (v1.1 E0): a damaged e2ee.json is kept (e2ee.json.bad) and the new identity that
    /// replaces the old one is told, once; so is a kept key that no longer opens. A first identity
    /// is not news.
    #[test]
    fn a_new_identity_in_place_of_an_old_one_is_never_silent() {
        let dir = tempfile_dir("damaged");
        let path = dir.join("e2ee.json");
        let mut s = Store::at(path.clone(), fake_protect, fake_unprotect);
        let first = s.identity("srv1", 3, 100).unwrap();
        assert_eq!(s.take_warning(), None, "the first identity is not news");
        let good = std::fs::read_to_string(&path).unwrap();
        // Half a file (a crash, a disk error).
        std::fs::write(&path, &good[..good.len() / 2]).unwrap();
        let mut s = Store::at(path.clone(), fake_protect, fake_unprotect);
        assert_eq!(std::fs::read_to_string(bad_path(&path)).unwrap(), good[..good.len() / 2], "the damaged file is kept");
        let second = s.identity("srv1", 3, 200).unwrap();
        assert_ne!(second.public_key(), first.public_key());
        assert_eq!(s.take_warning().as_deref(), Some(DAMAGED_TEXT));
        assert_eq!(s.take_warning(), None, "once");
        s.identity("srv2", 3, 300).unwrap();
        assert_eq!(s.take_warning(), None, "once per damaged file");
        // A key that no longer opens.
        let mut s = Store::at(path.clone(), fake_protect, |_| None);
        s.identity("srv1", 3, 400).unwrap();
        assert_eq!(s.take_warning().as_deref(), Some(UNREADABLE_KEY_TEXT));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempfile_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("concord-e2ee-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
