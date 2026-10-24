//! Item 12: personal passwords, moderation, roles, channels and the audit log (HTTP handlers).
//!
//! Every action is checked here against the caller's *effective* role (a role whose personal
//! password is not set yet counts as Membro), using the matrix in `vox_proto::Role`; the app only
//! hides what is not allowed. Voice moderation is enforced in LiveKit too ([`LiveKitAdmin`]).
//!
//! [`LiveKitAdmin`]: crate::livekit_admin::LiveKitAdmin

use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use vox_proto::*;

use crate::api::{argon2_blocking, too_many, ApiErr, Authed};
use crate::auth::{self, user_key};
use crate::db::{self, UserRow};
use crate::livekit_admin::LkError;
use crate::state::AppState;
use crate::util::{now_ms, random_hex, sha256_hex};
use crate::voice_guard::{self, RoomKind};

type ApiResult<T> = Result<Json<T>, ApiErr>;

/// How long a moved member gets to leave the old room by itself before the hub removes it.
pub const MOVE_GRACE: Duration = Duration::from_secs(2);
/// Same for a disconnected member.
pub const DISCONNECT_GRACE: Duration = Duration::from_millis(1500);

fn ok() -> Json<OkResponse> {
    Json(OkResponse { ok: true })
}

fn forbidden() -> ApiErr {
    ApiErr::new(StatusCode::FORBIDDEN, "your role does not allow this")
}

fn room(channel: ChannelId) -> String {
    voice_guard::voice_room(channel)
}

/// Record a moderation action. A failure is logged, never fatal for the action itself.
pub(crate) async fn audit(s: &AppState, actor: Option<UserId>, action: &'static str, target: Option<UserId>, detail: String) {
    tracing::info!(?actor, action, ?target, detail, "audit");
    if let Err(e) = s.db.call(move |c| db::audit(c, actor, action, target, &detail)).await {
        tracing::error!("audit log write failed: {e:#}");
    }
}

/// A LiveKit call failing must not undo the hub-side action (the next join token enforces it
/// anyway); "not found" just means the person already left the room.
fn lk_result(what: &str, r: Result<impl Sized, LkError>) {
    match r {
        Ok(_) => tracing::info!(what, "livekit: done"),
        Err(LkError::NotFound) => tracing::debug!(what, "livekit: participant not in the room"),
        Err(LkError::Disabled) => tracing::debug!(what, "livekit: disabled"),
        Err(LkError::Other(e)) => tracing::warn!(what, error = %e, "livekit server call failed"),
    }
}

async fn target_row(s: &AppState, id: UserId) -> Result<UserRow, ApiErr> {
    s.db.call(move |c| db::get_user(c, id)).await?.ok_or_else(|| ApiErr::new(StatusCode::NOT_FOUND, "user not found"))
}

/// The target of a person-targeted moderation action: someone else, whose role is below mine.
pub(crate) async fn moderation_target(s: &AppState, me: &Authed, id: UserId) -> Result<UserRow, ApiErr> {
    if !me.role.can_moderate() {
        return Err(forbidden());
    }
    if id == me.user.id {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, "not on yourself"));
    }
    let t = target_row(s, id).await?;
    if !me.role.outranks(t.role(&s.owner())) {
        return Err(ApiErr::new(StatusCode::FORBIDDEN, "their role is not below yours"));
    }
    Ok(t)
}

fn channel_name(s: &AppState, id: ChannelId) -> String {
    s.channel(id).map(|c| c.name).unwrap_or_else(|| format!("#{id}"))
}

// ---------------------------------------------------------------- my account

fn check_new_password(p: &str) -> Result<(), ApiErr> {
    let n = p.chars().count();
    if !(PERSONAL_PASSWORD_MIN..=PERSONAL_PASSWORD_MAX).contains(&n) {
        return Err(ApiErr::new(
            StatusCode::BAD_REQUEST,
            format!("personal password must be {PERSONAL_PASSWORD_MIN}-{PERSONAL_PASSWORD_MAX} characters"),
        ));
    }
    Ok(())
}

/// `POST /api/account/password`
pub async fn set_password(
    State(s): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    me: Authed,
    body: Result<Json<SetPasswordRequest>, JsonRejection>,
) -> ApiResult<SetPasswordResponse> {
    let Json(req) = body?;
    if let Some(p) = &req.new {
        check_new_password(p)?;
    }
    let row = me.row.clone();
    if let Some(phc) = row.password_hash.clone() {
        // One check at a time per name: the attempt is reserved before the slow verification.
        let attempt = s.throttle.begin(&user_key(&row.username)).map_err(too_many)?;
        let current = req.current.clone().unwrap_or_default();
        let good = argon2_blocking(&s, move || auth::verify_password(&current, &phc)).await?;
        if !good {
            attempt.fail();
            tokio::time::sleep(Duration::from_millis(300)).await;
            return Err(ApiErr::new(StatusCode::UNAUTHORIZED, ERR_WRONG_PERSONAL_PASSWORD));
        }
        attempt.succeed();
    } else if req.new.is_some() && row.role(&s.owner()) == Role::Owner {
        // The owner's first personal password switches the owner's powers on: only the host may
        // choose it (the server PC, the session that claimed the server, or the console's code).
        crate::owner::check_first_password(&s, &me, peer.ip(), &headers, req.owner_code.as_deref()).await?;
    }
    let hash = match req.new.clone() {
        None => {
            if row.role(&s.owner()) > Role::Member {
                return Err(ApiErr::new(StatusCode::BAD_REQUEST, "your role needs a personal password"));
            }
            None
        }
        Some(p) => Some(argon2_blocking(&s, move || auth::hash_password(&p)).await??),
    };
    let (id, keep) = (row.id, me.session_hash.clone());
    s.db.call(move |c| db::set_password(c, id, hash.as_deref(), Some(&keep))).await?;
    // Whoever else was logged in as this name (another device, or someone who took the name
    // before its owner protected it) is out.
    s.kick(id, Some(&me.session_hash));
    let device_token = if req.new.is_some() && req.remember_device {
        let token = random_hex::<32>();
        let h = sha256_hex(&token);
        s.db.call(move |c| db::add_device_token(c, id, &h)).await?;
        Some(token)
    } else {
        None
    };
    let fresh = target_row(&s, id).await?;
    if fresh.has_password() && fresh.role(&s.owner()) == Role::Owner {
        s.ownership.owner_protected();
    }
    // A staff role's powers (and private channels, v0.9) switch on with its first password.
    crate::access::reload_roles(&s).await;
    tracing::info!(user = %fresh.username, set = fresh.has_password(), "personal password changed");
    Ok(Json(SetPasswordResponse { account: s.account(&fresh), device_token }))
}

// ---------------------------------------------------------------- voice moderation

/// Every room `user` is in, as far as the hub knows (declared voice channel, DM call) and as
/// LiveKit really has it.
async fn rooms_of(s: &AppState, user: UserId) -> Vec<String> {
    let mut rooms = voice_guard::rooms_of(s, user).await;
    if let Some(ch) = s.voice_of(user).and_then(|m| m.channel_id) {
        rooms.push(room(ch));
    }
    if let Some(r) = crate::calls::room_of(s, user) {
        rooms.push(r);
    }
    rooms.sort();
    rooms.dedup();
    rooms
}

/// The voice channels (not DM calls) `user` is in: the declared one first, then the ones LiveKit
/// has them in without the hub knowing. Only those `viewer` (the moderator) may see (v0.9): a
/// private room they have no access to is as if it did not exist ("not in voice").
async fn voice_channels_of(s: &AppState, user: UserId, viewer: UserId) -> Vec<ChannelId> {
    let mut out: Vec<ChannelId> = s.voice_of(user).and_then(|m| m.channel_id).into_iter().collect();
    for r in voice_guard::rooms_of(s, user).await {
        if let Some(RoomKind::Voice(ch)) = RoomKind::parse(&r) {
            if !out.contains(&ch) {
                out.push(ch);
            }
        }
    }
    let rules = s.access.rules();
    out.retain(|ch| rules.can_see(viewer, *ch));
    out
}

/// `POST /api/mod/users/{id}/mute`
pub async fn mute(State(s): State<AppState>, me: Authed, Path(id): Path<UserId>, body: Result<Json<ModMuteRequest>, JsonRejection>) -> ApiResult<OkResponse> {
    let Json(req) = body?;
    let t = moderation_target(&s, &me, id).await?;
    let muted = req.muted;
    s.db.call(move |c| db::set_server_muted(c, id, muted)).await?;
    let changed = s.set_server_muted(id, muted);
    // Wherever they really are, listed or not (voice channels and a DM call).
    for r in rooms_of(&s, id).await {
        lk_result("server mute", sync_mic_permission(&s, id, &r).await);
    }
    if changed {
        let action = if muted { ModAction::ServerMute } else { ModAction::ServerUnmute };
        s.send_to(id, &ServerEvent::Moderated { action, by: me.user.clone() });
        let what = if muted { AUDIT_SERVER_MUTE } else { AUDIT_SERVER_UNMUTE };
        audit(&s, Some(me.user.id), what, Some(t.id), String::new()).await;
    }
    Ok(ok())
}

/// A server-muted member just entered a voice room: their join token already leaves the mic
/// out, but one issued before the mute does not. Make LiveKit agree (the participant may still
/// be connecting, so try for a few seconds). The periodic voice check catches whatever this misses.
pub fn enforce_mute_on_join(s: &AppState, user: UserId, room: String) {
    let s = s.clone();
    tokio::spawn(async move {
        for _ in 0..6 {
            match sync_mic_permission(&s, user, &room).await {
                Err(LkError::NotFound) => tokio::time::sleep(Duration::from_secs(1)).await,
                r => return lk_result("server mute on join", r),
            }
        }
    });
}

/// Make LiveKit's microphone permission for `user` in `room` match the hub's current
/// server-mute state. One at a time, reading the state inside the lock: whatever order a mute,
/// an unmute and a join retry run in, the last call applies the newest state.
pub(crate) async fn sync_mic_permission(s: &AppState, user: UserId, room: &str) -> Result<(), LkError> {
    static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one = ONE_AT_A_TIME.lock().await;
    let allowed = !s.is_server_muted(user);
    s.livekit.set_mic_allowed(room, &user.to_string(), allowed).await
}

/// `POST /api/mod/users/{id}/disconnect`
pub async fn disconnect(State(s): State<AppState>, me: Authed, Path(id): Path<UserId>) -> ApiResult<OkResponse> {
    let t = moderation_target(&s, &me, id).await?;
    let channels = voice_channels_of(&s, id, me.user.id).await;
    let Some(&ch) = channels.first() else {
        return Err(ApiErr::new(StatusCode::CONFLICT, "not in voice"));
    };
    // Tokens from before this moment no longer get them back in.
    voice_guard::revoke(&s, id);
    s.send_to(id, &ServerEvent::Moderated { action: ModAction::Disconnect, by: me.user.clone() });
    s.force_leave(id);
    // A current client leaves by itself (a clean, client-side close); LiveKit removes whoever is
    // still in the room a moment later (an older client, or one the hub did not list).
    let s2 = s.clone();
    tokio::spawn(async move {
        tokio::time::sleep(DISCONNECT_GRACE).await;
        for ch in channels {
            lk_result("disconnect", s2.livekit.remove(&room(ch), &id.to_string()).await);
        }
    });
    audit(&s, Some(me.user.id), AUDIT_DISCONNECT, Some(t.id), channel_name(&s, ch)).await;
    Ok(ok())
}

/// `POST /api/mod/users/{id}/move`
pub async fn move_user(State(s): State<AppState>, me: Authed, Path(id): Path<UserId>, body: Result<Json<ModMoveRequest>, JsonRejection>) -> ApiResult<OkResponse> {
    let Json(req) = body?;
    let t = moderation_target(&s, &me, id).await?;
    let to = req.channel_id;
    // v0.9: a private channel I may not see is unknown to me; one they may not see, refused.
    if !matches!(s.channel_for(me.user.id, to), Some(c) if c.kind == ChannelKind::Voice) {
        return Err(ApiErr::new(StatusCode::NOT_FOUND, "unknown voice channel"));
    }
    if !s.can_see(id, to) {
        return Err(ApiErr::new(StatusCode::FORBIDDEN, "they have no access to that private channel"));
    }
    let channels = voice_channels_of(&s, id, me.user.id).await;
    let Some(&from) = channels.first() else {
        return Err(ApiErr::new(StatusCode::CONFLICT, "not in voice"));
    };
    if from == to {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, "already in that channel"));
    }
    // The app joins the new room with a fresh token; older tokens are done.
    voice_guard::revoke(&s, id);
    s.send_to(id, &ServerEvent::Moderated { action: ModAction::Move { channel_id: to }, by: me.user.clone() });
    // A current client leaves the old room by itself and joins the new one; an older client does
    // not know the event, so it is removed from the old room after a grace period.
    let s2 = s.clone();
    tokio::spawn(async move {
        tokio::time::sleep(MOVE_GRACE).await;
        for ch in channels.into_iter().filter(|c| *c != to) {
            lk_result("move: leave old room", s2.livekit.remove(&room(ch), &id.to_string()).await);
        }
        if s2.voice_of(id).and_then(|m| m.channel_id) == Some(from) {
            tracing::info!(user = id, from, to, "moved member still in the old channel: removed");
            s2.force_leave(id);
        }
    });
    let detail = format!("{} -> {}", channel_name(&s, from), channel_name(&s, to));
    audit(&s, Some(me.user.id), AUDIT_MOVE, Some(t.id), detail).await;
    Ok(ok())
}

/// `POST /api/mod/users/{id}/stop-stream`
pub async fn stop_stream(State(s): State<AppState>, me: Authed, Path(id): Path<UserId>) -> ApiResult<OkResponse> {
    let t = moderation_target(&s, &me, id).await?;
    let channels = voice_channels_of(&s, id, me.user.id).await;
    let Some(&first) = channels.first() else {
        return Err(ApiErr::new(StatusCode::CONFLICT, "not in voice"));
    };
    let declared = s.voice_of(id).filter(|m| m.streaming).and_then(|m| m.channel_id);
    // Mute the screen share wherever LiveKit has it, listed or not.
    let mut muted = 0;
    for ch in &channels {
        match s.livekit.mute_screen_share(&room(*ch), &id.to_string()).await {
            Ok(n) => muted += n,
            r => lk_result("stop stream", r),
        }
    }
    if declared.is_none() && muted == 0 {
        return Err(ApiErr::new(StatusCode::CONFLICT, "not streaming"));
    }
    s.send_to(id, &ServerEvent::Moderated { action: ModAction::StopStream, by: me.user.clone() });
    s.force_stop_stream(id);
    audit(&s, Some(me.user.id), AUDIT_STOP_STREAM, Some(t.id), channel_name(&s, declared.unwrap_or(first))).await;
    Ok(ok())
}

// ---------------------------------------------------------------- chat

/// `POST /api/messages/{id}/delete`
pub async fn delete_message(State(s): State<AppState>, me: Authed, Path(id): Path<MessageId>) -> ApiResult<OkResponse> {
    // A message of a private channel I may not see is as unknown as a missing one (v0.9).
    let Some((channel_id, author, content)) = s.db.call(move |c| db::get_message(c, id)).await?.filter(|(ch, _, _)| s.can_see(me.user.id, *ch)) else {
        return Err(ApiErr::new(StatusCode::NOT_FOUND, "message not found"));
    };
    let own = author == me.user.id;
    if !own && !me.role.can_moderate() {
        return Err(forbidden());
    }
    let dir = s.attachments.dir.clone();
    s.db
        .call(move |c| {
            db::delete_message(c, id)?;
            crate::attachments::collect_garbage(c, &dir)
        })
        .await?;
    s.send_channel(channel_id, &ServerEvent::MessageDeleted { channel_id, message_id: id });
    if !own {
        let excerpt: String = content.chars().take(80).collect();
        let more = if content.chars().count() > 80 { "..." } else { "" };
        let detail = format!("#{}: {excerpt}{more}", channel_name(&s, channel_id));
        audit(&s, Some(me.user.id), AUDIT_DELETE_MESSAGE, Some(author), detail).await;
    }
    Ok(ok())
}

// ---------------------------------------------------------------- members and roles

/// `GET /api/admin/users`
pub async fn users(State(s): State<AppState>, me: Authed) -> ApiResult<Vec<MemberInfo>> {
    if !me.role.can_admin() {
        return Err(forbidden());
    }
    let rows = s.db.call(|c| db::list_users(c)).await?;
    let owner = s.owner().to_string();
    Ok(Json(
        rows.into_iter()
            .map(|r| MemberInfo { user: r.user(&owner), has_password: r.has_password(), server_muted: r.server_muted, created_at_ms: r.created_at_ms })
            .collect(),
    ))
}

/// `POST /api/admin/users/{id}/role`
pub async fn set_role(State(s): State<AppState>, me: Authed, Path(id): Path<UserId>, body: Result<Json<SetRoleRequest>, JsonRejection>) -> ApiResult<OkResponse> {
    let Json(req) = body?;
    if !me.role.can_admin() {
        return Err(forbidden());
    }
    let t = target_row(&s, id).await?;
    let current = t.role(&s.owner());
    if !me.role.can_set_role(current, req.role) {
        return Err(forbidden());
    }
    if req.role > Role::Member && !t.has_password() {
        return Err(ApiErr::new(StatusCode::CONFLICT, "they must set a personal password first"));
    }
    let to = req.role;
    s.db.call(move |c| db::set_role(c, id, to)).await?;
    let user = target_row(&s, id).await?.user(&s.owner());
    s.update_user(&user);
    // Their private channels follow the new role at once (v0.9).
    crate::access::reload_roles(&s).await;
    let detail = format!("{} -> {}", current.wire_name(), to.wire_name());
    audit(&s, Some(me.user.id), AUDIT_SET_ROLE, Some(id), detail).await;
    Ok(ok())
}

/// `POST /api/admin/users/{id}/reset-password` (owner only)
pub async fn reset_password(State(s): State<AppState>, me: Authed, Path(id): Path<UserId>) -> ApiResult<ResetPasswordResponse> {
    if !me.role.can_reset_passwords() {
        return Err(forbidden());
    }
    if id == me.user.id {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, "change your own password in your settings"));
    }
    let t = target_row(&s, id).await?;
    let (code, expires_at_ms) = issue_reset_code_async(&s, t.id).await?;
    s.kick(id, None);
    // Their sessions are gone: so is their voice, and an old join token does not bring it back.
    voice_guard::revoke(&s, id);
    s.force_leave(id);
    voice_guard::remove_everywhere(&s, id).await;
    // Until a new password is chosen, a staff role counts as Membro for private channels (v0.9).
    crate::access::reload_roles(&s).await;
    audit(&s, Some(me.user.id), AUDIT_RESET_PASSWORD, Some(t.id), String::new()).await;
    Ok(Json(ResetPasswordResponse { code, expires_at_ms }))
}

async fn issue_reset_code_async(s: &AppState, id: UserId) -> Result<(String, i64), ApiErr> {
    let code = auth::new_reset_code();
    let hash = auth::reset_code_hash(&code);
    let expires = now_ms() + auth::RESET_CODE_TTL_MS;
    s.db.call(move |c| db::set_reset_code(c, id, &hash, expires)).await?;
    Ok((code, expires))
}

#[derive(Deserialize)]
pub struct AuditQuery {
    before: Option<i64>,
    limit: Option<i64>,
}

/// `GET /api/admin/audit`
pub async fn audit_log(State(s): State<AppState>, me: Authed, Query(q): Query<AuditQuery>) -> ApiResult<Vec<AuditEntry>> {
    if !me.role.can_admin() {
        return Err(forbidden());
    }
    let limit = q.limit.unwrap_or(100).clamp(1, 200);
    let owner = s.owner().to_string();
    Ok(Json(s.db.call(move |c| db::list_audit(c, q.before, limit, &owner)).await?))
}

// ---------------------------------------------------------------- channels

fn check_channel_name(name: &str) -> Result<String, ApiErr> {
    let name = name.trim();
    let n = name.chars().count();
    if !(1..=CHANNEL_NAME_MAX).contains(&n) || name.chars().any(char::is_control) {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, format!("channel name must be 1-{CHANNEL_NAME_MAX} characters")));
    }
    Ok(name.to_string())
}

fn kind_pt(kind: ChannelKind) -> &'static str {
    match kind {
        ChannelKind::Text => "texto",
        ChannelKind::Voice => "voz",
    }
}

/// Read the channel list again and tell everybody their own view of it; whoever lost access to a
/// private voice channel is taken out of it at once (v0.9).
async fn reload_channels(s: &AppState) -> Result<(), ApiErr> {
    let list = s.db.call(|c| db::list_channels(c)).await?;
    s.set_channels(list);
    crate::access::enforce(s).await;
    Ok(())
}

/// The names of these accounts (the moderation log).
async fn names_of(s: &AppState, ids: Vec<UserId>) -> Vec<String> {
    match s.db.call(move |c| ids.iter().map(|id| db::get_user(c, *id)).collect::<rusqlite::Result<Vec<_>>>()).await {
        Ok(rows) => rows.into_iter().flatten().map(|r| r.username).collect(),
        Err(e) => {
            tracing::error!("names for the moderation log: {e:#}");
            Vec::new()
        }
    }
}

fn can_manage_channels(me: &Authed) -> Result<(), ApiErr> {
    if me.role.can_manage_channels() { Ok(()) } else { Err(forbidden()) }
}

/// `POST /api/admin/channels`
pub async fn create_channel(State(s): State<AppState>, me: Authed, body: Result<Json<ChannelCreateRequest>, JsonRejection>) -> ApiResult<Channel> {
    let Json(req) = body?;
    can_manage_channels(&me)?;
    let name = check_channel_name(&req.name)?;
    let (kind, private) = (req.kind, req.private.clone());
    // v0.9: private from the start, in the same transaction (it is never public for a moment).
    let ch = s
        .db
        .call(move |c| {
            let tx = c.transaction()?;
            let mut ch = db::create_channel(&tx, &name, kind)?;
            if let Some(p) = &private {
                ch.private = crate::access::store(&tx, ch.id, Some(p))?;
            }
            tx.commit()?;
            Ok(ch)
        })
        .await?;
    reload_channels(&s).await?;
    let mut detail = format!("{} ({})", ch.name, kind_pt(ch.kind));
    if let Some(p) = &ch.private {
        let names = names_of(&s, p.users.clone()).await;
        detail = format!("{} ({}, {})", ch.name, kind_pt(ch.kind), crate::access::describe(Some(p), &names));
    }
    audit(&s, Some(me.user.id), AUDIT_CHANNEL_CREATE, None, detail).await;
    Ok(Json(ch))
}

/// `POST /api/admin/channels/{id}/access` (v0.9): private with this list, or public again.
pub async fn set_channel_access(State(s): State<AppState>, me: Authed, Path(id): Path<ChannelId>, body: Result<Json<ChannelAccessRequest>, JsonRejection>) -> ApiResult<OkResponse> {
    let Json(req) = body?;
    can_manage_channels(&me)?;
    let Some(ch) = s.channel(id) else {
        return Err(ApiErr::new(StatusCode::NOT_FOUND, "channel not found"));
    };
    let asked = req.private.clone();
    let stored = s.db.call(move |c| crate::access::store(c, id, asked.as_ref())).await?;
    if stored == ch.private {
        return Ok(ok()); // nothing changed: nobody to tell, nothing to log
    }
    reload_channels(&s).await?;
    let names = names_of(&s, stored.as_ref().map(|p| p.users.clone()).unwrap_or_default()).await;
    tracing::info!(channel = id, private = stored.is_some(), "channel access changed");
    let detail = format!("{}: {}", ch.name, crate::access::describe(stored.as_ref(), &names));
    audit(&s, Some(me.user.id), AUDIT_CHANNEL_ACCESS, None, detail).await;
    Ok(ok())
}

/// Is anybody in voice channel `id`'s room: listed by the hub, or present at LiveKit (an
/// unlisted participant counts too; LiveKit unreachable or off counts as empty only when the hub
/// lists nobody).
async fn room_occupied(s: &AppState, id: ChannelId) -> bool {
    if s.voice_members().iter().any(|m| m.channel_id == Some(id)) {
        return true;
    }
    match s.livekit.list_participants(&room(id)).await {
        Ok(list) => !list.is_empty(),
        Err(LkError::NotFound) | Err(LkError::Disabled) => false,
        Err(LkError::Other(e)) => {
            // Cannot tell: refuse the change rather than flip the flag under somebody's feet.
            tracing::warn!(channel = id, error = %e, "cannot list the voice room before an e2ee change");
            true
        }
    }
}

/// The people who may see channel `ch` but have no device with encryption keys on this server
/// (an app older than 1.1, or 1.1 never opened here): they cannot enter it while it is encrypted.
async fn e2ee_outdated(s: &AppState, ch: ChannelId) -> Result<Vec<User>, ApiErr> {
    let owner = s.owner();
    let (rows, devices) = s.db.call(|c| Ok((db::list_users(c)?, crate::e2ee::list_all(c)?))).await?;
    let mut out: Vec<User> = rows
        .into_iter()
        .filter(|r| s.can_see(r.id, ch) && !devices.iter().any(|d| d.user_id == r.id))
        .map(|r| r.user(&owner))
        .collect();
    out.sort_by_key(|u| u.username.to_lowercase());
    Ok(out)
}

/// v1.1 (E1): `GET /api/channels/{id}/e2ee` (Admin and up): the flag and who could not enter.
pub async fn get_channel_e2ee(State(s): State<AppState>, me: Authed, Path(id): Path<ChannelId>) -> ApiResult<ChannelE2eeResponse> {
    can_manage_channels(&me)?;
    let Some(ch) = s.channel(id).filter(|c| c.kind == ChannelKind::Voice) else {
        return Err(ApiErr::new(StatusCode::NOT_FOUND, "voice channel not found"));
    };
    Ok(Json(ChannelE2eeResponse { on: ch.e2ee, outdated: e2ee_outdated(&s, id).await? }))
}

/// v1.1 (E1): `POST /api/channels/{id}/e2ee` (Admin and up): end-to-end encrypted calls on or off,
/// only while the room is empty (`409` [`ERR_ROOM_OCCUPIED`]): nobody's call changes under them,
/// and nobody keeps a token from the other mode.
pub async fn set_channel_e2ee(State(s): State<AppState>, me: Authed, Path(id): Path<ChannelId>, body: Result<Json<ChannelE2eeRequest>, JsonRejection>) -> ApiResult<ChannelE2eeResponse> {
    let Json(req) = body?;
    can_manage_channels(&me)?;
    let Some(ch) = s.channel(id) else {
        return Err(ApiErr::new(StatusCode::NOT_FOUND, "channel not found"));
    };
    if ch.kind != ChannelKind::Voice {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, "not a voice channel"));
    }
    if ch.e2ee != req.on {
        if room_occupied(&s, id).await {
            return Err(ApiErr::new(StatusCode::CONFLICT, ERR_ROOM_OCCUPIED));
        }
        let on = req.on;
        s.db.call(move |c| db::set_channel_e2ee(c, id, on)).await?;
        reload_channels(&s).await?;
        // A join token of the other mode may still be on its way into the room (the room was
        // empty a moment ago): such a participant is removed (`voice_guard`); check closely while
        // one could still be used.
        crate::voice_guard::watch_closely(&s);
        tracing::info!(channel = id, on, "channel e2ee changed");
        let detail = format!("{}: {}", ch.name, if on { "ligada" } else { "desligada" });
        audit(&s, Some(me.user.id), AUDIT_CHANNEL_E2EE, None, detail).await;
    }
    Ok(Json(ChannelE2eeResponse { on: req.on, outdated: e2ee_outdated(&s, id).await? }))
}

/// `POST /api/admin/channels/{id}/rename`
pub async fn rename_channel(State(s): State<AppState>, me: Authed, Path(id): Path<ChannelId>, body: Result<Json<ChannelRenameRequest>, JsonRejection>) -> ApiResult<OkResponse> {
    let Json(req) = body?;
    can_manage_channels(&me)?;
    let name = check_channel_name(&req.name)?;
    let Some(old) = s.channel(id) else {
        return Err(ApiErr::new(StatusCode::NOT_FOUND, "channel not found"));
    };
    let n2 = name.clone();
    s.db.call(move |c| db::rename_channel(c, id, &n2)).await?;
    reload_channels(&s).await?;
    audit(&s, Some(me.user.id), AUDIT_CHANNEL_RENAME, None, format!("{} -> {name}", old.name)).await;
    Ok(ok())
}

/// `POST /api/admin/channels/{id}/delete`: its messages go too; people in a voice channel are
/// taken out of it.
pub async fn delete_channel(State(s): State<AppState>, me: Authed, Path(id): Path<ChannelId>) -> ApiResult<OkResponse> {
    can_manage_channels(&me)?;
    let Some(ch) = s.channel(id) else {
        return Err(ApiErr::new(StatusCode::NOT_FOUND, "channel not found"));
    };
    if ch.kind == ChannelKind::Text && s.channels().iter().filter(|c| c.kind == ChannelKind::Text).count() <= 1 {
        return Err(ApiErr::new(StatusCode::CONFLICT, "the last text channel cannot be deleted"));
    }
    let dir = s.attachments.dir.clone();
    s.db
        .call(move |c| {
            db::delete_channel(c, id)?;
            crate::attachments::collect_garbage(c, &dir)
        })
        .await?;
    reload_channels(&s).await?;
    if ch.kind == ChannelKind::Voice {
        let mut who: Vec<UserId> = s.voice_members().into_iter().filter(|m| m.channel_id == Some(id)).map(|m| m.user.id).collect();
        // ...and whoever LiveKit has in the room without the hub listing them.
        if let Ok(list) = s.livekit.list_participants(&room(id)).await {
            who.extend(list.iter().filter_map(|p| p.identity.parse::<UserId>().ok()));
        }
        who.sort_unstable();
        who.dedup();
        for user in who {
            lk_result("channel deleted: remove", s.livekit.remove(&room(id), &user.to_string()).await);
            if s.voice_of(user).and_then(|m| m.channel_id) == Some(id) {
                s.force_leave(user);
            }
        }
    }
    audit(&s, Some(me.user.id), AUDIT_CHANNEL_DELETE, None, format!("{} ({})", ch.name, kind_pt(ch.kind))).await;
    Ok(ok())
}

/// `POST /api/admin/channels/reorder`
pub async fn reorder_channels(State(s): State<AppState>, me: Authed, body: Result<Json<ChannelReorderRequest>, JsonRejection>) -> ApiResult<OkResponse> {
    let Json(req) = body?;
    can_manage_channels(&me)?;
    let mut have: Vec<ChannelId> = s.channels().iter().map(|c| c.id).collect();
    let mut want = req.ids.clone();
    have.sort_unstable();
    want.sort_unstable();
    if have != want {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, "ids must list every channel exactly once"));
    }
    let ids = req.ids.clone();
    s.db.call(move |c| db::reorder_channels(c, &ids)).await?;
    reload_channels(&s).await?;
    audit(&s, Some(me.user.id), AUDIT_CHANNEL_REORDER, None, String::new()).await;
    Ok(ok())
}

// ---------------------------------------------------------------- command line

/// `concord-server --reset-password <name>`: the host's way back in (also for the owner). Prints
/// what to do; the running hub, if any, notices at the next request of that user.
pub fn reset_password_cli(data_dir: &std::path::Path, username: &str) -> anyhow::Result<String> {
    let path = data_dir.join("voxhub.db");
    if !path.exists() {
        anyhow::bail!("no database at {} (wrong --data-dir?)", path.display());
    }
    let db = db::Db::open(&path)?;
    let name = username.trim().to_string();
    let Some(row) = db.call_now(|c| db::find_user(c, &name))? else {
        anyhow::bail!("no user named {name:?} has ever logged in");
    };
    let code = auth::new_reset_code();
    let hash = auth::reset_code_hash(&code);
    let expires = now_ms() + auth::RESET_CODE_TTL_MS;
    db.call_now(|c| {
        db::set_reset_code(c, row.id, &hash, expires)?;
        db::audit(c, None, AUDIT_RESET_PASSWORD, Some(row.id), "linha de comando")
    })?;
    Ok(format!(
        "Senha pessoal de \"{}\" redefinida.\n\
         Código: {code}\n\
         Vale até a senha nova ser escolhida, por no máximo {} dias. No app, entre com esse nome e digite o código no campo \"Senha pessoal\";\n\
         o app pede uma senha nova em seguida. Sessões e computadores lembrados desse nome foram desconectados.",
        row.username,
        auth::RESET_CODE_TTL_MS / (24 * 3600 * 1000)
    ))
}
