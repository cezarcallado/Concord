//! Chat-image calls of [`HubClient`] (the contract is in `vox_proto::attachment`).
//!
//! Failures carry the HTTP status as [`HttpStatusError`] (`400` refused, `413` too big, `429`
//! daily allowance, `507` server storage full), so the app can say what went wrong.

use std::time::Duration;

use anyhow::{Context, Result};
use vox_proto::{Attachment, AttachmentId, AttachmentLimits, ChannelId, Message as ChatMessage, SendMessageRequest, ATTACHMENT_MAX_ANIMATED_BYTES};

use super::*;

/// The biggest file the hub stores (an animation), with slack; anything bigger is not loaded.
pub const MAX_ATTACHMENT_DOWNLOAD_BYTES: usize = ATTACHMENT_MAX_ANIMATED_BYTES + 64 * 1024;

/// How the hub answered `GET /api/attachments/limits`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentsReply {
    Ready(AttachmentLimits),
    /// `404`: a hub older than chat images. The app hides the 📎 button.
    Unsupported,
    Failed(String),
}

async fn checked(what: &str, resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let message = serde_json::from_str::<ApiError>(&body).map(|e| e.error).unwrap_or(body);
    Err(anyhow::Error::new(HttpStatusError { status: status.as_u16(), message }).context(format!("{what} failed")))
}

impl HubClient {
    /// `GET /api/attachments/limits`
    pub async fn attachment_limits(&self) -> AttachmentsReply {
        let req = match self.url("api/attachments/limits").and_then(|u| self.auth(self.http.get(u))) {
            Ok(r) => r,
            Err(e) => return AttachmentsReply::Failed(format!("{e:#}")),
        };
        match req.send().await {
            Err(e) => AttachmentsReply::Failed(format!("attachment limits request: {e}")),
            Ok(resp) => match resp.status().as_u16() {
                404 | 405 => AttachmentsReply::Unsupported,
                s if (200..300).contains(&s) => match resp.json::<AttachmentLimits>().await {
                    Ok(l) => AttachmentsReply::Ready(l),
                    Err(e) => AttachmentsReply::Failed(format!("attachment limits: invalid response body: {e}")),
                },
                s => AttachmentsReply::Failed(format!("attachment limits: HTTP {s}")),
            },
        }
    }

    /// `POST /api/attachments?thumb_len=`: a compressed picture and its thumbnail. The answer is a
    /// pending attachment: put it on a message within the hour.
    pub async fn upload_attachment(&self, image: &[u8], thumb: Option<&[u8]>) -> Result<Attachment> {
        self.upload_attachment_as(image, thumb, false).await
    }

    /// E2: `POST /api/attachments?sealed=1&thumb_len=`: the ciphertext of a picture (and of its
    /// thumbnail), sealed by `vox_e2ee::dm::seal_blob`. Only a sealed DM line can carry it.
    pub async fn upload_sealed_attachment(&self, image: &[u8], thumb: Option<&[u8]>) -> Result<Attachment> {
        self.upload_attachment_as(image, thumb, true).await
    }

    async fn upload_attachment_as(&self, image: &[u8], thumb: Option<&[u8]>, sealed: bool) -> Result<Attachment> {
        let n = thumb.map_or(0, <[u8]>::len);
        let mut body = Vec::with_capacity(image.len() + n);
        body.extend_from_slice(image);
        if let Some(t) = thumb {
            body.extend_from_slice(t);
        }
        let mut url = self.url("api/attachments")?;
        if sealed {
            url.query_pairs_mut().append_pair("sealed", "1");
        }
        url.query_pairs_mut().append_pair("thumb_len", &n.to_string());
        let resp = self.auth(self.http.post(url))?.timeout(Duration::from_secs(180)).body(body).send().await.context("image upload request")?;
        checked("image upload", resp).await?.json::<Attachment>().await.context("image upload: invalid response body")
    }

    /// `POST /api/channels/{id}/messages`: a channel message with images (or plain text).
    pub async fn send_channel_message(&self, channel: ChannelId, content: &str, attachments: &[AttachmentId]) -> Result<ChatMessage> {
        let body = SendMessageRequest { content: content.to_string(), attachments: attachments.to_vec(), ..Default::default() };
        self.send_channel_request(channel, &body).await
    }

    /// `POST /api/channels/{id}/messages` with everything a message can carry (images, mentions,
    /// a reply).
    pub async fn send_channel_request(&self, channel: ChannelId, body: &SendMessageRequest) -> Result<ChatMessage> {
        let resp = self.auth(self.http.post(self.url(&format!("api/channels/{channel}/messages"))?))?.json(body).send().await.context("send message request")?;
        checked("send message", resp).await?.json::<ChatMessage>().await.context("send message: invalid response body")
    }

    /// `GET /api/attachments/{id}` (or its `/thumb`): the stored file.
    pub async fn attachment_file(&self, id: AttachmentId, thumb: bool) -> Result<Vec<u8>> {
        let path = if thumb { format!("api/attachments/{id}/thumb") } else { format!("api/attachments/{id}") };
        let resp = self.auth(self.http.get(self.url(&path)?))?.timeout(Duration::from_secs(120)).send().await.context("image download request")?;
        let resp = checked("image download", resp).await?;
        if resp.content_length().is_some_and(|n| n > MAX_ATTACHMENT_DOWNLOAD_BYTES as u64) {
            anyhow::bail!("image download: larger than any stored image can be");
        }
        let bytes = resp.bytes().await.context("image download body")?;
        anyhow::ensure!(bytes.len() <= MAX_ATTACHMENT_DOWNLOAD_BYTES, "image download: larger than any stored image can be");
        Ok(bytes.to_vec())
    }
}
