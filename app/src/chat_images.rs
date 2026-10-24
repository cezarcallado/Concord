//! Images in the chat (channels and DMs), the app side:
//!
//! - **Sending**: the 📎 button (native "Abrir", several files), Ctrl+V of a picture or of files
//!   copied in Explorer, and drag-and-drop onto the chat. Up to four images per message, with an
//!   optional caption. Each picture is compressed on its own thread
//!   (`vox_client_core::images`), with "Comprimindo… 45%" in the preview strip above the input,
//!   then "4,8 MB → 312 KB"; ✖ takes one out. "Enviar" uploads them (the hub checks them again),
//!   then sends the message with their ids.
//! - **Showing**: thumbnails under the message text, aspect-correct inside 400x300 (two columns
//!   of 200x150 for several), rounded corners, with their space reserved from the sizes in the
//!   message (nothing jumps when they arrive). Only thumbnails of messages on screen are asked
//!   for, at most three downloads at once; textures not drawn for a while are freed and the bytes
//!   kept (bounded). "Mostrar imagens automaticamente" off: a "clique para carregar" box instead.
//! - **Viewer**: a click opens the full image over the window, fitted; the wheel zooms around the
//!   pointer, dragging pans, double click toggles 1:1, ←/→ go through the message's images, Esc or
//!   a click outside closes. "Salvar imagem…" (native "Salvar como", the stored file as it is) and
//!   "Copiar" (bitmap + PNG on the clipboard). Animations play. Closing frees everything.
//!
//! A hub that predates images answers `404` to `GET /api/attachments/limits`: the 📎 button,
//! paste and drop are off then.
//!
//! **Cache keys** (v0.8.2): thumbnails, full images and "clique para carregar" are keyed by the
//! attachment id *and* its content hash ([`ImgKey`]), never by the id alone: a hub up to v0.8.1
//! gives a deleted image's id to the next upload, and a thumbnail cached under the bare id then
//! showed the deleted picture on the new message. What a deleted message or channel had is
//! dropped from the caches too ([`App::forget_images`]).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Pos2, Rect, RichText, Sense, TextureHandle, TextureOptions, Vec2};
use vox_client_core::hub::{AttachmentsReply, HttpStatusError, HubClient};
use vox_client_core::images::{self as imgs, Compressed, Quality};
use vox_proto::{Attachment, AttachmentId, AttachmentLimits, ChannelId, DmId, DmMessage, Message, ATTACHMENT_MAX_PER_MESSAGE, ERR_ATTACHMENT_DAILY};

use crate::app::App;
use crate::backend::e2ee::dm::{self as e2dm, SendMode};
use crate::backend::e2ee::E2eeBackend;
use crate::backend::Cmd;
use crate::theme::ph;

/// The biggest inline thumbnail (points).
pub const THUMB_MAX: Vec2 = Vec2::new(400.0, 300.0);
/// Each image of a message with several.
const GRID_MAX: Vec2 = Vec2::new(200.0, 150.0);
const GAP: f32 = 4.0;
const ROUND: f32 = 8.0;
/// Preview strip cards.
const CARD: f32 = 92.0;
/// Downloads at once (thumbnails and full images together).
const MAX_IN_FLIGHT: usize = 3;
/// Thumbnails decoded into textures per frame (a screen of new ones doesn't hitch).
const DECODES_PER_FRAME: u32 = 3;
/// A thumbnail texture nobody drew for this long is freed (the bytes stay).
const TEXTURE_IDLE: Duration = Duration::from_secs(45);
/// Thumbnail bytes kept in memory, at most.
const MAX_THUMB_BYTES: usize = 24 * 1024 * 1024;
/// A failed download is asked again no sooner than this.
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// The viewer's decoded animation frames, at most (RGBA bytes).
const MAX_VIEWER_DECODED: u64 = 384 * 1024 * 1024;
/// The hub promises thumbnails inside 400x400; a bigger decoded one is refused.
const MAX_THUMB_SIDE: u32 = 512;

/// Where a message goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    Channel(ChannelId),
    Dm(DmId),
}

/// What the image caches are keyed by: the attachment id and the content it stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImgKey {
    pub id: AttachmentId,
    /// [`content_tag`] of the attachment's hash.
    pub content: u64,
}

impl ImgKey {
    pub fn of(a: &Attachment) -> ImgKey {
        ImgKey { id: a.id, content: content_tag(&a.hash) }
    }
}

/// An attachment's content hash as a number: the hub's short hash is 16 hex digits (64 bits);
/// anything else is folded (FNV-1a). `0`: a hub that sends no hash.
pub fn content_tag(hash: &str) -> u64 {
    if hash.is_empty() {
        return 0;
    }
    u64::from_str_radix(hash, 16).unwrap_or_else(|_| hash.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)))
}

// ---------------------------------------------------------------- backend calls

/// The image calls the backend runs ([`Cmd::Image`]); answers are [`ImageEv`]s.
#[derive(Debug, Clone)]
pub enum ImageCmd {
    Limits,
    Fetch { key: ImgKey, thumb: bool },
    /// Upload these pictures, then send the message. `seal` (DMs, v1.1 E2): how the line may go
    /// out; when the backend seals it, the pictures are sealed uploads too.
    Send { token: u64, target: Target, caption: String, images: Vec<Arc<Compressed>>, extras: crate::app::chat_extras::SendExtras, seal: Option<SendMode> },
}

#[derive(Debug)]
pub enum SentMessage {
    Channel(Box<Message>),
    Dm(Box<DmMessage>),
}

#[derive(Debug)]
pub enum ImageEv {
    Limits(AttachmentsReply),
    Fetched { key: ImgKey, thumb: bool, result: Result<Vec<u8>, FetchError> },
    /// `done` of `total` pictures uploaded.
    Uploaded { token: u64, done: usize, total: usize },
    Sent { token: u64, target: Target, result: Result<SentMessage, String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// `404`: deleted, or not mine to see. Not asked again.
    Gone,
    Failed(String),
}

impl ImageCmd {
    pub fn offline(self) -> ImageEv {
        let msg = "Sem conexão com o servidor.".to_string();
        match self {
            ImageCmd::Limits => ImageEv::Limits(AttachmentsReply::Failed(msg)),
            ImageCmd::Fetch { key, thumb } => ImageEv::Fetched { key, thumb, result: Err(FetchError::Failed(msg)) },
            ImageCmd::Send { token, target, .. } => ImageEv::Sent { token, target, result: Err(msg) },
        }
    }
}

/// A failed upload / send in words for the person.
pub fn error_text(e: &anyhow::Error) -> String {
    match e.downcast_ref::<HttpStatusError>() {
        Some(s) if s.status == 429 && s.message == ERR_ATTACHMENT_DAILY => "Você chegou ao limite diário de imagens deste servidor. Tente de novo amanhã.".into(),
        Some(s) if s.status == 429 => "Calma: espere um instante e tente de novo.".into(),
        Some(s) if s.status == 507 => "O servidor está sem espaço para imagens. Avise quem hospeda.".into(),
        Some(s) if s.status == 413 => "A imagem ficou grande demais para o servidor.".into(),
        Some(s) if s.status == 400 => format!("O servidor recusou a imagem ({}).", s.message),
        Some(s) if s.status == 403 => "Não dá: um de vocês bloqueou o outro.".into(),
        Some(s) if s.status == 404 => "Este servidor não aceita imagens (versão antiga).".into(),
        _ => crate::backend::friendly(e),
    }
}

/// Run one call against the hub (on the backend's runtime); `emit` sends answers to the UI.
/// `e2`: E2's sealed DM lines and images (`backend/e2ee/dm.rs`).
pub async fn run(hub: &HubClient, e2: &E2eeBackend, cmd: ImageCmd, emit: impl Fn(ImageEv)) {
    match cmd {
        ImageCmd::Limits => emit(ImageEv::Limits(hub.attachment_limits().await)),
        ImageCmd::Fetch { key, thumb } => {
            let result = hub.attachment_file(key.id, thumb).await.map_err(|e| match e.downcast_ref::<HttpStatusError>() {
                Some(s) if s.status == 404 => FetchError::Gone,
                _ => FetchError::Failed(format!("{e:#}")),
            });
            // A sealed image is decrypted here; the decoder (and its limits) only sees the result.
            let result = result.and_then(|bytes| e2dm::open_image(key.id, thumb, bytes).map_err(FetchError::Failed));
            emit(ImageEv::Fetched { key, thumb, result });
        }
        ImageCmd::Send { token, target, caption, images, extras, seal } => {
            let total = images.len();
            // E2: a DM line to somebody with a device key is sealed, its pictures too.
            let plan = match (target, seal) {
                (Target::Dm(dm), Some(mode)) => {
                    e2.prepare(hub).await;
                    match e2.plan(dm, mode).await {
                        Ok(p) => Some((mode.peer, p)),
                        Err(e) => {
                            emit(ImageEv::Sent { token, target, result: Err(e) });
                            return;
                        }
                    }
                }
                // A DM whose peer the app does not know: never plain without the plan.
                (Target::Dm(_), None) => {
                    emit(ImageEv::Sent { token, target, result: Err("conversa não encontrada (nada foi enviado)".into()) });
                    return;
                }
                (Target::Channel(_), _) => None,
            };
            let sealing = matches!(plan, Some((_, e2dm::Plan::Seal)));
            let mut ids = Vec::with_capacity(total);
            let mut att_keys = Vec::new();
            for (i, c) in images.iter().enumerate() {
                let up = if sealing {
                    match e2dm::seal_image(c) {
                        Ok(s) => hub.upload_sealed_attachment(&s.main, s.thumb.as_deref()).await.map(|a| {
                            att_keys.push(s.att_key(a.id));
                            a
                        }),
                        Err(e) => {
                            emit(ImageEv::Sent { token, target, result: Err(e) });
                            return;
                        }
                    }
                } else {
                    hub.upload_attachment(&c.bytes, c.thumb.as_deref()).await
                };
                match up {
                    Ok(a) => ids.push(a.id),
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "image upload failed");
                        emit(ImageEv::Sent { token, target, result: Err(error_text(&e)) });
                        return;
                    }
                }
                emit(ImageEv::Uploaded { token, done: i + 1, total });
            }
            let result = match target {
                Target::Channel(ch) => {
                    let req = vox_proto::SendMessageRequest { content: caption, attachments: ids, mentions: extras.mentions, reply_to: extras.reply_to, reply_mention: extras.reply_mention };
                    hub.send_channel_request(ch, &req).await.map(|m| SentMessage::Channel(Box::new(m)))
                }
                Target::Dm(dm) => {
                    let mut req = vox_proto::DmSendRequest { content: caption, attachments: ids, reply_to: extras.reply_to, sealed: None };
                    if let Some((peer, e2dm::Plan::Seal)) = plan {
                        match e2.seal_line(dm, peer, &req.content, att_keys) {
                            Ok(body) => {
                                req.content = String::new();
                                req.sealed = Some(body);
                            }
                            Err(e) => {
                                emit(ImageEv::Sent { token, target, result: Err(e) });
                                return;
                            }
                        }
                    }
                    match hub.send_dm_request(dm, &req).await {
                        Ok(m) => {
                            let mut v = vec![m];
                            e2.open_lines(hub, &mut v).await;
                            v.pop().map(|m| SentMessage::Dm(Box::new(m))).ok_or_else(|| anyhow::anyhow!("the line came back as a replay"))
                        }
                        Err(e) => Err(e),
                    }
                }
            };
            emit(ImageEv::Sent { token, target, result: result.map_err(|e| error_text(&e)) });
        }
    }
}

// ---------------------------------------------------------------- the composer

enum PendingState {
    Compressing,
    Ready(Arc<Compressed>),
    Failed(String),
}

struct Pending {
    id: u64,
    target: Target,
    name: String,
    state: PendingState,
    /// 0..=1000, written by the compressing thread.
    progress: Arc<AtomicU32>,
    tex: Option<TextureHandle>,
}

struct Sending {
    token: u64,
    target: Target,
    done: usize,
    total: usize,
}

/// What the compressing thread hands back.
type Done = (u64, Result<Compressed, String>);
/// The file picker's answer, for the chat it was opened from.
type Picked = (Target, mpsc::Receiver<Result<Vec<PathBuf>, String>>);

/// Everything about chat images an `App` holds.
pub struct ImagesUi {
    /// The hub knows chat images (its limits answered).
    pub supported: bool,
    pub limits: Option<AttachmentLimits>,
    pending: Vec<Pending>,
    next_id: u64,
    done_tx: mpsc::Sender<Done>,
    done_rx: mpsc::Receiver<Done>,
    pick_rx: Option<Picked>,
    sending: Option<Sending>,
    next_token: u64,
    /// The caption of the message being sent (given back if it fails).
    caption_backup: Option<(Target, String)>,
    pub store: ImageStore,
    pub viewer: Option<Viewer>,
    save_rx: Option<mpsc::Receiver<Result<Option<PathBuf>, String>>>,
    /// "Clique para carregar" was clicked for these (auto display off).
    allowed: HashSet<ImgKey>,
    /// Files are being dragged over the window.
    pub drop_hover: bool,
    /// Autotest / diagnostics: pictures compressed and messages with images sent.
    pub compressed: u32,
    pub sent: u32,
}

impl Default for ImagesUi {
    fn default() -> Self {
        let (done_tx, done_rx) = mpsc::channel();
        ImagesUi {
            supported: false,
            limits: None,
            pending: Vec::new(),
            next_id: 1,
            done_tx,
            done_rx,
            pick_rx: None,
            sending: None,
            next_token: 1,
            caption_backup: None,
            store: ImageStore::default(),
            viewer: None,
            save_rx: None,
            allowed: HashSet::new(),
            drop_hover: false,
            compressed: 0,
            sent: 0,
        }
    }
}

impl ImagesUi {
    /// Pictures waiting for `target`'s next message.
    pub fn pending_for(&self, target: Target) -> usize {
        self.pending.iter().filter(|p| p.target == target).count()
    }

    /// Some picture of `target` is still being compressed.
    pub fn compressing(&self, target: Target) -> bool {
        self.pending.iter().any(|p| p.target == target && matches!(p.state, PendingState::Compressing))
    }

    /// Every picture of `target` is compressed (and there is at least one).
    pub fn ready(&self, target: Target) -> bool {
        let mut any = false;
        for p in self.pending.iter().filter(|p| p.target == target) {
            match p.state {
                PendingState::Ready(_) => any = true,
                PendingState::Compressing => return false,
                PendingState::Failed(_) => {}
            }
        }
        any
    }

    /// The conversation is gone (a deleted channel): its pictures waiting to be sent go too.
    pub fn drop_drafts(&mut self, target: Target) {
        self.pending.retain(|p| p.target != target);
        if self.caption_backup.as_ref().is_some_and(|(t, _)| *t == target) {
            self.caption_backup = None;
        }
    }

    pub fn sending_to(&self, target: Target) -> bool {
        self.sending.as_ref().is_some_and(|s| s.target == target)
    }

    /// The compressed pictures of `target` (autotest).
    pub fn ready_images(&self, target: Target) -> Vec<Arc<Compressed>> {
        self.pending
            .iter()
            .filter(|p| p.target == target)
            .filter_map(|p| match &p.state {
                PendingState::Ready(c) => Some(c.clone()),
                _ => None,
            })
            .collect()
    }

    /// A new hub session.
    pub fn reset(&mut self) {
        self.supported = false;
        self.limits = None;
        self.sending = None;
        self.store = ImageStore::default();
        self.viewer = None;
        self.allowed.clear();
    }
}

/// `1234567` -> `1,2 MB`, `312000` -> `305 KB` (Brazilian decimal comma).
pub fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0)).replace('.', ",")
    } else if bytes >= 1024 {
        format!("{} KB", bytes.div_ceil(1024))
    } else {
        format!("{bytes} B")
    }
}

/// `size` (pixels) fitted inside `max` (points), never enlarged, at least a few points.
pub fn fit_size(size: (u32, u32), max: Vec2) -> Vec2 {
    let (w, h) = (size.0.max(1) as f32, size.1.max(1) as f32);
    let s = (max.x / w).min(max.y / h).min(1.0);
    Vec2::new((w * s).round().max(16.0), (h * s).round().max(16.0))
}

/// Where each of `n` images goes, inside `avail` width: one image up to [`THUMB_MAX`], several in
/// two columns of up to [`GRID_MAX`]. Returns rects relative to the top-left corner.
pub fn layout(sizes: &[(u32, u32)], avail: f32) -> Vec<Rect> {
    if sizes.len() == 1 {
        let max = Vec2::new(THUMB_MAX.x.min(avail.max(40.0)), THUMB_MAX.y);
        return vec![Rect::from_min_size(Pos2::ZERO, fit_size(sizes[0], max))];
    }
    let col_w = GRID_MAX.x.min(((avail - GAP) / 2.0).max(40.0));
    let cell = Vec2::new(col_w, GRID_MAX.y * col_w / GRID_MAX.x);
    let mut out = Vec::with_capacity(sizes.len());
    let mut y = 0.0;
    for row in sizes.chunks(2) {
        let fitted: Vec<Vec2> = row.iter().map(|s| fit_size(*s, cell)).collect();
        let row_h = fitted.iter().map(|v| v.y).fold(0.0, f32::max);
        let mut x = 0.0;
        for f in fitted {
            out.push(Rect::from_min_size(Pos2::new(x, y), f));
            x += cell.x + GAP;
        }
        y += row_h + GAP;
    }
    out
}

impl App {
    /// The chat the input belongs to right now (a channel or a DM shown in the centre).
    pub fn image_target(&self) -> Option<Target> {
        // (A channel docked beside the cards counts: its input is on screen.)
        self.chat_target_on_screen()
    }

    /// A hub snapshot arrived: ask what it accepts (a `404` means an older hub without images).
    pub fn images_on_ready(&mut self) {
        self.backend.send(Cmd::Image(ImageCmd::Limits));
    }

    /// Compress a file's bytes for `target` (picker, drop, pasted files, the autotest).
    pub fn attach_bytes(&mut self, target: Target, name: String, bytes: Vec<u8>, ctx: &egui::Context) {
        let q = self.settings.image_quality;
        self.attach_job(target, name, ctx, move |progress| imgs::compress(&bytes, q, progress));
    }

    /// Compress a decoded picture (a pasted bitmap).
    pub fn attach_picture(&mut self, target: Target, name: String, img: image::DynamicImage, bytes: u64, ctx: &egui::Context) {
        let q = self.settings.image_quality;
        self.attach_job(target, name, ctx, move |progress| imgs::compress_image(img, bytes, q, progress));
    }

    fn attach_job(&mut self, target: Target, name: String, ctx: &egui::Context, job: impl FnOnce(&dyn Fn(f32)) -> Result<Compressed, imgs::CompressError> + Send + 'static) {
        if !self.images.supported {
            return;
        }
        if self.images.pending_for(target) >= ATTACHMENT_MAX_PER_MESSAGE {
            self.toast(format!("No máximo {ATTACHMENT_MAX_PER_MESSAGE} imagens por mensagem."));
            return;
        }
        let ui = &mut self.images;
        let id = ui.next_id;
        ui.next_id += 1;
        let progress = Arc::new(AtomicU32::new(0));
        ui.pending.push(Pending { id, target, name: name.clone(), state: PendingState::Compressing, progress: progress.clone(), tex: None });
        let (tx, ctx) = (ui.done_tx.clone(), ctx.clone());
        let spawned = std::thread::Builder::new().name("image-compress".into()).spawn(move || {
            let report = |p: f32| {
                progress.store((p.clamp(0.0, 1.0) * 1000.0) as u32, Ordering::Relaxed);
                ctx.request_repaint();
            };
            let result = job(&report).map_err(|e| e.to_string());
            if let Err(e) = &result {
                tracing::info!(name, error = %e, "image not attached");
            }
            let _ = tx.send((id, result));
            ctx.request_repaint();
        });
        if spawned.is_err() {
            self.images.pending.retain(|p| p.id != id);
            self.notice = Some("Não foi possível compactar a imagem.".into());
        }
    }

    /// Read files (picker, drop, pasted from Explorer) and attach the pictures among them.
    pub fn attach_paths(&mut self, target: Target, paths: Vec<PathBuf>, ctx: &egui::Context) {
        let free = ATTACHMENT_MAX_PER_MESSAGE.saturating_sub(self.images.pending_for(target));
        if paths.len() > free {
            self.toast(format!("No máximo {ATTACHMENT_MAX_PER_MESSAGE} imagens por mensagem."));
        }
        for p in paths.into_iter().take(free) {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            match std::fs::metadata(&p) {
                Ok(m) if m.len() > imgs::MAX_INPUT_BYTES as u64 => {
                    self.notice = Some(format!("{name}: {}", imgs::CompressError::FileTooBig));
                    continue;
                }
                Err(e) => {
                    self.notice = Some(format!("Não foi possível ler {name}: {e}"));
                    continue;
                }
                _ => {}
            }
            match std::fs::read(&p) {
                Ok(bytes) if imgs::sniff(&bytes).is_some() => self.attach_bytes(target, name, bytes, ctx),
                Ok(_) => self.notice = Some(format!("{name} não é uma imagem que dê para enviar (PNG, JPEG, WebP, GIF ou BMP).")),
                Err(e) => self.notice = Some(format!("Não foi possível ler {name}: {e}")),
            }
        }
    }

    /// 📎: the native "Abrir" on its own thread.
    pub fn pick_images(&mut self, target: Target, ctx: &egui::Context) {
        if self.images.pick_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let (owner, ctx) = (self.video.parent, ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(crate::filedlg::open(owner, "Enviar imagens", crate::filedlg::IMAGE_FILTER, true));
            ctx.request_repaint();
        });
        self.images.pick_rx = Some((target, rx));
    }

    /// "Colar" in a text box's menu with a picture (or files) on the clipboard: as Ctrl+V.
    pub fn paste_image_from_menu(&mut self, target: Target, ctx: &egui::Context) {
        if self.images.supported {
            self.paste_from_clipboard(target, ctx);
        } else {
            self.toast("Este servidor não aceita imagens no chat.");
        }
    }

    /// Ctrl+V with a picture (or image files) on the clipboard.
    pub fn paste_from_clipboard(&mut self, target: Target, ctx: &egui::Context) {
        let Some(owner) = self.video.parent else { return };
        match crate::clipboard::read(owner) {
            crate::clipboard::Clip::Files(paths) => {
                let images: Vec<PathBuf> = paths.into_iter().filter(|p| is_image_name(p)).collect();
                if !images.is_empty() {
                    self.attach_paths(target, images, ctx);
                }
            }
            crate::clipboard::Clip::Encoded(bytes) => self.attach_bytes(target, "imagem colada".into(), bytes, ctx),
            crate::clipboard::Clip::Dib(dib) => {
                let q = self.settings.image_quality;
                self.attach_job(target, "imagem colada".into(), ctx, move |progress| {
                    let bytes = dib.len() as u64;
                    let img = crate::clipboard::dib_to_image(&dib).ok_or(imgs::CompressError::NotImage)?;
                    imgs::compress_image(img, bytes, q, progress)
                });
            }
            crate::clipboard::Clip::Text | crate::clipboard::Clip::Nothing => {}
        }
    }

    /// Remove a picture from the strip (✖).
    fn unattach(&mut self, id: u64) {
        self.images.pending.retain(|p| p.id != id);
    }

    /// "Enviar" with pictures waiting: upload them and send. Returns false when there are none
    /// for `target` (a plain text message then). `caption` is given back on failure.
    pub fn send_with_images(&mut self, target: Target, caption: String) -> bool {
        if self.images.pending_for(target) == 0 {
            return false;
        }
        if self.images.sending.is_some() {
            self.toast("Espere o envio anterior terminar.");
            return true;
        }
        if self.images.compressing(target) {
            self.toast("Espere a compactação das imagens terminar.");
            return true;
        }
        let images = self.images.ready_images(target);
        if images.is_empty() {
            self.toast(format!("Nenhuma imagem pronta: tire as que falharam ({}) e tente de novo.", ph::X));
            return true;
        }
        let token = self.images.next_token;
        self.images.next_token += 1;
        self.images.sending = Some(Sending { token, target, done: 0, total: images.len() });
        tracing::info!(images = images.len(), bytes = images.iter().map(|c| c.bytes.len()).sum::<usize>(), ?target, "sending a message with images");
        // The mentions and the reply go with it (the reply draft is used up now).
        let extras = self.take_send_extras(target, &caption);
        let seal = match target {
            Target::Dm(dm) => self.dm_send_mode(dm),
            Target::Channel(_) => None,
        };
        self.backend.send(Cmd::Image(ImageCmd::Send { token, target, caption: caption.clone(), images, extras, seal }));
        match target {
            Target::Channel(_) => self.input.clear(),
            Target::Dm(_) => self.dms.input.clear(),
        }
        self.images.caption_backup = Some((target, caption));
        true
    }

    /// The backend's answers about images.
    pub fn on_image_event(&mut self, ev: ImageEv) {
        match ev {
            ImageEv::Limits(AttachmentsReply::Ready(l)) => {
                tracing::info!(daily_left = l.daily_bytes_left, storage_full = l.storage_full, "the hub accepts chat images");
                self.images.supported = true;
                self.images.limits = Some(l);
            }
            ImageEv::Limits(AttachmentsReply::Unsupported) => {
                tracing::info!("the hub has no chat images (older server): the attach button is hidden");
                self.images.supported = false;
            }
            ImageEv::Limits(AttachmentsReply::Failed(e)) => tracing::warn!(error = %e, "image limits failed"),
            ImageEv::Fetched { key, thumb, result } => self.images.store.on_fetched(key, thumb, result),
            ImageEv::Uploaded { token, done, total } => {
                if let Some(s) = self.images.sending.as_mut().filter(|s| s.token == token) {
                    s.done = done;
                    s.total = total;
                }
            }
            ImageEv::Sent { token, target, result } => {
                if self.images.sending.as_ref().is_none_or(|s| s.token != token) {
                    return;
                }
                self.images.sending = None;
                let caption = self.images.caption_backup.take().filter(|(t, _)| *t == target).map(|(_, c)| c);
                match result {
                    Ok(sent) => {
                        self.images.sent += 1;
                        if let SentMessage::Channel(m) = &sent {
                            tracing::info!(message = m.id, images = m.attachments.len(), "message with images sent");
                        }
                        self.images.pending.retain(|p| p.target != target);
                        if let SentMessage::Dm(m) = sent {
                            let m = *m;
                            if let Some(me) = self.my_id() {
                                crate::app::dms::apply_message(&mut self.dms.convs, &m, me, true);
                            }
                            let dm = m.dm_id;
                            let log = self.dms.logs.entry(dm).or_default();
                            if log.requested {
                                crate::app::dms::insert_line(&mut log.msgs, m);
                            }
                        }
                    }
                    Err(e) => {
                        // Keep the pictures and give the caption back.
                        if let Some(c) = caption {
                            let input = match target {
                                Target::Channel(_) => &mut self.input,
                                Target::Dm(_) => &mut self.dms.input,
                            };
                            if input.is_empty() {
                                *input = c;
                            }
                        }
                        // (E2: refused to go out in the clear: the question instead.)
                        let asked = match target {
                            Target::Dm(dm) => self.dm_refused_plain(dm, &e),
                            Target::Channel(_) => false,
                        };
                        if !asked {
                            self.notice = Some(format!("Imagens não enviadas: {e}"));
                        }
                    }
                }
            }
        }
    }

    /// Every frame: compression results, the file picker, saving, Ctrl+V and drops.
    pub fn images_tick(&mut self, ctx: &egui::Context) {
        while let Ok((id, result)) = self.images.done_rx.try_recv() {
            let Some(p) = self.images.pending.iter_mut().find(|p| p.id == id) else { continue };
            match result {
                Ok(c) => {
                    let img = egui::ColorImage::from_rgba_unmultiplied([c.preview.w as usize, c.preview.h as usize], &c.preview.rgba);
                    p.tex = Some(ctx.load_texture(format!("attach-preview-{id}"), img, TextureOptions::LINEAR));
                    p.state = PendingState::Ready(Arc::new(c));
                    self.images.compressed += 1;
                }
                Err(e) => p.state = PendingState::Failed(e),
            }
        }
        if self.images.pending.iter().any(|p| matches!(p.state, PendingState::Compressing)) {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
        if let Some((target, rx)) = &self.images.pick_rx {
            let target = *target;
            match rx.try_recv() {
                Ok(r) => {
                    self.images.pick_rx = None;
                    match r {
                        Ok(paths) => self.attach_paths(target, paths, ctx),
                        Err(e) => self.notice = Some(e),
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => self.images.pick_rx = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.images.save_rx {
            match rx.try_recv() {
                Ok(r) => {
                    self.images.save_rx = None;
                    let msg = match r {
                        Ok(Some(p)) => (true, format!("Salva em {}", p.display())),
                        Ok(None) => (true, String::new()),
                        Err(e) => (false, e),
                    };
                    if let Some(v) = self.images.viewer.as_mut() {
                        v.msg = (!msg.1.is_empty()).then_some(msg);
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => self.images.save_rx = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        // Ctrl+V and drops go to the chat on screen (not while a dialog or the viewer is up, and
        // not while another text box has the keyboard).
        let paste = crate::clipboard::take_paste();
        let target = self.image_target().filter(|_| self.images.supported && self.images.viewer.is_none() && !self.settings_ui.open && self.sb.add.is_none());
        self.images.drop_hover = target.is_some() && ctx.input(|i| !i.raw.hovered_files.is_empty());
        let Some(target) = target else { return };
        if paste {
            let focus = ctx.memory(|m| m.focused());
            let ours = [egui::Id::new("chat_input_box"), egui::Id::new("dm_input_box")];
            if focus.is_none_or(|f| ours.contains(&f)) {
                self.paste_from_clipboard(target, ctx);
            }
        }
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if !dropped.is_empty() {
            let images: Vec<PathBuf> = dropped.into_iter().filter(|p| is_image_name(p)).collect();
            if images.is_empty() {
                self.toast("Só dá para enviar imagens (PNG, JPEG, WebP, GIF ou BMP).");
            } else {
                self.attach_paths(target, images, ctx);
            }
        }
    }

    /// 📎 next to the input (hidden on an older hub).
    pub fn attach_button(&mut self, ui: &mut egui::Ui, target: Target) {
        if !self.images.supported {
            return;
        }
        let full = self.images.pending_for(target) >= ATTACHMENT_MAX_PER_MESSAGE;
        let enabled = !full && self.images.pick_rx.is_none();
        let r = ui.add_enabled_ui(enabled, |ui| crate::ui_kit::icon_button(ui, ph::PLUS_CIRCLE, "Enviar imagens (até 4). Também dá para colar com Ctrl+V ou arrastar para o chat.", crate::ui_kit::IconState::Normal)).inner;
        if r.clicked() {
            let ctx = ui.ctx().clone();
            self.pick_images(target, &ctx);
        }
    }

    /// The strip of pictures waiting to be sent, above the input.
    pub fn attach_strip(&mut self, ui: &mut egui::Ui, target: Target) {
        if self.images.pending_for(target) == 0 && !self.images.drop_hover {
            return;
        }
        let sending = self.images.sending.as_ref().filter(|s| s.target == target).map(|s| (s.done, s.total));
        let mut remove = None;
        ui.add_space(4.0);
        if self.images.drop_hover {
            ui.label(RichText::new("Solte para anexar as imagens").color(ui.visuals().hyperlink_color));
        }
        egui::ScrollArea::horizontal().id_salt("attach_strip").show(ui, |ui| {
            ui.horizontal(|ui| {
                for p in self.images.pending.iter().filter(|p| p.target == target) {
                    let frame = egui::Frame::new().fill(ui.visuals().extreme_bg_color).corner_radius(ROUND).inner_margin(4.0);
                    frame.show(ui, |ui| {
                        ui.set_width(CARD + 8.0);
                        ui.vertical(|ui| {
                            let (rect, _) = ui.allocate_exact_size(Vec2::splat(CARD), Sense::hover());
                            match (&p.state, &p.tex) {
                                (PendingState::Ready(_), Some(t)) => {
                                    let s = fit_size((t.size()[0] as u32, t.size()[1] as u32), Vec2::splat(CARD));
                                    let r = Rect::from_center_size(rect.center(), s);
                                    egui::Image::new(egui::load::SizedTexture::new(t.id(), s)).corner_radius(4.0).paint_at(ui, r);
                                }
                                (PendingState::Failed(_), _) => {
                                    ui.painter().rect_filled(rect, 4.0, ui.visuals().faint_bg_color);
                                    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, ph::WARNING, egui::FontId::proportional(28.0), crate::theme::AVISO);
                                }
                                _ => {
                                    ui.painter().rect_filled(rect, 4.0, ui.visuals().faint_bg_color);
                                    let pct = p.progress.load(Ordering::Relaxed) as f32 / 1000.0;
                                    let bar = Rect::from_min_size(rect.left_bottom() - Vec2::new(-6.0, 12.0), Vec2::new((CARD - 12.0) * pct, 4.0));
                                    ui.painter().rect_filled(bar, 2.0, ui.visuals().hyperlink_color);
                                    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, format!("{:.0}%", pct * 100.0), egui::FontId::proportional(13.0), ui.visuals().weak_text_color());
                                }
                            }
                            if sending.is_none() {
                                let x = Rect::from_min_size(rect.right_top() + Vec2::new(-22.0, 2.0), Vec2::splat(20.0));
                                let r = ui.interact(x, ui.id().with(("unattach", p.id)), Sense::click()).on_hover_text("Tirar esta imagem");
                                let fill = if r.hovered() { Color32::from_black_alpha(220) } else { Color32::from_black_alpha(160) };
                                ui.painter().circle_filled(x.center(), 10.0, fill);
                                ui.painter().text(x.center(), egui::Align2::CENTER_CENTER, ph::X, egui::FontId::proportional(11.0), Color32::WHITE);
                                if r.clicked() {
                                    remove = Some(p.id);
                                }
                            }
                            let weak = ui.visuals().weak_text_color();
                            let line = match &p.state {
                                PendingState::Compressing => size_line("Comprimindo…", None, weak),
                                PendingState::Ready(c) => size_line(&human_size(c.original_bytes), Some(&human_size(c.bytes.len() as u64)), weak),
                                PendingState::Failed(_) => size_line("Falhou", None, crate::theme::AVISO),
                            };
                            let tip = match &p.state {
                                PendingState::Ready(c) => format!(
                                    "{}\n{}x{} para {}x{} {}{}",
                                    p.name,
                                    c.original_w,
                                    c.original_h,
                                    c.width,
                                    c.height,
                                    c.encoding.label(),
                                    if c.screenshot { " (print: texto nítido)" } else { "" }
                                ),
                                PendingState::Failed(e) => format!("{}\n{e}", p.name),
                                PendingState::Compressing => p.name.clone(),
                            };
                            ui.label(line).on_hover_text(tip);
                        });
                    });
                }
                if let Some((done, total)) = sending {
                    ui.label(RichText::new(format!("Enviando… {done}/{total}")).weak());
                }
            });
        });
        if let Some(id) = remove {
            self.unattach(id);
        }
    }

    /// Open the viewer on `items[index]`.
    pub fn open_viewer(&mut self, items: Vec<Attachment>, index: usize) {
        if items.is_empty() {
            return;
        }
        let index = index.min(items.len() - 1);
        tracing::info!(attachment = items[index].id, of = items.len(), "image viewer opened");
        self.images.viewer = Some(Viewer { items, index, zoom: 1.0, pan: Vec2::ZERO, frame: 0, frame_at: Instant::now(), msg: None });
    }

    pub fn close_viewer(&mut self) {
        self.images.viewer = None;
        self.images.store.free_full_except(&[]);
    }

    /// These images are gone (their message or channel was deleted): drop everything cached for
    /// them, and close the viewer if it shows one of them.
    pub fn forget_images(&mut self, atts: &[Attachment]) {
        if atts.is_empty() {
            return;
        }
        let keys: Vec<ImgKey> = atts.iter().map(ImgKey::of).collect();
        self.images.store.forget(&keys);
        self.images.allowed.retain(|k| !keys.contains(k));
        if self.images.viewer.as_ref().is_some_and(|v| v.items.iter().any(|a| keys.contains(&ImgKey::of(a)))) {
            tracing::info!("the image in the viewer was deleted: viewer closed");
            self.close_viewer();
        }
    }

    /// "Salvar imagem…": the stored file, as it is.
    fn save_current(&mut self) {
        let Some(v) = &self.images.viewer else { return };
        let a = v.items[v.index].clone();
        let Some(bytes) = self.images.store.full_bytes(ImgKey::of(&a)) else { return };
        let ext = match a.mime.as_str() {
            "image/jpeg" => "jpg",
            "image/png" => "png",
            "image/gif" => "gif",
            _ => "webp",
        };
        let (tx, rx) = mpsc::channel();
        let owner = self.video.parent;
        std::thread::spawn(move || {
            let filter = (format!("Imagem (*.{ext})"), format!("*.{ext}"));
            let r = crate::filedlg::save(owner, "Salvar imagem", &format!("imagem-{}.{ext}", a.id), (&filter.0, &filter.1), ext)
                .and_then(|p| match p {
                    Some(p) => std::fs::write(&p, &*bytes).map(|_| Some(p)).map_err(|e| format!("Não foi possível salvar: {e}")),
                    None => Ok(None),
                });
            let _ = tx.send(r);
        });
        self.images.save_rx = Some(rx);
    }

    /// "Copiar": the picture (first frame) on the clipboard.
    fn copy_current(&mut self) {
        let Some(v) = &self.images.viewer else { return };
        let key = ImgKey::of(&v.items[v.index]);
        let (Some(rgba), Some(owner)) = (self.images.store.full_rgba(key), self.video.parent) else { return };
        let r = crate::clipboard::write_image(owner, &rgba);
        if let Some(v) = self.images.viewer.as_mut() {
            v.msg = Some(match r {
                Ok(()) => (true, "Imagem copiada.".to_string()),
                Err(e) => (false, e),
            });
        }
    }

    /// The overlay itself (drawn last, over everything).
    pub fn image_viewer(&mut self, ctx: &egui::Context) {
        let Some(mut v) = self.images.viewer.take() else { return };
        // The chat box under the viewer must not take the keys (typing, arrows).
        ctx.memory_mut(|m| {
            if let Some(f) = m.focused() {
                m.surrender_focus(f);
            }
        });
        let id = ImgKey::of(&v.items[v.index]);
        // The current image, and its neighbours ready for the arrows; everything else freed.
        let keep: Vec<ImgKey> = [v.index.checked_sub(1), Some(v.index), Some(v.index + 1)].into_iter().flatten().filter_map(|i| v.items.get(i).map(ImgKey::of)).collect();
        self.images.store.free_full_except(&keep);
        for k in &keep {
            self.images.store.want_full(*k);
        }
        self.images.store.pump_full_decodes(ctx);
        let screen = ctx.content_rect();
        let mut close = false;
        let (mut prev, mut next, mut save, mut copy) = (false, false, false, false);
        ctx.input_mut(|i| {
            close |= i.consume_key(egui::Modifiers::NONE, egui::Key::Escape);
            prev |= i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft);
            next |= i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight);
        });
        let a = v.items[v.index].clone();
        egui::Area::new(egui::Id::new("image_viewer")).order(egui::Order::Foreground).fixed_pos(screen.min).show(ctx, |ui| {
            let (rect, bg) = ui.allocate_exact_size(screen.size(), Sense::click_and_drag());
            ui.painter().rect_filled(rect, 0.0, Color32::from_black_alpha(225));
            let bar_h = 44.0;
            let area = Rect::from_min_max(rect.min + Vec2::new(16.0, 16.0), rect.max - Vec2::new(16.0, bar_h + 16.0));
            let fit = fit_size((a.width, a.height), area.size());
            let scale = fit.x / a.width.max(1) as f32;
            // Wheel: zoom around the pointer; drag: pan; double click: 1:1 <-> fit.
            if bg.hovered() {
                let (scroll, pinch, pointer) = ui.input(|i| (i.smooth_scroll_delta().y, i.zoom_delta(), i.pointer.hover_pos()));
                let factor = pinch * (scroll / 300.0).exp();
                if (factor - 1.0).abs() > 1e-4 {
                    let old = v.zoom;
                    v.zoom = (v.zoom * factor).clamp(1.0, 16.0 / scale.max(0.01));
                    if let Some(p) = pointer {
                        let c = area.center() + v.pan;
                        v.pan += (p - c) * (1.0 - v.zoom / old);
                    }
                }
            }
            if bg.dragged() {
                v.pan += bg.drag_delta();
            }
            if bg.double_clicked() {
                v.zoom = if v.zoom > 1.01 { 1.0 } else { (1.0 / scale).max(1.0) };
                v.pan = Vec2::ZERO;
            }
            if v.zoom <= 1.0 {
                v.pan = Vec2::ZERO;
            }
            let size = fit * v.zoom;
            let img_rect = Rect::from_center_size(area.center() + v.pan, size);
            match self.images.store.full_frame(id, &mut v.frame, &mut v.frame_at, ctx) {
                Some(tex) => {
                    let painter = ui.painter().with_clip_rect(area.expand(16.0));
                    painter.image(tex, img_rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                }
                None => {
                    // The thumbnail, stretched, while the full image loads.
                    if let Some(t) = self.images.store.thumb_texture(id) {
                        ui.painter().with_clip_rect(area).image(t.id(), img_rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::from_gray(160));
                    }
                    let text = if self.images.store.full_failed(id) { "Não foi possível carregar a imagem." } else { "Carregando…" };
                    ui.painter().text(area.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(16.0), Color32::from_gray(220));
                }
            }
            // A click outside the picture (not a drag) closes.
            if bg.clicked() && !img_rect.contains(bg.interact_pointer_pos().unwrap_or(Pos2::ZERO)) {
                close = true;
            }
            // The bar.
            let bar = Rect::from_min_max(Pos2::new(rect.left(), rect.bottom() - bar_h), rect.max);
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(bar.shrink2(Vec2::new(16.0, 6.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
            // (The kit's buttons, like the rest of the app.)
            use crate::ui_kit::{self, IconState, Kind, Size};
            child.spacing_mut().item_spacing.x = crate::theme::space::M;
            if v.items.len() > 1 {
                prev |= child.add_enabled_ui(v.index > 0, |ui| ui_kit::icon_button(ui, ph::CARET_LEFT, "Anterior", IconState::Normal)).inner.clicked();
                child.label(RichText::new(format!("{} / {}", v.index + 1, v.items.len())).color(crate::theme::GIZ));
                next |= child.add_enabled_ui(v.index + 1 < v.items.len(), |ui| ui_kit::icon_button(ui, ph::CARET_RIGHT, "Próxima", IconState::Normal)).inner.clicked();
                child.add_space(12.0);
            }
            child.label(RichText::new(format!("{}x{} · {}{}", a.width, a.height, human_size(a.size), if a.animated { " · animada" } else { "" })).color(crate::theme::NEVOA));
            child.add_space(12.0);
            let loaded = self.images.store.full_bytes(id).is_some();
            let can_save = loaded && self.images.save_rx.is_none();
            save |= child.add_enabled_ui(can_save, |ui| ui_kit::button_sized(ui, Kind::Secondary, Size::Small, Some(ph::FOLDER_OPEN), "Salvar imagem…")).inner.clicked();
            copy |= child.add_enabled_ui(loaded, |ui| ui_kit::button_sized(ui, Kind::Secondary, Size::Small, Some(ph::COPY), "Copiar")).inner.clicked();
            if v.zoom > 1.01 {
                if ui_kit::button_sized(&mut child, Kind::Ghost, Size::Small, Some(ph::CORNERS_IN), "Ajustar à janela").clicked() {
                    v.zoom = 1.0;
                    v.pan = Vec2::ZERO;
                }
                child.label(RichText::new(format!("{:.0}%", scale * v.zoom * 100.0)).color(crate::theme::NEVOA));
            }
            if let Some((ok, m)) = &v.msg {
                child.label(RichText::new(m).color(if *ok { crate::app::GREEN } else { crate::theme::AVISO }));
            }
            child.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close |= ui_kit::button_sized(ui, Kind::Secondary, Size::Small, Some(ph::X), "Fechar").on_hover_text("Esc").clicked();
                ui.label(RichText::new("roda: zoom · arrastar: mover · duplo clique: 100%").small().color(crate::theme::NEVOA));
            });
        });
        if prev && v.index > 0 {
            v.index -= 1;
            v.zoom = 1.0;
            v.pan = Vec2::ZERO;
            v.frame = 0;
            v.msg = None;
        }
        if next && v.index + 1 < v.items.len() {
            v.index += 1;
            v.zoom = 1.0;
            v.pan = Vec2::ZERO;
            v.frame = 0;
            v.msg = None;
        }
        self.images.viewer = Some(v);
        if save {
            self.save_current();
        }
        if copy {
            self.copy_current();
        }
        if close {
            self.close_viewer();
        }
        self.pump_images();
    }

    /// Send the backend whatever the store wants downloaded now.
    pub fn pump_images(&mut self) {
        for (key, thumb) in self.images.store.take_requests() {
            self.backend.send(Cmd::Image(ImageCmd::Fetch { key, thumb }));
        }
    }
}

/// "4,8 MB → 312 KB" in small text (the arrow comes from the monospace font: the proportional
/// one has none).
fn size_line(before: &str, after: Option<&str>, color: Color32) -> egui::text::LayoutJob {
    let small = |font: egui::FontId| egui::TextFormat { font_id: font, color, ..Default::default() };
    let mut job = egui::text::LayoutJob::default();
    job.append(before, 0.0, small(egui::FontId::proportional(11.0)));
    if let Some(after) = after {
        job.append(" → ", 0.0, small(egui::FontId::monospace(11.0)));
        job.append(after, 0.0, small(egui::FontId::proportional(11.0)));
    }
    job
}

fn is_image_name(p: &std::path::Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "jfif" | "webp" | "gif" | "bmp"))
}

/// Draw a message's images under its text. `on_screen`: thumbnails may be asked for. Returns
/// the index clicked (to open the viewer).
pub fn show_attachments(ui: &mut egui::Ui, images: &mut ImagesUi, atts: &[Attachment], auto: bool, on_screen: bool) -> Option<usize> {
    if atts.is_empty() {
        return None;
    }
    let sizes: Vec<(u32, u32)> = atts.iter().map(|a| if a.thumb_w > 0 && a.thumb_h > 0 { (a.thumb_w, a.thumb_h) } else { (a.width, a.height) }).collect();
    // Display size follows the full image's shape at the thumbnail's resolution.
    let rects = layout(&sizes, ui.available_width());
    let total = rects.iter().fold(Rect::NOTHING, |acc, r| acc.union(*r));
    ui.add_space(2.0);
    let (outer, _) = ui.allocate_exact_size(total.size(), Sense::hover());
    let now = Instant::now();
    let mut clicked = None;
    for (i, (a, r)) in atts.iter().zip(&rects).enumerate() {
        let rect = r.translate(outer.min.to_vec2());
        let visible = ui.is_rect_visible(rect);
        let key = ImgKey::of(a);
        let allowed = auto || images.allowed.contains(&key);
        let resp = ui.interact(rect, ui.id().with(("attachment", a.id, i)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
        if visible && allowed && on_screen {
            images.store.want_thumb(key, now);
        }
        match images.store.thumb_texture_for(ui.ctx(), key, now).filter(|_| allowed) {
            Some(tex) => {
                egui::Image::new(egui::load::SizedTexture::new(tex, rect.size())).corner_radius(ROUND).paint_at(ui, rect);
                if a.animated {
                    let badge = Rect::from_min_size(rect.left_bottom() + Vec2::new(6.0, -22.0), Vec2::new(34.0, 16.0));
                    ui.painter().rect_filled(badge, 3.0, Color32::from_black_alpha(170));
                    // ("GIF", or a play icon for the other animated formats: no all-caps made-up tag.)
                    let (label, font) = if a.mime == "image/gif" { ("GIF", crate::theme::semibold(11.0)) } else { (ph::PLAY, crate::theme::body(12.0)) };
                    ui.painter().text(badge.center(), egui::Align2::CENTER_CENTER, label, font, Color32::WHITE);
                }
                if resp.clicked() {
                    clicked = Some(i);
                }
            }
            None => {
                ui.painter().rect_filled(rect, ROUND, ui.visuals().faint_bg_color);
                let text = if images.store.thumb_gone(key) {
                    "imagem removida".to_string()
                } else if allowed {
                    ph::IMAGE.to_string()
                } else {
                    format!("{} Imagem · {}\nclique para carregar", ph::IMAGE, human_size(a.size))
                };
                ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(13.0), ui.visuals().weak_text_color());
                if resp.clicked() {
                    if allowed {
                        clicked = Some(i);
                    } else {
                        images.allowed.insert(key);
                    }
                }
            }
        }
        resp.on_hover_text(format!("{}x{} · {}", a.width, a.height, human_size(a.size)));
    }
    ui.add_space(2.0);
    clicked
}

// ---------------------------------------------------------------- the store

enum Bytes {
    Loading,
    Ready(Vec<u8>),
    Gone,
    Retry(Instant),
}

struct ThumbEntry {
    bytes: Bytes,
    tex: Option<TextureHandle>,
    last_seen: Instant,
}

enum FullState {
    Loading,
    Decoding(Arc<Vec<u8>>),
    Ready { bytes: Arc<Vec<u8>>, frames: Vec<(TextureHandle, u32)>, first: Arc<image::RgbaImage> },
    Failed,
}

/// What a decoding thread hands back: frames (RGBA, delay ms).
type Decoded = (ImgKey, Result<Vec<(image::RgbaImage, u32)>, String>);

/// Downloads, bytes and textures of the chat's images, keyed by id and content ([`ImgKey`]).
pub struct ImageStore {
    thumbs: HashMap<ImgKey, ThumbEntry>,
    full: HashMap<ImgKey, FullState>,
    queue: VecDeque<(ImgKey, bool)>,
    in_flight: usize,
    decodes_left: u32,
    last_sweep: Instant,
    dec_tx: mpsc::Sender<Decoded>,
    dec_rx: mpsc::Receiver<Decoded>,
}

impl Default for ImageStore {
    fn default() -> Self {
        let (dec_tx, dec_rx) = mpsc::channel();
        ImageStore { thumbs: HashMap::new(), full: HashMap::new(), queue: VecDeque::new(), in_flight: 0, decodes_left: DECODES_PER_FRAME, last_sweep: Instant::now(), dec_tx, dec_rx }
    }
}

impl ImageStore {
    /// Call once per frame: refills the decode budget, sweeps now and then.
    pub fn begin_frame(&mut self, now: Instant) {
        self.decodes_left = DECODES_PER_FRAME;
        if now.duration_since(self.last_sweep) >= Duration::from_secs(5) {
            self.last_sweep = now;
            self.sweep(now);
        }
    }

    /// This thumbnail is on screen: make sure it gets downloaded.
    pub fn want_thumb(&mut self, id: ImgKey, now: Instant) {
        match self.thumbs.get_mut(&id) {
            None => {
                self.thumbs.insert(id, ThumbEntry { bytes: Bytes::Loading, tex: None, last_seen: now });
                self.queue.push_back((id, true));
            }
            Some(e) => {
                e.last_seen = now;
                if matches!(e.bytes, Bytes::Retry(at) if now >= at) {
                    e.bytes = Bytes::Loading;
                    self.queue.push_back((id, true));
                }
            }
        }
    }

    pub fn want_full(&mut self, id: ImgKey) {
        if let std::collections::hash_map::Entry::Vacant(e) = self.full.entry(id) {
            e.insert(FullState::Loading);
            // The viewer's image goes before any thumbnail.
            self.queue.push_front((id, false));
        }
    }

    pub fn take_requests(&mut self) -> Vec<(ImgKey, bool)> {
        let mut out = Vec::new();
        while self.in_flight < MAX_IN_FLIGHT {
            let Some(r) = self.queue.pop_front() else { break };
            self.in_flight += 1;
            out.push(r);
        }
        out
    }

    pub fn on_fetched(&mut self, id: ImgKey, thumb: bool, result: Result<Vec<u8>, FetchError>) {
        self.in_flight = self.in_flight.saturating_sub(1);
        if thumb {
            let Some(e) = self.thumbs.get_mut(&id) else { return };
            e.bytes = match result {
                Ok(b) => Bytes::Ready(b),
                Err(FetchError::Gone) => Bytes::Gone,
                Err(FetchError::Failed(err)) => {
                    tracing::debug!(id = id.id, error = %err, "thumbnail download failed");
                    Bytes::Retry(Instant::now() + RETRY_AFTER)
                }
            };
            return;
        }
        let Some(state) = self.full.get_mut(&id) else { return };
        match result {
            Ok(bytes) => {
                let bytes = Arc::new(bytes);
                *state = FullState::Decoding(bytes.clone());
                let tx = self.dec_tx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send((id, decode_full(&bytes)));
                });
            }
            Err(e) => {
                tracing::info!(id = id.id, error = ?e, "image download failed");
                *state = FullState::Failed;
            }
        }
    }

    /// Turn finished decodes into textures.
    pub fn pump_full_decodes(&mut self, ctx: &egui::Context) {
        while let Ok((id, result)) = self.dec_rx.try_recv() {
            let Some(state) = self.full.get_mut(&id) else { continue };
            let FullState::Decoding(bytes) = state else { continue };
            let bytes = bytes.clone();
            *state = match result {
                Ok(frames) if !frames.is_empty() => {
                    let first = Arc::new(frames[0].0.clone());
                    let texs = frames
                        .into_iter()
                        .enumerate()
                        .map(|(i, (f, d))| {
                            let img = egui::ColorImage::from_rgba_unmultiplied([f.width() as usize, f.height() as usize], f.as_raw());
                            (ctx.load_texture(format!("attachment-{}-{:016x}-{i}", id.id, id.content), img, TextureOptions::LINEAR), d)
                        })
                        .collect();
                    tracing::info!(id = id.id, w = first.width(), h = first.height(), "image shown in the viewer");
                    FullState::Ready { bytes, frames: texs, first }
                }
                Ok(_) | Err(_) => FullState::Failed,
            };
        }
        if self.full.values().any(|s| matches!(s, FullState::Loading | FullState::Decoding(_))) {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// The viewer's texture now (advancing an animation).
    fn full_frame(&mut self, id: ImgKey, frame: &mut usize, at: &mut Instant, ctx: &egui::Context) -> Option<egui::TextureId> {
        let FullState::Ready { frames, .. } = self.full.get(&id)? else { return None };
        if frames.len() > 1 {
            *frame %= frames.len();
            let delay = Duration::from_millis(u64::from(frames[*frame].1.max(20)));
            let elapsed = at.elapsed();
            if elapsed >= delay {
                *frame = (*frame + 1) % frames.len();
                *at = Instant::now();
            }
            ctx.request_repaint_after(delay.saturating_sub(at.elapsed()));
        }
        frames.get(*frame).map(|(t, _)| t.id())
    }

    fn full_failed(&self, id: ImgKey) -> bool {
        matches!(self.full.get(&id), Some(FullState::Failed))
    }

    /// The stored file of a viewed image (for "Salvar imagem…").
    pub fn full_bytes(&self, id: ImgKey) -> Option<Arc<Vec<u8>>> {
        match self.full.get(&id)? {
            FullState::Ready { bytes, .. } => Some(bytes.clone()),
            _ => None,
        }
    }

    /// The first frame of a viewed image (for "Copiar").
    pub fn full_rgba(&self, id: ImgKey) -> Option<Arc<image::RgbaImage>> {
        match self.full.get(&id)? {
            FullState::Ready { first, .. } => Some(first.clone()),
            _ => None,
        }
    }

    /// The viewer shows an image fully decoded: its pixel size.
    pub fn full_size(&self, id: ImgKey) -> Option<(u32, u32)> {
        match self.full.get(&id)? {
            FullState::Ready { first, .. } => Some((first.width(), first.height())),
            _ => None,
        }
    }

    pub fn free_full_except(&mut self, keep: &[ImgKey]) {
        self.full.retain(|id, s| keep.contains(id) || matches!(s, FullState::Loading));
    }

    /// Drop everything known about these images (deleted): bytes, textures, queued downloads.
    /// An answer still on its way finds no entry and is ignored.
    pub fn forget(&mut self, keys: &[ImgKey]) {
        self.thumbs.retain(|k, _| !keys.contains(k));
        self.full.retain(|k, _| !keys.contains(k));
        // (Queued requests were not sent yet, so nothing is in flight for them.)
        self.queue.retain(|(k, _)| !keys.contains(k));
    }

    /// Is anything known about this image (autotest, tests)?
    pub fn knows(&self, id: ImgKey) -> bool {
        self.thumbs.contains_key(&id) || self.full.contains_key(&id)
    }

    fn thumb_texture(&self, id: ImgKey) -> Option<&TextureHandle> {
        self.thumbs.get(&id)?.tex.as_ref()
    }

    pub fn thumb_gone(&self, id: ImgKey) -> bool {
        self.thumbs.get(&id).is_some_and(|e| matches!(e.bytes, Bytes::Gone))
    }

    /// Has the thumbnail been turned into a texture (autotest)?
    pub fn thumb_shown(&self, id: ImgKey) -> bool {
        self.thumbs.get(&id).is_some_and(|e| e.tex.is_some())
    }

    /// The downloaded thumbnail file (autotest: compare it with what the hub serves now).
    pub fn thumb_bytes(&self, id: ImgKey) -> Option<&[u8]> {
        match &self.thumbs.get(&id)?.bytes {
            Bytes::Ready(b) => Some(b),
            _ => None,
        }
    }

    /// The thumbnail texture to draw, created from the kept bytes when needed.
    fn thumb_texture_for(&mut self, ctx: &egui::Context, id: ImgKey, now: Instant) -> Option<egui::TextureId> {
        let e = self.thumbs.get_mut(&id)?;
        e.last_seen = now;
        if e.tex.is_none() {
            if let Bytes::Ready(b) = &e.bytes {
                if self.decodes_left == 0 {
                    ctx.request_repaint();
                    return None;
                }
                self.decodes_left -= 1;
                match decode_thumb(b) {
                    Some(img) => e.tex = Some(ctx.load_texture(format!("attachment-thumb-{}-{:016x}", id.id, id.content), img, TextureOptions::LINEAR)),
                    None => e.bytes = Bytes::Gone,
                }
            }
        }
        e.tex.as_ref().map(TextureHandle::id)
    }

    /// Free idle textures; keep the thumbnail bytes under their budget (oldest first).
    pub fn sweep(&mut self, now: Instant) {
        for e in self.thumbs.values_mut() {
            if e.tex.is_some() && now.duration_since(e.last_seen) > TEXTURE_IDLE {
                e.tex = None;
            }
        }
        let size = |e: &ThumbEntry| if let Bytes::Ready(b) = &e.bytes { b.len() } else { 0 };
        let mut total: usize = self.thumbs.values().map(size).sum();
        if total > MAX_THUMB_BYTES {
            let mut old: Vec<(Instant, ImgKey)> = self.thumbs.iter().filter(|(_, e)| e.tex.is_none() && size(e) > 0).map(|(k, e)| (e.last_seen, *k)).collect();
            old.sort();
            for (_, k) in old {
                if total <= MAX_THUMB_BYTES {
                    break;
                }
                if let Some(e) = self.thumbs.remove(&k) {
                    total -= size(&e);
                }
            }
        }
    }

    /// (thumbnails known, textures alive, full images held) for logs and tests.
    pub fn stats(&self) -> (usize, usize, usize) {
        (self.thumbs.len(), self.thumbs.values().filter(|e| e.tex.is_some()).count(), self.full.len())
    }
}

/// Decode a thumbnail (small; anything bigger than the hub promises is refused).
pub fn decode_thumb(bytes: &[u8]) -> Option<egui::ColorImage> {
    // The size limit applies before decoding: a sealed (E2) thumbnail was never checked by the hub,
    // and a small file can claim a huge picture (the decoder would allocate for all of it).
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_THUMB_SIDE);
    limits.max_image_height = Some(MAX_THUMB_SIDE);
    limits.max_alloc = Some(64 << 20);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    if img.width() == 0 || img.height() == 0 || img.width() > MAX_THUMB_SIDE || img.height() > MAX_THUMB_SIDE {
        return None;
    }
    let rgba = img.to_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw()))
}

/// Decode a full image for the viewer: every frame of an animation (within a budget, else the
/// first), with each frame's delay.
pub fn decode_full(bytes: &[u8]) -> Result<Vec<(image::RgbaImage, u32)>, String> {
    use image::AnimationDecoder;
    let fmt = image::guess_format(bytes).map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    let frames = match fmt {
        image::ImageFormat::Gif => {
            let mut d = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
            image::ImageDecoder::set_limits(&mut d, limits.clone()).map_err(|e| e.to_string())?;
            Some(d.into_frames())
        }
        image::ImageFormat::WebP => {
            let mut d = image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
            image::ImageDecoder::set_limits(&mut d, limits.clone()).map_err(|e| e.to_string())?;
            if d.has_animation() { Some(d.into_frames()) } else { None }
        }
        _ => None,
    };
    if let Some(frames) = frames {
        let mut out = Vec::new();
        let mut total = 0u64;
        for f in frames {
            let f = f.map_err(|e| e.to_string())?;
            let (n, d) = f.delay().numer_denom_ms();
            let delay = n.checked_div(d).map_or(100, |ms| ms.max(20));
            let buf = f.into_buffer();
            total += buf.as_raw().len() as u64;
            out.push((buf, delay));
            if total > MAX_VIEWER_DECODED {
                out.truncate(1);
                break;
            }
        }
        if !out.is_empty() {
            return Ok(out);
        }
    }
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), fmt);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| e.to_string())?;
    Ok(vec![(img.to_rgba8(), 0)])
}

/// The viewer's state.
pub struct Viewer {
    pub items: Vec<Attachment>,
    pub index: usize,
    /// 1 = fitted to the window.
    pub zoom: f32,
    pub pan: Vec2,
    frame: usize,
    frame_at: Instant,
    /// (ok, text) under the picture: saved, copied, or what failed.
    msg: Option<(bool, String)>,
}

impl Viewer {
    pub fn current(&self) -> &Attachment {
        &self.items[self.index]
    }
}

/// The "Qualidade das imagens enviadas" choices, for the settings dialog.
pub fn quality_choices() -> [(Quality, &'static str); 3] {
    Quality::ALL.map(|q| (q, q.label()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_the_brazilian_way() {
        assert_eq!(human_size(5_033_164), "4,8 MB");
        assert_eq!(human_size(319_488), "312 KB");
        assert_eq!(human_size(700), "700 B");
    }

    #[test]
    fn one_image_fits_400x300_and_several_make_a_grid() {
        let one = layout(&[(400, 225)], 800.0);
        assert_eq!(one[0].size(), Vec2::new(400.0, 225.0));
        let tall = layout(&[(225, 400)], 800.0);
        assert_eq!(tall[0].size().y, 300.0);
        assert!((tall[0].size().x - 168.75).abs() < 1.0);
        // Never enlarged; shrinks with a narrow chat.
        assert_eq!(layout(&[(120, 80)], 800.0)[0].size(), Vec2::new(120.0, 80.0));
        assert!(layout(&[(400, 225)], 250.0)[0].size().x <= 250.0);
        let four = layout(&[(400, 225); 4], 800.0);
        assert_eq!(four.len(), 4);
        assert!(four.iter().all(|r| r.width() <= GRID_MAX.x && r.height() <= GRID_MAX.y));
        assert!(four[1].min.x > four[0].max.x && four[2].min.y > four[0].max.y, "two columns, two rows");
    }

    /// A key for id `id` of content `c`.
    fn k(id: AttachmentId) -> ImgKey {
        ImgKey { id, content: 0xab00 + id as u64 }
    }

    #[test]
    fn the_store_asks_once_frees_idle_textures_and_never_asks_for_gone_images() {
        let mut s = ImageStore::default();
        let t0 = Instant::now();
        s.want_thumb(k(1), t0);
        s.want_thumb(k(1), t0);
        s.want_thumb(k(2), t0);
        s.want_thumb(k(3), t0);
        s.want_thumb(k(4), t0);
        let r = s.take_requests();
        assert_eq!(r, vec![(k(1), true), (k(2), true), (k(3), true)], "at most three at once, each once");
        assert!(s.take_requests().is_empty());
        s.on_fetched(k(1), true, Err(FetchError::Gone));
        assert_eq!(s.take_requests(), vec![(k(4), true)]);
        assert!(s.thumb_gone(k(1)));
        s.want_thumb(k(1), t0 + Duration::from_secs(100));
        assert!(s.take_requests().is_empty(), "a gone image is not asked again");
        s.on_fetched(k(2), true, Err(FetchError::Failed("x".into())));
        s.want_thumb(k(2), t0 + Duration::from_secs(1));
        assert!(s.take_requests().is_empty(), "not right away");
        s.want_thumb(k(2), Instant::now() + RETRY_AFTER + Duration::from_secs(1));
        assert_eq!(s.take_requests(), vec![(k(2), true)], "later, yes");
        // The viewer's image jumps the queue.
        s.want_thumb(k(9), t0);
        s.want_full(k(5));
        s.on_fetched(k(3), true, Ok(vec![1, 2, 3]));
        assert_eq!(s.take_requests(), vec![(k(5), false)]);
    }

    /// The v0.8.1 bug: a deleted image's id came back on a new upload and the cached thumbnail of
    /// the old picture was shown. Keyed by content, the new one is a new entry (downloaded); and a
    /// deleted message's images are forgotten.
    #[test]
    fn a_reused_id_with_other_content_is_a_different_image() {
        let mut s = ImageStore::default();
        let t0 = Instant::now();
        let old = Attachment { id: 7, mime: "image/webp".into(), width: 10, height: 10, size: 1, thumb_w: 10, thumb_h: 10, animated: false, hash: "00000000000000aa".into(), sealed: false };
        let new = Attachment { hash: "00000000000000bb".into(), ..old.clone() };
        let (ko, kn) = (ImgKey::of(&old), ImgKey::of(&new));
        assert_ne!(ko, kn);
        s.want_thumb(ko, t0);
        assert_eq!(s.take_requests(), vec![(ko, true)]);
        s.on_fetched(ko, true, Ok(vec![1, 2, 3]));
        assert_eq!(s.thumb_bytes(ko), Some(&[1u8, 2, 3][..]));
        s.want_thumb(kn, t0);
        assert_eq!(s.take_requests(), vec![(kn, true)], "same id, new content: downloaded, not served from the cache");
        assert_eq!(s.thumb_bytes(kn), None);
        // A late answer for the old key never fills the new one.
        s.on_fetched(kn, true, Ok(vec![9]));
        assert_eq!(s.thumb_bytes(kn), Some(&[9u8][..]));
        assert_eq!(s.thumb_bytes(ko), Some(&[1u8, 2, 3][..]));
        s.forget(&[ko]);
        assert!(!s.knows(ko) && s.knows(kn));
        // An older hub without hashes still gets an entry per id.
        assert_eq!(content_tag(""), 0);
        assert_eq!(content_tag("00000000000000ff"), 255);
        assert_ne!(content_tag("not-hex"), content_tag("not-hex2"));
    }

    #[test]
    fn thumbnails_and_full_images_decode() {
        let img = image::RgbaImage::from_pixel(40, 30, image::Rgba([1, 2, 3, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        assert_eq!(decode_thumb(&png).unwrap().size, [40, 30]);
        assert_eq!(decode_full(&png).unwrap().len(), 1);
        let big = image::RgbaImage::from_pixel(600, 10, image::Rgba([1, 2, 3, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(big).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        assert!(decode_thumb(&png).is_none(), "bigger than any thumbnail");
        assert!(decode_full(b"junk").is_err());
    }

    /// A sealed (E2) thumbnail is only checked here, after decryption: a 60-byte PNG that claims
    /// 60000 x 60000 pixels must be refused from its header, not decoded into 14 GB.
    #[test]
    fn a_thumbnail_claiming_a_huge_size_is_refused_before_decoding() {
        fn crc(data: &[u8]) -> u32 {
            let mut c = 0xffff_ffffu32;
            for b in data {
                c ^= u32::from(*b);
                for _ in 0..8 {
                    c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
                }
            }
            !c
        }
        fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let mut body = kind.to_vec();
            body.extend_from_slice(data);
            out.extend_from_slice(&body);
            out.extend_from_slice(&crc(&body).to_be_bytes());
        }
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&60_000u32.to_be_bytes());
        ihdr.extend_from_slice(&60_000u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        chunk(&mut png, b"IHDR", &ihdr);
        chunk(&mut png, b"IDAT", &[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01]);
        chunk(&mut png, b"IEND", &[]);
        assert!(image::ImageReader::new(std::io::Cursor::new(&png)).with_guessed_format().unwrap().into_dimensions().unwrap() == (60_000, 60_000));
        assert!(decode_thumb(&png).is_none());
        assert!(decode_full(&png).is_err(), "the viewer's own limit (8192)");
    }
}
