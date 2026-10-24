//! Go Live hover previews (v1.1, no protocol bump): a small still picture of what someone is
//! streaming and what it is (the app's or game's name and icon), shown when the pointer rests on
//! their live anywhere in the app, also for people who are not in that voice room.
//!
//! - The streamer's app grabs a frame from the capture it already runs, makes a ~320 px JPEG of it
//!   (10-25 KB) and sends it with `PUT /api/me/stream-preview` ([`StreamPreviewUpload`]) shortly
//!   after going live, then every ~15-20 s and only when the picture changed. The app's name and
//!   icon go with the first upload of each source.
//! - The hub keeps only the latest one per streamer, in memory (never in the database), and drops
//!   it when the live ends, becomes sound only, the person leaves or moves, or asks with
//!   `DELETE /api/me/stream-preview` (the streamer turned the previews off). Uploads are capped
//!   ([`STREAM_PREVIEW_MAX_JPEG`], [`STREAM_PREVIEW_MAX_ICON`]) and rate limited.
//! - A viewer asks with `GET /api/users/{id}/stream-preview` ([`StreamPreview`]) after the pointer
//!   rested ~250 ms, and keeps the answer a few seconds. Only people who may see that voice
//!   channel get it (private channels, v0.9): anybody else gets `404` as for a stream that does
//!   not exist.
//! - Compatibility: a hub with this sets [`crate::ServerEvent::Ready`]'s `stream_previews`; an app
//!   uploads and asks only then (an older hub never sees the routes). Older apps ignore the flag.
//!   Nothing is sent over the WebSocket.

use serde::{Deserialize, Serialize};

use crate::{ChannelId, UserId};

/// The largest preview picture the hub keeps (bytes of JPEG).
pub const STREAM_PREVIEW_MAX_JPEG: usize = 64 * 1024;
/// The largest app icon (bytes of PNG; the app sends 32 x 32).
pub const STREAM_PREVIEW_MAX_ICON: usize = 16 * 1024;
/// Largest width or height of a preview picture (the app sends about 320 x 180).
pub const STREAM_PREVIEW_MAX_SIDE: u32 = 640;
/// Largest width or height of an app icon.
pub const STREAM_ICON_MAX_SIDE: u32 = 64;
/// Most characters of an app's name.
pub const STREAM_APP_NAME_MAX: usize = 64;
/// v1.1 (E1): `403` of an upload with a picture from a live in an end-to-end encrypted room (only
/// the app's name and icon may go there).
pub const ERR_STREAM_PREVIEW_E2EE: &str = "no pictures from an end-to-end encrypted room";
/// The largest upload body (base64 of both pictures plus the name, with room to spare).
pub const STREAM_PREVIEW_MAX_BODY: usize = (STREAM_PREVIEW_MAX_JPEG + STREAM_PREVIEW_MAX_ICON) * 4 / 3 + 4096;

/// What is being streamed: the app's name (from the program itself, never a window title) or
/// "Tela inteira", and whether it looks like a game.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamApp {
    /// "Google Chrome", "Counter-Strike 2"... (at most [`STREAM_APP_NAME_MAX`] characters).
    pub name: String,
    /// The program is a game (cheap local guesses, see `vox_capture::looks_like_game`): the card
    /// says "Jogando <name>" with a controller badge.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_game: bool,
    /// A whole screen is shared, not a window.
    #[serde(default, skip_serializing_if = "is_false")]
    pub screen: bool,
}

impl StreamApp {
    /// The name cut to [`STREAM_APP_NAME_MAX`] characters, control characters removed.
    pub fn clean_name(name: &str) -> String {
        name.chars().filter(|c| !c.is_control()).take(STREAM_APP_NAME_MAX).collect::<String>().trim().to_string()
    }
}

/// `PUT /api/me/stream-preview` (authenticated, while streaming). `200` [`crate::OkResponse`];
/// `400` bad picture or name, `403` not streaming a picture, `413` too big, `429` too soon after
/// the previous upload. A hub without hover previews answers `404`. `DELETE` on the same path
/// (authenticated) drops my preview at once: `200` [`crate::OkResponse`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamPreviewUpload {
    /// The picture: a baseline JPEG, base64 (standard alphabet, padded). Absent = keep the
    /// previous picture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jpeg: Option<String>,
    /// What is streamed. Present = replaces the app and its icon (with `icon_png`, or none);
    /// absent = keep both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<StreamApp>,
    /// The app's icon: a PNG of at most [`STREAM_ICON_MAX_SIDE`] pixels, base64. Only read with
    /// `app`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_png: Option<String>,
}

/// `GET /api/users/{id}/stream-preview[?icon=<hash>]` (authenticated): `id`'s live as the hub
/// last heard of it. `404` when they are not streaming, nothing was sent yet, or the caller may not
/// see their voice channel. `icon` = the [`StreamPreview::icon_hash`] the caller already has: the
/// icon's bytes are then left out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamPreview {
    pub user_id: UserId,
    pub channel_id: ChannelId,
    /// The picture (JPEG, base64), when one was sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jpeg: Option<String>,
    /// Milliseconds since the picture arrived at the hub.
    #[serde(default)]
    pub age_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<StreamApp>,
    /// A short hash of the icon (the cache key), when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_hash: Option<String>,
    /// The icon (PNG, base64), unless the caller said it has `icon_hash` already.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_png: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ServerEvent, User};

    /// `Ready` exactly as v1.0.2 has it (what an older app parses).
    mod v102 {
        use crate::{Account, Channel, ChatFeatures, User, VoiceMember};
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        pub enum ServerEvent {
            Ready {
                user: User,
                channels: Vec<Channel>,
                online: Vec<User>,
                voice: Vec<VoiceMember>,
                #[serde(default, skip_serializing_if = "Option::is_none")]
                account: Option<Account>,
                #[serde(default, skip_serializing_if = "Option::is_none")]
                chat: Option<ChatFeatures>,
                #[serde(default, skip_serializing_if = "Option::is_none")]
                server_id: Option<String>,
            },
        }
    }

    fn ready(stream_previews: bool) -> ServerEvent {
        ServerEvent::Ready { user: User::new(1, "a"), channels: vec![], online: vec![], voice: vec![], account: None, chat: None, server_id: None, stream_previews, e2ee: false }
    }

    #[test]
    fn the_capability_is_additive_both_ways() {
        // A new hub's snapshot reads in a v1.0.2 app (which ignores the flag).
        let json = serde_json::to_string(&ready(true)).unwrap();
        assert!(json.contains(r#""stream_previews":true"#), "{json}");
        assert!(matches!(serde_json::from_str::<v102::ServerEvent>(&json), Ok(v102::ServerEvent::Ready { .. })));
        // Off: byte-identical to v1.0.2's.
        let old = v102::ServerEvent::Ready { user: User::new(1, "a"), channels: vec![], online: vec![], voice: vec![], account: None, chat: None, server_id: None };
        assert_eq!(serde_json::to_string(&ready(false)).unwrap(), serde_json::to_string(&old).unwrap());
        // A v1.0.2 hub's snapshot: no previews.
        let parsed: ServerEvent = serde_json::from_str(&serde_json::to_string(&old).unwrap()).unwrap();
        assert!(matches!(parsed, ServerEvent::Ready { stream_previews: false, .. }));
    }

    #[test]
    fn uploads_and_answers_leave_out_what_is_absent() {
        let frame_only = StreamPreviewUpload { jpeg: Some("AAAA".into()), ..Default::default() };
        assert_eq!(serde_json::to_string(&frame_only).unwrap(), r#"{"jpeg":"AAAA"}"#);
        let app = StreamApp { name: "Counter-Strike 2".into(), is_game: true, screen: false };
        let json = serde_json::to_string(&StreamPreviewUpload { jpeg: None, app: Some(app.clone()), icon_png: Some("iVBO".into()) }).unwrap();
        assert_eq!(json, r#"{"app":{"name":"Counter-Strike 2","is_game":true},"icon_png":"iVBO"}"#);
        assert_eq!(serde_json::from_str::<StreamPreviewUpload>("{}").unwrap(), StreamPreviewUpload::default());
        let a: StreamPreview = serde_json::from_str(r#"{"user_id":3,"channel_id":4}"#).unwrap();
        assert_eq!((a.user_id, a.channel_id, a.jpeg, a.app), (3, 4, None, None));
        let screen = StreamApp { name: "Tela inteira".into(), is_game: false, screen: true };
        assert_eq!(serde_json::to_string(&screen).unwrap(), r#"{"name":"Tela inteira","screen":true}"#);
    }

    #[test]
    fn names_are_cleaned_and_capped() {
        assert_eq!(StreamApp::clean_name("  Google\u{7} Chrome \n"), "Google Chrome");
        assert_eq!(StreamApp::clean_name(&"x".repeat(200)).chars().count(), STREAM_APP_NAME_MAX);
        assert!(STREAM_PREVIEW_MAX_BODY > STREAM_PREVIEW_MAX_JPEG * 4 / 3 + STREAM_PREVIEW_MAX_ICON * 4 / 3);
    }
}
