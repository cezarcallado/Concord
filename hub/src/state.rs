//! Shared server state: presence, voice states, event fan-out, chat rate limiting.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::{broadcast, watch};
use vox_proto::{Account, Channel, ChannelId, ChannelKind, CustomStatus, Presence, ServerEvent, User, UserId, VoiceMember};

use crate::access;
use crate::auth::Throttle;
use crate::config::Config;
use crate::db::Db;
use crate::linkpreview::LinkPreviews;
use crate::livekit_admin::LiveKitAdmin;
use crate::profile::PresenceEntry;

pub type AppState = Arc<Shared>;

const MSG_RATE_PER_SEC: f64 = 5.0;
const MSG_BURST: f64 = 5.0;
/// Reactions (v0.8) have their own bucket: a few clicks in a row are normal, a flood is not.
const REACTION_RATE_PER_SEC: f64 = 4.0;
const REACTION_BURST: f64 = 12.0;
/// Most streams one person can be listed as watching (item 9 will allow several).
const MAX_WATCHING: usize = 8;
/// `WatchState` token bucket: a real client sends one per watch/unwatch click.
const WATCH_RATE_PER_SEC: f64 = 2.0;
const WATCH_BURST: f64 = 20.0;
/// `VoiceState` token bucket: a real client sends one per join / leave / mute / deafen / stream /
/// camera click. Over it, a connection keeps only the newest state and applies it when the bucket
/// allows (`ws.rs`), so the list stays right without a flood reaching everybody.
pub const VOICE_RATE_PER_SEC: f64 = 2.0;
pub const VOICE_BURST: f64 = 10.0;
/// Open WebSockets per person (the app uses one; a few more for a second PC or a reconnect race).
pub const MAX_WS_PER_USER: usize = 8;

/// What the fan-out channel carries to every WebSocket task.
#[derive(Debug, Clone)]
pub enum Out {
    /// An event (already JSON) for everybody.
    All(Arc<str>),
    /// An event for one user's connections only.
    To(UserId, Arc<str>),
    /// An event for the connections of several users (the members of a voice channel).
    Many(Arc<[UserId]>, Arc<str>),
    /// An event about channel `ch` (v0.9): only to the people who may see it at delivery time
    /// (`access.rs`), and with `to`, only to those of them.
    Channel { ch: ChannelId, to: Option<Arc<[UserId]>>, text: Arc<str> },
    /// `VoiceStateChanged` of `member`, now in `ch`, before in `prev` (v0.9): each recipient gets
    /// `full`, `left` (the member went where they may not see) or nothing ([`access::voice_view`]).
    Voice { member: UserId, ch: Option<ChannelId>, prev: Option<ChannelId>, full: Arc<str>, left: Arc<str> },
    /// The channel list, a channel's access or somebody's role changed (v0.9): every connection
    /// works out again what it may see, and sends its own `ChannelsChanged` when that changed.
    Access,
    /// Close the user's connections, except the one opened with session `keep` (a token hash).
    Kick { user: UserId, keep: Option<Arc<str>> },
}

pub struct Shared {
    pub cfg: Config,
    pub db: Db,
    /// This hub's identity (v1.0.1, `db::server_id`): in `/api/health` and `Ready`.
    pub server_id: String,
    pub previews: LinkPreviews,
    pub shutdown: watch::Receiver<bool>,
    /// Login brakes (item 12).
    pub throttle: Throttle,
    /// LiveKit's server API (item 12: moderation enforced in the SFU).
    pub livekit: LiveKitAdmin,
    /// Profile pictures: rate limit and decode slots.
    pub avatars: crate::avatar::AvatarState,
    /// Soundboard: upload and play rate limits.
    pub sounds: crate::sounds::SoundState,
    /// Who the owner is, and the claim code while there is none ("quem hospeda vira dono").
    pub ownership: crate::owner::OwnerState,
    /// Private 1:1 calls from DMs (ringing and answered).
    pub calls: crate::calls::CallsState,
    /// Profiles: edit and presence rate limits.
    pub profiles: crate::profile::ProfileState,
    /// Chat images: where the files live, quotas, upload rate limit.
    pub attachments: crate::attachments::AttachmentState,
    /// Timings and limits (`crate::Tuning`).
    pub tuning: crate::Tuning,
    /// LiveKit's rooms against the voice list (`voice_guard.rs`).
    pub voice_guard: crate::voice_guard::VoiceGuard,
    /// argon2 runs at once (logins with a personal password, password changes): bounded, so a
    /// burst of guesses cannot fill the blocking pool the database shares.
    pub argon2: Arc<tokio::sync::Semaphore>,
    /// Private channels: who may see what (`access.rs`), the hub's one access check.
    pub access: access::Access,
    /// Go Live hover previews (v1.1, `stream_preview.rs`): the latest picture of each live.
    pub stream_previews: crate::stream_preview::StreamPreviews,
    /// v1.1 (T0): the SHA-256 of this hub's TLS certificate (unset with `tls = "off"`), for the
    /// invites (`InviteHints::tls_pin`).
    pub tls_pin: std::sync::OnceLock<[u8; 32]>,
    /// v1.1 (E0): the directory of device keys' rate limit (`e2ee.rs`).
    pub e2ee: crate::e2ee::E2eeState,
    channels: Mutex<Vec<Channel>>,
    tx: broadcast::Sender<Out>,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// user -> (user, open connection count)
    online: HashMap<UserId, (User, usize)>,
    voice: HashMap<UserId, VoiceMember>,
    /// token buckets for chat rate limiting: (tokens, last refill)
    buckets: HashMap<UserId, (f64, Instant)>,
    /// same, for `WatchState`
    watch_buckets: HashMap<UserId, (f64, Instant)>,
    /// same, for `VoiceState`
    voice_buckets: HashMap<UserId, (f64, Instant)>,
    /// same, for reactions
    reaction_buckets: HashMap<UserId, (f64, Instant)>,
    /// Users a moderator server-muted (mirrors the database).
    server_muted: HashSet<UserId>,
    /// Presence and custom status of everybody connected (an invisible person is in here too:
    /// everything that is *sent* about them is filtered through `visible`).
    presence: HashMap<UserId, PresenceEntry>,
}

impl Inner {
    /// Do other people see `user_id` as online?
    fn visible(&self, user_id: UserId) -> bool {
        self.online.contains_key(&user_id) && self.presence.get(&user_id).is_none_or(|p| p.effective().visible_to_others())
    }

    /// `streamer` stopped being watchable (left voice, changed channel, stopped streaming):
    /// take them out of every viewer's list. Returns the viewers' updated members.
    fn drop_viewers_of(&mut self, streamer: UserId) -> Vec<VoiceMember> {
        let mut changed = Vec::new();
        for m in self.voice.values_mut() {
            if m.watching.contains(&streamer) {
                m.watching.retain(|id| *id != streamer);
                changed.push(m.clone());
            }
        }
        changed.sort_by_key(|m| m.user.id);
        changed
    }
}

/// `m` as somebody who left voice.
pub(crate) fn left(mut m: VoiceMember) -> VoiceMember {
    m.channel_id = None;
    m.muted = false;
    m.deafened = false;
    m.streaming = false;
    m.audio_only = false;
    m.camera = false;
    m.server_muted = false;
    m.watching.clear();
    m
}

impl Shared {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Config,
        db: Db,
        server_id: String,
        channels: Vec<Channel>,
        server_muted: Vec<UserId>,
        previews: LinkPreviews,
        livekit: LiveKitAdmin,
        attachments: crate::attachments::AttachmentState,
        tuning: crate::Tuning,
        shutdown: watch::Receiver<bool>,
    ) -> AppState {
        let (tx, _) = broadcast::channel(256);
        let inner = Inner { server_muted: server_muted.into_iter().collect(), ..Default::default() };
        let ownership = crate::owner::OwnerState::new(&cfg.owner);
        let access = access::Access::default();
        access.set_channels(&channels);
        Arc::new(Shared {
            access,
            cfg,
            db,
            server_id,
            previews,
            shutdown,
            throttle: Throttle::default(),
            livekit,
            avatars: Default::default(),
            sounds: Default::default(),
            ownership,
            calls: Default::default(),
            profiles: Default::default(),
            attachments,
            tuning,
            voice_guard: Default::default(),
            argon2: Arc::new(tokio::sync::Semaphore::new(crate::auth::ARGON2_AT_ONCE)),
            stream_previews: Default::default(),
            tls_pin: Default::default(),
            e2ee: Default::default(),
            channels: Mutex::new(channels),
            tx,
            inner: Mutex::new(inner),
        })
    }

    /// The owner's username right now (empty: none yet). `config.toml`'s `owner` when it pins one,
    /// otherwise the stored one.
    pub fn owner(&self) -> String {
        self.ownership.name()
    }

    /// `row`'s account as the client sees it (`Ready`, password changes, claiming).
    pub fn account(&self, row: &crate::db::UserRow) -> Account {
        let mut a = row.account(&self.owner());
        a.no_owner = !self.ownership.has_owner();
        a.owner_pinned = self.ownership.pinned();
        a
    }

    /// Any channel, private or not: only for code that checks access itself (or is Admin+ only).
    pub fn channel(&self, id: i64) -> Option<Channel> {
        self.channels.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|c| c.id == id).cloned()
    }

    /// Channel `id` if it exists *and* `user` may see it (v0.9): what every read by id uses, so a
    /// private channel without access looks exactly like an unknown one.
    pub fn channel_for(&self, user: UserId, id: ChannelId) -> Option<Channel> {
        self.channel(id).filter(|_| self.can_see(user, id))
    }

    /// Every channel, private or not (see [`Shared::visible_channels`]).
    pub fn channels(&self) -> Vec<Channel> {
        self.channels.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// May `user` see channel `ch`? (`access.rs`; in memory.)
    pub fn can_see(&self, user: UserId, ch: ChannelId) -> bool {
        self.access.rules().can_see(user, ch)
    }

    /// The channels `user` may see, in order.
    pub fn visible_channels(&self, user: UserId) -> Vec<Channel> {
        let rules = self.access.rules();
        self.channels().into_iter().filter(|c| rules.can_see(user, c.id)).collect()
    }

    /// The channel list changed (item 12; v0.9: or a channel's access): store it, and every
    /// connection sends its own view ([`Out::Access`]). The rules change first, so no event of a
    /// channel that just became private slips out under the old ones.
    pub fn set_channels(&self, channels: Vec<Channel>) {
        self.access.set_channels(&channels);
        *self.channels.lock().unwrap_or_else(|e| e.into_inner()) = channels;
        let _ = self.tx.send(Out::Access);
    }

    /// New effective roles for the access rules (`access::reload_roles`). Returns whether anything
    /// changed (then every connection works out its view again).
    pub fn set_roles(&self, roles: HashMap<UserId, vox_proto::Role>) -> bool {
        let changed = self.access.set_roles(roles);
        if changed {
            let _ = self.tx.send(Out::Access);
        }
        changed
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Fan an event out to every connection. Serialised once.
    pub fn broadcast(&self, ev: &ServerEvent) {
        if let Ok(s) = serde_json::to_string(ev) {
            let _ = self.tx.send(Out::All(s.into()));
        }
    }

    /// An event for one user's connections only.
    pub fn send_to(&self, user: UserId, ev: &ServerEvent) {
        if let Ok(s) = serde_json::to_string(ev) {
            let _ = self.tx.send(Out::To(user, s.into()));
        }
    }

    /// An event for the connections of the listed users only.
    pub fn send_to_many(&self, users: &[UserId], ev: &ServerEvent) {
        if let Ok(s) = serde_json::to_string(ev) {
            let _ = self.tx.send(Out::Many(users.into(), s.into()));
        }
    }

    /// An event about channel `ch`, for everybody who may see it (v0.9).
    pub fn send_channel(&self, ch: ChannelId, ev: &ServerEvent) {
        if let Ok(s) = serde_json::to_string(ev) {
            let _ = self.tx.send(Out::Channel { ch, to: None, text: s.into() });
        }
    }

    /// An event about channel `ch` for the listed users, those of them who may see it (v0.9).
    pub fn send_channel_to(&self, ch: ChannelId, users: &[UserId], ev: &ServerEvent) {
        if let Ok(s) = serde_json::to_string(ev) {
            let _ = self.tx.send(Out::Channel { ch, to: Some(users.into()), text: s.into() });
        }
    }

    /// `VoiceStateChanged` of `member`, who was in `prev` before (v0.9: per recipient, see
    /// [`Out::Voice`]).
    fn send_voice(&self, member: VoiceMember, prev: Option<ChannelId>) {
        let ch = member.channel_id;
        let (Ok(full), Ok(gone)) = (
            serde_json::to_string(&ServerEvent::VoiceStateChanged { member: member.clone() }),
            serde_json::to_string(&ServerEvent::VoiceStateChanged { member: left(member.clone()) }),
        ) else {
            return;
        };
        let _ = self.tx.send(Out::Voice { member: member.user.id, ch, prev, full: full.into(), left: gone.into() });
    }

    /// Close the user's WebSockets (all of them, or all but session `keep`).
    pub fn kick(&self, user: UserId, keep: Option<&str>) {
        let _ = self.tx.send(Out::Kick { user, keep: keep.map(Into::into) });
    }

    /// Register a connection. Returns the `Ready` snapshot and an event receiver that starts
    /// right after it (snapshot and subscription are taken under one lock, so nothing is missed
    /// or duplicated).
    ///
    /// `presence` is what the person starts with (their stored presence and status); it is only
    /// used by their first connection. An invisible person is not announced and not listed for
    /// anybody else; every other person whose state is not plain "online, no status" is told to
    /// this connection right after `Ready` (as `PresenceChanged`, to this person's connections).
    pub fn connect(&self, user: &User, account: Account, presence: PresenceEntry) -> (ServerEvent, broadcast::Receiver<Out>) {
        let mut g = self.lock();
        let entry = g.online.entry(user.id).or_insert_with(|| (user.clone(), 0));
        entry.0 = user.clone();
        entry.1 += 1;
        if entry.1 == 1 {
            g.presence.insert(user.id, presence);
            if g.visible(user.id) {
                self.broadcast(&ServerEvent::UserOnline { user: user.clone() });
                let e = &g.presence[&user.id];
                if !e.is_default() {
                    self.broadcast(&e.event(user.id));
                }
            }
        }
        let rx = self.tx.subscribe();
        let mut online: Vec<User> = g.online.values().filter(|(u, _)| g.visible(u.id)).map(|(u, _)| u.clone()).collect();
        online.sort_by_key(|u| u.id);
        // v0.9: only the channels this person may see, and only the voice members of those (a
        // member in a private room they may not see is simply not in voice for them).
        let rules = self.access.rules();
        let channels: Vec<Channel> = self.channels().into_iter().filter(|c| rules.can_see(user.id, c.id)).collect();
        let mut voice: Vec<VoiceMember> = g.voice.values().filter(|m| m.user.id == user.id || m.channel_id.is_some_and(|c| rules.can_see(user.id, c))).cloned().collect();
        voice.sort_by_key(|m| m.user.id);
        // The presence snapshot: sorted, only the people it is news about (plus me, even when invisible).
        let mut known: Vec<(&UserId, &PresenceEntry)> =
            g.presence.iter().filter(|(id, e)| (g.visible(**id) || **id == user.id) && !e.is_default()).collect();
        known.sort_by_key(|(id, _)| **id);
        for (id, e) in known {
            self.send_to(user.id, &e.event(*id));
        }
        let ready = ServerEvent::Ready {
            user: user.clone(),
            channels,
            online,
            voice,
            account: Some(account),
            chat: Some(vox_proto::ChatFeatures::ALL),
            server_id: Some(self.server_id.clone()),
            stream_previews: true,
            // v1.1 (E0): the directory of device keys (`e2ee.rs`).
            e2ee: true,
        };
        (ready, rx)
    }

    /// Drop a connection; the user goes offline (and leaves voice) with the last one.
    pub fn disconnect(&self, user_id: UserId) {
        let mut g = self.lock();
        let Some(entry) = g.online.get_mut(&user_id) else { return };
        entry.1 -= 1;
        if entry.1 > 0 {
            return;
        }
        let was_visible = g.visible(user_id);
        g.online.remove(&user_id);
        g.presence.remove(&user_id);
        g.buckets.remove(&user_id);
        g.watch_buckets.remove(&user_id);
        g.voice_buckets.remove(&user_id);
        g.reaction_buckets.remove(&user_id);
        if let Some(m) = g.voice.remove(&user_id) {
            self.stream_previews.forget(user_id);
            let prev = m.channel_id;
            self.send_voice(left(m), prev);
            for v in g.drop_viewers_of(user_id) {
                let ch = v.channel_id;
                self.send_voice(v, ch);
            }
        }
        // An invisible person was already announced as gone.
        if was_visible {
            self.broadcast(&ServerEvent::UserOffline { user_id });
        }
    }

    /// How many WebSockets `user_id` has open.
    pub fn connections(&self, user_id: UserId) -> usize {
        self.lock().online.get(&user_id).map_or(0, |(_, n)| *n)
    }

    /// The user as everybody currently sees them (fresh role), falling back to `user`.
    pub fn current_user(&self, user: &User) -> User {
        self.lock().online.get(&user.id).map(|(u, _)| u.clone()).unwrap_or_else(|| user.clone())
    }

    /// Someone's role changed: update presence and voice, and tell everybody.
    pub fn update_user(&self, user: &User) {
        let mut g = self.lock();
        if let Some(e) = g.online.get_mut(&user.id) {
            e.0 = user.clone();
        }
        if let Some(m) = g.voice.get_mut(&user.id) {
            m.user = user.clone();
        }
        self.broadcast(&ServerEvent::UserUpdated { user: user.clone() });
    }

    // ---- profiles: presence and custom status

    /// The live presence and status of a connected person.
    pub fn presence_entry(&self, user_id: UserId) -> Option<PresenceEntry> {
        self.lock().presence.get(&user_id).cloned()
    }

    /// Tell the right people that `user_id`'s presence went from `before` to what it is now:
    /// everybody, except that going invisible is a plain `UserOffline` (coming back a `UserOnline`)
    /// so that clients that know nothing about presence see it too, and an invisible person's
    /// own connections are the only ones told about changes while they stay invisible.
    fn announce_presence(&self, g: &Inner, user_id: UserId, before: Presence) {
        let Some(e) = g.presence.get(&user_id) else { return };
        let after = e.effective();
        let ev = e.event(user_id);
        match (before.visible_to_others(), after.visible_to_others()) {
            (true, false) => {
                self.broadcast(&ServerEvent::UserOffline { user_id });
                self.send_to(user_id, &ev);
            }
            (false, true) => {
                if let Some((u, _)) = g.online.get(&user_id) {
                    self.broadcast(&ServerEvent::UserOnline { user: u.clone() });
                }
                self.broadcast(&ev);
            }
            (true, true) => self.broadcast(&ev),
            (false, false) => self.send_to(user_id, &ev),
        }
    }

    /// Change a connected person's presence. `auto` (the app's idle detection) only moves between
    /// `Online` and `Away` and never overrides a presence the person chose. Returns what the
    /// person is afterwards, or `None` when they are not connected.
    pub fn set_presence(&self, user_id: UserId, presence: Presence, auto: bool) -> Option<Presence> {
        let mut g = self.lock();
        let e = g.presence.get_mut(&user_id)?;
        let before = e.effective();
        if auto {
            if e.stored == Presence::Online {
                match presence {
                    Presence::Away => e.auto_away = true,
                    Presence::Online => e.auto_away = false,
                    _ => {}
                }
            }
        } else {
            e.stored = presence;
            e.auto_away = false;
        }
        let after = e.effective();
        if after != before {
            self.announce_presence(&g, user_id, before);
        }
        Some(after)
    }

    /// Set (or clear) a connected person's custom status and tell people (nobody, while invisible,
    /// except their own connections).
    pub fn set_status(&self, user_id: UserId, status: Option<CustomStatus>) {
        let mut g = self.lock();
        let Some(e) = g.presence.get_mut(&user_id) else { return };
        if e.status == status {
            return;
        }
        e.status = status;
        let before = e.effective();
        self.announce_presence(&g, user_id, before);
    }

    /// Clear the statuses of connected people whose time is up.
    pub fn expire_statuses(&self, now_ms: i64) {
        let mut g = self.lock();
        let due: Vec<UserId> = g.presence.iter().filter(|(_, e)| e.status.as_ref().is_some_and(|s| s.is_expired(now_ms))).map(|(id, _)| *id).collect();
        for id in due {
            if let Some(e) = g.presence.get_mut(&id) {
                e.status = None;
                let before = e.effective();
                self.announce_presence(&g, id, before);
            }
        }
    }

    pub fn voice_of(&self, user_id: UserId) -> Option<VoiceMember> {
        self.lock().voice.get(&user_id).cloned()
    }

    pub fn voice_members(&self) -> Vec<VoiceMember> {
        self.lock().voice.values().cloned().collect()
    }

    pub fn is_server_muted(&self, user_id: UserId) -> bool {
        self.lock().server_muted.contains(&user_id)
    }

    /// Server-mute or unmute (item 12). Fans the member out if they are in voice. Returns whether
    /// anything changed.
    pub fn set_server_muted(&self, user_id: UserId, muted: bool) -> bool {
        let mut g = self.lock();
        let changed = if muted { g.server_muted.insert(user_id) } else { g.server_muted.remove(&user_id) };
        if let Some(m) = g.voice.get_mut(&user_id) {
            if m.server_muted != muted {
                m.server_muted = muted;
                let member = m.clone();
                let ch = member.channel_id;
                self.send_voice(member, ch);
            }
        }
        changed
    }

    /// A moderator took the user out of voice (or their channel was deleted, or they lost access
    /// to it): forget their voice state as if they had left. Returns the state they had.
    pub fn force_leave(&self, user_id: UserId) -> Option<VoiceMember> {
        let mut g = self.lock();
        let prev = g.voice.remove(&user_id)?;
        self.stream_previews.forget(user_id);
        self.send_voice(left(prev.clone()), prev.channel_id);
        for v in g.drop_viewers_of(user_id) {
            let ch = v.channel_id;
            self.send_voice(v, ch);
        }
        Some(prev)
    }

    /// A moderator ended the user's Go Live: they are no longer streaming, nobody watches them.
    pub fn force_stop_stream(&self, user_id: UserId) {
        let mut g = self.lock();
        let Some(m) = g.voice.get_mut(&user_id) else { return };
        if !m.streaming {
            return;
        }
        m.streaming = false;
        m.audio_only = false;
        self.stream_previews.forget(user_id);
        let member = m.clone();
        let ch = member.channel_id;
        self.send_voice(member, ch);
        for v in g.drop_viewers_of(user_id) {
            let ch = v.channel_id;
            self.send_voice(v, ch);
        }
    }

    /// Would this voice state change nothing (same room, same flags)? Such a frame is dropped
    /// before the rate limit.
    #[allow(clippy::too_many_arguments)]
    pub fn voice_unchanged(&self, user: UserId, channel_id: Option<i64>, muted: bool, deafened: bool, streaming: bool, camera: bool, audio_only: bool) -> bool {
        let g = self.lock();
        match (g.voice.get(&user), channel_id) {
            (None, None) => true,
            (Some(m), Some(ch)) => {
                m.channel_id == Some(ch) && m.muted == muted && m.deafened == deafened && m.streaming == streaming && m.camera == camera && m.audio_only == (audio_only && streaming)
            }
            _ => false,
        }
    }

    /// Take one `VoiceState` token of `user`; `Err(wait)` until the next one.
    pub fn take_voice_token(&self, user: UserId) -> Result<(), std::time::Duration> {
        let now = Instant::now();
        let mut g = self.lock();
        let b = g.voice_buckets.entry(user).or_insert((VOICE_BURST, now));
        b.0 = (b.0 + now.duration_since(b.1).as_secs_f64() * VOICE_RATE_PER_SEC).min(VOICE_BURST);
        b.1 = now;
        if b.0 >= 1.0 {
            b.0 -= 1.0;
            Ok(())
        } else {
            Err(std::time::Duration::from_secs_f64((1.0 - b.0) / VOICE_RATE_PER_SEC))
        }
    }

    /// Apply a client's voice state. `Err` carries a message for the sender. Nothing is sent to
    /// anybody when nothing changed.
    #[allow(clippy::too_many_arguments)]
    pub fn set_voice(
        &self,
        user: &User,
        channel_id: Option<i64>,
        muted: bool,
        deafened: bool,
        streaming: bool,
        camera: bool,
        audio_only: bool,
    ) -> Result<(), &'static str> {
        if let Some(id) = channel_id {
            // A private channel without access is as unknown as a missing one (v0.9).
            if !matches!(self.channel_for(user.id, id), Some(c) if c.kind == ChannelKind::Voice) {
                return Err("unknown voice channel");
            }
        }
        let mut g = self.lock();
        let user = g.online.get(&user.id).map(|(u, _)| u.clone()).unwrap_or_else(|| user.clone());
        let prev = g.voice.get(&user.id).cloned();
        let server_muted = g.server_muted.contains(&user.id);
        // "Só áudio" only means something while streaming (an old client never sends it).
        let audio_only = audio_only && streaming;
        let member = match channel_id {
            // Mute / deafen / stream flips inside the same room keep who they are watching
            // (older clients never send that, and must not wipe it).
            Some(ch) => {
                let keep = prev.as_ref().filter(|p| p.channel_id == Some(ch));
                let watching = keep.map(|p| p.watching.clone()).unwrap_or_default();
                VoiceMember { user: user.clone(), channel_id, muted, deafened, streaming, watching, camera, server_muted, audio_only }
            }
            None => left(VoiceMember { user: user.clone(), channel_id: None, muted, deafened, streaming, watching: Vec::new(), camera, server_muted, audio_only }),
        };
        if channel_id.is_some() {
            if prev.as_ref() == Some(&member) {
                return Ok(()); // the same state again: nothing to tell anybody
            }
            g.voice.insert(user.id, member.clone());
        } else if g.voice.remove(&user.id).is_none() {
            return Ok(()); // was not in voice: nothing changed
        }
        // Nobody can keep watching someone who left, moved, or stopped streaming.
        let moved = prev.as_ref().is_some_and(|p| p.channel_id != member.channel_id);
        let viewers = if member.channel_id.is_some() && member.streaming && !moved { Vec::new() } else { g.drop_viewers_of(user.id) };
        // The hover picture of a live goes with it (ended, left, moved, or now sound only; v1.1).
        if !(member.channel_id.is_some() && member.streaming && !member.audio_only && !moved) {
            self.stream_previews.forget(user.id);
        }
        self.send_voice(member, prev.and_then(|p| p.channel_id));
        for v in viewers {
            let ch = v.channel_id;
            self.send_voice(v, ch);
        }
        Ok(())
    }

    /// Apply a client's `WatchState`: the set of streamers it watches. Anything that is not a
    /// live member of the watcher's own voice channel is ignored, as is the whole call when the
    /// watcher is not in voice. Fans the watcher's member out only when the set changed.
    pub fn set_watching(&self, user: &User, targets: &[UserId]) {
        let now = Instant::now();
        let mut g = self.lock();
        let b = g.watch_buckets.entry(user.id).or_insert((WATCH_BURST, now));
        b.0 = (b.0 + now.duration_since(b.1).as_secs_f64() * WATCH_RATE_PER_SEC).min(WATCH_BURST);
        b.1 = now;
        if b.0 < 1.0 {
            tracing::debug!(user = %user.username, "watch state ignored (rate limited)");
            return;
        }
        b.0 -= 1.0;
        let Some(me) = g.voice.get(&user.id) else { return };
        let Some(channel) = me.channel_id else { return };
        let mut valid: Vec<UserId> = Vec::new();
        for &t in targets {
            if t == user.id || valid.contains(&t) || valid.len() >= MAX_WATCHING {
                continue;
            }
            if g.voice.get(&t).is_some_and(|s| s.streaming && s.channel_id == Some(channel)) {
                valid.push(t);
            }
        }
        let Some(me) = g.voice.get_mut(&user.id) else { return };
        if me.watching == valid {
            return;
        }
        me.watching = valid;
        let member = me.clone();
        let ch = member.channel_id;
        self.send_voice(member, ch);
    }

    /// Token bucket: ~5 messages/second per user (across all their connections).
    pub fn allow_message(&self, user_id: UserId) -> bool {
        take_token(&mut self.lock().buckets, user_id, MSG_RATE_PER_SEC, MSG_BURST)
    }

    /// Token bucket for reactions (channels and DMs together): ~4 a second, bursts of 12.
    pub fn allow_reaction(&self, user_id: UserId) -> bool {
        take_token(&mut self.lock().reaction_buckets, user_id, REACTION_RATE_PER_SEC, REACTION_BURST)
    }
}

fn take_token(buckets: &mut HashMap<UserId, (f64, Instant)>, user_id: UserId, rate: f64, burst: f64) -> bool {
    let now = Instant::now();
    let b = buckets.entry(user_id).or_insert((burst, now));
    let refill = now.duration_since(b.1).as_secs_f64() * rate;
    b.0 = (b.0 + refill).min(burst);
    b.1 = now;
    if b.0 >= 1.0 {
        b.0 -= 1.0;
        true
    } else {
        false
    }
}

/// Resolves when shutdown is signalled (or the sender is gone).
pub async fn wait_shutdown(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() || rx.changed().await.is_err() {
            return;
        }
    }
}
