//! Mentions, replies and reactions ("Menções, respostas e reações", v0.8), added after v0.7.0
//! with no protocol bump: new optional fields, one new request type and two new events; nothing
//! existing changes meaning.
//!
//! **Mentions** (text channels). The text keeps what was typed (`@Tati`, `@todos`), so an older app
//! shows it as it is; the people it mentions travel as user ids in [`crate::Message::mentions`]. A
//! sender lists the ids it means (`mentions` of [`crate::ClientEvent::SendMessage`] /
//! [`crate::SendMessageRequest`]); the hub keeps only real people whose `@name` (username or
//! display name, any case, see [`find_mention`]) is in the text, at most [`MENTIONS_MAX`].
//! `@todos` ([`MENTION_EVERYONE`]) mentions everybody, but only when a Moderador or above writes
//! it ([`crate::Message::mention_everyone`]); anybody else's `@todos` is plain text. The hub reads
//! `@todos` from the text itself, so it works from older apps too. DMs have no mention ids (both
//! people are always told).
//!
//! **Replies** (channels and DMs). `reply_to` on a send names a message of the same channel / DM;
//! anything else is dropped (the message goes out as a plain one). Every message that is a reply
//! carries a [`ReplyRef`]: the original's author and a one-line excerpt, or `deleted` once it is
//! gone, filled by the hub for history pages in one query per page. `reply_mention` (Discord's
//! "@ ligado", on by default in the app) also mentions the original's author. An older app shows a
//! reply as a plain message.
//!
//! **Reactions** (channels and DMs): `POST /api/messages/{id}/reactions` and
//! `POST /api/dms/{id}/messages/{message_id}/reactions` with a [`ReactionRequest`] ->
//! [`crate::OkResponse`]. `400` not a single emoji ([`clean_reaction_emoji`]), `404` no such message
//! (or, for DMs, not my conversation), `403` [`crate::ERR_DM_BLOCKED`], `409`
//! [`ERR_TOO_MANY_REACTIONS`] (a message has at most [`REACTIONS_MAX_PER_MESSAGE`] distinct emoji),
//! `429` too fast. Every change goes out as [`crate::ServerEvent::ReactionChanged`] (to everybody who
//! may see the channel; v0.9: a private channel's message is `404` without access)
//! or [`crate::ServerEvent::DmReactionChanged`] (to the two people of the DM only); history pages
//! carry [`crate::Message::reactions`]. Deleting a message deletes its reactions.
//!
//! A hub that knows all this says so in [`crate::ServerEvent::Ready`] (`chat`); a new app on an
//! older hub hides replies, reactions and the mention list. An older app skips the new events (it
//! logs an event it cannot parse) and ignores the new fields.

use serde::{Deserialize, Serialize};

use crate::{MessageId, UserId};

/// Most people one message mentions by id.
pub const MENTIONS_MAX: usize = 20;
/// Most mention ids a message may *carry* (v1.0.1): a longer list is refused before any work
/// (`400` over HTTP, an error line over the WebSocket). An app never sends more than
/// [`MENTIONS_MAX`]; the margin is for ids that are dropped (people without access, names no
/// longer in the text).
pub const MENTIONS_WIRE_MAX: usize = 50;
/// The answer to a message whose mention list is longer than [`MENTIONS_WIRE_MAX`].
pub const ERR_TOO_MANY_MENTIONS: &str = "too many mentions";
/// `@todos`: everybody (Moderador and above only).
pub const MENTION_EVERYONE: &str = "todos";
/// Most distinct emoji on one message.
pub const REACTIONS_MAX_PER_MESSAGE: usize = 20;
/// A reaction is one emoji: at most this many code points (a ZWJ family is 7, a flag 2)...
pub const REACTION_EMOJI_MAX_CHARS: usize = 10;
/// ...and this many bytes.
pub const REACTION_EMOJI_MAX_BYTES: usize = 40;
/// The excerpt of a quoted message, in characters.
pub const REPLY_EXCERPT_MAX_CHARS: usize = 120;

/// `409` answer: the message already has [`REACTIONS_MAX_PER_MESSAGE`] distinct emoji.
pub const ERR_TOO_MANY_REACTIONS: &str = "too many reactions";

/// What the hub's chat knows beyond plain messages ([`crate::ServerEvent::Ready`]'s `chat`;
/// absent from older hubs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatFeatures {
    pub mentions: bool,
    pub replies: bool,
    pub reactions: bool,
    /// v0.9: private channels ([`crate::Channel::private`], [`crate::ChannelAccessRequest`]). A new
    /// app offers "Canal privado" only when this is set (an older hub would create a *public*
    /// channel). Absent from the JSON when false; older apps ignore it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub private_channels: bool,
}

impl ChatFeatures {
    /// Everything this version knows.
    pub const ALL: ChatFeatures = ChatFeatures { mentions: true, replies: true, reactions: true, private_channels: true };
}

/// The message a reply answers, as shown above it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReplyRef {
    /// The original (a channel message id, or a DM line id for DMs).
    pub id: MessageId,
    /// Who wrote it (`None` once it is deleted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<crate::User>,
    /// Its text on one line, cut to [`REPLY_EXCERPT_MAX_CHARS`] (without the `[imagem]` marker).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub excerpt: String,
    /// How many images it has.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub images: u32,
    /// The original was deleted: show "mensagem apagada".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
    /// E2: the original is an end-to-end encrypted DM line: no excerpt from the hub, the app fills
    /// it from the line it decrypted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sealed: bool,
}

impl ReplyRef {
    /// The quote of a message that no longer exists.
    pub fn gone(id: MessageId) -> Self {
        ReplyRef { id, deleted: true, ..Default::default() }
    }
}

/// One emoji on a message and who put it there (in order).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    pub emoji: String,
    pub users: Vec<UserId>,
}

/// `POST /api/messages/{id}/reactions` and `POST /api/dms/{id}/messages/{message_id}/reactions`:
/// put my `emoji` on the message (`add`), or take it off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionRequest {
    pub emoji: String,
    pub add: bool,
}

pub(crate) fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Variation selectors, skin tones, ZWJ and the keycap mark: they belong to the emoji before them.
fn is_emoji_modifier(c: char) -> bool {
    matches!(c, '\u{FE0E}' | '\u{FE0F}' | '\u{200D}' | '\u{20E3}' | '\u{1F3FB}'..='\u{1F3FF}')
}

fn is_regional_indicator(c: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)
}

/// A reaction emoji, trimmed: exactly one emoji (one base, plus variation selectors, a skin tone,
/// ZWJ-joined parts, or a flag's two regional indicators), at most [`REACTION_EMOJI_MAX_CHARS`]
/// code points and [`REACTION_EMOJI_MAX_BYTES`] bytes, no letters, spaces or controls (ASCII only
/// as a keycap's base). `None` when it is not one.
pub fn clean_reaction_emoji(raw: &str) -> Option<String> {
    let e = raw.trim();
    let n = e.chars().count();
    if n == 0 || n > REACTION_EMOJI_MAX_CHARS || e.len() > REACTION_EMOJI_MAX_BYTES || e.is_ascii() {
        return None;
    }
    let plain = e.chars().all(|c| {
        !crate::is_forbidden_char(c) && !c.is_whitespace() && !c.is_alphabetic() && (!c.is_ascii() || matches!(c, '0'..='9' | '#' | '*'))
    });
    if !plain {
        return None;
    }
    // One base: every other code point is a modifier, the part after a ZWJ, or a flag's pair.
    let mut bases = 0;
    let mut after_zwj = false;
    let mut flag_half = false;
    for c in e.chars() {
        if c == '\u{200D}' {
            after_zwj = true;
            continue;
        }
        if is_emoji_modifier(c) {
            continue;
        }
        if is_regional_indicator(c) && flag_half {
            flag_half = false;
            continue;
        }
        if !after_zwj {
            bases += 1;
        }
        after_zwj = false;
        flag_half = is_regional_indicator(c);
    }
    (bases == 1 && !after_zwj).then(|| e.to_string())
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.' || c == '-'
}

/// Where `@name` appears in `text` as a mention (any case; not inside a word or an e-mail address,
/// not followed by more of a name): the byte ranges of `@name`.
pub fn find_mention(text: &str, name: &str) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    if name.is_empty() {
        return out;
    }
    let mut prev: Option<char> = None;
    for (i, c) in text.char_indices() {
        if c == '@' && prev.is_none_or(|p| !is_word_char(p) && p != '@') {
            let rest = &text[i + 1..];
            let mut ti = rest.char_indices();
            let mut end = Some(0);
            for nc in name.chars() {
                match ti.next() {
                    Some((j, tc)) if tc.to_lowercase().eq(nc.to_lowercase()) => end = Some(j + tc.len_utf8()),
                    _ => {
                        end = None;
                        break;
                    }
                }
            }
            if let Some(e) = end {
                let next = rest[e..].chars().next();
                if next.is_none_or(|n| !is_word_char(n) || (n == '.' && rest[e + 1..].chars().next().is_none_or(|x| !is_word_char(x)))) {
                    out.push(i..i + 1 + e);
                }
            }
        }
        prev = Some(c);
    }
    out
}

/// `@todos` is in the text (as a mention, see [`find_mention`]).
pub fn has_everyone_mention(text: &str) -> bool {
    !find_mention(text, MENTION_EVERYONE).is_empty()
}

/// The one-line excerpt of a quoted message (whitespace collapsed, cut with "…").
pub fn reply_excerpt(caption: &str) -> String {
    let text = caption.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= REPLY_EXCERPT_MAX_CHARS {
        return text;
    }
    let mut cut: String = text.chars().take(REPLY_EXCERPT_MAX_CHARS - 1).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

/// Apply one reaction change to a message's list (what [`crate::ServerEvent::ReactionChanged`]
/// says). Idempotent; an emoji nobody has any more disappears. Returns whether anything changed.
pub fn apply_reaction(list: &mut Vec<Reaction>, emoji: &str, user: UserId, add: bool) -> bool {
    match list.iter().position(|r| r.emoji == emoji) {
        Some(i) if add => {
            if list[i].users.contains(&user) {
                return false;
            }
            list[i].users.push(user);
            true
        }
        Some(i) => {
            let before = list[i].users.len();
            list[i].users.retain(|u| *u != user);
            let changed = list[i].users.len() != before;
            if list[i].users.is_empty() {
                list.remove(i);
            }
            changed
        }
        None if add => {
            list.push(Reaction { emoji: emoji.to_string(), users: vec![user] });
            true
        }
        None => false,
    }
}

impl crate::Message {
    /// This message mentions `me` (by id, through `@todos`, or as the author a reply pinged).
    pub fn mentions_me(&self, me: UserId) -> bool {
        self.author.id != me && (self.mention_everyone || self.mentions.contains(&me))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DmMessage, DmMessageKind, Message, ServerEvent, User};

    #[test]
    fn single_emoji_only() {
        for ok in ["👍", "❤", "❤️", "👍🏽", "🇧🇷", "👨‍👩‍👧", "1️⃣", "✅", " 🎉 "] {
            assert!(clean_reaction_emoji(ok).is_some(), "{ok:?}");
        }
        assert_eq!(clean_reaction_emoji(" 🎉 ").as_deref(), Some("🎉"));
        let long = "👍🏽".repeat(6);
        for bad in ["", " ", "a", "ok", "👍👍", "👍a", "😀😀😀", "🇧🇷🇧", "🇧🇷🇧🇷", "\u{200B}👍", "👍\u{200D}", "1", "é", "🎉 🎉", long.as_str()] {
            assert!(clean_reaction_emoji(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn mentions_are_found_as_words_in_any_case() {
        let t = "oi @Tati, e @joao.silva! tati@x.com @TatiAna @todos.";
        assert_eq!(find_mention(t, "tati").iter().map(|r| &t[r.clone()]).collect::<Vec<_>>(), ["@Tati"]);
        assert_eq!(find_mention(t, "Joao.Silva").len(), 1);
        assert_eq!(find_mention(t, "joao").len(), 0, "a longer name follows");
        assert_eq!(find_mention(t, "tatiana").len(), 1);
        assert!(has_everyone_mention(t));
        assert!(!has_everyone_mention("@todosjuntos e email@todos"));
        assert!(has_everyone_mention("@TODOS"));
        // Names with spaces (display names) and accents.
        let t = "valeu @Tati Silva e @Conceição";
        assert_eq!(find_mention(t, "Tati Silva").len(), 1);
        assert_eq!(find_mention(t, "conceição").len(), 1);
        assert_eq!(&t[find_mention(t, "CONCEIÇÃO")[0].clone()], "@Conceição");
        assert!(find_mention("@@tati", "tati").is_empty());
        assert!(find_mention("@ta", "tati").is_empty());
        assert!(find_mention("x", "").is_empty());
    }

    #[test]
    fn excerpts_are_one_short_line() {
        assert_eq!(reply_excerpt("  oi \n tudo  bem? "), "oi tudo bem?");
        let long = "palavra ".repeat(40);
        let e = reply_excerpt(&long);
        assert_eq!(e.chars().count(), REPLY_EXCERPT_MAX_CHARS);
        assert!(e.ends_with('…'));
    }

    #[test]
    fn reactions_apply_idempotently() {
        let mut l = Vec::new();
        assert!(apply_reaction(&mut l, "👍", 1, true));
        assert!(!apply_reaction(&mut l, "👍", 1, true));
        assert!(apply_reaction(&mut l, "👍", 2, true));
        assert!(apply_reaction(&mut l, "🎉", 1, true));
        assert_eq!(l, vec![Reaction { emoji: "👍".into(), users: vec![1, 2] }, Reaction { emoji: "🎉".into(), users: vec![1] }]);
        assert!(apply_reaction(&mut l, "👍", 1, false));
        assert!(!apply_reaction(&mut l, "👍", 1, false));
        assert!(apply_reaction(&mut l, "🎉", 1, false));
        assert_eq!(l, vec![Reaction { emoji: "👍".into(), users: vec![2] }]);
        assert!(!apply_reaction(&mut l, "😀", 3, false));
    }

    fn msg() -> Message {
        Message {
            id: 9,
            channel_id: 2,
            author: User::new(3, "Tati"),
            content: "@Joao olha".into(),
            created_at_ms: 4,
            attachments: vec![],
            mentions: vec![],
            mention_everyone: false,
            reply: None,
            reactions: vec![],
        }
    }

    #[test]
    fn a_plain_message_is_byte_identical_to_v070() {
        assert_eq!(serde_json::to_string(&msg()).unwrap(), r#"{"id":9,"channel_id":2,"author":{"id":3,"username":"Tati"},"content":"@Joao olha","created_at_ms":4}"#);
        let line = DmMessage { id: 1, dm_id: 2, author: User::new(3, "a"), content: "oi".into(), created_at_ms: 4, kind: DmMessageKind::Text, call_secs: 0, attachments: vec![], reply: None, reactions: vec![], sealed: None, sealed_attachments: vec![] };
        assert_eq!(serde_json::to_string(&line).unwrap(), r#"{"id":1,"dm_id":2,"author":{"id":3,"username":"a"},"content":"oi","created_at_ms":4}"#);
        assert_eq!(
            serde_json::to_string(&crate::ClientEvent::SendMessage { channel_id: 1, content: "oi".into(), mentions: vec![], reply_to: None, reply_mention: false }).unwrap(),
            r#"{"type":"send_message","channel_id":1,"content":"oi"}"#
        );
        assert_eq!(serde_json::to_string(&crate::SendMessageRequest { content: "oi".into(), ..Default::default() }).unwrap(), r#"{"content":"oi"}"#);
        assert_eq!(serde_json::to_string(&crate::DmSendRequest { content: "oi".into(), ..Default::default() }).unwrap(), r#"{"content":"oi"}"#);
    }

    /// The shapes exactly as v0.7.0 had them (a subset is enough: unknown tags fail to parse).
    mod v070 {
        use crate::{Attachment, ChannelId, DmId, DmMessageId, MessageId, User, UserId};
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, Serialize, Deserialize)]
        pub struct Message {
            pub id: MessageId,
            pub channel_id: ChannelId,
            pub author: User,
            pub content: String,
            pub created_at_ms: i64,
            #[serde(default, skip_serializing_if = "Vec::is_empty")]
            pub attachments: Vec<Attachment>,
        }

        #[derive(Debug, Clone, Serialize, Deserialize)]
        pub struct DmMessage {
            pub id: DmMessageId,
            pub dm_id: DmId,
            pub author: User,
            pub content: String,
            pub created_at_ms: i64,
        }

        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        #[allow(dead_code)]
        pub enum ServerEvent {
            Ready { user: User, channels: Vec<crate::Channel>, online: Vec<User>, voice: Vec<crate::VoiceMember> },
            MessageCreated { message: Message },
            MessageDeleted { channel_id: ChannelId, message_id: MessageId },
            DmMessageCreated { message: DmMessage },
            UserOffline { user_id: UserId },
        }

        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        pub enum ClientEvent {
            SendMessage { channel_id: ChannelId, content: String },
        }
    }

    #[test]
    fn a_v070_app_reads_the_new_shapes_and_skips_the_new_events() {
        let mut m = msg();
        m.mentions = vec![5, 6];
        m.mention_everyone = true;
        m.reply = Some(ReplyRef { id: 3, author: Some(User::new(5, "Joao")), excerpt: "oi".into(), images: 1, deleted: false, sealed: false });
        m.reactions = vec![Reaction { emoji: "👍".into(), users: vec![5] }];
        let json = serde_json::to_string(&ServerEvent::MessageCreated { message: m.clone() }).unwrap();
        assert!(json.contains(r#""mentions":[5,6],"mention_everyone":true,"reply":{"id":3,"author":{"id":5,"username":"Joao"},"excerpt":"oi","images":1},"reactions":[{"emoji":"👍","users":[5]}]"#), "{json}");
        let v070::ServerEvent::MessageCreated { message } = serde_json::from_str::<v070::ServerEvent>(&json).unwrap() else { panic!() };
        assert_eq!(message.content, "@Joao olha", "an old app shows the text as typed");
        let ServerEvent::MessageCreated { message } = serde_json::from_str::<ServerEvent>(&json).unwrap() else { panic!() };
        assert_eq!(message, m);
        assert!(message.mentions_me(5) && message.mentions_me(7), "@todos reaches everybody");
        assert!(!message.mentions_me(3), "never my own message");
        // A DM reply reads as a plain line in the old app.
        let line = DmMessage { id: 1, dm_id: 2, author: User::new(3, "a"), content: "oi".into(), created_at_ms: 4, kind: DmMessageKind::Text, call_secs: 0, attachments: vec![], reply: Some(ReplyRef::gone(7)), reactions: vec![], sealed: None, sealed_attachments: vec![] };
        let json = serde_json::to_string(&ServerEvent::DmMessageCreated { message: line }).unwrap();
        assert!(json.contains(r#""reply":{"id":7,"deleted":true}"#), "{json}");
        assert!(matches!(serde_json::from_str::<v070::ServerEvent>(&json), Ok(v070::ServerEvent::DmMessageCreated { .. })));
        // The new events are unknown to it (it logs and skips them); a new app reads them.
        for ev in [
            ServerEvent::ReactionChanged { channel_id: 1, message_id: 2, emoji: "👍".into(), user_id: 3, added: true },
            ServerEvent::DmReactionChanged { dm_id: 1, message_id: 2, emoji: "👍".into(), user_id: 3, added: false },
        ] {
            let json = serde_json::to_string(&ev).unwrap();
            assert!(serde_json::from_str::<v070::ServerEvent>(&json).is_err(), "{json}");
            assert!(serde_json::from_str::<ServerEvent>(&json).is_ok(), "{json}");
        }
        assert_eq!(
            serde_json::to_string(&ServerEvent::ReactionChanged { channel_id: 1, message_id: 2, emoji: "🎉".into(), user_id: 3, added: true }).unwrap(),
            r#"{"type":"reaction_changed","channel_id":1,"message_id":2,"emoji":"🎉","user_id":3,"added":true}"#
        );
        // `Ready` with the feature flags still parses in the old shape.
        let ready = ServerEvent::Ready { user: User::new(1, "a"), channels: vec![], online: vec![], voice: vec![], account: None, chat: Some(ChatFeatures::ALL), server_id: None, stream_previews: false, e2ee: false };
        let json = serde_json::to_string(&ready).unwrap();
        assert!(json.contains(r#""chat":{"mentions":true,"replies":true,"reactions":true,"private_channels":true}"#), "{json}");
        assert!(matches!(serde_json::from_str::<v070::ServerEvent>(&json), Ok(v070::ServerEvent::Ready { .. })));
    }

    #[test]
    fn old_shapes_still_deserialize() {
        // What a v0.7.0 hub sends: no mentions, no reply, no reactions, no feature flags.
        let m: Message = serde_json::from_str(r#"{"id":9,"channel_id":2,"author":{"id":3,"username":"Tati"},"content":"@Joao olha","created_at_ms":4}"#).unwrap();
        assert_eq!(m, msg());
        let ready: ServerEvent = serde_json::from_str(r#"{"type":"ready","user":{"id":1,"username":"a"},"channels":[],"online":[],"voice":[]}"#).unwrap();
        assert!(matches!(ready, ServerEvent::Ready { chat: None, .. }), "an old hub: the app hides replies and reactions");
        let line: DmMessage = serde_json::from_str(r#"{"id":1,"dm_id":2,"author":{"id":3,"username":"a"},"content":"oi","created_at_ms":4}"#).unwrap();
        assert!(line.reply.is_none() && line.reactions.is_empty());
        // What a v0.7.0 app sends: no mentions, no reply.
        let old = serde_json::to_string(&v070::ClientEvent::SendMessage { channel_id: 1, content: "@todos oi".into() }).unwrap();
        let ev: crate::ClientEvent = serde_json::from_str(&old).unwrap();
        assert!(matches!(ev, crate::ClientEvent::SendMessage { mentions, reply_to: None, reply_mention: false, .. } if mentions.is_empty()));
        let req: crate::SendMessageRequest = serde_json::from_str(r#"{"content":"oi","attachments":[1]}"#).unwrap();
        assert!(req.mentions.is_empty() && req.reply_to.is_none() && !req.reply_mention);
        // A partial feature list from a newer hub reads with the rest off.
        let f: ChatFeatures = serde_json::from_str(r#"{"reactions":true,"polls":true}"#).unwrap();
        assert_eq!(f, ChatFeatures { reactions: true, ..Default::default() });
        // A quote of a deleted message, as small as it gets.
        assert_eq!(serde_json::from_str::<ReplyRef>(r#"{"id":4}"#).unwrap(), ReplyRef { id: 4, ..Default::default() });
    }
}
