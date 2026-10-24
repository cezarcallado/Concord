//! Direct messages ("Mensagens diretas") and private 1:1 calls in the app: the sidebar section,
//! the DM chat in the center area, "Nova mensagem", the incoming-call prompt with its ringtone,
//! notifications, and the call itself on top of the voice machinery.
//!
//! A DM call is a voice session like any other (mic, mute/deafen, the denoiser and gate, camera,
//! Go Live, the call view's tiles), only in the private room the hub hands out. Inside the app it
//! is the voice "channel" [`DM_CALL_CHANNEL`] (never a real channel id), so the voice bar, the call
//! view and the backend's session code work unchanged; the hub is never told about it as voice
//! state, so nobody else sees it.
//!
//! A child module of `app` (declared there), so it can use the app's private helpers.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, Layout, RichText};
use vox_client_core::hub::{DmsReply, HttpStatusError, HubClient};
use vox_client_core::{AlertKind, VoiceEvent};
use vox_proto::{
    CallEndReason, CallState, ChannelId, DmCall, DmConversation, DmId, DmMessage, DmMessageId, DmMessageKind, ServerEvent, User, UserId, VoiceMember, CALL_RING_SECS,
    DM_MESSAGE_MAX_CHARS, ERR_CALL_BUSY, ERR_DM_BLOCKED,
};

use crate::context_menu::{Lead, Origin};
use super::{now_ms, App, Center, Screen, GREEN};
use crate::backend::e2ee::dm::{self as e2dm, SendMode};
use crate::backend::e2ee::E2eeBackend;
use crate::backend::Cmd;

/// The voice "channel" of a DM call inside the app (real channel ids are positive).
pub const DM_CALL_CHANNEL: ChannelId = -1;
/// The ringtone repeats this often while a call rings.
const RING_EVERY: Duration = Duration::from_millis(2500);
const HISTORY_PAGE: u32 = 50;
const MAX_KEPT: usize = 1000;
const ROW_H: f32 = 40.0;
const ROW_AVATAR: f32 = 28.0;
const ICON_PHONE: &str = crate::theme::ph::PHONE;
/// The honest one-liner under every DM header.
pub const HOST_NOTE: &str = "Mensagens ficam guardadas no servidor de quem hospeda";
/// v1.1 (E2): the same, for an end-to-end encrypted conversation.
pub const HOST_NOTE_E2E: &str = "Mensagens ficam guardadas criptografadas no servidor de quem hospeda: só os aparelhos de vocês dois conseguem ler";
/// v1.1 (E2): the safety code dialog and its buttons; the banner when it changed after a check.
pub const SAFETY_TITLE: &str = "Verificar código de segurança";
pub const VERIFY: &str = "Marcar como verificado";
pub const UNVERIFY: &str = "Desmarcar";
pub const REVERIFY: &str = "Verifique de novo";

// ---------------------------------------------------------------- backend calls

/// The DM calls the backend runs ([`Cmd::Dm`]); the answer is a [`DmEv`]. (Joining a call's room
/// is [`Cmd::JoinDmCall`], which goes through the voice session code.)
#[derive(Debug, Clone)]
pub enum DmCmd {
    List,
    People,
    Open(UserId),
    History { dm: DmId, before: Option<DmMessageId> },
    /// `reply_to`: the line this one answers (v0.8). `mode` (v1.1 E2): to whom, and whether the
    /// person agreed to send it unencrypted in an encrypted conversation.
    Send { dm: DmId, content: String, reply_to: Option<DmMessageId>, mode: SendMode },
    Delete { dm: DmId, id: DmMessageId },
    Read { dm: DmId, up_to: DmMessageId },
    Block { user: UserId, blocked: bool },
    Decline { dm: DmId },
    End { dm: DmId },
}

#[derive(Debug)]
pub enum DmEv {
    Listed(DmsReply),
    People(Result<Vec<User>, String>),
    Opened(Result<DmConversation, String>),
    History { dm: DmId, before: Option<DmMessageId>, result: Result<Vec<DmMessage>, String> },
    Sent { dm: DmId, content: String, result: Result<DmMessage, String> },
    /// Anything else finished; `Err` is a message for the user.
    Done { cmd: DmCmd, result: Result<(), String> },
}

impl DmCmd {
    pub fn offline(self) -> DmEv {
        let msg = "Sem conexão com o servidor.".to_string();
        match self {
            DmCmd::List => DmEv::Listed(DmsReply::Failed(msg)),
            DmCmd::People => DmEv::People(Err(msg)),
            DmCmd::Open(_) => DmEv::Opened(Err(msg)),
            DmCmd::History { dm, before } => DmEv::History { dm, before, result: Err(msg) },
            DmCmd::Send { dm, content, .. } => DmEv::Sent { dm, content, result: Err(msg) },
            cmd => DmEv::Done { cmd, result: Err(msg) },
        }
    }
}

/// Run one call against the hub (on the backend's runtime). Every line that comes back passes
/// through `e2` first (v1.1 E2: sealed lines are opened, `backend/e2ee/dm.rs`), and a line to
/// somebody with a device key is sealed.
pub async fn run(hub: &HubClient, e2: &E2eeBackend, cmd: DmCmd) -> DmEv {
    let err = |e: anyhow::Error| error_text(&e);
    match cmd {
        DmCmd::List => {
            let mut reply = hub.dms().await;
            if let DmsReply::Ready(list) = &mut reply {
                e2.open_convs(hub, list).await;
            }
            DmEv::Listed(reply)
        }
        DmCmd::People => DmEv::People(hub.dm_people().await.map_err(err)),
        DmCmd::Open(user) => {
            let mut result = hub.open_dm(user).await.map_err(err);
            if let Ok(c) = &mut result {
                e2.open_convs(hub, std::slice::from_mut(c)).await;
            }
            DmEv::Opened(result)
        }
        DmCmd::History { dm, before } => {
            let mut result = hub.dm_messages(dm, before, HISTORY_PAGE).await.map_err(err);
            if let Ok(page) = &mut result {
                e2.open_lines(hub, page).await;
            }
            DmEv::History { dm, before, result }
        }
        DmCmd::Send { dm, content, reply_to, mode } => {
            let mut req = vox_proto::DmSendRequest { content: content.clone(), attachments: Vec::new(), reply_to, sealed: None };
            e2.prepare(hub).await;
            match e2.plan(dm, mode).await {
                Ok(e2dm::Plan::Seal) => match e2.seal_line(dm, mode.peer, &content, Vec::new()) {
                    Ok(body) => {
                        req.content = String::new();
                        req.sealed = Some(body);
                    }
                    Err(e) => return DmEv::Sent { dm, content, result: Err(e) },
                },
                Ok(e2dm::Plan::Plain) => {}
                Err(e) => return DmEv::Sent { dm, content, result: Err(e) },
            }
            let result = match hub.send_dm_request(dm, &req).await {
                Ok(m) => {
                    let mut v = vec![m];
                    e2.open_lines(hub, &mut v).await;
                    v.pop().ok_or_else(|| "a mensagem voltou repetida".to_string())
                }
                Err(e) => Err(err(e)),
            };
            DmEv::Sent { dm, content, result }
        }
        cmd => {
            let result = match &cmd {
                DmCmd::Delete { dm, id } => hub.delete_dm_message(*dm, *id).await,
                DmCmd::Read { dm, up_to } => hub.dm_read(*dm, *up_to).await,
                DmCmd::Block { user, blocked } => hub.dm_block(*user, *blocked).await,
                DmCmd::Decline { dm } => hub.dm_call_decline(*dm).await,
                DmCmd::End { dm } => hub.dm_call_end(*dm).await,
                _ => Ok(()),
            };
            DmEv::Done { cmd, result: result.map_err(err) }
        }
    }
}

/// A failed DM / call request in words for the user.
pub fn error_text(e: &anyhow::Error) -> String {
    match e.downcast_ref::<HttpStatusError>() {
        Some(s) if s.status == 403 && s.message == ERR_DM_BLOCKED => "Não dá: um de vocês bloqueou o outro.".into(),
        Some(s) if s.status == 403 => "Você só pode apagar as suas próprias mensagens.".into(),
        Some(s) if s.status == 409 && s.message == ERR_CALL_BUSY => "A pessoa está em outra chamada agora. Ela vai ver que você ligou.".into(),
        // v1.1 (E1): the hub's own text (an older app shows it as is too).
        Some(s) if s.status == 409 && s.message == vox_proto::ERR_E2EE_CALL_REQUIRED => vox_proto::ERR_E2EE_CALL_REQUIRED.into(),
        Some(s) if s.status == 409 => "Essa chamada já terminou.".into(),
        Some(s) if s.status == 429 => "Calma: espere um instante e tente de novo.".into(),
        Some(s) if s.status == 400 => format!("A mensagem precisa ter de 1 a {DM_MESSAGE_MAX_CHARS} caracteres."),
        Some(s) if s.status == 404 => "Essa conversa não está disponível.".into(),
        _ => crate::backend::friendly(e),
    }
}

// ---------------------------------------------------------------- state

#[derive(Default)]
pub struct DmLog {
    /// Oldest first, unique by id.
    pub msgs: Vec<DmMessage>,
    pub requested: bool,
    pub loading_older: bool,
    pub no_more: bool,
}

/// "Nova mensagem": pick a person.
#[derive(Default)]
pub struct Picker {
    pub open: bool,
    pub filter: String,
    pub people: Vec<User>,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct DmUi {
    /// The hub has DMs (its `GET /api/dms` answered). False on an older hub: everything is hidden.
    pub supported: bool,
    /// The list arrived at least once for this session.
    pub listed: bool,
    /// Most recent activity first.
    pub convs: Vec<DmConversation>,
    pub logs: HashMap<DmId, DmLog>,
    /// The DM shown in the center area (instead of a text channel), when the chat is shown.
    pub selected: Option<DmId>,
    pub input: String,
    pub(super) focus_input: bool,
    pub picker: Picker,
    /// "Enviar mensagem" to somebody with no conversation yet: select it once the hub answers.
    pending_open: Option<UserId>,
    /// "Ligar" to somebody with no conversation yet: start the call once it exists.
    pending_call: Option<UserId>,
    /// People I blocked from a menu before any conversation with them is listed.
    blocked_hint: HashSet<UserId>,
    /// The call the hub told us about that involves me (ringing or answered), if any.
    pub call: Option<DmCall>,
    /// The DM whose call our voice session is in (or joining).
    pub voice_dm: Option<DmId>,
    /// The LiveKit room of the DM call we are in (`dm-<a>-<b>`), once connected.
    pub room: Option<String>,
    /// Who else is in the call's room (identities), and who of them is live.
    room_peers: HashSet<String>,
    live_peers: HashSet<String>,
    /// The call view's members while in a DM call (me and the other person once connected).
    pub call_members: Vec<VoiceMember>,
    ring_next: Option<Instant>,
    /// What the hub was last told I read, per DM.
    read_sent: HashMap<DmId, DmMessageId>,
    /// Counters for the autotest / diagnostics: ringtone repetitions (requested, even when the
    /// sound itself is off), DM alerts, taskbar flashes.
    pub rings: u32,
    pub dm_alerts: u32,
    pub flashes: u32,
    /// The last call that ended (DM, why), for messages and the autotest.
    pub last_end: Option<(DmId, CallEndReason)>,
    /// v1.1 (E2): this computer's local lines about a person's keys ("O código de segurança de
    /// Fulano mudou", "Fulano adicionou um aparelho"), by person: (when, text). Shown among the
    /// lines of the conversation with them; never sent anywhere.
    pub notes: HashMap<UserId, Vec<(i64, String)>>,
    /// "Enviar sem criptografia?" is open for this DM (the question).
    pub ask_plain: Option<(DmId, String)>,
    /// The person said yes: the next send in this DM may go out unencrypted (just this once).
    plain_ok_once: Option<DmId>,
    /// "Verificar código de segurança" is open for this person.
    pub safety_for: Option<UserId>,
}

impl DmUi {
    pub fn conv(&self, dm: DmId) -> Option<&DmConversation> {
        self.convs.iter().find(|c| c.id == dm)
    }

    pub fn conv_with(&self, user: UserId) -> Option<&DmConversation> {
        self.convs.iter().find(|c| c.peer.id == user)
    }

    pub fn total_unread(&self) -> u32 {
        total_unread(&self.convs)
    }

    /// Did I block this person? (The conversation says so when there is one.)
    pub fn is_blocked(&self, user: UserId) -> bool {
        match self.conv_with(user) {
            Some(c) => c.blocked,
            None => self.blocked_hint.contains(&user),
        }
    }

    /// A call is ringing for me to answer.
    pub fn incoming(&self, me: Option<UserId>) -> Option<&DmCall> {
        self.call.as_ref().filter(|c| c.state == CallState::Ringing && Some(c.callee.id) == me)
    }
}

// ---------------------------------------------------------------- pure rules (unit tested)

/// Most recent activity first (ties: the newer conversation first).
pub fn sort_convs(convs: &mut [DmConversation]) {
    convs.sort_by(|a, b| b.last_activity_ms.cmp(&a.last_activity_ms).then(b.id.cmp(&a.id)));
}

pub fn total_unread(convs: &[DmConversation]) -> u32 {
    convs.iter().filter(|c| !c.blocked).map(|c| c.unread).sum()
}

/// A new line in DM `msg.dm_id`: the list's preview, order and unread count follow. `reading`:
/// that DM is on screen right now (its lines are read as they come). Returns false for a DM the
/// list does not have yet (the caller fetches the list again).
pub fn apply_message(convs: &mut [DmConversation], msg: &DmMessage, me: UserId, reading: bool) -> bool {
    let Some(c) = convs.iter_mut().find(|c| c.id == msg.dm_id) else { return false };
    if c.last_message.as_ref().is_none_or(|m| m.id <= msg.id) {
        c.last_message = Some(msg.clone());
        c.last_activity_ms = c.last_activity_ms.max(msg.created_at_ms);
    }
    if msg.author.id == me {
        // Writing reads everything before it (the hub moves my read pointer too).
        c.unread = 0;
        c.read_up_to = c.read_up_to.max(msg.id);
    } else if reading {
        c.read_up_to = c.read_up_to.max(msg.id);
    } else if msg.id > c.read_up_to {
        c.unread += 1;
    }
    sort_convs(convs);
    true
}

/// I read DM `dm` up to `up_to` (here or on another device). `log`: its lines, when loaded.
pub fn apply_read(convs: &mut [DmConversation], dm: DmId, up_to: DmMessageId, log: Option<&[DmMessage]>, me: UserId) {
    let Some(c) = convs.iter_mut().find(|c| c.id == dm) else { return };
    c.read_up_to = c.read_up_to.max(up_to);
    if c.last_message.as_ref().is_none_or(|m| m.id <= c.read_up_to) {
        c.unread = 0;
    } else if let Some(log) = log {
        c.unread = log.iter().filter(|m| m.author.id != me && m.id > c.read_up_to).count() as u32;
    }
}

/// Line `id` of DM `dm` was deleted (`gone`: the line, if it was loaded).
pub fn apply_delete(convs: &mut [DmConversation], dm: DmId, id: DmMessageId, gone: Option<&DmMessage>, newest_left: Option<&DmMessage>, me: UserId) {
    let Some(c) = convs.iter_mut().find(|c| c.id == dm) else { return };
    if c.last_message.as_ref().is_some_and(|m| m.id == id) {
        c.last_message = newest_left.cloned();
    }
    if let Some(m) = gone {
        if m.author.id != me && m.id > c.read_up_to && c.unread > 0 {
            c.unread -= 1;
        }
    }
}

/// Keep `msgs` ordered by id without duplicates.
pub fn insert_line(msgs: &mut Vec<DmMessage>, m: DmMessage) {
    match msgs.binary_search_by_key(&m.id, |x| x.id) {
        Ok(i) => msgs[i] = m,
        Err(i) => msgs.insert(i, m),
    }
}

/// v1.1 (E2): the question before an encrypted conversation goes out in the clear.
pub fn plain_question(name: &str) -> String {
    format!("{name} não tem nenhum aparelho com criptografia. Enviar sem criptografia?")
}

/// v1.1 (E2): the question when the device list could not be read (whether the line could be
/// encrypted is unknown).
pub fn unknown_question(name: &str) -> String {
    format!("Não foi possível confirmar agora se {name} tem um aparelho com criptografia. Enviar sem criptografia?")
}

/// v1.1 (E2): the safety code's explanation.
pub fn safety_text(name: &str) -> String {
    format!("Compare este código com {name} pessoalmente ou numa chamada. Se for igual, ninguém está no meio.")
}

/// v1.1 (E2): the local line when somebody's keys changed.
pub fn code_changed_text(name: &str) -> String {
    format!("O código de segurança de {name} mudou (aparelho novo ou reinstalação).")
}

/// v1.1 (E2): the local line for each device somebody added.
pub fn device_added_text(name: &str, label: &str) -> String {
    format!("{name} adicionou um aparelho: {label}")
}

/// What a call line says.
pub fn call_line_text(m: &DmMessage, me: UserId) -> String {
    let mine = m.author.id == me;
    let who = m.author.shown_name();
    match m.kind {
        DmMessageKind::CallMissed if mine => "Você ligou: sem resposta".into(),
        DmMessageKind::CallMissed => format!("Chamada perdida de {who}"),
        DmMessageKind::CallDeclined if mine => "Você ligou: chamada recusada".into(),
        DmMessageKind::CallDeclined => format!("{who} ligou: você recusou"),
        DmMessageKind::CallEnded => format!("Chamada de {} · {}", if mine { "você" } else { who }, duration_text(m.call_secs)),
        _ => m.content.clone(),
    }
}

/// "45 s", "3 min", "1 h 05 min".
pub fn duration_text(secs: u32) -> String {
    match secs {
        0..=59 => format!("{secs} s"),
        60..=3599 => format!("{} min", secs / 60),
        _ => format!("{} h {:02} min", secs / 3600, (secs % 3600) / 60),
    }
}

/// The one-line preview under a name in the list.
pub fn preview_text(c: &DmConversation, me: UserId) -> String {
    match &c.last_message {
        None => "Nenhuma mensagem ainda".into(),
        Some(m) if m.kind.is_call() => format!("{ICON_PHONE} {}", call_line_text(m, me)),
        Some(m) if m.author.id == me => format!("Você: {}", one_line(&m.content)),
        Some(m) => one_line(&m.content),
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// "Nova mensagem": people matching `query` (name or display name, any case), by name.
pub fn filter_people<'a>(people: &'a [User], query: &str, me: Option<UserId>) -> Vec<&'a User> {
    let q = query.trim().to_lowercase();
    let mut v: Vec<&User> = people
        .iter()
        .filter(|u| Some(u.id) != me && (q.is_empty() || u.username.to_lowercase().contains(&q) || u.shown_name().to_lowercase().contains(&q)))
        .collect();
    v.sort_by_key(|u| u.shown_name().to_lowercase());
    v
}

/// Should the ringtone sound again now? (While it rings, every [`RING_EVERY`].)
pub fn ring_due(next: Option<Instant>, now: Instant) -> bool {
    next.is_none_or(|t| now >= t)
}

// ---------------------------------------------------------------- the app side

impl App {
    /// "Não perturbe" (profiles): no DM sounds, no ringtone, no taskbar flash; the call prompt and
    /// the unread badges still show.
    pub fn do_not_disturb(&self) -> bool {
        self.notifications_muted()
    }

    /// Open (or start) the conversation with `user_id` and show it. The profile card's "Enviar
    /// mensagem" calls this.
    pub fn open_dm(&mut self, user_id: UserId) {
        if Some(user_id) == self.my_id() {
            return;
        }
        if self.dms.listed && !self.dms.supported {
            self.notice = Some("Este servidor ainda não tem mensagens diretas (peça ao host para atualizar).".into());
            return;
        }
        if let Some(id) = self.dms.conv_with(user_id).map(|c| c.id) {
            self.select_dm(id);
            return;
        }
        self.dms.pending_open = Some(user_id);
        self.backend.send(Cmd::Dm(DmCmd::Open(user_id)));
    }

    /// "Ligar" from a person's menu: the call of our conversation (opened first when there is none).
    pub fn call_user(&mut self, user_id: UserId) {
        if Some(user_id) == self.my_id() || !self.hub_up {
            return;
        }
        if let Some(id) = self.dms.conv_with(user_id).map(|c| c.id) {
            self.start_dm_call(id);
            return;
        }
        self.dms.pending_call = Some(user_id);
        self.open_dm(user_id);
    }

    /// "Bloquear" / "Desbloquear" (the header's options and the person menu). Blocking ends a
    /// call with them.
    pub fn block_user(&mut self, user: &User, blocked: bool) {
        if blocked {
            self.dms.blocked_hint.insert(user.id);
        } else {
            self.dms.blocked_hint.remove(&user.id);
        }
        // (The list the hub sends next is the truth; until then the menu already shows it.)
        if let Some(c) = self.dms.convs.iter_mut().find(|c| c.peer.id == user.id) {
            c.blocked = blocked;
        }
        self.backend.send(Cmd::Dm(DmCmd::Block { user: user.id, blocked }));
        let in_call_with = self.in_dm_call() && self.dms.voice_dm.and_then(|d| self.dm_peer(d)).is_some_and(|p| p.id == user.id);
        if blocked && in_call_with {
            self.hang_up_dm_call();
        }
    }

    /// Show DM `dm` in the center area.
    pub fn select_dm(&mut self, dm: DmId) {
        self.dms.selected = Some(dm);
        self.center = Center::Chat;
        self.dms.focus_input = true;
        let log = self.dms.logs.entry(dm).or_default();
        if !log.requested {
            log.requested = true;
            self.backend.send(Cmd::Dm(DmCmd::History { dm, before: None }));
        }
    }

    /// Enter / "Enviar" in a DM.
    pub fn send_dm(&mut self) {
        let Some(dm) = self.dms.selected else { return };
        // `/calladex` opens the easter egg instead of being sent (`calladex.rs`).
        if self.calladex_command(crate::chat_images::Target::Dm(dm)) {
            return;
        }
        let text = self.dms.input.trim().to_string();
        // E2: an encrypted conversation never goes out in the clear without asking.
        if !text.is_empty() || self.images.pending_for(crate::chat_images::Target::Dm(dm)) > 0 {
            if let Some(q) = self.dm_plain_question(dm) {
                self.dms.ask_plain = Some((dm, q));
                return;
            }
        }
        if text.chars().count() <= DM_MESSAGE_MAX_CHARS && self.send_with_images(crate::chat_images::Target::Dm(dm), text.clone()) {
            return;
        }
        if text.is_empty() {
            return;
        }
        if text.chars().count() > DM_MESSAGE_MAX_CHARS {
            self.notice = Some(format!("A mensagem passou de {DM_MESSAGE_MAX_CHARS} caracteres."));
            return;
        }
        let Some(mode) = self.dm_send_mode(dm) else { return };
        self.dms.input.clear();
        let extras = self.take_send_extras(crate::chat_images::Target::Dm(dm), &text);
        self.backend.send(Cmd::Dm(DmCmd::Send { dm, content: text, reply_to: extras.reply_to, mode }));
    }

    /// v1.1 (E2): how a line in `dm` goes out (the backend seals it when it can).
    pub fn dm_send_mode(&self, dm: DmId) -> Option<SendMode> {
        let peer = self.dms.conv(dm)?.peer.id;
        Some(SendMode { peer, plain_ok: self.dms.plain_ok_once == Some(dm) })
    }

    /// v1.1 (E2): `dm` was end-to-end encrypted (on this computer, or its lines say so).
    pub fn dm_was_sealed(&self, dm: DmId) -> bool {
        self.privacy.view.sealed_dms.contains(&dm)
            || self.dms.logs.get(&dm).is_some_and(|l| l.msgs.iter().any(e2dm::is_e2e))
            || self.dms.conv(dm).and_then(|c| c.last_message.as_ref()).is_some_and(e2dm::is_e2e)
    }

    /// v1.1 (E2): the next line of `dm` would go out unencrypted although the conversation was
    /// encrypted: the question to ask first ("Fulano não tem nenhum aparelho com criptografia.
    /// Enviar sem criptografia?"). `None` while this computer is still registering (the backend
    /// waits for the device list and refuses rather than go plain).
    pub fn dm_plain_question(&self, dm: DmId) -> Option<String> {
        use crate::backend::e2ee::E2eeStatus;
        let conv = self.dms.conv(dm)?;
        let v = &self.privacy.view;
        if self.dms.plain_ok_once == Some(dm) || v.can_seal_for(conv.peer.id) || v.status == E2eeStatus::Starting || !self.dm_was_sealed(dm) {
            return None;
        }
        let peer_has_one = v.devices_of.get(&conv.peer.id).is_some_and(|d| !d.is_empty());
        Some(if peer_has_one && v.status != E2eeStatus::Active {
            "Este computador está sem criptografia agora. Enviar sem criptografia?".to_string()
        } else {
            plain_question(conv.peer.shown_name())
        })
    }

    /// v1.1 (E2): the backend refused to send in `dm` in the clear (an encrypted conversation it
    /// cannot encrypt now, or a device list it could not read): ask, as [`Self::dm_plain_question`]
    /// does before sending. Whether it asked.
    pub(crate) fn dm_refused_plain(&mut self, dm: DmId, error: &str) -> bool {
        let Some(name) = self.dms.conv(dm).map(|c| c.peer.shown_name().to_string()) else { return false };
        let q = if error == e2dm::DIRECTORY_UNKNOWN {
            unknown_question(&name)
        } else if error == e2dm::REFUSED_PLAIN {
            plain_question(&name)
        } else if error == e2dm::THIS_DEVICE_OFF {
            "Este computador está sem criptografia agora. Enviar sem criptografia?".to_string()
        } else {
            return false;
        };
        tracing::info!(dm, "e2ee: asking before sending without encryption");
        self.dms.ask_plain = Some((dm, q));
        true
    }

    /// Viewer counts ("👁") come from the hub's voice state, which never has a private call: they
    /// are not shown during one.
    pub fn viewers_shown(&self) -> bool {
        self.viewers_supported && !self.in_dm_call()
    }

    pub fn in_dm_call(&self) -> bool {
        self.voice.channel == Some(DM_CALL_CHANNEL) || self.voice.joining == Some(DM_CALL_CHANNEL)
    }

    /// The person of the DM call (or of DM `dm`).
    fn dm_peer(&self, dm: DmId) -> Option<User> {
        self.dms.conv(dm).map(|c| c.peer.clone()).or_else(|| {
            let me = self.my_id()?;
            self.dms.call.as_ref().filter(|c| c.dm_id == dm).map(|c| c.peer_of(me).clone())
        })
    }

    /// "Chamada com Fulano" (the voice bar and the call view's title).
    pub fn dm_call_title(&self) -> String {
        let name = self.dms.voice_dm.and_then(|dm| self.dm_peer(dm)).map(|u| u.shown_name().to_string()).unwrap_or_default();
        let ringing = self.dms.call.as_ref().is_some_and(|c| c.state == CallState::Ringing && Some(c.caller.id) == self.my_id());
        if ringing {
            format!("{ICON_PHONE} Chamando {name}...")
        } else {
            format!("{ICON_PHONE} Chamada com {name}")
        }
    }

    fn voice_opts(&self) -> crate::backend::VoiceOpts {
        crate::backend::VoiceOpts {
            input_device: self.settings.input_device.clone(),
            output_device: self.settings.output_device.clone(),
            volume_pct: self.settings.volume_pct,
            ptt: self.ptt_active(),
            user_audio: self.saved_audio_for_known_users(),
            others_muted: self.voice.others_muted,
            gate: self.gate_voice.clone(),
            noise: self.noise.clone(),
        }
    }

    /// Join DM `dm`'s call room (ringing it first when `accept` is false and there is no call).
    fn join_dm_call(&mut self, dm: DmId, accept: bool) {
        if self.dms.voice_dm == Some(dm) && self.in_dm_call() {
            self.center = Center::Call;
            return;
        }
        // One call at a time: a call in another DM ends first (a voice channel is left by the
        // backend, and the hub does the same).
        if let Some(other) = self.dms.voice_dm.filter(|d| *d != dm) {
            self.backend.send(Cmd::Dm(DmCmd::End { dm: other }));
        }
        self.dms.voice_dm = Some(dm);
        self.dms.room = None;
        self.dms.room_peers.clear();
        self.dms.live_peers.clear();
        self.voice.joining = Some(DM_CALL_CHANNEL);
        self.voice.joining_since = Some(Instant::now());
        self.voice.error = None;
        self.voice.retry = None;
        self.voice.warning = None;
        self.center = Center::Call;
        let opts = self.voice_opts();
        self.backend.send(Cmd::JoinDmCall { dm, accept, opts });
    }

    /// "Ligar" in DM `dm`'s header (also "Entrar na chamada" when its call is going on).
    pub fn start_dm_call(&mut self, dm: DmId) {
        self.join_dm_call(dm, false);
    }

    /// "Atender".
    pub fn accept_dm_call(&mut self) {
        let Some(call) = self.dms.incoming(self.my_id()).cloned() else { return };
        self.dms.ring_next = None;
        self.join_dm_call(call.dm_id, true);
    }

    /// "Recusar".
    pub fn decline_dm_call(&mut self) {
        let Some(call) = self.dms.incoming(self.my_id()).cloned() else { return };
        self.dms.ring_next = None;
        self.dms.call = None;
        self.backend.send(Cmd::Dm(DmCmd::Decline { dm: call.dm_id }));
    }

    /// "Desligar": end the call on the hub and leave its room.
    pub fn hang_up_dm_call(&mut self) {
        if let Some(dm) = self.dms.voice_dm.take() {
            self.backend.send(Cmd::Dm(DmCmd::End { dm }));
        }
        self.dms.call = None;
        if self.in_dm_call() {
            self.leave_voice();
        }
    }

    /// Called before joining a voice channel: as on Discord, that leaves the DM call.
    pub(super) fn dm_before_voice_channel(&mut self) {
        if let Some(dm) = self.dms.voice_dm.take() {
            self.backend.send(Cmd::Dm(DmCmd::End { dm }));
            self.dms.call = None;
        }
    }

    /// The voice session is being left (any button): a DM call ends with it.
    pub(super) fn dm_leaving_voice(&mut self) {
        if self.in_dm_call() {
            if let Some(dm) = self.dms.voice_dm.take() {
                self.backend.send(Cmd::Dm(DmCmd::End { dm }));
            }
            self.dms.call = None;
        }
    }

    pub(super) fn dm_reset(&mut self) {
        self.dms = DmUi::default();
    }

    /// A hub snapshot (first connect or reconnect): fetch the list, catch up on open DMs.
    pub(super) fn dm_on_ready(&mut self) {
        self.backend.send(Cmd::Dm(DmCmd::List));
        let reload: Vec<DmId> = self.dms.logs.iter().filter(|(_, l)| l.requested).map(|(id, _)| *id).collect();
        for dm in reload {
            self.backend.send(Cmd::Dm(DmCmd::History { dm, before: None }));
        }
        self.dms.read_sent.clear();
    }

    /// The DM is on screen, the window has focus: its lines are being read.
    fn dm_reading(&self, dm: DmId, ctx: Option<&egui::Context>) -> bool {
        let focused = !self.notify.hidden && self.notify.focus_override.unwrap_or_else(|| ctx.is_none_or(|c| c.input(|i| i.viewport().focused.unwrap_or(true))));
        self.screen == Screen::Main && self.center == Center::Chat && self.dms.selected == Some(dm) && focused
    }

    // ------------------------------------------------------------ events

    pub(super) fn on_dm_server_event(&mut self, ev: ServerEvent) {
        let Some(me) = self.my_id() else { return };
        let ctx = self.repaint.ctx();
        match ev {
            ServerEvent::DmMessageCreated { message } => {
                let dm = message.dm_id;
                self.chatx.note_user(&message.author);
                let mine = message.author.id == me;
                let reading = self.dm_reading(dm, ctx.as_ref());
                if !apply_message(&mut self.dms.convs, &message, me, reading) {
                    self.backend.send(Cmd::Dm(DmCmd::List));
                }
                if !mine && !reading {
                    if !self.do_not_disturb() && self.settings.alerts.allows(AlertKind::DirectMessage) {
                        self.dms.dm_alerts += 1;
                        self.remote_alert(AlertKind::DirectMessage, message.author.id);
                    }
                    // The taskbar flash and the Windows notification (see `notify.rs`).
                    self.dm_message_arrived(&message, me);
                }
                let log = self.dms.logs.entry(dm).or_default();
                if log.requested {
                    insert_line(&mut log.msgs, message);
                    if log.msgs.len() > MAX_KEPT {
                        let extra = log.msgs.len() - MAX_KEPT;
                        log.msgs.drain(..extra);
                    }
                }
            }
            ServerEvent::DmMessageDeleted { dm_id, message_id } => {
                let (gone, newest) = match self.dms.logs.get_mut(&dm_id) {
                    Some(log) => {
                        let gone = log.msgs.iter().position(|m| m.id == message_id).map(|i| log.msgs.remove(i));
                        (gone, log.msgs.last().cloned())
                    }
                    None => (None, None),
                };
                apply_delete(&mut self.dms.convs, dm_id, message_id, gone.as_ref(), newest.as_ref(), me);
                // Its images, chips and a reply being written to it go; replies to it now quote
                // "mensagem apagada" (v0.8).
                let atts = gone.map(|m| m.attachments).unwrap_or_default();
                self.forget_message(crate::chat_images::Target::Dm(dm_id), message_id, &atts);
            }
            ServerEvent::DmRead { dm_id, up_to } => {
                let log = self.dms.logs.get(&dm_id).filter(|l| l.requested).map(|l| l.msgs.as_slice());
                apply_read(&mut self.dms.convs, dm_id, up_to, log, me);
            }
            ServerEvent::CallInvite { call } => {
                tracing::info!(call = call.id, dm = call.dm_id, caller = %call.caller.username, mine = call.caller.id == me, "dm call ringing");
                let incoming = call.callee.id == me;
                // A call from somebody I am already talking to in another call still rings (I decide).
                self.dms.call = Some(call);
                if incoming {
                    self.dms.ring_next = Some(Instant::now());
                    self.flash_taskbar(ctx.as_ref());
                }
            }
            ServerEvent::CallAccepted { call } => {
                tracing::info!(call = call.id, dm = call.dm_id, "dm call answered");
                self.dms.ring_next = None;
                self.dms.call = Some(call);
            }
            ServerEvent::CallDeclined { dm_id, .. } => self.dm_call_over(dm_id, None),
            ServerEvent::CallEnded { dm_id, reason, .. } => self.dm_call_over(dm_id, Some(reason)),
            _ => {}
        }
    }

    /// The call of `dm` is over (`None`: declined).
    fn dm_call_over(&mut self, dm: DmId, reason: Option<CallEndReason>) {
        tracing::info!(dm, ?reason, "dm call over");
        self.dms.last_end = Some((dm, reason.unwrap_or(CallEndReason::Other)));
        if self.dms.call.as_ref().is_some_and(|c| c.dm_id == dm) {
            self.dms.call = None;
        }
        self.dms.ring_next = None;
        if self.dms.voice_dm == Some(dm) {
            self.dms.voice_dm = None;
            let name = self.dm_peer(dm).map(|u| u.shown_name().to_string()).unwrap_or_default();
            if self.in_dm_call() {
                self.leave_voice();
            }
            let text = match reason {
                None => format!("{ICON_PHONE} {name} recusou a chamada."),
                Some(CallEndReason::Missed) => format!("{ICON_PHONE} {name} não atendeu."),
                Some(CallEndReason::JoinedVoice) | Some(CallEndReason::Hangup) | Some(CallEndReason::Cancelled) => format!("{ICON_PHONE} Chamada encerrada."),
                Some(CallEndReason::Offline) => format!("{ICON_PHONE} Chamada encerrada: {name} ficou sem conexão."),
                Some(CallEndReason::Other) => format!("{ICON_PHONE} Chamada encerrada."),
            };
            self.toast(text);
        }
    }

    /// The backend's answers.
    pub(super) fn on_dm_event(&mut self, ev: DmEv) {
        match ev {
            DmEv::Listed(DmsReply::Ready(mut list)) => {
                sort_convs(&mut list);
                self.dms.supported = true;
                self.dms.listed = true;
                self.dms.convs = list;
                tracing::info!(convs = self.dms.convs.len(), unread = self.dms.total_unread(), "dm list");
                if self.dms.selected.is_some_and(|s| self.dms.conv(s).is_none()) {
                    self.dms.selected = None;
                }
            }
            DmEv::Listed(DmsReply::Unsupported) => {
                tracing::info!("the hub has no direct messages (older server): DMs hidden");
                self.dms.supported = false;
                self.dms.listed = true;
            }
            DmEv::Listed(DmsReply::Failed(e)) => tracing::warn!(error = %e, "dm list failed"),
            DmEv::People(r) => {
                self.dms.picker.loading = false;
                match r {
                    Ok(p) => {
                        // (Also the member list of the "@" mentions.)
                        self.chat_people(&p);
                        self.dms.picker.people = p;
                        self.dms.picker.error = None;
                    }
                    Err(e) => self.dms.picker.error = Some(e),
                }
            }
            DmEv::Opened(Ok(conv)) => {
                let id = conv.id;
                let wanted = self.dms.pending_open.take() == Some(conv.peer.id);
                let call = self.dms.pending_call.take() == Some(conv.peer.id) && !conv.blocked;
                self.dms.supported = true;
                match self.dms.convs.iter_mut().find(|c| c.id == id) {
                    Some(c) => *c = conv,
                    None => self.dms.convs.push(conv),
                }
                sort_convs(&mut self.dms.convs);
                if wanted {
                    self.select_dm(id);
                }
                if call {
                    self.start_dm_call(id);
                }
            }
            DmEv::Opened(Err(e)) => {
                self.dms.pending_open = None;
                self.dms.pending_call = None;
                self.notice = Some(format!("Não foi possível abrir a conversa: {e}"));
            }
            DmEv::History { dm, before, result } => {
                let log = self.dms.logs.entry(dm).or_default();
                log.loading_older = false;
                match result {
                    Ok(page) => {
                        let complete = page.len() < HISTORY_PAGE as usize;
                        if before.is_some() || log.msgs.is_empty() {
                            log.no_more = complete;
                        }
                        // The page is the hub's truth for what it covers (lines deleted meanwhile
                        // go, an id an older hub reused shows the new line), see `merge_page`.
                        let same = |a: &DmMessage, b: &DmMessage| a.author.id == b.author.id && a.created_at_ms == b.created_at_ms;
                        let out = super::chat_extras::merge_page(&mut log.msgs, page, before, complete, |m| m.id, same);
                        let target = crate::chat_images::Target::Dm(dm);
                        for m in out.deleted {
                            self.forget_message(target, m.id, &m.attachments);
                        }
                        for m in out.replaced {
                            self.forget_images(&m.attachments);
                        }
                        self.chat_on_history(target);
                    }
                    Err(e) => {
                        log.no_more = true;
                        self.notice = Some(format!("Não foi possível carregar a conversa: {e}"));
                    }
                }
            }
            DmEv::Sent { dm, content, result } => match result {
                Ok(m) => {
                    if let Some(me) = self.my_id() {
                        apply_message(&mut self.dms.convs, &m, me, true);
                    }
                    let log = self.dms.logs.entry(dm).or_default();
                    if log.requested {
                        insert_line(&mut log.msgs, m);
                    }
                }
                Err(e) => {
                    // Give the text back so nothing typed is lost.
                    if self.dms.input.is_empty() && self.dms.selected == Some(dm) {
                        self.dms.input = content;
                    }
                    if !self.dm_refused_plain(dm, &e) {
                        self.notice = Some(format!("Mensagem não enviada: {e}"));
                    }
                }
            },
            DmEv::Done { cmd, result } => match (cmd, result) {
                (DmCmd::Block { .. }, Ok(())) => self.backend.send(Cmd::Dm(DmCmd::List)),
                (DmCmd::Read { .. } | DmCmd::End { .. }, Err(e)) => tracing::debug!(error = %e, "dm call failed quietly"),
                (cmd, Err(e)) => {
                    tracing::warn!(?cmd, error = %e, "dm request failed");
                    self.notice = Some(e);
                }
                _ => {}
            },
        }
    }

    /// Voice events, for the DM call's members (who is in the room, who is live).
    pub(super) fn dm_on_voice(&mut self, ev: &VoiceEvent) {
        if !self.in_dm_call() {
            return;
        }
        match ev {
            VoiceEvent::Connected { room, participants, .. } => {
                tracing::info!(room, others = participants.len(), "dm call room joined");
                self.dms.room = Some(room.clone());
                self.dms.room_peers = participants.iter().cloned().collect();
            }
            VoiceEvent::ParticipantJoined { identity, .. } => {
                self.dms.room_peers.insert(identity.clone());
            }
            VoiceEvent::ParticipantLeft { identity } => {
                self.dms.room_peers.remove(identity);
                self.dms.live_peers.remove(identity);
            }
            VoiceEvent::ShareStarted { identity } => {
                self.dms.live_peers.insert(identity.clone());
            }
            VoiceEvent::ShareStopped { identity } => {
                self.dms.live_peers.remove(identity);
            }
            VoiceEvent::Disconnected { .. } => {
                // The room dropped us: the call is over for the hub too.
                if let Some(dm) = self.dms.voice_dm.take() {
                    self.backend.send(Cmd::Dm(DmCmd::End { dm }));
                }
                self.dms.call = None;
            }
            _ => {}
        }
        self.refresh_call_members();
    }

    /// The call view's members while in a DM call: me, and the other person once in the room.
    fn refresh_call_members(&mut self) {
        let mut members = Vec::new();
        let in_call = self.voice.channel == Some(DM_CALL_CHANNEL);
        if let (true, Some(me), Some(dm)) = (in_call, self.me.clone(), self.dms.voice_dm) {
            members.push(VoiceMember {
                user: me,
                channel_id: Some(DM_CALL_CHANNEL),
                muted: self.voice.muted,
                deafened: self.voice.deafened,
                streaming: !matches!(self.voice.share, super::Share::Off),
                watching: Vec::new(),
                camera: self.camera_on(),
                server_muted: false,
                audio_only: self.voice.share_audio_only,
            });
            if let Some(peer) = self.dm_peer(dm) {
                let id = peer.id.to_string();
                if self.dms.room_peers.contains(&id) {
                    // A screen live shows as a Go Live track; a "Só áudio" one only as its kind.
                    let streaming = self.dms.live_peers.contains(&id) || self.voice.audio_only.contains(&id);
                    members.push(VoiceMember {
                        user: peer,
                        channel_id: Some(DM_CALL_CHANNEL),
                        muted: false,
                        deafened: false,
                        streaming,
                        watching: Vec::new(),
                        camera: self.voice.cameras.contains(&id),
                        server_muted: false,
                        audio_only: streaming && self.voice.audio_only.contains(&id),
                    });
                }
            }
        }
        self.dms.call_members = members;
    }

    /// Once per frame (from `logic`): the ringtone, the call members, read pointers.
    pub(super) fn dm_tick(&mut self, ctx: &egui::Context) {
        // A call that never got going (the hub refused it, the room could not be joined) leaves
        // nothing behind.
        if !self.in_dm_call() && self.dms.call.as_ref().is_none_or(|c| Some(c.dm_id) != self.dms.voice_dm) {
            self.dms.voice_dm = None;
        }
        self.refresh_call_members();
        let now = Instant::now();
        let me = self.my_id();
        // Ringtone: while a call rings for me (the hub ends it after CALL_RING_SECS; the extra
        // second is a safety net if that event never comes).
        match self.dms.incoming(me).cloned() {
            Some(call) => {
                let age_ms = now_ms() - call.started_at_ms;
                if age_ms > (CALL_RING_SECS as i64 + 5) * 1000 {
                    self.dms.ring_next = None;
                } else if ring_due(self.dms.ring_next, now) {
                    self.dms.ring_next = Some(now + RING_EVERY);
                    self.dms.rings += 1;
                    if !self.do_not_disturb() && self.settings.alerts.allows(AlertKind::Ring) {
                        self.send_alert(AlertKind::Ring, false);
                    }
                }
                if let Some(t) = self.dms.ring_next {
                    ctx.request_repaint_after(t.saturating_duration_since(now));
                }
            }
            None => self.dms.ring_next = None,
        }
        // Read pointer: the DM on screen is read up to its newest line.
        if let Some(dm) = self.dms.selected {
            if self.dm_reading(dm, Some(ctx)) {
                let newest = self.dms.logs.get(&dm).and_then(|l| l.msgs.last()).map(|m| m.id);
                let conv_read = self.dms.conv(dm).map(|c| (c.read_up_to, c.unread));
                if let (Some(newest), Some((read, unread))) = (newest, conv_read) {
                    let sent = self.dms.read_sent.get(&dm).copied().unwrap_or(0);
                    if (newest > read || unread > 0) && newest > sent {
                        self.dms.read_sent.insert(dm, newest);
                        self.backend.send(Cmd::Dm(DmCmd::Read { dm, up_to: newest }));
                        if let Some(me) = me {
                            let log = self.dms.logs.get(&dm).map(|l| l.msgs.as_slice());
                            apply_read(&mut self.dms.convs, dm, newest, log, me);
                        }
                    }
                }
            }
        }
    }

    /// Blink the taskbar button when the window is not in front (and "Não perturbe" is off).
    fn flash_taskbar(&mut self, ctx: Option<&egui::Context>) {
        // (Autotests simulate the focus: `notify.focus_override`.)
        let focused = self.notify.focus_override.unwrap_or_else(|| ctx.is_some_and(|c| c.input(|i| i.viewport().focused.unwrap_or(false))));
        if focused || self.do_not_disturb() {
            return;
        }
        self.dms.flashes += 1;
        self.platform_flash();
    }

    // ------------------------------------------------------------ drawing

    /// "Mensagens diretas" at the top of the sidebar (v1.0: highlighted only while open in the
    /// center, like the channels).
    pub(super) fn dm_sidebar(&mut self, ui: &mut egui::Ui) {
        if !self.dms.supported {
            return;
        }
        let me = self.my_id().unwrap_or_default();
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            let unread = self.dms.total_unread();
            let title = if unread > 0 { format!("Mensagens diretas ({unread})") } else { "Mensagens diretas".to_string() };
            crate::ui_kit::section(ui, &title);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if crate::ui_kit::icon_button(ui, crate::theme::ph::PLUS, "Nova mensagem", crate::ui_kit::IconState::Normal).clicked() {
                    self.open_dm_picker();
                }
            });
        });
        let convs: Vec<DmConversation> = self.dms.convs.clone();
        if convs.is_empty() {
            let r = crate::ui_kit::sidebar_row(ui, crate::ui_kit::Row { icon: crate::theme::ph::PLUS, name: "Nova mensagem", label: "Nova mensagem", selected: false, unread: false, count: 0, mentions: 0, hover: None }).0;
            if r.clicked() {
                self.open_dm_picker();
            }
        }
        let open = self.open_in_center();
        for c in convs {
            let selected = open == crate::layout::Open::Dm(c.id);
            let calling = self.dms.call.as_ref().is_some_and(|k| k.dm_id == c.id);
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), egui::Sense::click());
            if ui.is_rect_visible(rect) {
                if selected {
                    ui.painter().rect_filled(rect, crate::theme::radius::ROW, crate::theme::ROW_SELECTED);
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, crate::theme::radius::ROW, crate::theme::ROW_HOVER);
                }
                let p = ui.painter_at(rect);
                let center = egui::pos2(rect.left() + 8.0 + ROW_AVATAR / 2.0, rect.center().y);
                let ctx = ui.ctx().clone();
                crate::avatars::draw_avatar(&mut self.avatars.store, &p, &ctx, center, ROW_AVATAR / 2.0, &c.peer, crate::avatars::Surface::Sidebar);
                let tx = rect.left() + 18.0 + ROW_AVATAR;
                // The name and the preview end before the badges (v0.8.2: the same layout as the
                // channels', so a "99+" never runs into the name).
                let show_count = c.unread > 0 && !c.blocked;
                let count_w = show_count.then(|| super::notify::pill_width(p.layout_no_wrap(super::notify::count_text(c.unread), crate::theme::semibold(11.0), Color32::WHITE).size().x));
                let layout = super::notify::row_layout(rect, tx, count_w, false, if calling { 16.0 } else { 0.0 });
                let w = layout.name.width().max(1.0);
                let strong = (c.unread > 0 && !c.blocked) || selected || resp.hovered();
                let name_color = if strong { crate::theme::GIZ } else { crate::theme::NEVOA };
                let name_font = if c.unread > 0 && !c.blocked { crate::theme::semibold(14.0) } else { crate::theme::body(14.0) };
                let mut name = c.peer.shown_name().to_string();
                if c.blocked {
                    name.push_str(" (bloqueado)");
                }
                let mut drawn = super::notify::DrawnRow { label: name.clone(), layout, text_right: tx, elided: false, selected };
                let preview_color = crate::theme::NEVOA.gamma_multiply(0.85);
                for (i, (text, font, color, dy)) in [(name, name_font, name_color, 3.0), (preview_text(&c, me), crate::theme::body(11.5), preview_color, 21.0)].into_iter().enumerate() {
                    let g = super::notify::fit_name(&p, &text, font, color, w);
                    if i == 0 {
                        (drawn.text_right, drawn.elided) = ((tx + g.size().x).min(layout.name.right()), g.elided);
                    }
                    p.with_clip_rect(layout.name.intersect(p.clip_rect())).galley(egui::pos2(tx, rect.top() + dy), g, color);
                }
                super::notify::paint_badges(&p, &layout, c.unread);
                if let Some(icon) = layout.icon {
                    p.text(icon.center(), egui::Align2::CENTER_CENTER, ICON_PHONE, crate::theme::body(14.0), GREEN);
                }
                self.sidebar_rows.push(drawn);
            }
            let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
            self.menu_hit(&resp, &c.peer, Origin::Dm, Lead::None);
            if resp.clicked() {
                self.select_dm(c.id);
            }
        }
        ui.add_space(10.0);
    }

    pub fn open_dm_picker(&mut self) {
        self.dms.picker.open = true;
        self.dms.picker.filter.clear();
        self.dms.picker.loading = true;
        self.dms.picker.error = None;
        self.backend.send(Cmd::Dm(DmCmd::People));
    }

    /// The DM header's buttons (right to left): the options (the person's menu), then the call:
    /// "Ligar", "Entrar na chamada", "Atender" / "Recusar", or "Desligar" / "Ver chamada".
    pub(super) fn dm_header_actions(&mut self, ui: &mut egui::Ui, dm: DmId, conv: &DmConversation) {
        use crate::ui_kit::{button, icon_button, IconState, Kind};
        let me = self.my_id().unwrap_or_default();
        let peer = conv.peer.clone();
        let call_here = self.dms.call.as_ref().filter(|c| c.dm_id == dm).cloned();
        let in_this_call = self.in_dm_call() && self.dms.voice_dm == Some(dm);
        let opts = icon_button(ui, crate::theme::ph::DOTS_THREE, "Opções", IconState::Normal);
        egui::Popup::menu(&opts).frame(crate::ui_kit::popup_frame(ui.style())).style(crate::ui_kit::menu_style).show(|ui| {
            let pos = ui.min_rect().left_top();
            // v1.1 (E2): the safety code with this person.
            let why = (!self.privacy.view.safety.contains_key(&peer.id)).then_some("Disponível quando vocês dois tiverem criptografia neste servidor");
            if crate::ui_kit::menu_item(ui, crate::theme::ph::SHIELD, SAFETY_TITLE, false, why) {
                self.dms.safety_for = Some(peer.id);
            }
            crate::ui_kit::menu_separator(ui);
            self.person_menu_ui(ui, &peer, Origin::Dm, Lead::None, pos);
        });
        ui.add_space(6.0);
        if in_this_call {
            if button(ui, Kind::Danger, Some(crate::theme::ph::PHONE_X), "Desligar").clicked() {
                self.hang_up_dm_call();
            }
            if self.center != Center::Call && button(ui, Kind::Secondary, Some(crate::theme::ph::SQUARES_FOUR), "Ver chamada").clicked() {
                self.center = Center::Call;
            }
        } else if call_here.as_ref().is_some_and(|c| c.state == CallState::Ringing && c.callee.id == me) {
            if button(ui, Kind::Danger, Some(crate::theme::ph::PHONE_X), "Recusar").clicked() {
                self.decline_dm_call();
            }
            // (Secondary here: the ringing window's "Atender" is the view's one lamp button.)
            if button(ui, Kind::Secondary, Some(ICON_PHONE), "Atender").clicked() {
                self.accept_dm_call();
            }
        } else if call_here.is_some() {
            if button(ui, Kind::Primary, Some(ICON_PHONE), "Entrar na chamada").clicked() {
                self.start_dm_call(dm);
            }
        } else {
            let r = ui.add_enabled_ui(!conv.blocked && self.hub_up, |ui| button(ui, Kind::Secondary, Some(ICON_PHONE), "Ligar")).inner;
            let why = if conv.blocked { "Você bloqueou esta pessoa: desbloqueie para ligar" } else { "Sem conexão com o servidor" };
            if r.on_disabled_hover_text(why).on_hover_text(format!("Chamada de voz privada com {} (só vocês dois)", peer.shown_name())).clicked() {
                self.start_dm_call(dm);
            }
        }
    }

    /// v1.1 (E2): "Enviar sem criptografia?" and "Verificar código de segurança".
    fn e2e_windows(&mut self, ctx: &egui::Context) {
        use crate::theme::{self, ph, space};
        use crate::ui_kit::{self, IconState, Kind};
        if let Some((dm, question)) = self.dms.ask_plain.clone() {
            let mut answer = None;
            let was_sealed = self.dm_was_sealed(dm);
            egui::Window::new("Enviar sem criptografia?")
                .id(egui::Id::new("dm_ask_plain"))
                .title_bar(false)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .frame(ui_kit::dialog_frame(&ctx.global_style()))
                .show(ctx, |ui| {
                    ui.set_width(ui_kit::dialog_w(ctx, 420.0));
                    ui.spacing_mut().item_spacing.y = 8.0;
                    ui_kit::dialog_title(ui, &format!("{}  Enviar sem criptografia?", ph::LOCK_SIMPLE_OPEN));
                    ui.add(egui::Label::new(RichText::new(&question).font(theme::body(13.5)).color(theme::TEXTO)).wrap());
                    ui_kit::hint(
                        ui,
                        if was_sealed {
                            "Esta conversa era criptografada de ponta a ponta. Sem criptografia, quem hospeda o servidor consegue ler a mensagem."
                        } else {
                            "Sem criptografia, quem hospeda o servidor consegue ler a mensagem."
                        },
                    );
                    ui.add_space(space::S);
                    ui.horizontal(|ui| {
                        if ui_kit::button(ui, Kind::Danger, Some(ph::LOCK_SIMPLE_OPEN), "Enviar sem criptografia").clicked() {
                            answer = Some(true);
                        }
                        if ui_kit::button(ui, Kind::Secondary, None, "Cancelar").clicked() {
                            answer = Some(false);
                        }
                    });
                });
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                answer = Some(false);
            }
            match answer {
                Some(true) => {
                    self.dms.ask_plain = None;
                    tracing::info!(dm, "e2ee: the person agreed to send without encryption");
                    self.dms.plain_ok_once = Some(dm);
                    if self.dms.selected == Some(dm) {
                        self.send_dm();
                    }
                    self.dms.plain_ok_once = None;
                }
                Some(false) => self.dms.ask_plain = None,
                None => {}
            }
        }
        let Some(peer_id) = self.dms.safety_for else { return };
        let Some(peer) = self.dms.conv_with(peer_id).map(|c| c.peer.clone()) else {
            self.dms.safety_for = None;
            return;
        };
        let safety = self.privacy.view.safety.get(&peer_id).cloned();
        let mut close = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let mut mark = None;
        egui::Window::new(SAFETY_TITLE)
            .id(egui::Id::new("dm_safety_code"))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(ui_kit::dialog_frame(&ctx.global_style()))
            .show(ctx, |ui| {
                let w = ui_kit::dialog_w(ctx, 440.0);
                ui.set_width(w);
                ui.spacing_mut().item_spacing.y = 8.0;
                ui.horizontal(|ui| {
                    ui_kit::dialog_title(ui, &format!("{}  {SAFETY_TITLE}", ph::SHIELD));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui_kit::icon_button(ui, ph::X, "Fechar", IconState::Normal).clicked() {
                            close = true;
                        }
                    });
                });
                let Some(s) = safety else {
                    ui_kit::hint(ui, format!("{} ainda não tem um aparelho com criptografia neste servidor (ou este computador ainda não tem).", peer.shown_name()));
                    return;
                };
                ui.add(egui::Label::new(RichText::new(safety_text(peer.shown_name())).font(theme::body(13.5)).color(theme::TEXTO)).wrap());
                ui.add_space(space::S);
                let groups: Vec<&str> = s.code.split(' ').collect();
                egui::Frame::new().fill(theme::NOITE).corner_radius(8.0).inner_margin(egui::Margin::symmetric(16, 12)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.vertical_centered(|ui| {
                        for row in groups.chunks(3) {
                            ui.label(RichText::new(row.join("   ")).font(egui::FontId::monospace(22.0)).color(theme::GIZ));
                        }
                    });
                });
                if s.verified {
                    ui.label(RichText::new(format!("{}  Verificado neste computador", ph::CHECK_CIRCLE)).font(theme::semibold(13.0)).color(theme::ONLINE));
                } else if s.reverify {
                    ui_kit::notice_frame(ui_kit::Notice::Warning).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui_kit::notice_text(ui, ui_kit::Notice::Warning, &format!("{REVERIFY}: o código de segurança de {} mudou desde que você verificou.", peer.shown_name()));
                    });
                }
                ui.add_space(space::S);
                ui.horizontal(|ui| {
                    if s.verified {
                        if ui_kit::button(ui, Kind::Secondary, None, UNVERIFY).clicked() {
                            mark = Some(false);
                        }
                    } else if ui_kit::button(ui, Kind::Primary, Some(ph::CHECK), VERIFY).clicked() {
                        mark = Some(true);
                    }
                    if ui_kit::button(ui, Kind::Ghost, None, "Fechar").clicked() {
                        close = true;
                    }
                });
            });
        if let Some(on) = mark {
            self.backend.send(Cmd::E2ee(crate::backend::e2ee::E2eeCmd::Verify { user: peer_id, on }));
        }
        if close {
            self.dms.safety_for = None;
        }
    }

    /// v1.1 (E2): somebody's keys changed (`E2eeEv::PeerKeys`): the local lines in the
    /// conversation with them.
    pub(crate) fn dm_peer_keys(&mut self, user: UserId, added: &[String]) {
        let name = self.dms.conv_with(user).map(|c| c.peer.shown_name().to_string()).or_else(|| self.online.iter().find(|u| u.id == user).map(|u| u.shown_name().to_string())).unwrap_or_else(|| "Esta pessoa".into());
        let at = now_ms();
        let notes = self.dms.notes.entry(user).or_default();
        notes.push((at, code_changed_text(&name)));
        for label in added {
            notes.push((at, device_added_text(&name, label)));
        }
    }

    /// "Nova mensagem" (the people picker) and the incoming-call prompt, in the kit's frames.
    pub(super) fn dm_windows(&mut self, ctx: &egui::Context) {
        use crate::theme::{self, ph, space};
        use crate::ui_kit::{self, IconState, Kind};
        self.e2e_windows(ctx);
        if self.dms.picker.open {
            let mut open = !ctx.input(|i| i.key_pressed(egui::Key::Escape));
            let mut pick = None;
            egui::Window::new("Nova mensagem")
                .id(egui::Id::new("dm_picker"))
                .title_bar(false)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .frame(ui_kit::dialog_frame(&ctx.global_style()))
                .show(ctx, |ui| {
                    // (I6: narrower and shorter in a small or zoomed-in window.)
                    let picker_w = ui_kit::dialog_w(ctx, PICKER_W);
                    ui.set_width(picker_w);
                    ui.spacing_mut().item_spacing.y = 6.0;
                    ui.horizontal(|ui| {
                        ui_kit::dialog_title(ui, "Nova mensagem");
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui_kit::icon_button(ui, ph::X, "Fechar", IconState::Normal).clicked() {
                                open = false;
                            }
                        });
                    });
                    ui_kit::hint(ui, "Uma conversa só entre vocês dois. Escolha a pessoa:");
                    let r = ui.add(ui_kit::field(&mut self.dms.picker.filter).hint_text(format!("{}  Procurar pelo nome", ph::MAGNIFYING_GLASS)).desired_width(picker_w));
                    if !r.has_focus() && self.dms.picker.filter.is_empty() {
                        r.request_focus();
                    }
                    if self.dms.picker.loading {
                        ui_kit::hint(ui, "Carregando...");
                    }
                    if let Some(e) = &self.dms.picker.error {
                        ui_kit::result_text(ui, false, e);
                    }
                    let me = self.my_id();
                    let people: Vec<User> = filter_people(&self.dms.picker.people, &self.dms.picker.filter, me).into_iter().cloned().collect();
                    ui.add_space(space::S);
                    egui::ScrollArea::vertical().max_height((ctx.content_rect().height() - 230.0).clamp(60.0, 320.0)).auto_shrink([false, true]).show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        let typed = !self.dms.picker.filter.trim().is_empty();
                        for (i, u) in people.iter().enumerate() {
                            // With a search typed, the first one is what Enter picks: shown raised.
                            if self.picker_row(ui, u, i == 0 && typed).clicked() {
                                pick = Some(u.id);
                            }
                        }
                        if people.is_empty() && !self.dms.picker.loading {
                            ui_kit::hint(ui, "Ninguém encontrado.");
                        }
                    });
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        pick = pick.or(people.first().map(|u| u.id));
                    }
                });
            if let Some(id) = pick {
                self.dms.picker.open = false;
                self.open_dm(id);
            } else if !open {
                self.dms.picker.open = false;
            }
        }
        let Some(call) = self.dms.incoming(self.my_id()).cloned() else { return };
        let caller = call.caller.clone();
        let mut action = None;
        egui::Window::new("Chamada recebida")
            .id(egui::Id::new("dm_incoming_call"))
            .collapsible(false)
            .resizable(false)
            .title_bar(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 60.0])
            .frame(ui_kit::dialog_frame(&ctx.global_style()).inner_margin(egui::Margin { left: 24, right: 24, top: 22, bottom: 20 }))
            .show(ctx, |ui| {
                ui.set_width(ui_kit::dialog_w(ctx, RING_W));
                ui.vertical_centered(|ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    ui.label(RichText::new(format!("{}  Chamada recebida", ph::PHONE_INCOMING)).font(theme::semibold(12.5)).color(theme::NEVOA));
                    ui.add_space(space::M);
                    let pic = crate::avatars::avatar_widget(&mut self.avatars.store, ui, &caller, 72.0, crate::avatars::Surface::Chat);
                    // A chalk ring: somebody wants to talk (drawn once, it does not pulse; the lamp's ring
                    // is only for whoever speaks).
                    ui_kit::paint_ring(ui.painter(), pic.rect.center(), 36.0, theme::PAREDE, theme::GIZ);
                    ui.add_space(space::M);
                    ui.add(egui::Label::new(RichText::new(caller.shown_name()).font(theme::display(19.0)).color(theme::GIZ)).truncate().selectable(false));
                    ui.label(RichText::new("está ligando para você · chamada de voz privada").font(theme::body(12.5)).color(theme::NEVOA));
                    ui.add_space(space::XL);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = space::M;
                        let w = 2.0 * 124.0 + space::M;
                        ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                        if ui_kit::button_sized(ui, Kind::Danger, ui_kit::Size::Large, Some(ph::PHONE_X), "Recusar").clicked() {
                            action = Some(false);
                        }
                        if ui_kit::button_sized(ui, Kind::Primary, ui_kit::Size::Large, Some(ICON_PHONE), "Atender").clicked() {
                            action = Some(true);
                        }
                    });
                });
            });
        match action {
            Some(true) => self.accept_dm_call(),
            Some(false) => self.decline_dm_call(),
            None => {}
        }
    }

    /// A person of "Nova mensagem": the picture, the name and the username, the row raised under
    /// the pointer (or when it is what Enter picks).
    fn picker_row(&mut self, ui: &mut egui::Ui, u: &User, first: bool) -> egui::Response {
        use crate::theme;
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 40.0), egui::Sense::click());
        if resp.hovered() || first {
            ui.painter().rect_filled(rect, theme::radius::ROW, if resp.hovered() { theme::MOVEL_HOVER } else { theme::ROW_HOVER });
        }
        let mut row = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(egui::vec2(8.0, 0.0))).layout(Layout::left_to_right(Align::Center)));
        row.spacing_mut().item_spacing.x = 10.0;
        crate::avatars::avatar_widget(&mut self.avatars.store, &mut row, u, 28.0, crate::avatars::Surface::Online);
        row.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(RichText::new(u.shown_name()).font(theme::semibold(13.5)).color(theme::GIZ));
            if u.has_display_name() {
                ui.label(RichText::new(format!("@{}", u.username)).font(theme::body(12.0)).color(theme::NEVOA));
            }
        });
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, u.shown_name()));
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }
}

/// Width of "Nova mensagem" and of the incoming-call prompt.
const PICKER_W: f32 = 360.0;
const RING_W: f32 = 300.0;

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(id: DmId, peer: UserId, at: i64) -> DmConversation {
        DmConversation { id, peer: User::new(peer, format!("u{peer}")), last_message: None, last_activity_ms: at, unread: 0, read_up_to: 0, blocked: false }
    }

    fn line(id: DmMessageId, dm: DmId, author: UserId, at: i64) -> DmMessage {
        DmMessage { id, dm_id: dm, author: User::new(author, format!("u{author}")), content: format!("m{id}"), created_at_ms: at, kind: DmMessageKind::Text, call_secs: 0, attachments: vec![], reply: None, reactions: vec![], sealed: None, sealed_attachments: vec![] }
    }

    const ME: UserId = 1;

    #[test]
    fn the_list_is_ordered_by_last_activity() {
        let mut v = vec![conv(1, 2, 100), conv(2, 3, 300), conv(3, 4, 200), conv(4, 5, 300)];
        sort_convs(&mut v);
        assert_eq!(v.iter().map(|c| c.id).collect::<Vec<_>>(), [4, 2, 3, 1], "newest first; a tie puts the newer conversation first");
        // A message moves its conversation to the top.
        assert!(apply_message(&mut v, &line(10, 1, 2, 400), ME, false));
        assert_eq!(v[0].id, 1);
        assert_eq!(v[0].last_message.as_ref().map(|m| m.id), Some(10));
        // An older line arriving late (history) does not replace the preview or move it back.
        assert!(apply_message(&mut v, &line(5, 1, 2, 50), ME, false));
        assert_eq!((v[0].id, v[0].last_message.as_ref().map(|m| m.id), v[0].last_activity_ms), (1, Some(10), 400));
        // Unknown conversation: the caller must fetch the list.
        assert!(!apply_message(&mut v, &line(11, 99, 2, 500), ME, false));
    }

    #[test]
    fn unread_counts_follow_messages_reads_and_deletes() {
        let mut v = vec![conv(1, 2, 0)];
        for (id, at) in [(1, 1), (2, 2), (3, 3)] {
            apply_message(&mut v, &line(id, 1, 2, at), ME, false);
        }
        assert_eq!((v[0].unread, total_unread(&v)), (3, 3));
        // Reading what is on screen counts nothing.
        apply_message(&mut v, &line(4, 1, 2, 4), ME, true);
        assert_eq!((v[0].unread, v[0].read_up_to), (3, 4));
        // A partial read with the log loaded counts what is left.
        let mut v = vec![conv(1, 2, 0)];
        let log: Vec<DmMessage> = (1..=4).map(|i| line(i, 1, 2, i)).collect();
        for m in &log {
            apply_message(&mut v, m, ME, false);
        }
        apply_read(&mut v, 1, 2, Some(&log), ME);
        assert_eq!((v[0].unread, v[0].read_up_to), (2, 2));
        apply_read(&mut v, 1, 1, Some(&log), ME);
        assert_eq!(v[0].read_up_to, 2, "never backwards");
        // Deleting an unread line takes it off the count; deleting the newest changes the preview.
        apply_delete(&mut v, 1, 4, Some(&log[3]), Some(&log[2]), ME);
        assert_eq!((v[0].unread, v[0].last_message.as_ref().map(|m| m.id)), (1, Some(3)));
        // Reading up to the newest line clears it, even without the log.
        apply_read(&mut v, 1, 3, None, ME);
        assert_eq!(v[0].unread, 0);
        // Writing myself reads everything.
        apply_message(&mut v, &line(5, 1, 2, 5), ME, false);
        apply_message(&mut v, &line(6, 1, ME, 6), ME, false);
        assert_eq!((v[0].unread, v[0].read_up_to), (0, 6));
        // A blocked conversation does not count in the total.
        v[0].unread = 2;
        v[0].blocked = true;
        assert_eq!(total_unread(&v), 0);
    }

    #[test]
    fn lines_stay_sorted_and_unique() {
        let mut msgs = Vec::new();
        for id in [5, 3, 9, 3, 5] {
            insert_line(&mut msgs, line(id, 1, 2, id));
        }
        assert_eq!(msgs.iter().map(|m| m.id).collect::<Vec<_>>(), [3, 5, 9]);
    }

    #[test]
    fn previews_and_call_lines_read_well() {
        let mut c = conv(1, 2, 0);
        assert_eq!(preview_text(&c, ME), "Nenhuma mensagem ainda");
        let mut m = line(1, 1, ME, 1);
        m.content = "oi\n  tudo   bem?".into();
        c.last_message = Some(m.clone());
        assert_eq!(preview_text(&c, ME), "Você: oi tudo bem?");
        m.author = User::new(2, "bia");
        c.last_message = Some(m.clone());
        assert_eq!(preview_text(&c, ME), "oi tudo bem?");
        m.kind = DmMessageKind::CallMissed;
        assert_eq!(call_line_text(&m, ME), "Chamada perdida de bia");
        m.author = User::new(ME, "eu");
        assert_eq!(call_line_text(&m, ME), "Você ligou: sem resposta");
        m.kind = DmMessageKind::CallEnded;
        m.call_secs = 125;
        assert_eq!(call_line_text(&m, ME), "Chamada de você · 2 min");
        assert_eq!((duration_text(9), duration_text(3600 + 5 * 60)), ("9 s".to_string(), "1 h 05 min".to_string()));
    }

    #[test]
    fn people_are_filtered_by_name_and_display_name() {
        let mut ana = User::new(2, "ana");
        ana.display_name = Some("Aninha".into());
        let people = vec![User::new(1, "eu"), User::new(3, "Bruno"), ana, User::new(4, "caio")];
        let names = |q: &str| filter_people(&people, q, Some(1)).iter().map(|u| u.username.clone()).collect::<Vec<_>>();
        assert_eq!(names(""), ["ana", "Bruno", "caio"], "by shown name, me left out");
        assert_eq!(names("NINH"), ["ana"]);
        assert_eq!(names("br"), ["Bruno"]);
        assert!(names("zzz").is_empty());
    }

    #[test]
    fn the_ringtone_repeats_on_schedule() {
        let t = Instant::now();
        assert!(ring_due(None, t));
        assert!(!ring_due(Some(t + RING_EVERY), t));
        assert!(ring_due(Some(t), t + Duration::from_millis(1)));
    }
}
