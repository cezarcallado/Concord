//! Wire types shared by the hub (server) and the desktop client.
//!
//! Transport:
//! - HTTP JSON under `/api/*` for request/response calls.
//! - One WebSocket per client at `/ws?token=<session token>` carrying [`ClientEvent`] /
//!   [`ServerEvent`] as JSON text frames (tagged by `"type"`).
//! - Voice/video never goes through the hub: the client asks the hub for a LiveKit token
//!   ([`VoiceJoinResponse`]) and connects to the LiveKit server directly.
//!
//! Every authenticated HTTP call sends `Authorization: Bearer <session token>`.

use serde::{Deserialize, Serialize};

mod attachment;
mod chat;
mod dm;
mod e2ee;
mod invite;
mod profile;
mod sound;
mod stream_preview;
pub use attachment::*;
pub use chat::*;
pub use dm::*;
pub use e2ee::*;
pub use invite::*;
pub use profile::*;
pub use sound::*;
pub use stream_preview::*;

pub type UserId = i64;
pub type ChannelId = i64;
pub type MessageId = i64;

/// Bump when a breaking change is made to any type in this crate.
pub const PROTOCOL_VERSION: u32 = 1;

// ---------- HTTP ----------

/// `POST /api/login`
///
/// Identity (item 12, no protocol bump): a name can be protected by a *personal password*. Then
/// logging in as that name needs, besides the server password, either `password` or a valid
/// `device_token` (from an earlier login with `remember_device`). A hub that predates this
/// ignores the new fields. Errors a new hub can answer (`ApiError::error`, HTTP 401 unless noted):
/// [`ERR_PERSONAL_PASSWORD_REQUIRED`], [`ERR_WRONG_PERSONAL_PASSWORD`], [`ERR_RESET_CODE_EXPIRED`],
/// and `429` "too many attempts ..." while a name or an address is being slowed down.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    /// Shared server password set by the host (TeamSpeak-style). Empty if the server has none.
    pub server_password: String,
    pub protocol_version: u32,
    /// The personal password of this name (or a one-time reset code), if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// A long-lived per-device token from an earlier [`LoginResponse::device_token`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_token: Option<String>,
    /// Ask for a [`LoginResponse::device_token`] so the personal password is not typed again on
    /// this computer.
    #[serde(default, skip_serializing_if = "is_false")]
    pub remember_device: bool,
    /// An invite token (hex, from a `vox1-` code) in place of the server password (v0.8, see
    /// [`InviteCode`]). The login uses up one of the invite's uses and, with `remember_device`,
    /// answers a device token: from then on that token (with the same name) lets this computer
    /// in without the server password too. Errors: [`ERR_INVITE_INVALID`],
    /// [`ERR_INVITE_EXPIRED`], [`ERR_INVITE_USED_UP`], [`ERR_INVITE_REVOKED`]. An older hub
    /// ignores the field and answers "wrong server password".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite: Option<String>,
}

/// Login answer: this name has a personal password and none was sent (or the device token is no
/// longer valid). The client asks for the personal password.
pub const ERR_PERSONAL_PASSWORD_REQUIRED: &str = "personal password required";
/// Login / password change answer: the personal password (or reset code) is wrong.
pub const ERR_WRONG_PERSONAL_PASSWORD: &str = "wrong personal password";
/// Login answer: the name was reset with a one-time code that has expired.
pub const ERR_RESET_CODE_EXPIRED: &str = "reset code expired";
/// Start of the `429` answer while logins for a name or from an address are being slowed down.
pub const ERR_TOO_MANY_ATTEMPTS: &str = "too many attempts";
/// Password change answer (`403`): the owner's name has no personal password yet, and the first
/// one may only be chosen with proof of being the host: from the server PC itself (`127.0.0.1`),
/// from the session that claimed the server, or with the code the server's window shows
/// ([`SetPasswordRequest::owner_code`]). Added after v0.6.1 (older apps show the text).
pub const ERR_OWNER_PROOF_REQUIRED: &str =
    "owner proof required: set the owner's first personal password from the server PC (127.0.0.1) or with the code shown in the server window";
/// v1.1 (T0): the `426` a hub with `tls = "required"` gives a request in the clear from another
/// computer (an app older than 1.1). Portuguese on purpose: an old app shows the hub's text as is.
pub const ERR_TLS_REQUIRED: &str = "Este servidor exige o concord 1.1 ou mais novo (conexão protegida).";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginResponse {
    pub token: String,
    pub user: User,
    pub server_name: String,
    /// Present when `remember_device` was asked for and the name has a personal password: keep it
    /// (encrypted) and send it as [`LoginRequest::device_token`] next time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_token: Option<String>,
}

/// `GET /api/health` (no auth)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub version: String,
    pub protocol_version: u32,
    pub livekit_ok: bool,
    /// The hub's identity (v1.0.1, no protocol bump): 32 hex digits, made once per server and
    /// never changed ([`is_server_id`]). An app keeps it with what it saved for the server (its
    /// password, this computer's login token) and sends those only to an address that answers
    /// with the same id. Absent from older hubs; older apps ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,
}

/// A well-formed [`HealthResponse::server_id`]: exactly 32 lowercase hex digits (128 bits).
pub fn is_server_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    pub username: String,
    /// Server role (item 12). Added after v0.4.1 with no protocol bump: `default` keeps older
    /// hubs' JSON parsing (everyone is a member), `skip_serializing_if` keeps a member's JSON
    /// byte-identical to before, and older clients ignore the unknown field.
    #[serde(default, skip_serializing_if = "Role::is_member")]
    pub role: Role,
    /// Profile picture version (a short hash of the stored image), `None` = no picture. The
    /// image itself is `GET /api/users/{id}/avatar?size=64|256` (see [`AvatarResponse`]); a
    /// client caches it by `(user, version)`. Added after v0.4.1 with no protocol bump, same
    /// rules as `role`: `default` for older hubs, absent from the JSON when there is no picture
    /// (so it is byte-identical to before), ignored by older clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    /// What people see instead of the username (profiles, v0.6): optional, at most
    /// [`DISPLAY_NAME_MAX`] characters. The username stays the login identity. Same
    /// compatibility rules as `avatar`: `default` for older hubs, absent from the JSON when unset
    /// (so a user without one is byte-identical to before), ignored by older clients. Use
    /// [`User::shown_name`] to pick what to display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

impl User {
    /// A plain member without a picture or a display name (what everybody was before items 12,
    /// "foto de perfil" and profiles).
    pub fn new(id: UserId, username: impl Into<String>) -> Self {
        User { id, username: username.into(), role: Role::Member, avatar: None, display_name: None }
    }
}

/// Server roles (item 12), ordered `Member < Moderator < Admin < Owner`. The permission matrix
/// is fixed (no custom roles) and lives here, so the hub enforces exactly what the app shows.
/// An unknown role from a newer hub reads as `Member`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// "Membro": chat, voice, Go Live; may delete their own messages.
    #[default]
    Member,
    /// "Moderador": server-mute, disconnect / move someone in voice, stop someone's stream,
    /// delete any message.
    Moderator,
    /// "Admin": everything a moderator does, plus channels, roles below Admin and the audit log.
    Admin,
    /// "Dono": the single owner of the server (whoever hosts it claims it, see "server owner"
    /// below, or `owner` in the hub's `config.toml`); everything, Admins included.
    Owner,
}

impl<'de> Deserialize<'de> for Role {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = std::borrow::Cow::<'de, str>::deserialize(d)?;
        Ok(Role::from_wire(&s))
    }
}

impl Role {
    pub const ALL: [Role; 4] = [Role::Member, Role::Moderator, Role::Admin, Role::Owner];

    /// The wire / database name (`"member"`, `"moderator"`, `"admin"`, `"owner"`).
    pub fn wire_name(self) -> &'static str {
        match self {
            Role::Member => "member",
            Role::Moderator => "moderator",
            Role::Admin => "admin",
            Role::Owner => "owner",
        }
    }

    /// Inverse of [`Role::wire_name`]; anything unknown is a member.
    pub fn from_wire(s: &str) -> Role {
        match s {
            "moderator" => Role::Moderator,
            "admin" => Role::Admin,
            "owner" => Role::Owner,
            _ => Role::Member,
        }
    }

    pub fn is_member(&self) -> bool {
        *self == Role::Member
    }

    /// Server-mute, disconnect, move, stop the stream of someone this role outranks; delete any
    /// chat message.
    pub fn can_moderate(self) -> bool {
        self >= Role::Moderator
    }

    /// Create, rename, delete and reorder channels.
    pub fn can_manage_channels(self) -> bool {
        self >= Role::Admin
    }

    /// The "Configurações do servidor" dialog: members and roles, and the moderation log.
    pub fn can_admin(self) -> bool {
        self >= Role::Admin
    }

    /// "Redefinir senha pessoal" of someone else (the owner only).
    pub fn can_reset_passwords(self) -> bool {
        self == Role::Owner
    }

    /// Person-targeted actions (mute, disconnect, move, stop stream) need a strictly higher role
    /// than the target's.
    pub fn outranks(self, target: Role) -> bool {
        self.can_moderate() && self > target
    }

    /// May `self` change the role of someone who is `target` into `to`? Nobody makes or unmakes
    /// the owner (that is claiming / transferring ownership); the owner sets anything else; an admin moves people
    /// below Admin between Membro and Moderador.
    pub fn can_set_role(self, target: Role, to: Role) -> bool {
        if target == Role::Owner || to == Role::Owner || target == to {
            return false;
        }
        match self {
            Role::Owner => true,
            Role::Admin => target < Role::Admin && to < Role::Admin,
            _ => false,
        }
    }

    /// The roles `self` may hand out, in dropdown order.
    pub fn assignable(self) -> Vec<Role> {
        match self {
            Role::Owner => vec![Role::Member, Role::Moderator, Role::Admin],
            Role::Admin => vec![Role::Member, Role::Moderator],
            _ => Vec::new(),
        }
    }
}

// ---------- profile pictures ----------
//
// - `PUT    /api/me/avatar`   body = the image file as it is (PNG / JPEG / WebP / GIF first frame,
//   any `Content-Type`), at most [`AVATAR_MAX_UPLOAD_BYTES`]  -> [`AvatarResponse`]. The *hub*
//   validates it (dimension caps, no decompression bombs), centre-crops it to a square, resizes
//   it to [`AVATAR_SIZES`] and re-encodes it (JPEG, or PNG when it has transparency) without any
//   metadata. `400` not an image, `413` too big, `429` too many changes
//   ([`AVATAR_RATE_LIMIT`] per [`AVATAR_RATE_WINDOW_SECS`]).
// - `DELETE /api/me/avatar`                                       -> [`AvatarResponse`]
// - `GET    /api/users/{id}/avatar?size=64|256[&v=<version>]`  the stored image (authenticated).
//   With `v` equal to the current version it is `Cache-Control: ... immutable` (the version is
//   the cache key); a different `v` is `404`, and so is a user without a picture.
// - `POST   /api/mod/users/{id}/remove-avatar`  (Moderador+, above the target's role)  -> [`OkResponse`]
//
// Every change broadcasts [`ServerEvent::UserUpdated`] with the new [`User::avatar`]. A hub older
// than this answers `404` / `405` to all of it, and its users never have an `avatar`.

/// Biggest accepted upload (bytes).
pub const AVATAR_MAX_UPLOAD_BYTES: usize = 5 * 1024 * 1024;
/// The sizes the hub stores and serves (square, pixels).
pub const AVATAR_SIZES: [u32; 2] = [64, 256];
/// At most this many picture changes per person...
pub const AVATAR_RATE_LIMIT: usize = 5;
/// ...in this many seconds.
pub const AVATAR_RATE_WINDOW_SECS: u64 = 600;

/// Answer of `PUT` / `DELETE /api/me/avatar`: the new version (`None` after a removal).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvatarResponse {
    #[serde(default)]
    pub avatar: Option<String>,
}

/// The connecting user's own identity state (item 12), only in [`ServerEvent::Ready`]. Its
/// presence tells a client that the hub knows roles and personal passwords.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Account {
    /// This name is protected by a personal password.
    #[serde(default)]
    pub has_password: bool,
    /// A personal password is needed and not set yet (the owner's first login after the update,
    /// a promoted role without one, or a login with a reset code): the app asks for one, and the
    /// role's powers stay off until it is set.
    #[serde(default)]
    pub must_set_password: bool,
    /// The server has no owner yet ("quem hospeda vira dono"): whoever logs in from the server
    /// PC itself becomes the owner, and anybody with the claim code the hub prints can claim it
    /// ([`ClaimOwnerRequest`]). The app shows a "Reivindicar servidor" entry while this is set.
    /// Absent (false) from hubs that predate it, so an old hub never shows the entry.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_owner: bool,
    /// The owner is fixed by `owner = "..."` in the hub's `config.toml`: ownership cannot be
    /// claimed or transferred from the app.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub owner_pinned: bool,
}

/// `POST /api/account/password` (item 12): set, change or (members only) remove my personal
/// password; `current` is needed when one is set. Answer: [`SetPasswordResponse`]. Every other
/// session and device token of the name is revoked.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SetPasswordRequest {
    #[serde(default)]
    pub current: Option<String>,
    /// `None` removes the password (members only).
    #[serde(default)]
    pub new: Option<String>,
    /// Also hand this computer a new device token.
    #[serde(default)]
    pub remember_device: bool,
    /// The code the server's window shows, for the owner's *first* personal password when it is
    /// not set from the server PC itself ([`ERR_OWNER_PROOF_REQUIRED`]). Added after v0.6.1:
    /// skipped when empty, ignored by older hubs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetPasswordResponse {
    pub account: Account,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_token: Option<String>,
}

/// Personal password length limits (characters).
pub const PERSONAL_PASSWORD_MIN: usize = 6;
pub const PERSONAL_PASSWORD_MAX: usize = 128;

// ---------- moderation and administration (item 12) ----------
//
// All with `Authorization: Bearer` and JSON bodies; `403` when the role does not allow it, `404`
// for an unknown user / channel / message. A hub older than item 12 answers `404` / `405`.
//
// - `POST /api/mod/users/{id}/mute`             [`ModMuteRequest`]  server-mute / unmute
// - `POST /api/mod/users/{id}/disconnect`                           disconnect from voice
// - `POST /api/mod/users/{id}/move`             [`ModMoveRequest`]  move to another voice channel
// - `POST /api/mod/users/{id}/stop-stream`                          end their Go Live
// - `POST /api/messages/{id}/delete`                                own message, or any (Moderador+)
// - `POST /api/admin/users/{id}/role`           [`SetRoleRequest`]
// - `POST /api/admin/users/{id}/reset-password` -> [`ResetPasswordResponse`] (owner only)
// - `GET  /api/admin/users`                     -> `Vec<MemberInfo>`
// - `GET  /api/admin/audit?before=&limit=`      -> `Vec<AuditEntry>` (newest first)
// - `POST /api/admin/channels`                  [`ChannelCreateRequest`] -> [`Channel`]
// - `POST /api/admin/channels/{id}/rename`      [`ChannelRenameRequest`]
// - `POST /api/admin/channels/{id}/delete`
// - `POST /api/admin/channels/reorder`          [`ChannelReorderRequest`]
// - `POST /api/admin/channels/{id}/access`      [`ChannelAccessRequest`]  private / public (v0.9)
//
// Calls with nothing to return answer [`OkResponse`].
//
// ---------- server owner ("quem hospeda vira dono") ----------
//
// The owner is stored by the hub (SQLite), unless `owner` in `config.toml` pins it. While there
// is none, [`Account::no_owner`] is set and:
// - the first login (or authenticated connection) whose TCP peer is the loopback address, i.e.
//   the app running on the server PC itself, becomes the owner on the spot;
// - anyone logged in can claim it with the one-time code the hub prints in its console:
//   `POST /api/owner/claim` [`ClaimOwnerRequest`] -> [`ClaimOwnerResponse`] (`403` wrong code,
//   `409` the server already has an owner, `429` too many wrong codes).
// A new owner must set a personal password right away (`Account::must_set_password`). The owner
// hands the server over with `POST /api/admin/transfer-owner` [`TransferOwnerRequest`] -> [`OkResponse`]
// (`403` not the owner, `409` pinned by `config.toml` / the target has no personal password yet).
// All three are in the moderation log ([`AUDIT_CLAIM_OWNER`], [`AUDIT_TRANSFER_OWNER`],
// [`AUDIT_RESET_OWNER`]). `concord-server --reset-owner` clears the owner from the host's console.
// A hub that predates it answers `404` / `405` and never sets `no_owner`.

/// `POST /api/owner/claim`: the code the hub printed (case, dashes and spaces do not matter).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimOwnerRequest {
    pub code: String,
}

/// The new owner and their account (a personal password is needed before the powers turn on).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimOwnerResponse {
    pub user: User,
    pub account: Account,
}

/// `POST /api/admin/transfer-owner`: make this user (who has a personal password) the owner; the
/// current owner becomes Admin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferOwnerRequest {
    pub user_id: UserId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModMuteRequest {
    pub muted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModMoveRequest {
    pub channel_id: ChannelId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetRoleRequest {
    pub role: Role,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetPasswordResponse {
    /// One-time code the person types as their personal password; then they choose a new one.
    pub code: String,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelCreateRequest {
    pub name: String,
    pub kind: ChannelKind,
    /// v0.9: create it private right away, with this access list (see [`PrivateAccess`]). Absent
    /// when public, so the JSON is byte-identical to before. An older hub ignores the field and
    /// creates a *public* channel: a client only offers it when the hub says it knows private
    /// channels ([`ChatFeatures::private_channels`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private: Option<PrivateAccess>,
}

/// `POST /api/admin/channels/{id}/access` (Admin and Dono, v0.9): make the channel private with
/// this access list, change the list, or (`private: None`) make it public again. Answer:
/// [`OkResponse`]. `403` not an admin, `404` unknown channel. Roles above Moderador in the list
/// and unknown accounts are dropped (Admin and Dono always have access). Every change is in the
/// moderation log ([`AUDIT_CHANNEL_ACCESS`]) and goes out at once: people who gain access get the
/// channel in their `ChannelsChanged`, people who lose it stop seeing it (and are taken out of its
/// voice room).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ChannelAccessRequest {
    #[serde(default)]
    pub private: Option<PrivateAccess>,
}

/// v1.1 (E1): `POST /api/channels/{id}/e2ee` (Admin and up): turn a voice channel's end-to-end
/// encrypted calls on or off. Only while its room is empty: `409` [`ERR_ROOM_OCCUPIED`] otherwise
/// ("Esvazie a sala para mudar isso."). `400` for a text channel, `404` unknown. Answers
/// [`ChannelE2eeResponse`]; everybody gets the new channel list. A hub older than 1.1 answers
/// `404` (the setting is then not offered).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ChannelE2eeRequest {
    pub on: bool,
}

/// The answer of [`ChannelE2eeRequest`] (also to a `GET` of the same path, which changes nothing):
/// the flag as stored and the people who may see the channel but have no device with encryption
/// keys on this server (they use the old app, 1.0, or never opened 1.1 here), who cannot enter
/// while it is on.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ChannelE2eeResponse {
    pub on: bool,
    #[serde(default)]
    pub outdated: Vec<User>,
}

/// `409` of [`ChannelE2eeRequest`]: somebody is in the voice room.
pub const ERR_ROOM_OCCUPIED: &str = "room occupied";
/// `403` of a join token for an end-to-end encrypted room asked without `?e2ee=1` (an app older
/// than 1.1). Portuguese on purpose: an older app shows the hub's text as is.
pub const ERR_E2EE_REQUIRED: &str = "Esta sala usa criptografia de ponta a ponta. Atualize o concord para entrar.";
/// `409` of a DM call's accept (or rejoin) without `?e2ee=1` when the call is end-to-end
/// encrypted. Portuguese on purpose, as above.
pub const ERR_E2EE_CALL_REQUIRED: &str = "Atualize o concord para atender esta chamada criptografada";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelRenameRequest {
    pub name: String,
}

/// The new order: every channel id exactly once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelReorderRequest {
    pub ids: Vec<ChannelId>,
}

/// Channel names: 1 to this many characters.
pub const CHANNEL_NAME_MAX: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OkResponse {
    pub ok: bool,
}

/// One row of `GET /api/admin/users`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberInfo {
    pub user: User,
    pub has_password: bool,
    pub server_muted: bool,
    pub created_at_ms: i64,
}

/// One row of the moderation log (`GET /api/admin/audit`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: i64,
    pub at_ms: i64,
    /// Who did it (`None`: the host, from the command line).
    #[serde(default)]
    pub actor: Option<User>,
    /// One of the `AUDIT_*` names (a newer hub may add more: show those raw).
    pub action: String,
    #[serde(default)]
    pub target: Option<User>,
    /// Free text: the channel, the role, an excerpt of a deleted message...
    #[serde(default)]
    pub detail: String,
}

pub const AUDIT_SERVER_MUTE: &str = "server_mute";
pub const AUDIT_SERVER_UNMUTE: &str = "server_unmute";
pub const AUDIT_DISCONNECT: &str = "disconnect";
pub const AUDIT_MOVE: &str = "move";
pub const AUDIT_STOP_STREAM: &str = "stop_stream";
pub const AUDIT_DELETE_MESSAGE: &str = "delete_message";
pub const AUDIT_SET_ROLE: &str = "set_role";
pub const AUDIT_RESET_PASSWORD: &str = "reset_password";
pub const AUDIT_CHANNEL_CREATE: &str = "channel_create";
pub const AUDIT_CHANNEL_RENAME: &str = "channel_rename";
pub const AUDIT_CHANNEL_DELETE: &str = "channel_delete";
pub const AUDIT_CHANNEL_REORDER: &str = "channel_reorder";
/// A channel was made private, its access list changed, or it was made public again (v0.9;
/// detail: the channel and, in Portuguese, who has access now).
pub const AUDIT_CHANNEL_ACCESS: &str = "channel_access";
/// v1.1 (E1): a voice channel's end-to-end encrypted calls were turned on or off (detail: the
/// channel and "ligada" / "desligada").
pub const AUDIT_CHANNEL_E2EE: &str = "channel_e2ee";
/// Someone became the server's owner (detail: how, "codigo" or "pc-do-servidor").
pub const AUDIT_CLAIM_OWNER: &str = "claim_owner";
/// The owner handed the server over to the target.
pub const AUDIT_TRANSFER_OWNER: &str = "transfer_owner";
/// The host cleared the owner from the command line (`--reset-owner`).
pub const AUDIT_RESET_OWNER: &str = "reset_owner";
/// A moderator removed someone's profile picture (abuse).
pub const AUDIT_REMOVE_AVATAR: &str = "remove_avatar";

/// What a moderator did to *you* ([`ServerEvent::Moderated`], sent only to the target).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModAction {
    /// Server-muted: the voice server no longer accepts your microphone.
    ServerMute,
    ServerUnmute,
    /// Removed from the voice channel.
    Disconnect,
    /// Join this voice channel now (the hub also removes you from the old room shortly after).
    Move { channel_id: ChannelId },
    /// Your Go Live was ended: stop sharing.
    StopStream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Text,
    Voice,
}

/// `GET /api/channels` -> `Vec<Channel>`: only the channels the caller may see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    pub id: ChannelId,
    pub name: String,
    pub kind: ChannelKind,
    pub position: i32,
    /// Private channel ("Canal privado", v0.9, no protocol bump): who may see it. `None` = public.
    /// Absent from the JSON when public (byte-identical to before), ignored by older apps.
    ///
    /// The hub enforces it for every person, whatever their app: somebody without access is never
    /// told the channel exists (not in `Ready` / `ChannelsChanged` / `GET /api/channels`), gets
    /// none of its events (messages, reactions, deletions, voice members, sounds), and every read
    /// of it (history, a message, an image, a reaction, a voice token) answers `404` as for an
    /// unknown id. Its voice room refuses them too (no token, and the hub removes a stray
    /// participant from LiveKit). So an older app simply never sees the private channels it has
    /// no access to; one that has access sees the channel without the lock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private: Option<PrivateAccess>,
    /// v1.1 (E1): the voice room's calls are end-to-end encrypted ("Chamada criptografada de ponta
    /// a ponta", opt-in by an Admin, see [`ChannelE2eeRequest`]): voice, cameras and Go Live are
    /// encrypted on the sender's computer with per-sender keys only the people in the room get
    /// (`vox_e2ee::media`). The hub refuses a join token without `?e2ee=1` ([`ERR_E2EE_REQUIRED`]),
    /// so an older app never enters. Absent from the JSON when off (byte-identical to before).
    #[serde(default, skip_serializing_if = "is_false")]
    pub e2ee: bool,
}

impl Channel {
    /// May this person (with this role) see the channel?
    pub fn allows(&self, user: UserId, role: Role) -> bool {
        self.private.as_ref().is_none_or(|p| p.allows(user, role))
    }
}

/// A private channel's access list ([`Channel::private`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PrivateAccess {
    /// Roles that may see it ("Cargos com acesso"): only [`Role::Moderator`] and [`Role::Member`]
    /// mean something (each on its own: Membro does not include Moderador), Admin and Dono always
    /// have access.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<Role>,
    /// Accounts that may see it whatever their role ("Pessoas com acesso").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub users: Vec<UserId>,
}

impl PrivateAccess {
    /// Admin and Dono always; anybody else by their role or by name. The hub checks the role
    /// permissions use (a staff member who still has to set a personal password counts as Membro).
    pub fn allows(&self, user: UserId, role: Role) -> bool {
        role >= Role::Admin || self.roles.contains(&role) || self.users.contains(&user)
    }

    /// The same list in its canonical form: only Moderador / Membro (once each, Moderador first),
    /// people sorted and once each.
    pub fn normalized(&self) -> PrivateAccess {
        let roles = [Role::Moderator, Role::Member].into_iter().filter(|r| self.roles.contains(r)).collect();
        let mut users = self.users.clone();
        users.sort_unstable();
        users.dedup();
        PrivateAccess { roles, users }
    }
}

/// `GET /api/channels/{id}/messages?before=<MessageId>&limit=<n>` -> `Vec<Message>` (newest first, limit <= 100)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub channel_id: ChannelId,
    pub author: User,
    pub content: String,
    /// Unix epoch milliseconds (UTC).
    pub created_at_ms: i64,
    /// Images (added after v0.6.0, no protocol bump; see [`attachment`](crate::attachment)):
    /// absent from the JSON when there are none (byte-identical to before), ignored by older apps,
    /// which show `content` (the caption plus an `[imagem]` marker line). Show [`Message::caption`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// Mentions, replies and reactions (v0.8, no protocol bump; see [`chat`](crate::chat)): all
    /// absent from the JSON when empty (byte-identical to before) and ignored by older apps.
    /// People this message mentions (user ids, checked by the hub).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<UserId>,
    /// `@todos` written by a Moderador or above: it mentions everybody.
    #[serde(default, skip_serializing_if = "is_false")]
    pub mention_everyone: bool,
    /// The message this one answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<ReplyRef>,
    /// Emoji reactions, in the order they were first used.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<Reaction>,
}

/// `POST /api/voice/{channel_id}/join` -> [`VoiceJoinResponse`]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceJoinResponse {
    /// `ws://host:7880` style URL the client connects to.
    pub livekit_url: String,
    /// LiveKit access token for room `voice-<channel_id>`, identity = user id.
    pub livekit_token: String,
    pub room: String,
    /// v1.1 (E1): the room is end-to-end encrypted (a channel with [`Channel::e2ee`], or a DM call
    /// with [`crate::DmCall::e2ee`]): join with media encryption and the key exchange. Only ever
    /// true for a request that said `?e2ee=1`. Absent when false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub e2ee: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}

/// `GET /api/link-preview?url=<http(s) url>` (authenticated), item 10. The *hub* fetches the page
/// (one fetch serves everyone, clients never contact arbitrary sites, the result is cached), and
/// answers `200` with this even when nothing could be fetched: then `title`, `description` and
/// `has_image` are empty and the client simply shows no card (the answer is deliberately the same
/// for "blocked", "failed" and "nothing to show"). `400` = not an http(s) URL, `429` = too many
/// fresh lookups. A hub older than v0.5 has no such route and answers `404`.
///
/// Added after v0.4.1 with no protocol bump: new endpoint, new type, nothing existing changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkPreview {
    /// The URL that was asked for (normalised: no fragment).
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// The site's own name, or its domain.
    #[serde(default)]
    pub site_name: String,
    /// A thumbnail exists: `GET /api/link-preview/image?url=<the same url>` returns it as a JPEG
    /// of at most 400x225 (authenticated; `404` when there is none).
    #[serde(default)]
    pub has_image: bool,
    /// The thumbnail's size in pixels, so a client can reserve the space before it loads.
    #[serde(default)]
    pub image_w: u32,
    #[serde(default)]
    pub image_h: u32,
}

impl LinkPreview {
    /// Nothing worth a card: no text and no image.
    pub fn is_empty(&self) -> bool {
        self.title.is_empty() && self.description.is_empty() && !self.has_image
    }
}

// ---------- WebSocket ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientEvent {
    /// A plain chat message. `mentions`, `reply_to` and `reply_mention` (v0.8, no protocol bump;
    /// see [`chat`](crate::chat)) are absent when unused, so the JSON is byte-identical to before;
    /// an older hub ignores them.
    SendMessage {
        channel_id: ChannelId,
        content: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mentions: Vec<UserId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<MessageId>,
        #[serde(default, skip_serializing_if = "is_false")]
        reply_mention: bool,
    },
    /// Client tells the hub which voice channel it is in (None = left). Hub fans it out.
    /// `camera` (added in v0.4.x, no protocol bump) says the sender's webcam is on; `default`
    /// keeps older clients' JSON parsing (camera off), and older hubs ignore the unknown field.
    VoiceState {
        channel_id: Option<ChannelId>,
        muted: bool,
        deafened: bool,
        streaming: bool,
        #[serde(default)]
        camera: bool,
        /// With `streaming`: the live is audio-only ("Só áudio", no picture). Added in v0.5.x
        /// (no protocol bump): `default` keeps older clients' JSON parsing (a screen live), and
        /// older hubs ignore the unknown field.
        #[serde(default)]
        audio_only: bool,
    },
    Ping { nonce: u64 },
    /// Added in v0.2 item 7 (no protocol bump): the full set of streamers this client is watching
    /// right now (empty = none). Sent whenever the set changes and again after every reconnect.
    /// A hub that predates it answers the unknown variant with `Error { "invalid event" }`
    /// (see [`WATCH_STATE_REJECTED`]) and keeps the socket open; the client then stops sending it.
    WatchState { watching: Vec<UserId> },
    /// Soundboard (v0.5, no protocol bump; see [`sound`](crate::sound) for the whole feature):
    /// play this sound for the voice channel I am in. A hub that predates it answers the unknown
    /// variant with `Error { "invalid event" }`; a client never sends it then, because the
    /// soundboard is hidden when `GET /api/sounds` is `404`.
    PlaySound { sound_id: SoundId },
    /// v1.1 (no protocol bump): stop the sounds I played, for my voice channel. Only sent to a
    /// hub whose `SoundPlayed` events carry `stoppable: true` (an older hub would answer the
    /// unknown variant with `Error { "invalid event" }`). The hub relays it as
    /// [`ServerEvent::SoundStopped`], at most once per play.
    StopSound,
}

/// The exact error text a v0.2.0/v0.2.1 hub sends for an event it cannot parse. A client that
/// has just sent [`ClientEvent::WatchState`] and gets this back knows the hub is too old.
pub const WATCH_STATE_REJECTED: &str = "invalid event";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    /// First event after connecting: full snapshot so the client needs no extra calls.
    /// `account` (item 12) is only sent by hubs that know roles: its absence means an older hub.
    Ready {
        user: User,
        channels: Vec<Channel>,
        online: Vec<User>,
        voice: Vec<VoiceMember>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<Account>,
        /// v0.8: the hub knows mentions, replies and reactions (absent: an older hub).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chat: Option<ChatFeatures>,
        /// v1.0.1: the hub's identity, the same as [`HealthResponse::server_id`] (absent: an
        /// older hub).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server_id: Option<String>,
        /// v1.1: the hub keeps Go Live hover previews (`PUT /api/me/stream-preview`, `GET
        /// /api/users/{id}/stream-preview`, see [`stream_preview`](crate::stream_preview)). Absent
        /// / false: an older hub, the app neither uploads nor asks.
        #[serde(default, skip_serializing_if = "is_false")]
        stream_previews: bool,
        /// v1.1 (E0): the hub keeps the directory of device keys (`/api/e2ee/devices`, see
        /// [`e2ee`](crate::e2ee)). Absent / false: an older hub, the app neither registers nor asks.
        #[serde(default, skip_serializing_if = "is_false")]
        e2ee: bool,
    },
    MessageCreated { message: Message },
    UserOnline { user: User },
    UserOffline { user_id: UserId },
    VoiceStateChanged { member: VoiceMember },
    Pong { nonce: u64 },
    Error { error: String },
    // ---- item 12, no protocol bump (older clients log and skip events they do not know)
    /// A chat message was deleted (by its author or a moderator).
    MessageDeleted { channel_id: ChannelId, message_id: MessageId },
    /// The channel list changed (created, renamed, deleted, reordered, made private or public, or my
    /// access to a private one changed): the full new list of the channels I may see.
    ChannelsChanged { channels: Vec<Channel> },
    /// Someone's role changed.
    UserUpdated { user: User },
    /// Only to the target: a moderator acted on you.
    Moderated { action: ModAction, by: User },
    // ---- soundboard, no protocol bump (older clients log and skip events they do not know)
    /// Someone played a sound in the voice channel `channel_id`: only that channel's members get
    /// this (the player too). Each client plays the clip locally.
    SoundPlayed {
        sound_id: SoundId,
        by: User,
        channel_id: ChannelId,
        /// v1.1: this hub relays [`ClientEvent::StopSound`] (absent / false: an older hub; the
        /// player can then only stop the sound for itself).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        stoppable: bool,
    },
    /// v1.1: `by` stopped the sounds they played in the voice channel `channel_id` (only that
    /// channel's members get this, the player too). Older clients log and skip it.
    SoundStopped { by: UserId, channel_id: ChannelId },
    /// The soundboard changed (upload, rename, delete): the full new list, to everybody.
    SoundsChanged { sounds: Vec<Sound> },
    // ---- direct messages and private calls (see `dm.rs`), no protocol bump. Every one of these
    // goes only to the two people of the conversation (`DmRead`: only to the reader's own
    // connections); older clients log and skip them.
    /// A new line in one of my DMs (mine too, for my other devices; call lines included).
    DmMessageCreated { message: DmMessage },
    /// A DM line was deleted by its author.
    DmMessageDeleted { dm_id: DmId, message_id: DmMessageId },
    /// I read a DM up to `up_to` (on any of my devices).
    DmRead { dm_id: DmId, up_to: DmMessageId },
    /// A call is ringing: the callee's app rings, the caller's shows "Chamando...". Also sent again
    /// to a callee who connects while it still rings.
    CallInvite { call: DmCall },
    /// The callee answered: both join the private room.
    CallAccepted { call: DmCall },
    /// The callee declined (the call is over).
    CallDeclined { dm_id: DmId, call_id: CallId },
    /// The call is over (hung up, rang out, cancelled...).
    CallEnded { dm_id: DmId, call_id: CallId, reason: CallEndReason },
    // ---- profiles (v0.6), no protocol bump (older clients log and skip events they do not know)
    /// Somebody's presence or custom status changed, or (right after `Ready`, once per person
    /// whose state is not plain "online, no status") what it is. `status: None` = no status. Never
    /// sent to others about an invisible person: they just go offline (see [`Presence`]).
    PresenceChanged {
        user_id: UserId,
        presence: Presence,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<CustomStatus>,
    },
    // ---- reactions (v0.8, see `chat.rs`), no protocol bump (older clients log and skip them)
    /// `user_id` put `emoji` on a channel message (`added`) or took it off. To everybody who may
    /// see the channel (as every channel event: messages, deletions, voice members, sounds).
    ReactionChanged { channel_id: ChannelId, message_id: MessageId, emoji: String, user_id: UserId, added: bool },
    /// The same on a DM line: only to the two people of the conversation.
    DmReactionChanged { dm_id: DmId, message_id: DmMessageId, emoji: String, user_id: UserId, added: bool },
    // ---- end-to-end encryption, E0 (v1.1, see `e2ee.rs`), no protocol bump (older clients log
    // and skip it)
    /// `user_id`'s devices changed (one registered, removed): the full new list, to everybody.
    E2eeDevicesChanged { user_id: UserId, devices: Vec<E2eeDevice> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceMember {
    pub user: User,
    pub channel_id: Option<ChannelId>,
    pub muted: bool,
    pub deafened: bool,
    pub streaming: bool,
    /// Streamers this member is watching (only people streaming in the same voice channel are
    /// ever listed). Added in v0.2 item 7: `default` keeps older hubs' JSON parsing, and older
    /// clients ignore the unknown field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watching: Vec<UserId>,
    /// This member's webcam is on. Added after v0.3.0 (no protocol bump): `default` keeps older
    /// hubs' JSON parsing, `skip_serializing_if` keeps the camera-off JSON byte-identical to
    /// before, and older clients ignore the unknown field.
    #[serde(default, skip_serializing_if = "is_false")]
    pub camera: bool,
    /// Server-muted by a moderator (item 12): the voice server refuses this member's microphone,
    /// whatever their own mute button says. Same compatibility rules as `camera`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub server_muted: bool,
    /// This member's live (`streaming`) is audio-only ("Só áudio"): everybody can tell, not just
    /// the people in the voice room that see the LiveKit tracks. Added in v0.5.x (no protocol
    /// bump), same compatibility rules as `camera`: `default` for older hubs' JSON, skipped when
    /// false so the JSON is byte-identical to v0.5.0's, ignored by older clients.
    #[serde(default, skip_serializing_if = "is_false")]
    pub audio_only: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire types exactly as v0.2.0/v0.2.1 had them (no `watching`, no `WatchState`).
    mod v020 {
        use super::super::{ChannelId, User};
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        pub struct VoiceMember {
            pub user: User,
            pub channel_id: Option<ChannelId>,
            pub muted: bool,
            pub deafened: bool,
            pub streaming: bool,
        }

        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        pub enum ServerEvent {
            Ready { user: User, channels: Vec<super::super::Channel>, online: Vec<User>, voice: Vec<VoiceMember> },
            VoiceStateChanged { member: VoiceMember },
            Pong { nonce: u64 },
            Error { error: String },
        }

        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        pub enum ClientEvent {
            VoiceState { channel_id: Option<ChannelId>, muted: bool, deafened: bool, streaming: bool },
            Ping { nonce: u64 },
        }
    }

    fn user(id: i64) -> User {
        User::new(id, format!("u{id}"))
    }

    #[test]
    fn json_from_an_old_hub_has_no_watching_and_still_parses() {
        let old = v020::VoiceMember { user: user(2), channel_id: Some(5), muted: false, deafened: true, streaming: true };
        let json = serde_json::to_string(&old).unwrap();
        let new: VoiceMember = serde_json::from_str(&json).unwrap();
        assert!(new.watching.is_empty());
        assert_eq!((new.user, new.channel_id, new.deafened, new.streaming), (user(2), Some(5), true, true));
        // The whole event too.
        let ev = serde_json::to_string(&v020::ServerEvent::VoiceStateChanged { member: old }).unwrap();
        assert!(matches!(serde_json::from_str::<ServerEvent>(&ev), Ok(ServerEvent::VoiceStateChanged { .. })));
    }

    #[test]
    fn json_from_a_new_hub_is_ignored_by_the_old_shape() {
        let m = VoiceMember { user: user(3), channel_id: Some(5), muted: true, deafened: false, streaming: false, watching: vec![2, 9], camera: false, server_muted: false, audio_only: false };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"watching\":[2,9]"));
        let old: v020::VoiceMember = serde_json::from_str(&json).unwrap();
        assert_eq!((old.user, old.channel_id, old.muted), (user(3), Some(5), true));
        // Inside the events an old client parses.
        let ev = ServerEvent::VoiceStateChanged { member: m.clone() };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(matches!(serde_json::from_str::<v020::ServerEvent>(&json), Ok(v020::ServerEvent::VoiceStateChanged { .. })));
        let ready = ServerEvent::Ready { user: user(1), channels: vec![], online: vec![user(1)], voice: vec![m], account: None, chat: None, server_id: None, stream_previews: false, e2ee: false };
        let json = serde_json::to_string(&ready).unwrap();
        assert!(matches!(serde_json::from_str::<v020::ServerEvent>(&json), Ok(v020::ServerEvent::Ready { .. })));
    }

    #[test]
    fn nobody_watching_serialises_exactly_as_before() {
        let m = VoiceMember { user: user(3), channel_id: None, muted: false, deafened: false, streaming: false, watching: vec![], camera: false, server_muted: false, audio_only: false };
        assert!(!serde_json::to_string(&m).unwrap().contains("watching"));
    }

    #[test]
    fn watch_state_wire_shape_and_old_hub_reaction() {
        let ev = ClientEvent::WatchState { watching: vec![4, 7] };
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(json, r#"{"type":"watch_state","watching":[4,7]}"#);
        assert!(matches!(serde_json::from_str::<ClientEvent>(&json), Ok(ClientEvent::WatchState { watching }) if watching == [4, 7]));
        // The old hub's enum has no such variant: it fails to parse (and answers `Error`).
        assert!(serde_json::from_str::<v020::ClientEvent>(&json).is_err());
        // Old client events are still understood.
        let old = serde_json::to_string(&v020::ClientEvent::Ping { nonce: 3 }).unwrap();
        assert!(matches!(serde_json::from_str::<ClientEvent>(&old), Ok(ClientEvent::Ping { nonce: 3 })));
    }

    /// The wire types exactly as v0.3.0 had them (`watching`, but no `camera`).
    mod v030 {
        use super::super::{ChannelId, User, UserId};
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        pub struct VoiceMember {
            pub user: User,
            pub channel_id: Option<ChannelId>,
            pub muted: bool,
            pub deafened: bool,
            pub streaming: bool,
            #[serde(default, skip_serializing_if = "Vec::is_empty")]
            pub watching: Vec<UserId>,
        }

        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        pub enum ClientEvent {
            VoiceState { channel_id: Option<ChannelId>, muted: bool, deafened: bool, streaming: bool },
            Ping { nonce: u64 },
        }
    }

    #[test]
    fn camera_off_serialises_exactly_as_v030() {
        let m = VoiceMember { user: user(3), channel_id: Some(5), muted: true, deafened: false, streaming: true, watching: vec![2], camera: false, server_muted: false, audio_only: false };
        let json = serde_json::to_string(&m).unwrap();
        assert!(!json.contains("camera"));
        let old = v030::VoiceMember { user: user(3), channel_id: Some(5), muted: true, deafened: false, streaming: true, watching: vec![2] };
        assert_eq!(json, serde_json::to_string(&old).unwrap());
    }

    #[test]
    fn camera_on_is_ignored_by_the_v030_and_v020_shapes() {
        let m = VoiceMember { user: user(4), channel_id: Some(5), muted: false, deafened: true, streaming: false, watching: vec![], camera: true, server_muted: false, audio_only: false };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"camera\":true"));
        let o3: v030::VoiceMember = serde_json::from_str(&json).unwrap();
        assert_eq!((o3.user, o3.channel_id, o3.deafened), (user(4), Some(5), true));
        let o2: v020::VoiceMember = serde_json::from_str(&json).unwrap();
        assert_eq!((o2.user, o2.channel_id, o2.deafened), (user(4), Some(5), true));
        let ev = serde_json::to_string(&ServerEvent::VoiceStateChanged { member: m }).unwrap();
        assert!(matches!(serde_json::from_str::<v020::ServerEvent>(&ev), Ok(v020::ServerEvent::VoiceStateChanged { .. })));
    }

    #[test]
    fn json_without_camera_parses_with_camera_off() {
        let old = v030::VoiceMember { user: user(2), channel_id: Some(5), muted: false, deafened: false, streaming: true, watching: vec![9] };
        let new: VoiceMember = serde_json::from_str(&serde_json::to_string(&old).unwrap()).unwrap();
        assert!(!new.camera);
        assert_eq!(new.watching, vec![9]);
        let old = v020::VoiceMember { user: user(2), channel_id: None, muted: false, deafened: false, streaming: false };
        assert!(!serde_json::from_str::<VoiceMember>(&serde_json::to_string(&old).unwrap()).unwrap().camera);
    }

    #[test]
    fn voice_state_camera_is_compatible_both_ways() {
        // An old client's VoiceState (no camera) parses on a new hub with camera off.
        let old = serde_json::to_string(&v030::ClientEvent::VoiceState { channel_id: Some(5), muted: true, deafened: false, streaming: false }).unwrap();
        assert!(matches!(
            serde_json::from_str::<ClientEvent>(&old),
            Ok(ClientEvent::VoiceState { channel_id: Some(5), muted: true, camera: false, .. })
        ));
        // A new client's VoiceState (with camera) is still understood by an old hub's shape.
        let new = serde_json::to_string(&ClientEvent::VoiceState { channel_id: Some(5), muted: false, deafened: false, streaming: true, camera: true, audio_only: false }).unwrap();
        assert!(new.contains("\"camera\":true"));
        assert!(matches!(
            serde_json::from_str::<v020::ClientEvent>(&new),
            Ok(v020::ClientEvent::VoiceState { channel_id: Some(5), streaming: true, .. })
        ));
    }

    /// The wire types exactly as v0.4.1 had them (no roles, no personal passwords, no
    /// `server_muted`, none of the item-12 events).
    mod v041 {
        use super::super::{Channel, ChannelId, MessageId, UserId};
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        pub struct User {
            pub id: UserId,
            pub username: String,
        }

        #[derive(Debug, Clone, Serialize, Deserialize)]
        pub struct LoginRequest {
            pub username: String,
            pub server_password: String,
            pub protocol_version: u32,
        }

        #[derive(Debug, Clone, Serialize, Deserialize)]
        pub struct LoginResponse {
            pub token: String,
            pub user: User,
            pub server_name: String,
        }

        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        pub struct Message {
            pub id: MessageId,
            pub channel_id: ChannelId,
            pub author: User,
            pub content: String,
            pub created_at_ms: i64,
        }

        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        pub struct VoiceMember {
            pub user: User,
            pub channel_id: Option<ChannelId>,
            pub muted: bool,
            pub deafened: bool,
            pub streaming: bool,
            #[serde(default, skip_serializing_if = "Vec::is_empty")]
            pub watching: Vec<UserId>,
            #[serde(default, skip_serializing_if = "std::ops::Not::not")]
            pub camera: bool,
        }

        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        pub enum ServerEvent {
            Ready { user: User, channels: Vec<Channel>, online: Vec<User>, voice: Vec<VoiceMember> },
            MessageCreated { message: Message },
            UserOnline { user: User },
            UserOffline { user_id: UserId },
            VoiceStateChanged { member: VoiceMember },
            Pong { nonce: u64 },
            Error { error: String },
        }
    }

    fn role_user(id: i64, role: Role) -> User {
        User { id, username: format!("u{id}"), role, avatar: None, display_name: None }
    }

    #[test]
    fn a_member_serialises_exactly_as_v041() {
        let u = User::new(7, "Ana");
        let old = v041::User { id: 7, username: "Ana".into() };
        assert_eq!(serde_json::to_string(&u).unwrap(), serde_json::to_string(&old).unwrap());
        let m = VoiceMember { user: u, channel_id: Some(2), muted: true, deafened: false, streaming: false, watching: vec![], camera: true, server_muted: false, audio_only: false };
        let old = v041::VoiceMember { user: old, channel_id: Some(2), muted: true, deafened: false, streaming: false, watching: vec![], camera: true };
        assert_eq!(serde_json::to_string(&m).unwrap(), serde_json::to_string(&old).unwrap());
        // A plain login request too.
        let req = LoginRequest { username: "Ana".into(), server_password: "x".into(), protocol_version: PROTOCOL_VERSION, ..Default::default() };
        let old = v041::LoginRequest { username: "Ana".into(), server_password: "x".into(), protocol_version: PROTOCOL_VERSION };
        assert_eq!(serde_json::to_string(&req).unwrap(), serde_json::to_string(&old).unwrap());
    }

    #[test]
    fn v041_json_parses_as_members_without_account() {
        let old = v041::ServerEvent::Ready {
            user: v041::User { id: 1, username: "a".into() },
            channels: vec![],
            online: vec![v041::User { id: 1, username: "a".into() }],
            voice: vec![v041::VoiceMember {
                user: v041::User { id: 1, username: "a".into() },
                channel_id: Some(3),
                muted: false,
                deafened: false,
                streaming: true,
                watching: vec![],
                camera: false,
            }],
        };
        let new: ServerEvent = serde_json::from_str(&serde_json::to_string(&old).unwrap()).unwrap();
        let ServerEvent::Ready { user, voice, account, .. } = new else { panic!() };
        assert_eq!(user.role, Role::Member);
        assert_eq!(account, None, "an old hub sends no account: the client hides the role UI");
        assert!(!voice[0].server_muted);
        let resp = v041::LoginResponse { token: "t".into(), user: v041::User { id: 1, username: "a".into() }, server_name: "s".into() };
        let new: LoginResponse = serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert_eq!((new.user.role, new.device_token), (Role::Member, None));
        // An old client's login (no personal password) reads with nothing new set.
        let old = v041::LoginRequest { username: "a".into(), server_password: "".into(), protocol_version: 1 };
        let new: LoginRequest = serde_json::from_str(&serde_json::to_string(&old).unwrap()).unwrap();
        assert!(new.password.is_none() && new.device_token.is_none() && !new.remember_device);
    }

    #[test]
    fn new_json_is_read_by_the_v041_shapes() {
        let admin = role_user(2, Role::Admin);
        let json = serde_json::to_string(&admin).unwrap();
        assert!(json.contains("\"role\":\"admin\""));
        assert_eq!(serde_json::from_str::<v041::User>(&json).unwrap(), v041::User { id: 2, username: "u2".into() });
        let m = VoiceMember { user: admin.clone(), channel_id: Some(3), muted: false, deafened: false, streaming: false, watching: vec![], camera: false, server_muted: true, audio_only: false };
        let json = serde_json::to_string(&ServerEvent::VoiceStateChanged { member: m.clone() }).unwrap();
        assert!(json.contains("\"server_muted\":true"));
        assert!(matches!(serde_json::from_str::<v041::ServerEvent>(&json), Ok(v041::ServerEvent::VoiceStateChanged { .. })));
        let ready = ServerEvent::Ready {
            user: admin.clone(),
            channels: vec![],
            online: vec![admin.clone()],
            voice: vec![m],
            account: Some(Account { has_password: true, must_set_password: false, ..Default::default() }),
            chat: None,
            server_id: Some("0123456789abcdef0123456789abcdef".into()),
            stream_previews: false,
            e2ee: false,
        };
        assert!(matches!(serde_json::from_str::<v041::ServerEvent>(&serde_json::to_string(&ready).unwrap()), Ok(v041::ServerEvent::Ready { .. })));
        let msg = Message { id: 1, channel_id: 1, author: admin.clone(), content: "oi".into(), created_at_ms: 5, attachments: vec![], mentions: vec![], mention_everyone: false, reply: None, reactions: vec![] };
        let json = serde_json::to_string(&ServerEvent::MessageCreated { message: msg }).unwrap();
        assert!(matches!(serde_json::from_str::<v041::ServerEvent>(&json), Ok(v041::ServerEvent::MessageCreated { .. })));
        let resp = LoginResponse { token: "t".into(), user: admin, server_name: "s".into(), device_token: Some("d".into()) };
        assert!(serde_json::from_str::<v041::LoginResponse>(&serde_json::to_string(&resp).unwrap()).is_ok());
        // The new events are unknown to an old client: it fails to parse them and skips them.
        for ev in [
            ServerEvent::MessageDeleted { channel_id: 1, message_id: 2 },
            ServerEvent::ChannelsChanged { channels: vec![] },
            ServerEvent::UserUpdated { user: role_user(3, Role::Moderator) },
            ServerEvent::Moderated { action: ModAction::Move { channel_id: 4 }, by: role_user(1, Role::Owner) },
        ] {
            let json = serde_json::to_string(&ev).unwrap();
            assert!(serde_json::from_str::<v041::ServerEvent>(&json).is_err(), "{json}");
            assert!(serde_json::from_str::<ServerEvent>(&json).is_ok(), "{json}");
        }
    }

    #[test]
    fn wire_names_of_the_new_types() {
        assert_eq!(serde_json::to_string(&Role::Moderator).unwrap(), "\"moderator\"");
        assert_eq!(serde_json::from_str::<Role>("\"owner\"").unwrap(), Role::Owner);
        assert_eq!(serde_json::from_str::<Role>("\"superstar\"").unwrap(), Role::Member, "unknown roles read as member");
        assert_eq!(serde_json::to_string(&ModAction::Move { channel_id: 4 }).unwrap(), r#"{"kind":"move","channel_id":4}"#);
        assert_eq!(serde_json::to_string(&ModAction::ServerMute).unwrap(), r#"{"kind":"server_mute"}"#);
        let ev = ServerEvent::MessageDeleted { channel_id: 1, message_id: 2 };
        assert_eq!(serde_json::to_string(&ev).unwrap(), r#"{"type":"message_deleted","channel_id":1,"message_id":2}"#);
    }

    #[test]
    fn permission_matrix() {
        use Role::*;
        assert!(!Member.can_moderate() && Moderator.can_moderate() && Admin.can_moderate() && Owner.can_moderate());
        assert!(!Moderator.can_manage_channels() && Admin.can_manage_channels() && Owner.can_manage_channels());
        assert!(!Moderator.can_admin() && Admin.can_admin());
        assert!(!Admin.can_reset_passwords() && Owner.can_reset_passwords());
        // Person actions: strictly above the target.
        assert!(Moderator.outranks(Member) && !Moderator.outranks(Moderator) && !Moderator.outranks(Admin));
        assert!(Admin.outranks(Moderator) && !Admin.outranks(Admin) && Owner.outranks(Admin));
        assert!(!Member.outranks(Member));
        // Roles.
        assert!(Owner.can_set_role(Member, Admin) && Owner.can_set_role(Admin, Member));
        assert!(Admin.can_set_role(Member, Moderator) && Admin.can_set_role(Moderator, Member));
        assert!(!Admin.can_set_role(Member, Admin) && !Admin.can_set_role(Admin, Member));
        assert!(!Moderator.can_set_role(Member, Moderator));
        assert!(!Owner.can_set_role(Member, Owner) && !Owner.can_set_role(Owner, Member) && !Owner.can_set_role(Admin, Admin));
        assert_eq!(Admin.assignable(), vec![Member, Moderator]);
        assert!(Moderator.assignable().is_empty());
    }

    // ---- profile pictures: compatibility with the shapes before them

    /// `User` exactly as the roles release (item 12, before profile pictures) had it.
    mod v050roles {
        use super::super::{Role, UserId};
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        pub struct User {
            pub id: UserId,
            pub username: String,
            #[serde(default, skip_serializing_if = "Role::is_member")]
            pub role: Role,
        }
    }

    #[test]
    fn a_user_without_a_picture_serialises_exactly_as_before() {
        let u = User::new(7, "Ana");
        assert_eq!(serde_json::to_string(&u).unwrap(), r#"{"id":7,"username":"Ana"}"#);
        let old = v041::User { id: 7, username: "Ana".into() };
        assert_eq!(serde_json::to_string(&u).unwrap(), serde_json::to_string(&old).unwrap());
        let staff = role_user(2, Role::Admin);
        let old = v050roles::User { id: 2, username: "u2".into(), role: Role::Admin };
        assert_eq!(serde_json::to_string(&staff).unwrap(), serde_json::to_string(&old).unwrap());
    }

    #[test]
    fn json_from_older_hubs_parses_without_a_picture() {
        let u: User = serde_json::from_str(r#"{"id":7,"username":"Ana"}"#).unwrap();
        assert_eq!(u.avatar, None);
        let u: User = serde_json::from_str(r#"{"id":7,"username":"Ana","role":"moderator"}"#).unwrap();
        assert_eq!((u.role, u.avatar), (Role::Moderator, None));
        // Events and messages of the previous shapes.
        let old = v041::ServerEvent::UserOnline { user: v041::User { id: 1, username: "a".into() } };
        let ev: ServerEvent = serde_json::from_str(&serde_json::to_string(&old).unwrap()).unwrap();
        assert!(matches!(ev, ServerEvent::UserOnline { user } if user.avatar.is_none()));
        let msg = v041::Message { id: 1, channel_id: 2, author: v041::User { id: 1, username: "a".into() }, content: "oi".into(), created_at_ms: 3 };
        let m: Message = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(m.author.avatar, None);
    }

    #[test]
    fn a_picture_is_ignored_by_the_older_shapes() {
        let mut u = role_user(3, Role::Moderator);
        u.avatar = Some("0123abcd".into());
        let json = serde_json::to_string(&u).unwrap();
        assert!(json.contains(r#""avatar":"0123abcd""#));
        assert_eq!(serde_json::from_str::<v041::User>(&json).unwrap(), v041::User { id: 3, username: "u3".into() });
        assert_eq!(serde_json::from_str::<v050roles::User>(&json).unwrap().role, Role::Moderator);
        assert_eq!(serde_json::from_str::<User>(&json).unwrap(), u);
        // Inside the events an old client understands.
        let online = serde_json::to_string(&ServerEvent::UserOnline { user: u.clone() }).unwrap();
        assert!(matches!(serde_json::from_str::<v041::ServerEvent>(&online), Ok(v041::ServerEvent::UserOnline { .. })));
        let m = VoiceMember { user: u.clone(), channel_id: Some(3), muted: false, deafened: false, streaming: false, watching: vec![], camera: false, server_muted: false, audio_only: false };
        let json = serde_json::to_string(&ServerEvent::VoiceStateChanged { member: m }).unwrap();
        assert!(matches!(serde_json::from_str::<v041::ServerEvent>(&json), Ok(v041::ServerEvent::VoiceStateChanged { .. })));
        let msg = Message { id: 1, channel_id: 2, author: u.clone(), content: "oi".into(), created_at_ms: 3, attachments: vec![], mentions: vec![], mention_everyone: false, reply: None, reactions: vec![] };
        let json = serde_json::to_string(&ServerEvent::MessageCreated { message: msg }).unwrap();
        assert!(matches!(serde_json::from_str::<v041::ServerEvent>(&json), Ok(v041::ServerEvent::MessageCreated { .. })));
        // `UserUpdated` (the broadcast of a change) is unknown to a v0.4.1 client, which skips it.
        let upd = serde_json::to_string(&ServerEvent::UserUpdated { user: u }).unwrap();
        assert!(serde_json::from_str::<v041::ServerEvent>(&upd).is_err());
        assert!(matches!(serde_json::from_str::<ServerEvent>(&upd), Ok(ServerEvent::UserUpdated { user }) if user.avatar.as_deref() == Some("0123abcd")));
    }

    // ---- server owner: compatibility with the Account of the roles release

    #[test]
    fn account_without_owner_fields_is_byte_identical_to_v050() {
        let a = Account { has_password: true, must_set_password: false, ..Default::default() };
        assert_eq!(serde_json::to_string(&a).unwrap(), r#"{"has_password":true,"must_set_password":false}"#);
        // What a v0.5.0 hub sends reads as "an owner exists, not pinned".
        let old: Account = serde_json::from_str(r#"{"has_password":false,"must_set_password":true}"#).unwrap();
        assert_eq!(old, Account { has_password: false, must_set_password: true, no_owner: false, owner_pinned: false });
        let none: Account = serde_json::from_str("{}").unwrap();
        assert_eq!(none, Account::default());
        // The new fields are written only when set, and an old client (which ignores them) parses it.
        let a = Account { has_password: false, must_set_password: false, no_owner: true, owner_pinned: false };
        assert_eq!(serde_json::to_string(&a).unwrap(), r#"{"has_password":false,"must_set_password":false,"no_owner":true}"#);
        assert_eq!(serde_json::from_str::<Account>(&serde_json::to_string(&a).unwrap()).unwrap(), a);
        #[derive(Deserialize)]
        struct V050Account {
            has_password: bool,
            must_set_password: bool,
        }
        let json = serde_json::to_string(&Account { owner_pinned: true, no_owner: true, ..Default::default() }).unwrap();
        let old: V050Account = serde_json::from_str(&json).unwrap();
        assert!(!old.has_password && !old.must_set_password);
    }

    #[test]
    fn owner_requests_wire_shape() {
        assert_eq!(serde_json::to_string(&ClaimOwnerRequest { code: "K7QM-2XD9".into() }).unwrap(), r#"{"code":"K7QM-2XD9"}"#);
        assert_eq!(serde_json::to_string(&TransferOwnerRequest { user_id: 4 }).unwrap(), r#"{"user_id":4}"#);
        assert_eq!((AUDIT_CLAIM_OWNER, AUDIT_TRANSFER_OWNER, AUDIT_RESET_OWNER), ("claim_owner", "transfer_owner", "reset_owner"));
    }

    #[test]
    fn avatar_response_wire_shape() {
        assert_eq!(serde_json::to_string(&AvatarResponse { avatar: Some("ab".into()) }).unwrap(), r#"{"avatar":"ab"}"#);
        assert_eq!(serde_json::to_string(&AvatarResponse { avatar: None }).unwrap(), r#"{"avatar":null}"#);
        assert_eq!(serde_json::from_str::<AvatarResponse>("{}").unwrap(), AvatarResponse { avatar: None });
        assert_eq!(AVATAR_SIZES, [64, 256]);
    }

    /// The voice wire types exactly as v0.5.0 had them (`camera` and `server_muted`, but no
    /// `audio_only`). `User` has not changed since.
    mod v050 {
        use super::super::{ChannelId, User, UserId};
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        pub struct VoiceMember {
            pub user: User,
            pub channel_id: Option<ChannelId>,
            pub muted: bool,
            pub deafened: bool,
            pub streaming: bool,
            #[serde(default, skip_serializing_if = "Vec::is_empty")]
            pub watching: Vec<UserId>,
            #[serde(default, skip_serializing_if = "std::ops::Not::not")]
            pub camera: bool,
            #[serde(default, skip_serializing_if = "std::ops::Not::not")]
            pub server_muted: bool,
        }

        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        pub enum ClientEvent {
            VoiceState {
                channel_id: Option<ChannelId>,
                muted: bool,
                deafened: bool,
                streaming: bool,
                #[serde(default)]
                camera: bool,
            },
            Ping { nonce: u64 },
        }

        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        pub enum ServerEvent {
            Ready { user: User, channels: Vec<crate::Channel>, online: Vec<User>, voice: Vec<VoiceMember> },
            VoiceStateChanged { member: VoiceMember },
        }
    }

    #[test]
    fn audio_only_off_serialises_exactly_as_v050() {
        // A member without the flag is byte-identical to what a v0.5.0 hub sent (streaming or not).
        for streaming in [false, true] {
            let m = VoiceMember { user: user(3), channel_id: Some(5), muted: true, deafened: false, streaming, watching: vec![2], camera: true, server_muted: true, audio_only: false };
            let old = v050::VoiceMember { user: user(3), channel_id: Some(5), muted: true, deafened: false, streaming, watching: vec![2], camera: true, server_muted: true };
            let json = serde_json::to_string(&m).unwrap();
            assert!(!json.contains("audio_only"), "{json}");
            assert_eq!(json, serde_json::to_string(&old).unwrap());
        }
        // The whole event too.
        let m = VoiceMember { user: user(3), channel_id: None, muted: false, deafened: false, streaming: false, watching: vec![], camera: false, server_muted: false, audio_only: false };
        let old = v050::VoiceMember { user: user(3), channel_id: None, muted: false, deafened: false, streaming: false, watching: vec![], camera: false, server_muted: false };
        assert_eq!(
            serde_json::to_string(&ServerEvent::VoiceStateChanged { member: m }).unwrap(),
            serde_json::to_string(&v050::ServerEvent::VoiceStateChanged { member: old }).unwrap()
        );
    }

    #[test]
    fn audio_only_on_is_ignored_by_the_v050_shapes() {
        let m = VoiceMember { user: user(4), channel_id: Some(5), muted: false, deafened: false, streaming: true, watching: vec![], camera: false, server_muted: false, audio_only: true };
        let member_json = serde_json::to_string(&m).unwrap();
        assert!(member_json.contains("\"audio_only\":true"));
        let old: v050::VoiceMember = serde_json::from_str(&member_json).unwrap();
        assert_eq!((old.user, old.channel_id, old.streaming), (user(4), Some(5), true));
        let ev = serde_json::to_string(&ServerEvent::VoiceStateChanged { member: m.clone() }).unwrap();
        assert!(matches!(serde_json::from_str::<v050::ServerEvent>(&ev), Ok(v050::ServerEvent::VoiceStateChanged { .. })));
        let ready = serde_json::to_string(&ServerEvent::Ready { user: user(4), channels: vec![], online: vec![user(4)], voice: vec![m], account: None, chat: None, server_id: None, stream_previews: false, e2ee: false }).unwrap();
        assert!(matches!(serde_json::from_str::<v050::ServerEvent>(&ready), Ok(v050::ServerEvent::Ready { voice, .. }) if voice.len() == 1));
    }

    #[test]
    fn v050_member_json_parses_with_audio_only_off() {
        let old = v050::VoiceMember { user: user(2), channel_id: Some(5), muted: false, deafened: false, streaming: true, watching: vec![9], camera: true, server_muted: false };
        let new: VoiceMember = serde_json::from_str(&serde_json::to_string(&old).unwrap()).unwrap();
        assert!(!new.audio_only);
        assert!(new.streaming && new.camera);
        assert_eq!(new.watching, vec![9]);
    }

    #[test]
    fn voice_state_audio_only_is_compatible_both_ways() {
        // A v0.5.0 client's VoiceState (no audio_only) parses on a new hub with it off.
        let old = serde_json::to_string(&v050::ClientEvent::VoiceState { channel_id: Some(5), muted: true, deafened: false, streaming: true, camera: true }).unwrap();
        assert!(matches!(
            serde_json::from_str::<ClientEvent>(&old),
            Ok(ClientEvent::VoiceState { channel_id: Some(5), muted: true, streaming: true, camera: true, audio_only: false, .. })
        ));
        // A new client's VoiceState (audio-only live) is still understood by a v0.5.0 hub's shape.
        let new = serde_json::to_string(&ClientEvent::VoiceState { channel_id: Some(5), muted: false, deafened: false, streaming: true, camera: false, audio_only: true }).unwrap();
        assert!(new.contains("\"audio_only\":true"));
        assert!(matches!(
            serde_json::from_str::<v050::ClientEvent>(&new),
            Ok(v050::ClientEvent::VoiceState { channel_id: Some(5), streaming: true, .. })
        ));
        assert!(matches!(
            serde_json::from_str::<ClientEvent>(&new),
            Ok(ClientEvent::VoiceState { audio_only: true, streaming: true, .. })
        ));
    }

    // ---- private channels (v0.9): compatibility with the v0.8.x shapes

    /// The channel wire types exactly as v0.8.2 (and every version since v0.1) had them.
    mod v082 {
        use super::super::{ChannelId, ChannelKind};
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        pub struct Channel {
            pub id: ChannelId,
            pub name: String,
            pub kind: ChannelKind,
            pub position: i32,
        }

        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        pub struct ChannelCreateRequest {
            pub name: String,
            pub kind: ChannelKind,
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
        #[serde(default)]
        pub struct ChatFeatures {
            pub mentions: bool,
            pub replies: bool,
            pub reactions: bool,
        }

        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        pub enum ServerEvent {
            Ready {
                user: super::super::User,
                channels: Vec<Channel>,
                online: Vec<super::super::User>,
                voice: Vec<super::super::VoiceMember>,
                #[serde(default)]
                chat: Option<ChatFeatures>,
            },
            ChannelsChanged {
                channels: Vec<Channel>,
            },
        }
    }

    fn channel(id: i64, private: Option<PrivateAccess>) -> Channel {
        Channel { id, name: format!("c{id}"), kind: ChannelKind::Text, position: id as i32, private, e2ee: false }
    }

    #[test]
    fn a_public_channel_serialises_exactly_as_v082() {
        let old = v082::Channel { id: 3, name: "c3".into(), kind: ChannelKind::Text, position: 3 };
        assert_eq!(serde_json::to_string(&channel(3, None)).unwrap(), serde_json::to_string(&old).unwrap());
        // v0.8 JSON reads as public.
        let new: Channel = serde_json::from_str(&serde_json::to_string(&old).unwrap()).unwrap();
        assert_eq!(new, channel(3, None));
        let req = ChannelCreateRequest { name: "x".into(), kind: ChannelKind::Voice, private: None };
        let old = v082::ChannelCreateRequest { name: "x".into(), kind: ChannelKind::Voice };
        assert_eq!(serde_json::to_string(&req).unwrap(), serde_json::to_string(&old).unwrap());
    }

    #[test]
    fn a_private_channel_is_read_by_the_v082_shapes() {
        let p = PrivateAccess { roles: vec![Role::Member], users: vec![7, 9] };
        let ch = channel(4, Some(p.clone()));
        let json = serde_json::to_string(&ch).unwrap();
        assert!(json.contains(r#""private":{"roles":["member"],"users":[7,9]}"#), "{json}");
        assert_eq!(serde_json::from_str::<v082::Channel>(&json).unwrap(), v082::Channel { id: 4, name: "c4".into(), kind: ChannelKind::Text, position: 4 });
        assert_eq!(serde_json::from_str::<Channel>(&json).unwrap(), ch);
        // An empty list (only Admin and Dono) still says "private".
        let only_staff = serde_json::to_string(&channel(5, Some(PrivateAccess::default()))).unwrap();
        assert!(only_staff.contains(r#""private":{}"#), "{only_staff}");
        assert_eq!(serde_json::from_str::<Channel>(&only_staff).unwrap().private, Some(PrivateAccess::default()));
        // Inside the events an old app parses.
        for ev in [
            ServerEvent::Ready { user: user(1), channels: vec![ch.clone()], online: vec![], voice: vec![], account: None, chat: Some(ChatFeatures::ALL), server_id: None, stream_previews: false, e2ee: false },
            ServerEvent::ChannelsChanged { channels: vec![ch.clone(), channel(6, None)] },
        ] {
            let json = serde_json::to_string(&ev).unwrap();
            assert!(serde_json::from_str::<v082::ServerEvent>(&json).is_ok(), "{json}");
        }
        // The create request with `private` is read by an old hub (which ignores it).
        let req = ChannelCreateRequest { name: "x".into(), kind: ChannelKind::Text, private: Some(p) };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<v082::ChannelCreateRequest>(&json).unwrap().name, "x");
        assert_eq!(serde_json::from_str::<ChannelCreateRequest>(&json).unwrap(), req);
        // The access request: `{}` and `{"private":null}` both mean public.
        assert_eq!(serde_json::from_str::<ChannelAccessRequest>("{}").unwrap(), ChannelAccessRequest { private: None });
        assert_eq!(serde_json::from_str::<ChannelAccessRequest>(r#"{"private":null}"#).unwrap(), ChannelAccessRequest { private: None });
    }

    #[test]
    fn the_private_channels_feature_flag_is_compatible_both_ways() {
        let json = serde_json::to_string(&ChatFeatures::ALL).unwrap();
        assert!(json.contains(r#""private_channels":true"#), "{json}");
        assert_eq!(serde_json::from_str::<v082::ChatFeatures>(&json).unwrap(), v082::ChatFeatures { mentions: true, replies: true, reactions: true });
        // A v0.8 hub's flags: no private channels.
        let old = serde_json::to_string(&v082::ChatFeatures { mentions: true, replies: true, reactions: true }).unwrap();
        let new: ChatFeatures = serde_json::from_str(&old).unwrap();
        assert!(!new.private_channels && new.mentions);
        // Off: the v0.8 JSON exactly.
        assert_eq!(serde_json::to_string(&ChatFeatures { private_channels: false, ..ChatFeatures::ALL }).unwrap(), old);
    }

    #[test]
    fn private_access_rules() {
        use Role::*;
        let p = PrivateAccess { roles: vec![Member], users: vec![7] };
        assert!(p.allows(1, Member) && p.allows(7, Moderator), "by role, or by name");
        assert!(!p.allows(2, Moderator), "Membro does not include Moderador");
        assert!(p.allows(3, Admin) && p.allows(4, Owner), "Admin and Dono always");
        let none = PrivateAccess::default();
        assert!(!none.allows(1, Member) && !none.allows(1, Moderator) && none.allows(1, Admin));
        assert!(channel(1, None).allows(99, Member), "public: everybody");
        assert!(!channel(1, Some(none)).allows(99, Member));
        let messy = PrivateAccess { roles: vec![Member, Owner, Admin, Member, Moderator], users: vec![9, 3, 9] };
        assert_eq!(messy.normalized(), PrivateAccess { roles: vec![Moderator, Member], users: vec![3, 9] });
    }
}

/// v1.0.1: the hub's identity in `/api/health` and `Ready`, additive both ways.
#[cfg(test)]
mod server_id_tests {
    use super::*;

    /// `GET /api/health` exactly as v1.0.0 has it.
    #[derive(Deserialize)]
    struct HealthV100 {
        ok: bool,
        protocol_version: u32,
    }

    #[test]
    fn the_server_id_is_additive() {
        // An older hub's answer: no id.
        let old: HealthResponse = serde_json::from_str(r#"{"ok":true,"version":"1.0.0","protocol_version":1,"livekit_ok":true}"#).unwrap();
        assert_eq!(old.server_id, None);
        // A new hub's answer still reads in an older app.
        let new = HealthResponse { ok: true, version: "1.0.1".into(), protocol_version: PROTOCOL_VERSION, livekit_ok: false, server_id: Some("00112233445566778899aabbccddeeff".into()) };
        let json = serde_json::to_string(&new).unwrap();
        assert!(json.contains(r#""server_id":"00112233445566778899aabbccddeeff""#));
        let seen: HealthV100 = serde_json::from_str(&json).unwrap();
        assert!(seen.ok && seen.protocol_version == PROTOCOL_VERSION);
        // Without an id the JSON is byte-identical to v1.0.0's.
        let none = HealthResponse { server_id: None, ..new };
        assert!(!serde_json::to_string(&none).unwrap().contains("server_id"));
    }

    #[test]
    fn server_ids_are_32_lowercase_hex_digits() {
        assert!(is_server_id("00112233445566778899aabbccddeeff"));
        assert!(!is_server_id("00112233445566778899AABBCCDDEEFF"));
        assert!(!is_server_id("0011"));
        assert!(!is_server_id("00112233445566778899aabbccddeefg"));
        assert!(!is_server_id(""));
    }
}
