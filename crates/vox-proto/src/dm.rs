//! Direct messages ("Mensagens diretas") and private 1:1 calls, added after v0.5.2 with no
//! protocol bump: new endpoints, new types and new events only; nothing existing changes.
//!
//! A conversation ([`DmConversation`]) belongs to exactly two people, and the hub delivers it,
//! its history and every event about it **only to those two**: nobody else (Admins and the owner
//! included) can list it, read it, delete from it, ring in it or get a voice token for its room.
//! Moderation does not reach DMs. The host can still read the hub's database file: DMs are private
//! from the other members, not end-to-end encrypted (see "DMs and E2E" in `docs/ROADMAP-v0.2.md`).
//!
//! HTTP (all authenticated; `{id}` is a [`DmId`]; a conversation that is not yours is `404`,
//! exactly like one that does not exist):
//! - `GET  /api/dms`                         -> `Vec<DmConversation>`, most recent activity first
//! - `GET  /api/dms/people`                  -> `Vec<User>`: everybody I can start a DM with
//! - `POST /api/dms/{user_id}`               -> [`DmConversation`]: open (or create) my DM with them
//! - `GET  /api/dms/{id}/messages?before=&limit=` -> `Vec<DmMessage>`, newest first, `limit <= 100`
//! - `POST /api/dms/{id}/messages`           [`DmSendRequest`] -> [`DmMessage`] (`400` empty or
//!   longer than [`DM_MESSAGE_MAX_CHARS`], `403` [`ERR_DM_BLOCKED`], `429` too fast: the same
//!   ~5 messages per second as the channels, shared with them)
//! - `POST /api/dms/{id}/messages/{message_id}/delete` -> `OkResponse` (only my own messages)
//! - `POST /api/dms/{id}/read`               [`DmReadRequest`] -> `OkResponse`
//! - `POST /api/dms/blocks/{user_id}`        [`DmBlockRequest`] -> `OkResponse`: a blocked person
//!   can neither message nor call me (and I cannot message them until I unblock)
//! - `POST /api/dms/{id}/call`               -> [`DmCallResponse`]: ring the other person (or, if
//!   a call of this DM already exists, join it: answer it as the callee, rejoin it as the caller).
//!   `403` blocked, `409` [`ERR_CALL_BUSY`] (they are in another call), `429` too many calls.
//! - `POST /api/dms/{id}/call/accept`        -> [`DmCallResponse`] (callee)
//! - `POST /api/dms/{id}/call/decline`       -> `OkResponse` (callee, while ringing)
//! - `POST /api/dms/{id}/call/end`           -> `OkResponse` (either: hang up, or cancel the ring)
//! - v1.1 (E1): `call` and `call/accept` take `?e2ee=1` from an app that can encrypt the call's
//!   media. A new call is end-to-end encrypted ([`DmCall::e2ee`]) when the caller said so and the
//!   callee has a device with encryption keys; its tokens then need `?e2ee=1` (`409`
//!   [`crate::ERR_E2EE_CALL_REQUIRED`] for an older app) and [`crate::VoiceJoinResponse::e2ee`] is
//!   set.
//!
//! Events, to the two participants only: `DmMessageCreated`, `DmMessageDeleted`, `DmRead` (my own
//! connections: another device read it), `CallInvite`, `CallAccepted`, `CallDeclined`,
//! `CallEnded` (see [`crate::ServerEvent`]). An older app skips them (it logs an unparseable
//! event); a new app on an older hub gets `404` from `GET /api/dms` and hides DMs.
//!
//! Calls: the hub makes a private LiveKit room [`dm_room`]`(a, b)` and hands tokens for it only to
//! the two people. It rings for [`CALL_RING_SECS`]; unanswered, declined or cancelled calls are
//! logged in the conversation as system lines ([`DmMessageKind`]). A DM call is not a voice
//! channel: it never shows in anybody's voice state, so nobody else sees who is in one. Joining a
//! voice channel ends your DM call; a person who stays offline for a short grace period leaves it.
//!
//! **End-to-end encryption (v1.1, phase E2; additive, no protocol bump).** An app whose peer has a
//! device key (`vox_proto::e2ee`) sends [`DmSendRequest::sealed`] (the `e2e1` body made by the
//! `vox-e2ee` crate, at most [`DM_SEALED_MAX_BYTES`]) with an empty `content`. The hub stores it
//! as is (`dm_messages.format = 'e2e1'`) and never reads it: a sealed line comes back with
//! [`DmMessage::sealed`] and, for older apps, `content` = [`DM_SEALED_PLACEHOLDER`]. Its images
//! are sealed uploads ([`crate::Attachment::sealed`]) and travel in
//! [`DmMessage::sealed_attachments`] (never in `attachments`, which an older app would try to
//! draw). A reply to a sealed line has [`crate::ReplyRef::sealed`] and no excerpt (the app fills
//! it from the line it decrypted). Reactions and call lines stay readable.

use serde::{Deserialize, Serialize};

use crate::{User, UserId, VoiceJoinResponse};

/// A conversation between two people.
pub type DmId = i64;
/// A message of a conversation (its own id space, separate from channel messages).
pub type DmMessageId = i64;
/// One call (ids restart with the hub; only meaningful while the call lasts).
pub type CallId = i64;

/// Longest message, in characters: the same limit as the channels.
pub const DM_MESSAGE_MAX_CHARS: usize = 4000;
/// Longest sealed body (E2), in characters of base64url.
pub const DM_SEALED_MAX_BYTES: usize = 32 * 1024;
/// `dm_messages.format` of a sealed line (E2).
pub const DM_FORMAT_SEALED: &str = "e2e1";
/// The `content` the hub gives a sealed line: what an app without E2 shows.
pub const DM_SEALED_PLACEHOLDER: &str = "Mensagem criptografada. Atualize o concord para ler.";
/// Most messages per history page.
pub const DM_PAGE_MAX: u32 = 100;
/// An unanswered call rings this long, then it is a missed call.
pub const CALL_RING_SECS: u64 = 30;
/// At most this many calls started per person...
pub const CALL_RATE_LIMIT: usize = 6;
/// ...in this many seconds.
pub const CALL_RATE_WINDOW_SECS: u64 = 60;

/// `403` answer: one of the two blocked the other (the text does not say who).
pub const ERR_DM_BLOCKED: &str = "blocked";
/// `409` answer of `POST /api/dms/{id}/call`: the other person is in another call.
pub const ERR_CALL_BUSY: &str = "busy";

/// The LiveKit room of the private call between `a` and `b` (the same whoever calls).
pub fn dm_room(a: UserId, b: UserId) -> String {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    format!("dm-{lo}-{hi}")
}

/// A conversation as one of its two people sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmConversation {
    pub id: DmId,
    /// The other person.
    pub peer: User,
    /// The newest message (text or a call line), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message: Option<DmMessage>,
    /// When the newest message was written, or the conversation opened (Unix ms): the list order.
    pub last_activity_ms: i64,
    /// Messages from the other person I have not read yet (kept by the hub, so every device of
    /// mine agrees).
    #[serde(default)]
    pub unread: u32,
    /// The newest message id I have read.
    #[serde(default)]
    pub read_up_to: DmMessageId,
    /// I blocked the other person.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub blocked: bool,
}

/// What a DM line is. An unknown kind from a newer hub reads as [`DmMessageKind::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DmMessageKind {
    /// A message somebody wrote.
    #[default]
    Text,
    /// System line: `author` called and nobody answered (rang out, or the caller gave up).
    CallMissed,
    /// System line: `author` called and the other person declined.
    CallDeclined,
    /// System line: a call `author` started took place; [`DmMessage::call_secs`] long.
    CallEnded,
    /// Something a newer hub knows about; shown as its text.
    Other,
}

impl<'de> Deserialize<'de> for DmMessageKind {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = std::borrow::Cow::<'de, str>::deserialize(d)?;
        Ok(DmMessageKind::from_wire(&s))
    }
}

impl DmMessageKind {
    pub fn wire_name(self) -> &'static str {
        match self {
            DmMessageKind::Text => "text",
            DmMessageKind::CallMissed => "call_missed",
            DmMessageKind::CallDeclined => "call_declined",
            DmMessageKind::CallEnded => "call_ended",
            DmMessageKind::Other => "other",
        }
    }

    pub fn from_wire(s: &str) -> Self {
        match s {
            "text" => DmMessageKind::Text,
            "call_missed" => DmMessageKind::CallMissed,
            "call_declined" => DmMessageKind::CallDeclined,
            "call_ended" => DmMessageKind::CallEnded,
            _ => DmMessageKind::Other,
        }
    }

    pub fn is_text(&self) -> bool {
        *self == DmMessageKind::Text
    }

    /// A call line (written by the hub, never deletable).
    pub fn is_call(&self) -> bool {
        matches!(self, DmMessageKind::CallMissed | DmMessageKind::CallDeclined | DmMessageKind::CallEnded)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmMessage {
    pub id: DmMessageId,
    pub dm_id: DmId,
    /// Who wrote it (for call lines: who called).
    pub author: User,
    /// The text (empty for call lines).
    pub content: String,
    /// Unix epoch milliseconds (UTC).
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "DmMessageKind::is_text")]
    pub kind: DmMessageKind,
    /// [`DmMessageKind::CallEnded`]: how long the call lasted.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub call_secs: u32,
    /// Images (added after v0.6.0, no protocol bump), same rules as [`crate::Message::attachments`]:
    /// only the two participants can fetch them. Show [`DmMessage::caption`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<crate::Attachment>,
    /// The line this one answers (v0.8, see [`crate::chat`]): absent when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<crate::ReplyRef>,
    /// Emoji reactions (v0.8): absent when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<crate::Reaction>,
    /// E2: the sealed body (`content` is then [`DM_SEALED_PLACEHOLDER`]); absent for a readable line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed: Option<String>,
    /// E2: the line's sealed images (ciphertext on the hub; their keys are inside `sealed`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sealed_attachments: Vec<crate::Attachment>,
}

impl DmMessage {
    /// An end-to-end encrypted line.
    pub fn is_sealed(&self) -> bool {
        self.sealed.is_some()
    }
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// `POST /api/dms/{id}/messages`
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmSendRequest {
    /// The text (may be empty when there are images).
    #[serde(default)]
    pub content: String,
    /// Up to [`crate::ATTACHMENT_MAX_PER_MESSAGE`] of my pending attachments (added after v0.6.0;
    /// absent when there are none, so the JSON is byte-identical to before).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<crate::AttachmentId>,
    /// A line of this conversation this one answers (v0.8; absent when none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<DmMessageId>,
    /// E2: the sealed body (then `content` must be empty and every image a sealed upload).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed: Option<String>,
}

/// `POST /api/dms/{id}/read`: everything up to (and including) this message is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmReadRequest {
    pub up_to: DmMessageId,
}

/// `POST /api/dms/blocks/{user_id}`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmBlockRequest {
    pub blocked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallState {
    /// The callee's app is ringing.
    #[default]
    Ringing,
    /// Answered: both may be in the room.
    Active,
}

/// A private call between the two people of a DM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmCall {
    pub id: CallId,
    pub dm_id: DmId,
    pub caller: User,
    pub callee: User,
    pub state: CallState,
    /// When it started ringing (Unix ms).
    pub started_at_ms: i64,
    /// v1.1 (E1): the call is end-to-end encrypted: the caller asked with `?e2ee=1` and the callee
    /// has a device with encryption keys. Decided when it starts ringing and kept for the whole
    /// call; tokens for it are then only given with `?e2ee=1` (an older app answering gets `409`
    /// [`crate::ERR_E2EE_CALL_REQUIRED`]). Absent when false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub e2ee: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl DmCall {
    /// The other person of the call, seen from `me`.
    pub fn peer_of(&self, me: UserId) -> &User {
        if self.caller.id == me { &self.callee } else { &self.caller }
    }
}

/// Why a call ended ([`crate::ServerEvent::CallEnded`]). Unknown reasons read as `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallEndReason {
    /// One of them hung up an answered call.
    #[default]
    Hangup,
    /// Nobody answered in [`CALL_RING_SECS`].
    Missed,
    /// The caller gave up before an answer.
    Cancelled,
    /// One of them stayed offline (closed the app, lost the connection).
    Offline,
    /// One of them joined a voice channel.
    JoinedVoice,
    Other,
}

impl<'de> Deserialize<'de> for CallEndReason {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = std::borrow::Cow::<'de, str>::deserialize(d)?;
        Ok(match s.as_ref() {
            "hangup" => CallEndReason::Hangup,
            "missed" => CallEndReason::Missed,
            "cancelled" => CallEndReason::Cancelled,
            "offline" => CallEndReason::Offline,
            "joined_voice" => CallEndReason::JoinedVoice,
            _ => CallEndReason::Other,
        })
    }
}

/// Answer of `POST /api/dms/{id}/call` and `/call/accept`: the call and the LiveKit token for its
/// private room (`join.room` is [`dm_room`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DmCallResponse {
    pub call: DmCall,
    pub join: VoiceJoinResponse,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_names_do_not_depend_on_who_calls() {
        assert_eq!(dm_room(7, 3), "dm-3-7");
        assert_eq!(dm_room(3, 7), "dm-3-7");
        assert!(!dm_room(3, 7).starts_with("voice-"));
    }

    #[test]
    fn a_text_message_has_no_kind_and_unknown_kinds_are_tolerated() {
        let m = DmMessage { id: 1, dm_id: 2, author: User::new(3, "a"), content: "oi".into(), created_at_ms: 4, kind: DmMessageKind::Text, call_secs: 0, attachments: vec![], reply: None, reactions: vec![], sealed: None, sealed_attachments: vec![] };
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, r#"{"id":1,"dm_id":2,"author":{"id":3,"username":"a"},"content":"oi","created_at_ms":4}"#);
        assert_eq!(serde_json::from_str::<DmMessage>(&json).unwrap(), m);
        let call = DmMessage { kind: DmMessageKind::CallEnded, call_secs: 61, content: String::new(), ..m.clone() };
        let json = serde_json::to_string(&call).unwrap();
        assert!(json.contains(r#""kind":"call_ended","call_secs":61"#), "{json}");
        let future = json.replace("call_ended", "video_note");
        assert_eq!(serde_json::from_str::<DmMessage>(&future).unwrap().kind, DmMessageKind::Other);
        assert!(DmMessageKind::CallMissed.is_call() && !DmMessageKind::Text.is_call());
    }

    #[test]
    fn conversation_and_call_wire_shapes() {
        let c = DmConversation { id: 5, peer: User::new(2, "b"), last_message: None, last_activity_ms: 9, unread: 0, read_up_to: 0, blocked: false };
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(json, r#"{"id":5,"peer":{"id":2,"username":"b"},"last_activity_ms":9,"unread":0,"read_up_to":0}"#);
        assert_eq!(serde_json::from_str::<DmConversation>(r#"{"id":5,"peer":{"id":2,"username":"b"},"last_activity_ms":9}"#).unwrap(), c);
        let call = DmCall { id: 1, dm_id: 5, caller: User::new(1, "a"), callee: User::new(2, "b"), state: CallState::Ringing, started_at_ms: 3, e2ee: false };
        assert_eq!(call.peer_of(1).id, 2);
        assert_eq!(call.peer_of(2).id, 1);
        assert!(serde_json::to_string(&call).unwrap().contains(r#""state":"ringing""#));
        assert_eq!(serde_json::to_string(&CallEndReason::JoinedVoice).unwrap(), r#""joined_voice""#);
        assert_eq!(serde_json::from_str::<CallEndReason>(r#""exploded""#).unwrap(), CallEndReason::Other);
    }

    /// The events a v0.5.2 app understands (a subset is enough: an unknown tag fails to parse).
    mod v052 {
        use crate::{Message, UserId};
        use serde::Deserialize;

        #[derive(Debug, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        #[allow(dead_code, clippy::large_enum_variant)]
        pub enum ServerEvent {
            MessageCreated { message: Message },
            UserOffline { user_id: UserId },
        }
    }

    #[test]
    fn the_new_events_are_skipped_by_a_v052_app_and_read_by_a_new_one() {
        use crate::ServerEvent;
        let msg = DmMessage { id: 1, dm_id: 2, author: User::new(3, "a"), content: "oi".into(), created_at_ms: 4, kind: DmMessageKind::Text, call_secs: 0, attachments: vec![], reply: None, reactions: vec![], sealed: None, sealed_attachments: vec![] };
        let call = DmCall { id: 1, dm_id: 2, caller: User::new(3, "a"), callee: User::new(4, "b"), state: CallState::Active, started_at_ms: 3, e2ee: false };
        for ev in [
            ServerEvent::DmMessageCreated { message: msg },
            ServerEvent::DmMessageDeleted { dm_id: 2, message_id: 1 },
            ServerEvent::DmRead { dm_id: 2, up_to: 1 },
            ServerEvent::CallInvite { call: call.clone() },
            ServerEvent::CallAccepted { call },
            ServerEvent::CallDeclined { dm_id: 2, call_id: 1 },
            ServerEvent::CallEnded { dm_id: 2, call_id: 1, reason: CallEndReason::Missed },
        ] {
            let json = serde_json::to_string(&ev).unwrap();
            assert!(serde_json::from_str::<v052::ServerEvent>(&json).is_err(), "{json}");
            assert!(serde_json::from_str::<ServerEvent>(&json).is_ok(), "{json}");
        }
        let json = serde_json::to_string(&ServerEvent::DmMessageDeleted { dm_id: 2, message_id: 1 }).unwrap();
        assert_eq!(json, r#"{"type":"dm_message_deleted","dm_id":2,"message_id":1}"#);
    }

    /// `DmMessage` / `Attachment` exactly as v1.0.2 had them (E2's compatibility promise).
    mod v102 {
        use serde::Deserialize;

        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        pub struct Attachment {
            pub id: i64,
            pub mime: String,
            pub width: u32,
            pub height: u32,
            pub size: u64,
        }

        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        pub struct DmMessage {
            pub id: i64,
            pub dm_id: i64,
            pub content: String,
            pub created_at_ms: i64,
            #[serde(default)]
            pub attachments: Vec<Attachment>,
        }

        #[derive(Debug, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        #[allow(dead_code)]
        pub enum ServerEvent {
            DmMessageCreated { message: DmMessage },
        }
    }

    #[test]
    fn a_sealed_line_reads_as_the_placeholder_in_an_old_app() {
        let img = crate::Attachment { id: 9, mime: "application/octet-stream".into(), width: 0, height: 0, size: 1234, thumb_w: 0, thumb_h: 0, animated: false, hash: "ab".into(), sealed: true };
        let m = DmMessage {
            id: 1,
            dm_id: 2,
            author: User::new(3, "a"),
            content: DM_SEALED_PLACEHOLDER.into(),
            created_at_ms: 4,
            kind: DmMessageKind::Text,
            call_secs: 0,
            attachments: vec![],
            reply: None,
            reactions: vec![],
            sealed: Some("AQID".into()),
            sealed_attachments: vec![img],
        };
        let json = serde_json::to_string(&crate::ServerEvent::DmMessageCreated { message: m.clone() }).unwrap();
        assert!(json.contains(r#""sealed":"AQID""#) && json.contains(r#""sealed_attachments":[{"id":9"#), "{json}");
        let v102::ServerEvent::DmMessageCreated { message } = serde_json::from_str::<v102::ServerEvent>(&json).unwrap();
        assert_eq!(message.content, "Mensagem criptografada. Atualize o concord para ler.");
        assert!(message.attachments.is_empty(), "an old app never sees the ciphertext as an image");
        // A new app gets everything back.
        let crate::ServerEvent::DmMessageCreated { message } = serde_json::from_str(&json).unwrap() else { panic!() };
        assert_eq!(message, m);
        assert!(message.is_sealed());
        // An old app's request (and an old hub's line) has no sealed parts.
        let req: DmSendRequest = serde_json::from_str(r#"{"content":"oi"}"#).unwrap();
        assert_eq!(req.sealed, None);
        assert_eq!(serde_json::to_string(&DmSendRequest { sealed: Some("x".into()), ..Default::default() }).unwrap(), r#"{"content":"","sealed":"x"}"#);
        let old: DmMessage = serde_json::from_str(r#"{"id":1,"dm_id":2,"author":{"id":3,"username":"a"},"content":"oi","created_at_ms":4}"#).unwrap();
        assert!(!old.is_sealed() && old.sealed_attachments.is_empty());
    }
}
