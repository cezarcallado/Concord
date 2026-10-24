//! Direct-message and private-call calls of [`HubClient`] (the contract is in `vox_proto::dm`).
//!
//! Failures carry the HTTP status as [`HttpStatusError`] (`403` blocked, `409` busy, `429` slow
//! down...), so the app can say what went wrong.

use anyhow::{Context, Result};
use vox_proto::{
    DmBlockRequest, DmCallResponse, DmConversation, DmId, DmMessage, DmMessageId, DmReadRequest, DmSendRequest, OkResponse, User, UserId,
};

use super::*;

/// How the hub answered `GET /api/dms`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DmsReply {
    Ready(Vec<DmConversation>),
    /// `404`: a hub older than DMs. The app hides them.
    Unsupported,
    Failed(String),
}

async fn checked<T: DeserializeOwned>(what: &str, resp: reqwest::Response) -> Result<T> {
    let status = resp.status();
    if status.is_success() {
        return resp.json::<T>().await.with_context(|| format!("{what}: invalid response body"));
    }
    let body = resp.text().await.unwrap_or_default();
    let message = serde_json::from_str::<ApiError>(&body).map(|e| e.error).unwrap_or(body);
    Err(anyhow::Error::new(HttpStatusError { status: status.as_u16(), message }).context(format!("{what} failed")))
}

impl HubClient {
    async fn dm_post<B: Serialize, T: DeserializeOwned>(&self, what: &str, path: &str, body: &B) -> Result<T> {
        let resp = self.auth(self.http.post(self.url(path)?))?.json(body).send().await.with_context(|| format!("{what} request"))?;
        checked(what, resp).await
    }

    /// `GET /api/dms`: my conversations, most recent first.
    pub async fn dms(&self) -> DmsReply {
        let req = match self.url("api/dms").and_then(|u| self.auth(self.http.get(u))) {
            Ok(r) => r,
            Err(e) => return DmsReply::Failed(format!("{e:#}")),
        };
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return DmsReply::Failed(format!("dms request: {e}")),
        };
        match resp.status().as_u16() {
            404 | 405 => DmsReply::Unsupported,
            s if (200..300).contains(&s) => match resp.json::<Vec<DmConversation>>().await {
                Ok(list) => DmsReply::Ready(list),
                Err(e) => DmsReply::Failed(format!("dms: invalid response body: {e}")),
            },
            s => DmsReply::Failed(format!("dms: HTTP {s}")),
        }
    }

    /// `GET /api/dms/people`: everybody I can start a DM with.
    pub async fn dm_people(&self) -> Result<Vec<User>> {
        let resp = self.auth(self.http.get(self.url("api/dms/people")?))?.send().await.context("dm people request")?;
        checked("dm people", resp).await
    }

    /// `POST /api/dms/{user_id}`: open (or create) my conversation with `user`.
    pub async fn open_dm(&self, user: UserId) -> Result<DmConversation> {
        self.dm_post("open dm", &format!("api/dms/{user}"), &serde_json::json!({})).await
    }

    /// `GET /api/dms/{id}/messages?before=&limit=` (newest first).
    pub async fn dm_messages(&self, dm: DmId, before: Option<DmMessageId>, limit: u32) -> Result<Vec<DmMessage>> {
        let mut url = self.url(&format!("api/dms/{dm}/messages"))?;
        {
            let mut q = url.query_pairs_mut();
            if let Some(b) = before {
                q.append_pair("before", &b.to_string());
            }
            q.append_pair("limit", &limit.clamp(1, vox_proto::DM_PAGE_MAX).to_string());
        }
        let resp = self.auth(self.http.get(url))?.send().await.context("dm messages request")?;
        checked("dm messages", resp).await
    }

    /// `POST /api/dms/{id}/messages`
    pub async fn send_dm(&self, dm: DmId, content: &str) -> Result<DmMessage> {
        self.send_dm_with(dm, content, &[]).await
    }

    /// `POST /api/dms/{id}/messages` with images (my pending uploads, see `upload_attachment`).
    pub async fn send_dm_with(&self, dm: DmId, content: &str, attachments: &[vox_proto::AttachmentId]) -> Result<DmMessage> {
        let body = DmSendRequest { content: content.to_string(), attachments: attachments.to_vec(), ..Default::default() };
        self.send_dm_request(dm, &body).await
    }

    /// `POST /api/dms/{id}/messages` with everything a line can carry (images, a reply, or E2's
    /// sealed body with an empty `content`).
    pub async fn send_dm_request(&self, dm: DmId, body: &DmSendRequest) -> Result<DmMessage> {
        self.dm_post("send dm", &format!("api/dms/{dm}/messages"), body).await
    }

    /// `POST /api/dms/{id}/messages/{message_id}/reactions`
    pub async fn dm_react(&self, dm: DmId, message: DmMessageId, emoji: &str, add: bool) -> Result<()> {
        let body = vox_proto::ReactionRequest { emoji: emoji.to_string(), add };
        self.dm_post::<_, OkResponse>("dm reaction", &format!("api/dms/{dm}/messages/{message}/reactions"), &body).await.map(|_| ())
    }

    /// `POST /api/messages/{id}/reactions` (a channel message).
    pub async fn react(&self, message: vox_proto::MessageId, emoji: &str, add: bool) -> Result<()> {
        let body = vox_proto::ReactionRequest { emoji: emoji.to_string(), add };
        self.dm_post::<_, OkResponse>("reaction", &format!("api/messages/{message}/reactions"), &body).await.map(|_| ())
    }

    /// `POST /api/dms/{id}/messages/{message_id}/delete`
    pub async fn delete_dm_message(&self, dm: DmId, message: DmMessageId) -> Result<()> {
        self.dm_post::<_, OkResponse>("delete dm message", &format!("api/dms/{dm}/messages/{message}/delete"), &serde_json::json!({})).await.map(|_| ())
    }

    /// `POST /api/dms/{id}/read`
    pub async fn dm_read(&self, dm: DmId, up_to: DmMessageId) -> Result<()> {
        self.dm_post::<_, OkResponse>("dm read", &format!("api/dms/{dm}/read"), &DmReadRequest { up_to }).await.map(|_| ())
    }

    /// `POST /api/dms/blocks/{user_id}`
    pub async fn dm_block(&self, user: UserId, blocked: bool) -> Result<()> {
        self.dm_post::<_, OkResponse>("dm block", &format!("api/dms/blocks/{user}"), &DmBlockRequest { blocked }).await.map(|_| ())
    }

    /// `POST /api/dms/{id}/call`: ring (or join this DM's call). The answer has the private room's token.
    pub async fn dm_call(&self, dm: DmId) -> Result<DmCallResponse> {
        self.dm_call_with(dm, false).await
    }

    /// `POST /api/dms/{id}/call/accept`
    pub async fn dm_call_accept(&self, dm: DmId) -> Result<DmCallResponse> {
        self.dm_call_accept_with(dm, false).await
    }

    /// v1.1 (E1): [`HubClient::dm_call`] saying whether this app can encrypt the call
    /// (`?e2ee=1`): the call is then encrypted when the other person has a device with keys.
    pub async fn dm_call_with(&self, dm: DmId, e2ee: bool) -> Result<DmCallResponse> {
        let q = if e2ee { "?e2ee=1" } else { "" };
        self.dm_post("call", &format!("api/dms/{dm}/call{q}"), &serde_json::json!({})).await
    }

    /// v1.1 (E1): [`HubClient::dm_call_accept`] saying whether this app can encrypt the call; an
    /// encrypted call refuses an answer without it (`409` [`vox_proto::ERR_E2EE_CALL_REQUIRED`]).
    pub async fn dm_call_accept_with(&self, dm: DmId, e2ee: bool) -> Result<DmCallResponse> {
        let q = if e2ee { "?e2ee=1" } else { "" };
        self.dm_post("answer call", &format!("api/dms/{dm}/call/accept{q}"), &serde_json::json!({})).await
    }

    /// `POST /api/dms/{id}/call/decline`
    pub async fn dm_call_decline(&self, dm: DmId) -> Result<()> {
        self.dm_post::<_, OkResponse>("decline call", &format!("api/dms/{dm}/call/decline"), &serde_json::json!({})).await.map(|_| ())
    }

    /// `POST /api/dms/{id}/call/end`
    pub async fn dm_call_end(&self, dm: DmId) -> Result<()> {
        self.dm_post::<_, OkResponse>("end call", &format!("api/dms/{dm}/call/end"), &serde_json::json!({})).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn serve(status: &'static str, body: &'static str) -> HubClient {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = l.accept().await else { return };
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    let raw = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = sock.write_all(raw.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        let c = HubClient::new(&format!("http://{addr}")).unwrap();
        c.set_token(Some("t".into()));
        c
    }

    #[tokio::test]
    async fn an_old_hub_means_no_dms_and_errors_keep_their_status() {
        assert_eq!(serve("404 Not Found", "").await.dms().await, DmsReply::Unsupported);
        assert_eq!(serve("200 OK", "[]").await.dms().await, DmsReply::Ready(vec![]));
        assert!(matches!(serve("500 Internal Server Error", "x").await.dms().await, DmsReply::Failed(_)));
        let e = serve("403 Forbidden", r#"{"error":"blocked"}"#).await.send_dm(1, "oi").await.unwrap_err();
        let st = e.downcast_ref::<HttpStatusError>().unwrap();
        assert_eq!((st.status, st.message.as_str()), (403, vox_proto::ERR_DM_BLOCKED));
    }
}
