//! Mentions, replies and reactions (v0.8): storage, the checks on a new message, and the reaction
//! handlers. The wire contract is in `vox_proto::chat`.
//!
//! Storage (additive and idempotent, so a v0.7.0 hub still runs on a migrated file and simply
//! never reads any of it):
//! - `messages.mentions` (comma-separated user ids, `''` = none), `messages.mention_everyone` and
//!   `messages.reply_to`; `dm_messages.reply_to`. All with defaults, and older hubs' `INSERT`s name
//!   their columns, so they keep working.
//! - `reactions` / `dm_reactions`: one row per (message, person, emoji). Triggers drop a message's
//!   reactions with it whichever hub deletes it (SQLite reuses the newest id after a delete, so a
//!   left-over row would land on somebody else's new message).
//!
//! History pages get their reply quotes and reactions with one query each per page (never one per
//! message).

use std::collections::HashMap;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use rusqlite::{params, Connection, OptionalExtension};
use vox_proto::*;

use crate::api::{ApiErr, AuthUser};
use crate::db;
use crate::state::AppState;
use crate::util::now_ms;

type ApiResult<T> = Result<Json<T>, ApiErr>;

pub fn init(conn: &Connection) -> rusqlite::Result<()> {
    for (table, column, decl) in [
        ("messages", "mentions", "TEXT NOT NULL DEFAULT ''"),
        ("messages", "mention_everyone", "INTEGER NOT NULL DEFAULT 0"),
        ("messages", "reply_to", "INTEGER"),
        ("dm_messages", "reply_to", "INTEGER"),
    ] {
        if !db::has_column(conn, table, column)? {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))?;
        }
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS reactions (
            id INTEGER PRIMARY KEY,
            message_id INTEGER NOT NULL,
            user_id INTEGER NOT NULL,
            emoji TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL,
            UNIQUE (message_id, user_id, emoji)
        );
        CREATE TABLE IF NOT EXISTS dm_reactions (
            id INTEGER PRIMARY KEY,
            message_id INTEGER NOT NULL,
            user_id INTEGER NOT NULL,
            emoji TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL,
            UNIQUE (message_id, user_id, emoji)
        );
        CREATE TRIGGER IF NOT EXISTS reactions_follow_message AFTER DELETE ON messages
         BEGIN DELETE FROM reactions WHERE message_id = OLD.id; END;
        CREATE TRIGGER IF NOT EXISTS reactions_follow_dm_message AFTER DELETE ON dm_messages
         BEGIN DELETE FROM dm_reactions WHERE message_id = OLD.id; END;",
    )
}

// ---------------------------------------------------------------- a new message

/// What a sender asked for besides the text.
#[derive(Debug, Clone, Default)]
pub struct Draft {
    pub mentions: Vec<UserId>,
    pub reply_to: Option<MessageId>,
    pub reply_mention: bool,
    /// Who may see the channel right now (v0.9): only they can be mentioned. `None`: everybody
    /// (unit tests).
    pub access: Option<std::sync::Arc<crate::access::Rules>>,
}

impl Draft {
    fn may_see(&self, user: UserId, channel: ChannelId) -> bool {
        self.access.as_ref().is_none_or(|r| r.can_see(user, channel))
    }
}

/// What a new channel message really carries, after the checks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    pub mentions: Vec<UserId>,
    pub everyone: bool,
    pub reply: Option<ReplyRef>,
}

pub fn mentions_column(ids: &[UserId]) -> String {
    ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")
}

fn parse_mentions(s: &str) -> Vec<UserId> {
    s.split(',').filter_map(|p| p.trim().parse().ok()).collect()
}

/// The checks on a new channel message by `author` (run inside its insert transaction):
/// - mentions: real people (not the author) whose `@username` or `@display name` is in the text,
///   at most [`MENTIONS_MAX`];
/// - `@todos` only counts from a Moderador or above (the *effective* role: a staff member who still
///   has to set a personal password is a member until then);
/// - the reply must be a message of the same channel (otherwise it is dropped), and with
///   `reply_mention` its author is mentioned too;
/// - (v0.9) only people who may see the channel are mentioned: in a private channel, nobody
///   without access is ever notified (nor gets the message: `@todos` reaches only the people the
///   message goes to).
pub fn resolve_channel(conn: &Connection, author: UserId, channel_id: ChannelId, caption: &str, draft: &Draft, owner: &str) -> rusqlite::Result<Resolved> {
    let mut out = Resolved::default();
    // v1.0.1: the senders refuse longer lists; this bounds the work (and the database lookups,
    // made while the hub's one connection is held) whatever the caller, and each id counts once.
    let mut looked: Vec<UserId> = Vec::new();
    for &id in draft.mentions.iter().take(MENTIONS_WIRE_MAX) {
        if out.mentions.len() >= MENTIONS_MAX {
            break;
        }
        if id == author || looked.contains(&id) || !draft.may_see(id, channel_id) {
            continue;
        }
        looked.push(id);
        let Some(row) = db::get_user(conn, id)? else { continue };
        let named = !find_mention(caption, &row.username).is_empty() || row.display_name.as_deref().is_some_and(|d| !find_mention(caption, d).is_empty());
        if named {
            out.mentions.push(id);
        }
    }
    if has_everyone_mention(caption) {
        out.everyone = db::get_user(conn, author)?.is_some_and(|r| r.effective_role(owner).can_moderate());
    }
    if let Some(to) = draft.reply_to {
        let same: Option<(ChannelId, UserId)> = conn.query_row("SELECT channel_id, user_id FROM messages WHERE id = ?", [to], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        if let Some((_, original_author)) = same.filter(|(ch, _)| *ch == channel_id) {
            out.reply = reply_refs(conn, &[to], false, owner)?.remove(&to).map(|(_, r)| r);
            if draft.reply_mention && original_author != author && !out.mentions.contains(&original_author) && draft.may_see(original_author, channel_id) {
                out.mentions.push(original_author);
            }
        }
    }
    Ok(out)
}

/// The reply of a new DM line: a text line of the same conversation, or nothing.
pub fn resolve_dm_reply(conn: &Connection, dm_id: DmId, reply_to: Option<DmMessageId>, owner: &str) -> rusqlite::Result<Option<ReplyRef>> {
    let Some(to) = reply_to else { return Ok(None) };
    let ok = conn
        .query_row("SELECT 1 FROM dm_messages WHERE id = ? AND dm_id = ? AND kind = 'text'", params![to, dm_id], |_| Ok(()))
        .optional()?
        .is_some();
    if !ok {
        return Ok(None);
    }
    Ok(reply_refs(conn, &[to], true, owner)?.remove(&to).map(|(_, r)| r))
}

// ---------------------------------------------------------------- history pages

/// The quotes of these originals (`dm`: DM lines), one query, each with the channel (or DM) it
/// is in. Missing ones are not in the map.
fn reply_refs(conn: &Connection, ids: &[i64], dm: bool, owner: &str) -> rusqlite::Result<HashMap<i64, (i64, ReplyRef)>> {
    let mut out = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let marks = vec!["?"; ids.len()].join(",");
    let sql = if dm {
        format!(
            "SELECT m.id, m.body, u.id, u.username, u.role, u.avatar_version, u.display_name,
                    (SELECT COUNT(*) FROM attachments a WHERE a.dm_message_id = m.id AND a.uploader_id = m.author_id), m.dm_id, m.format
             FROM dm_messages m JOIN users u ON u.id = m.author_id WHERE m.id IN ({marks})"
        )
    } else {
        format!(
            "SELECT m.id, m.content, u.id, u.username, u.role, u.avatar_version, u.display_name,
                    (SELECT COUNT(*) FROM attachments a WHERE a.message_id = m.id AND a.uploader_id = m.user_id), m.channel_id, 'plain'
             FROM messages m JOIN users u ON u.id = m.user_id WHERE m.id IN ({marks})"
        )
    };
    let mut st = conn.prepare(&sql)?;
    let rows = st.query_map(rusqlite::params_from_iter(ids.iter()), |r| {
        let images = r.get::<_, i64>(7)?.clamp(0, u32::MAX as i64) as u32;
        let content: String = r.get(1)?;
        // E2: a sealed original has no excerpt the hub could make (the app fills it).
        let sealed = r.get::<_, String>(9)? == DM_FORMAT_SEALED;
        let quote = ReplyRef {
            id: r.get(0)?,
            author: Some(db::display_user(r.get(2)?, r.get(3)?, &r.get::<_, String>(4)?, r.get(5)?, r.get(6)?, owner)),
            excerpt: if sealed { String::new() } else { reply_excerpt(strip_attachment_marker(&content, images as usize)) },
            images,
            deleted: false,
            sealed,
        };
        Ok((r.get::<_, i64>(8)?, quote))
    })?;
    for r in rows {
        let (container, quote) = r?;
        out.insert(quote.id, (container, quote));
    }
    Ok(out)
}

/// Reactions of these messages, in the order each emoji was first used. One query.
fn reactions_of(conn: &Connection, ids: &[i64], dm: bool) -> rusqlite::Result<HashMap<i64, Vec<Reaction>>> {
    let mut out: HashMap<i64, Vec<Reaction>> = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let table = if dm { "dm_reactions" } else { "reactions" };
    let marks = vec!["?"; ids.len()].join(",");
    let mut st = conn.prepare(&format!("SELECT message_id, emoji, user_id FROM {table} WHERE message_id IN ({marks}) ORDER BY message_id, id"))?;
    let rows = st.query_map(rusqlite::params_from_iter(ids.iter()), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, UserId>(2)?)))?;
    for r in rows {
        let (m, emoji, user) = r?;
        apply_reaction(out.entry(m).or_default(), &emoji, user, true);
    }
    Ok(out)
}

/// Turn the raw columns of a channel message into its fields (`reply` is a placeholder until
/// [`fill_messages`]).
pub fn raw_fields(m: &mut Message, mentions: &str, everyone: bool, reply_to: Option<MessageId>) {
    m.mentions = parse_mentions(mentions);
    m.mention_everyone = everyone;
    m.reply = reply_to.map(ReplyRef::gone);
}

/// The quote of `id` if it is in `container` (the message's own channel / DM: a quote never shows
/// text from anywhere else, v0.9), else "mensagem apagada".
fn quote_in(quotes: &HashMap<i64, (i64, ReplyRef)>, id: i64, container: i64) -> ReplyRef {
    quotes.get(&id).filter(|(c, _)| *c == container).map(|(_, r)| r.clone()).unwrap_or_else(|| ReplyRef::gone(id))
}

/// Fill the reply quotes and reactions of a page of channel messages.
pub fn fill_messages(conn: &Connection, msgs: &mut [Message], owner: &str) -> rusqlite::Result<()> {
    let ids: Vec<i64> = msgs.iter().map(|m| m.id).collect();
    let originals: Vec<i64> = msgs.iter().filter_map(|m| m.reply.as_ref().map(|r| r.id)).collect();
    let quotes = reply_refs(conn, &originals, false, owner)?;
    let mut reactions = reactions_of(conn, &ids, false)?;
    for m in msgs {
        if let Some(r) = m.reply.as_mut() {
            *r = quote_in(&quotes, r.id, m.channel_id);
        }
        m.reactions = reactions.remove(&m.id).unwrap_or_default();
    }
    Ok(())
}

/// The same for DM lines.
pub fn fill_dm_lines(conn: &Connection, lines: &mut [DmMessage], owner: &str) -> rusqlite::Result<()> {
    let ids: Vec<i64> = lines.iter().map(|m| m.id).collect();
    let originals: Vec<i64> = lines.iter().filter_map(|m| m.reply.as_ref().map(|r| r.id)).collect();
    let quotes = reply_refs(conn, &originals, true, owner)?;
    let mut reactions = reactions_of(conn, &ids, true)?;
    for m in lines {
        if let Some(r) = m.reply.as_mut() {
            *r = quote_in(&quotes, r.id, m.dm_id);
        }
        m.reactions = reactions.remove(&m.id).unwrap_or_default();
    }
    Ok(())
}

// ---------------------------------------------------------------- reactions

/// What a reaction request did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggled {
    NoMessage,
    TooMany,
    Unchanged,
    Changed,
}

/// Put `emoji` of `user` on message `id` (`dm`: a DM line), or take it off. The caller checked
/// that the message is one the person may react to.
pub fn toggle(conn: &mut Connection, dm: bool, id: i64, user: UserId, emoji: &str, add: bool) -> rusqlite::Result<Toggled> {
    let (table, messages) = if dm { ("dm_reactions", "dm_messages") } else { ("reactions", "messages") };
    let tx = conn.transaction()?;
    if tx.query_row(&format!("SELECT 1 FROM {messages} WHERE id = ?"), [id], |_| Ok(())).optional()?.is_none() {
        return Ok(Toggled::NoMessage);
    }
    let result = if add {
        let mine = tx.query_row(&format!("SELECT 1 FROM {table} WHERE message_id = ? AND user_id = ? AND emoji = ?"), params![id, user, emoji], |_| Ok(())).optional()?.is_some();
        if mine {
            Toggled::Unchanged
        } else {
            let used = tx.query_row(&format!("SELECT 1 FROM {table} WHERE message_id = ? AND emoji = ? LIMIT 1"), params![id, emoji], |_| Ok(())).optional()?.is_some();
            let distinct: i64 = tx.query_row(&format!("SELECT COUNT(DISTINCT emoji) FROM {table} WHERE message_id = ?"), [id], |r| r.get(0))?;
            if !used && distinct as usize >= REACTIONS_MAX_PER_MESSAGE {
                Toggled::TooMany
            } else {
                tx.execute(&format!("INSERT INTO {table} (message_id, user_id, emoji, created_at_ms) VALUES (?, ?, ?, ?)"), params![id, user, emoji, now_ms()])?;
                Toggled::Changed
            }
        }
    } else {
        let n = tx.execute(&format!("DELETE FROM {table} WHERE message_id = ? AND user_id = ? AND emoji = ?"), params![id, user, emoji])?;
        if n > 0 { Toggled::Changed } else { Toggled::Unchanged }
    };
    tx.commit()?;
    Ok(result)
}

/// The checks every reaction request shares: one emoji, not too fast.
pub fn check_request(s: &AppState, user: UserId, req: &ReactionRequest) -> Result<String, ApiErr> {
    let emoji = clean_reaction_emoji(&req.emoji).ok_or_else(|| ApiErr::new(StatusCode::BAD_REQUEST, "a reaction must be a single emoji"))?;
    if !s.allow_reaction(user) {
        return Err(ApiErr::new(StatusCode::TOO_MANY_REQUESTS, "rate limited: slow down"));
    }
    Ok(emoji)
}

pub fn toggled_err(t: Toggled) -> Option<ApiErr> {
    match t {
        Toggled::NoMessage => Some(ApiErr::new(StatusCode::NOT_FOUND, "message not found")),
        Toggled::TooMany => Some(ApiErr::new(StatusCode::CONFLICT, ERR_TOO_MANY_REACTIONS)),
        Toggled::Unchanged | Toggled::Changed => None,
    }
}

/// `POST /api/messages/{id}/reactions`: anybody logged in, on any channel message they may see
/// (v0.9: a message of a private channel without access is `404`, like an unknown one).
pub async fn react(State(s): State<AppState>, AuthUser(me): AuthUser, Path(id): Path<MessageId>, body: Result<Json<ReactionRequest>, JsonRejection>) -> ApiResult<OkResponse> {
    let Json(req) = body?;
    let emoji = check_request(&s, me.id, &req)?;
    let (e, add, me_id, rules) = (emoji.clone(), req.add, me.id, s.access.rules());
    let (t, channel_id) = s
        .db
        .call(move |c| {
            let ch: Option<ChannelId> = c.query_row("SELECT channel_id FROM messages WHERE id = ?", [id], |r| r.get(0)).optional()?;
            match ch {
                Some(ch) if rules.can_see(me_id, ch) => Ok((toggle(c, false, id, me_id, &e, add)?, Some(ch))),
                _ => Ok((Toggled::NoMessage, None)),
            }
        })
        .await?;
    if let Some(err) = toggled_err(t) {
        return Err(err);
    }
    if let (Toggled::Changed, Some(channel_id)) = (t, channel_id) {
        s.send_channel(channel_id, &ServerEvent::ReactionChanged { channel_id, message_id: id, emoji, user_id: me.id, added: add });
    }
    Ok(Json(OkResponse { ok: true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, channel_id INTEGER, user_id INTEGER, content TEXT, created_at_ms INTEGER);
             CREATE TABLE dm_messages (id INTEGER PRIMARY KEY, dm_id INTEGER, author_id INTEGER, kind TEXT NOT NULL DEFAULT 'text', body TEXT, created_at_ms INTEGER);
             INSERT INTO messages (channel_id, user_id, content, created_at_ms) VALUES (1, 1, 'oi', 1), (1, 2, 'olá', 2);",
        )
        .unwrap();
        init(&c).unwrap();
        init(&c).unwrap(); // idempotent
        c
    }

    #[test]
    fn reactions_toggle_cap_and_follow_their_message() {
        let mut c = mem();
        assert_eq!(toggle(&mut c, false, 1, 7, "👍", true).unwrap(), Toggled::Changed);
        assert_eq!(toggle(&mut c, false, 1, 7, "👍", true).unwrap(), Toggled::Unchanged);
        assert_eq!(toggle(&mut c, false, 1, 8, "👍", true).unwrap(), Toggled::Changed);
        assert_eq!(toggle(&mut c, false, 99, 8, "👍", true).unwrap(), Toggled::NoMessage);
        let r = reactions_of(&c, &[1, 2], false).unwrap();
        assert_eq!(r[&1], vec![Reaction { emoji: "👍".into(), users: vec![7, 8] }]);
        assert!(!r.contains_key(&2));
        // At most REACTIONS_MAX_PER_MESSAGE distinct emoji; one already there can still be used.
        let many = ["😀", "😃", "😄", "😁", "😆", "😅", "😂", "😊", "😇", "😉", "😍", "😘", "😎", "😐", "😏", "😬", "😴", "😭", "😱"];
        for e in many {
            assert_eq!(toggle(&mut c, false, 1, 7, e, true).unwrap(), Toggled::Changed);
        }
        assert_eq!(toggle(&mut c, false, 1, 9, "🎉", true).unwrap(), Toggled::TooMany);
        assert_eq!(toggle(&mut c, false, 1, 9, "😀", true).unwrap(), Toggled::Changed);
        assert_eq!(toggle(&mut c, false, 1, 7, "👍", false).unwrap(), Toggled::Changed);
        assert_eq!(toggle(&mut c, false, 1, 7, "👍", false).unwrap(), Toggled::Unchanged);
        // Deleting the message (as any hub does) takes its reactions with it.
        c.execute("DELETE FROM messages WHERE id = 1", []).unwrap();
        assert_eq!(c.query_row("SELECT COUNT(*) FROM reactions", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    }

    #[test]
    fn mention_ids_round_trip() {
        assert_eq!(mentions_column(&[3, 12]), "3,12");
        assert_eq!(parse_mentions("3,12"), vec![3, 12]);
        assert!(parse_mentions("").is_empty());
        assert_eq!(parse_mentions("4,x,5"), vec![4, 5]);
    }
}
