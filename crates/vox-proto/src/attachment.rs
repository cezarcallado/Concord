//! Image attachments ("Imagens no chat"), added after v0.6.0 with no protocol bump: new
//! endpoints, one new type and new optional fields; nothing existing changes meaning.
//!
//! The *sending app* does the heavy lifting (decode, EXIF orientation, resize, WebP / JPEG
//! re-encode without metadata, thumbnail), so the host's server only ever receives small files.
//! The *hub* never trusts it: it sniffs the format, strips every metadata block from the
//! container, decodes the whole file under dimension / allocation caps (every frame of an
//! animation), checks sizes and the per-person daily and total storage quotas, and stores each
//! distinct file once (content addressed).
//!
//! HTTP (all authenticated):
//! - `GET  /api/attachments/limits` -> [`AttachmentLimits`]. A hub that predates attachments
//!   answers `404`: the app then hides the 📎 button (and paste / drop of images).
//! - `POST /api/attachments?thumb_len=<n>` body = the image file followed by `n` bytes of its
//!   thumbnail (`n` = 0: none; the hub then makes one when it is needed) -> [`Attachment`]. The
//!   new attachment is *pending*: only its uploader can see it or put it on a message, for
//!   [`ATTACHMENT_PENDING_SECS`]; unused ones are deleted. Errors: `400` not a supported image /
//!   damaged / dimensions, `413` too big ([`ATTACHMENT_MAX_BYTES`], animated
//!   [`ATTACHMENT_MAX_ANIMATED_BYTES`]), `429` [`ERR_ATTACHMENT_DAILY`] (the per-person daily
//!   allowance) or too many pending, `507` [`ERR_ATTACHMENT_STORAGE_FULL`] (the server's total
//!   storage cap).
//! - `GET  /api/attachments/{id}` and `GET /api/attachments/{id}/thumb`: the stored file (`ETag`
//!   = its hash, `Cache-Control: private, max-age=31536000, immutable`). Who may fetch: anybody
//!   logged in for a channel message's image; **only the two participants** for a DM's image
//!   (everybody else, Admins and the owner included, gets `404`, exactly like an id that does not
//!   exist); only the uploader while it is pending.
//! - `POST /api/channels/{id}/messages` [`SendMessageRequest`] -> [`crate::Message`]: a channel
//!   message with up to [`ATTACHMENT_MAX_PER_MESSAGE`] of my pending attachments (the caption may
//!   be empty then). The WebSocket `SendMessage` keeps working for plain text.
//! - DMs: [`crate::DmSendRequest::attachments`].
//!
//! Compatibility: a message with images carries them in `attachments` (absent from the JSON when
//! there are none, so every other message is byte-identical to before) **and** its `content`
//! ends with a short marker line (`[imagem]`, `[2 imagens]`, see [`attachment_marker`]): an
//! older app ignores the unknown field and shows the caption plus the marker. A new app strips the
//! marker again with [`strip_attachment_marker`] (`Message::caption`, `DmMessage::caption`).
//! Deleting a message deletes its images (a file shared by several messages stays until the last
//! one goes); moderators deleting a channel message remove its images too.
//!
//! **Sealed images (v1.1, E2).** `POST /api/attachments?sealed=1&thumb_len=<n>`: the body is the
//! ciphertext of a picture (and `n` bytes of its thumbnail's), made by the `vox-e2ee` crate with a
//! key only the DM's devices get. The hub cannot check it: it stores it as
//! `application/octet-stream` with no sizes (`width` / `height` 0, [`Attachment::sealed`]), still
//! under every quota (daily allowance, pending count, total storage). A sealed attachment can only
//! go on a sealed DM line ([`ERR_SEALED_ONLY_DM`] otherwise, and a sealed line takes only sealed
//! images). [`AttachmentLimits::sealed`] says the hub knows all this.

use serde::{Deserialize, Serialize};

pub type AttachmentId = i64;

/// Images on one message.
pub const ATTACHMENT_MAX_PER_MESSAGE: usize = 4;
/// Biggest still image the hub accepts (after the app's compression; normally a few hundred KB).
pub const ATTACHMENT_MAX_BYTES: usize = 3 * 1024 * 1024;
/// Biggest animated image (GIF / animated WebP).
pub const ATTACHMENT_MAX_ANIMATED_BYTES: usize = 10 * 1024 * 1024;
/// Biggest thumbnail.
pub const ATTACHMENT_MAX_THUMB_BYTES: usize = 256 * 1024;
/// Longest side of a stored image ("Original" quality goes up to this).
pub const ATTACHMENT_MAX_SIDE: u32 = 4096;
/// Thumbnails fit in a square of this many pixels.
pub const ATTACHMENT_THUMB_BOX: u32 = 400;
/// A pending (uploaded, not yet sent) attachment lives this long.
pub const ATTACHMENT_PENDING_SECS: u64 = 3600;
/// At most this many pending attachments per person.
pub const ATTACHMENT_MAX_PENDING: usize = 16;
/// The request body limit of the upload (image + thumbnail + slack).
pub const ATTACHMENT_MAX_UPLOAD_BODY: usize = ATTACHMENT_MAX_ANIMATED_BYTES + ATTACHMENT_MAX_THUMB_BYTES + 4096;

/// `429` answer: the per-person daily upload allowance is used up.
pub const ERR_ATTACHMENT_DAILY: &str = "daily upload limit reached";
/// `507` answer: the server's total image storage is full.
pub const ERR_ATTACHMENT_STORAGE_FULL: &str = "server image storage is full";
/// `400` answer: a sealed image on a channel message or a readable DM line (or a readable image on
/// a sealed line).
pub const ERR_SEALED_ONLY_DM: &str = "sealed images only go on encrypted direct messages";
/// What sealing adds to a picture or a thumbnail (the AEAD tag).
pub const ATTACHMENT_SEALED_OVERHEAD: usize = 16;
/// The `mime` of a sealed attachment on the hub.
pub const ATTACHMENT_SEALED_MIME: &str = "application/octet-stream";

/// One image on a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: AttachmentId,
    /// `image/webp`, `image/jpeg`, `image/png` or `image/gif`.
    pub mime: String,
    pub width: u32,
    pub height: u32,
    /// Bytes of the stored file.
    pub size: u64,
    /// Size of `GET /api/attachments/{id}/thumb`, so the chat can reserve the space before it
    /// loads (the same as `width` / `height` when the image is its own thumbnail).
    #[serde(default)]
    pub thumb_w: u32,
    #[serde(default)]
    pub thumb_h: u32,
    /// GIF / animated WebP (the thumbnail is the first frame).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub animated: bool,
    /// Short hash of the stored file (a cache key; the same file always has the same one).
    #[serde(default)]
    pub hash: String,
    /// E2: the stored file is ciphertext (`mime` is [`ATTACHMENT_SEALED_MIME`], sizes 0 on the
    /// hub; the app fills the real ones from the line's plaintext).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sealed: bool,
}

/// `GET /api/attachments/limits`: what this hub accepts, and how much of my daily allowance is
/// left. Its mere existence (not `404`) says the hub knows attachments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentLimits {
    pub max_per_message: u32,
    pub max_bytes: u64,
    pub max_animated_bytes: u64,
    pub max_side: u32,
    /// Bytes per person per day (UTC).
    pub daily_bytes: u64,
    pub daily_bytes_left: u64,
    /// The total storage cap is reached (uploads are refused until the owner frees space).
    #[serde(default)]
    pub storage_full: bool,
    /// E2: the hub takes sealed uploads (`?sealed=1`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sealed: bool,
}

/// `POST /api/channels/{id}/messages`
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendMessageRequest {
    #[serde(default)]
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentId>,
    /// Mentions and replies (v0.8, see [`crate::chat`]): absent when unused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<crate::UserId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<crate::MessageId>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reply_mention: bool,
}

/// The line an older app shows for `n` images: `[imagem]`, `[2 imagens]`.
pub fn attachment_marker(n: usize) -> String {
    if n <= 1 { "[imagem]".to_string() } else { format!("[{n} imagens]") }
}

/// The stored / sent `content` of a message with `n` images: the caption, then the marker line.
pub fn with_attachment_marker(caption: &str, n: usize) -> String {
    if n == 0 {
        return caption.to_string();
    }
    let marker = attachment_marker(n);
    if caption.is_empty() { marker } else { format!("{caption}\n{marker}") }
}

/// The caption of a message with `n` images (the marker line taken off again). Content that does
/// not end with the marker is returned as it is.
pub fn strip_attachment_marker(content: &str, n: usize) -> &str {
    if n == 0 {
        return content;
    }
    let marker = attachment_marker(n);
    match content.strip_suffix(marker.as_str()) {
        Some("") => "",
        Some(rest) => rest.strip_suffix('\n').unwrap_or(content),
        None => content,
    }
}

impl crate::Message {
    /// What to show as the text of this message (without the `[imagem]` marker of its images).
    pub fn caption(&self) -> &str {
        strip_attachment_marker(&self.content, self.attachments.len())
    }
}

impl crate::DmMessage {
    /// What to show as the text of this line (without the `[imagem]` marker of its images).
    pub fn caption(&self) -> &str {
        strip_attachment_marker(&self.content, self.attachments.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, ServerEvent, User};

    fn att(id: i64) -> Attachment {
        Attachment { id, mime: "image/webp".into(), width: 1920, height: 1080, size: 312_000, thumb_w: 400, thumb_h: 225, animated: false, hash: "ab12".into(), sealed: false }
    }

    #[test]
    fn markers_round_trip() {
        assert_eq!(attachment_marker(1), "[imagem]");
        assert_eq!(attachment_marker(3), "[3 imagens]");
        assert_eq!(with_attachment_marker("olha", 1), "olha\n[imagem]");
        assert_eq!(with_attachment_marker("", 2), "[2 imagens]");
        assert_eq!(with_attachment_marker("só texto", 0), "só texto");
        for (caption, n) in [("olha", 1), ("", 2), ("[imagem]", 1), ("duas\nlinhas", 4)] {
            assert_eq!(strip_attachment_marker(&with_attachment_marker(caption, n), n), caption);
        }
        // Not our marker: left alone.
        assert_eq!(strip_attachment_marker("oi", 1), "oi");
        assert_eq!(strip_attachment_marker("oi\n[2 imagens]", 1), "oi\n[2 imagens]");
        assert_eq!(strip_attachment_marker("x[imagem]", 1), "x[imagem]");
    }

    #[test]
    fn a_message_without_images_is_byte_identical_to_before() {
        let m = Message { id: 1, channel_id: 2, author: User::new(3, "a"), content: "oi".into(), created_at_ms: 4, attachments: vec![], mentions: vec![], mention_everyone: false, reply: None, reactions: vec![] };
        assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"id":1,"channel_id":2,"author":{"id":3,"username":"a"},"content":"oi","created_at_ms":4}"#);
    }

    /// `Message` exactly as v0.6.0 had it.
    #[derive(Debug, Deserialize)]
    struct V060Message {
        content: String,
    }

    #[derive(Debug, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum V060ServerEvent {
        MessageCreated { message: V060Message },
    }

    #[test]
    fn an_old_app_reads_the_caption_and_the_marker() {
        let m = Message { id: 1, channel_id: 2, author: User::new(3, "a"), content: with_attachment_marker("olha isso", 2), created_at_ms: 4, attachments: vec![att(7), att(8)], mentions: vec![], mention_everyone: false, reply: None, reactions: vec![] };
        let json = serde_json::to_string(&ServerEvent::MessageCreated { message: m.clone() }).unwrap();
        assert!(json.contains("\"attachments\":["));
        let V060ServerEvent::MessageCreated { message } = serde_json::from_str::<V060ServerEvent>(&json).unwrap();
        assert_eq!(message.content, "olha isso\n[2 imagens]");
        // A new app gets the images and the bare caption.
        let ServerEvent::MessageCreated { message } = serde_json::from_str::<ServerEvent>(&json).unwrap() else { panic!() };
        assert_eq!(message.attachments.len(), 2);
        assert_eq!(message.caption(), "olha isso");
        assert_eq!(message, m);
    }

    #[test]
    fn old_json_parses_without_images() {
        let m: Message = serde_json::from_str(r#"{"id":1,"channel_id":2,"author":{"id":3,"username":"a"},"content":"[imagem]","created_at_ms":4}"#).unwrap();
        assert!(m.attachments.is_empty());
        assert_eq!(m.caption(), "[imagem]", "without attachments the text is shown as it is");
        let a: Attachment = serde_json::from_str(r#"{"id":1,"mime":"image/jpeg","width":10,"height":20,"size":30}"#).unwrap();
        assert_eq!((a.thumb_w, a.animated, a.hash.as_str()), (0, false, ""));
    }

    #[test]
    fn wire_shapes() {
        let a = att(5);
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            r#"{"id":5,"mime":"image/webp","width":1920,"height":1080,"size":312000,"thumb_w":400,"thumb_h":225,"hash":"ab12"}"#
        );
        let gif = Attachment { animated: true, ..att(6) };
        assert!(serde_json::to_string(&gif).unwrap().contains("\"animated\":true"));
        assert_eq!(serde_json::to_string(&SendMessageRequest { content: "oi".into(), ..Default::default() }).unwrap(), r#"{"content":"oi"}"#);
        assert_eq!(serde_json::to_string(&SendMessageRequest { content: String::new(), attachments: vec![1, 2], ..Default::default() }).unwrap(), r#"{"content":"","attachments":[1,2]}"#);
        let req: crate::DmSendRequest = serde_json::from_str(r#"{"content":"oi"}"#).unwrap();
        assert!(req.attachments.is_empty(), "an old app's DM request has no images");
        assert_eq!(serde_json::to_string(&crate::DmSendRequest { content: "oi".into(), ..Default::default() }).unwrap(), r#"{"content":"oi"}"#);
    }
}
