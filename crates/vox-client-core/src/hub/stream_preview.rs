//! Go Live hover-preview calls of [`HubClient`] (the contract is in `vox_proto::stream_preview`).
//! Only made to a hub whose `Ready` said `stream_previews` (an older hub never sees them).

use base64::Engine;
use vox_proto::{StreamApp, StreamPreview, StreamPreviewUpload, STREAM_PREVIEW_MAX_ICON, STREAM_PREVIEW_MAX_JPEG};

use super::*;

/// The largest answer read (both pictures in base64 and the rest, with room to spare).
pub const MAX_STREAM_PREVIEW_ANSWER: usize = (STREAM_PREVIEW_MAX_JPEG + STREAM_PREVIEW_MAX_ICON) * 2;

/// A preview as the app uses it (pictures decoded from base64, still compressed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamPreviewData {
    pub user_id: UserId,
    pub channel_id: ChannelId,
    /// JPEG bytes.
    pub jpeg: Option<Vec<u8>>,
    pub age_ms: u64,
    pub app: Option<StreamApp>,
    pub icon_hash: Option<String>,
    /// PNG bytes (absent when the caller said it has `icon_hash`).
    pub icon_png: Option<Vec<u8>>,
}

/// How the hub answered a hover lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamPreviewReply {
    Ready(StreamPreviewData),
    /// `404`: not live, nothing sent yet, or not a room I may see.
    NotFound,
    /// `429`: too many lookups just now.
    Busy,
    Failed(String),
}

/// How the hub answered an upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadReply {
    Stored,
    /// `429`: too soon after the previous one.
    TooSoon,
    /// `403`: the hub does not see me live (yet, or any more).
    NotLive,
    /// `404`: the hub has no such route after all.
    Unsupported,
    Refused(String),
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

fn unb64(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(s.as_bytes()).ok()
}

impl HubClient {
    /// `PUT /api/me/stream-preview`: a new picture and/or (with `app`) what is streamed and its
    /// icon.
    pub async fn upload_stream_preview(&self, jpeg: Option<&[u8]>, app: Option<(&StreamApp, Option<&[u8]>)>) -> UploadReply {
        let body = StreamPreviewUpload { jpeg: jpeg.map(b64), app: app.map(|(a, _)| a.clone()), icon_png: app.and_then(|(_, i)| i).map(b64) };
        let req = match self.url("api/me/stream-preview").and_then(|u| self.auth(self.http.put(u))) {
            Ok(r) => r,
            Err(e) => return UploadReply::Refused(format!("{e:#}")),
        };
        match req.json(&body).send().await {
            Err(e) => UploadReply::Refused(format!("stream preview upload: {e}")),
            Ok(resp) => match resp.status().as_u16() {
                s if (200..300).contains(&s) => UploadReply::Stored,
                429 => UploadReply::TooSoon,
                403 => UploadReply::NotLive,
                404 | 405 => UploadReply::Unsupported,
                s => UploadReply::Refused(format!("stream preview upload: HTTP {s}: {}", resp.text().await.unwrap_or_default())),
            },
        }
    }

    /// `DELETE /api/me/stream-preview`: drop my picture now (hover previews turned off mid-live).
    pub async fn delete_stream_preview(&self) -> Result<()> {
        let req = self.auth(self.http.delete(self.url("api/me/stream-preview")?))?;
        let resp = req.send().await.context("stream preview delete")?;
        if !resp.status().is_success() {
            bail!("stream preview delete: HTTP {}", resp.status());
        }
        Ok(())
    }

    /// `GET /api/users/{id}/stream-preview[?icon=]`: `have_icon` = the icon hash already cached.
    pub async fn stream_preview(&self, user: UserId, have_icon: Option<&str>) -> StreamPreviewReply {
        let url = match self.url(&format!("api/users/{user}/stream-preview")) {
            Ok(mut u) => {
                if let Some(h) = have_icon {
                    u.query_pairs_mut().append_pair("icon", h);
                }
                u
            }
            Err(e) => return StreamPreviewReply::Failed(format!("{e:#}")),
        };
        let req = match self.auth(self.http.get(url)) {
            Ok(r) => r,
            Err(e) => return StreamPreviewReply::Failed(format!("{e:#}")),
        };
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return StreamPreviewReply::Failed(format!("stream preview request: {e}")),
        };
        match resp.status().as_u16() {
            404 | 403 => StreamPreviewReply::NotFound,
            429 => StreamPreviewReply::Busy,
            s if (200..300).contains(&s) => {
                if resp.content_length().is_some_and(|n| n > MAX_STREAM_PREVIEW_ANSWER as u64) {
                    return StreamPreviewReply::Failed("stream preview too large".into());
                }
                let bytes = match resp.bytes().await {
                    Ok(b) if b.len() <= MAX_STREAM_PREVIEW_ANSWER => b,
                    Ok(_) => return StreamPreviewReply::Failed("stream preview too large".into()),
                    Err(e) => return StreamPreviewReply::Failed(format!("stream preview body: {e}")),
                };
                match serde_json::from_slice::<StreamPreview>(&bytes) {
                    Ok(p) => StreamPreviewReply::Ready(StreamPreviewData {
                        user_id: p.user_id,
                        channel_id: p.channel_id,
                        jpeg: p.jpeg.as_deref().and_then(unb64).filter(|j| j.len() <= STREAM_PREVIEW_MAX_JPEG),
                        age_ms: p.age_ms,
                        app: p.app,
                        icon_hash: p.icon_hash,
                        icon_png: p.icon_png.as_deref().and_then(unb64).filter(|i| i.len() <= STREAM_PREVIEW_MAX_ICON),
                    }),
                    Err(e) => StreamPreviewReply::Failed(format!("stream preview: invalid response body: {e}")),
                }
            }
            s => StreamPreviewReply::Failed(format!("stream preview: HTTP {s}")),
        }
    }
}
