//! Chat images ("Imagens no chat"): upload, checks, quotas, storage and serving. The wire
//! contract is in `vox_proto::attachment`; the checks of a file are in [`validate`].
//!
//! **Storage.** Files live on disk under `<data-dir>/attachments/<aa>/<sha256>.<ext>`, content
//! addressed: the same file uploaded twice (or a GIF everybody reposts) is stored once. SQLite
//! keeps the metadata (additive tables, `CREATE TABLE IF NOT EXISTS`, so v0.6.0 and older hubs
//! still run on a migrated file and never read them):
//! - `attachment_blobs`: one row per stored file (hash, size, type).
//! - `attachments`: one row per image on a message (or pending, before its message is sent):
//!   uploader, the file, its thumbnail's file, sizes, and the channel message *or* DM line it
//!   belongs to. A file is deleted when no row points at it any more ([`collect_garbage`]).
//! - `attachment_uploads`: bytes each person uploaded per UTC day (the daily allowance).
//!
//! Every write and every garbage collection runs under the database's mutex (the files too), so a
//! file can never be deleted while an upload is deduplicating onto it.
//!
//! **Privacy.** A DM's images are served only to the conversation's two people (same `404` for
//! everybody else as for an unknown id); a pending image only to its uploader; a private channel's
//! images only to the people who may see the channel (v0.9, `access.rs`). Files are only ever
//! served by attachment id, never by hash.
//!
//! **Sealed images (v1.1, E2).** `?sealed=1`: the body is ciphertext the hub cannot read. It is
//! not decoded (nothing to validate), stored as `application/octet-stream` with no sizes and
//! `attachments.sealed = 1` (one more column, `DEFAULT 0`: older hubs never read it), and counts
//! against the same quotas. Sealed rows only link to sealed DM lines ([`check_pending`]).

mod validate;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use vox_proto::*;

pub use validate::{Kind, Refusal};

use crate::api::{ApiErr, AuthUser};
use crate::state::AppState;
use crate::util::{hex, now_ms};

type ApiResult<T> = Result<Json<T>, ApiErr>;

const DAY_MS: i64 = 24 * 3600 * 1000;
/// When the storage is full and old images may make room, only images older than this go.
const EVICT_MIN_AGE_MS: i64 = DAY_MS;
/// Housekeeping (expired pending uploads, retention) runs this often.
const SWEEP_EVERY: Duration = Duration::from_secs(600);
/// Uploads per person: a burst of this many...
const UPLOAD_RATE_LIMIT: usize = 30;
/// ...per this window.
const UPLOAD_RATE_WINDOW: Duration = Duration::from_secs(60);
/// Characters of `Attachment::hash`.
const SHORT_HASH: usize = 16;

// ---------------------------------------------------------------- state

/// Per-hub settings and limits of chat images.
pub struct AttachmentState {
    /// `<data-dir>/attachments`
    pub dir: PathBuf,
    pub storage_cap: u64,
    pub daily_bytes: u64,
    pub retention_days: u32,
    /// Upload rate limit and decode slots (the same machinery as profile pictures).
    uploads: crate::avatar::AvatarState,
}

impl AttachmentState {
    pub fn new(data_dir: &Path, cfg: &crate::config::Config) -> Self {
        AttachmentState {
            dir: data_dir.join("attachments"),
            storage_cap: cfg.attachment_storage_cap(),
            daily_bytes: cfg.attachment_daily_bytes(),
            retention_days: cfg.attachment_retention_days,
            uploads: crate::avatar::AvatarState::new(UPLOAD_RATE_LIMIT, UPLOAD_RATE_WINDOW),
        }
    }

    fn policy(&self) -> Policy {
        Policy { storage_cap: self.storage_cap, daily_bytes: self.daily_bytes, may_evict: self.retention_days > 0 }
    }
}

/// The limits an upload is checked against.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub storage_cap: u64,
    pub daily_bytes: u64,
    /// `attachment_retention_days` is set: a full storage evicts the oldest images (older than a
    /// day) instead of refusing.
    pub may_evict: bool,
}

// ---------------------------------------------------------------- database

pub fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS attachment_blobs (
            hash TEXT PRIMARY KEY,
            size INTEGER NOT NULL,
            mime TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS attachments (
            id INTEGER PRIMARY KEY,
            uploader_id INTEGER NOT NULL REFERENCES users(id),
            blob_hash TEXT NOT NULL,
            thumb_hash TEXT,
            mime TEXT NOT NULL,
            size INTEGER NOT NULL,
            width INTEGER NOT NULL,
            height INTEGER NOT NULL,
            thumb_w INTEGER NOT NULL,
            thumb_h INTEGER NOT NULL,
            animated INTEGER NOT NULL DEFAULT 0,
            message_id INTEGER,
            dm_message_id INTEGER,
            position INTEGER NOT NULL DEFAULT 0,
            created_at_ms INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS attachments_message ON attachments(message_id);
        CREATE INDEX IF NOT EXISTS attachments_dm_message ON attachments(dm_message_id);
        CREATE INDEX IF NOT EXISTS attachments_blob ON attachments(blob_hash);
        CREATE INDEX IF NOT EXISTS attachments_thumb ON attachments(thumb_hash);
        CREATE INDEX IF NOT EXISTS attachments_pending ON attachments(uploader_id) WHERE message_id IS NULL AND dm_message_id IS NULL;
        CREATE TABLE IF NOT EXISTS attachment_uploads (
            user_id INTEGER NOT NULL,
            day INTEGER NOT NULL,
            bytes INTEGER NOT NULL,
            PRIMARY KEY (user_id, day)
        );",
    )?;
    // v1.1 (E2): sealed (end-to-end encrypted) files. Checked before it is added: repeatable.
    if !crate::db::has_column(conn, "attachments", "sealed")? {
        conn.execute_batch("ALTER TABLE attachments ADD COLUMN sealed INTEGER NOT NULL DEFAULT 0")?;
    }
    Ok(())
}

/// Keep image rows tied to their messages whichever hub deletes a message (run after the
/// message tables exist). Message ids are reused by SQLite after the newest message is deleted,
/// and a v0.6.0 hub (still able to run on this file) deletes messages without their images: an
/// image left behind would then show up on somebody else's new message. So:
/// - triggers drop a message's image rows when the message row goes, from any hub;
/// - rows whose message is gone, or now belongs to somebody else than the uploader (an id that
///   was reused before the triggers existed), are dropped at every start (their files go with the
///   next housekeeping); [`fill_messages`] / [`visible_to`] check the author too.
pub fn init_links(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS attachments_follow_message AFTER DELETE ON messages
         BEGIN DELETE FROM attachments WHERE message_id = OLD.id; END;
         CREATE TRIGGER IF NOT EXISTS attachments_follow_dm_message AFTER DELETE ON dm_messages
         BEGIN DELETE FROM attachments WHERE dm_message_id = OLD.id; END;",
    )?;
    let channel = conn.execute(
        "DELETE FROM attachments WHERE message_id IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM messages m WHERE m.id = attachments.message_id AND m.user_id = attachments.uploader_id)",
        [],
    )?;
    let dm = conn.execute(
        "DELETE FROM attachments WHERE dm_message_id IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM dm_messages m WHERE m.id = attachments.dm_message_id AND m.author_id = attachments.uploader_id)",
        [],
    )?;
    if channel + dm > 0 {
        tracing::warn!(channel, dm, "images left behind by deleted messages were dropped");
    }
    Ok(())
}

/// One `attachments` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: AttachmentId,
    pub uploader: UserId,
    pub blob_hash: String,
    pub thumb_hash: Option<String>,
    pub mime: String,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    pub thumb_w: u32,
    pub thumb_h: u32,
    pub animated: bool,
    pub message_id: Option<MessageId>,
    pub dm_message_id: Option<DmMessageId>,
    pub created_at_ms: i64,
    /// E2: ciphertext (only for a sealed DM line).
    pub sealed: bool,
}

const COLS: &str = "id, uploader_id, blob_hash, thumb_hash, mime, size, width, height, thumb_w, thumb_h, animated, message_id, dm_message_id, created_at_ms, sealed";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Row> {
    let n = |i: usize| -> rusqlite::Result<u32> { Ok(r.get::<_, i64>(i)?.clamp(0, i64::from(u32::MAX)) as u32) };
    Ok(Row {
        id: r.get(0)?,
        uploader: r.get(1)?,
        blob_hash: r.get(2)?,
        thumb_hash: r.get(3)?,
        mime: r.get(4)?,
        size: r.get::<_, i64>(5)?.max(0) as u64,
        width: n(6)?,
        height: n(7)?,
        thumb_w: n(8)?,
        thumb_h: n(9)?,
        animated: r.get::<_, i64>(10)? != 0,
        message_id: r.get(11)?,
        dm_message_id: r.get(12)?,
        created_at_ms: r.get(13)?,
        sealed: r.get::<_, i64>(14)? != 0,
    })
}

impl Row {
    pub fn to_wire(&self) -> Attachment {
        Attachment {
            id: self.id,
            mime: self.mime.clone(),
            width: self.width,
            height: self.height,
            size: self.size,
            thumb_w: self.thumb_w,
            thumb_h: self.thumb_h,
            animated: self.animated,
            hash: self.blob_hash.chars().take(SHORT_HASH).collect(),
            sealed: self.sealed,
        }
    }

    pub fn pending(&self) -> bool {
        self.message_id.is_none() && self.dm_message_id.is_none()
    }
}

pub fn get(conn: &Connection, id: AttachmentId) -> rusqlite::Result<Option<Row>> {
    conn.query_row(&format!("SELECT {COLS} FROM attachments WHERE id = ?"), [id], row).optional()
}

/// The images of these messages (`dm`: DM lines instead of channel messages), in order.
fn for_owners(conn: &Connection, ids: &[i64], dm: bool) -> rusqlite::Result<HashMap<i64, Vec<Attachment>>> {
    let mut out: HashMap<i64, Vec<Attachment>> = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let col = if dm { "dm_message_id" } else { "message_id" };
    // Only images of the message's own author (see `init_links`).
    let join = if dm {
        "JOIN dm_messages m ON m.id = a.dm_message_id AND m.author_id = a.uploader_id"
    } else {
        "JOIN messages m ON m.id = a.message_id AND m.user_id = a.uploader_id"
    };
    let cols: Vec<String> = COLS.split(", ").map(|c| format!("a.{c}")).collect();
    // Pages are at most 100 messages: one query with a bound list.
    let marks = vec!["?"; ids.len()].join(",");
    let mut st = conn.prepare(&format!("SELECT {} FROM attachments a {join} WHERE a.{col} IN ({marks}) ORDER BY a.{col}, a.position, a.id", cols.join(", ")))?;
    let rows = st.query_map(rusqlite::params_from_iter(ids.iter()), row)?;
    for r in rows {
        let r = r?;
        let owner = if dm { r.dm_message_id } else { r.message_id };
        if let Some(o) = owner {
            out.entry(o).or_default().push(r.to_wire());
        }
    }
    Ok(out)
}

/// Fill `attachments` of channel messages.
pub fn fill_messages(conn: &Connection, msgs: &mut [Message]) -> rusqlite::Result<()> {
    let ids: Vec<i64> = msgs.iter().map(|m| m.id).collect();
    let mut map = for_owners(conn, &ids, false)?;
    for m in msgs {
        m.attachments = map.remove(&m.id).unwrap_or_default();
    }
    Ok(())
}

/// Fill `attachments` of DM lines (sealed ones go to `sealed_attachments`, where an older app
/// never looks).
pub fn fill_dm_messages(conn: &Connection, msgs: &mut [DmMessage]) -> rusqlite::Result<()> {
    let ids: Vec<i64> = msgs.iter().map(|m| m.id).collect();
    let mut map = for_owners(conn, &ids, true)?;
    for m in msgs {
        let (sealed, plain): (Vec<Attachment>, Vec<Attachment>) = map.remove(&m.id).unwrap_or_default().into_iter().partition(|a| a.sealed);
        m.attachments = plain;
        m.sealed_attachments = sealed;
    }
    Ok(())
}

/// Why pending attachments cannot go on a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError {
    TooMany,
    /// Unknown, not mine, already used, or listed twice.
    Invalid,
    /// E2: a sealed file on a readable message, or a readable file on a sealed DM line.
    Sealed,
}

/// Where linked attachments go.
#[derive(Debug, Clone, Copy)]
pub enum Owner {
    Channel(MessageId),
    Dm(DmMessageId),
}

/// Check that `ids` are `me`'s pending attachments (distinct, at most four), all sealed for a
/// sealed DM line (`sealed`) and none sealed otherwise. Run it in the same transaction as [`link`].
pub fn check_pending(conn: &Connection, me: UserId, ids: &[AttachmentId], sealed: bool) -> rusqlite::Result<Result<(), LinkError>> {
    if ids.len() > ATTACHMENT_MAX_PER_MESSAGE {
        return Ok(Err(LinkError::TooMany));
    }
    for (i, id) in ids.iter().enumerate() {
        if ids[..i].contains(id) {
            return Ok(Err(LinkError::Invalid));
        }
        match get(conn, *id)? {
            Some(r) if r.uploader == me && r.pending() && r.sealed != sealed => return Ok(Err(LinkError::Sealed)),
            Some(r) if r.uploader == me && r.pending() => {}
            _ => return Ok(Err(LinkError::Invalid)),
        }
    }
    Ok(Ok(()))
}

/// Put checked pending attachments on a message, in the order given; returns them.
pub fn link(conn: &Connection, ids: &[AttachmentId], owner: Owner) -> rusqlite::Result<Vec<Attachment>> {
    let mut out = Vec::with_capacity(ids.len());
    for (pos, id) in ids.iter().enumerate() {
        match owner {
            Owner::Channel(m) => conn.execute("UPDATE attachments SET message_id = ?, position = ? WHERE id = ?", params![m, pos as i64, id])?,
            Owner::Dm(m) => conn.execute("UPDATE attachments SET dm_message_id = ?, position = ? WHERE id = ?", params![m, pos as i64, id])?,
        };
        if let Some(r) = get(conn, *id)? {
            out.push(r.to_wire());
        }
    }
    Ok(out)
}

/// Drop the attachment rows of a channel message (the files go in [`collect_garbage`]).
pub fn unlink_message(conn: &Connection, message_id: MessageId) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM attachments WHERE message_id = ?", [message_id])
}

/// Drop the attachment rows of a DM line.
pub fn unlink_dm_message(conn: &Connection, dm_message_id: DmMessageId) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM attachments WHERE dm_message_id = ?", [dm_message_id])
}

/// Drop the attachment rows of every message of a channel (it is being deleted).
pub fn unlink_channel(conn: &Connection, channel_id: ChannelId) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM attachments WHERE message_id IN (SELECT id FROM messages WHERE channel_id = ?)", [channel_id])
}

fn blob_path(dir: &Path, hash: &str, mime: &str) -> PathBuf {
    let ext = Kind::from_mime(mime).map_or("bin", Kind::ext);
    dir.join(&hash[..2.min(hash.len())]).join(format!("{hash}.{ext}"))
}

/// Delete every stored file no attachment points at any more (rows and files). Returns how many.
pub fn collect_garbage(conn: &Connection, dir: &Path) -> rusqlite::Result<usize> {
    let mut st = conn.prepare(
        "SELECT hash, mime FROM attachment_blobs b
         WHERE NOT EXISTS (SELECT 1 FROM attachments a WHERE a.blob_hash = b.hash)
           AND NOT EXISTS (SELECT 1 FROM attachments a WHERE a.thumb_hash = b.hash)",
    )?;
    let dead = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (hash, mime) in &dead {
        conn.execute("DELETE FROM attachment_blobs WHERE hash = ?", [hash])?;
        let p = blob_path(dir, hash, mime);
        if let Err(e) = std::fs::remove_file(&p) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %p.display(), "could not delete an image file: {e}");
            }
        }
    }
    Ok(dead.len())
}

/// Bytes of every stored file.
pub fn usage(conn: &Connection) -> rusqlite::Result<u64> {
    Ok(conn.query_row("SELECT COALESCE(SUM(size), 0) FROM attachment_blobs", [], |r| r.get::<_, i64>(0))?.max(0) as u64)
}

fn day_of(ms: i64) -> i64 {
    ms.div_euclid(DAY_MS)
}

/// Bytes `user` uploaded on the UTC day of `now_ms`.
pub fn uploaded_today(conn: &Connection, user: UserId, now_ms: i64) -> rusqlite::Result<u64> {
    let n: Option<i64> = conn.query_row("SELECT bytes FROM attachment_uploads WHERE user_id = ? AND day = ?", params![user, day_of(now_ms)], |r| r.get(0)).optional()?;
    Ok(n.unwrap_or(0).max(0) as u64)
}

/// A checked file, ready for [`store_upload`].
#[derive(Debug, Clone)]
pub struct Upload {
    pub main: validate::Clean,
    pub thumb: Option<validate::Clean>,
    /// E2: ciphertext. The parts' `kind`, sizes and `animated` mean nothing then: they are stored
    /// as [`ATTACHMENT_SEALED_MIME`] with no sizes.
    pub sealed: bool,
}

impl Upload {
    fn bytes(&self) -> u64 {
        (self.main.bytes.len() + self.thumb.as_ref().map_or(0, |t| t.bytes.len())) as u64
    }

    /// The stored type of one part.
    fn mime(&self, part: &validate::Clean) -> &'static str {
        if self.sealed { ATTACHMENT_SEALED_MIME } else { part.kind.mime() }
    }

    /// A sealed upload (the picture's ciphertext, then `thumb_len` bytes of its thumbnail's): no
    /// decoding, only the lengths (each part at least an AEAD tag) are checked.
    pub fn sealed(body: &[u8], thumb_len: usize) -> Option<Upload> {
        let (main, thumb) = body.split_at(body.len().checked_sub(thumb_len)?);
        let part = |b: &[u8]| validate::Clean { bytes: b.to_vec(), kind: Kind::Jpeg, width: 0, height: 0, animated: false };
        if main.len() < ATTACHMENT_SEALED_OVERHEAD || main.len() > ATTACHMENT_MAX_ANIMATED_BYTES + ATTACHMENT_SEALED_OVERHEAD {
            return None;
        }
        if thumb_len > 0 && (thumb.len() < ATTACHMENT_SEALED_OVERHEAD || thumb.len() > ATTACHMENT_MAX_THUMB_BYTES + ATTACHMENT_SEALED_OVERHEAD) {
            return None;
        }
        Some(Upload { main: part(main), thumb: (thumb_len > 0).then(|| part(thumb)), sealed: true })
    }
}

/// Why a checked upload could not be stored.
#[derive(Debug)]
pub enum StoreError {
    Daily,
    Full,
    TooManyPending,
    Db(rusqlite::Error),
    Io(std::io::Error),
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Db(e)
    }
}

fn sha256(b: &[u8]) -> String {
    hex(&Sha256::digest(b))
}

/// Write a file (unless it is already stored) and its blob row. Returns whether it was new.
fn put_blob(conn: &Connection, dir: &Path, bytes: &[u8], mime: &str, now: i64) -> Result<(String, bool), StoreError> {
    let hash = sha256(bytes);
    let exists = conn.query_row("SELECT 1 FROM attachment_blobs WHERE hash = ?", [&hash], |_| Ok(())).optional()?.is_some();
    if exists {
        return Ok((hash, false));
    }
    let path = blob_path(dir, &hash, mime);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(StoreError::Io)?;
    }
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, bytes).map_err(StoreError::Io)?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        StoreError::Io(e)
    })?;
    conn.execute("INSERT INTO attachment_blobs (hash, size, mime, created_at_ms) VALUES (?, ?, ?, ?)", params![hash, bytes.len() as i64, mime, now])?;
    Ok((hash, true))
}

/// Make room for `needed` more bytes by deleting the oldest images older than a day. Returns
/// whether there is room now.
fn evict_for(conn: &Connection, dir: &Path, needed: u64, cap: u64, now: i64) -> rusqlite::Result<bool> {
    loop {
        if usage(conn)?.saturating_add(needed) <= cap {
            return Ok(true);
        }
        let oldest: Option<i64> = conn
            .query_row("SELECT id FROM attachments WHERE created_at_ms < ? ORDER BY created_at_ms, id LIMIT 1", [now - EVICT_MIN_AGE_MS], |r| r.get(0))
            .optional()?;
        let Some(id) = oldest else { return Ok(false) };
        conn.execute("DELETE FROM attachments WHERE id = ?", [id])?;
        let freed = collect_garbage(conn, dir)?;
        tracing::info!(attachment = id, files_freed = freed, "image storage full: deleted the oldest image");
    }
}

/// Store a checked upload of `user` as a pending attachment, enforcing the daily allowance, the
/// pending limit and the storage cap (all under the database lock, with the files).
pub fn store_upload(conn: &mut Connection, dir: &Path, user: UserId, up: &Upload, policy: Policy, now: i64) -> Result<Row, StoreError> {
    let tx = conn.transaction()?;
    let bytes = up.bytes();
    if uploaded_today(&tx, user, now)?.saturating_add(bytes) > policy.daily_bytes {
        return Err(StoreError::Daily);
    }
    let pending: i64 = tx.query_row("SELECT COUNT(*) FROM attachments WHERE uploader_id = ? AND message_id IS NULL AND dm_message_id IS NULL", [user], |r| r.get(0))?;
    if pending as usize >= ATTACHMENT_MAX_PENDING {
        return Err(StoreError::TooManyPending);
    }
    // Only files not stored yet take space.
    let new_bytes: u64 = [Some(&up.main), up.thumb.as_ref()]
        .into_iter()
        .flatten()
        .map(|c| {
            let h = sha256(&c.bytes);
            let exists = tx.query_row("SELECT 1 FROM attachment_blobs WHERE hash = ?", [&h], |_| Ok(())).optional()?.is_some();
            Ok(if exists { 0 } else { c.bytes.len() as u64 })
        })
        .collect::<rusqlite::Result<Vec<u64>>>()?
        .into_iter()
        .sum();
    if new_bytes > 0 && usage(&tx)?.saturating_add(new_bytes) > policy.storage_cap && !(policy.may_evict && evict_for(&tx, dir, new_bytes, policy.storage_cap, now)?) {
        return Err(StoreError::Full);
    }
    let mut written = Vec::new();
    let result = (|| -> Result<Row, StoreError> {
        let (main_hash, new) = put_blob(&tx, dir, &up.main.bytes, up.mime(&up.main), now)?;
        if new {
            written.push((main_hash.clone(), up.mime(&up.main)));
        }
        let (thumb_hash, tw, th) = match &up.thumb {
            Some(t) => {
                let (h, new) = put_blob(&tx, dir, &t.bytes, up.mime(t), now)?;
                if new {
                    written.push((h.clone(), up.mime(t)));
                }
                (Some(h), t.width, t.height)
            }
            None => (None, up.main.width, up.main.height),
        };
        // A fresh id: a deleted image's id never comes back (the apps cache thumbnails).
        let id = crate::ids::insert_with(&tx, crate::ids::Seq::Attachments, |id| {
            tx.execute(
                "INSERT INTO attachments (id, uploader_id, blob_hash, thumb_hash, mime, size, width, height, thumb_w, thumb_h, animated, created_at_ms, sealed)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, user, main_hash, thumb_hash, up.mime(&up.main), up.main.bytes.len() as i64, up.main.width, up.main.height, tw, th, up.main.animated as i64, now, up.sealed as i64],
            )?;
            Ok(id)
        })?;
        tx.execute(
            "INSERT INTO attachment_uploads (user_id, day, bytes) VALUES (?1, ?2, ?3)
             ON CONFLICT (user_id, day) DO UPDATE SET bytes = bytes + ?3",
            params![user, day_of(now), bytes as i64],
        )?;
        // Old days of the allowance are of no use.
        tx.execute("DELETE FROM attachment_uploads WHERE day < ?", [day_of(now) - 2])?;
        Ok(get(&tx, id)?.expect("just inserted"))
    })();
    match result {
        Ok(r) => {
            tx.commit()?;
            Ok(r)
        }
        Err(e) => {
            // The transaction rolls back; files written for it must go too.
            for (hash, mime) in written {
                let _ = std::fs::remove_file(blob_path(dir, &hash, mime));
            }
            Err(e)
        }
    }
}

/// Housekeeping: pending uploads older than their lifetime, images past the retention, unused
/// files, and files on disk the database does not know (a crash between writing and committing).
pub fn sweep(conn: &Connection, dir: &Path, now: i64, retention_days: u32) -> rusqlite::Result<(usize, usize, usize)> {
    let expired = conn.execute(
        "DELETE FROM attachments WHERE message_id IS NULL AND dm_message_id IS NULL AND created_at_ms < ?",
        [now - ATTACHMENT_PENDING_SECS as i64 * 1000],
    )?;
    let old = if retention_days > 0 { conn.execute("DELETE FROM attachments WHERE created_at_ms < ?", [now - i64::from(retention_days) * DAY_MS])? } else { 0 };
    let mut files = collect_garbage(conn, dir)?;
    // Stray files: not in `attachment_blobs` (or a leftover `.part`).
    if let Ok(subdirs) = std::fs::read_dir(dir) {
        for sub in subdirs.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())) {
            let Ok(entries) = std::fs::read_dir(sub.path()) else { continue };
            for f in entries.flatten() {
                let name = f.file_name().to_string_lossy().into_owned();
                let hash = name.split('.').next().unwrap_or_default().to_string();
                let known = !name.ends_with(".part") && conn.query_row("SELECT 1 FROM attachment_blobs WHERE hash = ?", [&hash], |_| Ok(())).optional()?.is_some();
                if !known && std::fs::remove_file(f.path()).is_ok() {
                    files += 1;
                }
            }
        }
    }
    Ok((expired, old, files))
}

/// Background housekeeping until shutdown.
pub async fn watch(s: AppState, mut shutdown: watch::Receiver<bool>) {
    let mut first = true;
    loop {
        let wait = if first { Duration::from_secs(5) } else { SWEEP_EVERY };
        first = false;
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = crate::state::wait_shutdown(&mut shutdown) => return,
        }
        let (dir, days) = (s.attachments.dir.clone(), s.attachments.retention_days);
        match s.db.call(move |c| sweep(c, &dir, now_ms(), days)).await {
            Ok((0, 0, 0)) => {}
            Ok((expired, old, files)) => tracing::info!(expired, old, files, "image housekeeping"),
            Err(e) => tracing::error!("image housekeeping: {e:#}"),
        }
    }
}

// ---------------------------------------------------------------- HTTP

fn refusal(r: Refusal) -> ApiErr {
    let mb = |n: usize| n / (1024 * 1024);
    match r {
        Refusal::Unsupported => ApiErr::new(StatusCode::BAD_REQUEST, "not a supported image (use PNG, JPEG, WebP or GIF)"),
        Refusal::Damaged => ApiErr::new(StatusCode::BAD_REQUEST, "could not read the image (damaged file?)"),
        Refusal::Dimensions => ApiErr::new(StatusCode::BAD_REQUEST, format!("image dimensions are too large (at most {ATTACHMENT_MAX_SIDE} px)")),
        Refusal::TooManyFrames => ApiErr::new(StatusCode::PAYLOAD_TOO_LARGE, "the animation has too many frames"),
        Refusal::TooBig { animated: false } => ApiErr::new(StatusCode::PAYLOAD_TOO_LARGE, format!("image too large (at most {} MB)", mb(ATTACHMENT_MAX_BYTES))),
        Refusal::TooBig { animated: true } => ApiErr::new(StatusCode::PAYLOAD_TOO_LARGE, format!("animation too large (at most {} MB)", mb(ATTACHMENT_MAX_ANIMATED_BYTES))),
        Refusal::BadThumb => ApiErr::new(StatusCode::BAD_REQUEST, "the thumbnail does not match the image"),
    }
}

fn store_err(e: StoreError) -> ApiErr {
    match e {
        StoreError::Daily => ApiErr::new(StatusCode::TOO_MANY_REQUESTS, ERR_ATTACHMENT_DAILY),
        StoreError::Full => ApiErr::new(StatusCode::INSUFFICIENT_STORAGE, ERR_ATTACHMENT_STORAGE_FULL),
        StoreError::TooManyPending => ApiErr::new(StatusCode::TOO_MANY_REQUESTS, "too many images waiting to be sent"),
        StoreError::Db(e) => anyhow::Error::from(e).into(),
        StoreError::Io(e) => anyhow::anyhow!("writing an image file: {e}").into(),
    }
}

/// `GET /api/attachments/limits`
pub async fn limits(State(s): State<AppState>, AuthUser(me): AuthUser) -> ApiResult<AttachmentLimits> {
    let a = &s.attachments;
    let (cap, daily, evict) = (a.storage_cap, a.daily_bytes, a.retention_days > 0);
    let (used_today, used) = s.db.call(move |c| Ok((uploaded_today(c, me.id, now_ms())?, usage(c)?))).await?;
    Ok(Json(AttachmentLimits {
        max_per_message: ATTACHMENT_MAX_PER_MESSAGE as u32,
        max_bytes: ATTACHMENT_MAX_BYTES as u64,
        max_animated_bytes: ATTACHMENT_MAX_ANIMATED_BYTES as u64,
        max_side: ATTACHMENT_MAX_SIDE,
        daily_bytes: daily,
        daily_bytes_left: daily.saturating_sub(used_today),
        storage_full: !evict && used >= cap,
        sealed: true,
    }))
}

#[derive(Deserialize)]
pub struct UploadQuery {
    #[serde(default)]
    thumb_len: usize,
    /// E2: `1` = the body is ciphertext.
    #[serde(default)]
    sealed: u8,
}

/// Check an upload (image + optional thumbnail); the hub makes the thumbnail when the app sent
/// none and the image is bigger than the thumbnail box.
pub fn check_upload(body: &[u8], thumb_len: usize) -> Result<Upload, Refusal> {
    let (main_raw, thumb_raw) = body.split_at(body.len() - thumb_len);
    let (main, first) = validate::image(main_raw)?;
    let thumb = if thumb_len > 0 {
        Some(validate::thumb(thumb_raw, main.width, main.height)?)
    } else if main.width > ATTACHMENT_THUMB_BOX || main.height > ATTACHMENT_THUMB_BOX || main.animated {
        validate::make_thumb(&first)
    } else {
        None
    };
    Ok(Upload { main, thumb, sealed: false })
}

fn body_rejection(e: BytesRejection) -> ApiErr {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        refusal(Refusal::TooBig { animated: true })
    } else {
        ApiErr::new(StatusCode::BAD_REQUEST, "could not read the request body")
    }
}

/// `POST /api/attachments?thumb_len=` (`&sealed=1`: ciphertext, see the module docs)
pub async fn upload(State(s): State<AppState>, AuthUser(me): AuthUser, Query(q): Query<UploadQuery>, body: Result<Bytes, BytesRejection>) -> ApiResult<Attachment> {
    let body = body.map_err(body_rejection)?;
    let sealed = q.sealed != 0;
    let thumb_max = ATTACHMENT_MAX_THUMB_BYTES + if sealed { ATTACHMENT_SEALED_OVERHEAD } else { 0 };
    if body.is_empty() || q.thumb_len >= body.len() || q.thumb_len > thumb_max {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, "expected an image (and a thumbnail no bigger than 256 KB)"));
    }
    if sealed && body.len() - q.thumb_len > ATTACHMENT_MAX_ANIMATED_BYTES + ATTACHMENT_SEALED_OVERHEAD {
        return Err(refusal(Refusal::TooBig { animated: true }));
    }
    let a = &s.attachments;
    if let Err(wait) = a.uploads.allow(me.id, Instant::now()) {
        return Err(crate::api::too_many(wait));
    }
    // Cheap early answers before any decoding (the authoritative checks come with the write).
    let (id, len, policy) = (me.id, body.len() as u64, a.policy());
    let (today, used) = s.db.call(move |c| Ok((uploaded_today(c, id, now_ms())?, usage(c)?))).await?;
    if today.saturating_add(len) > policy.daily_bytes {
        return Err(store_err(StoreError::Daily));
    }
    if !policy.may_evict && used >= policy.storage_cap {
        return Err(store_err(StoreError::Full));
    }
    if sealed {
        // Nothing to decode: the quotas are all there is to check.
        let up = Upload::sealed(&body, q.thumb_len).ok_or_else(|| ApiErr::new(StatusCode::BAD_REQUEST, "a sealed image is too short or too long"))?;
        let (dir, sizes) = (a.dir.clone(), (up.main.bytes.len(), up.thumb.as_ref().map_or(0, |t| t.bytes.len())));
        let stored = s.db.call(move |c| Ok(store_upload(c, &dir, id, &up, policy, now_ms()))).await?.map_err(|e| {
            tracing::info!(user = %me.username, ?e, "sealed image not stored");
            store_err(e)
        })?;
        tracing::info!(user = %me.username, id = stored.id, stored = sizes.0, thumb = sizes.1, "sealed image uploaded");
        return Ok(Json(stored.to_wire()));
    }
    // The decode slot goes with the blocking decode: a client that hangs up mid-decode cannot
    // free it while the decode still runs.
    let slot = a.uploads.decodes.clone().acquire_owned().await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let thumb_len = q.thumb_len;
    let checked = crate::util::spawn_blocking_with(slot, move || check_upload(&body, thumb_len)).await.map_err(|_| refusal(Refusal::Damaged))?;
    let up = checked.map_err(|r| {
        tracing::info!(user = %me.username, len, ?r, "image refused");
        refusal(r)
    })?;
    let (dir, sizes) = (a.dir.clone(), (up.main.bytes.len(), up.thumb.as_ref().map_or(0, |t| t.bytes.len())));
    let (w, h, mime, animated) = (up.main.width, up.main.height, up.main.kind.mime(), up.main.animated);
    let stored = s.db.call(move |c| Ok(store_upload(c, &dir, id, &up, policy, now_ms()))).await?.map_err(|e| {
        tracing::info!(user = %me.username, ?e, "image not stored");
        store_err(e)
    })?;
    tracing::info!(user = %me.username, id = stored.id, received = len, stored = sizes.0, thumb = sizes.1, w, h, mime, animated, "image uploaded");
    Ok(Json(stored.to_wire()))
}

/// May `me` see this attachment? Channel images: anybody who may see the channel (v0.9: a private
/// channel's images are `404` without access, like an unknown id). DM images: the two people of
/// the conversation. Pending: the uploader.
fn visible_to(conn: &Connection, r: &Row, me: UserId, rules: &crate::access::Rules) -> rusqlite::Result<bool> {
    // The message must exist and be the uploader's own (an id reused after a deletion is not).
    if let Some(m) = r.message_id {
        let channel: Option<ChannelId> = conn.query_row("SELECT channel_id FROM messages WHERE id = ? AND user_id = ?", [m, r.uploader], |x| x.get(0)).optional()?;
        return Ok(channel.is_some_and(|ch| rules.can_see(me, ch)));
    }
    if let Some(line) = r.dm_message_id {
        let pair: Option<(UserId, UserId)> = conn
            .query_row(
                "SELECT c.user_lo, c.user_hi FROM dm_messages m JOIN dm_conversations c ON c.id = m.dm_id WHERE m.id = ? AND m.author_id = ?",
                [line, r.uploader],
                |x| Ok((x.get(0)?, x.get(1)?)),
            )
            .optional()?;
        return Ok(pair.is_some_and(|(lo, hi)| me == lo || me == hi));
    }
    Ok(r.uploader == me)
}

async fn serve(s: AppState, me: UserId, id: AttachmentId, thumb: bool, headers: HeaderMap) -> Result<Response, ApiErr> {
    let rules = s.access.rules();
    let found = s
        .db
        .call(move |c| {
            let Some(r) = get(c, id)? else { return Ok(None) };
            if !visible_to(c, &r, me, &rules)? {
                return Ok(None);
            }
            let (hash, mime) = match (thumb, &r.thumb_hash) {
                (true, Some(h)) => {
                    let mime: String = c.query_row("SELECT mime FROM attachment_blobs WHERE hash = ?", [h], |x| x.get(0))?;
                    (h.clone(), mime)
                }
                _ => (r.blob_hash.clone(), r.mime.clone()),
            };
            Ok(Some((hash, mime)))
        })
        .await?;
    // Not yours and unknown look the same.
    let Some((hash, mime)) = found else {
        return Err(ApiErr::new(StatusCode::NOT_FOUND, "attachment not found"));
    };
    let etag = format!("\"{}\"", &hash[..SHORT_HASH.min(hash.len())]);
    let mut resp = if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|t| t.trim() == etag)) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let path = blob_path(&s.attachments.dir, &hash, &mime);
        let read = path.clone();
        let bytes = tokio::task::spawn_blocking(move || std::fs::read(read)).await.map_err(|e| anyhow::anyhow!("{e}"))?.map_err(|e| {
            tracing::warn!(path = %path.display(), "image file missing: {e}");
            ApiErr::new(StatusCode::NOT_FOUND, "attachment not found")
        })?;
        let mut r = bytes.into_response();
        r.headers_mut().insert(header::CONTENT_TYPE, mime.parse().map_err(|_| anyhow::anyhow!("bad mime"))?);
        r
    };
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, header::HeaderValue::from_static("private, max-age=31536000, immutable"));
    h.insert(header::ETAG, etag.parse().map_err(|_| anyhow::anyhow!("bad etag"))?);
    h.insert(header::X_CONTENT_TYPE_OPTIONS, header::HeaderValue::from_static("nosniff"));
    Ok(resp)
}

/// `GET /api/attachments/{id}`
pub async fn get_file(State(s): State<AppState>, AuthUser(me): AuthUser, UrlPath(id): UrlPath<AttachmentId>, headers: HeaderMap) -> Result<Response, ApiErr> {
    serve(s, me.id, id, false, headers).await
}

/// `GET /api/attachments/{id}/thumb`
pub async fn get_thumb(State(s): State<AppState>, AuthUser(me): AuthUser, UrlPath(id): UrlPath<AttachmentId>, headers: HeaderMap) -> Result<Response, ApiErr> {
    serve(s, me.id, id, true, headers).await
}

/// The `400` of a message whose images cannot be used.
pub fn link_err(e: LinkError) -> ApiErr {
    match e {
        LinkError::TooMany => ApiErr::new(StatusCode::BAD_REQUEST, format!("at most {ATTACHMENT_MAX_PER_MESSAGE} images per message")),
        LinkError::Invalid => ApiErr::new(StatusCode::BAD_REQUEST, "unknown or already used image"),
        LinkError::Sealed => ApiErr::new(StatusCode::BAD_REQUEST, ERR_SEALED_ONLY_DM),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, username TEXT); INSERT INTO users (username) VALUES ('a'), ('b');
             CREATE TABLE messages (id INTEGER PRIMARY KEY, channel_id INTEGER, user_id INTEGER, content TEXT, created_at_ms INTEGER);",
        )
        .unwrap();
        init(&c).unwrap();
        init(&c).unwrap();
        c
    }

    fn upload_of(bytes: Vec<u8>) -> Upload {
        Upload { main: validate::Clean { bytes, kind: Kind::Jpeg, width: 10, height: 10, animated: false }, thumb: None, sealed: false }
    }

    const BIG: Policy = Policy { storage_cap: 1 << 40, daily_bytes: 1 << 40, may_evict: false };

    #[test]
    fn identical_files_are_stored_once_and_go_with_the_last_reference() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = mem();
        let a = store_upload(&mut c, dir.path(), 1, &upload_of(vec![1; 100]), BIG, 1000).unwrap();
        let b = store_upload(&mut c, dir.path(), 2, &upload_of(vec![1; 100]), BIG, 1000).unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(a.blob_hash, b.blob_hash);
        assert_eq!(usage(&c).unwrap(), 100, "one file");
        let path = blob_path(dir.path(), &a.blob_hash, &a.mime);
        assert!(path.exists());
        c.execute("DELETE FROM attachments WHERE id = ?", [a.id]).unwrap();
        assert_eq!(collect_garbage(&c, dir.path()).unwrap(), 0);
        assert!(path.exists(), "still used by the other");
        c.execute("DELETE FROM attachments WHERE id = ?", [b.id]).unwrap();
        assert_eq!(collect_garbage(&c, dir.path()).unwrap(), 1);
        assert!(!path.exists());
        assert_eq!(usage(&c).unwrap(), 0);
    }

    #[test]
    fn the_daily_allowance_and_the_cap_are_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = mem();
        let p = Policy { storage_cap: 1 << 30, daily_bytes: 250, may_evict: false };
        store_upload(&mut c, dir.path(), 1, &upload_of(vec![1; 200]), p, 1000).unwrap();
        assert!(matches!(store_upload(&mut c, dir.path(), 1, &upload_of(vec![2; 100]), p, 1000), Err(StoreError::Daily)));
        // Another person, and the next day, are fine.
        store_upload(&mut c, dir.path(), 2, &upload_of(vec![2; 100]), p, 1000).unwrap();
        store_upload(&mut c, dir.path(), 1, &upload_of(vec![3; 100]), p, 1000 + DAY_MS).unwrap();
        // Total cap: 400 bytes stored now.
        let cap = Policy { storage_cap: 450, daily_bytes: 1 << 30, may_evict: false };
        assert!(matches!(store_upload(&mut c, dir.path(), 2, &upload_of(vec![4; 100]), cap, 2000), Err(StoreError::Full)));
        // A file that is already stored takes no new space.
        store_upload(&mut c, dir.path(), 2, &upload_of(vec![1; 200]), cap, 2000).unwrap();
        // With retention on, the oldest images (older than a day) make room.
        let evict = Policy { may_evict: true, ..cap };
        let now = 1000 + 3 * DAY_MS;
        let r = store_upload(&mut c, dir.path(), 2, &upload_of(vec![5; 100]), evict, now).unwrap();
        assert!(usage(&c).unwrap() <= 450);
        assert!(get(&c, r.id).unwrap().is_some());
    }

    #[test]
    fn pending_uploads_expire_and_stray_files_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = mem();
        let r = store_upload(&mut c, dir.path(), 1, &upload_of(vec![9; 50]), BIG, 1000).unwrap();
        std::fs::create_dir_all(dir.path().join("zz")).unwrap();
        std::fs::write(dir.path().join("zz").join("zzzz.webp"), b"stray").unwrap();
        let (expired, old, files) = sweep(&c, dir.path(), 1000 + 10_000, 0).unwrap();
        assert_eq!((expired, old, files), (0, 0, 1), "fresh pending stays, the stray file goes");
        let (expired, _, files) = sweep(&c, dir.path(), 1000 + ATTACHMENT_PENDING_SECS as i64 * 1000 + 1, 0).unwrap();
        assert_eq!((expired, files), (1, 1));
        assert!(get(&c, r.id).unwrap().is_none());
    }

    #[test]
    fn linking_needs_my_own_pending_images() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = mem();
        let a = store_upload(&mut c, dir.path(), 1, &upload_of(vec![1; 10]), BIG, 1).unwrap();
        let b = store_upload(&mut c, dir.path(), 2, &upload_of(vec![2; 10]), BIG, 1).unwrap();
        assert_eq!(check_pending(&c, 1, &[a.id], false).unwrap(), Ok(()));
        assert_eq!(check_pending(&c, 1, &[a.id], true).unwrap(), Err(LinkError::Sealed), "a readable image on a sealed line");
        assert_eq!(check_pending(&c, 1, &[b.id], false).unwrap(), Err(LinkError::Invalid), "not mine");
        assert_eq!(check_pending(&c, 1, &[a.id, a.id], false).unwrap(), Err(LinkError::Invalid), "twice");
        assert_eq!(check_pending(&c, 1, &[1, 2, 3, 4, 5], false).unwrap(), Err(LinkError::TooMany));
        let s = store_upload(&mut c, dir.path(), 1, &Upload::sealed(&[7u8; 64], 20).unwrap(), BIG, 1).unwrap();
        assert!(s.sealed && s.mime == ATTACHMENT_SEALED_MIME && (s.width, s.height, s.thumb_w, s.thumb_h) == (0, 0, 0, 0));
        assert_eq!(check_pending(&c, 1, &[s.id], false).unwrap(), Err(LinkError::Sealed), "a sealed image on a readable message");
        assert_eq!(check_pending(&c, 1, &[s.id], true).unwrap(), Ok(()));
        assert!(Upload::sealed(&[7u8; 20], 10).is_none(), "each part is at least a tag");
        assert!(Upload::sealed(&[7u8; 20], 0).is_some());
        c.execute("INSERT INTO messages VALUES (77, 1, 1, '', 0)", []).unwrap();
        let linked = link(&c, &[a.id], Owner::Channel(77)).unwrap();
        assert_eq!(linked[0].id, a.id);
        assert_eq!(check_pending(&c, 1, &[a.id], false).unwrap(), Err(LinkError::Invalid), "already used");
        let mut m = vec![Message { id: 77, channel_id: 1, author: User::new(1, "a"), content: String::new(), created_at_ms: 0, attachments: vec![], mentions: vec![], mention_everyone: false, reply: None, reactions: vec![] }];
        fill_messages(&c, &mut m).unwrap();
        assert_eq!(m[0].attachments.len(), 1);
        assert_eq!(unlink_message(&c, 77).unwrap(), 1);
    }

    /// Rollback: a v0.6.0 hub deletes a DM line without its images, and SQLite gives the deleted
    /// (newest) id to the next line, in any conversation (an older hub's own insert; this hub
    /// never reuses an id since v0.8.2). The images must not move over to it.
    #[test]
    fn images_do_not_follow_a_reused_message_id() {
        use crate::dms;
        // An older hub's insert: no id named, so SQLite hands out `MAX(id) + 1`.
        let old_insert = |c: &Connection, conv: &dms::ConvRow, author: UserId, body: &str| -> rusqlite::Result<DmMessage> {
            c.execute("INSERT INTO dm_messages (dm_id, author_id, kind, format, body, call_secs, created_at_ms) VALUES (?, ?, 'text', 'plain', ?, 0, 1)", params![conv.id, author, body])?;
            Ok(dms::list_lines(c, conv, None, 1, "")?.remove(0))
        };
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(&dir.path().join("voxhub.db")).unwrap();
        let files = dir.path().join("attachments");
        db.call_now(|c| {
            let id = |c: &mut Connection, n: &str| crate::db::login(c, n, &format!("t-{n}")).map(|r| r.id);
            let (a, b, cc, d) = (id(c, "a")?, id(c, "b")?, id(c, "c")?, id(c, "d")?);
            let ab = dms::open_conv(c, a, b, a)?;
            let cd = dms::open_conv(c, cc, d, cc)?;
            // With the triggers: the v0.6.0 delete takes the image rows along.
            let img = store_upload(c, &files, a, &upload_of(vec![1; 10]), BIG, 1).unwrap();
            let line = dms::insert_line(c, &ab, a, DmMessageKind::Text, "foto", 0, "")?;
            link(c, &[img.id], Owner::Dm(line.id))?;
            c.execute("DELETE FROM dm_messages WHERE id = ?", [line.id])?; // v0.6.0's SQL
            let reused = old_insert(c, &cd, cc, "oi")?;
            assert_eq!(reused.id, line.id, "SQLite reuses the id (the scenario)");
            assert!(get(c, img.id)?.is_none(), "the image row went with its line");
            let mut lines = vec![reused.clone()];
            fill_dm_messages(c, &mut lines)?;
            assert!(lines[0].attachments.is_empty());

            // Rows orphaned before the triggers existed: the author check hides them, and the
            // next start drops them.
            c.execute_batch("DROP TRIGGER attachments_follow_dm_message")?;
            let img2 = store_upload(c, &files, a, &upload_of(vec![2; 10]), BIG, 1).unwrap();
            let line2 = dms::insert_line(c, &ab, a, DmMessageKind::Text, "foto", 0, "")?;
            link(c, &[img2.id], Owner::Dm(line2.id))?;
            c.execute("DELETE FROM dm_messages WHERE id = ?", [line2.id])?;
            let reused2 = old_insert(c, &cd, cc, "oi de novo")?;
            assert_eq!(reused2.id, line2.id);
            // This hub's own insert never takes a deleted id again.
            c.execute("DELETE FROM dm_messages WHERE id = ?", [reused2.id])?;
            let fresh = dms::insert_line(c, &cd, cc, DmMessageKind::Text, "nova", 0, "")?;
            assert!(fresh.id > reused2.id, "{} reused", fresh.id);
            let row = get(c, img2.id)?.expect("left behind");
            let mut lines = vec![reused2];
            fill_dm_messages(c, &mut lines)?;
            assert!(lines[0].attachments.is_empty(), "not the author's image: not shown");
            let rules = crate::access::Rules::default();
            assert!(!visible_to(c, &row, cc, &rules)? && !visible_to(c, &row, d, &rules)?, "and not served");
            Ok(())
        })
        .unwrap();
        drop(db);
        let db = crate::db::Db::open(&dir.path().join("voxhub.db")).unwrap();
        db.call_now(|c| {
            let left: i64 = c.query_row("SELECT COUNT(*) FROM attachments", [], |r| r.get(0))?;
            assert_eq!(left, 0, "dropped at start");
            let t: i64 = c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' AND name LIKE 'attachments_%'", [], |r| r.get(0))?;
            assert_eq!(t, 2, "and the trigger is back");
            Ok(())
        })
        .unwrap();
    }
}
