//! End-to-end encryption, phase E0 (v1.1): the directory of device keys (the contract is in
//! `vox_proto::e2ee`). The hub only stores and hands out public keys: it never links any crypto of
//! the envelope (a test checks that `cargo tree -p voxhub-server` has no X25519) and never logs a
//! key.
//!
//! - `GET /api/e2ee/devices`: every member's devices.
//! - `PUT /api/e2ee/devices/{device_id}` `{public_key, label}`: register a device of the caller, or
//!   confirm it (same key: `last_seen_ms` refreshed, the label may change). A registered (id, key)
//!   pair never changes (`409` [`ERR_E2EE_KEY_CHANGED`]); at most [`E2EE_MAX_DEVICES`] per member
//!   (`409` [`ERR_E2EE_TOO_MANY_DEVICES`]).
//! - `DELETE /api/e2ee/devices/{device_id}`: only the caller's own devices (anybody else's is
//!   `404`, as a device that does not exist).
//! - PUT and DELETE share a rate limit per member; every change goes to everybody as
//!   `E2eeDevicesChanged` with that member's whole list.
//!
//! The concord hub has no way to remove a member yet (no kick or ban: accounts are never deleted).
//! Deleting a row of `users` drops that member's devices anyway (a trigger, see [`init`]); an
//! action that removes members must also tell everybody (`E2eeDevicesChanged` with an empty list)
//! or call [`forget_user`] and announce it.

use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};
use base64::Engine;
use rusqlite::{params, Connection, OptionalExtension};
use vox_proto::*;

use crate::api::{ApiErr, AuthUser};
use crate::profile::RateWindow;
use crate::state::AppState;
use crate::util::now_ms;

/// Registrations and removals per member: a login (or a reconnect) re-registers once, so this is
/// far above what an app does and far below a flood.
pub const CHANGES_PER_WINDOW: usize = 20;
pub const CHANGES_WINDOW: Duration = Duration::from_secs(60);
/// The largest PUT body (a 43-character key and a 40-character label, with room to spare).
pub const MAX_BODY: usize = 2048;

/// Per-hub state: the rate limit.
pub struct E2eeState {
    pub changes: RateWindow,
}

impl Default for E2eeState {
    fn default() -> Self {
        E2eeState { changes: RateWindow::new(CHANGES_PER_WINDOW, CHANGES_WINDOW) }
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/e2ee/devices", get(list))
        .route("/api/e2ee/devices/{device_id}", put(register).delete(remove).layer(DefaultBodyLimit::max(MAX_BODY)))
}

// ---------------------------------------------------------------- database

/// The directory (called at every start; safe to run any number of times). Purely additive: an
/// older hub never looks at it.
pub fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS e2ee_devices (
            user_id INTEGER NOT NULL REFERENCES users(id),
            device_id TEXT NOT NULL,
            public_key TEXT NOT NULL,
            label TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL,
            last_seen_ms INTEGER NOT NULL,
            PRIMARY KEY (user_id, device_id)
        );
        -- A member removed from the server takes their devices along (the hub has no such action
        -- yet; this covers any that comes, and a host removing a row by hand).
        CREATE TRIGGER IF NOT EXISTS e2ee_devices_member_gone AFTER DELETE ON users
         BEGIN DELETE FROM e2ee_devices WHERE user_id = OLD.id; END;",
    )
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<E2eeDevice> {
    Ok(E2eeDevice { user_id: r.get(0)?, device_id: r.get(1)?, public_key: r.get(2)?, label: r.get(3)?, created_at_ms: r.get(4)? })
}

const COLUMNS: &str = "user_id, device_id, public_key, label, created_at_ms";

/// Every member's devices, by member and then oldest first.
pub fn list_all(c: &Connection) -> rusqlite::Result<Vec<E2eeDevice>> {
    let mut st = c.prepare(&format!("SELECT {COLUMNS} FROM e2ee_devices ORDER BY user_id, created_at_ms, device_id"))?;
    let rows = st.query_map([], row)?.collect();
    rows
}

/// One member's devices, oldest first.
pub fn list_user(c: &Connection, user: UserId) -> rusqlite::Result<Vec<E2eeDevice>> {
    let mut st = c.prepare(&format!("SELECT {COLUMNS} FROM e2ee_devices WHERE user_id = ?1 ORDER BY created_at_ms, device_id"))?;
    let rows = st.query_map([user], row)?.collect();
    rows
}

/// What a registration did.
#[derive(Debug, PartialEq, Eq)]
pub enum Put {
    /// A new device.
    Created(E2eeDevice),
    /// The device was there with this key: seen again (`changed`: its label is new).
    Same { device: E2eeDevice, changed: bool },
    /// The id is registered with another key.
    KeyChanged,
    /// The member already has the most devices.
    TooMany,
}

/// Register (or confirm) a device of `user`, in one transaction.
pub fn put_device(c: &mut Connection, user: UserId, device_id: &str, key: &str, label: &str, now: i64) -> rusqlite::Result<Put> {
    let tx = c.transaction()?;
    let existing = tx
        .query_row(&format!("SELECT {COLUMNS} FROM e2ee_devices WHERE user_id = ?1 AND device_id = ?2"), params![user, device_id], row)
        .optional()?;
    let out = match existing {
        Some(d) if d.public_key != key => Put::KeyChanged,
        Some(mut d) => {
            tx.execute("UPDATE e2ee_devices SET last_seen_ms = ?3, label = ?4 WHERE user_id = ?1 AND device_id = ?2", params![user, device_id, now, label])?;
            let changed = d.label != label;
            d.label = label.to_string();
            Put::Same { device: d, changed }
        }
        None => {
            let n: i64 = tx.query_row("SELECT COUNT(*) FROM e2ee_devices WHERE user_id = ?1", [user], |r| r.get(0))?;
            if n as usize >= E2EE_MAX_DEVICES {
                Put::TooMany
            } else {
                tx.execute(
                    "INSERT INTO e2ee_devices (user_id, device_id, public_key, label, created_at_ms, last_seen_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                    params![user, device_id, key, label, now],
                )?;
                Put::Created(E2eeDevice { user_id: user, device_id: device_id.into(), public_key: key.into(), label: label.into(), created_at_ms: now })
            }
        }
    };
    tx.commit()?;
    Ok(out)
}

/// Remove one of `user`'s devices; whether there was one.
pub fn delete_device(c: &Connection, user: UserId, device_id: &str) -> rusqlite::Result<bool> {
    Ok(c.execute("DELETE FROM e2ee_devices WHERE user_id = ?1 AND device_id = ?2", params![user, device_id])? > 0)
}

/// Drop every device of `user` (for a member removed from the server; the hub has no such
/// action yet). Returns how many went.
pub fn forget_user(c: &Connection, user: UserId) -> rusqlite::Result<usize> {
    c.execute("DELETE FROM e2ee_devices WHERE user_id = ?1", [user])
}

// ---------------------------------------------------------------- checks

/// A public key as stored: canonical base64url without padding of exactly 32 bytes, not all zero.
pub fn check_key(key: &str) -> Option<String> {
    if key.len() != E2EE_KEY_B64_LEN {
        return None;
    }
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(key.as_bytes()).ok()?;
    if raw.len() != 32 || raw.iter().all(|b| *b == 0) {
        return None;
    }
    let canonical = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&raw);
    (canonical == key).then_some(canonical)
}

/// The first digits of a device id, for the log (never a key).
fn short(device_id: &str) -> &str {
    device_id.get(..8).unwrap_or(device_id)
}

fn changes_limited(s: &AppState, user: UserId) -> Result<(), ApiErr> {
    s.e2ee.changes.allow(user, Instant::now()).map_err(crate::api::too_many)
}

fn bad(msg: &str) -> ApiErr {
    ApiErr::new(StatusCode::BAD_REQUEST, msg)
}

/// Tell everybody `user`'s devices now.
async fn announce(s: &AppState, user: UserId) -> Result<(), ApiErr> {
    let devices = s.db.call(move |c| list_user(c, user)).await?;
    s.broadcast(&ServerEvent::E2eeDevicesChanged { user_id: user, devices });
    Ok(())
}

// ---------------------------------------------------------------- handlers

/// `GET /api/e2ee/devices`
async fn list(State(s): State<AppState>, _me: AuthUser) -> Result<Json<Vec<E2eeDevice>>, ApiErr> {
    Ok(Json(s.db.call(|c| list_all(c)).await?))
}

/// `PUT /api/e2ee/devices/{device_id}`
async fn register(
    State(s): State<AppState>,
    AuthUser(me): AuthUser,
    Path(device_id): Path<String>,
    body: Result<Json<E2eeDeviceRegister>, JsonRejection>,
) -> Result<Json<E2eeDevice>, ApiErr> {
    changes_limited(&s, me.id)?;
    let Json(req) = body?;
    if !is_e2ee_device_id(&device_id) {
        return Err(bad("device id must be 32 lowercase hex digits"));
    }
    let key = check_key(&req.public_key).ok_or_else(|| bad("public key must be 32 bytes of base64url without padding"))?;
    let label = clean_e2ee_label(&req.label).ok_or_else(|| bad("label must be 1-40 characters"))?;
    let (user, id) = (me.id, device_id.clone());
    let put = s.db.call(move |c| put_device(c, user, &id, &key, &label, now_ms())).await?;
    match put {
        Put::Created(d) => {
            tracing::info!(user = %me.username, device = short(&device_id), "e2ee: device registered");
            announce(&s, me.id).await?;
            Ok(Json(d))
        }
        Put::Same { device, changed } => {
            if changed {
                tracing::info!(user = %me.username, device = short(&device_id), "e2ee: device renamed");
                announce(&s, me.id).await?;
            }
            Ok(Json(device))
        }
        Put::KeyChanged => {
            tracing::warn!(user = %me.username, device = short(&device_id), "e2ee: registration refused, the id has another key");
            Err(ApiErr::new(StatusCode::CONFLICT, ERR_E2EE_KEY_CHANGED))
        }
        Put::TooMany => {
            tracing::warn!(user = %me.username, "e2ee: registration refused, too many devices");
            Err(ApiErr::new(StatusCode::CONFLICT, ERR_E2EE_TOO_MANY_DEVICES))
        }
    }
}

/// `DELETE /api/e2ee/devices/{device_id}`
async fn remove(State(s): State<AppState>, AuthUser(me): AuthUser, Path(device_id): Path<String>) -> Result<Json<OkResponse>, ApiErr> {
    changes_limited(&s, me.id)?;
    if !is_e2ee_device_id(&device_id) {
        return Err(bad("device id must be 32 lowercase hex digits"));
    }
    let (user, id) = (me.id, device_id.clone());
    if !s.db.call(move |c| delete_device(c, user, &id)).await? {
        return Err(ApiErr::new(StatusCode::NOT_FOUND, "no such device"));
    }
    tracing::info!(user = %me.username, device = short(&device_id), "e2ee: device removed");
    announce(&s, me.id).await?;
    Ok(Json(OkResponse { ok: true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE users (id INTEGER PRIMARY KEY); INSERT INTO users (id) VALUES (1), (2);").unwrap();
        init(&c).unwrap();
        init(&c).unwrap();
        c
    }

    fn id(n: u8) -> String {
        format!("{n:02x}").repeat(16)
    }

    fn key(n: u8) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([n; 32])
    }

    #[test]
    fn keys_are_checked_without_crypto() {
        assert_eq!(check_key(&key(7)).as_deref(), Some(key(7).as_str()));
        assert_eq!(check_key(&key(0)), None, "all zero");
        assert_eq!(check_key(&format!("{}=", key(7))), None, "padding");
        assert_eq!(check_key(&key(7)[..42]), None);
        assert_eq!(check_key(&base64::engine::general_purpose::STANDARD_NO_PAD.encode([0xfb; 32])), None, "standard alphabet");
        assert_eq!(check_key(&base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([1; 31])), None, "31 bytes");
        // The last character carries 2 spare bits: a non-canonical spelling of the same bytes.
        let k = key(7);
        let mut odd = k.clone();
        let last = odd.pop().unwrap();
        let alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let next = alphabet.chars().nth(alphabet.find(last).unwrap() + 1).unwrap();
        odd.push(next);
        assert_eq!(check_key(&odd), None, "{odd}");
    }

    #[test]
    fn register_confirm_cap_and_delete() {
        let mut c = db();
        let Put::Created(d) = put_device(&mut c, 1, &id(1), &key(1), "PC", 10).unwrap() else { panic!() };
        assert_eq!((d.user_id, d.created_at_ms), (1, 10));
        assert_eq!(put_device(&mut c, 1, &id(1), &key(1), "PC", 20).unwrap(), Put::Same { device: d.clone(), changed: false });
        let seen: i64 = c.query_row("SELECT last_seen_ms FROM e2ee_devices WHERE user_id = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(seen, 20);
        assert!(matches!(put_device(&mut c, 1, &id(1), &key(1), "PC novo", 30).unwrap(), Put::Same { changed: true, .. }));
        assert_eq!(put_device(&mut c, 1, &id(1), &key(2), "PC", 40).unwrap(), Put::KeyChanged);
        assert_eq!(list_user(&c, 1).unwrap()[0].public_key, key(1), "the pair never changes");
        for n in 2..=5 {
            assert!(matches!(put_device(&mut c, 1, &id(n), &key(n), "PC", 50 + n as i64).unwrap(), Put::Created(_)));
        }
        assert_eq!(put_device(&mut c, 1, &id(6), &key(6), "PC", 60).unwrap(), Put::TooMany);
        // Another member has their own five; the same id under another member is another device.
        assert!(matches!(put_device(&mut c, 2, &id(1), &key(9), "Mac", 70).unwrap(), Put::Created(_)));
        assert_eq!(list_all(&c).unwrap().len(), 6);
        assert!(!delete_device(&c, 2, &id(2)).unwrap(), "not theirs");
        assert!(delete_device(&c, 1, &id(2)).unwrap());
        assert!(matches!(put_device(&mut c, 1, &id(6), &key(6), "PC", 80).unwrap(), Put::Created(_)), "room again");
        assert_eq!(forget_user(&c, 1).unwrap(), 5);
        assert_eq!(list_all(&c).unwrap().iter().map(|d| d.user_id).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn a_removed_member_takes_their_devices_along() {
        let mut c = db();
        c.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        for (user, n) in [(1, 1), (1, 2), (2, 3)] {
            assert!(matches!(put_device(&mut c, user, &id(n), &key(n), "PC", 10).unwrap(), Put::Created(_)));
        }
        c.execute("DELETE FROM users WHERE id = 1", []).unwrap();
        assert_eq!(list_all(&c).unwrap().iter().map(|d| (d.user_id, d.device_id.clone())).collect::<Vec<_>>(), vec![(2, id(3))]);
    }
}
