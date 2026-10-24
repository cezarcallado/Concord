//! HTTP API (`/api/*`) and router assembly.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::DefaultBodyLimit;
use axum::extract::{ConnectInfo, FromRequestParts, Path, Query, Request, State};
use axum::http::{header, request::Parts, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use vox_proto::*;

use crate::auth::{self, ip_key, user_key, Denied, Proof};
use crate::db::UserRow;
use crate::linkpreview::{LinkPreviews, RateLimited};
use crate::livekit_token;
use crate::state::AppState;
use crate::util::{now_ms, random_hex, sha256_hex};
use crate::{admin, avatar, db, owner, profile, sounds, ws};

pub fn router(state: AppState) -> Router {
    let limits = std::sync::Arc::new(state.tuning.limits.clone());
    Router::new()
        .route("/api/health", get(health))
        .route("/api/login", post(login))
        .route("/api/channels", get(channels))
        .route("/api/channels/{id}/messages", get(messages).post(post_message).layer(DefaultBodyLimit::max(MESSAGE_MAX_BODY)))
        .route("/api/voice/{id}/join", post(voice_join))
        .route("/api/link-preview", get(link_preview))
        .route("/api/link-preview/image", get(link_preview_image))
        // item 12
        .route("/api/account/password", post(admin::set_password))
        .route("/api/messages/{id}/delete", post(admin::delete_message))
        .route("/api/messages/{id}/reactions", post(crate::chat::react))
        .route("/api/mod/users/{id}/mute", post(admin::mute))
        .route("/api/mod/users/{id}/disconnect", post(admin::disconnect))
        .route("/api/mod/users/{id}/move", post(admin::move_user))
        .route("/api/mod/users/{id}/stop-stream", post(admin::stop_stream))
        .route("/api/admin/users", get(admin::users))
        .route("/api/admin/users/{id}/role", post(admin::set_role))
        .route("/api/admin/users/{id}/reset-password", post(admin::reset_password))
        .route("/api/admin/audit", get(admin::audit_log))
        // "quem hospeda vira dono"
        .route("/api/owner/claim", post(owner::claim_with_code))
        .route("/api/admin/transfer-owner", post(owner::transfer))
        .route("/api/admin/channels", post(admin::create_channel))
        .route("/api/admin/channels/reorder", post(admin::reorder_channels))
        .route("/api/admin/channels/{id}/rename", post(admin::rename_channel))
        .route("/api/admin/channels/{id}/delete", post(admin::delete_channel))
        // private channels (v0.9, `access.rs`)
        .route("/api/admin/channels/{id}/access", post(admin::set_channel_access))
        // v1.1 (E1): end-to-end encrypted calls in a voice channel (Admin and up)
        .route("/api/channels/{id}/e2ee", get(admin::get_channel_e2ee).post(admin::set_channel_e2ee))
        // profile pictures
        .route("/api/me/avatar", put(avatar::put_avatar).delete(avatar::delete_avatar).layer(DefaultBodyLimit::max(AVATAR_MAX_UPLOAD_BYTES)))
        .route("/api/users/{id}/avatar", get(avatar::get_avatar))
        // Go Live hover previews (v1.1, `stream_preview.rs`)
        .route("/api/me/stream-preview", put(crate::stream_preview::put).delete(crate::stream_preview::delete).layer(DefaultBodyLimit::max(STREAM_PREVIEW_MAX_BODY)))
        .route("/api/users/{id}/stream-preview", get(crate::stream_preview::get))
        .route("/api/mod/users/{id}/remove-avatar", post(avatar::remove_avatar))
        // profiles: display name, bio, colour, custom status, presence
        .route("/api/users/{id}/profile", get(profile::get_profile))
        .route("/api/me/profile", put(profile::put_profile))
        .route("/api/me/presence", put(profile::put_presence))
        .route("/api/me/status", put(profile::put_status).delete(profile::delete_status))
        .route("/api/mod/users/{id}/clear-profile", post(profile::clear_profile))
        // soundboard
        .route("/api/sounds", get(sounds::list).post(sounds::upload).layer(DefaultBodyLimit::max(SOUND_MAX_UPLOAD_BYTES + 4096)))
        .route("/api/sounds/{id}/audio", get(sounds::audio))
        .route("/api/sounds/{id}/rename", post(sounds::rename))
        .route("/api/sounds/{id}/delete", post(sounds::delete))
        // chat images (`attachments.rs`)
        .route("/api/attachments/limits", get(crate::attachments::limits))
        .route("/api/attachments", post(crate::attachments::upload).layer(DefaultBodyLimit::max(ATTACHMENT_MAX_UPLOAD_BODY)))
        .route("/api/attachments/{id}", get(crate::attachments::get_file))
        .route("/api/attachments/{id}/thumb", get(crate::attachments::get_thumb))
        // invites (v0.8, `invites.rs`)
        .route("/api/invites", get(crate::invites::list_invites).post(crate::invites::create))
        .route("/api/invites/redeem", post(crate::invites::redeem))
        .route("/api/invites/{id}/revoke", post(crate::invites::revoke))
        // direct messages and private calls (`dms.rs`, `calls.rs`)
        .merge(crate::dms::routes())
        // end-to-end encryption, E0 (v1.1, `e2ee.rs`): the directory of device keys
        .merge(crate::e2ee::routes())
        .route("/ws", get(ws::handler))
        .layer(middleware::from_fn(log_request))
        .layer(middleware::from_fn(move |req, next| crate::serve::timeout_layer(limits.clone(), req, next)))
        .with_state(state)
}

// ---------- errors ----------

pub struct ApiErr(pub StatusCode, pub String);

impl ApiErr {
    pub fn new(status: StatusCode, msg: impl Into<String>) -> Self {
        ApiErr(status, msg.into())
    }
}

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.0, Json(ApiError { error: self.1 })).into_response()
    }
}

impl From<anyhow::Error> for ApiErr {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!("internal error: {e:#}");
        ApiErr::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }
}

impl From<JsonRejection> for ApiErr {
    fn from(e: JsonRejection) -> Self {
        ApiErr::new(StatusCode::BAD_REQUEST, format!("invalid request body: {}", e.body_text()))
    }
}

type ApiResult<T> = Result<Json<T>, ApiErr>;

async fn log_request(req: Request, next: Next) -> Response {
    let (method, path) = (req.method().clone(), req.uri().path().to_owned());
    let t = Instant::now();
    let resp = next.run(req).await;
    tracing::debug!(%method, path, status = resp.status().as_u16(), ms = t.elapsed().as_millis() as u64, "http");
    resp
}

// ---------- auth ----------

fn bearer_hash(parts: &Parts) -> Option<String> {
    let v = parts.headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let t = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer "))?;
    Some(sha256_hex(t.trim()))
}

/// Resolve a session token hash to its user.
pub async fn user_for_token_hash(state: &AppState, hash: String) -> Result<UserRow, ApiErr> {
    state
        .db
        .call(move |c| db::user_for_session(c, &hash))
        .await?
        .ok_or_else(|| ApiErr::new(StatusCode::UNAUTHORIZED, "invalid or expired session"))
}

pub struct AuthUser(pub User);

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiErr;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiErr> {
        let hash = bearer_hash(parts).ok_or_else(|| ApiErr::new(StatusCode::UNAUTHORIZED, "missing bearer token"))?;
        Ok(AuthUser(user_for_token_hash(state, hash).await?.user(&state.owner())))
    }
}

/// An authenticated caller with what permission checks need (item 12).
pub struct Authed {
    pub row: UserRow,
    pub user: User,
    /// The role permissions are checked against: `Member` while a needed personal password is
    /// not set yet.
    pub role: Role,
    pub session_hash: String,
}

impl FromRequestParts<AppState> for Authed {
    type Rejection = ApiErr;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiErr> {
        let hash = bearer_hash(parts).ok_or_else(|| ApiErr::new(StatusCode::UNAUTHORIZED, "missing bearer token"))?;
        let row = user_for_token_hash(state, hash.clone()).await?;
        let owner = state.owner();
        Ok(Authed { user: row.user(&owner), role: row.effective_role(&owner), row, session_hash: hash })
    }
}

/// `429` with the wait in seconds.
pub fn too_many(wait: Duration) -> ApiErr {
    let secs = wait.as_secs_f64().ceil().max(1.0) as u64;
    ApiErr::new(StatusCode::TOO_MANY_REQUESTS, format!("{ERR_TOO_MANY_ATTEMPTS}, wait {secs} s"))
}

/// How long a password check waits for an argon2 slot before the hub answers "busy" (`429`).
const ARGON2_QUEUE_WAIT: Duration = Duration::from_secs(3);

/// Run `f` (argon2) on the blocking pool, at most [`crate::auth::ARGON2_AT_ONCE`] at a time. The
/// slot is held by the blocking work itself, so a cancelled request does not free it early.
pub async fn argon2_blocking<T: Send + 'static>(s: &AppState, f: impl FnOnce() -> T + Send + 'static) -> Result<T, ApiErr> {
    let permit = match tokio::time::timeout(ARGON2_QUEUE_WAIT, s.argon2.clone().acquire_owned()).await {
        Ok(Ok(p)) => p,
        _ => {
            tracing::warn!("password checks are saturated: request refused");
            return Err(too_many(Duration::from_secs(2)));
        }
    };
    crate::util::spawn_blocking_with(permit, f).await.map_err(|e| anyhow::anyhow!("{e}").into())
}

// ---------- handlers ----------

async fn health(State(s): State<AppState>) -> Json<HealthResponse> {
    let addr = ("127.0.0.1", s.cfg.livekit.signal_port);
    let livekit_ok = matches!(
        tokio::time::timeout(Duration::from_millis(500), tokio::net::TcpStream::connect(addr)).await,
        Ok(Ok(_))
    );
    Json(HealthResponse {
        ok: true,
        version: env!("CARGO_PKG_VERSION").into(),
        protocol_version: PROTOCOL_VERSION,
        livekit_ok,
        server_id: Some(s.server_id.clone()),
    })
}

async fn login(
    State(s): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: header::HeaderMap,
    body: Result<Json<LoginRequest>, JsonRejection>,
) -> ApiResult<LoginResponse> {
    let Json(req) = body?;
    if req.protocol_version != PROTOCOL_VERSION {
        return Err(ApiErr::new(
            StatusCode::BAD_REQUEST,
            format!("protocol version mismatch: server {PROTOCOL_VERSION}, client {}", req.protocol_version),
        ));
    }
    let username = req.username.trim().to_string();
    let len = username.chars().count();
    if !(2..=32).contains(&len) || username.chars().any(char::is_control) {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, "username must be 2-32 characters"));
    }
    let ip = ip_key(peer.ip());
    if let Some(wait) = s.throttle.must_wait(&ip) {
        tracing::warn!(%peer, username, "login refused: address is being slowed down");
        return Err(too_many(wait));
    }
    // v0.8: the server password, an invite, or a remembered device of an invite member.
    let admission = match crate::invites::admit(&s, &req, &username).await? {
        Ok(a) => a,
        Err(refused) => {
            tracing::warn!(%peer, username, invite = req.invite.is_some(), reason = %refused.1, "login rejected: server password / invite");
            s.throttle.fail(&ip);
            tokio::time::sleep(Duration::from_millis(300)).await; // blunt brute-force brake
            return Err(refused);
        }
    };

    // Item 12: a name protected by a personal password needs it (or a remembered device).
    let name = username.clone();
    let existing = s.db.call(move |c| db::find_user(c, &name)).await?;
    let mut proof = Proof::Open;
    if let Some(row) = existing {
        let ukey = user_key(&row.username);
        let device_ok = match req.device_token.as_deref() {
            Some(t) if row.has_password() => {
                let (h, id) = (sha256_hex(t), row.id);
                s.db.call(move |c| db::use_device_token(c, id, &h)).await?
            }
            _ => false,
        };
        // A check of a password / code reserves its attempt first (check-and-reserve in one step),
        // so simultaneous guesses get no more checks than guesses one after the other.
        let checks_secret = !device_ok && req.password.is_some() && (row.has_password() || row.reset_hash.is_some());
        let attempt = if checks_secret {
            match s.throttle.begin(&ukey) {
                Ok(a) => Some(a),
                Err(wait) => {
                    tracing::warn!(%peer, username, "login refused: name is being slowed down");
                    return Err(too_many(wait));
                }
            }
        } else {
            None
        };
        let password = req.password.clone();
        let checked = if checks_secret && row.has_password() {
            argon2_blocking(&s, move || auth::check(&row, password.as_deref(), device_ok, now_ms())).await?
        } else {
            auth::check(&row, password.as_deref(), device_ok, now_ms())
        };
        match checked {
            Ok(p) => {
                proof = p;
                if let Some(a) = attempt {
                    if matches!(p, Proof::Password | Proof::ResetCode) {
                        a.succeed();
                    }
                }
            }
            Err(Denied::Required) => {
                tracing::info!(%peer, username, device_token = req.device_token.is_some(), "login needs the personal password");
                return Err(ApiErr::new(StatusCode::UNAUTHORIZED, ERR_PERSONAL_PASSWORD_REQUIRED));
            }
            Err(Denied::Wrong) => {
                tracing::warn!(%peer, username, "login rejected: wrong personal password");
                if let Some(a) = attempt {
                    a.fail();
                }
                s.throttle.fail(&ip);
                tokio::time::sleep(Duration::from_millis(300)).await;
                return Err(ApiErr::new(StatusCode::UNAUTHORIZED, ERR_WRONG_PERSONAL_PASSWORD));
            }
            Err(Denied::Expired) => {
                tracing::warn!(%peer, username, "login rejected: reset code expired");
                return Err(ApiErr::new(StatusCode::UNAUTHORIZED, ERR_RESET_CODE_EXPIRED));
            }
        }
    }
    // v1.0.1: an invite only creates new accounts. An existing name came in with its own proof
    // (personal password, remembered device, reset code) or not at all: an unprotected name
    // needs the server password, as before invites existed.
    let invite_name_taken = || {
        tracing::warn!(%peer, username, "login rejected: an invite does not admit an existing name");
        ApiErr::new(StatusCode::UNAUTHORIZED, ERR_INVITE_NAME_TAKEN)
    };
    if admission == crate::invites::Admission::ExistingName && !matches!(proof, Proof::Password | Proof::DeviceToken | Proof::ResetCode) {
        return Err(invite_name_taken());
    }
    s.throttle.succeed(&ip);
    crate::invites::take_use(&s, admission).await?;

    let token = random_hex::<32>();
    let hash = sha256_hex(&token);
    let name = username.clone();
    let row = if matches!(admission, crate::invites::Admission::Invite(_)) {
        // The name was free at the check; somebody may have taken it since (the use is spent).
        match s.db.call(move |c| db::login_new(c, &name, &hash)).await? {
            Some(row) => row,
            None => return Err(invite_name_taken()),
        }
    } else {
        s.db.call(move |c| db::login(c, &name, &hash)).await?
    };
    crate::invites::finish_login(&s, admission, row.id).await?;
    // Private channels (v0.9): a new account, or one whose password state changed outside this hub
    // (`--reset-password`), counts with its role as it is now.
    crate::access::reload_roles(&s).await;
    // A device token for a typed personal password, and for a newcomer with an invite (it is what
    // lets them back in without the server password, see `invites.rs`).
    let by_invite = proof == Proof::Open && matches!(admission, crate::invites::Admission::Invite(_));
    let device_token = if req.remember_device && (proof == Proof::Password || by_invite) {
        let device = random_hex::<32>();
        let (h, id) = (sha256_hex(&device), row.id);
        s.db.call(move |c| db::add_device_token(c, id, &h)).await?;
        Some(device)
    } else {
        None
    };
    // "Quem hospeda vira dono": no owner yet and this login comes from the server PC itself.
    if let Err(e) = owner::claim_from_host_pc(&s, &row, peer.ip(), &headers, Some(&sha256_hex(&token))).await {
        tracing::error!("claiming the server for {}: {e:#}", row.username);
    }
    let owner = s.owner();
    let user = row.user(&owner);
    tracing::info!(%peer, user = %user.username, id = user.id, role = ?user.role, ?proof, ?admission, must_set_password = row.must_set_password(&owner), "login");
    Ok(Json(LoginResponse { token, user, server_name: s.cfg.server_name.clone(), device_token }))
}

/// Only the channels the caller may see (v0.9).
async fn channels(State(s): State<AppState>, AuthUser(me): AuthUser) -> Json<Vec<Channel>> {
    Json(s.visible_channels(me.id))
}

#[derive(Deserialize)]
struct MessagesQuery {
    before: Option<MessageId>,
    limit: Option<i64>,
}

async fn messages(
    State(s): State<AppState>,
    AuthUser(me): AuthUser,
    Path(id): Path<ChannelId>,
    Query(q): Query<MessagesQuery>,
) -> ApiResult<Vec<Message>> {
    // A private channel without access is as unknown as a missing one (v0.9).
    if s.channel_for(me.id, id).is_none() {
        return Err(ApiErr::new(StatusCode::NOT_FOUND, "channel not found"));
    }
    let limit = q.limit.unwrap_or(50).clamp(1, 100);
    let owner = s.owner().to_string();
    Ok(Json(s.db.call(move |c| db::list_messages(c, id, q.before, limit, &owner)).await?))
}

/// The largest body `POST /api/channels/{id}/messages` reads (v1.0.1): 4000 characters of text,
/// a few image ids and [`MENTIONS_WIRE_MAX`] mention ids fit many times over.
pub const MESSAGE_MAX_BODY: usize = 64 * 1024;

/// `POST /api/channels/{id}/messages`: a message with images (or plain text) over HTTP, so the
/// app gets a clear answer (bad image, quota...) instead of a WebSocket error line.
async fn post_message(
    State(s): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<ChannelId>,
    body: Result<Json<SendMessageRequest>, JsonRejection>,
) -> ApiResult<Message> {
    let Json(req) = body?;
    if !matches!(s.channel_for(user.id, id), Some(c) if c.kind == ChannelKind::Text) {
        return Err(ApiErr::new(StatusCode::NOT_FOUND, "unknown text channel"));
    }
    let caption = req.content.trim().to_string();
    let n = caption.chars().count();
    if (n == 0 && req.attachments.is_empty()) || n > crate::ws::MAX_CONTENT_CHARS {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, "message must be 1-4000 characters"));
    }
    if req.mentions.len() > MENTIONS_WIRE_MAX {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, ERR_TOO_MANY_MENTIONS));
    }
    if !s.allow_message(user.id) {
        return Err(ApiErr::new(StatusCode::TOO_MANY_REQUESTS, "rate limited: slow down"));
    }
    let (author, owner) = (s.current_user(&user), s.owner());
    let ids = req.attachments.clone();
    let draft = crate::chat::Draft { mentions: req.mentions, reply_to: req.reply_to, reply_mention: req.reply_mention, access: Some(s.access.rules()) };
    let message = s
        .db
        .call(move |c| db::insert_message_with(c, id, &author, &caption, &ids, &draft, &owner))
        .await?
        .map_err(crate::attachments::link_err)?;
    s.send_channel(id, &ServerEvent::MessageCreated { message: message.clone() });
    Ok(Json(message))
}

/// A LiveKit join token for a voice room. v0.9: none for a private channel without access (`404`
/// like an unknown channel), so LiveKit never admits them (and `access::enforce` /
/// `voice_guard.rs` remove a session with a token from before they lost access).
async fn voice_join(
    State(s): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<ChannelId>,
    Query(q): Query<E2eeQuery>,
    LkUrl(livekit_url): LkUrl,
) -> ApiResult<VoiceJoinResponse> {
    let e2ee = match s.channel_for(user.id, id) {
        None => return Err(ApiErr::new(StatusCode::NOT_FOUND, "channel not found")),
        Some(c) if c.kind != ChannelKind::Voice => return Err(ApiErr::new(StatusCode::BAD_REQUEST, "not a voice channel")),
        Some(c) => c.e2ee,
    };
    // v1.1 (E1): an end-to-end encrypted room only lets in apps that encrypt (an older one would
    // send its media in the clear and fail to decode everybody else's).
    if e2ee && !q.capable() {
        tracing::info!(user = %user.username, channel = id, "voice join refused: the room is end-to-end encrypted and the app cannot encrypt");
        return Err(ApiErr::new(StatusCode::FORBIDDEN, ERR_E2EE_REQUIRED));
    }
    let room = format!("voice-{id}");
    let lk = &s.cfg.livekit;
    // A server-muted member (item 12) gets a token that does not allow the microphone at all.
    let server_muted = s.is_server_muted(user.id);
    let token = livekit_token::join_token_with(&lk.api_key, &lk.api_secret, &room, &user.id.to_string(), &user.username, now_ms() / 1000, server_muted);
    // v1.1 (E1): with the mode it was given for (a join racing the encryption toggle is removed).
    s.voice_guard.issued_voice(user.id, id, e2ee);
    tracing::info!(user = %user.username, room, url = %livekit_url, server_muted, e2ee, "voice join");
    Ok(Json(VoiceJoinResponse { livekit_url, livekit_token: token, room, e2ee }))
}

/// v1.1 (E1): `?e2ee=1` on a join token request (a voice channel, a DM call): the app can encrypt
/// the room's media end to end.
#[derive(Debug, Default, Deserialize)]
pub struct E2eeQuery {
    #[serde(default)]
    e2ee: Option<String>,
}

impl E2eeQuery {
    pub fn capable(&self) -> bool {
        self.e2ee.as_deref() == Some("1")
    }
}

#[derive(Deserialize)]
struct PreviewQuery {
    url: String,
}

/// `GET /api/link-preview?url=`: always 200 with a [`LinkPreview`] for a valid http(s) URL, even
/// when nothing could be fetched (then it is empty), so the answer never reveals whether an
/// internal address exists. 400 = not an http(s) URL, 429 = too many fresh lookups.
async fn link_preview(State(s): State<AppState>, AuthUser(user): AuthUser, Query(q): Query<PreviewQuery>) -> ApiResult<LinkPreview> {
    let url = LinkPreviews::parse_url(&q.url).ok_or_else(|| ApiErr::new(StatusCode::BAD_REQUEST, "not an http(s) url"))?;
    match s.previews.get(user.id, &url).await {
        Ok(row) => Ok(Json(LinkPreviews::to_wire(&row))),
        Err(RateLimited) => Err(ApiErr::new(StatusCode::TOO_MANY_REQUESTS, "too many link previews, try again shortly")),
    }
}

/// `GET /api/link-preview/image?url=`: the cached thumbnail of an already-fetched preview (JPEG).
/// It never fetches anything: only images the hub itself downloaded, checked and re-encoded.
async fn link_preview_image(State(s): State<AppState>, _: AuthUser, Query(q): Query<PreviewQuery>) -> Result<Response, ApiErr> {
    let url = LinkPreviews::parse_url(&q.url).ok_or_else(|| ApiErr::new(StatusCode::BAD_REQUEST, "not an http(s) url"))?;
    let Some(jpeg) = s.previews.image(&url).await else {
        return Err(ApiErr::new(StatusCode::NOT_FOUND, "no thumbnail"));
    };
    Ok((
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "private, max-age=3600"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        jpeg,
    )
        .into_response())
}

/// The LiveKit URL for a client that reached the hub with these headers: `ws://<host>:<signal
/// port>`, IPv6 hosts in brackets (`ws://[2001:db8::1]:7880`). A v1.0.1 app connects to it as
/// given (its LiveKit SDK and its own port check both take a bracketed IPv6 host), and LiveKit
/// listens on IPv6 as well as IPv4 by itself.
///
/// v1.1 (T0): over TLS, `wss://<host>:<hub port>`: the hub's own port (the one the client used,
/// from `Host`, else the configured one), where `serve.rs` splices `/rtc` to LiveKit. The app
/// reaches it through its pinned TLS; a plain client keeps the direct `ws://` of before.
pub(crate) fn livekit_url(headers: &header::HeaderMap, cfg: &crate::config::Config, transport: crate::tls::Transport) -> String {
    let host = livekit_host(headers, &cfg.public_host);
    if transport == crate::tls::Transport::Tls {
        let port = headers.get(header::HOST).and_then(|v| v.to_str().ok()).and_then(host_port).or_else(|| cfg.bind.rsplit_once(':').and_then(|(_, p)| p.trim().parse().ok())).unwrap_or(7890);
        return format!("wss://{host}:{port}");
    }
    format!("ws://{host}:{}", cfg.livekit.signal_port)
}

/// The port of a `Host` header (`a:1`, `[::1]:2`); `None` without one.
fn host_port(host: &str) -> Option<u16> {
    let rest = match host.strip_prefix('[') {
        Some(r) => r.split_once(']')?.1.strip_prefix(':')?,
        None => {
            let (h, p) = host.rsplit_once(':')?;
            if h.contains(':') {
                return None;
            }
            p
        }
    };
    rest.parse().ok()
}

/// The LiveKit URL for this request ([`livekit_url`]): what a voice join or a call answers with.
pub struct LkUrl(pub String);

impl FromRequestParts<AppState> for LkUrl {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let transport = parts.extensions.get::<crate::tls::Transport>().copied().unwrap_or_default();
        Ok(LkUrl(livekit_url(&parts.headers, &state.cfg, transport)))
    }
}

/// LiveKit runs on the same machine as the hub, so the host the client used to reach the hub
/// (LAN IP, public IP or DDNS name) is the right one for LiveKit too. That keeps LAN clients
/// off NAT hairpinning and survives public-IP changes. `public_host` is only the fallback.
pub(crate) fn livekit_host(headers: &header::HeaderMap, fallback: &str) -> String {
    let from_header = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(strip_port)
        .filter(|h| !h.is_empty());
    let host = from_header.unwrap_or(fallback);
    if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() }
}

/// `example.com:7890` -> `example.com`, `[::1]:7890` -> `[::1]`, `::1` stays as is.
fn strip_port(host: &str) -> &str {
    if let Some(end) = host.strip_prefix('[').and_then(|h| h.find(']')) {
        return &host[..end + 2];
    }
    match host.rsplit_once(':') {
        Some((h, port)) if !h.contains(':') && port.chars().all(|c| c.is_ascii_digit()) => h,
        _ => host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_host(h: &str) -> header::HeaderMap {
        let mut m = header::HeaderMap::new();
        m.insert(header::HOST, h.parse().unwrap());
        m
    }

    #[test]
    fn livekit_host_follows_the_hub_address() {
        assert_eq!(livekit_host(&with_host("192.168.1.50:7890"), "1.2.3.4"), "192.168.1.50");
        assert_eq!(livekit_host(&with_host("voxhub.example.com:7890"), "1.2.3.4"), "voxhub.example.com");
        assert_eq!(livekit_host(&with_host("example.com"), "1.2.3.4"), "example.com");
        assert_eq!(livekit_host(&with_host("[::1]:7890"), "1.2.3.4"), "[::1]");
        assert_eq!(livekit_host(&header::HeaderMap::new(), "1.2.3.4"), "1.2.3.4");
        assert_eq!(livekit_host(&header::HeaderMap::new(), "::1"), "[::1]");
    }

    /// v1.1 (T0): over TLS, LiveKit is reached through the hub's own port (`wss://`), the port the
    /// client used; in the clear it stays the direct `ws://` signal port of before.
    #[test]
    fn livekit_url_over_tls_uses_the_hub_port() {
        use crate::tls::Transport::{Plain, Tls};
        let cfg = crate::config::Config::default();
        assert_eq!(livekit_url(&with_host("192.0.2.10:7890"), &cfg, Tls), "wss://192.0.2.10:7890");
        assert_eq!(livekit_url(&with_host("192.0.2.10:7890"), &cfg, Plain), "ws://192.0.2.10:7880");
        assert_eq!(livekit_url(&with_host("example.org:9000"), &cfg, Tls), "wss://example.org:9000", "a router that remaps the port");
        assert_eq!(livekit_url(&with_host("[2001:db8::1]:7997"), &cfg, Tls), "wss://[2001:db8::1]:7997");
        assert_eq!(livekit_url(&with_host("example.org"), &cfg, Tls), "wss://example.org:7890", "no port: the configured one");
        assert_eq!(livekit_url(&header::HeaderMap::new(), &cfg, Tls), "wss://127.0.0.1:7890");
        let mut cfg = cfg;
        cfg.bind = "127.0.0.1:7997".into();
        assert_eq!(livekit_url(&with_host("localhost"), &cfg, Tls), "wss://localhost:7997");
        assert_eq!(host_port("[::1]:5"), Some(5));
        assert_eq!(host_port("::1"), None);
    }

    /// v1.0.2: a client that reached the hub over IPv6 gets LiveKit at the same IPv6 address.
    #[test]
    fn livekit_url_keeps_ipv6_hosts_in_brackets() {
        let livekit_url = |h: &header::HeaderMap, c: &crate::config::Config| livekit_url(h, c, crate::tls::Transport::Plain);
        let cfg = crate::config::Config::default();
        assert_eq!(livekit_url(&with_host("[2001:db8::1]:7890"), &cfg), "ws://[2001:db8::1]:7880");
        assert_eq!(livekit_url(&with_host("[2001:db8::1]"), &cfg), "ws://[2001:db8::1]:7880");
        assert_eq!(livekit_url(&with_host("[::1]:7997"), &cfg), "ws://[::1]:7880");
        assert_eq!(livekit_url(&with_host("192.0.2.10:7890"), &cfg), "ws://192.0.2.10:7880");
        let mut cfg = cfg;
        cfg.public_host = "2001:db8::5".into();
        cfg.livekit.signal_port = 7970;
        assert_eq!(livekit_url(&header::HeaderMap::new(), &cfg), "ws://[2001:db8::5]:7970");
        // What the app's LiveKit SDK does with it: a plain IPv6 host and the signal port.
        let u = url::Url::parse(&livekit_url(&with_host("[2001:db8::1]:7890"), &cfg)).unwrap();
        assert_eq!((u.host(), u.port()), (Some(url::Host::Ipv6("2001:db8::1".parse().unwrap())), Some(7970)));
    }
}
