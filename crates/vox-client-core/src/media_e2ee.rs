//! v1.1 (E1): end-to-end encrypted calls, the key exchange and rotation of one voice session.
//!
//! LiveKit's frame cryptor encrypts every track after the encoder (the microphone, the camera and
//! Go Live, also the pre-encoded NVENC / Media Foundation H.264) with a per-sender key ring of
//! [`KEY_RING`] indexes (the room's `e2ee` options, NOT `encryption`: that one also encrypts the
//! data channel under one global key index which per-sender keys overwrite). This module decides
//! which keys go where; [`crate::voice`] executes its [`Action`]s on the room.
//!
//! - **Joining:** a fresh random key at index 0 for my tracks, and a `Hello` (my directory device,
//!   my current index) to everybody present. Whoever hears a Hello from a device listed in the
//!   hub's directory under that person's identity (= user id) answers with their own Hello (once:
//!   so the newcomer learns their device) and, if that person may have their current key, sends it
//!   wrapped for that device ([`vox_e2ee::media`]).
//! - **Rotation:** a newcomer's Hello, or somebody leaving, makes every member draw a fresh key at
//!   `idx + 1 mod 16` ([`ROTATE_DEBOUNCE`] after the last such event), send it to everybody present,
//!   and switch its own cryptors to it [`SWITCH_DELAY`] later (the others have it by then: no
//!   gap). So a newcomer never gets a key from before they arrived, and somebody who left never
//!   gets a key from after.
//! - **Late keys:** a key is set at its own index whatever the order; a late key at an older index
//!   still decrypts the frames sent with it (the ring keeps 16).
//! - **Self-healing:** somebody whose media stays undecryptable for [`REHELLO_AFTER`] (no key, or
//!   a stale one after the ring wrapped) gets a Hello again, which makes them send their keys.
//! - **New publications:** LiveKit starts a new sender cryptor at index 0; the current index is
//!   applied on `LocalTrackPublished` and checked again periodically.
//!
//! Keys are only taken from devices in the directory under the sender's identity; they never go
//! to anybody who did not say Hello with such a device (a hidden participant gets nothing). Keys
//! are never logged, and are wiped when the session ends.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};
use vox_e2ee::media::{self, MediaKey, Message, SenderKey, KEY_RING};
use vox_e2ee::{DeviceId, Identity, KEY_LEN};

use crate::voice::VoiceEvent;

/// How long after a newcomer's Hello or a departure the keys rotate (several arrivals or
/// departures close together rotate once).
pub const ROTATE_DEBOUNCE: Duration = Duration::from_millis(300);
/// The debounce slides with each new arrival or departure, but never past this long after the
/// first one: a participant who keeps sending Hellos while waiting for their first key cannot
/// postpone the rotation for somebody's departure (which would let the leaver keep decrypting).
pub const ROTATE_MAX_DELAY: Duration = Duration::from_millis(1000);
/// Key exchange messages taken from one participant per second; the rest of that second is
/// ignored (a legitimate member sends a few: a Hello, a key per rotation). Each message can cost a
/// key agreement here (opening a key, wrapping ours), so a flood stays bounded.
pub const MAX_MESSAGES_PER_S: u32 = 32;
/// How long after sending a new key a member starts using it (everybody has it by then).
pub const SWITCH_DELAY: Duration = Duration::from_millis(400);
/// Somebody's media undecryptable this long: send them a Hello (again).
pub const REHELLO_AFTER: Duration = Duration::from_millis(1500);
/// At most one Hello answer to the same person this often.
const HELLO_REPLY_GAP: Duration = Duration::from_secs(1);
/// At most one resend of my keys to the same person this often (a Hello flood stays cheap).
const RESEND_GAP: Duration = Duration::from_millis(500);
/// Somebody in the room whose device I still do not know gets my Hello again this often (the
/// first one, sent right after joining, can be lost while the data channel opens).
const HELLO_RETRY: Duration = Duration::from_secs(1);

/// Where the session finds the directory's device keys (the app's E0 directory, kept current by
/// `E2eeDevicesChanged`).
pub trait MediaDirectory: Send + Sync {
    /// The public key of `device` if it is listed under the person whose LiveKit identity (user
    /// id) is `identity`; `None` otherwise.
    fn key_of(&self, identity: &str, device: &DeviceId) -> Option<[u8; KEY_LEN]>;
}

/// What a voice session needs to encrypt its room ([`crate::VoiceConfig::e2ee`]).
#[derive(Clone)]
pub struct MediaE2ee {
    /// This computer's device identity on the hub (E0).
    pub identity: Arc<Identity>,
    pub directory: Arc<dyn MediaDirectory>,
}

impl std::fmt::Debug for MediaE2ee {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaE2ee").field("device", &self.identity.device_id_hex()).finish_non_exhaustive()
    }
}

/// How somebody's media decrypts here (or ours encrypts), from LiveKit's frame cryptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaE2eeState {
    Ok,
    /// No key for the index their frames use: "Aguardando a chave de Fulano…".
    MissingKey,
    /// The key does not open their frames: "Não deu para descriptografar o áudio de Fulano".
    DecryptionFailed,
    /// Our own frames could not be encrypted.
    EncryptionFailed,
    InternalError,
}

impl MediaE2eeState {
    /// From LiveKit's state (`None`: nothing to tell, "new" and "key ratcheted").
    pub fn from_livekit(s: livekit::webrtc::native::frame_cryptor::EncryptionState) -> Option<MediaE2eeState> {
        use livekit::webrtc::native::frame_cryptor::EncryptionState as E;
        Some(match s {
            E::Ok => MediaE2eeState::Ok,
            E::MissingKey => MediaE2eeState::MissingKey,
            E::DecryptionFailed => MediaE2eeState::DecryptionFailed,
            E::EncryptionFailed => MediaE2eeState::EncryptionFailed,
            E::InternalError => MediaE2eeState::InternalError,
            E::New | E::KeyRatcheted => return None,
        })
    }
}

/// What the session does for the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// A key exchange message (reliable, topic [`media::TOPIC`]) to these identities only.
    Send { to: Vec<String>, payload: Vec<u8> },
    /// Into LiveKit's key provider: `identity`'s key at `idx`.
    SetKey { identity: String, idx: u8, key: MediaKeyBox },
    /// Our own sender cryptors use this index from now on (and every new publication too).
    UseIndex(u8),
    Event(VoiceEvent),
}

/// A media key inside an [`Action`]: compares and prints without showing the key.
#[derive(Clone)]
pub struct MediaKeyBox(pub MediaKey);

impl PartialEq for MediaKeyBox {
    fn eq(&self, other: &Self) -> bool {
        *self.0 == *other.0
    }
}
impl Eq for MediaKeyBox {}
impl std::fmt::Debug for MediaKeyBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<key>")
    }
}

/// One of my keys: its ring index, its generation (counts every key I drew in this session),
/// the key.
#[derive(Clone)]
struct MyKey {
    idx: u8,
    gen: u64,
    key: MediaKey,
}

struct Pending {
    key: MyKey,
    switch_at: Instant,
}

struct Peer {
    /// Their device in this room (from a Hello or a key) and its directory key.
    device: Option<(DeviceId, [u8; KEY_LEN])>,
    /// The first of my key generations they may have (0: they were here when I joined; a
    /// newcomer only gets keys drawn after they arrived).
    min_gen: u64,
    /// A newcomer my keys have not rotated for yet: they get nothing until then.
    waiting: bool,
    /// The indexes of their keys I hold (bit per index).
    have: u16,
    state: Option<MediaE2eeState>,
    /// Since when their media does not decrypt here.
    missing_since: Option<Instant>,
    last_hello_to: Option<Instant>,
    last_keys_to: Option<Instant>,
}

impl Peer {
    fn new(min_gen: u64, waiting: bool) -> Peer {
        Peer { device: None, min_gen, waiting, have: 0, state: None, missing_since: None, last_hello_to: None, last_keys_to: None }
    }
}

/// The key exchange of one session (pure: time comes in, [`Action`]s go out).
pub struct Engine {
    room: String,
    me: String,
    identity: Arc<Identity>,
    dir: Arc<dyn MediaDirectory>,
    cur: Option<MyKey>,
    pending: Option<Pending>,
    rotate_due: Option<Instant>,
    /// When the first of the events behind `rotate_due` came ([`ROTATE_MAX_DELAY`]).
    rotate_first: Option<Instant>,
    peers: HashMap<String, Peer>,
    /// Messages taken from each participant in the current second ([`MAX_MESSAGES_PER_S`]).
    budget: HashMap<String, (Instant, u32)>,
}

fn short(d: &DeviceId) -> String {
    vox_e2ee::hex(&d[..4])
}

impl Engine {
    pub fn new(room: &str, me: &str, cfg: &MediaE2ee) -> Engine {
        Engine { room: room.to_string(), me: me.to_string(), identity: cfg.identity.clone(), dir: cfg.directory.clone(), cur: None, pending: None, rotate_due: None, rotate_first: None, peers: HashMap::new(), budget: HashMap::new() }
    }

    /// A rotation [`ROTATE_DEBOUNCE`] from now (several events close together rotate once), but
    /// never later than [`ROTATE_MAX_DELAY`] after the first event still waiting for it.
    fn schedule_rotation(&mut self, now: Instant) {
        let first = *self.rotate_first.get_or_insert(now);
        self.rotate_due = Some((now + ROTATE_DEBOUNCE).min(first + ROTATE_MAX_DELAY));
    }

    /// `who` is somebody I hold a device for and who gets my keys (said Hello with a directory
    /// device, and is no newcomer still waiting for a rotation). In an encrypted room only they can
    /// judge how my video decodes (rx-health): anybody else cannot decrypt it at all.
    pub fn keyed(&self, who: &str) -> bool {
        self.peers.get(who).is_some_and(|p| p.device.is_some() && !p.waiting)
    }

    /// One more message from `from` in this second: `false` once over [`MAX_MESSAGES_PER_S`].
    fn within_budget(&mut self, from: &str, now: Instant) -> bool {
        let e = self.budget.entry(from.to_string()).or_insert((now, 0));
        if now.duration_since(e.0) >= Duration::from_secs(1) {
            *e = (now, 0);
        }
        e.1 = e.1.saturating_add(1);
        if e.1 == MAX_MESSAGES_PER_S + 1 {
            warn!(who = from, "e2ee: too many key exchange messages from one participant: the rest of this second is ignored");
        }
        e.1 <= MAX_MESSAGES_PER_S
    }

    /// The index my cryptors use now.
    pub fn current_index(&self) -> u8 {
        self.cur.as_ref().map_or(0, |k| k.idx)
    }

    fn latest(&self) -> Option<&MyKey> {
        self.pending.as_ref().map(|p| &p.key).or(self.cur.as_ref())
    }

    fn latest_gen(&self) -> u64 {
        self.latest().map_or(0, |k| k.gen)
    }

    fn hello(&self) -> Vec<u8> {
        Message::Hello { device_id: self.identity.device_id, cur_idx: self.current_index() }.encode()
    }

    /// Joined the room with `present` already in it: my first key (index 0) and a Hello to all.
    pub fn start(&mut self, present: &[String], now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        let key = match media::new_media_key() {
            Ok(k) => k,
            Err(e) => {
                warn!(error = %e, "e2ee: no randomness for a media key");
                return out;
            }
        };
        out.push(Action::SetKey { identity: self.me.clone(), idx: 0, key: MediaKeyBox(key.clone()) });
        out.push(Action::UseIndex(0));
        self.cur = Some(MyKey { idx: 0, gen: 0, key });
        let present: Vec<String> = present.iter().filter(|p| **p != self.me).cloned().collect();
        for p in &present {
            let mut peer = Peer::new(0, false);
            peer.last_hello_to = Some(now);
            self.peers.insert(p.clone(), peer);
        }
        if !present.is_empty() {
            out.push(Action::Send { to: present, payload: self.hello() });
        }
        info!(room = %self.room, device = %self.identity.device_id_hex().get(..8).unwrap_or(""), peers = self.peers.len(), "e2ee: media encryption started");
        out
    }

    /// Somebody joined after me: a newcomer (keys only after the next rotation).
    pub fn connected(&mut self, who: &str) {
        if who == self.me || self.peers.contains_key(who) {
            return;
        }
        let min_gen = self.latest_gen() + 1;
        self.peers.insert(who.to_string(), Peer::new(min_gen, true));
    }

    /// Somebody left: they get nothing from now on, and my keys rotate.
    pub fn left(&mut self, who: &str, now: Instant) {
        self.budget.remove(who);
        if self.peers.remove(who).is_some() {
            debug!(who, "e2ee: participant left, keys rotate");
            self.schedule_rotation(now);
        }
    }

    /// A key exchange message from `from` (their LiveKit identity, as the SFU says).
    pub fn packet(&mut self, from: &str, payload: &[u8], now: Instant) -> Vec<Action> {
        if from == self.me || !self.within_budget(from, now) {
            return Vec::new();
        }
        match Message::decode(payload) {
            Ok(Message::Hello { device_id, cur_idx }) => self.on_hello(from, device_id, cur_idx, now),
            Ok(Message::SenderKey(k)) => self.on_sender_key(from, &k, now),
            Err(_) => {
                debug!(from, bytes = payload.len(), "e2ee: unreadable key exchange message ignored");
                Vec::new()
            }
        }
    }

    /// The person behind `from`, a newcomer if I never heard of them.
    fn peer_mut(&mut self, from: &str) -> &mut Peer {
        let min_gen = self.latest_gen() + 1;
        self.peers.entry(from.to_string()).or_insert_with(|| Peer::new(min_gen, true))
    }

    /// `from` is in the room with `device` (directory key `public`): record it; a device seen for
    /// the first time gets my keys (or a rotation, for a newcomer), and so does one asking again
    /// (`asked`: a Hello) at most every [`RESEND_GAP`].
    fn learn_device(&mut self, from: &str, device: DeviceId, public: [u8; KEY_LEN], asked: bool, now: Instant, out: &mut Vec<Action>) -> bool {
        let peer = self.peer_mut(from);
        let is_new = peer.device.map(|d| d.0) != Some(device);
        if is_new {
            peer.device = Some((device, public));
            peer.have = 0;
            peer.last_keys_to = None;
            info!(who = from, device = %short(&device), "e2ee: participant's device");
            out.push(Action::Event(VoiceEvent::E2eePeerDevice { identity: from.to_string(), device_id: vox_e2ee::hex(&device) }));
        }
        let waiting = self.peers.get(from).is_some_and(|p| p.waiting);
        if waiting {
            // A newcomer: my keys rotate for them (they never get one from before they came).
            self.schedule_rotation(now);
        } else {
            let gap_ok = self.peers.get(from).and_then(|p| p.last_keys_to).is_none_or(|t| now.duration_since(t) >= RESEND_GAP);
            if is_new || (asked && gap_ok) {
                out.extend(self.keys_for(from, now));
            }
        }
        is_new
    }

    fn on_hello(&mut self, from: &str, device: DeviceId, cur_idx: u8, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        let Some(public) = self.dir.key_of(from, &device) else {
            warn!(who = from, device = %short(&device), "e2ee: Hello from a device that is not in the directory under that person: ignored");
            return out;
        };
        let is_new = self.learn_device(from, device, public, true, now, &mut out);
        let peer = self.peers.get_mut(from).expect("learned above");
        let lacking = peer.have & (1 << cur_idx) == 0;
        let gap_ok = peer.last_hello_to.is_none_or(|t| now.duration_since(t) >= HELLO_REPLY_GAP);
        if gap_ok && (is_new || lacking) {
            peer.last_hello_to = Some(now);
            out.push(Action::Send { to: vec![from.to_string()], payload: self.hello() });
        }
        out
    }

    fn on_sender_key(&mut self, from: &str, k: &SenderKey, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        let Some(public) = self.dir.key_of(from, &k.from_dev) else {
            warn!(who = from, device = %short(&k.from_dev), "e2ee: key from a device that is not in the directory under that person: ignored");
            return out;
        };
        let key = match media::open_key(&self.identity, &self.room, from, &self.me, &public, k) {
            Ok(key) => key,
            Err(_) => {
                warn!(who = from, idx = k.idx, "e2ee: a media key that does not open here was ignored");
                return out;
            }
        };
        self.learn_device(from, k.from_dev, public, false, now, &mut out);
        if let Some(p) = self.peers.get_mut(from) {
            p.have |= 1 << k.idx;
        }
        debug!(who = from, idx = k.idx, "e2ee: media key received");
        out.push(Action::SetKey { identity: from.to_string(), idx: k.idx, key: MediaKeyBox(key) });
        out
    }

    /// My keys `who` may have (drawn since they arrived), wrapped for their device.
    fn keys_for(&mut self, who: &str, now: Instant) -> Vec<Action> {
        let Some(peer) = self.peers.get(who) else { return Vec::new() };
        let Some((dev, public)) = peer.device else { return Vec::new() };
        if peer.waiting {
            return Vec::new();
        }
        let min_gen = peer.min_gen;
        let mut keys: Vec<&MyKey> = self.cur.iter().chain(self.pending.as_ref().map(|p| &p.key)).filter(|k| k.gen >= min_gen).collect();
        keys.dedup_by_key(|k| k.gen);
        let mut out = Vec::new();
        for k in keys {
            match media::seal_key(&self.identity, &self.room, &self.me, who, &dev, &public, k.idx, &k.key) {
                Ok(sk) => out.push(Action::Send { to: vec![who.to_string()], payload: Message::SenderKey(sk).encode() }),
                Err(e) => warn!(who, error = %e, "e2ee: could not wrap my key for a participant"),
            }
        }
        if let Some(p) = self.peers.get_mut(who) {
            p.last_keys_to = Some(now);
        }
        out
    }

    /// LiveKit's frame cryptor state for `who`'s media (or, for my own identity, my tracks).
    pub fn state(&mut self, who: &str, state: MediaE2eeState, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        if who == self.me {
            if state != MediaE2eeState::Ok {
                warn!(?state, "e2ee: my own media encryption reports a problem");
            }
            out.push(Action::Event(VoiceEvent::E2eeState { identity: who.to_string(), state }));
            return out;
        }
        let Some(p) = self.peers.get_mut(who) else { return out };
        match state {
            MediaE2eeState::MissingKey | MediaE2eeState::DecryptionFailed => {
                p.missing_since.get_or_insert(now);
            }
            _ => p.missing_since = None,
        }
        if p.state != Some(state) {
            p.state = Some(state);
            info!(who, ?state, "e2ee: media state");
            out.push(Action::Event(VoiceEvent::E2eeState { identity: who.to_string(), state }));
        }
        out
    }

    /// A new local publication: its cryptor starts at index 0, give it mine.
    pub fn local_published(&self) -> Vec<Action> {
        vec![Action::UseIndex(self.current_index())]
    }

    /// Timers: a due rotation, a due switch, Hellos to people whose media stays undecryptable.
    pub fn tick(&mut self, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        if self.rotate_due.is_some_and(|d| d <= now) {
            out.extend(self.rotate(now));
        }
        if self.pending.as_ref().is_some_and(|p| p.switch_at <= now) {
            let p = self.pending.take().expect("checked");
            info!(idx = p.key.idx, gen = p.key.gen, "e2ee: switched to my new media key");
            out.push(Action::UseIndex(p.key.idx));
            self.cur = Some(p.key);
        }
        let hello = self.hello();
        let mut again = Vec::new();
        for (who, p) in self.peers.iter_mut() {
            let stuck = p.missing_since.is_some_and(|t| now.duration_since(t) >= REHELLO_AFTER);
            let gap_ok = p.last_hello_to.is_none_or(|t| now.duration_since(t) >= REHELLO_AFTER);
            let unknown = p.device.is_none() && p.last_hello_to.is_some_and(|t| now.duration_since(t) >= HELLO_RETRY);
            if (stuck && gap_ok) || unknown {
                p.last_hello_to = Some(now);
                again.push(who.clone());
            }
        }
        if !again.is_empty() {
            again.sort();
            info!(who = ?again, "e2ee: Hello again (media undecryptable, or their device still unknown)");
            out.push(Action::Send { to: again, payload: hello });
        }
        out
    }

    /// A fresh key at the next index for everybody present (who may have it), used after
    /// [`SWITCH_DELAY`].
    fn rotate(&mut self, now: Instant) -> Vec<Action> {
        self.rotate_due = None;
        self.rotate_first = None;
        let (idx, gen) = match self.latest() {
            Some(k) => ((k.idx + 1) % KEY_RING, k.gen + 1),
            None => return Vec::new(),
        };
        let key = match media::new_media_key() {
            Ok(k) => k,
            Err(e) => {
                warn!(error = %e, "e2ee: no randomness for a media key");
                return Vec::new();
            }
        };
        let mut out = vec![Action::SetKey { identity: self.me.clone(), idx, key: MediaKeyBox(key.clone()) }];
        self.pending = Some(Pending { key: MyKey { idx, gen, key }, switch_at: now + SWITCH_DELAY });
        for p in self.peers.values_mut() {
            if p.waiting && p.min_gen <= gen {
                p.waiting = false;
            }
        }
        let mut who: Vec<String> = self.peers.keys().cloned().collect();
        who.sort();
        let mut sent = 0;
        for w in who {
            let min_gen = self.peers[&w].min_gen;
            let Some((dev, public)) = self.peers[&w].device else { continue };
            if min_gen > gen {
                continue;
            }
            let k = &self.pending.as_ref().expect("set above").key;
            match media::seal_key(&self.identity, &self.room, &self.me, &w, &dev, &public, k.idx, &k.key) {
                Ok(sk) => {
                    out.push(Action::Send { to: vec![w.clone()], payload: Message::SenderKey(sk).encode() });
                    sent += 1;
                }
                Err(e) => warn!(who = %w, error = %e, "e2ee: could not wrap my new key"),
            }
        }
        info!(idx, gen, sent, "e2ee: my media key rotated");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A directory: identity -> devices.
    #[derive(Default)]
    struct Dir(Mutex<HashMap<String, Vec<(DeviceId, [u8; 32])>>>);

    impl MediaDirectory for Dir {
        fn key_of(&self, identity: &str, device: &DeviceId) -> Option<[u8; 32]> {
            self.0.lock().unwrap().get(identity)?.iter().find(|(d, _)| d == device).map(|(_, k)| *k)
        }
    }

    struct Member {
        id: String,
        engine: Engine,
        /// identity -> idx -> key (what LiveKit's key provider got).
        keys: HashMap<String, HashMap<u8, [u8; 32]>>,
        index: u8,
        events: Vec<VoiceEvent>,
    }

    /// A room of members exchanging messages through a perfect data channel.
    struct Room {
        dir: Arc<Dir>,
        members: Vec<Member>,
        /// (to, from, payload), delivered by `deliver`.
        wire: Vec<(String, String, Vec<u8>)>,
        /// Every message ever sent (to, from, kind).
        log: Vec<(String, String, u8)>,
        /// Messages of this kind to this member are lost.
        drop: Vec<(String, u8)>,
        t: Instant,
    }

    impl Room {
        fn new() -> Room {
            Room { dir: Arc::new(Dir::default()), members: Vec::new(), wire: Vec::new(), log: Vec::new(), drop: Vec::new(), t: Instant::now() }
        }

        fn idx(&self, id: &str) -> usize {
            self.members.iter().position(|m| m.id == id).unwrap()
        }

        fn apply(&mut self, i: usize, actions: Vec<Action>) {
            for a in actions {
                match a {
                    Action::Send { to, payload } => {
                        for t in to {
                            self.log.push((t.clone(), self.members[i].id.clone(), payload[1]));
                            if !self.drop.contains(&(t.clone(), payload[1])) {
                                self.wire.push((t, self.members[i].id.clone(), payload.clone()));
                            }
                        }
                    }
                    Action::SetKey { identity, idx, key } => {
                        self.members[i].keys.entry(identity).or_default().insert(idx, *key.0);
                    }
                    Action::UseIndex(idx) => self.members[i].index = idx,
                    Action::Event(e) => self.members[i].events.push(e),
                }
            }
        }

        fn join(&mut self, id: &str) {
            let identity = Arc::new(Identity::generate().unwrap());
            self.dir.0.lock().unwrap().entry(id.to_string()).or_default().push((identity.device_id, identity.public_key()));
            let cfg = MediaE2ee { identity, directory: self.dir.clone() };
            let present: Vec<String> = self.members.iter().map(|m| m.id.clone()).collect();
            for i in 0..self.members.len() {
                self.members[i].engine.connected(id);
            }
            self.members.push(Member { id: id.into(), engine: Engine::new("voice-1", id, &cfg), keys: HashMap::new(), index: 99, events: Vec::new() });
            let i = self.members.len() - 1;
            let t = self.t;
            let a = self.members[i].engine.start(&present, t);
            self.apply(i, a);
        }

        fn leave(&mut self, id: &str) {
            let i = self.idx(id);
            self.members.remove(i);
            self.wire.retain(|(to, _, _)| to != id);
            let t = self.t;
            for i in 0..self.members.len() {
                self.members[i].engine.left(id, t);
            }
        }

        fn deliver(&mut self) {
            while !self.wire.is_empty() {
                let batch = std::mem::take(&mut self.wire);
                for (to, from, payload) in batch {
                    let Some(i) = self.members.iter().position(|m| m.id == to) else { continue };
                    let t = self.t;
                    let a = self.members[i].engine.packet(&from, &payload, t);
                    self.apply(i, a);
                }
            }
        }

        /// Advance the clock by `ms`, ticking every 50 ms and delivering everything.
        fn run(&mut self, ms: u64) {
            let end = self.t + Duration::from_millis(ms);
            while self.t < end {
                self.t += Duration::from_millis(50);
                for i in 0..self.members.len() {
                    let t = self.t;
                    let a = self.members[i].engine.tick(t);
                    self.apply(i, a);
                }
                self.deliver();
            }
        }

        /// Can `viewer` decrypt `sender`'s media right now (has the key at the index in use)?
        fn decrypts(&self, viewer: &str, sender: &str) -> bool {
            let s = &self.members[self.idx(sender)];
            let own = s.keys[sender][&s.index];
            self.members[self.idx(viewer)].keys.get(sender).and_then(|k| k.get(&s.index)) == Some(&own)
        }

        fn all_decrypt(&self) -> bool {
            self.members.iter().all(|a| self.members.iter().all(|b| a.id == b.id || self.decrypts(&a.id, &b.id)))
        }

        /// Every key `viewer` ever got from `sender`.
        fn keys_of(&self, viewer: &str, sender: &str) -> Vec<[u8; 32]> {
            self.members[self.idx(viewer)].keys.get(sender).map(|m| m.values().copied().collect()).unwrap_or_default()
        }
    }

    #[test]
    fn two_people_exchange_keys_and_decrypt_each_other() {
        let mut r = Room::new();
        r.join("1");
        r.run(100);
        r.join("2");
        r.run(1000);
        assert!(r.all_decrypt());
        assert!(r.members.iter().all(|m| m.events.iter().any(|e| matches!(e, VoiceEvent::E2eePeerDevice { .. }))));
    }

    #[test]
    fn a_newcomer_never_gets_an_earlier_key_and_a_leaver_never_a_later_one() {
        let mut r = Room::new();
        r.join("1");
        r.join("2");
        r.run(1000);
        assert!(r.all_decrypt());
        // What 1 used before 3 came.
        let before: Vec<[u8; 32]> = r.keys_of("1", "1");
        r.join("3");
        r.run(1500);
        assert!(r.all_decrypt(), "everybody decrypts everybody after the rotation");
        let got = r.keys_of("3", "1");
        assert!(!got.is_empty());
        assert!(got.iter().all(|k| !before.contains(k)), "3 has none of 1's earlier keys");
        // (1 rotated for 2's arrival, then for 3's.)
        assert_eq!(r.members[r.idx("1")].index, 2);
        // 3 leaves: 1 and 2 rotate; the new keys are never addressed to 3.
        let with3 = r.keys_of("1", "1");
        let sent_before = r.log.len();
        r.leave("3");
        r.run(1500);
        assert!(r.all_decrypt());
        let now = r.keys_of("1", "1");
        assert_eq!(now.iter().filter(|k| !with3.contains(k)).count(), 1, "one new key after the departure");
        assert!(r.log[sent_before..].iter().all(|(to, _, _)| to != "3"));
        assert_eq!(r.members[r.idx("1")].index, 3);
    }

    #[test]
    fn rotation_is_debounced_and_the_switch_waits_for_everybody() {
        let mut r = Room::new();
        r.join("1");
        r.join("2");
        r.run(1000);
        let t0 = r.t;
        // Two newcomers 100 ms apart: one rotation, 300 ms after the last Hello.
        r.join("3");
        r.deliver();
        r.t = t0 + Duration::from_millis(100);
        r.join("4");
        r.deliver();
        let i = r.idx("1");
        let t = t0 + Duration::from_millis(350);
        let a = r.members[i].engine.tick(t);
        assert!(!a.iter().any(|a| matches!(a, Action::SetKey { identity, .. } if identity == "1")), "not yet (debounce slides): {a:?}");
        let t = t0 + Duration::from_millis(400);
        let a = r.members[i].engine.tick(t);
        let new_keys: Vec<_> = a.iter().filter(|a| matches!(a, Action::SetKey { identity, idx: 2, .. } if identity == "1")).collect();
        assert_eq!(new_keys.len(), 1, "one rotation for both: {a:?}");
        let sends = a.iter().filter(|a| matches!(a, Action::Send { .. })).count();
        assert_eq!(sends, 3, "the new key goes to 2, 3 and 4");
        assert!(!a.iter().any(|a| matches!(a, Action::UseIndex(_))), "the switch waits");
        let a = r.members[i].engine.tick(t + SWITCH_DELAY - Duration::from_millis(1));
        assert!(!a.iter().any(|a| matches!(a, Action::UseIndex(_))));
        let a = r.members[i].engine.tick(t + SWITCH_DELAY);
        assert!(a.contains(&Action::UseIndex(2)));
    }

    #[test]
    fn the_ring_wraps_from_15_to_0() {
        let mut r = Room::new();
        r.join("1");
        r.join("2");
        r.run(800);
        for n in 1..=17u32 {
            let id = format!("x{n}");
            r.join(&id);
            r.run(900);
            r.leave(&id);
            r.run(900);
            assert!(r.all_decrypt(), "round {n}");
        }
        // 1 rotated once for 2, then both twice a round: (1 + 34) mod 16 = 3 and 34 mod 16 = 2.
        assert_eq!(r.members[r.idx("1")].index, 3);
        assert_eq!(r.members[r.idx("2")].index, 2);
    }

    #[test]
    fn a_late_key_at_the_old_index_is_still_accepted() {
        let dir = Arc::new(Dir::default());
        let a = Arc::new(Identity::generate().unwrap());
        let b = Arc::new(Identity::generate().unwrap());
        dir.0.lock().unwrap().insert("1".into(), vec![(a.device_id, a.public_key())]);
        dir.0.lock().unwrap().insert("2".into(), vec![(b.device_id, b.public_key())]);
        let mut eb = Engine::new("voice-1", "2", &MediaE2ee { identity: b.clone(), directory: dir.clone() });
        let t = Instant::now();
        eb.start(&["1".to_string()], t);
        // Keys of 1 at index 5 and then (late) index 4.
        let k5 = media::seal_key(&a, "voice-1", "1", "2", &b.device_id, &b.public_key(), 5, &[5; 32]).unwrap();
        let k4 = media::seal_key(&a, "voice-1", "1", "2", &b.device_id, &b.public_key(), 4, &[4; 32]).unwrap();
        let got5 = eb.packet("1", &Message::SenderKey(k5).encode(), t);
        assert!(got5.contains(&Action::SetKey { identity: "1".into(), idx: 5, key: MediaKeyBox(vox_e2ee::zeroize::Zeroizing::new([5; 32])) }));
        let got4 = eb.packet("1", &Message::SenderKey(k4).encode(), t);
        assert!(got4.contains(&Action::SetKey { identity: "1".into(), idx: 4, key: MediaKeyBox(vox_e2ee::zeroize::Zeroizing::new([4; 32])) }), "{got4:?}");
    }

    #[test]
    fn keys_only_from_directory_devices_and_for_this_room_and_person() {
        let dir = Arc::new(Dir::default());
        let a = Arc::new(Identity::generate().unwrap());
        let b = Arc::new(Identity::generate().unwrap());
        let stranger = Identity::generate().unwrap();
        dir.0.lock().unwrap().insert("1".into(), vec![(a.device_id, a.public_key())]);
        dir.0.lock().unwrap().insert("2".into(), vec![(b.device_id, b.public_key())]);
        let mut eb = Engine::new("voice-1", "2", &MediaE2ee { identity: b.clone(), directory: dir.clone() });
        let t = Instant::now();
        eb.start(&["1".to_string(), "3".to_string()], t);
        let none = |v: &Vec<Action>| !v.iter().any(|a| matches!(a, Action::SetKey { .. }));
        // A device not in the directory (under anybody), sent as "1".
        let k = media::seal_key(&stranger, "voice-1", "1", "2", &b.device_id, &b.public_key(), 0, &[1; 32]).unwrap();
        assert!(none(&eb.packet("1", &Message::SenderKey(k).encode(), t)));
        // 1's real device, but relayed as if 3 sent it (the SFU lying about the sender).
        let k = media::seal_key(&a, "voice-1", "1", "2", &b.device_id, &b.public_key(), 0, &[1; 32]).unwrap();
        assert!(none(&eb.packet("3", &Message::SenderKey(k.clone()).encode(), t)));
        // Another room's key.
        let other = media::seal_key(&a, "voice-2", "1", "2", &b.device_id, &b.public_key(), 0, &[1; 32]).unwrap();
        assert!(none(&eb.packet("1", &Message::SenderKey(other).encode(), t)));
        // A Hello from a device that is not 1's: no key goes out, no answer.
        let hello = Message::Hello { device_id: stranger.device_id, cur_idx: 0 }.encode();
        assert!(eb.packet("1", &hello, t).is_empty());
        // The real one is taken.
        assert!(!none(&eb.packet("1", &Message::SenderKey(k).encode(), t)));
        // My own identity's messages are ignored; garbage is ignored.
        assert!(eb.packet("2", &hello, t).is_empty());
        assert!(eb.packet("1", &[1, 2, 3], t).is_empty());
    }

    #[test]
    fn the_current_index_is_applied_to_new_publications() {
        let mut r = Room::new();
        r.join("1");
        r.join("2");
        r.run(1000);
        r.join("3");
        r.run(1500);
        let e = &r.members[r.idx("1")].engine;
        assert_eq!(e.current_index(), 2);
        assert_eq!(e.local_published(), vec![Action::UseIndex(2)]);
    }

    #[test]
    fn undecryptable_media_asks_for_keys_again() {
        let mut r = Room::new();
        // 2's keys to 1 get lost.
        r.drop.push(("1".into(), 2));
        r.join("1");
        r.join("2");
        r.run(1000);
        assert!(!r.decrypts("1", "2"));
        assert!(r.decrypts("2", "1"), "the other way works");
        r.drop.clear();
        // 1's cryptor says MissingKey for 2; after 1.5 s 1 says Hello again, and 2 resends.
        let i = r.idx("1");
        let t = r.t;
        let a = r.members[i].engine.state("2", MediaE2eeState::MissingKey, t);
        assert!(a.iter().any(|a| matches!(a, Action::Event(VoiceEvent::E2eeState { state: MediaE2eeState::MissingKey, .. }))));
        r.apply(i, a);
        r.run(1000);
        assert!(!r.decrypts("1", "2"), "not yet");
        r.run(1000);
        assert!(r.decrypts("1", "2"), "healed by the second Hello");
    }

    #[test]
    fn a_hello_flood_cannot_postpone_the_rotation_for_a_departure() {
        // Audit (E1): a newcomer keeps saying Hello every 100 ms while waiting for its first key;
        // each Hello used to push the debounced rotation further, so a leaver's key stayed in use.
        let dir = Arc::new(Dir::default());
        let a = Arc::new(Identity::generate().unwrap());
        let x = Identity::generate().unwrap();
        dir.0.lock().unwrap().insert("1".into(), vec![(a.device_id, a.public_key())]);
        dir.0.lock().unwrap().insert("9".into(), vec![(x.device_id, x.public_key())]);
        let mut e = Engine::new("voice-1", "1", &MediaE2ee { identity: a.clone(), directory: dir.clone() });
        let t0 = Instant::now();
        e.start(&["2".to_string()], t0);
        e.connected("9");
        // 2 leaves: a rotation is due.
        e.left("2", t0);
        let hello = Message::Hello { device_id: x.device_id, cur_idx: 0 }.encode();
        let mut rotated_at = None;
        for n in 1..=40u64 {
            let t = t0 + Duration::from_millis(100 * n);
            e.packet("9", &hello, t);
            let a = e.tick(t);
            if a.iter().any(|a| matches!(a, Action::SetKey { identity, .. } if identity == "1")) {
                rotated_at = Some(t);
                break;
            }
        }
        let at = rotated_at.expect("the rotation happens despite the Hellos");
        assert!(at.duration_since(t0) <= ROTATE_MAX_DELAY + Duration::from_millis(100), "rotated after {:?}", at.duration_since(t0));
    }

    #[test]
    fn a_message_flood_from_one_participant_is_capped() {
        let dir = Arc::new(Dir::default());
        let a = Arc::new(Identity::generate().unwrap());
        let b = Arc::new(Identity::generate().unwrap());
        dir.0.lock().unwrap().insert("1".into(), vec![(a.device_id, a.public_key())]);
        dir.0.lock().unwrap().insert("2".into(), vec![(b.device_id, b.public_key())]);
        let mut eb = Engine::new("voice-1", "2", &MediaE2ee { identity: b.clone(), directory: dir.clone() });
        let t = Instant::now();
        eb.start(&["1".to_string()], t);
        let k = Message::SenderKey(media::seal_key(&a, "voice-1", "1", "2", &b.device_id, &b.public_key(), 1, &[7; 32]).unwrap()).encode();
        let taken = (0..100).filter(|_| eb.packet("1", &k, t).iter().any(|a| matches!(a, Action::SetKey { .. }))).count();
        assert_eq!(taken, MAX_MESSAGES_PER_S as usize, "the rest of the second is ignored");
        // The next second, messages are taken again; somebody else was never limited.
        let t2 = t + Duration::from_secs(1);
        assert!(eb.packet("1", &k, t2).iter().any(|a| matches!(a, Action::SetKey { .. })));
    }

    #[test]
    fn only_keyed_participants_count_for_rx_health() {
        let mut r = Room::new();
        r.join("1");
        let i = r.idx("1");
        // A participant that never says Hello (no keys, cannot decrypt anything of ours).
        r.members[i].engine.connected("9");
        r.join("2");
        r.run(1500);
        let e = &r.members[r.idx("1")].engine;
        assert!(e.keyed("2"));
        assert!(!e.keyed("9"));
        assert!(!e.keyed("7"), "unknown");
    }

    #[test]
    fn nothing_for_people_who_never_said_hello() {
        let mut r = Room::new();
        r.join("1");
        // A participant in the room (hidden, or an app without encryption) never says Hello.
        let i = r.idx("1");
        r.members[i].engine.connected("9");
        r.run(2000);
        // Rotations for 2 coming and going: 9 has no device, nothing is ever wrapped for it.
        r.join("2");
        r.run(1000);
        r.leave("2");
        r.run(1000);
        assert!(r.log.iter().all(|(to, _, kind)| to != "9" || *kind == 1), "only Hellos, never a key: {:?}", r.log);
    }
}
