//! Server soundboard (hub side): storage, HTTP handlers, rate limits and the `PlaySound` event.
//!
//! The wire contract is documented in `vox_proto::sound`. The hub stores one canonical WAV per
//! sound (48 kHz, 16-bit PCM, mono or stereo, at most 5 s) as a BLOB in `sound_data`, kept apart
//! from the small `sounds` rows so listing never reads audio. Nothing here trusts the uploader:
//! the bytes are parsed again and re-encoded, and permissions come from the caller's effective
//! role.
//!
//! Schema: two additive tables created with `IF NOT EXISTS` (no existing table is touched), so a
//! v0.4.1 database opens as it is and an older hub still runs on a database that has them.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, JsonRejection};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use vox_proto::*;

use crate::api::{ApiErr, AuthUser, Authed};
use crate::db;
use crate::state::AppState;
use crate::util::now_ms;

type ApiResult<T> = Result<Json<T>, ApiErr>;

/// One person plays at most one sound per this long.
pub const PLAY_GAP_PER_USER: Duration = Duration::from_secs(2);
/// A voice channel hears at most [`PLAY_MAX_PER_ROOM`] sounds in this window.
pub const PLAY_ROOM_WINDOW: Duration = Duration::from_secs(10);
pub const PLAY_MAX_PER_ROOM: usize = 5;
/// v1.1: a stop is relayed only this long after the player's last play (the longest clip, plus
/// a margin for a clip that started late on a slow download).
pub const STOP_WINDOW: Duration = Duration::from_millis(SOUND_MAX_MS as u64 + 3000);
/// Upload token bucket per person: a burst of this many, then one more every [`UPLOAD_REFILL`].
pub const UPLOAD_BURST: f64 = 5.0;
pub const UPLOAD_REFILL: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------- storage

pub fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sounds (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            emoji TEXT NOT NULL DEFAULT '',
            uploader_id INTEGER NOT NULL REFERENCES users(id),
            created_at_ms INTEGER NOT NULL,
            duration_ms INTEGER NOT NULL,
            size INTEGER NOT NULL,
            hash TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS sound_data (
            sound_id INTEGER PRIMARY KEY REFERENCES sounds(id) ON DELETE CASCADE,
            data BLOB NOT NULL
        );",
    )
}

struct SoundRow {
    id: SoundId,
    name: String,
    emoji: String,
    uploader_id: UserId,
    created_at_ms: i64,
    duration_ms: u32,
    size: u32,
    hash: String,
}

const SOUND_COLS: &str = "id, name, emoji, uploader_id, created_at_ms, duration_ms, size, hash";

fn sound_row(r: &rusqlite::Row) -> rusqlite::Result<SoundRow> {
    Ok(SoundRow {
        id: r.get(0)?,
        name: r.get(1)?,
        emoji: r.get(2)?,
        uploader_id: r.get(3)?,
        created_at_ms: r.get(4)?,
        duration_ms: r.get(5)?,
        size: r.get(6)?,
        hash: r.get(7)?,
    })
}

/// Rows to wire sounds, uploaders as everybody currently sees them (role, picture).
fn with_uploaders(conn: &Connection, rows: Vec<SoundRow>, owner: &str) -> rusqlite::Result<Vec<Sound>> {
    let mut people: HashMap<UserId, User> = HashMap::new();
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let uploader = match people.get(&r.uploader_id) {
            Some(u) => u.clone(),
            None => {
                let u = db::get_user(conn, r.uploader_id)?.map(|u| u.user(owner)).unwrap_or_else(|| User::new(r.uploader_id, "?"));
                people.insert(r.uploader_id, u.clone());
                u
            }
        };
        out.push(Sound {
            id: r.id,
            name: r.name,
            emoji: r.emoji,
            uploader,
            created_at_ms: r.created_at_ms,
            duration_ms: r.duration_ms,
            size: r.size,
            hash: r.hash,
        });
    }
    Ok(out)
}

/// Every sound, oldest first.
pub fn list_sounds(conn: &Connection, owner: &str) -> rusqlite::Result<Vec<Sound>> {
    let rows = {
        let mut st = conn.prepare(&format!("SELECT {SOUND_COLS} FROM sounds ORDER BY id"))?;
        let rows = st.query_map([], sound_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    with_uploaders(conn, rows, owner)
}

pub fn get_sound(conn: &Connection, id: SoundId, owner: &str) -> rusqlite::Result<Option<Sound>> {
    let row = conn.query_row(&format!("SELECT {SOUND_COLS} FROM sounds WHERE id = ?"), [id], sound_row).optional()?;
    Ok(match row {
        Some(r) => with_uploaders(conn, vec![r], owner)?.pop(),
        None => None,
    })
}

pub fn sound_exists(conn: &Connection, id: SoundId) -> rusqlite::Result<bool> {
    Ok(conn.query_row("SELECT 1 FROM sounds WHERE id = ?", [id], |_| Ok(())).optional()?.is_some())
}

/// The stored clip.
pub fn sound_data(conn: &Connection, id: SoundId) -> rusqlite::Result<Option<(Vec<u8>, String)>> {
    conn.query_row("SELECT d.data, s.hash FROM sound_data d JOIN sounds s ON s.id = d.sound_id WHERE d.sound_id = ?", [id], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })
    .optional()
}

/// `Ok(None)`: the server already holds [`SOUND_MAX_COUNT`] sounds (checked in the same
/// transaction as the insert).
#[allow(clippy::too_many_arguments)]
pub fn insert_sound(
    conn: &mut Connection,
    name: &str,
    emoji: &str,
    uploader: UserId,
    duration_ms: u32,
    hash: &str,
    data: &[u8],
) -> rusqlite::Result<Option<SoundId>> {
    let tx = conn.transaction()?;
    let n: i64 = tx.query_row("SELECT COUNT(*) FROM sounds", [], |r| r.get(0))?;
    if n >= SOUND_MAX_COUNT as i64 {
        return Ok(None);
    }
    // A fresh id: a deleted sound's id never comes back (the apps cache sounds by id).
    let id = crate::ids::insert_with(&tx, crate::ids::Seq::Sounds, |id| {
        tx.execute(
            "INSERT INTO sounds (id, name, emoji, uploader_id, created_at_ms, duration_ms, size, hash) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![id, name, emoji, uploader, now_ms(), duration_ms, data.len() as i64, hash],
        )?;
        Ok(id)
    })?;
    tx.execute("INSERT INTO sound_data (sound_id, data) VALUES (?, ?)", params![id, data])?;
    tx.commit()?;
    Ok(Some(id))
}

pub fn rename_sound(conn: &Connection, id: SoundId, name: &str, emoji: Option<&str>) -> rusqlite::Result<bool> {
    let n = match emoji {
        Some(e) => conn.execute("UPDATE sounds SET name = ?, emoji = ? WHERE id = ?", params![name, e, id])?,
        None => conn.execute("UPDATE sounds SET name = ? WHERE id = ?", params![name, id])?,
    };
    Ok(n > 0)
}

pub fn delete_sound(conn: &mut Connection, id: SoundId) -> rusqlite::Result<bool> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM sound_data WHERE sound_id = ?", [id])?;
    let n = tx.execute("DELETE FROM sounds WHERE id = ?", [id])?;
    tx.commit()?;
    Ok(n > 0)
}

// ---------------------------------------------------------------- rate limits

/// Upload and play limits (in memory; they reset with the hub, which is fine for brakes).
#[derive(Default)]
pub struct SoundState {
    uploads: Mutex<HashMap<UserId, (f64, Instant)>>,
    plays: Mutex<Plays>,
}

#[derive(Default)]
struct Plays {
    last_by_user: HashMap<UserId, Instant>,
    /// v1.1: people who played lately and may still stop it (one stop per play).
    stoppable: HashMap<UserId, Instant>,
    by_room: HashMap<ChannelId, VecDeque<Instant>>,
}

/// Why a play was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayDenied {
    User,
    Room,
}

impl SoundState {
    /// One upload token (see [`UPLOAD_BURST`]).
    pub fn allow_upload(&self, user: UserId) -> bool {
        self.allow_upload_at(user, Instant::now())
    }

    fn allow_upload_at(&self, user: UserId, now: Instant) -> bool {
        let mut g = self.uploads.lock().unwrap_or_else(|e| e.into_inner());
        let b = g.entry(user).or_insert((UPLOAD_BURST, now));
        b.0 = (b.0 + now.saturating_duration_since(b.1).as_secs_f64() / UPLOAD_REFILL.as_secs_f64()).min(UPLOAD_BURST);
        b.1 = now;
        if b.0 >= 1.0 {
            b.0 -= 1.0;
            true
        } else {
            false
        }
    }

    /// May `user` play in `channel` now? Records the play when yes.
    pub fn allow_play(&self, user: UserId, channel: ChannelId) -> Result<(), PlayDenied> {
        self.allow_play_at(user, channel, Instant::now())
    }

    fn allow_play_at(&self, user: UserId, channel: ChannelId, now: Instant) -> Result<(), PlayDenied> {
        let mut g = self.plays.lock().unwrap_or_else(|e| e.into_inner());
        if g.last_by_user.get(&user).is_some_and(|t| now.saturating_duration_since(*t) < PLAY_GAP_PER_USER) {
            return Err(PlayDenied::User);
        }
        let room = g.by_room.entry(channel).or_default();
        while room.front().is_some_and(|t| now.saturating_duration_since(*t) >= PLAY_ROOM_WINDOW) {
            room.pop_front();
        }
        if room.len() >= PLAY_MAX_PER_ROOM {
            return Err(PlayDenied::Room);
        }
        room.push_back(now);
        g.last_by_user.insert(user, now);
        g.stoppable.insert(user, now);
        // Keep the maps small: forget people who have been quiet for a while.
        if g.last_by_user.len() > 256 {
            g.last_by_user.retain(|_, t| now.saturating_duration_since(*t) < PLAY_ROOM_WINDOW);
        }
        if g.stoppable.len() > 256 {
            g.stoppable.retain(|_, t| now.saturating_duration_since(*t) < STOP_WINDOW);
        }
        Ok(())
    }

    /// May `user` stop their sound now? Only while one they played could still be playing, and
    /// once per play (so a stop is never a way to flood a room).
    pub fn allow_stop(&self, user: UserId) -> bool {
        self.allow_stop_at(user, Instant::now())
    }

    fn allow_stop_at(&self, user: UserId, now: Instant) -> bool {
        let mut g = self.plays.lock().unwrap_or_else(|e| e.into_inner());
        g.stoppable.remove(&user).is_some_and(|t| now.saturating_duration_since(t) < STOP_WINDOW)
    }
}

// ---------------------------------------------------------------- playing

/// `ClientEvent::PlaySound`: tell the members of the player's voice channel. Returns the error
/// for the sender when refused.
pub async fn play(s: &AppState, user: &User, sound_id: SoundId) -> Option<ServerEvent> {
    let err = |m: &str| Some(ServerEvent::Error { error: m.into() });
    let Some(channel) = s.voice_of(user.id).and_then(|m| m.channel_id) else {
        return err(ERR_SOUND_NOT_IN_VOICE);
    };
    // A server-muted member may not get around the mute with the soundboard.
    if s.is_server_muted(user.id) {
        return err(ERR_SOUND_SERVER_MUTED);
    }
    if let Err(why) = s.sounds.allow_play(user.id, channel) {
        tracing::debug!(user = %user.username, channel, ?why, "sound play refused (rate limit)");
        return err(ERR_SOUND_RATE_LIMITED);
    }
    match s.db.call(move |c| sound_exists(c, sound_id)).await {
        Ok(true) => {}
        Ok(false) => return err(ERR_SOUND_UNKNOWN),
        Err(e) => {
            tracing::error!("sound lookup: {e:#}");
            return err("could not play the sound");
        }
    }
    let members: Vec<UserId> = s.voice_members().into_iter().filter(|m| m.channel_id == Some(channel)).map(|m| m.user.id).collect();
    tracing::info!(user = %user.username, sound_id, channel, listeners = members.len(), "sound played");
    // Only the room's members, and only those who may see the channel (v0.9: private channels).
    s.send_channel_to(channel, &members, &ServerEvent::SoundPlayed { sound_id, by: s.current_user(user), channel_id: channel, stoppable: true });
    None
}

/// `ClientEvent::StopSound` (v1.1): tell the player's voice channel to stop the sounds they
/// played. Silently ignored when there is nothing of theirs to stop (no error: the client sends
/// it whenever its "Parar" is pressed).
pub fn stop(s: &AppState, user: &User) -> Option<ServerEvent> {
    let channel = s.voice_of(user.id).and_then(|m| m.channel_id)?;
    if !s.sounds.allow_stop(user.id) {
        return None;
    }
    let members: Vec<UserId> = s.voice_members().into_iter().filter(|m| m.channel_id == Some(channel)).map(|m| m.user.id).collect();
    tracing::info!(user = %user.username, channel, listeners = members.len(), "sound stopped");
    s.send_channel_to(channel, &members, &ServerEvent::SoundStopped { by: user.id, channel_id: channel });
    None
}

// ---------------------------------------------------------------- HTTP

fn not_found() -> ApiErr {
    ApiErr::new(StatusCode::NOT_FOUND, "sound not found")
}

fn forbidden() -> ApiErr {
    ApiErr::new(StatusCode::FORBIDDEN, "only the person who added this sound, or a moderator, can change it")
}

/// Tell everybody the list changed.
async fn announce(s: &AppState) -> Result<(), ApiErr> {
    let owner = s.owner().to_string();
    let sounds = s.db.call(move |c| list_sounds(c, &owner)).await?;
    s.broadcast(&ServerEvent::SoundsChanged { sounds });
    Ok(())
}

async fn audit(s: &AppState, actor: UserId, action: &'static str, target: UserId, detail: String) {
    tracing::info!(actor, action, target, detail, "audit");
    if let Err(e) = s.db.call(move |c| db::audit(c, Some(actor), action, Some(target), &detail)).await {
        tracing::error!("audit log write failed: {e:#}");
    }
}

/// `GET /api/sounds`
pub async fn list(State(s): State<AppState>, _: AuthUser) -> ApiResult<Vec<Sound>> {
    let owner = s.owner().to_string();
    Ok(Json(s.db.call(move |c| list_sounds(c, &owner)).await?))
}

/// `GET /api/sounds/{id}/audio`
pub async fn audio(State(s): State<AppState>, _: AuthUser, Path(id): Path<SoundId>, headers: HeaderMap) -> Result<Response, ApiErr> {
    let (data, hash) = s.db.call(move |c| sound_data(c, id)).await?.ok_or_else(not_found)?;
    let etag = format!("\"{hash}\"");
    let cache = [(header::ETAG, etag.clone()), (header::CACHE_CONTROL, "private, max-age=31536000, immutable".to_string())];
    if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|t| t.trim() == etag)) {
        return Ok((StatusCode::NOT_MODIFIED, cache).into_response());
    }
    Ok((cache, [(header::CONTENT_TYPE, "audio/wav".to_string()), (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string())], data).into_response())
}

#[derive(Deserialize)]
pub struct UploadQuery {
    name: String,
    #[serde(default)]
    emoji: String,
}

/// `POST /api/sounds?name=&emoji=` with the WAV as the body.
pub async fn upload(
    State(s): State<AppState>,
    me: Authed,
    Query(q): Query<UploadQuery>,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult<Sound> {
    let bad = |m: &str| ApiErr::new(StatusCode::BAD_REQUEST, m);
    let name = clean_sound_name(&q.name).ok_or_else(|| bad(&format!("the name must be 1-{SOUND_NAME_MAX} characters")))?;
    let emoji = clean_sound_emoji(&q.emoji).ok_or_else(|| bad("invalid emoji"))?;
    let too_big = || ApiErr::new(StatusCode::PAYLOAD_TOO_LARGE, "the sound is over 1 MB");
    let body = body.map_err(|_| too_big())?;
    if body.len() > SOUND_MAX_UPLOAD_BYTES {
        return Err(too_big());
    }
    if !s.sounds.allow_upload(me.user.id) {
        tracing::warn!(user = %me.user.username, "sound upload refused (rate limit)");
        return Err(ApiErr::new(StatusCode::TOO_MANY_REQUESTS, "too many uploads, try again in a little while"));
    }
    // Parse again (never trust the client) and store a canonical re-encoding: no other chunk of
    // the upload survives.
    let info = parse_wav(&body).map_err(|e| bad(&e.to_string()))?;
    let stored = canonical_wav(info.channels, &wav_samples(&body, &info));
    let hash = hash_of(&stored);
    let (uploader, duration, size) = (me.user.id, info.duration_ms(), stored.len());
    let (n2, e2, h2) = (name.clone(), emoji.clone(), hash.clone());
    let id = s.db.call(move |c| insert_sound(c, &n2, &e2, uploader, duration, &h2, &stored)).await?;
    let Some(id) = id else {
        return Err(ApiErr::new(StatusCode::CONFLICT, format!("the soundboard is full ({SOUND_MAX_COUNT} sounds): delete one first")));
    };
    tracing::info!(user = %me.user.username, id, name, duration_ms = duration, size, "sound added");
    let owner = s.owner().to_string();
    let sound = s.db.call(move |c| get_sound(c, id, &owner)).await?.ok_or_else(not_found)?;
    announce(&s).await?;
    Ok(Json(sound))
}

/// The cache key of a clip: the first 16 hex digits of the SHA-256 of its bytes.
fn hash_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    crate::util::hex(&Sha256::digest(bytes)).chars().take(16).collect()
}

async fn editable(s: &AppState, me: &Authed, id: SoundId) -> Result<Sound, ApiErr> {
    let owner = s.owner().to_string();
    let sound = s.db.call(move |c| get_sound(c, id, &owner)).await?.ok_or_else(not_found)?;
    if !may_edit_sound(me.role, me.user.id, sound.uploader.id) {
        return Err(forbidden());
    }
    Ok(sound)
}

/// `POST /api/sounds/{id}/rename`
pub async fn rename(
    State(s): State<AppState>,
    me: Authed,
    Path(id): Path<SoundId>,
    body: Result<Json<SoundRenameRequest>, JsonRejection>,
) -> ApiResult<Sound> {
    let Json(req) = body?;
    let sound = editable(&s, &me, id).await?;
    let name = clean_sound_name(&req.name).ok_or_else(|| ApiErr::new(StatusCode::BAD_REQUEST, format!("the name must be 1-{SOUND_NAME_MAX} characters")))?;
    let emoji = match req.emoji.as_deref() {
        Some(e) => Some(clean_sound_emoji(e).ok_or_else(|| ApiErr::new(StatusCode::BAD_REQUEST, "invalid emoji"))?),
        None => None,
    };
    let n2 = name.clone();
    if !s.db.call(move |c| rename_sound(c, id, &n2, emoji.as_deref())).await? {
        return Err(not_found());
    }
    if sound.uploader.id != me.user.id {
        audit(&s, me.user.id, AUDIT_SOUND_RENAME, sound.uploader.id, format!("{} -> {name}", sound.name)).await;
    }
    let owner = s.owner().to_string();
    let fresh = s.db.call(move |c| get_sound(c, id, &owner)).await?.ok_or_else(not_found)?;
    announce(&s).await?;
    Ok(Json(fresh))
}

/// `POST /api/sounds/{id}/delete`
pub async fn delete(State(s): State<AppState>, me: Authed, Path(id): Path<SoundId>) -> ApiResult<OkResponse> {
    let sound = editable(&s, &me, id).await?;
    if !s.db.call(move |c| delete_sound(c, id)).await? {
        return Err(not_found());
    }
    if sound.uploader.id != me.user.id {
        audit(&s, me.user.id, AUDIT_SOUND_DELETE, sound.uploader.id, sound.name.clone()).await;
    }
    announce(&s).await?;
    Ok(Json(OkResponse { ok: true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_person_plays_once_per_two_seconds() {
        let st = SoundState::default();
        let t = Instant::now();
        assert_eq!(st.allow_play_at(1, 10, t), Ok(()));
        assert_eq!(st.allow_play_at(1, 10, t + Duration::from_millis(1900)), Err(PlayDenied::User));
        assert_eq!(st.allow_play_at(2, 10, t + Duration::from_millis(1900)), Ok(()), "somebody else is not slowed down");
        assert_eq!(st.allow_play_at(1, 10, t + Duration::from_millis(2100)), Ok(()));
    }

    #[test]
    fn a_play_can_be_stopped_once_while_it_may_still_play() {
        let st = SoundState::default();
        let t = Instant::now();
        assert!(!st.allow_stop_at(1, t), "nothing played");
        st.allow_play_at(1, 10, t).unwrap();
        assert!(!st.allow_stop_at(2, t), "only the player");
        assert!(st.allow_stop_at(1, t + Duration::from_millis(300)));
        assert!(!st.allow_stop_at(1, t + Duration::from_millis(400)), "once per play");
        st.allow_play_at(1, 10, t + Duration::from_secs(3)).unwrap();
        assert!(!st.allow_stop_at(1, t + Duration::from_secs(3) + STOP_WINDOW), "too late: it has ended");
    }

    #[test]
    fn a_room_hears_five_per_ten_seconds() {
        let st = SoundState::default();
        let t = Instant::now();
        for who in 1..=5 {
            assert_eq!(st.allow_play_at(who, 10, t + Duration::from_millis(who as u64 * 100)), Ok(()));
        }
        assert_eq!(st.allow_play_at(6, 10, t + Duration::from_secs(1)), Err(PlayDenied::Room));
        assert_eq!(st.allow_play_at(6, 11, t + Duration::from_secs(1)), Ok(()), "another room is its own count");
        assert_eq!(st.allow_play_at(7, 10, t + Duration::from_millis(10_150)), Ok(()), "the window slides");
    }

    #[test]
    fn uploads_are_a_token_bucket() {
        let st = SoundState::default();
        let t = Instant::now();
        for _ in 0..5 {
            assert!(st.allow_upload_at(1, t));
        }
        assert!(!st.allow_upload_at(1, t + Duration::from_secs(1)));
        assert!(st.allow_upload_at(2, t), "per person");
        assert!(st.allow_upload_at(1, t + Duration::from_secs(31)), "one token back after 30 s");
        assert!(!st.allow_upload_at(1, t + Duration::from_secs(32)));
    }

    #[test]
    fn storage_round_trip_and_the_limit() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE users (id INTEGER PRIMARY KEY, username TEXT, role TEXT, avatar_version TEXT);").unwrap();
        conn.execute("INSERT INTO users (id, username, role) VALUES (1, 'ana', 'member')", []).unwrap();
        init(&conn).unwrap();
        init(&conn).unwrap(); // idempotent
        let wav = canonical_wav(1, &[1i16; 9600]);
        for i in 0..SOUND_MAX_COUNT {
            assert!(insert_sound(&mut conn, &format!("s{i}"), "", 1, 200, "h", &wav).unwrap().is_some());
        }
        assert_eq!(insert_sound(&mut conn, "one too many", "", 1, 200, "h", &wav).unwrap(), None);
        assert!(delete_sound(&mut conn, 3).unwrap());
        assert!(!delete_sound(&mut conn, 3).unwrap());
        assert_eq!(sound_data(&conn, 3).unwrap(), None, "the audio goes with the row");
        assert!(insert_sound(&mut conn, "fits again", "🎺", 1, 200, "h", &wav).unwrap().is_some());
        assert!(rename_sound(&conn, 4, "renamed", Some("😀")).unwrap());
        assert!(rename_sound(&conn, 4, "again", None).unwrap());
        let (name, emoji): (String, String) = conn.query_row("SELECT name, emoji FROM sounds WHERE id = 4", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((name.as_str(), emoji.as_str()), ("again", "😀"), "no emoji given keeps it");
    }
}
