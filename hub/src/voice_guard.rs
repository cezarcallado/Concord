//! Keeps LiveKit's rooms honest: the voice list everybody sees (and every voice moderation
//! action) must be about the people really in the rooms, not only what their apps declared.
//!
//! The hub's voice list comes from each app's own `VoiceState`. A modified app could join a
//! LiveKit room with an ordinary join token and never declare it (or declare another room): it
//! would hear and be heard without being listed, and moderation could not reach it. So, every
//! [`Tuning::voice_check_every`](crate::Tuning):
//!
//! - **Unlisted participants are removed.** Somebody in `voice-<id>` whom the hub does not list
//!   in that channel, or in `dm-<lo>-<hi>` without a live call of that pair, is removed from the
//!   room once they have been unlisted for [`Tuning::voice_grace`](crate::Tuning) (an app
//!   declares its room a moment after joining; a WebSocket that reconnects re-declares it).
//! - **Server mute is applied to the real participant.** A server-muted member whose LiveKit
//!   permission still allows the microphone (an old token, a room the hub did not know about)
//!   loses it.
//! - **Old tokens stop working after a removal.** A moderator's disconnect / move and a password
//!   reset [`revoke`] the person: a session that joins after that with a token the hub issued
//!   *before* it is removed at the next check, without the grace. (Join tokens also only work for
//!   a minute, see `livekit_token::TOKEN_TTL_SECS`.)
//! - **Private channels (v0.9).** Somebody in a private channel's room who may not see that
//!   channel (a token from before they lost access) is removed at the next check, without the
//!   grace. (A change of access already removes them at once, `access::enforce`; this catches a
//!   rejoin with the old token.) The hub never gives a token for a room the person may not see.
//! - **Closer watch after a removal (v1.0.1).** For as long as a token from before a removal could
//!   still be used ([`livekit_token::TOKEN_USABLE_SECS`](crate::livekit_token::TOKEN_USABLE_SECS)),
//!   the check runs every [`Tuning::voice_check_fast`](crate::Tuning) instead, so each rejoin
//!   with the old token lasts about a second, and after that the token is useless.
//!
//! Moderation asks [`rooms_of`] where somebody really is, so mute / disconnect / move work on an
//! unlisted participant too.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;
use std::time::Instant;

use vox_proto::{ChannelId, UserId};

use crate::livekit_admin::LkError;
use crate::state::AppState;
use crate::util::now_ms;

/// What kind of room a LiveKit room name is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomKind {
    Voice(ChannelId),
    Dm(UserId, UserId),
}

impl RoomKind {
    /// `voice-<id>` or `dm-<lo>-<hi>`; anything else is not ours and is left alone.
    pub fn parse(room: &str) -> Option<Self> {
        if let Some(id) = room.strip_prefix("voice-") {
            return id.parse().ok().map(RoomKind::Voice);
        }
        let (lo, hi) = room.strip_prefix("dm-")?.split_once('-')?;
        Some(RoomKind::Dm(lo.parse().ok()?, hi.parse().ok()?))
    }
}

pub fn voice_room(channel: ChannelId) -> String {
    format!("voice-{channel}")
}

#[derive(Default)]
pub struct VoiceGuard {
    inner: Mutex<Inner>,
    /// Wakes [`watch`] when the pace changes ([`watch_closely`]).
    wake: tokio::sync::Notify,
}

#[derive(Default)]
struct Inner {
    /// (user, room) -> when the person was first seen there without being listed.
    unlisted: HashMap<(UserId, String), Instant>,
    /// user -> when they were last revoked (ms).
    revoked_at: HashMap<UserId, i64>,
    /// user -> when the hub last gave them a join token (ms).
    issued_at: HashMap<UserId, i64>,
    /// v1.1 (E1): (user, voice channel) -> whether the last join token for that room was for an
    /// end-to-end encrypted room.
    issued_e2ee: HashMap<(UserId, ChannelId), bool>,
    /// Check every `voice_check_fast` until then ([`watch_closely`]).
    close_until: Option<Instant>,
}

impl VoiceGuard {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The hub just gave `user` a join token.
    pub fn issued(&self, user: UserId) {
        self.lock().issued_at.insert(user, now_ms());
    }

    /// v1.1 (E1): the hub just gave `user` a join token for voice channel `channel`'s room, for an
    /// end-to-end encrypted room or not (`e2ee`).
    pub fn issued_voice(&self, user: UserId, channel: ChannelId, e2ee: bool) {
        let mut g = self.lock();
        g.issued_at.insert(user, now_ms());
        g.issued_e2ee.insert((user, channel), e2ee);
    }

    /// v1.1 (E1): is `user`'s last token for `channel` from the other encryption mode than the
    /// room's now (`e2ee`)? The flag only changes in an empty room, so somebody in it with such a
    /// token joined in the moment the flag changed: an app that encrypts in a plain room (noise for
    /// everybody) or one that sends in the clear in an encrypted room. Unknown (a hub restarted
    /// since): no.
    fn wrong_mode(&self, user: UserId, channel: ChannelId, e2ee: bool) -> bool {
        self.lock().issued_e2ee.get(&(user, channel)).is_some_and(|m| *m != e2ee)
    }

    /// Would a participant session of `user` that joined at `joined_at` (unix seconds) be using a
    /// token from before their last revocation?
    fn stale_token(&self, user: UserId, joined_at: i64) -> bool {
        let g = self.lock();
        let Some(&revoked) = g.revoked_at.get(&user) else { return false };
        // LiveKit gives seconds: a session that joined in the revocation's second may be after it.
        let joined_after = joined_at.saturating_mul(1000).saturating_add(999) >= revoked;
        let token_before = g.issued_at.get(&user).is_none_or(|&issued| issued < revoked);
        joined_after && token_before
    }
}

/// `user` was taken out of voice (disconnect, move) or lost their sessions (password reset): a
/// participant session that joins after this with an older token is removed at once.
pub fn revoke(s: &AppState, user: UserId) {
    s.voice_guard.lock().revoked_at.insert(user, now_ms());
    watch_closely(s);
}

/// Somebody just lost a room (or access to one): check every `voice_check_fast` for as long as
/// a join token issued before now could still be used, so a rejoin with it is cut short.
pub fn watch_closely(s: &AppState) {
    let until = Instant::now() + std::time::Duration::from_secs(crate::livekit_token::TOKEN_USABLE_SECS as u64 + 5);
    {
        let mut g = s.voice_guard.lock();
        g.close_until = Some(g.close_until.map_or(until, |u| u.max(until)));
    }
    s.voice_guard.wake.notify_one();
}

/// How long until the next check.
fn next_check(s: &AppState) -> std::time::Duration {
    let mut g = s.voice_guard.lock();
    match g.close_until {
        Some(u) if Instant::now() < u => s.tuning.voice_check_fast.min(s.tuning.voice_check_every),
        Some(_) => {
            g.close_until = None;
            s.tuning.voice_check_every
        }
        None => s.tuning.voice_check_every,
    }
}

/// Is `user` allowed in this room by the hub's own state?
fn listed(s: &AppState, kind: RoomKind, user: UserId) -> bool {
    match kind {
        RoomKind::Voice(ch) => s.voice_of(user).and_then(|m| m.channel_id) == Some(ch),
        RoomKind::Dm(lo, hi) => crate::calls::in_call_room(s, user, lo, hi),
    }
}

/// Where `user` really is in LiveKit right now (room names; empty without LiveKit).
pub async fn rooms_of(s: &AppState, user: UserId) -> Vec<String> {
    let rooms = match s.livekit.list_rooms().await {
        Ok(r) => r,
        Err(LkError::Disabled) => return Vec::new(),
        Err(e) => {
            tracing::warn!(error = %e, "livekit: listing rooms failed");
            return Vec::new();
        }
    };
    let me = user.to_string();
    let mut out = Vec::new();
    for room in rooms.into_iter().filter(|r| RoomKind::parse(r).is_some()) {
        if let Ok(list) = s.livekit.list_participants(&room).await {
            if list.iter().any(|p| p.identity == me) {
                out.push(room);
            }
        }
    }
    out
}

/// Take `user` out of every room of LiveKit (best effort).
pub async fn remove_everywhere(s: &AppState, user: UserId) {
    for room in rooms_of(s, user).await {
        match s.livekit.remove(&room, &user.to_string()).await {
            Ok(()) | Err(LkError::NotFound) | Err(LkError::Disabled) => {}
            Err(LkError::Other(e)) => tracing::warn!(room, error = %e, "livekit: removing a participant failed"),
        }
    }
}

enum Act {
    Remove { room: String, user: UserId, why: &'static str },
    TakeMic { room: String, user: UserId },
}

/// One comparison of LiveKit's rooms with the hub's state, and the fixes.
pub async fn check(s: &AppState) {
    let rooms = match s.livekit.list_rooms().await {
        Ok(r) => r,
        Err(LkError::Disabled) => return,
        Err(e) => {
            tracing::debug!(error = %e, "voice check: listing rooms failed");
            return;
        }
    };
    let now = Instant::now();
    let mut acts = Vec::new();
    let mut seen: BTreeSet<(UserId, String)> = BTreeSet::new();
    for room in rooms {
        let Some(kind) = RoomKind::parse(&room) else { continue };
        let participants = match s.livekit.list_participants(&room).await {
            Ok(p) => p,
            Err(_) => continue,
        };
        for p in participants {
            // Identities are user ids; anything else did not come from this hub.
            let Ok(user) = p.identity.parse::<UserId>() else { continue };
            let key = (user, room.clone());
            seen.insert(key.clone());
            if s.voice_guard.stale_token(user, p.joined_at) {
                acts.push(Act::Remove { room: room.clone(), user, why: "joined with a token from before a removal" });
                continue;
            }
            // v0.9: a private room without access (a token from before they lost it): out at once.
            if let RoomKind::Voice(ch) = kind {
                if !s.can_see(user, ch) {
                    acts.push(Act::Remove { room: room.clone(), user, why: "no access to this private channel" });
                    continue;
                }
                // v1.1 (E1): a token from before the room's end-to-end encryption changed.
                if s.channel(ch).is_some_and(|c| s.voice_guard.wrong_mode(user, ch, c.e2ee)) {
                    acts.push(Act::Remove { room: room.clone(), user, why: "joined with a token from before the room's encryption changed" });
                    continue;
                }
            }
            if listed(s, kind, user) {
                s.voice_guard.lock().unlisted.remove(&key);
            } else {
                let first = *s.voice_guard.lock().unlisted.entry(key).or_insert(now);
                if now.duration_since(first) >= s.tuning.voice_grace {
                    acts.push(Act::Remove { room: room.clone(), user, why: "in the room without being in the hub's voice list" });
                    continue;
                }
            }
            if p.can_publish_mic && s.is_server_muted(user) {
                acts.push(Act::TakeMic { room: room.clone(), user });
            }
        }
    }
    s.voice_guard.lock().unlisted.retain(|k, _| seen.contains(k));
    for act in acts {
        match act {
            Act::Remove { room, user, why } => {
                tracing::warn!(room, user, why, "voice check: participant removed");
                s.voice_guard.lock().unlisted.remove(&(user, room.clone()));
                if let Err(LkError::Other(e)) = s.livekit.remove(&room, &user.to_string()).await {
                    tracing::warn!(room, error = %e, "voice check: removing failed");
                }
            }
            Act::TakeMic { room, user } => {
                tracing::warn!(room, user, "voice check: server-muted participant could still publish a microphone");
                if let Err(LkError::Other(e)) = crate::admin::sync_mic_permission(s, user, &room).await {
                    tracing::warn!(room, error = %e, "voice check: server mute failed");
                }
            }
        }
    }
}

/// Background task: [`check`] every `voice_check_every` until shutdown.
pub async fn watch(s: AppState, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    if !s.livekit.enabled() {
        return;
    }
    loop {
        tokio::select! {
            _ = tokio::time::sleep(next_check(&s)) => check(&s).await,
            // A removal (already done by whoever called): from now on, the closer pace.
            _ = s.voice_guard.wake.notified() => {}
            _ = crate::state::wait_shutdown(&mut shutdown) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_names() {
        assert_eq!(RoomKind::parse("voice-12"), Some(RoomKind::Voice(12)));
        assert_eq!(RoomKind::parse("dm-3-9"), Some(RoomKind::Dm(3, 9)));
        assert_eq!(RoomKind::parse("voice-x"), None);
        assert_eq!(RoomKind::parse("dm-3"), None);
        assert_eq!(RoomKind::parse("other"), None);
        assert_eq!(voice_room(4), "voice-4");
    }

    #[test]
    fn tokens_from_before_a_revocation_are_stale() {
        let g = VoiceGuard::default();
        assert!(!g.stale_token(1, 0), "never revoked");
        g.lock().revoked_at.insert(1, 10_000);
        g.lock().issued_at.insert(1, 9_000);
        assert!(g.stale_token(1, 10), "joined after the revocation with an older token");
        assert!(!g.stale_token(1, 8), "the session from before (removed by the revocation itself)");
        g.lock().issued_at.insert(1, 10_500);
        assert!(!g.stale_token(1, 11), "a token issued after the revocation is fine");
        assert!(!g.stale_token(2, 11), "somebody else");
    }

    #[test]
    fn tokens_from_the_other_encryption_mode_are_wrong() {
        let g = VoiceGuard::default();
        assert!(!g.wrong_mode(1, 5, true), "unknown (a restarted hub): left alone");
        g.issued_voice(1, 5, false);
        assert!(g.wrong_mode(1, 5, true), "a plain token in a room that is encrypted now");
        assert!(!g.wrong_mode(1, 5, false));
        assert!(!g.wrong_mode(1, 6, true), "another room");
        g.issued_voice(1, 5, true);
        assert!(!g.wrong_mode(1, 5, true), "a fresh token of the right mode");
        assert!(g.wrong_mode(1, 5, false));
    }
}
