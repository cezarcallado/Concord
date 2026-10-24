//! Hub API client: HTTP calls from `vox-proto` plus a self-reconnecting WebSocket that yields
//! [`ServerEvent`]s and accepts [`ClientEvent`]s.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{self, Message};
use tracing::{debug, info, warn};
use url::Url;
use vox_proto::{
    ApiError, AuditEntry, AvatarResponse, Channel, ChannelAccessRequest, ChannelCreateRequest, ChannelId, ChannelKind, ChannelRenameRequest, ChannelReorderRequest, ClaimOwnerRequest,
    ClaimOwnerResponse, ClientEvent, HealthResponse, LinkPreview, LoginRequest, LoginResponse, MemberInfo, Message as ChatMessage, MessageId, ModMoveRequest, ModMuteRequest,
    OkResponse, PrivateAccess, ResetPasswordResponse, Role, ServerEvent, SetPasswordRequest, SetPasswordResponse, SetRoleRequest, TransferOwnerRequest, UserId,
    VoiceJoinResponse, PROTOCOL_VERSION,
};

mod attachments;
mod dms;
mod e2ee;
mod invites;
mod profile;
mod sounds;
mod stream_preview;
pub use attachments::{AttachmentsReply, MAX_ATTACHMENT_DOWNLOAD_BYTES};
pub use dms::DmsReply;
pub use e2ee::RegisterReply as E2eeRegisterReply;
pub use profile::ProfileReply;
pub use sounds::{HttpStatusError, SoundsReply, MAX_SOUND_DOWNLOAD_BYTES};
pub use stream_preview::{StreamPreviewData, StreamPreviewReply, UploadReply, MAX_STREAM_PREVIEW_ANSWER};

/// A thumbnail is at most 400x225 JPEG (a few tens of KB); anything much bigger is not from a
/// hub that follows the contract, and is not loaded.
pub const MAX_PREVIEW_IMAGE_BYTES: usize = 512 * 1024;

/// How the hub answered a link-preview lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewReply {
    /// May be empty (nothing worth showing): see [`LinkPreview::is_empty`].
    Ready(LinkPreview),
    /// `404`: a hub older than link previews has no such route. Quietly means "no previews".
    Unsupported,
    /// `429`: too many fresh lookups just now; try again in a few seconds.
    Busy,
    Failed(String),
}

/// A profile picture is at most a few tens of KB (256x256 JPEG / PNG); a bigger answer is not from a
/// hub that follows the contract, and is not loaded.
pub const MAX_AVATAR_IMAGE_BYTES: usize = 512 * 1024;

/// How the hub answered a profile-picture download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AvatarReply {
    /// The JPEG / PNG bytes.
    Image(Vec<u8>),
    /// `404`: no such picture (an older hub, a removed picture, or a stale version). Not an
    /// error: the client keeps the coloured initial.
    NotFound,
    Failed(String),
}

/// HTTP client for the hub. Cheap to clone the inner `reqwest::Client`; keep one per server.
pub struct HubClient {
    base: Url,
    http: reqwest::Client,
    token: RwLock<Option<String>>,
    /// v1.1 (T0): pinned TLS (the pin, and the configuration the event socket uses too).
    tls: Option<(crate::tls::Pin, std::sync::Arc<rustls::ClientConfig>)>,
}

impl HubClient {
    /// A client for `base` as given (`http://` in the clear; an `https://` URL with the system's
    /// certificate authorities). The app talks to hubs with [`HubClient::new_pinned`] whenever it
    /// knows the hub's certificate.
    pub fn new(base: &str) -> Result<Self> {
        Self::build(base, None)
    }

    /// v1.1 (T0): pinned TLS to the hub at `base` (saved as `http://host:port`: the scheme becomes
    /// `https`, the event socket `wss`). Only the certificate whose SHA-256 is `pin` is accepted,
    /// by every connection of this client (HTTP and the event socket); no redirects.
    pub fn new_pinned(base: &str, pin: crate::tls::Pin) -> Result<Self> {
        Self::build(base, Some(pin))
    }

    fn build(base: &str, pin: Option<crate::tls::Pin>) -> Result<Self> {
        let mut base = Url::parse(base).with_context(|| format!("invalid hub url {base:?}"))?;
        if !base.path().ends_with('/') {
            let p = format!("{}/", base.path());
            base.set_path(&p);
        }
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            // The hub closes connections idle for 20 s (v0.6.2+): drop ours first, so a request
            // never goes out on a socket the hub is closing.
            .pool_idle_timeout(Duration::from_secs(15));
        let tls = match pin {
            Some(pin) => {
                if base.scheme() == "http" {
                    base.set_scheme("https").map_err(|_| anyhow!("cannot use TLS for {base}"))?;
                }
                if base.scheme() != "https" {
                    bail!("pinned TLS needs an http(s) hub url, not {base}");
                }
                builder = builder.use_preconfigured_tls(crate::tls::client_config(pin)).https_only(true).redirect(reqwest::redirect::Policy::none());
                Some((pin, std::sync::Arc::new(crate::tls::client_config(pin))))
            }
            None => None,
        };
        let http = builder.build().context("build http client")?;
        Ok(Self { base, http, token: RwLock::new(None), tls })
    }

    pub fn base_url(&self) -> &Url {
        &self.base
    }

    /// v1.1 (T0): the pinned certificate, when this client talks TLS ([`HubClient::new_pinned`]).
    pub fn tls_pin(&self) -> Option<crate::tls::Pin> {
        self.tls.as_ref().map(|(p, _)| *p)
    }

    pub fn token(&self) -> Option<String> {
        self.token.read().clone()
    }

    pub fn set_token(&self, token: Option<String>) {
        *self.token.write() = token;
    }

    fn url(&self, path: &str) -> Result<Url> {
        self.base.join(path).with_context(|| format!("bad path {path}"))
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        let token = self.token().ok_or_else(|| anyhow!("not logged in"))?;
        Ok(req.bearer_auth(token))
    }

    async fn parse<T: DeserializeOwned>(what: &str, resp: reqwest::Response) -> Result<T> {
        let status = resp.status();
        if status.is_success() {
            return resp.json::<T>().await.with_context(|| format!("{what}: invalid response body"));
        }
        let body = resp.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<ApiError>(&body).map(|e| e.error).unwrap_or(body);
        bail!("{what} failed: HTTP {status}: {msg}")
    }

    /// `GET /api/health`
    pub async fn health(&self) -> Result<HealthResponse> {
        let resp = self.http.get(self.url("api/health")?).send().await.context("health request")?;
        Self::parse("health", resp).await
    }

    /// `POST /api/login`; stores the session token for later calls.
    pub async fn login(&self, username: &str, server_password: &str) -> Result<LoginResponse> {
        self.login_with(&LoginRequest { username: username.to_string(), server_password: server_password.to_string(), ..Default::default() })
            .await
    }

    /// `POST /api/login` with the item-12 identity fields (personal password, device token).
    /// `protocol_version` is filled in. A name that needs its personal password fails with an
    /// error containing [`vox_proto::ERR_PERSONAL_PASSWORD_REQUIRED`].
    pub async fn login_with(&self, req: &LoginRequest) -> Result<LoginResponse> {
        let mut req = req.clone();
        req.protocol_version = PROTOCOL_VERSION;
        let resp = self.http.post(self.url("api/login")?).json(&req).send().await.context("login request")?;
        let login: LoginResponse = Self::parse("login", resp).await?;
        self.set_token(Some(login.token.clone()));
        info!(
            user = %login.user.username,
            user_id = login.user.id,
            role = ?login.user.role,
            server = %login.server_name,
            device_token = login.device_token.is_some(),
            "logged in to hub"
        );
        Ok(login)
    }

    async fn post<B: Serialize, T: DeserializeOwned>(&self, what: &str, path: &str, body: &B) -> Result<T> {
        let resp = self.auth(self.http.post(self.url(path)?))?.json(body).send().await.with_context(|| format!("{what} request"))?;
        Self::parse(what, resp).await
    }

    async fn get<T: DeserializeOwned>(&self, what: &str, url: Url) -> Result<T> {
        let resp = self.auth(self.http.get(url))?.send().await.with_context(|| format!("{what} request"))?;
        Self::parse(what, resp).await
    }

    // ---- item 12: my account, moderation, administration (see `vox_proto` for the rules)

    /// `POST /api/account/password`
    pub async fn set_password(&self, req: &SetPasswordRequest) -> Result<SetPasswordResponse> {
        self.post("set password", "api/account/password", req).await
    }

    /// `POST /api/mod/users/{id}/mute`
    pub async fn mod_mute(&self, user: UserId, muted: bool) -> Result<()> {
        self.post::<_, OkResponse>("server mute", &format!("api/mod/users/{user}/mute"), &ModMuteRequest { muted }).await.map(|_| ())
    }

    /// `POST /api/mod/users/{id}/disconnect`
    pub async fn mod_disconnect(&self, user: UserId) -> Result<()> {
        self.post::<_, OkResponse>("disconnect", &format!("api/mod/users/{user}/disconnect"), &serde_json::json!({})).await.map(|_| ())
    }

    /// `POST /api/mod/users/{id}/move`
    pub async fn mod_move(&self, user: UserId, channel_id: ChannelId) -> Result<()> {
        self.post::<_, OkResponse>("move", &format!("api/mod/users/{user}/move"), &ModMoveRequest { channel_id }).await.map(|_| ())
    }

    /// `POST /api/mod/users/{id}/stop-stream`
    pub async fn mod_stop_stream(&self, user: UserId) -> Result<()> {
        self.post::<_, OkResponse>("stop stream", &format!("api/mod/users/{user}/stop-stream"), &serde_json::json!({})).await.map(|_| ())
    }

    /// `POST /api/messages/{id}/delete`
    pub async fn delete_message(&self, id: MessageId) -> Result<()> {
        self.post::<_, OkResponse>("delete message", &format!("api/messages/{id}/delete"), &serde_json::json!({})).await.map(|_| ())
    }

    /// `GET /api/admin/users`
    pub async fn admin_users(&self) -> Result<Vec<MemberInfo>> {
        self.get("members", self.url("api/admin/users")?).await
    }

    /// `POST /api/admin/users/{id}/role`
    pub async fn set_role(&self, user: UserId, role: Role) -> Result<()> {
        self.post::<_, OkResponse>("set role", &format!("api/admin/users/{user}/role"), &SetRoleRequest { role }).await.map(|_| ())
    }

    /// `POST /api/admin/users/{id}/reset-password` (owner only)
    pub async fn reset_password(&self, user: UserId) -> Result<ResetPasswordResponse> {
        self.post("reset password", &format!("api/admin/users/{user}/reset-password"), &serde_json::json!({})).await
    }

    /// `POST /api/owner/claim`: take a server that has no owner with the code its console shows.
    pub async fn claim_owner(&self, code: &str) -> Result<ClaimOwnerResponse> {
        self.post("claim server", "api/owner/claim", &ClaimOwnerRequest { code: code.to_string() }).await
    }

    /// `POST /api/admin/transfer-owner` (owner only)
    pub async fn transfer_owner(&self, user: UserId) -> Result<()> {
        self.post::<_, OkResponse>("transfer ownership", "api/admin/transfer-owner", &TransferOwnerRequest { user_id: user }).await.map(|_| ())
    }

    /// `GET /api/admin/audit?before=&limit=` (newest first)
    pub async fn audit_log(&self, before: Option<i64>, limit: u32) -> Result<Vec<AuditEntry>> {
        let mut url = self.url("api/admin/audit")?;
        {
            let mut q = url.query_pairs_mut();
            if let Some(b) = before {
                q.append_pair("before", &b.to_string());
            }
            q.append_pair("limit", &limit.clamp(1, 200).to_string());
        }
        self.get("audit log", url).await
    }

    /// `POST /api/admin/channels` (v0.9: `private` makes it private from the start).
    pub async fn create_channel(&self, name: &str, kind: ChannelKind, private: Option<PrivateAccess>) -> Result<Channel> {
        self.post("create channel", "api/admin/channels", &ChannelCreateRequest { name: name.to_string(), kind, private }).await
    }

    /// `POST /api/admin/channels/{id}/access` (v0.9): private with this list, or public (`None`).
    pub async fn set_channel_access(&self, id: ChannelId, private: Option<PrivateAccess>) -> Result<()> {
        self.post::<_, OkResponse>("channel access", &format!("api/admin/channels/{id}/access"), &ChannelAccessRequest { private }).await.map(|_| ())
    }

    /// v1.1 (E1): `GET /api/channels/{id}/e2ee` (Admin and up): whether the voice channel's calls
    /// are end-to-end encrypted and who could not enter (no device with keys).
    pub async fn channel_e2ee(&self, id: ChannelId) -> Result<vox_proto::ChannelE2eeResponse> {
        let resp = self.auth(self.http.get(self.url(&format!("api/channels/{id}/e2ee"))?))?.send().await.context("channel e2ee request")?;
        Self::parse("channel e2ee", resp).await
    }

    /// v1.1 (E1): `POST /api/channels/{id}/e2ee`: turn them on or off. Only in an empty room: the
    /// error then contains [`vox_proto::ERR_ROOM_OCCUPIED`].
    pub async fn set_channel_e2ee(&self, id: ChannelId, on: bool) -> Result<vox_proto::ChannelE2eeResponse> {
        self.post("channel e2ee", &format!("api/channels/{id}/e2ee"), &vox_proto::ChannelE2eeRequest { on }).await
    }

    /// `POST /api/admin/channels/{id}/rename`
    pub async fn rename_channel(&self, id: ChannelId, name: &str) -> Result<()> {
        self.post::<_, OkResponse>("rename channel", &format!("api/admin/channels/{id}/rename"), &ChannelRenameRequest { name: name.to_string() })
            .await
            .map(|_| ())
    }

    /// `POST /api/admin/channels/{id}/delete`
    pub async fn delete_channel(&self, id: ChannelId) -> Result<()> {
        self.post::<_, OkResponse>("delete channel", &format!("api/admin/channels/{id}/delete"), &serde_json::json!({})).await.map(|_| ())
    }

    /// `POST /api/admin/channels/reorder`
    pub async fn reorder_channels(&self, ids: Vec<ChannelId>) -> Result<()> {
        self.post::<_, OkResponse>("reorder channels", "api/admin/channels/reorder", &ChannelReorderRequest { ids }).await.map(|_| ())
    }

    /// `GET /api/channels`
    pub async fn channels(&self) -> Result<Vec<Channel>> {
        let resp = self.auth(self.http.get(self.url("api/channels")?))?.send().await.context("channels request")?;
        Self::parse("channels", resp).await
    }

    /// `GET /api/channels/{id}/messages?before=&limit=` (newest first).
    pub async fn messages(&self, channel: ChannelId, before: Option<MessageId>, limit: u32) -> Result<Vec<ChatMessage>> {
        let mut url = self.url(&format!("api/channels/{channel}/messages"))?;
        {
            let mut q = url.query_pairs_mut();
            if let Some(b) = before {
                q.append_pair("before", &b.to_string());
            }
            q.append_pair("limit", &limit.min(100).to_string());
        }
        let resp = self.auth(self.http.get(url))?.send().await.context("messages request")?;
        Self::parse("messages", resp).await
    }

    /// `GET /api/link-preview?url=`
    pub async fn link_preview(&self, page_url: &str) -> PreviewReply {
        let req = match self.url("api/link-preview").and_then(|mut u| {
            u.query_pairs_mut().append_pair("url", page_url);
            self.auth(self.http.get(u))
        }) {
            Ok(r) => r,
            Err(e) => return PreviewReply::Failed(format!("{e:#}")),
        };
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return PreviewReply::Failed(format!("link preview request: {e}")),
        };
        match resp.status().as_u16() {
            404 => PreviewReply::Unsupported,
            429 => PreviewReply::Busy,
            s if (200..300).contains(&s) => match resp.json::<LinkPreview>().await {
                Ok(p) => PreviewReply::Ready(p),
                Err(e) => PreviewReply::Failed(format!("link preview: invalid response body: {e}")),
            },
            s => PreviewReply::Failed(format!("link preview: HTTP {s}")),
        }
    }

    /// `GET /api/link-preview/image?url=`: the thumbnail's JPEG bytes (`None`: there is none, or
    /// it could not be loaded).
    pub async fn link_preview_image(&self, page_url: &str) -> Option<Vec<u8>> {
        let mut u = self.url("api/link-preview/image").ok()?;
        u.query_pairs_mut().append_pair("url", page_url);
        let resp = self.auth(self.http.get(u)).ok()?.send().await.ok()?;
        if !resp.status().is_success() || resp.content_length().is_some_and(|n| n > MAX_PREVIEW_IMAGE_BYTES as u64) {
            return None;
        }
        let bytes = resp.bytes().await.ok()?;
        (bytes.len() <= MAX_PREVIEW_IMAGE_BYTES).then(|| bytes.to_vec())
    }

    // ---- profile pictures (see `vox_proto` for the rules)

    /// `PUT /api/me/avatar`: the picture file as it is. The hub crops, resizes and re-encodes it.
    pub async fn set_avatar(&self, file: Vec<u8>) -> Result<AvatarResponse> {
        let req = self.auth(self.http.put(self.url("api/me/avatar")?))?.timeout(Duration::from_secs(60)).body(file);
        let resp = req.send().await.context("set avatar request")?;
        Self::parse("set avatar", resp).await
    }

    /// `DELETE /api/me/avatar`
    pub async fn clear_avatar(&self) -> Result<AvatarResponse> {
        let resp = self.auth(self.http.delete(self.url("api/me/avatar")?))?.send().await.context("clear avatar request")?;
        Self::parse("clear avatar", resp).await
    }

    /// `POST /api/mod/users/{id}/remove-avatar`
    pub async fn mod_remove_avatar(&self, user: UserId) -> Result<()> {
        self.post::<_, OkResponse>("remove avatar", &format!("api/mod/users/{user}/remove-avatar"), &serde_json::json!({})).await.map(|_| ())
    }

    /// `GET /api/users/{id}/avatar?size=&v=`: `size` is 64 or 256, `version` is `User::avatar`.
    pub async fn avatar_image(&self, user: UserId, size: u32, version: &str) -> AvatarReply {
        let url = match self.url(&format!("api/users/{user}/avatar")) {
            Ok(mut u) => {
                u.query_pairs_mut().append_pair("size", &size.to_string()).append_pair("v", version);
                u
            }
            Err(e) => return AvatarReply::Failed(format!("{e:#}")),
        };
        let req = match self.auth(self.http.get(url)) {
            Ok(r) => r,
            Err(e) => return AvatarReply::Failed(format!("{e:#}")),
        };
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return AvatarReply::Failed(format!("avatar request: {e}")),
        };
        match resp.status().as_u16() {
            404 => AvatarReply::NotFound,
            s if (200..300).contains(&s) => {
                if resp.content_length().is_some_and(|n| n > MAX_AVATAR_IMAGE_BYTES as u64) {
                    return AvatarReply::Failed("avatar too large".into());
                }
                match resp.bytes().await {
                    Ok(b) if b.len() <= MAX_AVATAR_IMAGE_BYTES => AvatarReply::Image(b.to_vec()),
                    Ok(_) => AvatarReply::Failed("avatar too large".into()),
                    Err(e) => AvatarReply::Failed(format!("avatar body: {e}")),
                }
            }
            s => AvatarReply::Failed(format!("avatar: HTTP {s}")),
        }
    }

    /// `POST /api/voice/{channel_id}/join`
    pub async fn voice_join(&self, channel: ChannelId) -> Result<VoiceJoinResponse> {
        self.voice_join_with(channel, false).await
    }

    /// v1.1 (E1): [`HubClient::voice_join`] saying whether this app can encrypt the room's media
    /// (`?e2ee=1`); an encrypted room refuses a join without it (`403`
    /// [`vox_proto::ERR_E2EE_REQUIRED`]) and answers [`VoiceJoinResponse::e2ee`] with it.
    pub async fn voice_join_with(&self, channel: ChannelId, e2ee: bool) -> Result<VoiceJoinResponse> {
        let q = if e2ee { "?e2ee=1" } else { "" };
        let url = self.url(&format!("api/voice/{channel}/join{q}"))?;
        let resp = self.auth(self.http.post(url))?.send().await.context("voice join request")?;
        Self::parse("voice join", resp).await
    }

    /// `ws(s)://host/ws?token=...`
    pub fn ws_url(&self) -> Result<Url> {
        let token = self.token().ok_or_else(|| anyhow!("not logged in"))?;
        let mut url = self.url("ws")?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme).map_err(|_| anyhow!("cannot convert hub url to websocket url"))?;
        url.query_pairs_mut().clear().append_pair("token", &token);
        Ok(url)
    }

    /// Start the event WebSocket (reconnects with exponential backoff until the returned
    /// receiver or sender is dropped).
    pub fn connect_events(&self) -> Result<(HubSender, mpsc::Receiver<HubEvent>)> {
        let url = self.ws_url()?;
        let (out_tx, out_rx) = mpsc::channel::<ClientEvent>(64);
        let (ev_tx, ev_rx) = mpsc::channel::<HubEvent>(256);
        tokio::spawn(ws_loop(url, out_rx, ev_tx, self.tls.as_ref().map(|(_, c)| c.clone())));
        Ok((HubSender { tx: out_tx }, ev_rx))
    }
}

/// What the WS task reports. (`Server` is big since v0.8's message fields; events are handled one
/// at a time, never kept in bulk, so it is not boxed.)
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum HubEvent {
    Connected,
    Disconnected { reason: String },
    /// The hub rejected our session token; the caller must log in again.
    AuthFailed,
    Server(ServerEvent),
}

/// Sends [`ClientEvent`]s over the (re)connecting socket. Events queue while disconnected.
#[derive(Clone)]
pub struct HubSender {
    tx: mpsc::Sender<ClientEvent>,
}

impl HubSender {
    pub fn send(&self, ev: ClientEvent) -> Result<()> {
        self.tx.try_send(ev).map_err(|e| anyhow!("hub send queue: {e}"))
    }
}

/// Exponential backoff with jitter: base * 2^n capped at max, +0..25% jitter.
#[derive(Debug, Clone)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    attempt: u32,
    seed: u64,
}

impl Backoff {
    pub fn new(base: Duration, max: Duration) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15)
            | 1;
        Self { base, max, attempt: 0, seed }
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    pub fn next_delay(&mut self) -> Duration {
        let exp = self.base.saturating_mul(1u32 << self.attempt.min(16));
        let capped = exp.min(self.max);
        self.attempt = self.attempt.saturating_add(1);
        // xorshift64
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        let jitter = capped.mul_f64((self.seed % 1000) as f64 / 4000.0);
        capped + jitter
    }
}

const PING_EVERY: Duration = Duration::from_secs(20);
const IDLE_TIMEOUT: Duration = Duration::from_secs(65);

async fn ws_loop(url: Url, mut out_rx: mpsc::Receiver<ClientEvent>, ev_tx: mpsc::Sender<HubEvent>, tls: Option<std::sync::Arc<rustls::ClientConfig>>) {
    let mut backoff = Backoff::new(Duration::from_millis(500), Duration::from_secs(30));
    let safe_url = {
        let mut u = url.clone();
        u.set_query(None);
        u.to_string()
    };
    loop {
        // v1.1 (T0): over pinned TLS, never the default (web PKI) connector.
        let connecting = async {
            match &tls {
                Some(cfg) => tokio_tungstenite::connect_async_tls_with_config(url.as_str(), None, true, Some(tokio_tungstenite::Connector::Rustls(cfg.clone()))).await,
                None => tokio_tungstenite::connect_async(url.as_str()).await,
            }
        };
        let attempt = tokio::select! {
            r = connecting => r,
            _ = ev_tx.closed() => return,
        };
        match attempt {
            Ok((ws, _resp)) => {
                info!(url = %safe_url, "hub websocket connected");
                if ev_tx.send(HubEvent::Connected).await.is_err() {
                    return;
                }
                let started = Instant::now();
                let outcome = run_session(ws, &mut out_rx, &ev_tx).await;
                match outcome {
                    SessionEnd::Shutdown => return,
                    SessionEnd::Lost(reason) => {
                        warn!(reason = %reason, connected_for_s = started.elapsed().as_secs(), "hub websocket lost");
                        if started.elapsed() > Duration::from_secs(30) {
                            backoff.reset();
                        }
                        if ev_tx.send(HubEvent::Disconnected { reason }).await.is_err() {
                            return;
                        }
                    }
                }
            }
            Err(tungstenite::Error::Http(resp)) if resp.status().as_u16() == 401 || resp.status().as_u16() == 403 => {
                warn!(status = %resp.status(), "hub rejected websocket session token");
                let _ = ev_tx.send(HubEvent::AuthFailed).await;
                return;
            }
            Err(e) => {
                debug!(error = %e, "hub websocket connect failed");
            }
        }
        let delay = backoff.next_delay();
        info!(retry_in_ms = delay.as_millis() as u64, "hub websocket reconnecting");
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = ev_tx.closed() => return,
        }
    }
}

enum SessionEnd {
    Shutdown,
    Lost(String),
}

async fn run_session<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    out_rx: &mut mpsc::Receiver<ClientEvent>,
    ev_tx: &mpsc::Sender<HubEvent>,
) -> SessionEnd
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut sink, mut stream) = ws.split();
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    let mut nonce = 0u64;
    let mut last_rx = Instant::now();
    loop {
        tokio::select! {
            msg = stream.next() => {
                let Some(msg) = msg else { return SessionEnd::Lost("stream ended".into()) };
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => return SessionEnd::Lost(format!("read error: {e}")),
                };
                last_rx = Instant::now();
                match msg {
                    Message::Text(text) => match serde_json::from_str::<ServerEvent>(text.as_str()) {
                        Ok(ev) => {
                            if ev_tx.send(HubEvent::Server(ev)).await.is_err() {
                                return SessionEnd::Shutdown;
                            }
                        }
                        Err(e) => warn!(error = %e, len = text.len(), "unparseable hub event"),
                    },
                    Message::Close(frame) => {
                        return SessionEnd::Lost(format!("closed by hub: {frame:?}"));
                    }
                    _ => {}
                }
            }
            out = out_rx.recv() => {
                let Some(ev) = out else {
                    let _ = sink.send(Message::Close(None)).await;
                    return SessionEnd::Shutdown;
                };
                let json = match serde_json::to_string(&ev) {
                    Ok(j) => j,
                    Err(e) => { warn!(error = %e, "cannot serialise client event"); continue; }
                };
                if let Err(e) = sink.send(Message::text(json)).await {
                    return SessionEnd::Lost(format!("write error: {e}"));
                }
            }
            _ = ping.tick() => {
                if last_rx.elapsed() > IDLE_TIMEOUT {
                    return SessionEnd::Lost("no traffic from hub (timeout)".into());
                }
                nonce += 1;
                let json = serde_json::to_string(&ClientEvent::Ping { nonce }).unwrap_or_default();
                if let Err(e) = sink.send(Message::text(json)).await {
                    return SessionEnd::Lost(format!("ping write error: {e}"));
                }
            }
            _ = ev_tx.closed() => return SessionEnd::Shutdown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let mut b = Backoff::new(Duration::from_millis(500), Duration::from_secs(30));
        let d0 = b.next_delay();
        assert!(d0 >= Duration::from_millis(500) && d0 <= Duration::from_millis(625));
        let d1 = b.next_delay();
        assert!(d1 >= Duration::from_millis(1000) && d1 <= Duration::from_millis(1250));
        // 2 s, 4 s, 8 s, 16 s, then capped at 30 s.
        for want in [2_000u64, 4_000, 8_000, 16_000] {
            let d = b.next_delay();
            assert!(d >= Duration::from_millis(want) && d <= Duration::from_millis(want * 5 / 4));
        }
        for _ in 0..20 {
            let d = b.next_delay();
            assert!(d >= Duration::from_secs(30) && d <= Duration::from_millis(37_500));
        }
        b.reset();
        assert!(b.next_delay() <= Duration::from_millis(625));
    }

    #[test]
    fn ws_url_carries_token_and_scheme() {
        let c = HubClient::new("http://127.0.0.1:7890").unwrap();
        assert!(c.ws_url().is_err());
        c.set_token(Some("abc def".into()));
        assert_eq!(c.ws_url().unwrap().as_str(), "ws://127.0.0.1:7890/ws?token=abc+def");
        let c = HubClient::new("https://hub.example/base").unwrap();
        c.set_token(Some("t".into()));
        assert_eq!(c.ws_url().unwrap().as_str(), "wss://hub.example/base/ws?token=t");
    }

    /// A one-connection-per-request HTTP server that answers every request with `raw`.
    async fn serve(raw: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = l.accept().await else { return };
                let raw = raw.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock.write_all(&raw).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    fn http(status: &str, ctype: &str, body: &[u8]) -> Vec<u8> {
        let mut v = format!("HTTP/1.1 {status}
Content-Type: {ctype}
Content-Length: {}
Connection: close

", body.len()).into_bytes();
        v.extend(body);
        v
    }

    async fn client(raw: Vec<u8>) -> HubClient {
        let c = HubClient::new(&serve(raw).await).unwrap();
        c.set_token(Some("t".into()));
        c
    }

    #[tokio::test]
    async fn link_preview_replies_are_classified() {
        // A hub that predates the feature has no route: 404 means "no previews", not an error.
        let old = client(http("404 Not Found", "text/plain", b"")).await;
        assert_eq!(old.link_preview("https://example.com/").await, PreviewReply::Unsupported);
        assert_eq!(old.link_preview_image("https://example.com/").await, None);

        let busy = client(http("429 Too Many Requests", "application/json", br#"{"error":"slow down"}"#)).await;
        assert_eq!(busy.link_preview("https://example.com/").await, PreviewReply::Busy);

        let broken = client(http("500 Internal Server Error", "text/plain", b"x")).await;
        assert!(matches!(broken.link_preview("https://example.com/").await, PreviewReply::Failed(_)));
        let junk = client(http("200 OK", "application/json", b"not json")).await;
        assert!(matches!(junk.link_preview("https://example.com/").await, PreviewReply::Failed(_)));

        let body = br#"{"url":"https://example.com/","title":"T","site_name":"S","has_image":true,"image_w":400,"image_h":225}"#;
        let good = client(http("200 OK", "application/json", body)).await;
        match good.link_preview("https://example.com/").await {
            PreviewReply::Ready(p) => assert_eq!((p.title.as_str(), p.description.as_str(), p.has_image, p.image_w), ("T", "", true, 400)),
            other => panic!("{other:?}"),
        }
        // Not logged in: a failure, no request made.
        let anon = HubClient::new("http://127.0.0.1:1").unwrap();
        assert!(matches!(anon.link_preview("https://example.com/").await, PreviewReply::Failed(_)));
    }

    #[tokio::test]
    async fn link_preview_image_is_size_capped() {
        let ok = client(http("200 OK", "image/jpeg", &[0xff, 0xd8, 0xff, 0xd9])).await;
        assert_eq!(ok.link_preview_image("https://example.com/").await, Some(vec![0xff, 0xd8, 0xff, 0xd9]));
        let huge = client(http("200 OK", "image/jpeg", &vec![0u8; MAX_PREVIEW_IMAGE_BYTES + 1])).await;
        assert_eq!(huge.link_preview_image("https://example.com/").await, None);
    }
}
