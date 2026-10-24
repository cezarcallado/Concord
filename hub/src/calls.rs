//! Private 1:1 calls from a DM ("Ligar"). The wire contract is in `vox_proto::dm`.
//!
//! A call lives only in memory (a hub restart drops it; the apps then leave the room). The hub
//! creates nothing at LiveKit: it hands out tokens for the private room `dm-<lo>-<hi>`, and only
//! to the two people of the conversation (every endpoint goes through `dms::conversation_of`, which
//! is `404` for anybody else, any role). A call never enters the voice state that is broadcast, so
//! nobody else sees who is in one.
//!
//! Life of a call: `POST /call` (ringing; `CallInvite` to both, the callee's app rings) ->
//! `accept` (`CallAccepted`, both join) -> `end` (`CallEnded`, a "call ended" line with the
//! duration). Unanswered: the caller's `end` (cancelled), the callee's `decline` (`CallDeclined`),
//! or [`ring_timeout`] (missed) close it with a missed / declined line. Somebody who joins a voice
//! channel leaves their call; somebody who stays offline for [`OFFLINE_GRACE`] too. When a call
//! ends the hub also removes both from the room (best effort) so nobody lingers in it.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use vox_proto::*;

use crate::api::{ApiErr, AuthUser, E2eeQuery, LkUrl};
use crate::dms::{self, ConvRow};
use crate::livekit_admin::LkError;
use crate::livekit_token;
use crate::state::AppState;
use crate::util::now_ms;

type ApiResult<T> = Result<Json<T>, ApiErr>;

/// Somebody in a call whose last connection closed gets this long to come back.
pub const OFFLINE_GRACE: Duration = Duration::from_secs(20);
/// How long after the end the hub removes whoever is still in the room (the apps leave by
/// themselves first).
const ROOM_CLEANUP_DELAY: Duration = Duration::from_millis(1500);

static RING_MS: AtomicU64 = AtomicU64::new(CALL_RING_SECS * 1000);

/// How long a call rings before it is missed ([`CALL_RING_SECS`]; tests shorten it).
pub fn ring_timeout() -> Duration {
    Duration::from_millis(RING_MS.load(Ordering::Relaxed))
}

/// Tests only: ring for `d` instead of [`CALL_RING_SECS`] (process-wide).
pub fn set_ring_timeout(d: Duration) {
    RING_MS.store(d.as_millis() as u64, Ordering::Relaxed);
}

#[derive(Default)]
pub struct CallsState {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    next_id: CallId,
    /// Ringing and answered calls.
    calls: HashMap<CallId, Live>,
    /// When each person started their recent calls (rate limit).
    starts: HashMap<UserId, VecDeque<Instant>>,
    /// Open WebSocket connections per person (independent of presence: an invisible person is
    /// still connected).
    conns: HashMap<UserId, usize>,
}

#[derive(Clone)]
struct Live {
    call: DmCall,
    conv: ConvRow,
    answered: Option<Instant>,
    /// People who called one of this call's two while they were in it ("busy"): each gets one
    /// missed-call line per call, however often they retry.
    busy_noted: Vec<UserId>,
}

impl Inner {
    fn of_user(&self, user: UserId) -> Option<&Live> {
        self.calls.values().find(|l| l.call.caller.id == user || l.call.callee.id == user)
    }

    fn of_dm(&self, dm: DmId) -> Option<&Live> {
        self.calls.values().find(|l| l.call.dm_id == dm)
    }

    fn allow_start(&mut self, user: UserId, now: Instant) -> bool {
        let window = Duration::from_secs(CALL_RATE_WINDOW_SECS);
        let q = self.starts.entry(user).or_default();
        while q.front().is_some_and(|t| now.saturating_duration_since(*t) >= window) {
            q.pop_front();
        }
        if q.len() >= CALL_RATE_LIMIT {
            return false;
        }
        q.push_back(now);
        true
    }
}

impl CallsState {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The call `user` is in (ringing or answered), if any.
    pub fn call_of(&self, user: UserId) -> Option<DmCall> {
        self.lock().of_user(user).map(|l| l.call.clone())
    }

    /// May `user` be in room `dm-<lo>-<hi>`: a live call of that pair, and they are one of it.
    pub fn allows(&self, user: UserId, lo: UserId, hi: UserId) -> bool {
        (user == lo || user == hi) && self.lock().calls.values().any(|l| l.conv.lo == lo && l.conv.hi == hi)
    }

    /// Take the call out if it is still the one we mean (`id`); the one taking it out ends it.
    fn take(&self, id: CallId) -> Option<Live> {
        self.lock().calls.remove(&id)
    }
}

/// Is `user` allowed in the LiveKit room of the pair (`voice_guard.rs`)?
pub fn in_call_room(s: &AppState, user: UserId, lo: UserId, hi: UserId) -> bool {
    s.calls.allows(user, lo, hi)
}

/// The LiveKit room of `user`'s call, if they are in one.
pub fn room_of(s: &AppState, user: UserId) -> Option<String> {
    s.calls.lock().of_user(user).map(|l| dm_room(l.conv.lo, l.conv.hi))
}

// ---------------------------------------------------------------- tokens

fn join_response(s: &AppState, me: &User, conv: &ConvRow, lk_url: &str, e2ee: bool) -> VoiceJoinResponse {
    let room = dm_room(conv.lo, conv.hi);
    let lk = &s.cfg.livekit;
    // A server mute holds in private calls too: no microphone in the token.
    let muted = s.is_server_muted(me.id);
    let token = livekit_token::join_token_with(&lk.api_key, &lk.api_secret, &room, &me.id.to_string(), &me.username, now_ms() / 1000, muted);
    s.voice_guard.issued(me.id);
    VoiceJoinResponse { livekit_url: lk_url.to_string(), livekit_token: token, room, e2ee }
}

/// A token for `live`'s room. v1.1 (E1): an end-to-end encrypted call only to an app that
/// encrypts (`capable`: it asked with `?e2ee=1`); an older one gets `409` with a text it shows.
fn answer(s: &AppState, me: &User, live: &Live, lk: &str, capable: bool) -> ApiResult<DmCallResponse> {
    if live.call.e2ee && !capable {
        tracing::info!(call = live.call.id, user = %me.username, "dm call token refused: the call is end-to-end encrypted and the app cannot encrypt");
        return Err(conflict(ERR_E2EE_CALL_REQUIRED));
    }
    Ok(Json(DmCallResponse { call: live.call.clone(), join: join_response(s, me, &live.conv, lk, live.call.e2ee) }))
}

/// Does `user` have a device with encryption keys (E0's directory)?
async fn has_e2ee_device(s: &AppState, user: UserId) -> Result<bool, ApiErr> {
    Ok(!s.db.call(move |c| crate::e2ee::list_user(c, user)).await?.is_empty())
}

fn conflict(msg: &str) -> ApiErr {
    ApiErr::new(StatusCode::CONFLICT, msg)
}

// ---------------------------------------------------------------- ending

/// How a call is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Close {
    Ended(CallEndReason),
    Declined,
    /// v1.0.1: this person left for a voice channel the other one may not see. They get
    /// `JoinedVoice`, the other one a plain `Hangup`, so a private channel's existence and who is
    /// in it do not leak (audit `voxhub:hub:dm-call-end-reveals-private-voice-join`).
    JoinedHidden(UserId),
}

/// Close a call that was just taken out of the table: the line in the conversation, the event to
/// both, and the room cleanup.
async fn close(s: &AppState, live: Live, how: Close) {
    let call = &live.call;
    let (kind, secs) = match (how, live.answered) {
        (Close::Declined, _) => (DmMessageKind::CallDeclined, 0),
        (_, Some(at)) => (DmMessageKind::CallEnded, at.elapsed().as_secs().clamp(1, u32::MAX as u64) as u32),
        (_, None) => (DmMessageKind::CallMissed, 0),
    };
    tracing::info!(call = call.id, dm = call.dm_id, caller = %call.caller.username, callee = %call.callee.username, ?how, ?kind, secs, "dm call closed");
    if let Err(e) = dms::add_line(s, &live.conv, call.caller.id, kind, String::new(), secs).await {
        tracing::error!("writing the call line failed: {}", e.1);
    }
    match how {
        Close::Declined => dms::tell_both(s, &live.conv, &ServerEvent::CallDeclined { dm_id: call.dm_id, call_id: call.id }),
        Close::Ended(reason) => dms::tell_both(s, &live.conv, &ServerEvent::CallEnded { dm_id: call.dm_id, call_id: call.id, reason }),
        Close::JoinedHidden(by) => {
            s.send_to(by, &ServerEvent::CallEnded { dm_id: call.dm_id, call_id: call.id, reason: CallEndReason::JoinedVoice });
            s.send_to(live.conv.peer_of(by), &ServerEvent::CallEnded { dm_id: call.dm_id, call_id: call.id, reason: CallEndReason::Hangup });
        }
    }
    if live.answered.is_some() {
        let (s2, room) = (s.clone(), dm_room(live.conv.lo, live.conv.hi));
        let who = [call.caller.id, call.callee.id];
        tokio::spawn(async move {
            tokio::time::sleep(ROOM_CLEANUP_DELAY).await;
            for id in who {
                match s2.livekit.remove(&room, &id.to_string()).await {
                    Ok(()) | Err(LkError::NotFound) | Err(LkError::Disabled) => {}
                    Err(LkError::Other(e)) => tracing::warn!(room, error = %e, "removing a dm call participant failed"),
                }
            }
        });
    }
}

/// End whatever call `user` is in (`reason` from their side).
async fn end_for(s: &AppState, user: UserId, reason: CallEndReason) {
    let Some(id) = s.calls.lock().of_user(user).map(|l| l.call.id) else { return };
    if let Some(live) = s.calls.take(id) {
        close(s, live, Close::Ended(reason)).await;
    }
}

/// The call between `a` and `b`, if there is one, ends (one of them blocked the other).
pub async fn end_between(s: &AppState, a: UserId, b: UserId) {
    let id = s.calls.lock().calls.values().find(|l| l.conv.has(a) && l.conv.has(b)).map(|l| l.call.id);
    if let Some(live) = id.and_then(|id| s.calls.take(id)) {
        close(s, live, Close::Ended(CallEndReason::Hangup)).await;
    }
}

/// The call rings out unless it was answered (or ended) by then.
fn arm_ring_timeout(s: &AppState, id: CallId) {
    let s = s.clone();
    tokio::spawn(async move {
        tokio::time::sleep(ring_timeout()).await;
        let still_ringing = s.calls.lock().calls.get(&id).is_some_and(|l| l.answered.is_none());
        if still_ringing {
            if let Some(live) = s.calls.take(id) {
                close(&s, live, Close::Ended(CallEndReason::Missed)).await;
            }
        }
    });
}

// ---------------------------------------------------------------- hooks (ws.rs)

/// A connection of `user` opened: count it, and ring again if a call to them is ringing (they
/// connected during the ring, or reconnected).
pub fn on_connect(s: &AppState, user: UserId) {
    let ringing: Vec<DmCall> = {
        let mut g = s.calls.lock();
        *g.conns.entry(user).or_default() += 1;
        g.calls.values().filter(|l| l.answered.is_none() && (l.call.callee.id == user || l.call.caller.id == user)).map(|l| l.call.clone()).collect()
    };
    for call in ringing {
        s.send_to(user, &ServerEvent::CallInvite { call });
    }
}

/// A connection of `user` closed. With none left and a call going on, they get
/// [`OFFLINE_GRACE`] to come back before the call ends.
pub fn on_disconnect(s: &AppState, user: UserId) {
    let in_call = {
        let mut g = s.calls.lock();
        let n = g.conns.get(&user).copied().unwrap_or(0).saturating_sub(1);
        if n == 0 {
            g.conns.remove(&user);
        } else {
            g.conns.insert(user, n);
        }
        if n > 0 {
            return;
        }
        g.of_user(user).map(|l| l.call.id)
    };
    let Some(id) = in_call else { return };
    let s = s.clone();
    tokio::spawn(async move {
        tokio::time::sleep(OFFLINE_GRACE).await;
        let gone = {
            let g = s.calls.lock();
            !g.conns.contains_key(&user) && g.calls.contains_key(&id)
        };
        if gone {
            if let Some(live) = s.calls.take(id) {
                close(&s, live, Close::Ended(CallEndReason::Offline)).await;
            }
        }
    });
}

/// `user` joined the voice channel `channel`: as on Discord, that leaves their DM call. The
/// other person hears why only when they may see that channel (v1.0.1); otherwise it is a plain
/// hang-up for them.
pub async fn on_voice_join(s: &AppState, user: UserId, channel: ChannelId) {
    let Some((id, peer)) = s.calls.lock().of_user(user).map(|l| (l.call.id, l.conv.peer_of(user))) else { return };
    let how = if s.can_see(peer, channel) { Close::Ended(CallEndReason::JoinedVoice) } else { Close::JoinedHidden(user) };
    if let Some(live) = s.calls.take(id) {
        close(s, live, how).await;
    }
}

// ---------------------------------------------------------------- handlers

/// `POST /api/dms/{id}/call`: ring the other person, or join this DM's call if there is one.
pub async fn call(State(s): State<AppState>, AuthUser(me): AuthUser, Path(id): Path<DmId>, Query(q): Query<E2eeQuery>, LkUrl(lk): LkUrl) -> ApiResult<DmCallResponse> {
    let capable = q.capable();
    let conv = dms::conversation_of(&s, me.id, id).await?;
    let peer_id = conv.peer_of(me.id);
    let (a, b) = (me.id, peer_id);
    if s.db.call(move |c| dms::blocked_between(c, a, b)).await? {
        return Err(dms::blocked_err());
    }
    // This DM already has a call: the callee answers it, anybody of the two rejoins it.
    let existing = s.calls.lock().of_dm(id).cloned();
    if let Some(live) = existing {
        if live.answered.is_none() && live.call.callee.id == me.id {
            return accept_live(&s, &me, live.call.id, &lk, capable);
        }
        return answer(&s, &me, &live, &lk, capable);
    }
    // A call elsewhere: mine ends (I am calling someone else now); theirs means busy.
    end_for(&s, me.id, CallEndReason::Hangup).await;
    let owner = s.owner();
    let peer = s
        .db
        .call(move |c| crate::db::get_user(c, peer_id))
        .await?
        .map(|r| r.user(&owner))
        .ok_or_else(|| ApiErr::new(StatusCode::NOT_FOUND, "user not found"))?;
    let me_now = s.current_user(&me);
    // v1.1 (E1): end-to-end encrypted when both ends can: this app said so, and the callee has a
    // device with keys (whichever of their computers answers must then be 1.1).
    let e2ee = capable && has_e2ee_device(&s, peer_id).await?;
    // `Err(first)`: busy (`first`: the first time I hit this busy call).
    let live: Result<Live, bool> = {
        let mut g = s.calls.lock();
        // Every attempt counts against the call budget, a busy one too: retrying a busy person
        // cannot write lines and events without limit.
        if !g.allow_start(me.id, Instant::now()) {
            return Err(ApiErr::new(StatusCode::TOO_MANY_REQUESTS, "too many calls, wait a little"));
        }
        let busy_id = g.of_user(peer_id).map(|l| l.call.id);
        if let Some(busy) = busy_id.and_then(|cid| g.calls.get_mut(&cid)) {
            let first = !busy.busy_noted.contains(&me.id);
            if first {
                busy.busy_noted.push(me.id);
            }
            Err(first)
        } else {
            g.next_id += 1;
            let call = DmCall { id: g.next_id, dm_id: id, caller: me_now, callee: peer, state: CallState::Ringing, started_at_ms: now_ms(), e2ee };
            let live = Live { call, conv: conv.clone(), answered: None, busy_noted: Vec::new() };
            g.calls.insert(live.call.id, live.clone());
            Ok(live)
        }
    };
    let live = match live {
        Ok(l) => l,
        Err(first) => {
            // They are in another call: they see a missed call from me later (once per call).
            tracing::info!(caller = %me.username, dm = id, first, "dm call: the other person is busy");
            if first {
                let _ = dms::add_line(&s, &conv, me.id, DmMessageKind::CallMissed, String::new(), 0).await;
            }
            return Err(conflict(ERR_CALL_BUSY));
        }
    };
    tracing::info!(call = live.call.id, dm = id, caller = %me.username, callee = %live.call.callee.username, e2ee, "dm call ringing");
    dms::tell_both(&s, &conv, &ServerEvent::CallInvite { call: live.call.clone() });
    arm_ring_timeout(&s, live.call.id);
    answer(&s, &me, &live, &lk, capable)
}

/// The callee answers call `id`.
fn accept_live(s: &AppState, me: &User, id: CallId, lk: &str, capable: bool) -> ApiResult<DmCallResponse> {
    let live = {
        let mut g = s.calls.lock();
        let Some(live) = g.calls.get_mut(&id) else { return Err(conflict("no call")) };
        if live.call.e2ee && !capable {
            // Not answered: the call keeps ringing for the callee's other (1.1) computers.
            tracing::info!(call = id, callee = %me.username, "dm call accept refused: the call is end-to-end encrypted and the app cannot encrypt");
            return Err(conflict(ERR_E2EE_CALL_REQUIRED));
        }
        if live.answered.is_none() {
            live.answered = Some(Instant::now());
            live.call.state = CallState::Active;
        }
        live.clone()
    };
    tracing::info!(call = id, dm = live.call.dm_id, callee = %me.username, e2ee = live.call.e2ee, "dm call answered");
    dms::tell_both(s, &live.conv, &ServerEvent::CallAccepted { call: live.call.clone() });
    answer(s, me, &live, lk, capable)
}

/// `POST /api/dms/{id}/call/accept`
pub async fn accept(State(s): State<AppState>, AuthUser(me): AuthUser, Path(id): Path<DmId>, Query(q): Query<E2eeQuery>, LkUrl(lk): LkUrl) -> ApiResult<DmCallResponse> {
    let capable = q.capable();
    dms::conversation_of(&s, me.id, id).await?;
    let Some(live) = s.calls.lock().of_dm(id).cloned() else { return Err(conflict("no call")) };
    if live.answered.is_none() {
        if live.call.callee.id != me.id {
            return Err(conflict("only the person being called can answer"));
        }
        return accept_live(&s, &me, live.call.id, &lk, capable);
    }
    answer(&s, &me, &live, &lk, capable)
}

/// `POST /api/dms/{id}/call/decline`
pub async fn decline(State(s): State<AppState>, AuthUser(me): AuthUser, Path(id): Path<DmId>) -> ApiResult<OkResponse> {
    dms::conversation_of(&s, me.id, id).await?;
    let found = s.calls.lock().of_dm(id).map(|l| (l.call.id, l.answered.is_none() && l.call.callee.id == me.id));
    match found {
        Some((call_id, true)) => {
            if let Some(live) = s.calls.take(call_id) {
                close(&s, live, Close::Declined).await;
            }
            Ok(Json(OkResponse { ok: true }))
        }
        Some(_) => Err(conflict("only a ringing call can be declined, by the person being called")),
        None => Err(conflict("no call")),
    }
}

/// `POST /api/dms/{id}/call/end`: hang up (or, before an answer, give up / decline).
pub async fn end(State(s): State<AppState>, AuthUser(me): AuthUser, Path(id): Path<DmId>) -> ApiResult<OkResponse> {
    dms::conversation_of(&s, me.id, id).await?;
    let Some((call_id, ringing, caller)) = s.calls.lock().of_dm(id).map(|l| (l.call.id, l.answered.is_none(), l.call.caller.id)) else {
        // Already over (the other side ended it first): nothing to do.
        return Ok(Json(OkResponse { ok: true }));
    };
    let how = match (ringing, caller == me.id) {
        (true, true) => Close::Ended(CallEndReason::Cancelled),
        (true, false) => Close::Declined,
        (false, _) => Close::Ended(CallEndReason::Hangup),
    };
    if let Some(live) = s.calls.take(call_id) {
        close(&s, live, how).await;
    }
    Ok(Json(OkResponse { ok: true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_starts_are_rate_limited_per_person() {
        let mut g = Inner::default();
        let t = Instant::now();
        for _ in 0..CALL_RATE_LIMIT {
            assert!(g.allow_start(1, t));
        }
        assert!(!g.allow_start(1, t));
        assert!(g.allow_start(2, t), "somebody else is not affected");
        assert!(g.allow_start(1, t + Duration::from_secs(CALL_RATE_WINDOW_SECS)));
    }
}
