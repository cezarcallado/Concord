//! Private channels ("Canais privados", v0.9): who may see which channel, decided in one place.
//!
//! **The rule** ([`vox_proto::PrivateAccess::allows`]): a public channel is for everybody; a private
//! one for Admin and Dono always, plus the roles (Moderador, Membro) and the accounts its list
//! names. The role is the one permissions use ([`crate::db::UserRow::effective_role`]): a staff
//! member who still has to set a personal password counts as Membro, so somebody who logs in as
//! the configured owner's name before its owner protected it sees nothing private.
//!
//! **One check, in memory.** [`Rules`] holds every channel's access list and every account's
//! effective role; [`Rules::can_see`] is the only access check of the hub, with no database query.
//! The rules are swapped as a whole (an `Arc`) whenever the channel list ([`Shared::set_channels`])
//! or the roles ([`reload_roles`]) change, and the swap is followed by [`Out::Access`], on which
//! every connection recomputes what it may see (`ws.rs`).
//!
//! **Where it is enforced** (somebody without access must not learn the channel exists):
//! - the channel list: `Ready`, `ChannelsChanged` (per connection) and `GET /api/channels`;
//! - every channel event goes through [`Out::Channel`] / [`Out::Voice`] and is checked per
//!   recipient when it is delivered: messages, deletions, reactions, sounds, voice members (a
//!   member in a channel I cannot see is "not in voice" for me, `voice_view`);
//! - every read and write by id answers `404` like an unknown id: history, sending (WebSocket and
//!   HTTP), images of its messages, reactions, deleting a message, voice tokens, `VoiceState`,
//!   moderation (move into it; disconnect / move / stop-stream only see the voice channels the
//!   moderator may see);
//! - mentions: only people with access are kept on a message, so nobody else is notified;
//! - LiveKit: no join token without access, and whoever is in a private room without access is
//!   removed at once ([`enforce`] after every change, and `voice_guard.rs` every few seconds for a
//!   token minted before the change).
//!
//! **Storage** (additive and idempotent, see [`init`]): `channels.private`, and the lists in
//! `channel_access_roles` / `channel_access_users` (a trigger drops them with their channel, from
//! any hub).
//!
//! **Downgrade guard.** An older hub (v0.8.x and before) knows nothing of all this: run on a
//! database with private channels, it would show them to everybody. Older hubs cannot be changed,
//! so this hub makes them refuse to start instead: while any channel is private, the `channels`
//! table holds one extra row ([`GUARD_ID`]) whose `position` is text. Every older hub reads the
//! channel list at start with `position` as an integer, fails on that row and exits ("Invalid
//! column type Text ... position"), so the private channels are never served by it. The row goes
//! as soon as no channel is private (then an older hub runs on the file as before). This hub skips
//! the row everywhere (`id > 0`). `server_meta.min_hub_version` says the same in words, for people
//! and for later hubs. The LEIA-ME (`hub/README.md`) warns about it too.
//!
//! [`Shared::set_channels`]: crate::state::Shared::set_channels
//! [`Out::Access`]: crate::state::Out::Access
//! [`Out::Channel`]: crate::state::Out::Channel
//! [`Out::Voice`]: crate::state::Out::Voice

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use rusqlite::{params, Connection, OptionalExtension};
use vox_proto::{Channel, ChannelId, ChannelKind, PrivateAccess, Role, UserId};

use crate::db;
use crate::livekit_admin::LkError;
use crate::state::AppState;

/// The extra `channels` row that keeps older hubs from serving private channels (module docs).
pub const GUARD_ID: i64 = -1;
/// Its `position`: text, which no older hub can read as the integer it expects. (Written into the
/// database and meant for the old `voxhub-server` builds: kept as it was after the rename.)
const GUARD_POSITION: &str = "canais privados: use o voxhub-server v0.9.0 ou mais novo";
const GUARD_NAME: &str = "(canais privados: servidor v0.9.0 ou mais novo)";
/// `server_meta` key: the oldest hub version that may open this database.
pub const MIN_HUB_VERSION_KEY: &str = "min_hub_version";
/// What it says while a channel is private.
pub const PRIVATE_MIN_HUB_VERSION: &str = "0.9.0";
/// Most people one channel's list names.
pub const MAX_USERS_PER_CHANNEL: usize = 500;

// ---------------------------------------------------------------- the rules

/// Who may see what, as of the last change. Cheap to share (`Arc`), never changed in place.
#[derive(Debug, Default)]
pub struct Rules {
    /// Every channel there is, with its list when private.
    channels: HashMap<ChannelId, Option<PrivateAccess>>,
    /// Channels that were private when they were deleted: an event about them still in flight is
    /// held to their old list (they never become "public" by going away).
    gone_private: HashMap<ChannelId, PrivateAccess>,
    /// Every account's effective role (an account not in here is a new Membro).
    roles: HashMap<UserId, Role>,
}

impl Rules {
    /// The role access is checked against.
    pub fn role(&self, user: UserId) -> Role {
        self.roles.get(&user).copied().unwrap_or(Role::Member)
    }

    /// May `user` see channel `ch`? The one access check of the hub.
    pub fn can_see(&self, user: UserId, ch: ChannelId) -> bool {
        let list = match self.channels.get(&ch) {
            Some(p) => p.as_ref(),
            None => self.gone_private.get(&ch),
        };
        list.is_none_or(|p| p.allows(user, self.role(user)))
    }

    pub fn is_private(&self, ch: ChannelId) -> bool {
        matches!(self.channels.get(&ch), Some(Some(_)))
    }

    /// The same rules for a new channel list.
    fn with_channels(&self, list: &[Channel]) -> Rules {
        let channels: HashMap<ChannelId, Option<PrivateAccess>> = list.iter().map(|c| (c.id, c.private.clone())).collect();
        let mut gone_private = self.gone_private.clone();
        for (id, p) in &self.channels {
            if let (false, Some(p)) = (channels.contains_key(id), p) {
                gone_private.insert(*id, p.clone());
            }
        }
        gone_private.retain(|id, _| !channels.contains_key(id));
        Rules { channels, gone_private, roles: self.roles.clone() }
    }
}

/// The hub's current [`Rules`].
#[derive(Default)]
pub struct Access {
    rules: RwLock<Arc<Rules>>,
}

impl Access {
    pub fn rules(&self) -> Arc<Rules> {
        self.rules.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// New channel list (the caller then tells every connection, `Out::Access`).
    pub(crate) fn set_channels(&self, list: &[Channel]) {
        let mut g = self.rules.write().unwrap_or_else(|e| e.into_inner());
        *g = Arc::new(g.with_channels(list));
    }

    /// New roles. Returns whether anything changed.
    pub(crate) fn set_roles(&self, roles: HashMap<UserId, Role>) -> bool {
        let mut g = self.rules.write().unwrap_or_else(|e| e.into_inner());
        if g.roles == roles {
            return false;
        }
        *g = Arc::new(Rules { channels: g.channels.clone(), gone_private: g.gone_private.clone(), roles });
        true
    }
}

/// What a recipient is told of a voice member's change ([`crate::state::Out::Voice`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceView {
    /// The member as they are.
    Full,
    /// "Left voice": they went somewhere the recipient may not see.
    Left,
    /// Nothing: it all happened where the recipient may not see.
    Nothing,
}

/// `member` is now in `ch` and was in `prev` (the same channel for a change inside a room). Each
/// person sees themselves as they are; anybody else only sees voice channels they may see.
pub fn voice_view(rules: &Rules, recipient: UserId, member: UserId, ch: Option<ChannelId>, prev: Option<ChannelId>) -> VoiceView {
    if recipient == member {
        return VoiceView::Full;
    }
    let sees = |c: Option<ChannelId>| c.is_some_and(|c| rules.can_see(recipient, c));
    match ch {
        _ if sees(ch) => VoiceView::Full,
        // Left voice: news only to whoever saw where they were.
        None if prev.is_none() || sees(prev) => VoiceView::Full,
        Some(_) if prev != ch && sees(prev) => VoiceView::Left,
        _ => VoiceView::Nothing,
    }
}

// ---------------------------------------------------------------- roles

/// Every account's effective role, from the database.
async fn load_roles(s: &AppState) -> anyhow::Result<HashMap<UserId, Role>> {
    let owner = s.owner();
    let rows = s.db.call(|c| db::list_users(c)).await?;
    Ok(rows.iter().map(|r| (r.id, r.effective_role(&owner))).collect())
}

/// Read every account's role again (after a role change, a new owner, a personal password set or
/// reset, a login); when anything changed, everybody's view is recomputed and whoever lost access
/// is taken out of the voice room at once.
pub async fn reload_roles(s: &AppState) {
    match load_roles(s).await {
        Ok(roles) => {
            if s.set_roles(roles) {
                enforce(s).await;
            }
        }
        Err(e) => tracing::error!("reading the roles for private channels: {e:#}"),
    }
}

/// At start (nothing to tell anybody yet).
pub async fn init_roles(s: &AppState) -> anyhow::Result<()> {
    let roles = load_roles(s).await?;
    s.access.set_roles(roles);
    Ok(())
}

// ---------------------------------------------------------------- enforcement

/// Take everybody out of the voice rooms they may no longer see: the hub's voice list at once,
/// and LiveKit (listed or not) through its server API.
pub async fn enforce(s: &AppState) {
    let rules = s.access.rules();
    let mut out: Vec<(UserId, ChannelId)> = s
        .voice_members()
        .into_iter()
        .filter_map(|m| m.channel_id.filter(|ch| !rules.can_see(m.user.id, *ch)).map(|ch| (m.user.id, ch)))
        .collect();
    for &(user, ch) in &out {
        tracing::info!(user, channel = ch, "lost access to a private voice channel: taken out of it");
        if s.voice_of(user).and_then(|m| m.channel_id) == Some(ch) {
            s.force_leave(user);
        }
    }
    // Whoever LiveKit has in a private room without access (listed or not).
    for c in s.channels().into_iter().filter(|c| c.kind == ChannelKind::Voice && rules.is_private(c.id)) {
        if let Ok(list) = s.livekit.list_participants(&crate::voice_guard::voice_room(c.id)).await {
            out.extend(list.iter().filter_map(|p| p.identity.parse::<UserId>().ok()).filter(|u| !rules.can_see(*u, c.id)).map(|u| (u, c.id)));
        }
    }
    out.sort_unstable();
    out.dedup();
    for (user, ch) in out {
        match s.livekit.remove(&crate::voice_guard::voice_room(ch), &user.to_string()).await {
            Ok(()) => tracing::info!(user, channel = ch, "livekit: removed from a private room (no access)"),
            Err(LkError::NotFound) | Err(LkError::Disabled) => {}
            Err(LkError::Other(e)) => tracing::warn!(user, channel = ch, error = %e, "livekit: removing from a private room failed"),
        }
    }
    // Whoever lost access may hold a join token from before (v1.0.1): watch the rooms closely
    // while it could still be used.
    crate::voice_guard::watch_closely(s);
}

// ---------------------------------------------------------------- storage

/// Additive and idempotent: one column with a default, two tables, one trigger. Run after
/// `channels` exists, on every start.
pub fn init(conn: &Connection) -> rusqlite::Result<()> {
    if !db::has_column(conn, "channels", "private")? {
        conn.execute_batch("ALTER TABLE channels ADD COLUMN private INTEGER NOT NULL DEFAULT 0")?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS channel_access_roles (
            channel_id INTEGER NOT NULL,
            role TEXT NOT NULL,
            PRIMARY KEY (channel_id, role)
        );
        CREATE TABLE IF NOT EXISTS channel_access_users (
            channel_id INTEGER NOT NULL,
            user_id INTEGER NOT NULL,
            PRIMARY KEY (channel_id, user_id)
        );
        CREATE TRIGGER IF NOT EXISTS channel_access_follow_channel AFTER DELETE ON channels
         BEGIN
           DELETE FROM channel_access_roles WHERE channel_id = OLD.id;
           DELETE FROM channel_access_users WHERE channel_id = OLD.id;
         END;",
    )?;
    sync_guard(conn)
}

/// Fill [`Channel::private`] of a channel list read from `channels`.
pub fn fill(conn: &Connection, list: &mut [Channel]) -> rusqlite::Result<()> {
    let private: HashSet<ChannelId> = {
        let mut st = conn.prepare("SELECT id FROM channels WHERE private != 0 AND id > 0")?;
        let rows = st.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    if private.is_empty() {
        return Ok(());
    }
    let mut lists: HashMap<ChannelId, PrivateAccess> = private.iter().map(|id| (*id, PrivateAccess::default())).collect();
    let mut st = conn.prepare("SELECT channel_id, role FROM channel_access_roles")?;
    for r in st.query_map([], |r| Ok((r.get::<_, ChannelId>(0)?, r.get::<_, String>(1)?)))? {
        let (ch, role) = r?;
        if let Some(p) = lists.get_mut(&ch) {
            p.roles.push(Role::from_wire(&role));
        }
    }
    let mut st = conn.prepare("SELECT channel_id, user_id FROM channel_access_users")?;
    for r in st.query_map([], |r| Ok((r.get::<_, ChannelId>(0)?, r.get::<_, UserId>(1)?)))? {
        let (ch, user) = r?;
        if let Some(p) = lists.get_mut(&ch) {
            p.users.push(user);
        }
    }
    for c in list {
        c.private = lists.get(&c.id).map(PrivateAccess::normalized);
    }
    Ok(())
}

/// Make channel `id` private with `list` (only real accounts are kept), or public (`None`), and
/// keep the downgrade guard in step. Returns the stored list. Run it inside the caller's
/// transaction when there is one.
pub fn store(conn: &Connection, id: ChannelId, list: Option<&PrivateAccess>) -> rusqlite::Result<Option<PrivateAccess>> {
    conn.execute_batch("SAVEPOINT channel_access")?;
    let result = (|| {
        conn.execute("DELETE FROM channel_access_roles WHERE channel_id = ?", [id])?;
        conn.execute("DELETE FROM channel_access_users WHERE channel_id = ?", [id])?;
        let stored = match list {
            None => None,
            Some(p) => {
                let mut p = p.normalized();
                let mut users = Vec::new();
                for u in p.users.iter().copied() {
                    if users.len() < MAX_USERS_PER_CHANNEL && db::get_user(conn, u)?.is_some() {
                        users.push(u);
                    }
                }
                p.users = users;
                for r in &p.roles {
                    conn.execute("INSERT INTO channel_access_roles (channel_id, role) VALUES (?, ?)", params![id, r.wire_name()])?;
                }
                for u in &p.users {
                    conn.execute("INSERT INTO channel_access_users (channel_id, user_id) VALUES (?, ?)", params![id, u])?;
                }
                Some(p)
            }
        };
        conn.execute("UPDATE channels SET private = ? WHERE id = ?", params![stored.is_some() as i64, id])?;
        sync_guard(conn)?;
        Ok(stored)
    })();
    match result {
        Ok(v) => {
            conn.execute_batch("RELEASE channel_access")?;
            Ok(v)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK TO channel_access; RELEASE channel_access");
            Err(e)
        }
    }
}

/// The downgrade guard row and `min_hub_version` exist exactly while a channel is private.
pub fn sync_guard(conn: &Connection) -> rusqlite::Result<()> {
    let any_private = conn.query_row("SELECT 1 FROM channels WHERE private != 0 AND id > 0 LIMIT 1", [], |_| Ok(())).optional()?.is_some();
    let has_meta = conn.query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'server_meta'", [], |_| Ok(())).optional()?.is_some();
    if any_private {
        // `position` is text on purpose: older hubs read it as an integer and refuse to start.
        conn.execute(
            "INSERT OR REPLACE INTO channels (id, name, kind, position, private) VALUES (?, ?, 'text', ?, 1)",
            params![GUARD_ID, GUARD_NAME, GUARD_POSITION],
        )?;
        if has_meta {
            conn.execute("INSERT OR REPLACE INTO server_meta (key, value) VALUES (?, ?)", params![MIN_HUB_VERSION_KEY, PRIVATE_MIN_HUB_VERSION])?;
        }
    } else {
        conn.execute("DELETE FROM channels WHERE id = ?", [GUARD_ID])?;
        if has_meta {
            conn.execute("DELETE FROM server_meta WHERE key = ?", [MIN_HUB_VERSION_KEY])?;
        }
    }
    Ok(())
}

/// `server_meta.min_hub_version`, if set.
pub fn min_hub_version(conn: &Connection) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT value FROM server_meta WHERE key = ?", [MIN_HUB_VERSION_KEY], |r| r.get(0)).optional()
}

/// "1.2.3" -> (1, 2, 3); anything unreadable is 0.
fn version_tuple(v: &str) -> (u32, u32, u32) {
    let mut it = v.trim().split('.').map(|p| p.trim().parse::<u32>().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
}

/// Is `this` hub version older than what the database asks for?
pub fn too_old(this: &str, wanted: &str) -> bool {
    version_tuple(this) < version_tuple(wanted)
}

/// Who has access, in Portuguese, for the moderation log.
pub fn describe(list: Option<&PrivateAccess>, names: &[String]) -> String {
    let Some(p) = list else { return "público".into() };
    let roles: Vec<&str> = p.roles.iter().map(|r| if *r == Role::Moderator { "Moderador" } else { "Membro" }).collect();
    let mut out = String::from("privado; cargos: Dono, Admin");
    for r in roles {
        out.push_str(", ");
        out.push_str(r);
    }
    if !names.is_empty() {
        out.push_str("; pessoas: ");
        out.push_str(&names.join(", "));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(id: ChannelId, private: Option<PrivateAccess>) -> Channel {
        Channel { id, name: format!("c{id}"), kind: ChannelKind::Voice, position: id as i32, private, e2ee: false }
    }

    fn rules(list: &[Channel], roles: &[(UserId, Role)]) -> Rules {
        let mut r = Rules::default().with_channels(list);
        r.roles = roles.iter().copied().collect();
        r
    }

    #[test]
    fn access_by_role_by_name_and_staff_always() {
        let members = PrivateAccess { roles: vec![Role::Member], users: vec![] };
        let named = PrivateAccess { roles: vec![], users: vec![5] };
        let r = rules(&[ch(1, None), ch(2, Some(members)), ch(3, Some(named))], &[(1, Role::Owner), (2, Role::Admin), (3, Role::Moderator), (4, Role::Member), (5, Role::Member)]);
        assert!((1..=5).all(|u| r.can_see(u, 1)), "public");
        assert!(r.can_see(4, 2) && r.can_see(99, 2), "Membro by role (an unknown account is a Membro)");
        assert!(!r.can_see(3, 2), "Moderador is not Membro");
        assert!(r.can_see(5, 3) && !r.can_see(4, 3) && !r.can_see(3, 3));
        assert!(r.can_see(1, 3) && r.can_see(2, 3), "Dono and Admin always");
        assert!(r.is_private(2) && !r.is_private(1));
    }

    #[test]
    fn a_deleted_private_channel_stays_private() {
        let only_staff = Some(PrivateAccess::default());
        let r = rules(&[ch(1, None), ch(2, only_staff)], &[(7, Role::Member)]);
        assert!(!r.can_see(7, 2));
        let after = r.with_channels(&[ch(1, None)]);
        assert!(!after.can_see(7, 2), "events still in flight about it reach nobody without access");
        assert!(after.can_see(7, 42), "an unknown (deleted public) channel");
        // Made public, then deleted: public.
        let public = r.with_channels(&[ch(1, None), ch(2, None)]).with_channels(&[ch(1, None)]);
        assert!(public.can_see(7, 2));
    }

    #[test]
    fn voice_members_in_hidden_channels_are_not_in_voice() {
        use VoiceView::*;
        let r = rules(&[ch(1, None), ch(2, Some(PrivateAccess::default()))], &[(9, Role::Admin)]);
        let me = 7;
        // A member (8) moving around, as I (a Membro without access to 2) see it.
        assert_eq!(voice_view(&r, me, 8, Some(1), None), Full, "joins a public room");
        assert_eq!(voice_view(&r, me, 8, Some(2), Some(1)), Left, "public -> private: left voice");
        assert_eq!(voice_view(&r, me, 8, Some(2), Some(2)), Nothing, "mutes inside the private room");
        assert_eq!(voice_view(&r, me, 8, Some(2), None), Nothing, "joins the private room");
        assert_eq!(voice_view(&r, me, 8, None, Some(2)), Nothing, "leaves the private room");
        assert_eq!(voice_view(&r, me, 8, Some(1), Some(2)), Full, "private -> public: appears");
        assert_eq!(voice_view(&r, me, 8, None, Some(1)), Full, "leaves a public room");
        assert_eq!(voice_view(&r, me, 8, None, None), Full);
        // Myself, always; an admin sees everything.
        assert_eq!(voice_view(&r, 8, 8, Some(2), None), Full);
        assert_eq!(voice_view(&r, 9, 8, Some(2), Some(2)), Full);
    }

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, username TEXT, role TEXT NOT NULL DEFAULT 'member', password_hash TEXT, reset_hash TEXT,
                reset_expires_ms INTEGER, server_muted INTEGER NOT NULL DEFAULT 0, created_at_ms INTEGER NOT NULL DEFAULT 0, avatar_version TEXT, display_name TEXT);
             CREATE TABLE channels (id INTEGER PRIMARY KEY, name TEXT NOT NULL, kind TEXT NOT NULL CHECK (kind IN ('text','voice')), position INTEGER NOT NULL);
             CREATE TABLE server_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO users (id, username) VALUES (3, 'Ana'), (4, 'Bia');
             INSERT INTO channels (id, name, kind, position) VALUES (1, 'geral', 'text', 0), (2, 'Sala', 'voice', 1);",
        )
        .unwrap();
        init(&c).unwrap();
        init(&c).unwrap(); // idempotent
        c
    }

    fn listed(c: &Connection) -> Vec<Channel> {
        let mut list = vec![ch(1, None), ch(2, None)];
        fill(c, &mut list).unwrap();
        list
    }

    #[test]
    fn stored_lists_round_trip_and_the_guard_follows_them() {
        let c = mem();
        assert!(listed(&c).iter().all(|c| c.private.is_none()));
        assert_eq!(c.query_row("SELECT COUNT(*) FROM channels WHERE id = ?", [GUARD_ID], |r| r.get::<_, i64>(0)).unwrap(), 0, "no guard while nothing is private");
        let asked = PrivateAccess { roles: vec![Role::Member, Role::Admin], users: vec![4, 99, 3, 4] };
        let stored = store(&c, 2, Some(&asked)).unwrap();
        assert_eq!(stored, Some(PrivateAccess { roles: vec![Role::Member], users: vec![3, 4] }), "unknown accounts and staff roles dropped");
        assert_eq!(listed(&c)[1].private, stored);
        // The guard: a row whose position no integer reader accepts, and the version marker.
        let pos_type: String = c.query_row("SELECT typeof(position) FROM channels WHERE id = ?", [GUARD_ID], |r| r.get(0)).unwrap();
        assert_eq!(pos_type, "text");
        assert!(c.query_row("SELECT position FROM channels WHERE id = ?", [GUARD_ID], |r| r.get::<_, i32>(0)).is_err(), "an older hub's read fails");
        assert_eq!(min_hub_version(&c).unwrap().as_deref(), Some(PRIVATE_MIN_HUB_VERSION));
        // Public again: the lists, the guard and the marker go.
        assert_eq!(store(&c, 2, None).unwrap(), None);
        assert!(listed(&c)[1].private.is_none());
        assert_eq!(c.query_row("SELECT COUNT(*) FROM channel_access_users", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        assert_eq!(c.query_row("SELECT COUNT(*) FROM channels WHERE id = ?", [GUARD_ID], |r| r.get::<_, i64>(0)).unwrap(), 0);
        assert_eq!(min_hub_version(&c).unwrap(), None);
        // Deleting a private channel (from any hub) takes its list with it.
        store(&c, 2, Some(&PrivateAccess { roles: vec![], users: vec![3] })).unwrap();
        c.execute("DELETE FROM channels WHERE id = 2", []).unwrap();
        assert_eq!(c.query_row("SELECT COUNT(*) FROM channel_access_users", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        sync_guard(&c).unwrap();
        assert_eq!(c.query_row("SELECT COUNT(*) FROM channels WHERE id = ?", [GUARD_ID], |r| r.get::<_, i64>(0)).unwrap(), 0);
    }

    #[test]
    fn versions_and_log_lines() {
        assert!(too_old("0.8.2", "0.9.0") && !too_old("0.9.0", "0.9.0") && !too_old("0.10.0", "0.9.0") && !too_old("1.0.0", "0.9.1"));
        assert_eq!(describe(None, &[]), "público");
        let p = PrivateAccess { roles: vec![Role::Moderator, Role::Member], users: vec![3] };
        assert_eq!(describe(Some(&p), &["Ana".into()]), "privado; cargos: Dono, Admin, Moderador, Membro; pessoas: Ana");
        assert_eq!(describe(Some(&PrivateAccess::default()), &[]), "privado; cargos: Dono, Admin");
    }
}
