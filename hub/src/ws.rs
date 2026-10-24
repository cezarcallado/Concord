//! WebSocket endpoint: snapshot on connect, chat, voice state, presence, keepalive.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message as WsMsg, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Extension, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tokio::sync::broadcast::{error::RecvError, Receiver};
use tokio::time::Instant;
use vox_proto::{ChannelId, ChannelKind, ClientEvent, ServerEvent, User, VoiceMember, ERR_TOO_MANY_MENTIONS, MENTIONS_WIRE_MAX};

use crate::access::{self, VoiceView};
use crate::api::{user_for_token_hash, ApiErr};
use crate::db::{self, UserRow};
use crate::state::{self, AppState, Out};
use crate::util::sha256_hex;

const PING_EVERY: Duration = Duration::from_secs(15);
const DEAD_AFTER: Duration = Duration::from_secs(45);
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const MAX_CONTENT_CHARS: usize = 4000;

#[derive(Deserialize)]
pub struct WsQuery {
    token: Option<String>,
}

pub async fn handler(
    State(s): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<WsQuery>,
    slot: Option<Extension<crate::serve::ConnSlot>>,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(token) = q.token else {
        return ApiErr::new(StatusCode::UNAUTHORIZED, "missing token").into_response();
    };
    let hash = sha256_hex(&token);
    let row = match user_for_token_hash(&s, hash.clone()).await {
        Ok(u) => u,
        Err(e) => return e.into_response(),
    };
    // The app on the server PC reconnecting with a session that predates the owner (the update
    // restarted the hub under it): the same "quem hospeda vira dono" as a login.
    if s.connections(row.id) >= state::MAX_WS_PER_USER {
        tracing::warn!(%peer, user = %row.username, "ws refused: too many connections of this user");
        return ApiErr::new(StatusCode::TOO_MANY_REQUESTS, "too many connections").into_response();
    }
    if let Err(e) = crate::owner::claim_from_host_pc(&s, &row, peer.ip(), &headers, Some(&hash)).await {
        tracing::error!("claiming the server for {}: {e:#}", row.username);
    }
    // The connection's slot in the listener's caps lives as long as the socket.
    let slot = slot.map(|Extension(s)| s);
    ws.max_message_size(64 * 1024).on_upgrade(move |socket| async move {
        let _slot = slot;
        run(socket, s, row, hash, peer).await
    })
}

async fn run(mut socket: WebSocket, s: AppState, row: UserRow, session_hash: String, peer: SocketAddr) {
    let owner = s.owner();
    let user = row.user(&owner);
    // Profiles: the presence and status the person left (a status that ran out is gone).
    let id = user.id;
    let presence = match s.db.call(move |c| crate::profile::load_entry(c, id, crate::util::now_ms())).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("loading the presence of {}: {e:#}", user.username);
            Default::default()
        }
    };
    let (ready, mut rx) = s.connect(&user, s.account(&row), presence);
    // DM calls: count the connection, ring again if a call to this person is ringing.
    crate::calls::on_connect(&s, user.id);
    tracing::info!(%peer, user = %user.username, role = ?user.role, "ws connected");
    let why = session(&mut socket, &s, &user, &session_hash, ready, &mut rx).await;
    s.disconnect(user.id);
    crate::calls::on_disconnect(&s, user.id);
    tracing::info!(%peer, user = %user.username, why, "ws disconnected");
}

async fn send(socket: &mut WebSocket, msg: WsMsg) -> bool {
    matches!(tokio::time::timeout(SEND_TIMEOUT, socket.send(msg)).await, Ok(Ok(())))
}

async fn send_event(socket: &mut WebSocket, ev: &ServerEvent) -> bool {
    match serde_json::to_string(ev) {
        Ok(t) => send(socket, WsMsg::Text(t.into())).await,
        Err(_) => true,
    }
}

fn err(msg: &str) -> ServerEvent {
    ServerEvent::Error { error: msg.into() }
}

/// Drives one connection until it ends; returns the reason for the log.
async fn session(
    socket: &mut WebSocket,
    s: &AppState,
    user: &User,
    session_hash: &str,
    ready: ServerEvent,
    rx: &mut Receiver<Out>,
) -> &'static str {
    // What this connection was told about the channels (v0.9), to tell it again only on a change.
    let mut view = match &ready {
        ServerEvent::Ready { channels, .. } => channels.clone(),
        _ => Vec::new(),
    };
    if !send_event(socket, &ready).await {
        return "send failed";
    }
    let mut shutdown = s.shutdown.clone();
    let mut last_seen = Instant::now();
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    // A `VoiceState` over the rate limit waits here (only the newest one) until it may go.
    let mut voice = PendingVoice { ev: None, at: Instant::now() };
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(Ok(msg)) = incoming else { return "closed" };
                last_seen = Instant::now();
                match msg {
                    WsMsg::Text(t) => match serde_json::from_str::<ClientEvent>(&t) {
                        Ok(ev @ ClientEvent::VoiceState { .. }) => {
                            if let Some(reply) = voice.offer(s, user, ev).await {
                                if !send_event(socket, &reply).await { return "send failed" }
                            }
                        }
                        Ok(ev) => {
                            if let Some(reply) = handle(s, user, ev).await {
                                if !send_event(socket, &reply).await { return "send failed" }
                            }
                        }
                        Err(_) => {
                            if !send_event(socket, &err("invalid event")).await { return "send failed" }
                        }
                    },
                    WsMsg::Close(_) => return "closed",
                    _ => {} // ping/pong answered by axum; binary ignored
                }
            }
            ev = rx.recv() => match ev {
                Ok(Out::All(text)) => if !send(socket, WsMsg::Text(text.as_ref().into())).await { return "send failed" },
                Ok(Out::To(to, text)) => {
                    if to == user.id && !send(socket, WsMsg::Text(text.as_ref().into())).await { return "send failed" }
                }
                Ok(Out::Many(to, text)) => {
                    if to.contains(&user.id) && !send(socket, WsMsg::Text(text.as_ref().into())).await { return "send failed" }
                }
                // v0.9: channel events only to the people who may see the channel, checked now.
                Ok(Out::Channel { ch, to, text }) => {
                    let mine = to.as_ref().is_none_or(|t| t.contains(&user.id)) && s.can_see(user.id, ch);
                    if mine && !send(socket, WsMsg::Text(text.as_ref().into())).await { return "send failed" }
                }
                Ok(Out::Voice { member, ch, prev, full, left }) => {
                    let text = match access::voice_view(&s.access.rules(), user.id, member, ch, prev) {
                        VoiceView::Full => Some(full),
                        VoiceView::Left => Some(left),
                        VoiceView::Nothing => None,
                    };
                    if let Some(text) = text {
                        if !send(socket, WsMsg::Text(text.as_ref().into())).await { return "send failed" }
                    }
                }
                Ok(Out::Access) => {
                    if !send_view(socket, s, user.id, &mut view).await { return "send failed" }
                }
                Ok(Out::Kick { user: who, keep }) => {
                    if who == user.id && keep.as_deref() != Some(session_hash) {
                        let _ = send(socket, WsMsg::Close(Some(CloseFrame { code: 4001, reason: "session revoked".into() }))).await;
                        return "session revoked";
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(user = %user.username, skipped = n, "ws client too slow, dropping");
                    return "lagged";
                }
                Err(RecvError::Closed) => return "hub closed",
            },
            _ = tokio::time::sleep_until(voice.at), if voice.ev.is_some() => {
                if let Some(ev) = voice.ev.take() {
                    if let Some(reply) = voice.offer(s, user, ev).await {
                        if !send_event(socket, &reply).await { return "send failed" }
                    }
                }
            }
            _ = ping.tick() => {
                if last_seen.elapsed() > DEAD_AFTER { return "timeout" }
                if !send(socket, WsMsg::Ping(Vec::new().into())).await { return "send failed" }
            }
            _ = state::wait_shutdown(&mut shutdown) => {
                let _ = send(socket, WsMsg::Close(Some(CloseFrame { code: 1001, reason: "server shutting down".into() }))).await;
                return "server shutdown";
            }
        }
    }
}

/// The channels `user` may see changed, maybe (`Out::Access`): when this connection's list is not
/// what it was told, it gets the new `ChannelsChanged`; the voice members of a channel it can no
/// longer see leave voice for it, those of a channel it can see now join. Returns false when
/// sending failed.
async fn send_view(socket: &mut WebSocket, s: &AppState, user: vox_proto::UserId, view: &mut Vec<vox_proto::Channel>) -> bool {
    let now = s.visible_channels(user);
    if now == *view {
        return true;
    }
    let had: HashSet<ChannelId> = view.iter().map(|c| c.id).collect();
    let has: HashSet<ChannelId> = now.iter().map(|c| c.id).collect();
    let mut members = s.voice_members();
    members.sort_by_key(|m| m.user.id);
    let in_channels = |set: &HashSet<ChannelId>, other: &HashSet<ChannelId>| -> Vec<VoiceMember> {
        members.iter().filter(|m| m.user.id != user && m.channel_id.is_some_and(|c| set.contains(&c) && !other.contains(&c))).cloned().collect()
    };
    let (gone, came) = (in_channels(&had, &has), in_channels(&has, &had));
    for m in gone {
        if !send_event(socket, &ServerEvent::VoiceStateChanged { member: state::left(m) }).await {
            return false;
        }
    }
    *view = now.clone();
    if !send_event(socket, &ServerEvent::ChannelsChanged { channels: now }).await {
        return false;
    }
    for m in came {
        if !send_event(socket, &ServerEvent::VoiceStateChanged { member: m }).await {
            return false;
        }
    }
    true
}

/// `VoiceState` frames of one connection: a frame that changes nothing is dropped; over the
/// per-person rate limit only the newest frame is kept and applied once the limit allows.
struct PendingVoice {
    ev: Option<ClientEvent>,
    at: Instant,
}

impl PendingVoice {
    async fn offer(&mut self, s: &AppState, user: &User, ev: ClientEvent) -> Option<ServerEvent> {
        let ClientEvent::VoiceState { channel_id, muted, deafened, streaming, camera, audio_only } = ev else {
            return None;
        };
        if self.ev.is_some() {
            self.ev = Some(ev); // newest wins; the timer is already set
            return None;
        }
        if s.voice_unchanged(user.id, channel_id, muted, deafened, streaming, camera, audio_only) {
            return None;
        }
        if let Err(wait) = s.take_voice_token(user.id) {
            tracing::debug!(user = %user.username, "voice state held back (rate limited)");
            self.ev = Some(ev);
            self.at = Instant::now() + wait;
            return None;
        }
        handle(s, user, ev).await
    }
}

/// Handle one client event; returns an event for the sender only, if any.
async fn handle(s: &AppState, user: &User, ev: ClientEvent) -> Option<ServerEvent> {
    match ev {
        ClientEvent::Ping { nonce } => Some(ServerEvent::Pong { nonce }),
        ClientEvent::VoiceState { channel_id, muted, deafened, streaming, camera, audio_only } => {
            tracing::debug!(user = %user.username, ?channel_id, muted, deafened, streaming, camera, audio_only, "voice state");
            let before = s.voice_of(user.id).and_then(|m| m.channel_id);
            if let Err(e) = s.set_voice(user, channel_id, muted, deafened, streaming, camera, audio_only) {
                return Some(err(e));
            }
            if let Some(ch) = channel_id.filter(|ch| Some(*ch) != before) {
                if s.is_server_muted(user.id) {
                    crate::admin::enforce_mute_on_join(s, user.id, crate::voice_guard::voice_room(ch));
                }
                // Joining a voice channel leaves a DM call (as on Discord).
                crate::calls::on_voice_join(s, user.id, ch).await;
            }
            None
        }
        ClientEvent::WatchState { watching } => {
            s.set_watching(user, &watching);
            None
        }
        ClientEvent::PlaySound { sound_id } => crate::sounds::play(s, user, sound_id).await,
        ClientEvent::StopSound => crate::sounds::stop(s, user),
        ClientEvent::SendMessage { channel_id, content, mentions, reply_to, reply_mention } => {
            let content = content.trim();
            if content.is_empty() || content.chars().count() > MAX_CONTENT_CHARS {
                return Some(err("message must be 1-4000 characters"));
            }
            if mentions.len() > MENTIONS_WIRE_MAX {
                return Some(err(ERR_TOO_MANY_MENTIONS));
            }
            // A private channel without access is as unknown as a missing one (v0.9).
            if !matches!(s.channel_for(user.id, channel_id), Some(c) if c.kind == ChannelKind::Text) {
                return Some(err("unknown text channel"));
            }
            if !s.allow_message(user.id) {
                return Some(err("rate limited: slow down"));
            }
            // The author as everybody sees them now (a role may have changed since connecting).
            let (u, text, owner) = (s.current_user(user), content.to_string(), s.owner());
            let draft = crate::chat::Draft { mentions, reply_to, reply_mention, access: Some(s.access.rules()) };
            match s.db.call(move |c| db::insert_message_with(c, channel_id, &u, &text, &[], &draft, &owner)).await {
                Ok(Ok(message)) => {
                    s.send_channel(channel_id, &ServerEvent::MessageCreated { message });
                    None
                }
                Ok(Err(_)) => Some(err("could not save message")), // (no attachments to check)
                Err(e) => {
                    tracing::error!("saving message: {e:#}");
                    Some(err("could not save message"))
                }
            }
        }
    }
}
