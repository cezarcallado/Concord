//! Invites (v0.8, "Convidar amigos"): the token that lets a new person in without the server
//! password. The rules and the code format are in `vox_proto::invite`; this is the hub side.
//!
//! - `invites`: one row per invite, the token stored as a sha256 hash (like sessions), with its
//!   expiry, use limit, uses and revocation.
//! - `invite_members`: the people who came in through an invite. A remembered device
//!   (`device_tokens`) of one of them lets that computer in without the server password
//!   ([`admit`]): that is how a friend who only ever had the invite keeps getting back in. The
//!   server password itself still rules everybody else, so changing it keeps locking out who it
//!   always locked out.
//!
//! Schema: two new tables created with `IF NOT EXISTS`, nothing existing changes, so a v0.7.0 hub
//! still runs on a migrated database (it never reads them; invite members then need the server
//! password there, like before invites existed).

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::Json;
use rusqlite::{params, Connection, OptionalExtension};
use vox_proto::*;

use crate::admin::audit;
use crate::api::{too_many, ApiErr, Authed};
use crate::auth::ip_key;
use crate::state::AppState;
use crate::util::{ct_eq, now_ms, random_bytes, sha256_hex};

type ApiResult<T> = Result<Json<T>, ApiErr>;

/// Shortest validity a new invite may ask for.
const MIN_EXPIRY_SECS: u64 = 60;

pub fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS invites (
            id INTEGER PRIMARY KEY,
            token_hash TEXT NOT NULL UNIQUE,
            created_by INTEGER NOT NULL REFERENCES users(id),
            created_at_ms INTEGER NOT NULL,
            expires_at_ms INTEGER,
            max_uses INTEGER,
            uses INTEGER NOT NULL DEFAULT 0,
            revoked_at_ms INTEGER
        );
        CREATE TABLE IF NOT EXISTS invite_members (
            user_id INTEGER PRIMARY KEY REFERENCES users(id),
            invite_id INTEGER NOT NULL,
            joined_at_ms INTEGER NOT NULL
        );",
    )
}

/// The token a person typed, as stored: the hash of its canonical (lowercase) hex. `None` when it
/// is not an invite token at all.
fn token_hash(hex: &str) -> Option<String> {
    let t = invite_token_from_hex(hex)?;
    // The salt keeps its pre-concord name: stored invites were hashed with it.
    Some(sha256_hex(&format!("voxhub-invite:{}", invite_token_hex(&t))))
}

const INVITE_COLS: &str = "i.id, i.created_by, u.username, i.created_at_ms, i.expires_at_ms, i.max_uses, i.uses, i.revoked_at_ms";

fn invite_row(r: &rusqlite::Row) -> rusqlite::Result<Invite> {
    Ok(Invite {
        id: r.get(0)?,
        created_by: r.get(1)?,
        created_by_name: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
        created_at_ms: r.get(3)?,
        expires_at_ms: r.get(4)?,
        max_uses: r.get::<_, Option<i64>>(5)?.map(|n| n.clamp(0, u32::MAX as i64) as u32),
        uses: r.get::<_, i64>(6)?.clamp(0, u32::MAX as i64) as u32,
        revoked: r.get::<_, Option<i64>>(7)?.is_some(),
    })
}

fn find_by_hash(conn: &Connection, hash: &str) -> rusqlite::Result<Option<Invite>> {
    conn.query_row(
        &format!("SELECT {INVITE_COLS} FROM invites i LEFT JOIN users u ON u.id = i.created_by WHERE i.token_hash = ?"),
        [hash],
        invite_row,
    )
    .optional()
}

fn get(conn: &Connection, id: InviteId) -> rusqlite::Result<Option<Invite>> {
    conn.query_row(&format!("SELECT {INVITE_COLS} FROM invites i LEFT JOIN users u ON u.id = i.created_by WHERE i.id = ?"), [id], invite_row).optional()
}

pub fn list(conn: &Connection, limit: i64) -> rusqlite::Result<Vec<Invite>> {
    let mut st = conn.prepare(&format!("SELECT {INVITE_COLS} FROM invites i LEFT JOIN users u ON u.id = i.created_by ORDER BY i.id DESC LIMIT ?"))?;
    let rows = st.query_map([limit], invite_row)?;
    rows.collect()
}

fn count_active(conn: &Connection, now: i64) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM invites WHERE revoked_at_ms IS NULL AND (expires_at_ms IS NULL OR expires_at_ms > ?1) AND (max_uses IS NULL OR uses < max_uses)",
        [now],
        |r| r.get(0),
    )
}

pub fn insert(conn: &Connection, hash: &str, by: UserId, expires_at_ms: Option<i64>, max_uses: Option<u32>) -> rusqlite::Result<Invite> {
    let id = crate::ids::insert_with(conn, crate::ids::Seq::Invites, |id| {
        conn.execute(
            "INSERT INTO invites (id, token_hash, created_by, created_at_ms, expires_at_ms, max_uses) VALUES (?, ?, ?, ?, ?, ?)",
            params![id, hash, by, now_ms(), expires_at_ms, max_uses],
        )?;
        Ok(id)
    })?;
    get(conn, id).map(|i| i.expect("just inserted"))
}

/// Revoke (idempotent). `false`: no such invite.
pub fn revoke_row(conn: &Connection, id: InviteId) -> rusqlite::Result<bool> {
    if get(conn, id)?.is_none() {
        return Ok(false);
    }
    conn.execute("UPDATE invites SET revoked_at_ms = ? WHERE id = ? AND revoked_at_ms IS NULL", params![now_ms(), id])?;
    Ok(true)
}

/// Take one use of the invite, if it still works (one statement: two people racing for the last
/// use cannot both get it). `Err` carries why it does not work.
pub fn consume(conn: &Connection, id: InviteId, now: i64) -> rusqlite::Result<Result<(), &'static str>> {
    let n = conn.execute(
        "UPDATE invites SET uses = uses + 1 WHERE id = ?1 AND revoked_at_ms IS NULL
         AND (expires_at_ms IS NULL OR expires_at_ms > ?2) AND (max_uses IS NULL OR uses < max_uses)",
        params![id, now],
    )?;
    if n == 1 {
        return Ok(Ok(()));
    }
    Ok(Err(match get(conn, id)? {
        Some(i) => state_error(i.state(now)).unwrap_or(ERR_INVITE_INVALID),
        None => ERR_INVITE_INVALID,
    }))
}

pub fn add_member(conn: &Connection, user_id: UserId, invite_id: InviteId) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO invite_members (user_id, invite_id, joined_at_ms) VALUES (?, ?, ?) ON CONFLICT(user_id) DO NOTHING",
        params![user_id, invite_id, now_ms()],
    )?;
    Ok(())
}

pub fn is_member(conn: &Connection, user_id: UserId) -> rusqlite::Result<bool> {
    conn.query_row("SELECT 1 FROM invite_members WHERE user_id = ?", [user_id], |_| Ok(())).optional().map(|r| r.is_some())
}

fn state_error(state: InviteState) -> Option<&'static str> {
    match state {
        InviteState::Active => None,
        InviteState::Expired => Some(ERR_INVITE_EXPIRED),
        InviteState::UsedUp => Some(ERR_INVITE_USED_UP),
        InviteState::Revoked => Some(ERR_INVITE_REVOKED),
    }
}

/// The invite a token belongs to, if it works right now; otherwise why not.
async fn check_token(s: &AppState, token: &str) -> anyhow::Result<Result<Invite, &'static str>> {
    let Some(hash) = token_hash(token) else { return Ok(Err(ERR_INVITE_INVALID)) };
    let found = s.db.call(move |c| find_by_hash(c, &hash)).await?;
    Ok(match found {
        None => Err(ERR_INVITE_INVALID),
        Some(i) => match state_error(i.state(now_ms())) {
            None => Ok(i),
            Some(e) => Err(e),
        },
    })
}

// ---------------------------------------------------------------- login

/// How a login got past the server password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// The server password (or the server has none).
    Password,
    /// A working invite (its use is taken by [`finish_login`]).
    Invite(InviteId),
    /// A remembered device of somebody who came in through an invite.
    Device,
    /// v1.0.1: a working invite, but the name already has an account. An invite only creates new
    /// accounts, so this admits nobody by itself: the account's own proof (personal password,
    /// remembered device, reset code) must follow, and even then the invite is not used and the
    /// account does not become an invite member (`api::login`). Before, an invite login on an
    /// existing name without a personal password marked that account as an invite member, which
    /// gave the invite holder a device token that outlived the invite and server-password changes.
    ExistingName,
}

/// The server-password step of a login: the password, else a remembered device of an invite
/// member (no use of an invite is taken for somebody already in), else the invite (new names
/// only: [`Admission::ExistingName`]). `Ok(Err(..))` is the refusal to send (the caller counts
/// it as a failure).
pub async fn admit(s: &AppState, req: &LoginRequest, username: &str) -> anyhow::Result<Result<Admission, ApiErr>> {
    if ct_eq(&req.server_password, &s.cfg.server_password) {
        return Ok(Ok(Admission::Password));
    }
    if let Some(device) = req.device_token.as_deref() {
        let (name, hash) = (username.to_string(), sha256_hex(device));
        let ok = s
            .db
            .call(move |c| {
                let Some(row) = crate::db::find_user(c, &name)? else { return Ok(false) };
                Ok(is_member(c, row.id)? && crate::db::use_device_token(c, row.id, &hash)?)
            })
            .await?;
        if ok {
            return Ok(Ok(Admission::Device));
        }
    }
    if let Some(token) = req.invite.as_deref().filter(|t| !t.trim().is_empty()) {
        return Ok(match check_token(s, token).await? {
            Ok(i) => {
                let name = username.to_string();
                let exists = s.db.call(move |c| crate::db::find_user(c, &name)).await?.is_some();
                Ok(if exists { Admission::ExistingName } else { Admission::Invite(i.id) })
            }
            Err(e) => Err(ApiErr::new(StatusCode::UNAUTHORIZED, e)),
        });
    }
    Ok(Err(ApiErr::new(StatusCode::UNAUTHORIZED, "wrong server password")))
}

/// Before the session is made: take the invite's use (it may have run out meanwhile).
pub async fn take_use(s: &AppState, admission: Admission) -> Result<(), ApiErr> {
    let Admission::Invite(id) = admission else { return Ok(()) };
    match s.db.call(move |c| consume(c, id, now_ms())).await? {
        Ok(()) => Ok(()),
        Err(e) => Err(ApiErr::new(StatusCode::UNAUTHORIZED, e)),
    }
}

/// After the session is made: remember that `user` came in through the invite (only a new
/// account: the caller makes the session with [`crate::db::login_new`] for an invite).
pub async fn finish_login(s: &AppState, admission: Admission, user: UserId) -> anyhow::Result<()> {
    let Admission::Invite(id) = admission else { return Ok(()) };
    s.db.call(move |c| add_member(c, user, id)).await?;
    audit(s, Some(user), AUDIT_INVITE_JOIN, None, format!("#{id}")).await;
    Ok(())
}

// ---------------------------------------------------------------- HTTP

fn require_admin(me: &Authed) -> Result<(), ApiErr> {
    if me.role.can_admin() {
        Ok(())
    } else {
        Err(ApiErr::new(StatusCode::FORBIDDEN, "your role does not allow this"))
    }
}

/// `POST /api/invites`
pub async fn create(State(s): State<AppState>, me: Authed, headers: HeaderMap, body: Result<Json<InviteCreateRequest>, JsonRejection>) -> ApiResult<InviteCreated> {
    let Json(req) = body?;
    require_admin(&me)?;
    if req.expires_secs.is_some_and(|e| !(MIN_EXPIRY_SECS..=INVITE_MAX_EXPIRY_SECS).contains(&e)) {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, "expires_secs out of range"));
    }
    if req.max_uses.is_some_and(|m| !(1..=INVITE_MAX_USES).contains(&m)) {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, "max_uses out of range"));
    }
    let token = random_bytes::<INVITE_TOKEN_LEN>();
    let hex = invite_token_hex(&token);
    let hash = token_hash(&hex).expect("valid token");
    let now = now_ms();
    let expires = req.expires_secs.map(|e| now + (e as i64) * 1000);
    let (by, max) = (me.user.id, req.max_uses);
    let invite = s
        .db
        .call(move |c| {
            if count_active(c, now)? >= INVITE_MAX_ACTIVE as i64 {
                return Ok(None);
            }
            insert(c, &hash, by, expires, max).map(Some)
        })
        .await?
        .ok_or_else(|| ApiErr::new(StatusCode::CONFLICT, "too many active invites"))?;
    let detail = format!("#{} {} {}", invite.id, req.expires_secs.map_or("sem prazo".to_string(), |e| format!("{e}s")), max.map_or("ilimitado".to_string(), |m| format!("{m}x")));
    audit(&s, Some(by), AUDIT_INVITE_CREATE, None, detail).await;
    // v1.0.2: with no public address of its own, the hub's global IPv6 (`ipv6.rs`).
    let hints = crate::ipv6::with_ipv6(hints(&s, &headers), crate::listen::plan(&s.cfg.bind, s.cfg.ipv6).has_ipv6(), crate::ipv6::stable_ipv6());
    Ok(Json(InviteCreated { invite, token: hex, hints }))
}

/// `GET /api/invites`
pub async fn list_invites(State(s): State<AppState>, me: Authed) -> ApiResult<Vec<Invite>> {
    require_admin(&me)?;
    Ok(Json(s.db.call(|c| list(c, 100)).await?))
}

/// `POST /api/invites/{id}/revoke`
pub async fn revoke(State(s): State<AppState>, me: Authed, Path(id): Path<InviteId>) -> ApiResult<OkResponse> {
    require_admin(&me)?;
    if !s.db.call(move |c| revoke_row(c, id)).await? {
        return Err(ApiErr::new(StatusCode::NOT_FOUND, "invite not found"));
    }
    audit(&s, Some(me.user.id), AUDIT_INVITE_REVOKE, None, format!("#{id}")).await;
    Ok(Json(OkResponse { ok: true }))
}

/// `POST /api/invites/redeem`: does this token work? (No login; slowed down like logins.)
pub async fn redeem(
    State(s): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    body: Result<Json<InviteRedeemRequest>, JsonRejection>,
) -> ApiResult<InviteRedeemResponse> {
    let Json(req) = body?;
    let ip = ip_key(peer.ip());
    if let Some(wait) = s.throttle.must_wait(&ip) {
        return Err(too_many(wait));
    }
    match check_token(&s, &req.token).await? {
        Ok(i) => {
            s.throttle.succeed(&ip);
            Ok(Json(InviteRedeemResponse { server_name: s.cfg.server_name.clone(), expires_at_ms: i.expires_at_ms }))
        }
        Err(e) => {
            tracing::warn!(%peer, reason = e, "invite check refused");
            s.throttle.fail(&ip);
            tokio::time::sleep(Duration::from_millis(300)).await;
            Err(ApiErr::new(StatusCode::UNAUTHORIZED, e))
        }
    }
}

/// Where this hub can be reached, as far as it knows (the app completes it).
fn hints(s: &AppState, headers: &HeaderMap) -> InviteHints {
    let header_port = headers.get(header::HOST).and_then(|v| v.to_str().ok()).and_then(host_port);
    let bind_port = s.cfg.bind.rsplit_once(':').and_then(|(_, p)| p.parse().ok());
    let public = s.cfg.public_host.trim();
    let useless = public.is_empty()
        || public.eq_ignore_ascii_case("localhost")
        || public.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified());
    let lan = crate::host::net::lan_ipv4();
    InviteHints {
        hub_port: header_port.or(bind_port).unwrap_or(7890),
        public_host: if useless { String::new() } else { public.to_string() },
        lan_ipv4: lan.map(|(ip, _)| ip),
        lan_prefix: lan.map_or(0, |(_, p)| p),
        // v1.1 (T0): the code carries the certificate's pin (layout 2).
        tls_pin: s.tls_pin.get().map(vox_proto::tls_pin_hex),
    }
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
    rest.parse().ok().filter(|p| *p != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE users (id INTEGER PRIMARY KEY, username TEXT NOT NULL UNIQUE COLLATE NOCASE, created_at_ms INTEGER NOT NULL); INSERT INTO users VALUES (1, 'dono', 0), (2, 'Tati', 0);").unwrap();
        init(&c).unwrap();
        init(&c).unwrap(); // repeatable
        c
    }

    #[test]
    fn uses_expiry_and_revocation() {
        let c = db();
        let now = now_ms();
        let h = token_hash(&"ab".repeat(16)).unwrap();
        let i = insert(&c, &h, 1, Some(now + 60_000), Some(2)).unwrap();
        assert_eq!((i.uses, i.max_uses, i.created_by_name.as_str()), (0, Some(2), "dono"));
        assert_eq!(find_by_hash(&c, &h).unwrap().unwrap().id, i.id);
        assert_eq!(consume(&c, i.id, now).unwrap(), Ok(()));
        assert_eq!(consume(&c, i.id, now).unwrap(), Ok(()));
        assert_eq!(consume(&c, i.id, now).unwrap(), Err(ERR_INVITE_USED_UP));
        assert_eq!(get(&c, i.id).unwrap().unwrap().uses, 2, "a refused use is not counted");

        let h2 = token_hash(&"cd".repeat(16)).unwrap();
        let j = insert(&c, &h2, 1, Some(now + 1000), None).unwrap();
        assert_eq!(consume(&c, j.id, now + 999).unwrap(), Ok(()));
        assert_eq!(consume(&c, j.id, now + 1000).unwrap(), Err(ERR_INVITE_EXPIRED));
        assert_eq!(count_active(&c, now).unwrap(), 1);

        let h3 = token_hash(&"ef".repeat(16)).unwrap();
        let k = insert(&c, &h3, 1, None, None).unwrap();
        for _ in 0..30 {
            assert_eq!(consume(&c, k.id, now).unwrap(), Ok(()), "unlimited and never expiring");
        }
        assert!(revoke_row(&c, k.id).unwrap());
        assert!(revoke_row(&c, k.id).unwrap(), "revoking twice is fine");
        assert!(!revoke_row(&c, 999).unwrap());
        assert_eq!(consume(&c, k.id, now).unwrap(), Err(ERR_INVITE_REVOKED));
        assert_eq!(get(&c, k.id).unwrap().unwrap().state(now), InviteState::Revoked);
        assert_eq!(consume(&c, 999, now).unwrap(), Err(ERR_INVITE_INVALID));
        assert_eq!(list(&c, 10).unwrap().iter().map(|i| i.id).collect::<Vec<_>>(), [k.id, j.id, i.id]);

        assert!(!is_member(&c, 2).unwrap());
        add_member(&c, 2, i.id).unwrap();
        add_member(&c, 2, j.id).unwrap();
        assert!(is_member(&c, 2).unwrap());
    }

    #[test]
    fn tokens_are_canonical_hex() {
        assert_eq!(token_hash(&"AB".repeat(16)), token_hash(&"ab".repeat(16)));
        assert_eq!(token_hash("not a token"), None);
        assert_eq!(token_hash(&"ab".repeat(15)), None);
    }

    #[test]
    fn ports_from_the_host_header() {
        assert_eq!(host_port("203.0.113.5:7990"), Some(7990));
        assert_eq!(host_port("[2001:db8::1]:7890"), Some(7890));
        assert_eq!(host_port("example.org"), None);
        assert_eq!(host_port("2001:db8::1"), None);
        assert_eq!(host_port("a:0"), None);
    }
}
