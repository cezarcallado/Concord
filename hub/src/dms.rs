//! Direct messages ("Mensagens diretas"): storage and HTTP handlers. The wire contract is in
//! `vox_proto::dm`; private calls are in `calls.rs`.
//!
//! **Privacy rule**: a conversation, its history and every event about it reach only its two
//! people. Every handler resolves the conversation through [`conversation_of`], which answers
//! `404` to anybody else (the same answer as a conversation that does not exist, so ids cannot be
//! probed); events go out with `send_to_many` / `send_to` to the two ids only. Roles do not
//! matter here at all: no moderation, admin or audit path reads or deletes DMs, and the audit log
//! never records their content. (The host can read the database file; see "DMs e E2E" in
//! `docs/ROADMAP-v0.2.md`.)
//!
//! Storage (additive, `CREATE TABLE IF NOT EXISTS`, so v0.5.2 and older hubs still run on a
//! migrated file and simply never read these tables):
//! - `dm_conversations`: one row per pair (`user_lo < user_hi`, unique), the last activity (list
//!   order), each side's read pointer (unread counts survive restarts and agree across devices)
//!   and whether each side opened it (an opened, still empty DM is listed only for the opener).
//! - `dm_messages`: their own id space (channel moderation, `POST /api/messages/{id}/delete`,
//!   can never reach them). `format` is `plain`, or `e2e1` (v1.1, E2): `body` is then the sealed
//!   line the app made, ciphertext the hub stores and hands back but never reads. A sealed line's
//!   `content` on the wire is the placeholder an older app shows; it gets no `[imagem]` marker
//!   (its images are sealed too) and no reply excerpt (`chat.rs`). Call lines are always plain.
//! - `dm_blocks`: who blocked whom.

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use vox_proto::*;

use crate::api::{ApiErr, AuthUser};
use crate::db::{self, UserRow};
use crate::state::AppState;
use crate::util::now_ms;

type ApiResult<T> = Result<Json<T>, ApiErr>;

/// Message `format` of a readable (not end-to-end encrypted) message.
pub const FORMAT_PLAIN: &str = "plain";
/// Message `format` of an end-to-end encrypted line (E2).
pub const FORMAT_SEALED: &str = DM_FORMAT_SEALED;

/// Is this a base64url body (no padding) the hub can store as a sealed line?
fn is_sealed_body(s: &str) -> bool {
    !s.is_empty() && s.len() <= DM_SEALED_MAX_BYTES && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/dms", get(list))
        .route("/api/dms/people", get(people))
        .route("/api/dms/blocks/{user_id}", post(block))
        .route("/api/dms/{id}", post(open))
        .route("/api/dms/{id}/messages", get(messages).post(send))
        .route("/api/dms/{id}/messages/{message_id}/delete", post(delete))
        .route("/api/dms/{id}/messages/{message_id}/reactions", post(react))
        .route("/api/dms/{id}/read", post(read))
        .route("/api/dms/{id}/call", post(crate::calls::call))
        .route("/api/dms/{id}/call/accept", post(crate::calls::accept))
        .route("/api/dms/{id}/call/decline", post(crate::calls::decline))
        .route("/api/dms/{id}/call/end", post(crate::calls::end))
}

// ---------------------------------------------------------------- storage

pub fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS dm_conversations (
            id INTEGER PRIMARY KEY,
            user_lo INTEGER NOT NULL REFERENCES users(id),
            user_hi INTEGER NOT NULL REFERENCES users(id),
            created_at_ms INTEGER NOT NULL,
            last_activity_ms INTEGER NOT NULL,
            lo_read_id INTEGER NOT NULL DEFAULT 0,
            hi_read_id INTEGER NOT NULL DEFAULT 0,
            lo_open INTEGER NOT NULL DEFAULT 0,
            hi_open INTEGER NOT NULL DEFAULT 0,
            CHECK (user_lo < user_hi),
            UNIQUE (user_lo, user_hi)
        );
        CREATE TABLE IF NOT EXISTS dm_messages (
            id INTEGER PRIMARY KEY,
            dm_id INTEGER NOT NULL REFERENCES dm_conversations(id),
            author_id INTEGER NOT NULL REFERENCES users(id),
            kind TEXT NOT NULL DEFAULT 'text',
            format TEXT NOT NULL DEFAULT 'plain',
            body TEXT NOT NULL,
            call_secs INTEGER NOT NULL DEFAULT 0,
            created_at_ms INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS dm_messages_dm ON dm_messages(dm_id, id);
        CREATE TABLE IF NOT EXISTS dm_blocks (
            blocker_id INTEGER NOT NULL REFERENCES users(id),
            blocked_id INTEGER NOT NULL REFERENCES users(id),
            created_at_ms INTEGER NOT NULL,
            PRIMARY KEY (blocker_id, blocked_id)
        );",
    )
}

/// One conversation row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvRow {
    pub id: DmId,
    pub lo: UserId,
    pub hi: UserId,
    pub last_activity_ms: i64,
    pub lo_read: DmMessageId,
    pub hi_read: DmMessageId,
}

impl ConvRow {
    pub fn has(&self, user: UserId) -> bool {
        user == self.lo || user == self.hi
    }

    pub fn peer_of(&self, me: UserId) -> UserId {
        if me == self.lo { self.hi } else { self.lo }
    }

    pub fn members(&self) -> [UserId; 2] {
        [self.lo, self.hi]
    }

    fn read_of(&self, me: UserId) -> DmMessageId {
        if me == self.lo { self.lo_read } else { self.hi_read }
    }
}

const CONV_COLS: &str = "id, user_lo, user_hi, last_activity_ms, lo_read_id, hi_read_id";

fn conv_row(r: &rusqlite::Row) -> rusqlite::Result<ConvRow> {
    Ok(ConvRow { id: r.get(0)?, lo: r.get(1)?, hi: r.get(2)?, last_activity_ms: r.get(3)?, lo_read: r.get(4)?, hi_read: r.get(5)? })
}

pub fn get_conv(conn: &Connection, id: DmId) -> rusqlite::Result<Option<ConvRow>> {
    conn.query_row(&format!("SELECT {CONV_COLS} FROM dm_conversations WHERE id = ?"), [id], conv_row).optional()
}

/// The conversation of `a` and `b`, created when there is none; `opener` marks it open on their side.
pub fn open_conv(conn: &Connection, a: UserId, b: UserId, opener: UserId) -> rusqlite::Result<ConvRow> {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    let now = now_ms();
    let exists = conn.query_row("SELECT 1 FROM dm_conversations WHERE user_lo = ? AND user_hi = ?", params![lo, hi], |_| Ok(())).optional()?.is_some();
    if !exists {
        crate::ids::insert_with(conn, crate::ids::Seq::DmConversations, |id| {
            conn.execute(
                "INSERT OR IGNORE INTO dm_conversations (id, user_lo, user_hi, created_at_ms, last_activity_ms) VALUES (?, ?, ?, ?, ?)",
                params![id, lo, hi, now, now],
            )
        })?;
    }
    let side = if opener == lo { "lo_open" } else { "hi_open" };
    conn.execute(&format!("UPDATE dm_conversations SET {side} = 1 WHERE user_lo = ? AND user_hi = ?"), params![lo, hi])?;
    conn.query_row(&format!("SELECT {CONV_COLS} FROM dm_conversations WHERE user_lo = ? AND user_hi = ?"), params![lo, hi], conv_row)
}

/// My conversations: those with at least one line, plus the empty ones I opened myself.
fn my_convs(conn: &Connection, me: UserId) -> rusqlite::Result<Vec<ConvRow>> {
    let mut st = conn.prepare(&format!(
        "SELECT {CONV_COLS} FROM dm_conversations c
         WHERE (c.user_lo = ?1 AND (c.lo_open = 1 OR EXISTS (SELECT 1 FROM dm_messages m WHERE m.dm_id = c.id)))
            OR (c.user_hi = ?1 AND (c.hi_open = 1 OR EXISTS (SELECT 1 FROM dm_messages m WHERE m.dm_id = c.id)))
         ORDER BY c.last_activity_ms DESC, c.id DESC"
    ))?;
    let rows = st.query_map([me], conv_row)?;
    rows.collect()
}

/// Has `blocker` blocked `blocked`?
pub fn is_blocked(conn: &Connection, blocker: UserId, blocked: UserId) -> rusqlite::Result<bool> {
    conn.query_row("SELECT 1 FROM dm_blocks WHERE blocker_id = ? AND blocked_id = ?", params![blocker, blocked], |_| Ok(()))
        .optional()
        .map(|r| r.is_some())
}

/// Either of the two blocked the other.
pub fn blocked_between(conn: &Connection, a: UserId, b: UserId) -> rusqlite::Result<bool> {
    Ok(is_blocked(conn, a, b)? || is_blocked(conn, b, a)?)
}

pub fn set_block(conn: &Connection, blocker: UserId, blocked: UserId, on: bool) -> rusqlite::Result<()> {
    if on {
        conn.execute("INSERT OR IGNORE INTO dm_blocks (blocker_id, blocked_id, created_at_ms) VALUES (?, ?, ?)", params![blocker, blocked, now_ms()])?;
    } else {
        conn.execute("DELETE FROM dm_blocks WHERE blocker_id = ? AND blocked_id = ?", params![blocker, blocked])?;
    }
    Ok(())
}

/// The two people of a conversation as everybody sees them now (role, picture, display name).
struct Pair {
    lo: User,
    hi: User,
}

impl Pair {
    fn load(conn: &Connection, c: &ConvRow, owner: &str) -> rusqlite::Result<Pair> {
        let user = |id: UserId| -> rusqlite::Result<User> {
            Ok(db::get_user(conn, id)?.map(|r| r.user(owner)).unwrap_or_else(|| User::new(id, format!("#{id}"))))
        };
        Ok(Pair { lo: user(c.lo)?, hi: user(c.hi)? })
    }

    fn get(&self, id: UserId) -> User {
        if id == self.lo.id { self.lo.clone() } else { self.hi.clone() }
    }
}

const MSG_COLS: &str = "id, dm_id, author_id, kind, body, call_secs, created_at_ms, reply_to, format";

fn msg_row(pair: &Pair) -> impl Fn(&rusqlite::Row) -> rusqlite::Result<DmMessage> + '_ {
    move |r| {
        let body: String = r.get(4)?;
        let (content, sealed) = if r.get::<_, String>(8)? == FORMAT_SEALED { (DM_SEALED_PLACEHOLDER.to_string(), Some(body)) } else { (body, None) };
        Ok(DmMessage {
            id: r.get(0)?,
            dm_id: r.get(1)?,
            author: pair.get(r.get(2)?),
            kind: DmMessageKind::from_wire(&r.get::<_, String>(3)?),
            content,
            call_secs: r.get::<_, i64>(5)?.clamp(0, u32::MAX as i64) as u32,
            created_at_ms: r.get(6)?,
            attachments: Vec::new(),
            // (A placeholder until `chat::fill_dm_lines`.)
            reply: r.get::<_, Option<DmMessageId>>(7)?.map(ReplyRef::gone),
            reactions: Vec::new(),
            sealed,
            sealed_attachments: Vec::new(),
        })
    }
}

/// Add a line (a message, or a call line written by the hub) and move the conversation up; the
/// author has read their own line.
pub fn insert_line(conn: &mut Connection, c: &ConvRow, author: UserId, kind: DmMessageKind, body: &str, call_secs: u32, owner: &str) -> rusqlite::Result<DmMessage> {
    insert_line_with(conn, c, author, kind, FORMAT_PLAIN, body, call_secs, owner, &[], None).map(|r| r.expect("no attachments to check"))
}

/// [`insert_line`] with images: `attachments` must be the author's pending uploads (checked in the
/// same transaction); the stored text gets the `[imagem]` marker. `reply_to` must be a text line of
/// the same conversation, otherwise the line is not a reply. `format` [`FORMAT_SEALED`] (E2):
/// `body` is the sealed line, stored as is; its images must be sealed uploads; no marker.
#[allow(clippy::too_many_arguments)]
pub fn insert_line_with(
    conn: &mut Connection,
    c: &ConvRow,
    author: UserId,
    kind: DmMessageKind,
    format: &str,
    body: &str,
    call_secs: u32,
    owner: &str,
    attachments: &[AttachmentId],
    reply_to: Option<DmMessageId>,
) -> rusqlite::Result<Result<DmMessage, crate::attachments::LinkError>> {
    let sealed = format == FORMAT_SEALED;
    let format = if sealed { FORMAT_SEALED } else { FORMAT_PLAIN };
    let tx = conn.transaction()?;
    if let Err(e) = crate::attachments::check_pending(&tx, author, attachments, sealed)? {
        return Ok(Err(e));
    }
    let reply = crate::chat::resolve_dm_reply(&tx, c.id, reply_to, owner)?;
    let body = &if sealed { body.to_string() } else { with_attachment_marker(body, attachments.len()) };
    let now = now_ms();
    // A fresh id: a deleted line's id never comes back (read pointers and the apps' caches use it).
    let id = crate::ids::insert_with(&tx, crate::ids::Seq::DmMessages, |id| {
        tx.execute(
            "INSERT INTO dm_messages (id, dm_id, author_id, kind, format, body, call_secs, created_at_ms, reply_to) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![id, c.id, author, kind.wire_name(), format, body, call_secs as i64, now, reply.as_ref().map(|r| r.id)],
        )?;
        Ok(id)
    })?;
    let side = if author == c.lo { "lo_read_id" } else { "hi_read_id" };
    tx.execute(&format!("UPDATE dm_conversations SET last_activity_ms = ?, {side} = MAX({side}, ?) WHERE id = ?"), params![now, id, c.id])?;
    let pair = Pair::load(&tx, c, owner)?;
    let linked = crate::attachments::link(&tx, attachments, crate::attachments::Owner::Dm(id))?;
    let (content, sealed_body, attachments, sealed_attachments) =
        if sealed { (DM_SEALED_PLACEHOLDER.to_string(), Some(body.to_string()), Vec::new(), linked) } else { (body.to_string(), None, linked, Vec::new()) };
    let msg = DmMessage { id, dm_id: c.id, author: pair.get(author), content, created_at_ms: now, kind, call_secs, attachments, reply, reactions: Vec::new(), sealed: sealed_body, sealed_attachments };
    tx.commit()?;
    Ok(Ok(msg))
}

/// Newest first; `before` is an exclusive id cursor.
pub fn list_lines(conn: &Connection, c: &ConvRow, before: Option<DmMessageId>, limit: i64, owner: &str) -> rusqlite::Result<Vec<DmMessage>> {
    let pair = Pair::load(conn, c, owner)?;
    let mut st = conn.prepare(&format!("SELECT {MSG_COLS} FROM dm_messages WHERE dm_id = ?1 AND id < ?2 ORDER BY id DESC LIMIT ?3"))?;
    let rows = st.query_map(params![c.id, before.unwrap_or(i64::MAX), limit], msg_row(&pair))?;
    let mut lines = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    crate::attachments::fill_dm_messages(conn, &mut lines)?;
    crate::chat::fill_dm_lines(conn, &mut lines, owner)?;
    Ok(lines)
}

/// The conversation as `me` sees it.
fn view(conn: &Connection, c: &ConvRow, me: UserId, owner: &str) -> rusqlite::Result<DmConversation> {
    let pair = Pair::load(conn, c, owner)?;
    let mut last_message = conn
        .query_row(&format!("SELECT {MSG_COLS} FROM dm_messages WHERE dm_id = ? ORDER BY id DESC LIMIT 1"), [c.id], msg_row(&pair))
        .optional()?;
    if let Some(m) = last_message.as_mut() {
        crate::attachments::fill_dm_messages(conn, std::slice::from_mut(m))?;
        crate::chat::fill_dm_lines(conn, std::slice::from_mut(m), owner)?;
    }
    let read = c.read_of(me);
    let unread: i64 = conn.query_row(
        "SELECT COUNT(*) FROM dm_messages WHERE dm_id = ? AND id > ? AND author_id != ?",
        params![c.id, read, me],
        |r| r.get(0),
    )?;
    let peer = c.peer_of(me);
    Ok(DmConversation {
        id: c.id,
        peer: pair.get(peer),
        last_message,
        last_activity_ms: c.last_activity_ms,
        unread: unread.clamp(0, u32::MAX as i64) as u32,
        read_up_to: read,
        blocked: is_blocked(conn, me, peer)?,
    })
}

// ---------------------------------------------------------------- shared checks

fn not_found() -> ApiErr {
    ApiErr::new(StatusCode::NOT_FOUND, "conversation not found")
}

pub fn blocked_err() -> ApiErr {
    ApiErr::new(StatusCode::FORBIDDEN, ERR_DM_BLOCKED)
}

/// The conversation `id` if `me` is one of its two people; `404` otherwise (whoever asks, any role).
pub async fn conversation_of(s: &AppState, me: UserId, id: DmId) -> Result<ConvRow, ApiErr> {
    match s.db.call(move |c| get_conv(c, id)).await? {
        Some(c) if c.has(me) => Ok(c),
        _ => Err(not_found()),
    }
}

async fn user_row(s: &AppState, id: UserId) -> Result<UserRow, ApiErr> {
    s.db.call(move |c| db::get_user(c, id)).await?.ok_or_else(|| ApiErr::new(StatusCode::NOT_FOUND, "user not found"))
}

/// Tell the two people of `c` (and nobody else).
pub fn tell_both(s: &AppState, c: &ConvRow, ev: &ServerEvent) {
    s.send_to_many(&c.members(), ev);
}

/// Write a line and tell both people about it.
pub async fn add_line(s: &AppState, c: &ConvRow, author: UserId, kind: DmMessageKind, body: String, call_secs: u32) -> Result<DmMessage, ApiErr> {
    let (conv, owner) = (c.clone(), s.owner());
    let message = s.db.call(move |db| insert_line(db, &conv, author, kind, &body, call_secs, &owner)).await?;
    tell_both(s, c, &ServerEvent::DmMessageCreated { message: message.clone() });
    Ok(message)
}

// ---------------------------------------------------------------- handlers

/// `GET /api/dms`
async fn list(State(s): State<AppState>, AuthUser(me): AuthUser) -> ApiResult<Vec<DmConversation>> {
    let owner = s.owner();
    let list = s
        .db
        .call(move |c| {
            let convs = my_convs(c, me.id)?;
            convs.iter().map(|conv| view(c, conv, me.id, &owner)).collect::<rusqlite::Result<Vec<_>>>()
        })
        .await?;
    Ok(Json(list))
}

/// `GET /api/dms/people`: everybody but me, by name.
async fn people(State(s): State<AppState>, AuthUser(me): AuthUser) -> ApiResult<Vec<User>> {
    let owner = s.owner();
    let rows = s.db.call(|c| db::list_users(c)).await?;
    Ok(Json(rows.into_iter().filter(|r| r.id != me.id).map(|r| r.user(&owner)).collect()))
}

/// `POST /api/dms/{user_id}`: my conversation with that person (created if needed).
async fn open(State(s): State<AppState>, AuthUser(me): AuthUser, Path(other): Path<UserId>) -> ApiResult<DmConversation> {
    if other == me.id {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, "that is you"));
    }
    user_row(&s, other).await?;
    let owner = s.owner();
    let conv = s
        .db
        .call(move |c| {
            let row = open_conv(c, me.id, other, me.id)?;
            view(c, &row, me.id, &owner)
        })
        .await?;
    tracing::info!(user = %me.username, dm = conv.id, "dm opened");
    Ok(Json(conv))
}

#[derive(Deserialize)]
struct PageQuery {
    before: Option<DmMessageId>,
    limit: Option<i64>,
}

/// `GET /api/dms/{id}/messages?before=&limit=`
async fn messages(State(s): State<AppState>, AuthUser(me): AuthUser, Path(id): Path<DmId>, Query(q): Query<PageQuery>) -> ApiResult<Vec<DmMessage>> {
    let conv = conversation_of(&s, me.id, id).await?;
    let limit = q.limit.unwrap_or(50).clamp(1, DM_PAGE_MAX as i64);
    let owner = s.owner();
    Ok(Json(s.db.call(move |c| list_lines(c, &conv, q.before, limit, &owner)).await?))
}

/// `POST /api/dms/{id}/messages` (E2: `sealed` instead of `content`)
async fn send(State(s): State<AppState>, AuthUser(me): AuthUser, Path(id): Path<DmId>, body: Result<Json<DmSendRequest>, JsonRejection>) -> ApiResult<DmMessage> {
    let Json(req) = body?;
    let conv = conversation_of(&s, me.id, id).await?;
    let content = req.content.trim().to_string();
    let n = content.chars().count();
    let format = match &req.sealed {
        Some(sealed) => {
            if !content.is_empty() {
                return Err(ApiErr::new(StatusCode::BAD_REQUEST, "an encrypted message has no readable text"));
            }
            if !is_sealed_body(sealed) {
                return Err(ApiErr::new(StatusCode::BAD_REQUEST, format!("an encrypted message must be 1-{DM_SEALED_MAX_BYTES} characters of base64url")));
            }
            FORMAT_SEALED
        }
        None => {
            if (n == 0 && req.attachments.is_empty()) || n > DM_MESSAGE_MAX_CHARS {
                return Err(ApiErr::new(StatusCode::BAD_REQUEST, format!("message must be 1-{DM_MESSAGE_MAX_CHARS} characters")));
            }
            FORMAT_PLAIN
        }
    };
    let content = req.sealed.clone().unwrap_or(content);
    let (a, b) = (me.id, conv.peer_of(me.id));
    if s.db.call(move |c| blocked_between(c, a, b)).await? {
        return Err(blocked_err());
    }
    // The same bucket as the channels: ~5 messages a second per person, wherever they go.
    if !s.allow_message(me.id) {
        return Err(ApiErr::new(StatusCode::TOO_MANY_REQUESTS, "rate limited: slow down"));
    }
    let (c2, owner, me_id, ids, reply_to) = (conv.clone(), s.owner(), me.id, req.attachments.clone(), req.reply_to);
    let message = s
        .db
        .call(move |db| insert_line_with(db, &c2, me_id, DmMessageKind::Text, format, &content, 0, &owner, &ids, reply_to))
        .await?
        .map_err(crate::attachments::link_err)?;
    tell_both(&s, &conv, &ServerEvent::DmMessageCreated { message: message.clone() });
    Ok(Json(message))
}

/// `POST /api/dms/{id}/messages/{message_id}/delete`: only my own messages (call lines stay).
async fn delete(State(s): State<AppState>, AuthUser(me): AuthUser, Path((id, message_id)): Path<(DmId, DmMessageId)>) -> ApiResult<OkResponse> {
    let conv = conversation_of(&s, me.id, id).await?;
    let found = s
        .db
        .call(move |c| {
            c.query_row("SELECT author_id, kind FROM dm_messages WHERE id = ? AND dm_id = ?", params![message_id, id], |r| {
                Ok((r.get::<_, UserId>(0)?, DmMessageKind::from_wire(&r.get::<_, String>(1)?)))
            })
            .optional()
        })
        .await?;
    let Some((author, kind)) = found else {
        return Err(ApiErr::new(StatusCode::NOT_FOUND, "message not found"));
    };
    if author != me.id || !kind.is_text() {
        return Err(ApiErr::new(StatusCode::FORBIDDEN, "only your own messages"));
    }
    let dir = s.attachments.dir.clone();
    s.db
        .call(move |c| {
            crate::attachments::unlink_dm_message(c, message_id)?;
            c.execute("DELETE FROM dm_messages WHERE id = ?", [message_id])?;
            crate::attachments::collect_garbage(c, &dir)
        })
        .await?;
    tell_both(&s, &conv, &ServerEvent::DmMessageDeleted { dm_id: id, message_id });
    Ok(Json(OkResponse { ok: true }))
}

/// `POST /api/dms/{id}/messages/{message_id}/reactions`: a reaction on a text line of my
/// conversation (not while either of us blocked the other). Only the two people are told.
async fn react(
    State(s): State<AppState>,
    AuthUser(me): AuthUser,
    Path((id, message_id)): Path<(DmId, DmMessageId)>,
    body: Result<Json<ReactionRequest>, JsonRejection>,
) -> ApiResult<OkResponse> {
    let Json(req) = body?;
    let conv = conversation_of(&s, me.id, id).await?;
    let emoji = crate::chat::check_request(&s, me.id, &req)?;
    let (a, b) = (me.id, conv.peer_of(me.id));
    if s.db.call(move |c| blocked_between(c, a, b)).await? {
        return Err(blocked_err());
    }
    let (e, add, me_id) = (emoji.clone(), req.add, me.id);
    let t = s
        .db
        .call(move |c| {
            let text_line = c.query_row("SELECT 1 FROM dm_messages WHERE id = ? AND dm_id = ? AND kind = 'text'", params![message_id, id], |_| Ok(())).optional()?.is_some();
            if !text_line {
                return Ok(crate::chat::Toggled::NoMessage);
            }
            crate::chat::toggle(c, true, message_id, me_id, &e, add)
        })
        .await?;
    if let Some(err) = crate::chat::toggled_err(t) {
        return Err(err);
    }
    if t == crate::chat::Toggled::Changed {
        tell_both(&s, &conv, &ServerEvent::DmReactionChanged { dm_id: id, message_id, emoji, user_id: me.id, added: add });
    }
    Ok(Json(OkResponse { ok: true }))
}

/// `POST /api/dms/{id}/read`: my read pointer moves forward (never back, never past the end).
async fn read(State(s): State<AppState>, AuthUser(me): AuthUser, Path(id): Path<DmId>, body: Result<Json<DmReadRequest>, JsonRejection>) -> ApiResult<OkResponse> {
    let Json(req) = body?;
    let conv = conversation_of(&s, me.id, id).await?;
    let side = if me.id == conv.lo { "lo_read_id" } else { "hi_read_id" };
    let up_to = s
        .db
        .call(move |c| {
            let last: i64 = c.query_row("SELECT COALESCE(MAX(id), 0) FROM dm_messages WHERE dm_id = ?", [id], |r| r.get(0))?;
            let to = req.up_to.clamp(0, last);
            c.execute(&format!("UPDATE dm_conversations SET {side} = MAX({side}, ?) WHERE id = ?"), params![to, id])?;
            c.query_row(&format!("SELECT {side} FROM dm_conversations WHERE id = ?"), [id], |r| r.get::<_, i64>(0))
        })
        .await?;
    s.send_to(me.id, &ServerEvent::DmRead { dm_id: id, up_to });
    Ok(Json(OkResponse { ok: true }))
}

/// `POST /api/dms/blocks/{user_id}`: block (or unblock) someone. Blocking also ends a call with them.
async fn block(State(s): State<AppState>, AuthUser(me): AuthUser, Path(other): Path<UserId>, body: Result<Json<DmBlockRequest>, JsonRejection>) -> ApiResult<OkResponse> {
    let Json(req) = body?;
    if other == me.id {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, "that is you"));
    }
    user_row(&s, other).await?;
    let me_id = me.id;
    s.db.call(move |c| set_block(c, me_id, other, req.blocked)).await?;
    tracing::info!(user = %me.username, other, blocked = req.blocked, "dm block changed");
    if req.blocked {
        crate::calls::end_between(&s, me.id, other).await;
    }
    Ok(Json(OkResponse { ok: true }))
}

/// The LiveKit address for a call token (the same host the client used for the hub; v1.1: the
/// hub's own port over TLS, see `api::livekit_url`). The call handlers get it through
/// `api::LkUrl`.
pub fn livekit_url(s: &AppState, headers: &HeaderMap, transport: crate::tls::Transport) -> String {
    crate::api::livekit_url(headers, &s.cfg, transport)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, username TEXT NOT NULL UNIQUE COLLATE NOCASE, created_at_ms INTEGER NOT NULL);
             INSERT INTO users (username, created_at_ms) VALUES ('a', 1), ('b', 1), ('c', 1);",
        )
        .unwrap();
        init(&c).unwrap();
        init(&c).unwrap(); // idempotent
        c
    }

    #[test]
    fn one_conversation_per_pair_whoever_opens_it() {
        let c = mem();
        let x = open_conv(&c, 2, 1, 2).unwrap();
        let y = open_conv(&c, 1, 2, 1).unwrap();
        assert_eq!(x.id, y.id);
        assert_eq!((x.lo, x.hi), (1, 2));
        assert!(x.has(1) && x.has(2) && !x.has(3));
        assert_eq!(x.peer_of(1), 2);
        let z = open_conv(&c, 1, 3, 1).unwrap();
        assert_ne!(z.id, x.id);
    }

    #[test]
    fn an_empty_conversation_is_listed_only_for_whoever_opened_it() {
        let c = mem();
        open_conv(&c, 1, 2, 1).unwrap();
        assert_eq!(my_convs(&c, 1).unwrap().len(), 1);
        assert!(my_convs(&c, 2).unwrap().is_empty());
        assert!(my_convs(&c, 3).unwrap().is_empty());
    }

    #[test]
    fn blocks_go_one_way_and_undo() {
        let c = mem();
        set_block(&c, 1, 2, true).unwrap();
        set_block(&c, 1, 2, true).unwrap();
        assert!(is_blocked(&c, 1, 2).unwrap() && !is_blocked(&c, 2, 1).unwrap());
        assert!(blocked_between(&c, 2, 1).unwrap());
        set_block(&c, 1, 2, false).unwrap();
        assert!(!blocked_between(&c, 1, 2).unwrap());
    }
}
