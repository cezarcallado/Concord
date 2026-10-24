//! User profiles (v0.6, no protocol bump): display name, "Sobre mim", accent colour, custom
//! status and presence. Everything here is optional on the wire, so a hub or a client that
//! predates it keeps working: older clients ignore the new fields and events, a newer client on an
//! older hub gets `404` and hides the profile features.
//!
//! The username stays the login identity (and the key of local settings); a *display name* is only
//! what people see. [`User::shown_name`] is the one place that chooses between the two.
//!
//! Endpoints (all authenticated):
//!
//! - `GET    /api/users/{id}/profile`   -> [`UserProfile`] (`404`: no such user, or a hub without
//!   profiles). An invisible person is `online: false` for everybody but themselves.
//! - `PUT    /api/me/profile`           [`ProfileUpdate`] -> [`UserProfile`] (mine). `400` with the
//!   reason (see [`ProfileError`]), `429` too many edits ([`PROFILE_RATE_LIMIT`] per
//!   [`PROFILE_RATE_WINDOW_SECS`]). A changed display name is broadcast as
//!   [`ServerEvent::UserUpdated`](crate::ServerEvent::UserUpdated).
//! - `PUT    /api/me/presence`          [`PresenceUpdate`] -> [`PresenceResponse`]
//! - `PUT    /api/me/status`            [`StatusUpdate`] -> [`StatusResponse`] (an empty emoji and text
//!   clears it); `DELETE /api/me/status` -> [`StatusResponse`]
//! - `POST   /api/mod/users/{id}/clear-profile` [`ModClearProfileRequest`] -> [`OkResponse`](crate::OkResponse)
//!   (Moderador+, above the target's role; audited as [`AUDIT_CLEAR_PROFILE`])
//!
//! Presence is *per person*, not per connection. A person who is [`Presence::Invisible`] is, for
//! everybody else, offline: the hub leaves them out of the online list, announces their going
//! invisible as [`ServerEvent::UserOffline`](crate::ServerEvent::UserOffline) (so a client that
//! predates presence also sees them leave) and coming back as `UserOnline`, and sends nothing else
//! about them to others. Everybody else's presence and custom status reach a client as
//! [`ServerEvent::PresenceChanged`](crate::ServerEvent::PresenceChanged): once right after `Ready`
//! for each person whose state is not plain "online, no status", then whenever it changes.

use serde::{Deserialize, Serialize};

use crate::User;

/// Display names: 1 to this many characters, or none.
pub const DISPLAY_NAME_MAX: usize = 32;
/// "Sobre mim".
pub const BIO_MAX: usize = 190;
/// Most line breaks a bio may have.
pub const BIO_MAX_LINES: usize = 8;
/// Custom status text.
pub const STATUS_TEXT_MAX: usize = 60;
/// A status emoji is at most this many code points (a ZWJ family is 7).
pub const STATUS_EMOJI_MAX_CHARS: usize = 8;
/// A status can be set to clear itself after at most this long.
pub const STATUS_MAX_SECS: u64 = 7 * 24 * 3600;
/// At most this many profile / status edits per person...
pub const PROFILE_RATE_LIMIT: usize = 12;
/// ...in this many seconds. (Presence changes have their own, looser limit.)
pub const PROFILE_RATE_WINDOW_SECS: u64 = 60;
pub const PRESENCE_RATE_LIMIT: usize = 30;
pub const PRESENCE_RATE_WINDOW_SECS: u64 = 60;

/// Audit action: a moderator cleared parts of someone's profile (abuse).
pub const AUDIT_CLEAR_PROFILE: &str = "clear_profile";

/// How a person has chosen to appear. `Away` can also be set automatically by the app after some
/// idle time (`PresenceUpdate::auto`), but only over `Online`: a manual choice is never overridden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    #[default]
    Online,
    /// "Ausente".
    Away,
    /// "Não perturbe": the client mutes its own alert sounds and direct-message notifications.
    Dnd,
    /// "Invisível": everybody else sees this person as offline.
    Invisible,
}

impl<'de> Deserialize<'de> for Presence {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = std::borrow::Cow::<'de, str>::deserialize(d)?;
        Ok(Presence::from_wire(&s))
    }
}

impl Presence {
    pub const ALL: [Presence; 4] = [Presence::Online, Presence::Away, Presence::Dnd, Presence::Invisible];

    pub fn wire_name(self) -> &'static str {
        match self {
            Presence::Online => "online",
            Presence::Away => "away",
            Presence::Dnd => "dnd",
            Presence::Invisible => "invisible",
        }
    }

    /// Inverse of [`Presence::wire_name`]; anything unknown (a newer hub's value) is `Online`.
    pub fn from_wire(s: &str) -> Presence {
        match s {
            "away" => Presence::Away,
            "dnd" => Presence::Dnd,
            "invisible" => Presence::Invisible,
            _ => Presence::Online,
        }
    }

    pub fn is_online(&self) -> bool {
        *self == Presence::Online
    }

    /// Do other people see this person at all?
    pub fn visible_to_others(self) -> bool {
        self != Presence::Invisible
    }
}

/// "Status personalizado": an emoji and/or a line of text, optionally clearing itself.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CustomStatus {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub emoji: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// Unix epoch milliseconds (the hub's clock) after which the status is gone; `None` = never.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
}

impl CustomStatus {
    pub fn is_empty(&self) -> bool {
        self.emoji.is_empty() && self.text.is_empty()
    }

    pub fn is_expired(&self, now_ms: i64) -> bool {
        self.expires_at_ms.is_some_and(|t| t <= now_ms)
    }

    /// `Some(self)` while it still counts (not empty, not expired).
    pub fn live(&self, now_ms: i64) -> Option<&CustomStatus> {
        (!self.is_empty() && !self.is_expired(now_ms)).then_some(self)
    }
}

/// One person's full profile, `GET /api/users/{id}/profile`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserProfile {
    pub user: User,
    /// "Sobre mim": plain text, at most [`BIO_MAX`] characters.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bio: String,
    /// The profile colour as `#rrggbb` (lowercase).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<CustomStatus>,
    /// Meaningful only when `online`.
    #[serde(default)]
    pub presence: Presence,
    /// Connected, and not invisible (to anyone but themselves).
    #[serde(default)]
    pub online: bool,
    /// When the person first logged in to this server.
    #[serde(default)]
    pub member_since_ms: i64,
}

/// `PUT /api/me/profile`: each field is optional; `None` leaves it as it is, `Some("")` clears it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProfileUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bio: Option<String>,
    /// `#rrggbb`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
}

/// `PUT /api/me/presence`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceUpdate {
    pub presence: Presence,
    /// Set by the app's idle detection, not by the person: `away` only applies over `online`,
    /// and `online` only undoes an automatic `away`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto: bool,
}

/// What the person's presence is after the call (an automatic change may have been ignored).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceResponse {
    pub presence: Presence,
}

/// `PUT /api/me/status`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StatusUpdate {
    #[serde(default)]
    pub emoji: String,
    #[serde(default)]
    pub text: String,
    /// Clear it after this many seconds (at most [`STATUS_MAX_SECS`]); `None` = never.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusResponse {
    #[serde(default)]
    pub status: Option<CustomStatus>,
}

/// `POST /api/mod/users/{id}/clear-profile`: what to clear.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModClearProfileRequest {
    #[serde(default)]
    pub bio: bool,
    #[serde(default)]
    pub status: bool,
    #[serde(default)]
    pub display_name: bool,
}

impl ModClearProfileRequest {
    pub fn is_empty(&self) -> bool {
        !(self.bio || self.status || self.display_name)
    }
}

impl User {
    /// What people see: the display name when there is one, else the username.
    pub fn shown_name(&self) -> &str {
        self.display_name.as_deref().map(str::trim).filter(|n| !n.is_empty()).unwrap_or(&self.username)
    }

    /// Does this person show a name other than their username?
    pub fn has_display_name(&self) -> bool {
        self.shown_name() != self.username
    }
}

// ---------------------------------------------------------------- validation

/// Why a profile field is refused. The `Display` text is the hub's (English) `400` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileError {
    DisplayNameLength,
    DisplayNameChars,
    BioLength,
    BioChars,
    AccentFormat,
    StatusTextLength,
    StatusTextChars,
    EmojiInvalid,
    ExpiryTooLong,
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProfileError::DisplayNameLength => write!(f, "display name must be 1-{DISPLAY_NAME_MAX} characters"),
            ProfileError::DisplayNameChars => write!(f, "display name has characters that are not allowed"),
            ProfileError::BioLength => write!(f, "bio must be at most {BIO_MAX} characters and {BIO_MAX_LINES} lines"),
            ProfileError::BioChars => write!(f, "bio has characters that are not allowed"),
            ProfileError::AccentFormat => write!(f, "colour must look like #rrggbb"),
            ProfileError::StatusTextLength => write!(f, "status text must be at most {STATUS_TEXT_MAX} characters"),
            ProfileError::StatusTextChars => write!(f, "status text has characters that are not allowed"),
            ProfileError::EmojiInvalid => write!(f, "status emoji must be a single emoji"),
            ProfileError::ExpiryTooLong => write!(f, "a status can clear itself after at most {} days", STATUS_MAX_SECS / 86_400),
        }
    }
}

impl std::error::Error for ProfileError {}

/// Characters that never belong in a name or a line of text: controls, and the invisible and
/// direction-changing characters used to spoof or hide text. (U+200D, the emoji joiner, is fine.)
pub fn is_forbidden_char(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{115F}' | '\u{1160}' | '\u{17B4}' | '\u{17B5}' | '\u{180E}'
            | '\u{200B}'..='\u{200C}' | '\u{200E}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}' | '\u{2060}'..='\u{206F}'
            | '\u{3164}' | '\u{FEFF}' | '\u{FFA0}' | '\u{FFF9}'..='\u{FFFB}'
            | '\u{E0000}'..='\u{E007F}')
}

/// A display name as stored: trimmed; `Ok(None)` for an empty one (= no display name).
pub fn clean_display_name(raw: &str) -> Result<Option<String>, ProfileError> {
    let name = raw.trim();
    if name.is_empty() {
        return Ok(None);
    }
    if name.chars().any(is_forbidden_char) {
        return Err(ProfileError::DisplayNameChars);
    }
    if name.chars().count() > DISPLAY_NAME_MAX {
        return Err(ProfileError::DisplayNameLength);
    }
    // Something must show: a name made only of joiners would be a blank.
    if name.chars().all(|c| c.is_whitespace() || c == '\u{200D}') {
        return Err(ProfileError::DisplayNameChars);
    }
    Ok(Some(name.to_string()))
}

/// "Sobre mim" as stored: trimmed, `\r\n` as `\n`; `Ok("")` for an empty one.
pub fn clean_bio(raw: &str) -> Result<String, ProfileError> {
    let text = raw.replace("\r\n", "\n").replace('\r', "\n");
    let text = text.trim();
    if text.chars().any(|c| c != '\n' && is_forbidden_char(c)) {
        return Err(ProfileError::BioChars);
    }
    if text.chars().count() > BIO_MAX || text.matches('\n').count() > BIO_MAX_LINES {
        return Err(ProfileError::BioLength);
    }
    Ok(text.to_string())
}

/// A colour as `#rrggbb` (lowercase); `Ok(None)` for an empty one. `#rgb` and a missing `#` are accepted.
pub fn clean_accent(raw: &str) -> Result<Option<String>, ProfileError> {
    let s = raw.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let hex = s.strip_prefix('#').unwrap_or(s);
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ProfileError::AccentFormat);
    }
    match hex.len() {
        6 => Ok(Some(format!("#{}", hex.to_ascii_lowercase()))),
        3 => Ok(Some(format!("#{}", hex.chars().flat_map(|c| [c, c]).collect::<String>().to_ascii_lowercase()))),
        _ => Err(ProfileError::AccentFormat),
    }
}

/// `#rrggbb` -> `[r, g, b]`.
pub fn accent_rgb(accent: &str) -> Option<[u8; 3]> {
    let hex = accent.strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let n = u32::from_str_radix(hex, 16).ok()?;
    Some([(n >> 16) as u8, (n >> 8) as u8, n as u8])
}

/// A status emoji: empty, or a short run of code points that is plainly an emoji and not text: at
/// least one non-ASCII character, no letters, no spaces, no controls; ASCII only as the base of a
/// keycap (`0-9 # *`).
pub fn clean_status_emoji(raw: &str) -> Result<String, ProfileError> {
    let e = raw.trim();
    if e.is_empty() {
        return Ok(String::new());
    }
    let n = e.chars().count();
    let ok = n <= STATUS_EMOJI_MAX_CHARS
        && !e.is_ascii()
        && e.chars().all(|c| {
            !is_forbidden_char(c)
                && !c.is_whitespace()
                && !c.is_alphabetic()
                && (!c.is_ascii() || matches!(c, '0'..='9' | '#' | '*'))
        });
    if ok {
        Ok(e.to_string())
    } else {
        Err(ProfileError::EmojiInvalid)
    }
}

/// A status as stored from the request fields: `Ok(None)` when both are empty.
pub fn clean_status(emoji: &str, text: &str, expires_in_secs: Option<u64>, now_ms: i64) -> Result<Option<CustomStatus>, ProfileError> {
    let emoji = clean_status_emoji(emoji)?;
    let text = text.trim();
    if text.chars().any(is_forbidden_char) {
        return Err(ProfileError::StatusTextChars);
    }
    if text.chars().count() > STATUS_TEXT_MAX {
        return Err(ProfileError::StatusTextLength);
    }
    if emoji.is_empty() && text.is_empty() {
        return Ok(None);
    }
    let expires_at_ms = match expires_in_secs {
        None => None,
        Some(s) if s > STATUS_MAX_SECS => return Err(ProfileError::ExpiryTooLong),
        Some(s) => Some(now_ms.saturating_add((s as i64).saturating_mul(1000))),
    };
    Ok(Some(CustomStatus { emoji, text: text.to_string(), expires_at_ms }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Role, ServerEvent};

    #[test]
    fn shown_name_falls_back_to_the_username() {
        let mut u = User::new(1, "ana");
        assert_eq!(u.shown_name(), "ana");
        u.display_name = Some("Ana Maria".into());
        assert_eq!(u.shown_name(), "Ana Maria");
        assert!(u.has_display_name());
        u.display_name = Some("   ".into());
        assert_eq!(u.shown_name(), "ana", "a blank display name is no display name");
        u.display_name = Some(String::new());
        assert!(!u.has_display_name());
    }

    #[test]
    fn display_names() {
        assert_eq!(clean_display_name("  Ana Maria "), Ok(Some("Ana Maria".into())));
        assert_eq!(clean_display_name("   "), Ok(None));
        assert_eq!(clean_display_name(""), Ok(None));
        assert_eq!(clean_display_name("Ünï 😀 çödé"), Ok(Some("Ünï 😀 çödé".into())));
        assert_eq!(clean_display_name(&"a".repeat(32)).unwrap().unwrap().len(), 32);
        assert_eq!(clean_display_name(&"a".repeat(33)), Err(ProfileError::DisplayNameLength));
        assert_eq!(clean_display_name("a\nb"), Err(ProfileError::DisplayNameChars));
        assert_eq!(clean_display_name("a\tb"), Err(ProfileError::DisplayNameChars));
        assert_eq!(clean_display_name("a\u{0}b"), Err(ProfileError::DisplayNameChars));
        // Direction overrides and zero-width characters are spoofing tools.
        assert_eq!(clean_display_name("evil\u{202E}gpj"), Err(ProfileError::DisplayNameChars));
        assert_eq!(clean_display_name("ad\u{200B}min"), Err(ProfileError::DisplayNameChars));
        assert_eq!(clean_display_name("\u{3164}"), Err(ProfileError::DisplayNameChars));
        // The emoji joiner is allowed inside a name.
        assert!(clean_display_name("👨\u{200D}👩").is_ok());
        assert_eq!(clean_display_name("\u{200D}"), Err(ProfileError::DisplayNameChars));
    }

    #[test]
    fn bios() {
        assert_eq!(clean_bio(""), Ok(String::new()));
        assert_eq!(clean_bio(" oi\r\nlinha 2 "), Ok("oi\nlinha 2".into()));
        assert_eq!(clean_bio(&"x".repeat(190)).unwrap().len(), 190);
        assert_eq!(clean_bio(&"x".repeat(191)), Err(ProfileError::BioLength));
        assert_eq!(clean_bio(&"é".repeat(190)).unwrap().chars().count(), 190, "characters, not bytes");
        assert_eq!(clean_bio(&"a\n".repeat(11)), Err(ProfileError::BioLength));
        assert_eq!(clean_bio("a\u{7}b"), Err(ProfileError::BioChars));
        assert_eq!(clean_bio("a\u{202E}b"), Err(ProfileError::BioChars));
        assert!(clean_bio("veja https://example.com/x?y=1 e 😀").is_ok());
    }

    #[test]
    fn accents() {
        assert_eq!(clean_accent("#FF8800"), Ok(Some("#ff8800".into())));
        assert_eq!(clean_accent("ff8800"), Ok(Some("#ff8800".into())));
        assert_eq!(clean_accent("#f80"), Ok(Some("#ff8800".into())));
        assert_eq!(clean_accent(""), Ok(None));
        for bad in ["#ff88", "#gg0000", "red", "#ff880000", "rgb(1,2,3)", "#ff 800"] {
            assert_eq!(clean_accent(bad), Err(ProfileError::AccentFormat), "{bad}");
        }
        assert_eq!(accent_rgb("#ff8800"), Some([255, 136, 0]));
        assert_eq!(accent_rgb("ff8800"), None);
        assert_eq!(accent_rgb("#ff88"), None);
    }

    #[test]
    fn status_emoji_sanity() {
        for good in ["🎮", "😀", "👨\u{200D}👩\u{200D}👧\u{200D}👦", "❤\u{FE0F}", "1\u{FE0F}\u{20E3}", "🇧🇷", "👍🏽", ""] {
            assert!(clean_status_emoji(good).is_ok(), "{good:?}");
        }
        for bad in ["a", "oi", "game", "🎮 🎮", "🎮\n🎮","1", "#", "ab😀", &"😀".repeat(9), "é", "\u{202E}😀"] {
            assert_eq!(clean_status_emoji(bad), Err(ProfileError::EmojiInvalid), "{bad:?}");
        }
    }

    #[test]
    fn statuses() {
        let now = 1_000_000;
        assert_eq!(clean_status("", "", None, now), Ok(None));
        assert_eq!(clean_status("  ", "  ", Some(60), now), Ok(None), "nothing to show is no status");
        let s = clean_status("🎮", " jogando ", Some(1800), now).unwrap().unwrap();
        assert_eq!((s.emoji.as_str(), s.text.as_str(), s.expires_at_ms), ("🎮", "jogando", Some(now + 1_800_000)));
        assert!(!s.is_expired(now + 1_799_999) && s.is_expired(now + 1_800_000));
        assert!(s.live(now).is_some() && s.live(now + 1_800_000).is_none());
        assert_eq!(clean_status("", &"x".repeat(60), None, now).unwrap().unwrap().expires_at_ms, None);
        assert_eq!(clean_status("", &"x".repeat(61), None, now), Err(ProfileError::StatusTextLength));
        assert_eq!(clean_status("", "a\nb", None, now), Err(ProfileError::StatusTextChars));
        assert_eq!(clean_status("abc", "x", None, now), Err(ProfileError::EmojiInvalid));
        assert_eq!(clean_status("🎮", "x", Some(STATUS_MAX_SECS + 1), now), Err(ProfileError::ExpiryTooLong));
        assert!(clean_status("🎮", "x", Some(STATUS_MAX_SECS), now).is_ok());
        assert!(CustomStatus::default().live(now).is_none());
    }

    #[test]
    fn wire_shapes() {
        assert_eq!(serde_json::to_string(&Presence::Dnd).unwrap(), "\"dnd\"");
        assert_eq!(serde_json::from_str::<Presence>("\"invisible\"").unwrap(), Presence::Invisible);
        assert_eq!(serde_json::from_str::<Presence>("\"sleeping\"").unwrap(), Presence::Online, "an unknown value reads as online");
        assert!(Presence::Online.is_online() && !Presence::Away.is_online());
        assert!(Presence::Dnd.visible_to_others() && !Presence::Invisible.visible_to_others());
        assert_eq!(serde_json::to_string(&PresenceUpdate { presence: Presence::Away, auto: false }).unwrap(), r#"{"presence":"away"}"#);
        assert_eq!(serde_json::to_string(&PresenceUpdate { presence: Presence::Away, auto: true }).unwrap(), r#"{"presence":"away","auto":true}"#);
        let u: PresenceUpdate = serde_json::from_str(r#"{"presence":"dnd"}"#).unwrap();
        assert_eq!((u.presence, u.auto), (Presence::Dnd, false));
        assert_eq!(serde_json::to_string(&ProfileUpdate::default()).unwrap(), "{}");
        let p: ProfileUpdate = serde_json::from_str(r##"{"bio":"","accent":"#fff"}"##).unwrap();
        assert_eq!((p.display_name, p.bio.as_deref(), p.accent.as_deref()), (None, Some(""), Some("#fff")));
        let s = CustomStatus { emoji: "🎮".into(), text: "".into(), expires_at_ms: None };
        assert_eq!(serde_json::to_string(&s).unwrap(), r#"{"emoji":"🎮"}"#);
        assert_eq!(serde_json::from_str::<CustomStatus>("{}").unwrap(), CustomStatus::default());
        assert!(ModClearProfileRequest::default().is_empty() && !ModClearProfileRequest { bio: true, ..Default::default() }.is_empty());
        assert_eq!(AUDIT_CLEAR_PROFILE, "clear_profile");
    }

    #[test]
    fn a_profile_reads_with_only_the_user() {
        // What a sparse (or future, trimmed) hub could send.
        let p: UserProfile = serde_json::from_str(r#"{"user":{"id":3,"username":"ana"}}"#).unwrap();
        assert_eq!((p.bio.as_str(), p.accent, p.status, p.presence, p.online, p.member_since_ms), ("", None, None, Presence::Online, false, 0));
        let full = UserProfile {
            user: User { id: 3, username: "ana".into(), role: Role::Admin, avatar: None, display_name: Some("Ana".into()) },
            bio: "oi".into(),
            accent: Some("#112233".into()),
            status: Some(CustomStatus { emoji: "🎮".into(), text: "x".into(), expires_at_ms: Some(5) }),
            presence: Presence::Dnd,
            online: true,
            member_since_ms: 99,
        };
        assert_eq!(serde_json::from_str::<UserProfile>(&serde_json::to_string(&full).unwrap()).unwrap(), full);
    }

    /// The shapes of v0.5.2, the last release before profiles.
    mod v052 {
        use crate::{Role, UserId};
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        pub struct User {
            pub id: UserId,
            pub username: String,
            #[serde(default, skip_serializing_if = "Role::is_member")]
            pub role: Role,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub avatar: Option<String>,
        }

        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        #[allow(clippy::large_enum_variant)]
        pub enum ServerEvent {
            Ready { user: User, channels: Vec<crate::Channel>, online: Vec<User>, voice: Vec<crate::VoiceMember> },
            MessageCreated { message: crate::Message },
            UserOnline { user: User },
            UserOffline { user_id: UserId },
            UserUpdated { user: User },
            Pong { nonce: u64 },
        }
    }

    #[test]
    fn a_user_without_a_display_name_serialises_exactly_as_v052() {
        let new = User { id: 7, username: "Ana".into(), role: Role::Moderator, avatar: Some("ab".into()), display_name: None };
        let old = v052::User { id: 7, username: "Ana".into(), role: Role::Moderator, avatar: Some("ab".into()) };
        assert_eq!(serde_json::to_string(&new).unwrap(), serde_json::to_string(&old).unwrap());
        assert_eq!(serde_json::to_string(&User::new(7, "Ana")).unwrap(), r#"{"id":7,"username":"Ana"}"#);
        // And an old hub's JSON reads with no display name.
        let back: User = serde_json::from_str(&serde_json::to_string(&old).unwrap()).unwrap();
        assert_eq!(back, new);
    }

    #[test]
    fn a_display_name_is_ignored_by_the_v052_shapes() {
        let mut u = User::new(7, "ana");
        u.display_name = Some("Ana Maria".into());
        let json = serde_json::to_string(&u).unwrap();
        assert!(json.contains(r#""display_name":"Ana Maria""#));
        let old: v052::User = serde_json::from_str(&json).unwrap();
        assert_eq!((old.id, old.username.as_str()), (7, "ana"));
        assert_eq!(serde_json::from_str::<User>(&json).unwrap(), u);
        // Inside the events an old client understands.
        for ev in [
            ServerEvent::UserOnline { user: u.clone() },
            ServerEvent::UserUpdated { user: u.clone() },
            ServerEvent::Ready { user: u.clone(), channels: vec![], online: vec![u.clone()], voice: vec![], account: None, chat: None, server_id: None, stream_previews: false, e2ee: false },
            ServerEvent::MessageCreated { message: crate::Message { id: 1, channel_id: 2, author: u.clone(), content: "oi".into(), created_at_ms: 3, attachments: vec![], mentions: vec![], mention_everyone: false, reply: None, reactions: vec![] } },
        ] {
            let json = serde_json::to_string(&ev).unwrap();
            assert!(serde_json::from_str::<v052::ServerEvent>(&json).is_ok(), "{json}");
        }
    }

    #[test]
    fn presence_changed_is_unknown_to_v052_clients_which_skip_it() {
        let ev = ServerEvent::PresenceChanged { user_id: 4, presence: Presence::Dnd, status: Some(CustomStatus { text: "x".into(), ..Default::default() }) };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(serde_json::from_str::<v052::ServerEvent>(&json).is_err());
        assert!(serde_json::from_str::<ServerEvent>(&json).is_ok());
        // Every other event keeps its shape: Ready has no presence field at all.
        let ready = ServerEvent::Ready { user: User::new(1, "a"), channels: vec![], online: vec![], voice: vec![], account: None, chat: None, server_id: None, stream_previews: false, e2ee: false };
        assert!(!serde_json::to_string(&ready).unwrap().contains("presence"));
    }

    #[test]
    fn presence_changed_wire_shape() {
        let ev = ServerEvent::PresenceChanged { user_id: 4, presence: Presence::Away, status: None };
        assert_eq!(serde_json::to_string(&ev).unwrap(), r#"{"type":"presence_changed","user_id":4,"presence":"away"}"#);
        let with = ServerEvent::PresenceChanged { user_id: 4, presence: Presence::Online, status: Some(CustomStatus { emoji: "🎮".into(), ..Default::default() }) };
        let back: ServerEvent = serde_json::from_str(&serde_json::to_string(&with).unwrap()).unwrap();
        assert!(matches!(back, ServerEvent::PresenceChanged { user_id: 4, status: Some(s), .. } if s.emoji == "🎮"));
    }
}
