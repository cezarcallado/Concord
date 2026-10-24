//! v1.1 (E2): end-to-end encrypted DM lines and their images, on the backend's side of the app.
//!
//! - **Opening.** Every DM line that comes from the hub (a history page, the list's newest lines,
//!   `DmMessageCreated`, the answer to a send) passes through [`E2eeBackend::open_lines`] before the
//!   UI sees it. A sealed line (`DmMessage::sealed`, the `e2e1` body) is opened with this
//!   computer's key and the sender device's key from the directory (or, for a device that has
//!   left it, the key pinned in e2ee.json). The UI then gets an ordinary line: `content` is the
//!   text, `attachments` the images (with the sizes and type from the plaintext, `sealed: true`),
//!   and `sealed` no longer holds ciphertext but how it went: [`OPENED`], [`BEFORE_DEVICE`]
//!   ("Mensagem enviada antes deste aparelho existir": not wrapped for this computer) or
//!   [`UNREADABLE`] ("Não foi possível ler esta mensagem"). Those tags have a `:`, which base64url
//!   never has, so a tag can never be mistaken for a body.
//! - **Replays.** Each plaintext carries a random uuid; a second line with an `id` this session
//!   already opened under another message id is dropped.
//! - **Replies.** A reply to a sealed line has no excerpt from the hub (`ReplyRef::sealed`): it is
//!   filled from the line opened here, or "Mensagem criptografada".
//! - **Sending.** [`E2eeBackend::plan`]: sealed when this computer is registered and the peer has
//!   at least one device; otherwise plain, except in a conversation that was ever encrypted here
//!   (e2ee.json `sealed_dms`), which is never sent in the clear without the person saying so
//!   (`SendMode::plain_ok`, the UI's "Enviar sem criptografia?"). The key of a line is wrapped for
//!   every device of the peer and every device of mine, this one included (so this computer reads
//!   its own lines again after a restart).
//! - **Images.** [`seal_image`]: a random key per picture, the file sealed with nonce 0 and the
//!   thumbnail with nonce 1 (the picture itself when it has no separate thumbnail), uploaded with
//!   `?sealed=1`. The keys of opened lines are kept in memory by attachment id
//!   ([`remember_image_key`]); [`open_image`] decrypts a downloaded file before the UI's decoder
//!   (and its limits) ever sees it.
//!
//! Nothing here logs a key, a plaintext or a ciphertext.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tracing::{debug, info, warn};
use vox_client_core::hub::{HubClient, HubEvent};
use vox_client_core::images::Compressed;
use vox_e2ee::dm::{self as edm, AttKey, Blob, DmContext, DmPlain, OpenError};
use vox_e2ee::envelope::Recipient;
use vox_e2ee::zeroize::Zeroizing;
use vox_e2ee::KEY_LEN;
use vox_proto::{Attachment, AttachmentId, DmConversation, DmId, DmMessage, DmMessageKind, ServerEvent, UserId, ATTACHMENT_MAX_THUMB_BYTES, DM_SEALED_MAX_BYTES};

use super::{group, E2eeBackend, Inner};
use crate::backend::{Controller, Ev};

/// `DmMessage::sealed` of a line opened here (`content` is its text).
pub const OPENED: &str = "e2e:ok";
/// `DmMessage::sealed` of a line not wrapped for this computer.
pub const BEFORE_DEVICE: &str = "e2e:new-device";
/// `DmMessage::sealed` of a line that did not open (wrong key, altered, unknown sender device).
pub const UNREADABLE: &str = "e2e:fail";
pub const BEFORE_DEVICE_TEXT: &str = "Mensagem enviada antes deste aparelho existir";
pub const UNREADABLE_TEXT: &str = "Não foi possível ler esta mensagem";
/// The notification of a line that did not open.
pub const NOTIFY_TEXT: &str = "Nova mensagem criptografada";
/// The quote of a reply to a sealed line this computer could not read (or has not loaded).
pub const QUOTE_TEXT: &str = "Mensagem criptografada";
/// The DM header's note.
pub const HEADER_TEXT: &str = "Criptografada de ponta a ponta";
/// A sealed conversation would have gone out in the clear (the UI asks first; this is the
/// backend's own check, e.g. when the peer's last device left meanwhile).
pub const REFUSED_PLAIN: &str = "a conversa é criptografada e não dá para criptografar agora (nada foi enviado)";
/// The device list could not be read: whether the line could be encrypted is unknown (the UI
/// asks before anything goes out in the clear).
pub const DIRECTORY_UNKNOWN: &str = "não deu para ler a lista de aparelhos com criptografia agora (nada foi enviado)";
/// The peer has a device but this computer is not registered (the UI asks).
pub const THIS_DEVICE_OFF: &str = "este computador está sem criptografia agora (nada foi enviado)";
/// How long a line waits for the device list after a (re)connect.
const DIRECTORY_WAIT: Duration = Duration::from_secs(10);

/// A line that went through [`E2eeBackend::open_lines`] and was end-to-end encrypted.
pub fn is_e2e(m: &DmMessage) -> bool {
    m.sealed.is_some()
}

/// An encrypted line this computer could not read (the UI shows its `content` in italics).
pub fn is_unreadable(m: &DmMessage) -> bool {
    m.sealed.as_deref().is_some_and(|s| s != OPENED)
}

/// How a DM line may go out: to whom (`peer`), and whether the person agreed to send it without
/// encryption in a conversation that was encrypted ("Enviar sem criptografia").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendMode {
    pub peer: UserId,
    pub plain_ok: bool,
}

/// What [`E2eeBackend::plan`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    Plain,
    Seal,
}

// ---------------------------------------------------------------- image keys

fn image_keys() -> &'static Mutex<HashMap<AttachmentId, Zeroizing<[u8; KEY_LEN]>>> {
    static KEYS: OnceLock<Mutex<HashMap<AttachmentId, Zeroizing<[u8; KEY_LEN]>>>> = OnceLock::new();
    KEYS.get_or_init(Default::default)
}

/// The key of a sealed image (from an opened line, or one of mine just uploaded).
pub fn remember_image_key(id: AttachmentId, key: &[u8; KEY_LEN]) {
    image_keys().lock().unwrap_or_else(|e| e.into_inner()).insert(id, Zeroizing::new(*key));
}

/// Logout: every image key goes (wiped).
pub fn forget_image_keys() {
    image_keys().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// A downloaded file (`thumb`: its thumbnail) as the decoder must see it: decrypted when it is a
/// sealed image this computer has the key of, as it is otherwise. A sealed file that does not
/// open is an error (never handed to the decoder).
pub fn open_image(id: AttachmentId, thumb: bool, bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    let key = image_keys().lock().unwrap_or_else(|e| e.into_inner()).get(&id).map(|k| Zeroizing::new(**k));
    match key {
        None => Ok(bytes),
        // A picture sent without a thumbnail of its own: the hub hands out the file itself.
        Some(k) => edm::open_blob(&k, if thumb { Blob::Thumb } else { Blob::Main }, &bytes)
            .or_else(|e| if thumb { edm::open_blob(&k, Blob::Main, &bytes) } else { Err(e) })
            .map_err(|_| "imagem criptografada que não abriu".to_string()),
    }
}

/// One picture sealed for upload: the two parts and what goes into the line's plaintext.
pub struct SealedImage {
    pub main: Vec<u8>,
    pub thumb: Option<Vec<u8>>,
    key: Zeroizing<[u8; KEY_LEN]>,
    mime: String,
    anim: bool,
    size: (u32, u32),
    thumb_size: (u32, u32),
}

impl SealedImage {
    /// The plaintext's entry for this picture, once the hub gave it `id`; the key is remembered
    /// here too (my own line shows it at once).
    pub fn att_key(&self, id: AttachmentId) -> AttKey {
        remember_image_key(id, &self.key);
        AttKey { id, k: edm::key_to_b64(&self.key), mime: self.mime.clone(), w: self.size.0, h: self.size.1, tw: self.thumb_size.0, th: self.thumb_size.1, anim: self.anim }
    }
}

/// Seal a compressed picture with a fresh key. Without a thumbnail of its own (the picture
/// already fits the thumbnail box) the picture is its thumbnail, when it is small enough.
pub fn seal_image(c: &Compressed) -> Result<SealedImage, String> {
    let key = edm::new_blob_key().map_err(|_| "sem números aleatórios para criptografar".to_string())?;
    let fail = |_| "não foi possível criptografar a imagem".to_string();
    let main = edm::seal_blob(&key, Blob::Main, &c.bytes).map_err(fail)?;
    let (thumb_src, thumb_size) = match &c.thumb {
        Some(t) => (Some(t.as_slice()), (c.thumb_w, c.thumb_h)),
        None if c.bytes.len() <= ATTACHMENT_MAX_THUMB_BYTES => (Some(c.bytes.as_slice()), (c.width, c.height)),
        None => (None, (0, 0)),
    };
    let thumb = thumb_src.map(|t| edm::seal_blob(&key, Blob::Thumb, t)).transpose().map_err(fail)?;
    Ok(SealedImage { main, thumb, key, mime: c.mime().to_string(), anim: c.animated, size: (c.width, c.height), thumb_size })
}

// ---------------------------------------------------------------- opening

/// How one line went.
enum Outcome {
    /// Shown (opened, or with its failure text).
    Keep,
    /// A replay of a line already shown under another id: dropped.
    Replay,
    /// The sender device is in neither the directory nor the pins: read the directory again.
    UnknownSender,
}

impl Inner {
    /// The keys `author`'s device `dev` had: the directory's, then the pinned ones (a device that
    /// left the directory, or a key from before a reinstall).
    fn sender_keys(&mut self, author: UserId, dev: &vox_e2ee::DeviceId) -> Vec<[u8; KEY_LEN]> {
        let Some(s) = self.session.as_ref() else { return Vec::new() };
        let hex = vox_e2ee::hex(dev);
        let mut keys: Vec<[u8; KEY_LEN]> = s.dir.get(&author).into_iter().flatten().filter(|d| d.device_id == hex).filter_map(|d| vox_e2ee::public_key_from_b64(&d.public_key)).collect();
        if author == s.me {
            if let Some(i) = s.identity.as_ref().filter(|i| i.device_id == *dev) {
                keys.push(i.public_key());
            }
        }
        let server_id = s.server_id.clone();
        for k in self.store().pinned_keys(&server_id, author, &hex) {
            if let Some(k) = vox_e2ee::public_key_from_b64(&k) {
                if !keys.contains(&k) {
                    keys.push(k);
                }
            }
        }
        keys
    }

    /// Open one line in place (see the module docs). `final_try`: an unknown sender device is
    /// unreadable now instead of [`Outcome::UnknownSender`].
    fn open_line(&mut self, m: &mut DmMessage, final_try: bool) -> Outcome {
        let Some(body) = m.sealed.as_deref() else { return Outcome::Keep };
        if body.contains(':') {
            return Outcome::Keep; // opened already
        }
        let unreadable = |m: &mut DmMessage, tag: &str, text: &str| {
            m.sealed = Some(tag.to_string());
            m.content = text.to_string();
            m.attachments.clear();
            m.sealed_attachments.clear();
            Outcome::Keep
        };
        // A conversation that was ever encrypted is never sent in the clear without asking.
        let Some(s) = self.session.as_ref() else { return unreadable(m, UNREADABLE, UNREADABLE_TEXT) };
        let server_id = s.server_id.clone();
        if self.store().mark_sealed_dm(&server_id, m.dm_id) {
            info!(dm = m.dm_id, "e2ee: this conversation is end-to-end encrypted from now on");
            self.dirty = true;
        }
        // (The hub refuses longer bodies; a modified hub's are not even decoded.)
        if body.len() > DM_SEALED_MAX_BYTES {
            return unreadable(m, UNREADABLE, UNREADABLE_TEXT);
        }
        let Ok(sealed) = edm::parse(body) else { return unreadable(m, UNREADABLE, UNREADABLE_TEXT) };
        let Some(this) = self.session.as_ref().and_then(|s| s.identity.as_ref()).map(|i| i.device_id) else { return unreadable(m, UNREADABLE, UNREADABLE_TEXT) };
        if !sealed.is_for(&this) {
            return unreadable(m, BEFORE_DEVICE, BEFORE_DEVICE_TEXT);
        }
        let keys = self.sender_keys(m.author.id, &sealed.sender_dev);
        if keys.is_empty() {
            if !final_try {
                return Outcome::UnknownSender;
            }
            debug!(line = m.id, "e2ee: a line from a device nobody listed");
            return unreadable(m, UNREADABLE, UNREADABLE_TEXT);
        }
        let ctx = DmContext { dm_id: m.dm_id, sender_user: m.author.id };
        let identity = self.session.as_ref().and_then(|s| s.identity.as_ref()).expect("checked above");
        let mut opened = None;
        for k in &keys {
            match edm::open(identity, ctx, k, &sealed) {
                Ok(p) => {
                    opened = Some(p);
                    break;
                }
                Err(OpenError::NotForThisDevice) => return unreadable(m, BEFORE_DEVICE, BEFORE_DEVICE_TEXT),
                Err(OpenError::Failed) => {}
            }
        }
        let Some(plain) = opened.and_then(|p| DmPlain::from_json(&p).ok()) else {
            debug!(line = m.id, "e2ee: a line did not open");
            return unreadable(m, UNREADABLE, UNREADABLE_TEXT);
        };
        let seen_key = (m.dm_id, m.author.id, plain.id.clone());
        match self.seen.get(&seen_key) {
            Some(first) if *first != m.id => {
                warn!(line = m.id, first = *first, "e2ee: a replayed line was dropped");
                return Outcome::Replay;
            }
            _ => {}
        }
        self.seen.insert(seen_key, m.id);
        self.texts.insert(m.id, (m.dm_id, Zeroizing::new(vox_proto::reply_excerpt(&plain.t))));
        // The images the plaintext names and the hub delivered, with their real type and sizes.
        let mut atts = Vec::new();
        for k in &plain.att {
            let (Some(a), Some(key)) = (m.sealed_attachments.iter().find(|a| a.id == k.id), k.key()) else { continue };
            remember_image_key(a.id, &key);
            atts.push(Attachment {
                id: a.id,
                mime: k.mime.clone(),
                width: k.w,
                height: k.h,
                size: a.size.saturating_sub(vox_proto::ATTACHMENT_SEALED_OVERHEAD as u64),
                thumb_w: k.tw,
                thumb_h: k.th,
                animated: k.anim,
                hash: a.hash.clone(),
                sealed: true,
            });
        }
        m.content = plain.t.clone();
        m.attachments = atts;
        m.sealed_attachments.clear();
        m.sealed = Some(OPENED.to_string());
        Outcome::Keep
    }

    /// Open `msgs`, dropping replays; then the quotes of replies to sealed lines. Returns whether a
    /// sender device was unknown (the caller reads the directory again and calls once more).
    fn open_all(&mut self, msgs: &mut Vec<DmMessage>, final_try: bool) -> bool {
        let mut unknown = false;
        // Oldest first: of two lines with the same uuid, the first one is kept.
        let mut order: Vec<usize> = (0..msgs.len()).collect();
        order.sort_by_key(|&i| msgs[i].id);
        let mut drop_ids = Vec::new();
        for &at in &order {
            match self.open_line(&mut msgs[at], final_try) {
                Outcome::Keep => {}
                Outcome::Replay => drop_ids.push(msgs[at].id),
                Outcome::UnknownSender => unknown = true,
            }
        }
        msgs.retain(|m| !drop_ids.contains(&m.id));
        for m in msgs.iter_mut() {
            if let Some(r) = m.reply.as_mut().filter(|r| r.sealed && !r.deleted && r.excerpt.is_empty()) {
                // Only a line of this same conversation (the hub picks `r.id`: a line of another
                // conversation must not show up quoted here).
                r.excerpt = match self.texts.get(&r.id).filter(|(dm, _)| *dm == m.dm_id) {
                    Some((_, t)) if !t.trim().is_empty() => t.to_string(),
                    Some(_) if r.images > 0 => String::new(),
                    _ => QUOTE_TEXT.to_string(),
                };
            }
        }
        unknown
    }

    /// Publish when opening changed what the UI knows (a conversation newly encrypted).
    fn flush(&mut self) {
        if std::mem::take(&mut self.dirty) {
            if let Some(out) = self.out.clone() {
                self.publish(&out);
            }
        }
    }

    /// Lines may open now: no session (nothing to wait for), or the directory was read.
    fn directory_ready(&self) -> bool {
        self.session.as_ref().is_none_or(|s| s.dir_loaded || s.identity.is_none())
    }
}

impl E2eeBackend {
    async fn wait_directory(&self) {
        let until = tokio::time::Instant::now() + DIRECTORY_WAIT;
        while !self.lock().directory_ready() && tokio::time::Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Read the whole directory again (a line from a device this computer did not know yet).
    pub(super) async fn refresh_directory(&self, hub: &HubClient) {
        let gen = self.lock().gen;
        let all = match hub.e2ee_devices().await {
            Ok(all) => all,
            Err(e) => {
                warn!(error = %format!("{e:#}"), "e2ee: cannot read the device list again");
                return;
            }
        };
        let mut g = self.lock();
        if g.gen != gen || g.session.is_none() {
            return;
        }
        if let Some(out) = g.out.clone() {
            g.observe(&all, &out);
        }
        if let Some(s) = g.session.as_mut() {
            s.dir = group(all);
            s.dir_loaded = true;
        }
        g.dirty = true;
    }

    /// Open the sealed lines among `msgs` in place (see the module docs); replays are removed.
    pub async fn open_lines(&self, hub: &HubClient, msgs: &mut Vec<DmMessage>) {
        if !msgs.iter().any(|m| m.sealed.as_deref().is_some_and(|b| !b.contains(':'))) {
            // Quotes of sealed lines still get their text.
            self.lock().open_all(msgs, true);
            return;
        }
        self.wait_directory().await;
        let unknown = self.lock().open_all(msgs, false);
        if unknown {
            self.refresh_directory(hub).await;
            self.lock().open_all(msgs, true);
        }
        self.lock().flush();
    }

    /// The newest line of every conversation of the list.
    pub async fn open_convs(&self, hub: &HubClient, convs: &mut [DmConversation]) {
        let mut lines: Vec<DmMessage> = convs.iter_mut().filter_map(|c| c.last_message.take()).collect();
        self.open_lines(hub, &mut lines).await;
        for c in convs.iter_mut() {
            // (A replayed newest line is dropped: the list shows no preview for it.)
            c.last_message = lines.iter().position(|m| m.dm_id == c.id).map(|i| lines.swap_remove(i));
        }
    }

    /// One line at once when everything it needs is here (`None`: it has to wait for the
    /// directory, see [`E2eeBackend::open_lines`]). `Some(false)`: a replay, dropped.
    fn open_now(&self, m: &mut DmMessage) -> Option<bool> {
        let mut g = self.lock();
        if !g.directory_ready() {
            return None;
        }
        let mut v = vec![std::mem::replace(m, empty_line())];
        if g.open_all(&mut v, false) {
            *m = v.pop().unwrap_or_else(empty_line);
            return None;
        }
        g.flush();
        match v.pop() {
            Some(line) => {
                *m = line;
                Some(true)
            }
            None => Some(false),
        }
    }

    /// Before [`E2eeBackend::plan`]: wait a moment for the device list after a connect, and read
    /// it again when it still has not come (the first read failed).
    pub async fn prepare(&self, hub: &HubClient) {
        self.wait_directory().await;
        if !self.lock().directory_ready() {
            self.refresh_directory(hub).await;
            self.lock().flush();
        }
    }

    /// How a line to `mode.peer` in `dm` goes out (waits a moment for the directory after a
    /// connect). `Err`: refused (a sealed conversation that cannot be sealed now, not agreed to).
    pub async fn plan(&self, dm: DmId, mode: SendMode) -> Result<Plan, String> {
        self.wait_directory().await;
        self.lock().plan_now(dm, mode)
    }
}

impl Inner {
    /// [`E2eeBackend::plan`] once the wait is over.
    fn plan_now(&mut self, dm: DmId, mode: SendMode) -> Result<Plan, String> {
        let g = self;
        let Some(s) = g.session.as_ref() else {
            // No directory (an older hub, or one that stopped announcing it): plain, except in a
            // conversation encrypted here before (never silently).
            let hint = g.server_hint.clone();
            let file = &g.store().file.sealed_dms;
            let was_sealed = match &hint {
                Some(server_id) => file.get(server_id).is_some_and(|v| v.contains(&dm)),
                // (A hub without an id: any hub's encrypted conversation with this id asks.)
                None => file.values().any(|v| v.contains(&dm)),
            };
            if was_sealed && !mode.plain_ok {
                warn!(dm, "e2ee: not sent: the conversation is encrypted and this hub has no device list now");
                return Err(REFUSED_PLAIN.to_string());
            }
            return Ok(Plan::Plain);
        };
        let peer_has = s.dir.get(&mode.peer).is_some_and(|v| !v.is_empty());
        // Registered this session, or already listed with this key (a registration the hub refused
        // or could not take now does not turn encryption off: the peer can still check my lines).
        let listed = s.identity.as_ref().is_some_and(|i| {
            s.dir.get(&s.me).is_some_and(|v| v.iter().any(|d| d.device_id == i.device_id_hex() && vox_e2ee::public_key_from_b64(&d.public_key) == Some(i.public_key())))
        });
        if s.identity.is_some() && (s.registered || listed) && peer_has {
            return Ok(Plan::Seal);
        }
        // The device list never came (still not read 10 s after connecting): whether the peer has
        // a device is unknown, so nothing goes out in the clear without asking.
        if s.identity.is_some() && !s.dir_loaded && !mode.plain_ok {
            warn!(dm, "e2ee: not sent: the device list could not be read");
            return Err(DIRECTORY_UNKNOWN.to_string());
        }
        // The peer has a device but this computer is not on the list (the hub refused or lost its
        // registration): not silently in the clear either.
        if s.identity.is_some() && peer_has && !mode.plain_ok {
            warn!(dm, "e2ee: not sent: this computer is not on the device list");
            return Err(THIS_DEVICE_OFF.to_string());
        }
        let server_id = s.server_id.clone();
        if mode.plain_ok || !g.store().sealed_dms(&server_id).contains(&dm) {
            if mode.plain_ok {
                info!(dm, "e2ee: sent without encryption, as the person agreed");
            }
            return Ok(Plan::Plain);
        }
        warn!(dm, "e2ee: not sent: the conversation is encrypted and cannot be now");
        Err(REFUSED_PLAIN.to_string())
    }
}

impl E2eeBackend {
    /// The sealed body of a line I send in `dm` to `peer`: `text` and the images' keys, for every
    /// device of the peer and of mine.
    pub fn seal_line(&self, dm: DmId, peer: UserId, text: &str, att: Vec<AttKey>) -> Result<String, String> {
        let mut g = self.lock();
        let Some(s) = g.session.as_ref() else { return Err(REFUSED_PLAIN.into()) };
        let Some(identity) = s.identity.as_ref() else { return Err(REFUSED_PLAIN.into()) };
        let devices = |u: UserId| -> Vec<Recipient> {
            s.dir
                .get(&u)
                .into_iter()
                .flatten()
                .filter_map(|d| Some(Recipient { device_id: vox_e2ee::device_id_from_hex(&d.device_id)?, public_key: vox_e2ee::public_key_from_b64(&d.public_key)? }))
                .collect()
        };
        let theirs = devices(peer);
        if theirs.is_empty() {
            return Err(REFUSED_PLAIN.into());
        }
        // Every device of mine, this one included, FIRST: `envelope::wrap_for_all` keeps the first
        // of two devices with one id, and ids are only unique per member (a peer who registers a
        // device under the id of one of my computers must not take its place in my own lines).
        let mut recipients = vec![Recipient { device_id: identity.device_id, public_key: identity.public_key() }];
        recipients.extend(devices(s.me));
        recipients.extend(theirs);
        let id = edm::new_message_id().map_err(|_| "sem números aleatórios para criptografar".to_string())?;
        let plain = DmPlain { t: text.to_string(), id, ts: crate::app::now_ms(), att };
        let ctx = DmContext { dm_id: dm, sender_user: s.me };
        let body = edm::seal(identity, ctx, &recipients, &plain.to_json()).map_err(|_| "não foi possível criptografar a mensagem".to_string())?;
        if body.len() > DM_SEALED_MAX_BYTES {
            return Err("a mensagem criptografada ficou grande demais (encurte o texto)".into());
        }
        let devices = recipients.len();
        let server_id = s.server_id.clone();
        if g.store().mark_sealed_dm(&server_id, dm) {
            g.dirty = true;
        }
        g.flush();
        debug!(dm, devices, "e2ee: line sealed");
        Ok(body)
    }
}

fn empty_line() -> DmMessage {
    DmMessage {
        id: 0,
        dm_id: 0,
        author: vox_proto::User::new(0, ""),
        content: String::new(),
        created_at_ms: 0,
        kind: DmMessageKind::Text,
        call_secs: 0,
        attachments: Vec::new(),
        reply: None,
        reactions: Vec::new(),
        sealed: None,
        sealed_attachments: Vec::new(),
    }
}

impl Controller {
    /// `DmMessageCreated` with a sealed line (or a reply quoting one): opened before the UI hears
    /// of it (at once when the directory has everything, otherwise in a task that waits for it).
    pub(in crate::backend) fn e2ee_dm_created(&mut self, mut message: DmMessage) {
        let e2 = self.e2ee.clone();
        let out = self.out.clone();
        match e2.open_now(&mut message) {
            Some(true) => out.send(Ev::Hub(HubEvent::Server(ServerEvent::DmMessageCreated { message }))),
            Some(false) => {}
            None => {
                let Some(hub) = self.hub.clone() else {
                    out.send(Ev::Hub(HubEvent::Server(ServerEvent::DmMessageCreated { message })));
                    return;
                };
                tokio::spawn(async move {
                    let mut v = vec![message];
                    e2.open_lines(&hub, &mut v).await;
                    if let Some(message) = v.pop() {
                        out.send(Ev::Hub(HubEvent::Server(ServerEvent::DmMessageCreated { message })));
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use vox_e2ee::Identity;
    use vox_proto::{E2eeDevice, ReplyRef, User};

    use super::super::Session;
    use super::*;
    use crate::e2ee_store::Store;

    fn device(user: UserId, i: &Identity) -> E2eeDevice {
        E2eeDevice { user_id: user, device_id: i.device_id_hex(), public_key: vox_e2ee::public_key_to_b64(&i.public_key()), label: format!("pc{user}"), created_at_ms: 1 }
    }

    fn copy(i: &Identity) -> Identity {
        Identity::from_parts(i.device_id, &mut i.secret_bytes())
    }

    /// The backend of `me` on this identity, with this directory (read).
    fn backend(me: UserId, identity: &Identity, dir: &[E2eeDevice]) -> E2eeBackend {
        let e2 = E2eeBackend::default();
        {
            let mut g = e2.lock();
            g.store = Some(Store::in_memory());
            let mut d: BTreeMap<UserId, Vec<E2eeDevice>> = BTreeMap::new();
            for x in dir {
                d.entry(x.user_id).or_default().push(x.clone());
            }
            g.session = Some(Session { server_id: "srv".into(), me, identity: Some(copy(identity)), registered: true, dir: d, dir_loaded: true });
        }
        e2
    }

    fn line(id: i64, dm: DmId, author: UserId, body: &str) -> DmMessage {
        DmMessage { id, dm_id: dm, author: User::new(author, format!("u{author}")), content: vox_proto::DM_SEALED_PLACEHOLDER.into(), sealed: Some(body.into()), ..empty_line() }
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(f)
    }

    #[test]
    fn a_line_opens_for_the_peer_and_my_other_computers_and_replays_are_dropped() {
        let (ana, ana2, bia) = (Identity::generate().unwrap(), Identity::generate().unwrap(), Identity::generate().unwrap());
        let dir = [device(1, &ana), device(1, &ana2), device(2, &bia)];
        let a = backend(1, &ana, &dir);
        let body = a.seal_line(5, 2, "oi, Bia", Vec::new()).unwrap();
        assert!(!body.contains(':') && body.len() <= DM_SEALED_MAX_BYTES);
        assert!(a.lock().store().sealed_dms("srv").contains(&5), "sending sealed marks the conversation");
        for (me, who) in [(2, &bia), (1, &ana2), (1, &ana)] {
            let e2 = backend(me, who, &dir);
            let mut v = vec![line(10, 5, 1, &body)];
            assert!(!e2.lock().open_all(&mut v, true));
            assert_eq!((v[0].content.as_str(), v[0].sealed.as_deref()), ("oi, Bia", Some(OPENED)), "user {me}");
            assert!(is_e2e(&v[0]) && !is_unreadable(&v[0]));
            assert!(e2.lock().store().sealed_dms("srv").contains(&5), "receiving one marks it too");
            // The same line again (a reload) stays; the same body under another id is a replay.
            let mut again = vec![line(10, 5, 1, &body), line(11, 5, 1, &body)];
            e2.lock().open_all(&mut again, true);
            assert_eq!(again.iter().map(|m| m.id).collect::<Vec<_>>(), vec![10]);
        }
        // Another conversation, another author: it does not open.
        let b = backend(2, &bia, &dir);
        let mut v = vec![line(12, 6, 1, &body), line(13, 5, 2, &body), line(14, 5, 1, "AAAA")];
        b.lock().open_all(&mut v, true);
        for m in &v {
            assert_eq!((m.sealed.as_deref(), m.content.as_str()), (Some(UNREADABLE), UNREADABLE_TEXT), "line {}", m.id);
        }
    }

    #[test]
    fn a_new_computer_sees_before_this_device_and_unknown_senders_wait_for_the_directory() {
        let (ana, bia, bia_new) = (Identity::generate().unwrap(), Identity::generate().unwrap(), Identity::generate().unwrap());
        let before = [device(1, &ana), device(2, &bia)];
        let body = backend(1, &ana, &before).seal_line(5, 2, "antes", Vec::new()).unwrap();
        let now = [device(1, &ana), device(2, &bia), device(2, &bia_new)];
        let mut v = vec![line(10, 5, 1, &body)];
        backend(2, &bia_new, &now).lock().open_all(&mut v, true);
        assert_eq!((v[0].sealed.as_deref(), v[0].content.as_str()), (Some(BEFORE_DEVICE), BEFORE_DEVICE_TEXT));
        assert!(is_unreadable(&v[0]));
        // A sender device the directory does not list yet: asked again before giving up.
        let lonely = backend(2, &bia, &[device(2, &bia)]);
        let mut v = vec![line(10, 5, 1, &body)];
        assert!(lonely.lock().open_all(&mut v, false), "unknown sender reported");
        assert_eq!(v[0].sealed.as_deref(), Some(body.as_str()), "untouched until the directory is read again");
        assert!(!lonely.lock().open_all(&mut v, true));
        assert_eq!(v[0].sealed.as_deref(), Some(UNREADABLE));
    }

    #[test]
    fn replies_get_their_excerpt_from_the_opened_line() {
        let (ana, bia) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let dir = [device(1, &ana), device(2, &bia)];
        let body = backend(1, &ana, &dir).seal_line(5, 2, "a   pergunta\ninteira", Vec::new()).unwrap();
        let reply = backend(2, &bia, &dir).seal_line(5, 1, "resposta", Vec::new()).unwrap();
        let a = backend(1, &ana, &dir);
        let mut quote = ReplyRef::gone(10);
        quote.deleted = false;
        quote.sealed = true;
        let mut r = line(11, 5, 2, &reply);
        r.reply = Some(quote.clone());
        let mut v = vec![r.clone(), line(10, 5, 1, &body)];
        a.lock().open_all(&mut v, true);
        assert_eq!(v[0].reply.as_ref().map(|q| q.excerpt.as_str()), Some("a pergunta inteira"));
        // The original is unknown here: "Mensagem criptografada".
        let mut v = vec![r];
        backend(1, &ana, &dir).lock().open_all(&mut v, true);
        assert_eq!(v[0].reply.as_ref().map(|q| q.excerpt.as_str()), Some(QUOTE_TEXT));
    }

    #[test]
    fn the_send_plan_never_goes_plain_silently_in_an_encrypted_conversation() {
        let (ana, bia) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let a = backend(1, &ana, &[device(1, &ana), device(2, &bia)]);
        let mode = SendMode { peer: 2, plain_ok: false };
        assert_eq!(block_on(a.plan(5, mode)), Ok(Plan::Seal));
        // Bia has no device: a conversation never encrypted goes plain, an encrypted one does not.
        let a = backend(1, &ana, &[device(1, &ana)]);
        assert_eq!(block_on(a.plan(5, mode)), Ok(Plan::Plain));
        assert!(a.seal_line(5, 2, "x", Vec::new()).is_err(), "nobody to seal for");
        a.lock().store().mark_sealed_dm("srv", 5);
        assert_eq!(block_on(a.plan(5, mode)), Err(REFUSED_PLAIN.to_string()));
        assert_eq!(block_on(a.plan(5, SendMode { plain_ok: true, ..mode })), Ok(Plan::Plain), "the person agreed");
        assert_eq!(block_on(a.plan(6, mode)), Ok(Plan::Plain));
        // No session (an older hub): plain.
        assert_eq!(block_on(E2eeBackend::default().plan(5, mode)), Ok(Plan::Plain));
    }

    #[test]
    fn sealed_images_are_opened_with_their_key_only() {
        let key = edm::new_blob_key().unwrap();
        let img = b"\x89PNG\r\n\x1a\nnot really a png".to_vec();
        let main = edm::seal_blob(&key, Blob::Main, &img).unwrap();
        let thumb = edm::seal_blob(&key, Blob::Thumb, &img).unwrap();
        assert_eq!(open_image(987_654, false, main.clone()).unwrap(), main, "no key: as it is");
        remember_image_key(987_654, &key);
        assert_eq!(open_image(987_654, false, main.clone()).unwrap(), img);
        assert_eq!(open_image(987_654, true, thumb.clone()).unwrap(), img);
        assert!(open_image(987_654, false, thumb.clone()).is_err(), "the thumbnail's nonce is not the file's");
        assert_eq!(open_image(987_654, true, main).unwrap(), img, "no thumbnail of its own: the hub serves the file");
        let mut bad = thumb;
        bad[3] ^= 1;
        assert!(open_image(987_654, true, bad).is_err(), "altered");
    }

    /// Audit (v1.1 E2): fail closed. The device list that never came, and a hub that stops
    /// announcing a directory, never send an encrypted (or maybe encryptable) line in the clear
    /// without the person agreeing.
    #[test]
    fn unknown_directory_or_a_hub_without_one_never_goes_plain_silently() {
        let (ana, bia) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let mode = SendMode { peer: 2, plain_ok: false };
        let agreed = SendMode { plain_ok: true, ..mode };
        // Registered, but the list could not be read: even a conversation never encrypted asks.
        let a = backend(1, &ana, &[]);
        a.lock().session.as_mut().unwrap().dir_loaded = false;
        assert_eq!(a.lock().plan_now(6, mode), Err(DIRECTORY_UNKNOWN.to_string()));
        assert_eq!(a.lock().plan_now(6, agreed), Ok(Plan::Plain));
        // Read now (Bia has a device): sealed.
        let a = backend(1, &ana, &[device(1, &ana), device(2, &bia)]);
        assert_eq!(a.lock().plan_now(6, mode), Ok(Plan::Seal));
        // The hub refused this session's registration: still sealed while I am on the list...
        a.lock().session.as_mut().unwrap().registered = false;
        assert_eq!(a.lock().plan_now(6, mode), Ok(Plan::Seal));
        // ... and asked when I am not (Bia could not check my lines).
        let off = backend(1, &ana, &[device(2, &bia)]);
        off.lock().session.as_mut().unwrap().registered = false;
        assert_eq!(off.lock().plan_now(6, mode), Err(THIS_DEVICE_OFF.to_string()));
        assert_eq!(off.lock().plan_now(6, agreed), Ok(Plan::Plain));
        // The same hub's next snapshot says "no e2ee" (no session): an encrypted conversation asks.
        a.lock().store().mark_sealed_dm("srv", 5);
        {
            let mut g = a.lock();
            g.session = None;
            g.server_hint = Some("srv".into());
        }
        assert_eq!(a.lock().plan_now(5, mode), Err(REFUSED_PLAIN.to_string()));
        assert_eq!(a.lock().plan_now(5, agreed), Ok(Plan::Plain));
        assert_eq!(a.lock().plan_now(6, mode), Ok(Plan::Plain), "never encrypted: plain");
        // ... also when it hides its id.
        a.lock().server_hint = None;
        assert_eq!(a.lock().plan_now(5, mode), Err(REFUSED_PLAIN.to_string()));
        // Another hub's conversation 5 is not this one's.
        a.lock().server_hint = Some("other".into());
        assert_eq!(a.lock().plan_now(5, mode), Ok(Plan::Plain));
    }

    /// Audit (v1.1 E2): the replay check and the reply excerpts cannot be turned against other
    /// lines: a peer reusing the uuid of my line does not hide mine, and a reply the hub points at
    /// a line of another conversation does not quote it.
    #[test]
    fn replays_are_per_conversation_and_author_and_quotes_stay_in_their_conversation() {
        let (ana, bia, caio) = (Identity::generate().unwrap(), Identity::generate().unwrap(), Identity::generate().unwrap());
        let dir = [device(1, &ana), device(2, &bia), device(3, &caio)];
        let a = backend(1, &ana, &dir);
        let mine = a.seal_line(5, 2, "minha", Vec::new()).unwrap();
        // Bia read it, and seals a line of hers with the same uuid.
        let b = backend(2, &bia, &dir);
        let mut v = vec![line(10, 5, 1, &mine)];
        b.lock().open_all(&mut v, true);
        let uuid = b.lock().seen.keys().next().unwrap().2.clone();
        let theirs = {
            let g = b.lock();
            let s = g.session.as_ref().unwrap();
            let id = s.identity.as_ref().unwrap();
            let plain = DmPlain { t: "dela".into(), id: uuid, ts: 0, att: Vec::new() };
            let r: Vec<Recipient> = [&ana, &bia].iter().map(|i| Recipient { device_id: i.device_id, public_key: i.public_key() }).collect();
            edm::seal(id, DmContext { dm_id: 5, sender_user: 2 }, &r, &plain.to_json()).unwrap()
        };
        // Ana loads the newest page first (Bia's line), then the older one (hers): both stay.
        let a2 = backend(1, &ana, &dir);
        let mut newest = vec![line(11, 5, 2, &theirs)];
        a2.lock().open_all(&mut newest, true);
        let mut older = vec![line(10, 5, 1, &mine)];
        a2.lock().open_all(&mut older, true);
        assert_eq!((newest.len(), older.len()), (1, 1), "a peer cannot hide my line");
        assert_eq!(older[0].content, "minha");
        // Caio's conversation with Ana (dm 9): the hub points a reply at Bia's line 11.
        let c = backend(3, &caio, &dir).seal_line(9, 1, "oi Ana", Vec::new()).unwrap();
        let mut q = ReplyRef::gone(11);
        q.deleted = false;
        q.sealed = true;
        let mut r = line(12, 9, 3, &c);
        r.reply = Some(q);
        let mut v = vec![r];
        a2.lock().open_all(&mut v, true);
        assert_eq!(v[0].content, "oi Ana");
        assert_eq!(v[0].reply.as_ref().map(|q| q.excerpt.as_str()), Some(QUOTE_TEXT), "not Bia's text");
    }

    /// Audit (v1.1 E2): device ids are unique per member only. Bia registering a device under the
    /// id of Ana's other computer does not take that computer's place in Ana's lines.
    #[test]
    fn a_peer_device_with_my_device_id_does_not_take_its_place() {
        let (ana, ana2, bia) = (Identity::generate().unwrap(), Identity::generate().unwrap(), Identity::generate().unwrap());
        let mut fake = device(2, &bia);
        fake.device_id = ana2.device_id_hex();
        let dir = [device(1, &ana), device(1, &ana2), device(2, &bia), fake];
        let body = backend(1, &ana, &dir).seal_line(5, 2, "minha", Vec::new()).unwrap();
        let mut v = vec![line(10, 5, 1, &body)];
        backend(1, &ana2, &dir).lock().open_all(&mut v, true);
        assert_eq!((v[0].sealed.as_deref(), v[0].content.as_str()), (Some(OPENED), "minha"));
        let mut v = vec![line(10, 5, 1, &body)];
        backend(2, &bia, &dir).lock().open_all(&mut v, true);
        assert_eq!(v[0].content, "minha", "Bia's real computer still reads it");
    }

    /// Audit (v1.1 E2): a body longer than the hub would ever store is not even decoded.
    #[test]
    fn an_oversized_body_is_unreadable() {
        let ana = Identity::generate().unwrap();
        let a = backend(1, &ana, &[device(1, &ana)]);
        let mut v = vec![line(10, 5, 2, &"A".repeat(DM_SEALED_MAX_BYTES + 4))];
        a.lock().open_all(&mut v, true);
        assert_eq!(v[0].sealed.as_deref(), Some(UNREADABLE));
    }
}
