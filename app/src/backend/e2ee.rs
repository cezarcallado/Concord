//! End-to-end encryption, phase E0 (v1.1): this computer's identity on the hub's directory of
//! device keys (the contract is in `vox_proto::e2ee`, the keys in `vox_e2ee`, the file in
//! `e2ee_store.rs`).
//!
//! - On every hub snapshot whose `Ready` says `e2ee`: this computer's identity for (server, user)
//!   (kept in e2ee.json, made on first use), registered with `PUT /api/e2ee/devices/{id}` (the same
//!   key again is a no-op; `409` "another key" means a reinstall reused the id: a new id, once),
//!   then the whole directory, recorded as pins (trust on first use).
//! - `E2eeDevicesChanged`: the directory follows it; a device of my own account that this
//!   computer never saw is an alert ("Um aparelho novo foi adicionado à sua conta: ...").
//! - This computer missing from my own list (removed from another device, confirmed with a fresh
//!   read): its identity is forgotten and it is not registered again until the next login (then
//!   with a new key, so every other device sees it as new).
//! - `Cmd::E2ee(Remove)`: "Remover" on the Privacy page (another of my devices).
//!
//! Every state change goes to the UI as [`E2eeEv::View`]. Nothing here logs a key.
//!
//! E2 (DMs, `e2ee/dm.rs`): the same session seals and opens DM lines and their images; the view
//! also carries the conversations ever encrypted here and every peer's safety code (verified or
//! not), and a key change of somebody this computer already knew is [`E2eeEv::PeerKeys`] (the
//! DM's local lines "o código de segurança mudou" / "adicionou um aparelho").

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use tracing::{info, warn};
use vox_client_core::hub::{E2eeRegisterReply, HubClient};
use vox_e2ee::Identity;
use vox_proto::{DmId, DmMessageId, E2eeDevice, User, UserId};

use super::{Controller, Ev, Out};
use crate::e2ee_store::{Store, StoreError};

pub mod dm;

/// E1: how long starting a DM call waits for this computer's encryption to be ready.
pub const CALL_MEDIA_WAIT: std::time::Duration = std::time::Duration::from_secs(6);

/// What the UI asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum E2eeCmd {
    /// "Remover": one of my other devices (by id).
    Remove(String),
    /// E2: "Marcar como verificado" (`on`) / "Desmarcar" the safety code with this person.
    Verify { user: UserId, on: bool },
}

/// What the UI hears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum E2eeEv {
    View(E2eeView),
    /// A device I never saw appeared under my own account.
    NewOwnDevice { device_id: String, label: String },
    /// E2: somebody else's keys changed since this computer first knew them (`added`: the labels of
    /// new devices; a reinstall adds none). `was_verified`: their code had been verified.
    PeerKeys { user_id: UserId, added: Vec<String>, was_verified: bool },
}

/// E2: the safety code with one person, as this computer sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SafetyView {
    /// 30 digits in 6 groups of 5 (`vox_e2ee::safety`).
    pub code: String,
    /// Every current key of both people was marked verified here.
    pub verified: bool,
    /// It had been verified, and a key changed since: "Verifique de novo".
    pub reverify: bool,
}

/// Where this computer stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum E2eeStatus {
    /// Not logged in.
    #[default]
    Off,
    /// The hub has no directory (an older hub).
    Unsupported,
    /// Registering.
    Starting,
    /// Registered: this computer is in the list.
    Active,
    /// `protect` failed: no key on this computer.
    NoKeyStorage,
    /// The account already has the most devices.
    TooMany,
    /// Removed from another device during this session.
    Removed,
    /// The hub refused or could not be reached; tried again at the next snapshot.
    Failed,
}

/// The Privacy page's data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct E2eeView {
    pub status: E2eeStatus,
    pub me: Option<UserId>,
    /// This computer's device id (when it has an identity).
    pub this_device: Option<String>,
    /// My account's devices, oldest first.
    pub mine: Vec<E2eeDevice>,
    /// Everybody's device ids on the server's list (E2 will use the keys; the autotest the ids).
    pub devices_of: BTreeMap<UserId, Vec<String>>,
    /// E1 (audit): the whole list was read in this session (else `devices_of` may miss people).
    pub directory_read: bool,
    /// The device being removed right now.
    pub removing: Option<String>,
    /// The last thing that went wrong (Portuguese).
    pub error: Option<String>,
    /// E2: the DMs that were ever encrypted on this computer (never sent without asking).
    pub sealed_dms: BTreeSet<DmId>,
    /// E2: the safety code with every other member who has a device.
    pub safety: BTreeMap<UserId, SafetyView>,
}

impl E2eeView {
    /// E2: this computer can encrypt for `peer` now (registered, and they have a device).
    pub fn can_seal_for(&self, peer: UserId) -> bool {
        self.status == E2eeStatus::Active && self.this_device.is_some() && self.devices_of.get(&peer).is_some_and(|v| !v.is_empty())
    }
}

/// A test hook: the label the autotests' second device registers with (instead of the computer's
/// name).
static LABEL_OVERRIDE: OnceLock<String> = OnceLock::new();

pub fn set_label_override(label: String) {
    let _ = LABEL_OVERRIDE.set(label);
}

/// This computer's name, as the device list shows it.
pub fn device_label() -> String {
    if let Some(l) = LABEL_OVERRIDE.get() {
        return vox_proto::fit_e2ee_label(l);
    }
    #[cfg(windows)]
    let name = std::env::var("COMPUTERNAME").ok();
    #[cfg(target_os = "macos")]
    let name = std::process::Command::new("/usr/sbin/scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| Some("Mac".to_string()));
    #[cfg(not(any(windows, target_os = "macos")))]
    let name: Option<String> = None;
    vox_proto::fit_e2ee_label(name.as_deref().unwrap_or(""))
}

/// The session's directory.
struct Session {
    server_id: String,
    me: UserId,
    /// `None`: no key on this computer (or removed).
    identity: Option<Identity>,
    registered: bool,
    dir: BTreeMap<UserId, Vec<E2eeDevice>>,
    /// E2: the whole directory was read at least once (lines wait for it before opening).
    dir_loaded: bool,
}

#[derive(Default)]
struct Inner {
    /// Opened at the first login (reads e2ee.json).
    store: Option<Store>,
    /// Bumped on every snapshot and logout: a task of an older one changes nothing.
    gen: u64,
    session: Option<Session>,
    /// This computer was removed during this login (server, user): not registered again until the
    /// next login.
    removed: Option<(String, UserId)>,
    view: E2eeView,
    /// E2: the `id` of every line opened this session (a second line with the same id is a
    /// replay and is dropped) and the text of opened lines (the excerpt of a reply to one).
    /// (Keyed with the conversation and the author: the hub cannot move a line to another
    /// conversation or author, so a peer reusing another line's id can only hide their own lines.
    /// The excerpts are kept with their conversation, wiped when dropped.)
    seen: HashMap<(DmId, UserId, String), DmMessageId>,
    texts: HashMap<DmMessageId, (DmId, vox_e2ee::zeroize::Zeroizing<String>)>,
    /// E2: the hub of the last snapshot (`Ready.server_id`), also when it has no directory: a
    /// conversation encrypted on it is never sent in the clear without asking, even if the hub
    /// stops announcing `e2ee`.
    server_hint: Option<String>,
    /// E2: where views go from tasks that open lines (set at every snapshot), and whether the view
    /// changed while opening (a conversation newly encrypted).
    out: Option<Out>,
    dirty: bool,
    /// E1: devices that appeared under somebody already known to this computer (a new computer of
    /// theirs, seen at a login or live): "Fulano entrou de um aparelho novo" when they join a call
    /// with one. (user, device id)
    new_devices: std::collections::HashSet<(UserId, String)>,
}

impl Inner {
    fn store(&mut self) -> &mut Store {
        self.store.get_or_insert_with(Store::open)
    }

    /// The view from the session, sent to the UI.
    fn publish(&mut self, out: &Out) {
        if let Some((server_id, me, dir)) = self.session.as_ref().map(|s| (s.server_id.clone(), s.me, s.dir.clone())) {
            let s = self.session.as_ref().expect("checked");
            self.view.me = Some(me);
            self.view.this_device = s.identity.as_ref().map(|i| i.device_id_hex());
            self.view.mine = dir.get(&me).cloned().unwrap_or_default();
            self.view.devices_of = dir.iter().map(|(u, v)| (*u, v.iter().map(|d| d.device_id.clone()).collect())).collect();
            self.view.directory_read = s.dir_loaded;
            let mine = dir.get(&me).cloned().unwrap_or_default();
            let store = self.store();
            let sealed_dms = store.sealed_dms(&server_id);
            let safety = dir
                .iter()
                .filter(|(u, v)| **u != me && !v.is_empty())
                .map(|(u, theirs)| (*u, safety_of(store, &server_id, me, &mine, *u, theirs)))
                .collect();
            self.view.sealed_dms = sealed_dms;
            self.view.safety = safety;
        }
        out.send(Ev::E2ee(E2eeEv::View(self.view.clone())));
    }

    /// Record `devices` (pins) and tell about new devices of mine (E0) and key changes of others
    /// (E2).
    fn observe(&mut self, devices: &[E2eeDevice], out: &Out) {
        let Some((server_id, me, this)) = self.session.as_ref().map(|s| (s.server_id.clone(), s.me, s.identity.as_ref().map(|i| i.device_id_hex()))) else { return };
        let seen = self.store().observe_all(&server_id, me, this.as_deref(), devices, crate::app::now_ms());
        // (Labels come from the hub: cleaned again here, a modified hub may send anything.)
        for d in seen.mine_new {
            info!(device = short(&d.device_id), "e2ee: a new device appeared under my account");
            out.send(Ev::E2ee(E2eeEv::NewOwnDevice { device_id: d.device_id, label: vox_proto::fit_e2ee_label(&d.label) }));
        }
        for c in seen.others {
            info!(user = c.user_id, added = c.added.len(), was_verified = c.was_verified, "e2ee: another member's keys changed");
            for d in &c.added {
                self.new_devices.insert((c.user_id, d.device_id.clone()));
            }
            out.send(Ev::E2ee(E2eeEv::PeerKeys { user_id: c.user_id, added: c.added.into_iter().map(|d| vox_proto::fit_e2ee_label(&d.label)).collect(), was_verified: c.was_verified }));
        }
    }
}

/// The safety code with `peer` and whether it is verified here (every current key of both people
/// verified), or was and changed since.
fn safety_of(store: &Store, server_id: &str, me: UserId, mine: &[E2eeDevice], peer: UserId, theirs: &[E2eeDevice]) -> SafetyView {
    let keys = |v: &[E2eeDevice]| v.iter().filter_map(|d| vox_e2ee::public_key_from_b64(&d.public_key)).collect::<Vec<_>>();
    let code = vox_e2ee::safety::safety_code(&[(me, keys(mine)), (peer, keys(theirs))]);
    let all = |user: UserId, v: &[E2eeDevice]| !v.is_empty() && v.iter().all(|d| store.is_verified(server_id, user, &d.device_id, &d.public_key));
    let verified = all(me, mine) && all(peer, theirs);
    SafetyView { code, verified, reverify: !verified && store.ever_verified(server_id, peer) }
}

/// The backend's end-to-end encryption state (E0's directory, E2's DM lines). Cloned into the
/// tasks that send and load DMs and images.
#[derive(Default, Clone)]
pub struct E2eeBackend(Arc<Mutex<Inner>>);

impl E2eeBackend {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// E1: what a voice join needs to encrypt its room (this computer's identity, a copy, and the
    /// directory as it follows `E2eeDevicesChanged`), when this computer can: registered on the
    /// hub's list with the directory read. `None` otherwise (the join then does not ask for an
    /// encrypted room).
    pub fn media(&self) -> Option<vox_client_core::MediaE2ee> {
        let g = self.lock();
        let s = g.session.as_ref()?;
        if !s.registered || !s.dir_loaded {
            return None;
        }
        let id = s.identity.as_ref()?;
        let copy = Identity::from_parts(id.device_id, &mut *id.secret_bytes());
        Some(vox_client_core::MediaE2ee { identity: Arc::new(copy), directory: Arc::new(MediaDir(self.clone())) })
    }

    /// Where this computer stands (for the join's messages).
    pub fn status(&self) -> E2eeStatus {
        self.lock().view.status
    }

    /// E1 (audit): [`Self::media`] for starting a DM call, which is encrypted only if this computer
    /// says it can when the call starts. While it is still registering, or registered without the
    /// device list read, wait for it (up to [`CALL_MEDIA_WAIT`]) and read the list once more, rather
    /// than start a plain call (which the other side would see as "uses an old version"). Also
    /// returns the status after waiting.
    pub async fn media_for_call(&self, hub: &HubClient) -> (Option<vox_client_core::MediaE2ee>, E2eeStatus) {
        let until = tokio::time::Instant::now() + CALL_MEDIA_WAIT;
        let mut reread = false;
        loop {
            let (media, status) = (self.media(), self.status());
            if media.is_some() || !matches!(status, E2eeStatus::Starting | E2eeStatus::Active) || tokio::time::Instant::now() >= until {
                return (media, status);
            }
            if status == E2eeStatus::Active && !reread {
                // Registered, but the list could not be read at the login: once more now.
                reread = true;
                self.refresh_directory(hub).await;
                continue;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    /// E1: is `device` one that appeared under `user` after this computer first knew them?
    pub fn is_new_device(&self, user: UserId, device: &str) -> bool {
        self.lock().new_devices.contains(&(user, device.to_string()))
    }
}

/// E1: the hub's directory as a voice session reads it (live: a device removed mid-call stops
/// being accepted at once).
struct MediaDir(E2eeBackend);

impl vox_client_core::MediaDirectory for MediaDir {
    fn key_of(&self, identity: &str, device: &vox_e2ee::DeviceId) -> Option<[u8; vox_e2ee::KEY_LEN]> {
        let user: UserId = identity.parse().ok()?;
        let hex = vox_e2ee::hex(device);
        let g = self.0.lock();
        let s = g.session.as_ref()?;
        s.dir.get(&user)?.iter().find(|d| d.device_id == hex).and_then(|d| vox_e2ee::public_key_from_b64(&d.public_key))
    }
}

/// The first digits of a device id, for the log (never a key).
fn short(device_id: &str) -> &str {
    device_id.get(..8).unwrap_or("?")
}

/// Whole directory: by member (labels cleaned: they come from the hub and are shown).
fn group(all: Vec<E2eeDevice>) -> BTreeMap<UserId, Vec<E2eeDevice>> {
    let mut dir: BTreeMap<UserId, Vec<E2eeDevice>> = BTreeMap::new();
    for d in all {
        dir.entry(d.user_id).or_default().push(clean_device(d));
    }
    dir
}

/// A device from the hub with its label cleaned the way the hub should have (no control,
/// invisible or direction-changing characters, at most 40): a modified hub may send anything.
fn clean_device(mut d: E2eeDevice) -> E2eeDevice {
    d.label = vox_proto::fit_e2ee_label(&d.label);
    d
}

/// `E2eeDevicesChanged`'s list as it may be used: only devices of the member it is about. A hub
/// that put somebody else's device in it would otherwise slip that device into this member's
/// list (lines to them would be wrapped for it) with no "código de segurança mudou": its pin is
/// under the other member.
fn devices_of_event(user_id: UserId, devices: Vec<E2eeDevice>) -> Vec<E2eeDevice> {
    devices.into_iter().filter(|d| d.user_id == user_id).map(clean_device).collect()
}

/// Register `identity`; a `409` "another key" once gets a new device id. Returns the status and
/// the identity (maybe renewed).
async fn register(hub: &HubClient, e2: &E2eeBackend, server_id: &str, me: UserId, mut identity: Identity) -> (E2eeStatus, Option<String>, Identity) {
    let label = device_label();
    for attempt in 0..2 {
        match hub.register_e2ee_device(&identity, &label).await {
            E2eeRegisterReply::Registered(_) => {
                info!(device = short(&identity.device_id_hex()), "e2ee: this computer is registered");
                return (E2eeStatus::Active, None, identity);
            }
            E2eeRegisterReply::KeyChanged if attempt == 0 => {
                warn!("e2ee: the hub has another key under this device id (a reinstall?); registering under a new id");
                if e2.lock().store().renew_device_id(server_id, me, &mut identity).is_err() {
                    return (E2eeStatus::NoKeyStorage, Some(crate::e2ee_store::UNAVAILABLE_TEXT.into()), identity);
                }
            }
            E2eeRegisterReply::TooMany => {
                return (E2eeStatus::TooMany, Some(format!("Sua conta já tem {} aparelhos com criptografia. Remova um que você não usa mais.", vox_proto::E2EE_MAX_DEVICES)), identity);
            }
            E2eeRegisterReply::Unsupported => return (E2eeStatus::Unsupported, None, identity),
            E2eeRegisterReply::Busy => return (E2eeStatus::Failed, Some("O servidor pediu para esperar um pouco. Tentamos de novo na próxima conexão.".into()), identity),
            E2eeRegisterReply::KeyChanged => return (E2eeStatus::Failed, Some("O servidor recusou a chave deste computador.".into()), identity),
            E2eeRegisterReply::Failed(e) => {
                warn!(error = %e, "e2ee: registration failed");
                return (E2eeStatus::Failed, Some("Não foi possível registrar este computador agora. Tentamos de novo na próxima conexão.".into()), identity);
            }
        }
    }
    (E2eeStatus::Failed, None, identity)
}

impl Controller {
    /// A hub snapshot: register this computer (when the hub has a directory) and read it.
    pub(super) fn e2ee_on_ready(&mut self, supported: bool, server_id: Option<String>, me: &User) {
        let e2 = self.e2ee.clone();
        let out = self.out.clone();
        let mut g = e2.lock();
        g.gen += 1;
        let gen = g.gen;
        g.view = E2eeView::default();
        g.out = Some(out.clone());
        g.server_hint = server_id.clone();
        let (Some(server_id), true, Some(hub)) = (server_id, supported, self.hub.clone()) else {
            g.session = None;
            g.view.status = E2eeStatus::Unsupported;
            g.publish(&out);
            return;
        };
        let removed = g.removed.as_ref() == Some(&(server_id.clone(), me.id));
        // The identity: kept, or made now (stored before anything goes out).
        let identity = if removed {
            None
        } else {
            match g.store().identity(&server_id, me.id, crate::app::now_ms()) {
                Ok(i) => Some(i),
                Err(StoreError::NoKeyStorage) | Err(StoreError::Random) => {
                    g.view.error = Some(crate::e2ee_store::UNAVAILABLE_TEXT.into());
                    None
                }
            }
        };
        // A new identity replaced one this computer had (a damaged e2ee.json, a key that no longer
        // opens): never silently.
        if let Some(w) = g.store().take_warning() {
            warn!("e2ee: this computer got a new key in place of the one it had (the person is told)");
            out.send(Ev::Notice(w));
        }
        g.view.status = match (&identity, removed) {
            (_, true) => E2eeStatus::Removed,
            (None, false) => E2eeStatus::NoKeyStorage,
            (Some(_), false) => E2eeStatus::Starting,
        };
        let has_identity = identity.is_some();
        // The identity stays here while it registers (the task gets its own copy of the secret).
        let task_id = identity.as_ref().map(|i| Identity::from_parts(i.device_id, &mut *i.secret_bytes()));
        g.session = Some(Session { server_id: server_id.clone(), me: me.id, identity, registered: false, dir: BTreeMap::new(), dir_loaded: false });
        g.publish(&out);
        drop(g);
        let me = me.id;
        tokio::spawn(async move {
            let mut status = None;
            if let Some(id) = task_id.filter(|_| has_identity) {
                let (st, err, id) = register(&hub, &e2, &server_id, me, id).await;
                let mut g = e2.lock();
                if g.gen != gen {
                    return;
                }
                if let Some(s) = g.session.as_mut() {
                    s.registered = st == E2eeStatus::Active;
                    // (A renewed device id.)
                    s.identity = Some(id);
                }
                g.view.error = err;
                status = Some(st);
            }
            let all = hub.e2ee_devices().await;
            let mut g = e2.lock();
            if g.gen != gen {
                return;
            }
            match all {
                Ok(all) => {
                    g.observe(&all, &out);
                    if let Some(s) = g.session.as_mut() {
                        s.dir = group(all);
                        s.dir_loaded = true;
                    }
                }
                Err(e) => warn!(error = %format!("{e:#}"), "e2ee: cannot read the device list"),
            }
            if let Some(st) = status {
                g.view.status = st;
            }
            g.publish(&out);
        });
    }

    /// `E2eeDevicesChanged`: follow the directory; my own computer missing is checked once more.
    pub(super) fn e2ee_devices_changed(&mut self, user_id: UserId, devices: Vec<E2eeDevice>) {
        let e2 = self.e2ee.clone();
        let out = self.out.clone();
        let devices = devices_of_event(user_id, devices);
        let mut g = e2.lock();
        let Some(s) = g.session.as_mut() else { return };
        let mine_gone = s.me == user_id && s.registered && s.identity.as_ref().is_some_and(|i| !devices.iter().any(|d| d.device_id == i.device_id_hex()));
        s.dir.insert(user_id, devices.clone());
        g.observe(&devices, &out);
        g.publish(&out);
        if !mine_gone {
            return;
        }
        let (gen, Some(hub)) = (g.gen, self.hub.clone()) else { return };
        drop(g);
        tokio::spawn(async move {
            // An event can be older than my own registration: only a fresh read decides.
            let Ok(all) = hub.e2ee_devices().await else { return };
            let mut g = e2.lock();
            if g.gen != gen {
                return;
            }
            let Some(s) = g.session.as_mut() else { return };
            let Some(this) = s.identity.as_ref().map(|i| i.device_id_hex()) else { return };
            if all.iter().any(|d| d.user_id == s.me && d.device_id == this) {
                s.dir = group(all);
                g.publish(&out);
                return;
            }
            info!("e2ee: this computer was removed from my account on another device");
            let (server_id, me) = (s.server_id.clone(), s.me);
            s.identity = None;
            s.registered = false;
            s.dir = group(all);
            g.store().forget(&server_id, me);
            g.removed = Some((server_id, me));
            g.view.status = E2eeStatus::Removed;
            g.publish(&out);
            out.send(Ev::Notice("Este computador foi removido da lista de aparelhos da sua conta. Na próxima vez que você entrar, ele volta como um aparelho novo.".into()));
        });
    }

    /// "Remover" (another of my devices); E2's "Marcar como verificado" / "Desmarcar".
    pub(super) fn e2ee_cmd(&mut self, cmd: E2eeCmd) {
        let device_id = match cmd {
            E2eeCmd::Remove(id) => id,
            E2eeCmd::Verify { user, on } => {
                let mut g = self.e2ee.lock();
                let Some((server_id, me, devices)) = g.session.as_ref().map(|s| {
                    let mut v = s.dir.get(&s.me).cloned().unwrap_or_default();
                    v.extend(s.dir.get(&user).cloned().unwrap_or_default());
                    (s.server_id.clone(), s.me, v)
                }) else {
                    return;
                };
                if user == me {
                    return;
                }
                g.store().set_verified(&server_id, user, &devices, on);
                info!(user, on, "e2ee: safety code marked");
                g.publish(&self.out);
                return;
            }
        };
        let e2 = self.e2ee.clone();
        let out = self.out.clone();
        let mut g = e2.lock();
        let Some(s) = g.session.as_ref() else { return };
        if s.identity.as_ref().is_some_and(|i| i.device_id_hex() == device_id) {
            return; // never this computer from here
        }
        let (Some(hub), gen) = (self.hub.clone(), g.gen) else {
            g.view.error = Some("Sem conexão com o servidor.".into());
            g.publish(&out);
            return;
        };
        g.view.removing = Some(device_id.clone());
        g.view.error = None;
        g.publish(&out);
        drop(g);
        tokio::spawn(async move {
            let res = hub.delete_e2ee_device(&device_id).await;
            let all = hub.e2ee_devices().await.ok();
            let mut g = e2.lock();
            if g.gen != gen {
                return;
            }
            g.view.removing = None;
            match res {
                Ok(_) => info!(device = short(&device_id), "e2ee: device removed"),
                Err(e) => {
                    warn!(error = %format!("{e:#}"), "e2ee: removing a device failed");
                    g.view.error = Some("Não foi possível remover o aparelho. Tente de novo.".into());
                }
            }
            if let (Some(all), Some(s)) = (all, g.session.as_mut()) {
                s.dir = group(all);
            }
            g.publish(&out);
        });
    }

    /// Logged out (or a new login starts): nothing of the session stays.
    pub(super) fn e2ee_teardown(&mut self) {
        let mut g = self.e2ee.lock();
        g.gen += 1;
        g.session = None;
        g.removed = None;
        g.view = E2eeView::default();
        g.seen.clear();
        g.texts.clear();
        g.new_devices.clear();
        g.server_hint = None;
        dm::forget_image_keys();
        g.publish(&self.out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(user: UserId, n: u8, label: &str) -> E2eeDevice {
        E2eeDevice { user_id: user, device_id: format!("{n:02x}").repeat(16), public_key: vox_e2ee::public_key_to_b64(&[n; 32]), label: label.into(), created_at_ms: 1 }
    }

    /// Audit (v1.1 E0/E2): what the hub says about devices is taken only as far as it can be: an
    /// `E2eeDevicesChanged` about Bia carries only Bia's devices (a device of somebody else slipped
    /// into it would be wrapped for in lines to Bia without any "código de segurança mudou"), and
    /// labels are cleaned again (no direction overrides or invisible characters in the alerts).
    #[test]
    fn device_events_keep_only_that_member_and_clean_labels() {
        let got = devices_of_event(2, vec![dev(2, 1, "PC da Bia"), dev(3, 2, "PC do Caio"), dev(2, 3, "\u{202E}cP\u{200B}")]);
        assert_eq!(got.iter().map(|d| (d.user_id, d.label.as_str())).collect::<Vec<_>>(), vec![(2, "PC da Bia"), (2, "cP")]);
        let dir = group(vec![dev(2, 1, &"x".repeat(80)), dev(3, 2, "\u{2066}\u{2069}")]);
        assert_eq!(dir[&2][0].label.chars().count(), vox_proto::E2EE_LABEL_MAX);
        assert_eq!(dir[&3][0].label, "Computador");
    }
}
